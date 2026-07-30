//! Ack persistence, and the two ways a row can be suppressed.
//!
//! Stores `session id -> { repo path -> edit timestamp acked }`. Storing the
//! timestamp rather than a bare path set is load-bearing: if it stored only
//! paths, a file you acked and an agent then rewrote would stay silently green,
//! which is the exact failure this tool exists to prevent.
//!
//! Lives under ~/.claude, never inside the repo -- the sidecar must not add
//! untracked files to a working tree that is already dirty.
//!
//! # Defer and dismiss are not the same gesture
//!
//! Three suppressions live here and they differ in *when they end*, which is the
//! only distinction that matters at the keyboard:
//!
//!   - **ack** -- this write-set was tested, at these timestamps. Ends when a
//!     path is written again, per path.
//!   - **defer** -- I have read this "waiting on you" state. Ends the moment the
//!     session logs anything newer, which is why it stores `last_activity`
//!     rather than a flag.
//!   - **dismiss** -- this session is over. Ends only when a human says so.
//!
//! `defer` was called `dismiss` while it was the only one of the two, and the
//! file it writes is still `dismissed.json` -- renaming it would strand every
//! deferral already on disk to buy a tidier filename. The methods carry the
//! honest names; `dismissed-sessions.json` is the new one, and it is a flat
//! array of ids because there is no timestamp to compare against. A dismissal
//! that compared against anything would be a deferral.
//!
//! # Many writers, one file
//!
//! The state file is global while the tools that write it are per-repo, so
//! several processes hold it open at once -- three `sauron` panes and a
//! `muthur` board is an ordinary Tuesday. A store that wrote its whole
//! in-memory map would then lose acks: each process loaded the file at launch,
//! and whichever saved last would erase every ack the others made since.
//!
//! So a save is not a dump, it is a *replay*. Each mutation appends to a
//! journal; `save` re-reads the file, applies only this process's journal on
//! top of whatever else landed meanwhile, and writes that. `unack` is why the
//! journal exists rather than a max-merge of two maps -- a merge cannot tell a
//! session someone deliberately un-acked from one they simply have not acked
//! yet, so it would resurrect it on the next save.
//!
//! grep targets:
//!   struct AckStore     -- in-memory maps plus their on-disk paths
//!   enum Op             -- one journalled mutation, replayed at save time
//!   fn AckStore::load   -- read, tolerating absent or corrupt state
//!   fn AckStore::ack    -- record every current edit ts for one session
//!   fn AckStore::defer  -- suppress a waiting state until the agent moves
//!   fn AckStore::dismiss -- suppress a session outright, until a human undoes it
//!   fn AckStore::save   -- lock, re-read, replay the journal, atomic write
//!   fn replay           -- apply a journal to a freshly-read state
//!   fn Lock::acquire    -- O_EXCL lockfile with a stale-lock steal

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value};

use crate::scan::home;

/// One mutation, held until `save` can replay it onto the current file.
#[derive(Debug, Clone)]
enum Op {
    Ack(String, PathAcks),
    Unack(String),
    Defer(String, i64),
    Undefer(String),
    Dismiss(String),
    Undismiss(String),
}

pub type PathAcks = BTreeMap<String, i64>;

pub struct AckStore {
    path: PathBuf,
    acks: BTreeMap<String, PathAcks>,
    /// session id -> the last_activity it had when the user deferred its
    /// "waiting on you" state. It stays deferred until the session logs
    /// something newer, at which point it has done fresh work and re-surfaces.
    /// Kept in a sibling file so the acks format is untouched.
    deferred_path: PathBuf,
    deferred: BTreeMap<String, i64>,
    /// Sessions dismissed outright -- old, finished, or useless. No timestamp,
    /// because there is nothing for a dismissal to be conditional on: it is the
    /// one suppression that survives whatever the agent does next.
    dismissed_path: PathBuf,
    dismissed: BTreeSet<String>,
    /// Mutations made since the last successful save, in order. Replayed onto
    /// the on-disk state so a concurrent writer's acks survive ours.
    journal: Vec<Op>,
    /// Guards the read-modify-write in `save` against the other processes.
    lock_path: PathBuf,
}

impl AckStore {
    pub fn load() -> Self {
        Self::load_at(home().join(".claude").join("sauron"))
    }

    /// `load` against an explicit directory. Exists so the multi-writer
    /// behaviour can be tested with two real stores over one real directory --
    /// the bug it guards only appears between processes, so a unit test of the
    /// merge function alone would not have caught it.
    pub fn load_at(dir: PathBuf) -> Self {
        let path = dir.join("acks.json");
        // `dismissed.json` holds *deferrals*, from before the two gestures were
        // told apart. See the module header: the name stays so existing state
        // keeps working.
        let deferred_path = dir.join("dismissed.json");
        let dismissed_path = dir.join("dismissed-sessions.json");
        // Absent on first run; corrupt means someone hand-edited it. Either way
        // an empty store is recoverable -- everything just reads as untested,
        // which is the safe direction to fail.
        let acks = read_map(&path, decode);
        let deferred = read_map(&deferred_path, decode_flat);
        let dismissed = read_map(&dismissed_path, decode_set);
        Self {
            lock_path: dir.join(".state.lock"),
            path,
            acks,
            deferred_path,
            deferred,
            dismissed_path,
            dismissed,
            journal: Vec::new(),
        }
    }

    pub fn for_session(&self, id: &str) -> Option<&PathAcks> {
        self.acks.get(id)
    }

    pub fn session_count(&self) -> usize {
        self.acks.len()
    }

    /// Mark this session's write-set as tested at its current timestamps.
    pub fn ack(&mut self, id: &str, edits: &BTreeMap<String, i64>) {
        self.journal.push(Op::Ack(id.to_string(), edits.clone()));
        let slot = self.acks.entry(id.to_string()).or_default();
        for (path, ts) in edits {
            slot.insert(path.clone(), *ts);
        }
    }

    /// Drop a session's acks so its whole write-set reads as untested again.
    pub fn unack(&mut self, id: &str) {
        self.journal.push(Op::Unack(id.to_string()));
        self.acks.remove(id);
    }

    /// Defer a session's current "waiting on you" state. Recording the activity
    /// timestamp -- not a bare flag -- is what makes it re-surface the moment
    /// the agent does anything new, so a deferred session that then asks a fresh
    /// question is not silently hidden.
    pub fn defer(&mut self, id: &str, last_activity: i64) {
        self.journal.push(Op::Defer(id.to_string(), last_activity));
        self.deferred.insert(id.to_string(), last_activity);
    }

    pub fn undefer(&mut self, id: &str) {
        self.journal.push(Op::Undefer(id.to_string()));
        self.deferred.remove(id);
    }

    /// Dismiss a session for good. Unconditional by design: this is the gesture
    /// for work that is finished or was never going anywhere, and a dismissal
    /// that could be undone by the agent writing one more log line would be a
    /// deferral wearing a different key.
    pub fn dismiss(&mut self, id: &str) {
        self.journal.push(Op::Dismiss(id.to_string()));
        self.dismissed.insert(id.to_string());
    }

    pub fn undismiss(&mut self, id: &str) {
        self.journal.push(Op::Undismiss(id.to_string()));
        self.dismissed.remove(id);
    }

    pub fn is_dismissed(&self, id: &str) -> bool {
        self.dismissed.contains(id)
    }

    pub fn dismissed_count(&self) -> usize {
        self.dismissed.len()
    }

    /// Un-dismiss everything, returning how many came back.
    ///
    /// The escape hatch for a dismissal made in a session that has since exited
    /// -- the in-app undo only remembers the last one, and a list of ids the
    /// board deliberately does not draw is not something a cursor can reach.
    pub fn restore_all_dismissed(&mut self) -> usize {
        let ids: Vec<String> = self.dismissed.iter().cloned().collect();
        for id in &ids {
            self.journal.push(Op::Undismiss(id.clone()));
        }
        self.dismissed.clear();
        ids.len()
    }

    /// Re-read the state files, discarding unsaved journal entries.
    ///
    /// This is how a long-lived reader picks up another process's acks. Without
    /// it a board left open all day would keep showing work as untested that you
    /// acked in a sauron pane an hour ago -- the file changed, but nothing ever
    /// re-read it.
    pub fn reload(&mut self) {
        self.acks = read_map(&self.path, decode);
        self.deferred = read_map(&self.deferred_path, decode_flat);
        self.dismissed = read_map(&self.dismissed_path, decode_set);
        self.journal.clear();
    }

    /// The activity timestamp at which this session was deferred, if it was.
    pub fn deferred_at(&self, id: &str) -> Option<i64> {
        self.deferred.get(id).copied()
    }

    /// Replay this process's journal onto the current file and write the result.
    ///
    /// Read-modify-write under a lock, not a dump of the in-memory map: see the
    /// module header. On success the merged state becomes our in-memory state,
    /// so a concurrent writer's acks are visible immediately rather than at the
    /// next relaunch, and the journal is cleared.
    ///
    /// A save with nothing journalled still re-reads, which is what keeps two
    /// open boards converging instead of drifting.
    pub fn save(&mut self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }

        // Held across the read and both writes. If it cannot be taken the save
        // still proceeds -- a lost ack is a smaller harm than a board that
        // silently stops persisting because a lockfile got wedged.
        let _lock = Lock::acquire(&self.lock_path);

        let mut acks = read_map(&self.path, decode);
        let mut deferred = read_map(&self.deferred_path, decode_flat);
        let mut dismissed = read_map(&self.dismissed_path, decode_set);
        replay(&self.journal, &mut acks, &mut deferred, &mut dismissed);

        write_atomic(&self.path, &encode(&acks))?;
        write_atomic(&self.deferred_path, &encode_flat(&deferred))?;
        write_atomic(&self.dismissed_path, &encode_set(&dismissed))?;

        self.acks = acks;
        self.deferred = deferred;
        self.dismissed = dismissed;
        self.journal.clear();
        Ok(())
    }
}

/// Apply a journal to freshly-read state. Order matters: ack-then-unack on one
/// session must land as un-acked, so the ops replay in the sequence they were
/// made rather than being grouped by kind.
fn replay(
    journal: &[Op],
    acks: &mut BTreeMap<String, PathAcks>,
    deferred: &mut BTreeMap<String, i64>,
    dismissed: &mut BTreeSet<String>,
) {
    for op in journal {
        match op {
            Op::Ack(id, edits) => {
                let slot = acks.entry(id.clone()).or_default();
                for (path, ts) in edits {
                    slot.insert(path.clone(), *ts);
                }
            }
            Op::Unack(id) => {
                acks.remove(id);
            }
            Op::Defer(id, ts) => {
                deferred.insert(id.clone(), *ts);
            }
            Op::Undefer(id) => {
                deferred.remove(id);
            }
            Op::Dismiss(id) => {
                dismissed.insert(id.clone());
            }
            Op::Undismiss(id) => {
                dismissed.remove(id);
            }
        }
    }
}

/// Read and decode a state file, treating absent and corrupt alike: an empty
/// map reads as "nothing acked", which fails toward showing work rather than
/// hiding it.
fn read_map<T: Default>(path: &Path, decode: fn(&Value) -> T) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| decode(&v))
        .unwrap_or_default()
}

/// How long a lockfile may sit before it is assumed to belong to a process that
/// died holding it. Well above a save (a few milliseconds), well below the point
/// where a user would notice acks refusing to persist.
const LOCK_STALE: Duration = Duration::from_secs(5);

/// An `O_EXCL` lockfile. Deliberately not `flock(2)`: that needs a `libc`
/// dependency, and sauron's four-crate tree is worth more than closing the last
/// microsecond of a race that this already shrinks from "every save" to "two
/// saves landing in the same instant".
struct Lock(PathBuf);

impl Lock {
    fn acquire(path: &Path) -> Option<Self> {
        for _ in 0..50 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => return Some(Self(path.to_path_buf())),
                Err(_) => {
                    // Steal a lock left behind by a process that crashed mid-save.
                    // Without this one bad exit wedges every sauron on the machine.
                    if let Ok(age) = std::fs::metadata(path).and_then(|m| m.modified()) {
                        if SystemTime::now()
                            .duration_since(age)
                            .map(|d| d > LOCK_STALE)
                            .unwrap_or(false)
                        {
                            let _ = std::fs::remove_file(path);
                            continue;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        None
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write-then-rename: a crash mid-save must not leave a truncated file that
/// reads as "everything untested" on next launch.
fn write_atomic(path: &std::path::Path, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

fn decode_flat(v: &Value) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    if let Some(obj) = v.as_object() {
        for (id, ts) in obj {
            if let Some(ts) = ts.as_i64() {
                out.insert(id.clone(), ts);
            }
        }
    }
    out
}

/// A flat array of session ids. Anything that is not a string is dropped rather
/// than coerced -- a junk entry that decoded to `""` would sit in the set
/// forever, matching nothing and confusing anyone who opened the file.
///
/// An object decodes to an empty set, which is what makes this safe to point at
/// a path that a much older build might have written a map to.
fn decode_set(v: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(items) = v.as_array() {
        for id in items {
            if let Some(id) = id.as_str() {
                out.insert(id.to_string());
            }
        }
    }
    out
}

fn encode_set(ids: &BTreeSet<String>) -> Value {
    Value::Array(ids.iter().map(|id| Value::from(id.clone())).collect())
}

fn encode_flat(map: &BTreeMap<String, i64>) -> Value {
    let mut root = Map::new();
    for (id, ts) in map {
        root.insert(id.clone(), Value::from(*ts));
    }
    Value::Object(root)
}

fn decode(v: &Value) -> BTreeMap<String, PathAcks> {
    let mut out = BTreeMap::new();
    let Some(obj) = v.as_object() else {
        return out;
    };
    for (session, paths) in obj {
        let Some(paths) = paths.as_object() else {
            continue;
        };
        let mut inner = PathAcks::new();
        for (path, ts) in paths {
            if let Some(ts) = ts.as_i64() {
                inner.insert(path.clone(), ts);
            }
        }
        out.insert(session.clone(), inner);
    }
    out
}

fn encode(acks: &BTreeMap<String, PathAcks>) -> Value {
    let mut root = Map::new();
    for (session, paths) in acks {
        let mut inner = Map::new();
        for (path, ts) in paths {
            inner.insert(path.clone(), Value::from(*ts));
        }
        root.insert(session.clone(), Value::Object(inner));
    }
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let mut acks: BTreeMap<String, PathAcks> = BTreeMap::new();
        let mut inner = PathAcks::new();
        inner.insert("src/a.rs".into(), 1_700_000_000_000);
        acks.insert("sess-1".into(), inner);

        let decoded = decode(&encode(&acks));
        assert_eq!(decoded, acks);
    }

    #[test]
    fn deferred_map_round_trips() {
        let mut m: BTreeMap<String, i64> = BTreeMap::new();
        m.insert("sess-1".into(), 1_700_000_000_000);
        assert_eq!(decode_flat(&encode_flat(&m)), m);
        // Non-integer timestamps are dropped, not defaulted.
        assert!(decode_flat(&serde_json::json!({"s": "nope"})).is_empty());
    }

    #[test]
    fn the_dismissed_set_round_trips_and_ignores_junk() {
        let m: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(decode_set(&encode_set(&m)), m);
        // Non-strings are dropped rather than coerced to "".
        assert_eq!(decode_set(&serde_json::json!(["a", 5, null])).len(), 1);
        // And a map -- what a much older build would have left at a path like
        // this -- decodes to nothing at all rather than to a set of keys.
        assert!(decode_set(&serde_json::json!({"a": 1})).is_empty());
        assert!(decode_set(&Value::String("junk".into())).is_empty());
    }

    fn edits(path: &str, ts: i64) -> PathAcks {
        let mut m = PathAcks::new();
        m.insert(path.into(), ts);
        m
    }

    /// The multi-writer bug: two sauron panes, each acking a different session.
    /// Writing the in-memory map would erase whichever landed first; replaying
    /// the journal onto the re-read file keeps both.
    #[test]
    fn a_concurrent_writers_ack_is_not_erased() {
        // What the other process wrote while we held our stale copy.
        let mut disk: BTreeMap<String, PathAcks> = BTreeMap::new();
        disk.insert("theirs".into(), edits("src/a.rs", 100));
        let (mut deferred, mut dismissed) = (BTreeMap::new(), BTreeSet::new());

        let journal = vec![Op::Ack("ours".into(), edits("src/b.rs", 200))];
        replay(&journal, &mut disk, &mut deferred, &mut dismissed);

        assert!(disk.contains_key("theirs"), "concurrent ack was erased");
        assert!(disk.contains_key("ours"));
    }

    /// A dismissal made in one pane must survive another pane's save, and an
    /// un-dismissal must not be resurrected by it. Same argument as `unack`
    /// above: a union of two sets cannot tell "restored" from "never dismissed".
    #[test]
    fn a_dismissal_merges_and_an_undismissal_sticks() {
        let (mut acks, mut deferred) = (BTreeMap::new(), BTreeMap::new());
        let mut disk: BTreeSet<String> = ["theirs".to_string()].into_iter().collect();

        replay(
            &[Op::Dismiss("ours".into())],
            &mut acks,
            &mut deferred,
            &mut disk,
        );
        assert!(disk.contains("theirs"), "concurrent dismissal was erased");
        assert!(disk.contains("ours"));

        replay(
            &[Op::Undismiss("theirs".into())],
            &mut acks,
            &mut deferred,
            &mut disk,
        );
        assert!(!disk.contains("theirs"), "undismiss was undone by the merge");
    }

    /// The two suppressions are stored apart, so neither gesture can silently
    /// perform the other. Deferring must never dismiss: one ends when the agent
    /// moves, the other does not end at all.
    #[test]
    fn deferring_and_dismissing_do_not_touch_each_others_state() {
        let dir = std::env::temp_dir().join(format!("sauron-two-gestures-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut s = AckStore::load_at(dir.clone());
        s.defer("waiting", 100);
        s.dismiss("finished");
        s.save().unwrap();

        let fresh = AckStore::load_at(dir.clone());
        assert_eq!(fresh.deferred_at("waiting"), Some(100));
        assert!(!fresh.is_dismissed("waiting"), "a deferral dismissed a session");
        assert!(fresh.is_dismissed("finished"));
        assert_eq!(fresh.deferred_at("finished"), None, "a dismissal wrote a deferral");

        // And the legacy deferral file is still where it was, holding a map --
        // renaming it would have stranded every deferral already on disk.
        let legacy = std::fs::read_to_string(dir.join("dismissed.json")).unwrap();
        assert!(legacy.contains("waiting"), "deferrals left their own file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The escape hatch for a dismissal made in a process that has since exited.
    #[test]
    fn restore_all_brings_back_every_dismissed_session() {
        let dir = std::env::temp_dir().join(format!("sauron-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut s = AckStore::load_at(dir.clone());
        s.dismiss("a");
        s.dismiss("b");
        s.save().unwrap();

        let mut later = AckStore::load_at(dir.clone());
        assert_eq!(later.dismissed_count(), 2);
        assert_eq!(later.restore_all_dismissed(), 2);
        later.save().unwrap();

        assert_eq!(AckStore::load_at(dir.clone()).dismissed_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unack must not be undone by the merge. This is why the journal holds
    /// operations rather than a map to union: a union cannot distinguish
    /// "deliberately un-acked" from "not acked yet", so it would resurrect it.
    #[test]
    fn unack_beats_the_copy_still_on_disk() {
        let mut disk: BTreeMap<String, PathAcks> = BTreeMap::new();
        disk.insert("sess".into(), edits("src/a.rs", 100));
        let (mut deferred, mut dismissed) = (BTreeMap::new(), BTreeSet::new());

        replay(
            &[Op::Unack("sess".into())],
            &mut disk,
            &mut deferred,
            &mut dismissed,
        );

        assert!(!disk.contains_key("sess"), "unack was resurrected by merge");
    }

    /// Ops replay in the order they were made, so the last gesture on a session
    /// wins regardless of kind.
    #[test]
    fn journal_replays_in_order() {
        let mut acks = BTreeMap::new();
        let (mut deferred, mut dismissed) = (BTreeMap::new(), BTreeSet::new());
        replay(
            &[
                Op::Ack("s".into(), edits("a.rs", 1)),
                Op::Unack("s".into()),
                Op::Ack("s".into(), edits("b.rs", 2)),
            ],
            &mut acks,
            &mut deferred,
            &mut dismissed,
        );
        assert_eq!(acks["s"], edits("b.rs", 2));

        let mut acks = BTreeMap::new();
        let (mut deferred, mut dismissed) = (BTreeMap::new(), BTreeSet::new());
        replay(
            &[
                Op::Defer("s".into(), 5),
                Op::Undefer("s".into()),
                Op::Dismiss("s".into()),
                Op::Undismiss("s".into()),
            ],
            &mut acks,
            &mut deferred,
            &mut dismissed,
        );
        assert!(deferred.is_empty());
        assert!(dismissed.is_empty());
    }

    /// The bug, end to end: two stores over one directory, as a sauron pane and
    /// a muthur board are. Each loads, each acks a different session, each
    /// saves. Before the journal existed the second save wrote its whole
    /// in-memory map and the first store's ack vanished; this fails against that
    /// implementation and passes against the replay.
    #[test]
    fn two_stores_over_one_directory_keep_both_acks() {
        let dir = std::env::temp_dir().join(format!("sauron-two-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Both load the same (empty) state, as two processes launched together do.
        let mut pane = AckStore::load_at(dir.clone());
        let mut board = AckStore::load_at(dir.clone());

        pane.ack("session-in-the-pane", &edits("src/a.rs", 100));
        pane.save().unwrap();

        // The board's copy predates that write and knows nothing about it.
        board.ack("session-on-the-board", &edits("src/b.rs", 200));
        board.save().unwrap();

        let fresh = AckStore::load_at(dir.clone());
        assert!(
            fresh.for_session("session-in-the-pane").is_some(),
            "the pane's ack was erased by the board's save"
        );
        assert!(fresh.for_session("session-on-the-board").is_some());

        // And the merged state is adopted in memory, so the board can see the
        // pane's ack without relaunching.
        assert!(board.for_session("session-in-the-pane").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crashed process must not wedge every other writer on the machine.
    #[test]
    fn a_stale_lock_is_stolen() {
        let dir = std::env::temp_dir().join(format!("sauron-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".state.lock");

        // A lock nobody will ever release.
        std::fs::write(&path, b"").unwrap();
        let old = SystemTime::now() - LOCK_STALE - Duration::from_secs(1);
        filetime_set(&path, old);

        assert!(Lock::acquire(&path).is_some(), "stale lock was never stolen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Backdate a file's mtime without pulling in a crate to do it.
    ///
    /// This used to shell out to `touch -t $(date -r ...)`, which is two
    /// BSD-only invocations and no test at all on a platform that has neither.
    /// `File::set_modified` is std, has been since 1.75, and says the same thing
    /// in one call.
    fn filetime_set(path: &Path, when: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for restamping");
        file.set_modified(when).expect("set mtime");
    }

    #[test]
    fn malformed_state_decodes_to_empty_not_panic() {
        assert!(decode(&Value::String("junk".into())).is_empty());
        assert!(decode(&serde_json::json!({"s": 5})).is_empty());
        // Non-integer timestamps are dropped rather than defaulted to 0, which
        // would otherwise read as "acked long ago" and hide real edits.
        let d = decode(&serde_json::json!({"s": {"a.rs": "nope"}}));
        assert!(d["s"].is_empty());
    }
}
