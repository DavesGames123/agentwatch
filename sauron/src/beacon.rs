//! The beacon: a snapshot of one repo's board, published where the watched
//! project's *own process* can read it.
//!
//! sauron already recomputes "what needs attending" every TICK and then throws
//! it away when the frame is drawn. The beacon is that same answer written to a
//! file, so a second reader -- a game, a server, anything with a debug pane --
//! can show the board without linking this crate, without shelling out to
//! `sauron`, and without a socket to keep alive.
//!
//! Three properties are load-bearing:
//!
//! * **It lives in sauron's state dir, not the repo.** `~/.claude/sauron/
//!   beacons/<encoded>.beacon`. The "sauron writes nothing to the watched repo"
//!   invariant is what makes it safe to run against someone else's checkout,
//!   and a status file dropped into the tree would break it for a convenience.
//!
//! * **Liveness is mtime, not a handshake.** The file is rewritten every TICK
//!   while sauron runs. A reader that finds it older than `STALE_MS` treats
//!   sauron as gone. Nothing has to be told about a crash, a SIGKILL, or a
//!   closed laptop lid -- all three stop the writes, which is the whole signal.
//!   `retire` on clean exit is a courtesy that makes the pane vanish
//!   immediately instead of six seconds later.
//!
//! * **It is line-oriented text, not JSON.** The intended readers are host
//!   projects that must parse it *without taking a dependency* -- the panel
//!   shipped by `panel.rs` installs as a single file into projects whose
//!   dependency graphs are pinned (vendored egui, patched crates). A
//!   tab-separated format parses in sixty lines of std. The cost is that no
//!   third-party tool gets free parsing; a `--json` emitter over these same
//!   structs is the escape hatch if that ever matters.
//!
//! Format v1. Tabs separate fields, `\` `\t` `\n` are escaped in free text:
//!
//! ```text
//! sauron-beacon 1
//! repo    /Users/d/Downloads/barnes-hut
//! label   barnes-hut
//! pid     2418
//! written 1753567890123
//! counts  3       1       12      2         (attention working clear stale)
//! row     needs_test      a1b2c3d4  <full-id> <turn_started> <last_activity> <orc> <edits> <name>
//! detail  station hull plates render at the wrong scale
//! file    src/gui/overlays/station_damage.rs
//! file    src/combat/hull.rs
//! step    xray    Colony / X-ray  2       verified        Press E, or click the x-ray squircle.
//! need    a station selected
//! route   0       0                                       (unmapped, truncated)
//! row     blocked ...
//! end
//! ```
//!
//! v2 added `step`, `need` and `route`: where to look in order to see the edits
//! a row is asking about, resolved from the watched repo's own
//! `.sauron/panels.toml` (see `route.rs`). Each attaches to the row above it,
//! and `need` to the `step` above it, the same way `file` already did.
//!
//! A repo with no map emits none of the three, which is not the same as a repo
//! whose map covered everything -- the `route` line is written whenever a map
//! was found, so its absence means "no map" and `route 0 0` means "mapped, all
//! covered". A reader that cannot tell those apart will eventually report a
//! stale map as a clean one.
//!
//! The trailing `end` line is the torn-read guard. `rename` is atomic on every
//! filesystem this runs on, so a reader should never see a partial file -- but
//! a reader that checks for `end` fails closed if one ever does, which costs one
//! line here and one there.
//!
//! grep targets:
//!   fn path_for      -- repo root -> beacon path, the encoding both ends share
//!   fn publish       -- Board -> file, atomically
//!   fn retire        -- unlink on clean exit
//!   fn render        -- Board -> the v1 wire text
//!   fn parse         -- wire text -> Beacon, the reader half
//!   const STALE_MS   -- how old a beacon may be and still count as live

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::board::Board;
use crate::model::{collapse_ws, now_ms, Status};
use crate::scan::home;

/// Wire format version. Bump on any field change; readers refuse a version they
/// were not built for rather than guessing at a shifted column.
pub const VERSION: u32 = 3;

/// How stale a beacon may be before a reader must treat sauron as gone.
///
/// Three times the TUI's 2s TICK. One missed write is a slow refresh on a big
/// log; three in a row is a process that is not coming back. Tightening this to
/// one TICK would blink the reader's pane off every time a scan ran long.
pub const STALE_MS: i64 = 6_000;

/// One session, flattened to what a foreign reader can render without knowing
/// anything about sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeaconRow {
    /// The full session id.
    ///
    /// Carried alongside `id_short` rather than instead of it because they are
    /// for different consumers: the short form is for eyes and collides
    /// eventually, the full one is the only thing that can address a session --
    /// resume it, or deliver a reply to it. A reader that can only see the
    /// short form can display a board but never answer it.
    pub id: String,
    /// Wire token -- see `status_token`. A string, not the enum, because the
    /// reader is in another crate and must not need this one's types.
    pub status: String,
    pub id_short: String,
    /// Epoch millis the current turn began; 0 when unknown.
    pub turn_started: i64,
    pub last_activity: i64,
    pub is_orc: bool,
    pub total_edits: usize,
    pub name: String,
    /// Why this row wants a human: the error or block reason where there is
    /// one, otherwise the prompt that started the turn.
    pub detail: String,
    /// Repo-relative paths written but not acked.
    pub files: Vec<String>,
    /// Where to look, best first. Empty when the repo ships no map, or when
    /// nothing this row touched is described by one.
    pub steps: Vec<BeaconStep>,
    /// Files in this row that no panel claims. The staleness signal -- see
    /// `route::Route::unmapped`.
    pub unmapped: usize,
    /// Surfaces beyond the route's cap that were not listed.
    pub truncated: usize,
    /// Whether the repo published a map at all. Distinguishes "nothing to
    /// open" from "nobody said".
    pub has_map: bool,
}

/// One surface to open, flattened for a reader that knows nothing about maps.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BeaconStep {
    /// What the host's own UI calls this control; empty when it has no button.
    /// A host that wants to highlight the thing to press matches on this.
    pub anchor: String,
    pub title: String,
    /// How many of the row's files this surface renders.
    pub hits: usize,
    /// `verified` or `inferred`, passed through from the map so a reader can
    /// show a guess as a guess.
    pub confidence: String,
    pub open: String,
    pub needs: Vec<String>,
}

/// A published board, as read back off disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    pub version: u32,
    pub repo: String,
    pub label: String,
    pub pid: u32,
    pub written_ms: i64,
    /// Rows in a status that wants a human: errored, blocked, awaiting ack,
    /// needs test. The number a header should show.
    pub attention: usize,
    /// Rows mid-turn or waiting on a sub-agent -- context, not a to-do.
    pub working: usize,
    pub clear: usize,
    pub hidden_stale: usize,
    /// Ranked exactly as the TUI ranks them.
    pub rows: Vec<BeaconRow>,
}

impl Beacon {
    /// Whether this snapshot is recent enough to mean "sauron is running".
    pub fn is_live(&self, now: i64) -> bool {
        now.saturating_sub(self.written_ms) < STALE_MS
    }
}

/// The directory beacons live in, created on demand.
pub fn beacon_dir() -> PathBuf {
    home().join(".claude").join("sauron").join("beacons")
}

/// Repo root -> its beacon path.
///
/// The encoding is the one Claude Code uses for `~/.claude/projects` and that
/// `scan::project_dir_for` already relies on: separators and dots become
/// dashes. Reusing it rather than hashing keeps the reader dependency-free --
/// a host project can compute this path with `String::replace` and no sha2 --
/// and keeps the file greppable by eye when something looks wrong.
pub fn path_for(repo_root: &Path) -> PathBuf {
    let encoded = repo_root.to_string_lossy().replace(['/', '.'], "-");
    beacon_dir().join(format!("{encoded}.beacon"))
}

/// Publish `board` to its beacon path.
///
/// Writes a sibling temp file and renames over the target, so a reader either
/// sees the previous snapshot or the new one and never a half-written frame.
/// The temp name carries the pid: two sauron instances watching the same repo
/// (which happens -- one per terminal pane) must not collide on the scratch
/// file even though they are racing to publish the same content.
pub fn publish(board: &Board) -> std::io::Result<()> {
    let path = path_for(board.repo_root());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;

    let body = render(board);
    let tmp = dir.join(format!(".{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}

/// Remove this repo's beacon. Called on clean exit so a reader's pane closes at
/// once instead of aging out.
///
/// Deliberately unconditional about *whose* beacon it is: with two watchers on
/// one repo the survivor rewrites it within a TICK, and the alternative -- pid
/// ownership -- would leave a stale file behind whenever the owner was the one
/// that crashed, which is the case that matters.
pub fn retire(repo_root: &Path) {
    let _ = std::fs::remove_file(path_for(repo_root));
}

/// Read and parse a repo's beacon, or `None` if absent, unreadable, malformed,
/// or from a version this build does not know.
pub fn read(repo_root: &Path) -> Option<Beacon> {
    let text = std::fs::read_to_string(path_for(repo_root)).ok()?;
    parse(&text)
}

/// The status word on the wire.
pub fn status_token(s: Status) -> &'static str {
    match s {
        Status::Errored => "errored",
        Status::Blocked => "blocked",
        Status::AwaitingAck => "awaiting_ack",
        Status::Working => "working",
        Status::Delegated => "delegated",
        Status::NeedsTest => "needs_test",
        Status::Clear => "clear",
    }
}

/// Whether a status is one a human has to act on. Kept here rather than left to
/// each reader so the header count and the TUI's sections cannot drift.
pub fn wants_human(s: Status) -> bool {
    matches!(
        s,
        Status::Errored | Status::Blocked | Status::AwaitingAck | Status::NeedsTest
    )
}

/// How much of a prompt survives into the detail line. Long enough to identify
/// the task, short enough that a pane can lay it out without wrapping into a
/// paragraph; readers are free to truncate further.
const DETAIL_MAX: usize = 180;

/// Board -> wire text.
fn render(board: &Board) -> String {
    let mut out = String::with_capacity(2048);
    // Once per publish, not once per row: the map is mtime-cached, but the
    // repo root lookup and the Option dance are still work, and every row on a
    // board shares the same map by construction.
    let map = crate::route::for_repo(board.repo_root());
    let attention = board.rows.iter().filter(|r| wants_human(r.status)).count();
    let working = board
        .rows
        .iter()
        .filter(|r| matches!(r.status, Status::Working | Status::Delegated))
        .count();

    out.push_str(&format!("sauron-beacon\t{VERSION}\n"));
    out.push_str(&format!("repo\t{}\n", esc(&board.repo_root().to_string_lossy())));
    out.push_str(&format!("label\t{}\n", esc(&board.repo_label)));
    out.push_str(&format!("pid\t{}\n", std::process::id()));
    out.push_str(&format!("written\t{}\n", now_ms()));
    out.push_str(&format!(
        "counts\t{attention}\t{working}\t{}\t{}\n",
        board.clear_count, board.hidden_stale
    ));

    for r in &board.rows {
        out.push_str(&format!(
            "row\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            status_token(r.status),
            esc(&r.id_short),
            esc(&r.id),
            r.turn_started,
            r.last_activity,
            if r.is_orc { 1 } else { 0 },
            r.total_edits,
            esc(&collapse_ws(&r.name)),
        ));
        let detail = detail_for(r);
        if !detail.is_empty() {
            out.push_str(&format!("detail\t{}\n", esc(&detail)));
        }
        for p in &r.pending {
            out.push_str(&format!("file\t{}\n", esc(p)));
        }
        if let Some(m) = &map {
            let route = m.route(&r.pending);
            for s in &route.steps {
                out.push_str(&format!(
                    "step\t{}\t{}\t{}\t{}\t{}\n",
                    esc(&s.anchor),
                    esc(&s.title),
                    s.hits,
                    esc(&s.confidence),
                    esc(&collapse_ws(&s.open)),
                ));
                for n in &s.needs {
                    out.push_str(&format!("need\t{}\n", esc(&collapse_ws(n))));
                }
            }
            // Written even when both are zero: its presence is how a reader
            // knows a map existed at all.
            out.push_str(&format!("route\t{}\t{}\n", route.unmapped, route.truncated));
        }
    }
    out.push_str("end\n");
    out
}

/// The one line that says why this row is on the board.
///
/// Ranked by how much it tells a human who is looking at the pane and not at
/// the log: a recorded failure beats a block reason beats "here is what it was
/// asked to do". An errored row whose prompt was shown instead would read as an
/// ordinary task and lose the only fact that matters about it.
fn detail_for(r: &crate::board::Row) -> String {
    if let Some(e) = r.error {
        return e.short().to_string();
    }
    if let Some(b) = r.blocked_reason {
        return b.short().to_string();
    }
    match &r.last_prompt {
        Some(p) => truncate(&collapse_ws(p), DETAIL_MAX),
        None => String::new(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Escape the field separators out of free text. Order matters: backslash
/// first, or the escapes introduced below would themselves be escaped.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "")
}

/// Inverse of `esc`. A trailing lone backslash is dropped rather than treated as
/// an error -- the writer cannot produce one, so its only source is a truncated
/// file, which the `end` check already rejects.
fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Wire text -> `Beacon`.
///
/// Fails closed on anything it does not recognise: a missing magic line, a
/// version it was not built for, or a file with no `end`. All three mean "do
/// not draw", which is the safe direction -- a pane showing a half-parsed board
/// is worse than no pane, because it looks authoritative.
pub fn parse(text: &str) -> Option<Beacon> {
    let mut lines = text.lines();
    let head = lines.next()?;
    let (magic, ver) = head.split_once('\t')?;
    if magic != "sauron-beacon" {
        return None;
    }
    let version: u32 = ver.trim().parse().ok()?;
    if version != VERSION {
        return None;
    }

    let mut b = Beacon {
        version,
        repo: String::new(),
        label: String::new(),
        pid: 0,
        written_ms: 0,
        attention: 0,
        working: 0,
        clear: 0,
        hidden_stale: 0,
        rows: Vec::new(),
    };
    let mut sealed = false;

    for line in lines {
        if line == "end" {
            sealed = true;
            break;
        }
        let (tag, rest) = match line.split_once('\t') {
            Some(pair) => pair,
            None => continue,
        };
        match tag {
            "repo" => b.repo = unesc(rest),
            "label" => b.label = unesc(rest),
            "pid" => b.pid = rest.parse().unwrap_or(0),
            "written" => b.written_ms = rest.parse().unwrap_or(0),
            "counts" => {
                let f: Vec<&str> = rest.split('\t').collect();
                b.attention = f.first().and_then(|v| v.parse().ok()).unwrap_or(0);
                b.working = f.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
                b.clear = f.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
                b.hidden_stale = f.get(3).and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            "row" => {
                let f: Vec<&str> = rest.split('\t').collect();
                if f.len() < 8 {
                    continue;
                }
                b.rows.push(BeaconRow {
                    status: f[0].to_string(),
                    id_short: unesc(f[1]),
                    id: unesc(f[2]),
                    turn_started: f[3].parse().unwrap_or(0),
                    last_activity: f[4].parse().unwrap_or(0),
                    is_orc: f[5] == "1",
                    total_edits: f[6].parse().unwrap_or(0),
                    name: unesc(f[7]),
                    detail: String::new(),
                    files: Vec::new(),
                    steps: Vec::new(),
                    unmapped: 0,
                    truncated: 0,
                    has_map: false,
                });
            }
            // `detail` and `file` attach to the row above them. A stray one
            // before any `row` is dropped rather than rejected: it can only come
            // from a hand-edited file, and dropping it degrades one card instead
            // of blanking the pane.
            "detail" => {
                if let Some(r) = b.rows.last_mut() {
                    r.detail = unesc(rest);
                }
            }
            "file" => {
                if let Some(r) = b.rows.last_mut() {
                    r.files.push(unesc(rest));
                }
            }
            // `step` attaches to the row above it and `need` to the step above
            // that, on the same reasoning as `file`: a stray one degrades a
            // single card rather than rejecting the whole snapshot.
            "step" => {
                let f: Vec<&str> = rest.split('\t').collect();
                if f.len() < 5 {
                    continue;
                }
                if let Some(r) = b.rows.last_mut() {
                    r.steps.push(BeaconStep {
                        anchor: unesc(f[0]),
                        title: unesc(f[1]),
                        hits: f[2].parse().unwrap_or(0),
                        confidence: unesc(f[3]),
                        open: unesc(f[4]),
                        needs: Vec::new(),
                    });
                }
            }
            "need" => {
                if let Some(s) = b.rows.last_mut().and_then(|r| r.steps.last_mut()) {
                    s.needs.push(unesc(rest));
                }
            }
            "route" => {
                let f: Vec<&str> = rest.split('\t').collect();
                if let Some(r) = b.rows.last_mut() {
                    r.unmapped = f.first().and_then(|v| v.parse().ok()).unwrap_or(0);
                    r.truncated = f.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
                    r.has_map = true;
                }
            }
            _ => {}
        }
    }

    if sealed {
        Some(b)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        let mut s = String::new();
        s.push_str(&format!("sauron-beacon\t{VERSION}\n"));
        s.push_str("repo\t/tmp/demo\n");
        s.push_str("label\tdemo\n");
        s.push_str("pid\t99\n");
        s.push_str("written\t1700000000000\n");
        s.push_str("counts\t2\t1\t5\t3\n");
        s.push_str("row\terrored\tabc12345\tabc12345-full-id\t1700000000000\t1700000001000\t0\t7\ta\\tname\n");
        s.push_str("detail\tAPI error — retry\n");
        s.push_str("file\tsrc/a.rs\n");
        s.push_str("step\txray\tColony\t1\tverified\tPress E.\n");
        s.push_str("need\ta station selected\n");
        s.push_str("route\t2\t1\n");
        s.push_str("row\tworking\tdef67890\tdef67890-full-id\t0\t1700000002000\t1\t0\torc: shrink b.rs\n");
        s.push_str("end\n");
        s
    }

    #[test]
    fn parses_a_full_beacon() {
        let b = parse(&sample()).expect("parses");
        assert_eq!(b.repo, "/tmp/demo");
        assert_eq!(b.label, "demo");
        assert_eq!(b.pid, 99);
        assert_eq!((b.attention, b.working, b.clear, b.hidden_stale), (2, 1, 5, 3));
        assert_eq!(b.rows.len(), 2);
        // The escaped tab must survive the round trip as a real tab, or a name
        // containing one would silently split into extra columns on re-read.
        assert_eq!(b.rows[0].name, "a\tname");
        // The full id is what a reader needs to answer a row at all.
        assert_eq!(b.rows[0].id, "abc12345-full-id");
        assert_eq!(b.rows[0].files, vec!["src/a.rs".to_string()]);
        assert!(b.rows[1].is_orc);
        assert!(b.rows[1].files.is_empty());
    }

    #[test]
    fn rejects_unsealed_file() {
        let torn = sample().replace("end\n", "");
        assert!(parse(&torn).is_none());
    }

    #[test]
    fn rejects_foreign_version() {
        let future = sample().replace(
            &format!("sauron-beacon\t{VERSION}"),
            &format!("sauron-beacon\t{}", VERSION + 1),
        );
        assert!(parse(&future).is_none());
        let past = sample().replace(&format!("sauron-beacon\t{VERSION}"), "sauron-beacon\t1");
        assert!(parse(&past).is_none());
    }

    #[test]
    fn parses_the_route_lines() {
        let b = parse(&sample()).expect("parses");
        let r = &b.rows[0];
        assert_eq!(r.steps.len(), 1);
        assert_eq!(r.steps[0].anchor, "xray");
        assert_eq!(r.steps[0].title, "Colony");
        assert_eq!(r.steps[0].hits, 1);
        assert_eq!(r.steps[0].confidence, "verified");
        assert_eq!(r.steps[0].needs, vec!["a station selected".to_string()]);
        assert_eq!((r.unmapped, r.truncated), (2, 1));
        assert!(r.has_map);
    }

    #[test]
    fn a_row_without_route_lines_reports_no_map() {
        // The distinction that matters: this row had no `route` line at all,
        // which means the repo published no map -- not that its map covered
        // everything. A reader that conflates the two shows a stale map as a
        // clean one, forever.
        let b = parse(&sample()).expect("parses");
        let r = &b.rows[1];
        assert!(!r.has_map);
        assert!(r.steps.is_empty());
        assert_eq!(r.unmapped, 0);
    }

    #[test]
    fn a_need_before_any_step_is_dropped_not_fatal() {
        let hand_edited = sample().replace(
            "file\tsrc/a.rs\n",
            "file\tsrc/a.rs\nneed\torphaned\n",
        );
        let b = parse(&hand_edited).expect("still parses");
        // Attached to the previous row's last step if there is one; the point
        // is that the snapshot survives rather than blanking the pane.
        assert_eq!(b.rows.len(), 2);
    }

    #[test]
    fn rejects_foreign_magic() {
        assert!(parse("something-else\t1\nend\n").is_none());
    }

    #[test]
    fn escape_round_trips() {
        let ugly = "a\tb\nc\\d";
        assert_eq!(unesc(&esc(ugly)), "a\tb\nc\\d");
    }

    #[test]
    fn liveness_is_a_clock_comparison() {
        let mut b = parse(&sample()).unwrap();
        b.written_ms = 1_000_000;
        assert!(b.is_live(1_000_000 + STALE_MS - 1));
        assert!(!b.is_live(1_000_000 + STALE_MS));
    }

    #[test]
    fn path_encoding_matches_claude_code() {
        let p = path_for(Path::new("/Users/d/Downloads/barnes-hut"));
        assert!(p.ends_with("-Users-d-Downloads-barnes-hut.beacon"));
    }
}
