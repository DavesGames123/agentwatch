//! The headless watcher for one repo: scanner, ack store, and the ranked rows
//! that fall out of them.
//!
//! This is everything about "what is the state of this repo's agents" that does
//! not involve a terminal. It exists as its own module because there are now two
//! front ends over it -- sauron's single-repo TUI and muthur's multi-repo board
//! -- and the alternative was a second implementation of `to_row`. Status
//! classification is the product here; two copies of it would disagree the first
//! time either was tuned, and the aggregate view would become the less
//! trustworthy of the two screens for no stated reason.
//!
//! What stays out: selection, scroll, flash timers, frame geometry. A `Board`
//! has no cursor. Callers that need one keep it themselves and address rows by
//! session id, which is also what survives a refresh reordering the list.
//!
//! grep targets:
//!   struct Row          -- one session flattened for rendering
//!   struct Board        -- scanner + ack store + the current row set
//!   fn Board::refresh   -- rescan logs, rebuild and rank rows
//!   fn Board::to_row    -- one Session -> Row, or None if it should not show
//!   fn Board::ack       -- context-sensitive ack/defer by session id
//!   fn Board::dismiss   -- drop a finished session from the board for good
//!   fn dismissable      -- which statuses may be dismissed, and why not the rest
//!   fn within_horizon   -- how long a row of each status stays on the board
//!   fn Board::hot_paths -- paths a live session is touching, for orc safety

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::model::{
    now_ms, BlockedReason, ErrorKind, Session, Status, ATTENTION_HORIZON_MS, DORMANT_AFTER_MS,
};
use crate::scan::Scanner;
use crate::store::AckStore;

#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub id_short: String,
    pub name: String,
    pub branch: Option<String>,
    pub last_activity: i64,
    /// When the current turn (the "task") began, epoch millis. Zero when unknown.
    /// Drives the elapsed timer and local start-time on the card and detail pane.
    pub turn_started: i64,
    pub status: Status,
    pub blocked_reason: Option<BlockedReason>,
    /// The recorded failure behind `Status::Errored`, for the detail line.
    pub error: Option<ErrorKind>,
    /// Repo paths written but not acked at their current timestamp.
    pub pending: Vec<String>,
    pub total_edits: usize,
    /// Total billed context movement for the session -- input, output and both
    /// cache figures added together. See `Session::total_tokens` for what the
    /// number is and is not. Zero on a Codex session and on any Claude session
    /// whose log carries no `usage`.
    ///
    /// Numeric, not a string: every surface formats it itself, through
    /// `model::fmt_count`, so a board that has room for the full figure can print
    /// it and a card that has not can compact it.
    pub tokens: i64,
    pub last_prompt: Option<String>,
    /// One of sauron's own orcs (a single-shot maintenance agent), not a hobbit.
    pub is_orc: bool,
    /// `cd <cwd> && claude --resume <id>` -- reattach a dropped thread.
    pub continue_cmd: String,
    pub edits: BTreeMap<String, i64>,
    /// Repo-relative path -> the most recent lines of text written to it, for the
    /// selected card's per-file preview. Keyed like `pending`.
    pub previews: BTreeMap<String, Vec<String>>,
}

/// What `Board::dismiss` did, so the front end can report it rather than
/// leaving the user wondering whether the key is bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dismissal {
    /// Gone, and the display name it went by.
    Done(String),
    /// Refused: the session is mid-turn. See `dismissable`.
    StillRunning,
    /// The id is not on the board -- a stale cursor, or a row that refreshed
    /// away underneath the keypress.
    NoSuchRow,
}

/// Whether a session in this state may be dismissed.
///
/// Everything except `Working`, `Delegated` and `Stalled`, and the exclusion is
/// a safety property rather than a manners one. `hot_paths` -- which is what an orc
/// consults before it is loosed on a file -- is derived from `rows`, so a
/// dismissed session contributes no hot files. Dismissing a session that is
/// still writing would therefore not merely hide a row: it would tell the next
/// orc that the files under that agent's hands are cold, and the orc would
/// refactor a file mid-edit.
///
/// `Delegated` is included in the refusal for the same reason. The session
/// itself is sitting still, but it is waiting on a sub-agent that is not, and
/// the write-set it is holding is just as hot.
///
/// `Stalled` is included on the same argument, and it is the one that has to be
/// argued rather than seen: the session looks parked, but what defines the state
/// is an *open tool call*, and the likeliest thing that call is doing is writing
/// files. It used to reach here as `Blocked` and was dismissable, which was
/// already wrong; splitting the state out is what made the mistake visible.
pub fn dismissable(status: Status) -> bool {
    !matches!(status, Status::Working | Status::Delegated | Status::Stalled)
}

/// Whether a row of this status, this old, still belongs on the board.
///
/// One rule now, and it is a rule about ownership rather than age. Work a human
/// owes does not age out at all: an untested write (`NeedsTest`), an unanswered
/// question (`Blocked`), a dead turn (`Errored`). None of the three stops being
/// owed because time passed, and an earlier build that aged them out silently
/// reaped a night's untested edits before their author woke -- which is the bug
/// this removes. They leave the board only when a human clears them (ack, defer
/// or dismiss), never on a clock.
///
/// `Stalled` is the one attention state that still ages, because nobody owes it
/// anything: it is a guess about a quiet tool call, most often a slow build, and
/// a guess is only as good as the hour it was made in.
///
/// Everything else is unbounded here on purpose. `Working` and `Delegated` are
/// claims about right now and are re-derived every tick; `AwaitingAck` and
/// `Clear` have their own exit (`DORMANT_AFTER_MS`) inside the classifier.
///
/// A free function, and the reason is that it is the rule this file exists to
/// get right: `Board::refresh` needs a whole scanner and a `~/.claude` full of
/// real logs, so a horizon tested only through it is a horizon tested by running
/// the program and squinting.
pub fn within_horizon(status: Status, age_ms: i64) -> bool {
    match status {
        Status::NeedsTest | Status::Errored | Status::Blocked => true,
        Status::Stalled => age_ms <= ATTENTION_HORIZON_MS,
        _ => true,
    }
}

pub struct Board {
    scanner: Scanner,
    store: AckStore,
    agent: Agent,
    /// The rows to show, ranked. Rows filtered out by the horizon or the clear
    /// collapse are counted below rather than kept here.
    pub rows: Vec<Row>,
    /// Rows past their horizon, kept out of `rows` but counted. Only `Stalled`
    /// reaches here now -- a guess past `ATTENTION_HORIZON_MS` -- because owed
    /// work no longer ages out. See `within_horizon`.
    pub hidden_stale: usize,
    /// Clear sessions are counted but kept out of `rows` unless `show_clear`.
    pub clear_count: usize,
    pub show_clear: bool,
    pub show_all: bool,
    /// The repo's directory name -- what a header calls it.
    pub repo_label: String,
}

impl Board {
    /// Build a board and take its first reading. The scan is incremental from
    /// here on, so this call is the expensive one.
    pub fn new(repo_root: PathBuf, agent: Agent) -> Self {
        Self::with_store(repo_root, agent, AckStore::load())
    }

    /// `new` against a store the caller built, which in practice means one
    /// pointed at a scratch directory by `AckStore::load_at`.
    ///
    /// The same seam `load_at` opened one level down, and for the same reason:
    /// the suppression rules are the product here, and a rule that can only be
    /// exercised against the real `~/.claude` is a rule that is tested by
    /// running the program and squinting.
    pub fn with_store(repo_root: PathBuf, agent: Agent, store: AckStore) -> Self {
        let repo_label = repo_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());

        let mut board = Self {
            scanner: Scanner::new(repo_root, agent),
            store,
            agent,
            rows: Vec::new(),
            hidden_stale: 0,
            clear_count: 0,
            show_clear: false,
            show_all: false,
            repo_label,
        };
        board.refresh();
        board
    }

    pub fn agent(&self) -> Agent {
        self.agent
    }

    pub fn repo_root(&self) -> &Path {
        self.scanner.repo_root()
    }

    pub fn log_dir(&self) -> &Path {
        self.scanner.log_dir()
    }

    pub fn store_len(&self) -> usize {
        self.store.session_count()
    }

    pub fn row(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }

    /// Tail the logs and rebuild the row set, ranked most-urgent first.
    pub fn refresh(&mut self) {
        let now = now_ms();
        let sessions = self.scanner.refresh();

        let mut rows: Vec<Row> = sessions
            .into_iter()
            .filter_map(|s| self.to_row(s, now))
            .collect();

        rows.sort_by(|a, b| {
            a.status
                .rank()
                .cmp(&b.status.rank())
                // Within the blocked band, a certain question outranks a guessed
                // approval outranks a plain idle-stop (BlockedReason is ordered).
                .then(a.blocked_reason.cmp(&b.blocked_reason))
                .then(b.last_activity.cmp(&a.last_activity))
        });

        // Collapse what has genuinely gone stale. Only a `Stalled` guess ages
        // out here now: it is a quiet tool call the log cannot read, and one
        // that has sat for half a day is almost certainly a build that finished
        // unobserved rather than a prompt still waiting. Owed work -- untested
        // edits, a blocked question, an errored turn -- is deliberately NOT
        // collapsed: it stays until a human clears it, which is the whole point
        // of the change. The horizon is lifted by `show_all`, so even the guess
        // is only put away, never destroyed.
        let before = rows.len();
        if !self.show_all {
            rows.retain(|r| within_horizon(r.status, now.saturating_sub(r.last_activity)));
        }
        self.hidden_stale = before - rows.len();

        // Idle sessions carry no action. Counting them is useful; giving each
        // one three lines of the window is not.
        self.clear_count = rows.iter().filter(|r| r.status == Status::Clear).count();
        if !self.show_clear {
            rows.retain(|r| r.status != Status::Clear);
        }

        self.rows = rows;
    }

    /// Pick up acks made by another process, then rebuild.
    ///
    /// A board left open all day beside a sauron pane would otherwise keep
    /// showing work as untested that you acked in the pane an hour ago: the file
    /// changed and nothing re-read it. Safe on every tick because a save follows
    /// each mutation immediately, so there is never an unsaved journal to lose.
    pub fn resync(&mut self) {
        self.store.reload();
        self.refresh();
    }

    fn to_row(&self, s: Session, now: i64) -> Option<Row> {
        // Dismissed sessions leave before any status is computed. Everything
        // downstream of the row set -- the census, `hot_paths`, `waiting_count`,
        // muthur's alert strip -- then agrees without being told separately,
        // which is the whole reason this is a filter here rather than a flag on
        // the Row that each of them would have to remember to check.
        if self.store.is_dismissed(&s.id) {
            return None;
        }

        let acked = self.store.for_session(&s.id);
        let mut status = s.status(now, acked);
        let mut blocked_reason = s.blocked_reason(now, acked);
        let pending: Vec<String> = s.pending(acked, now).into_iter().map(String::from).collect();

        // A deferred "waiting on you" / "awaiting acknowledgement" session drops
        // off the board until the agent does something new. Compared against
        // last_activity, not a flag, so a fresh turn (which advances
        // last_activity) re-surfaces it. Both waiting states share the gesture:
        // deferring is exactly "I have acknowledged this".
        if matches!(status, Status::Blocked | Status::AwaitingAck) {
            if let Some(d) = self.store.deferred_at(&s.id) {
                if s.last_activity <= d {
                    status = Status::Clear;
                    blocked_reason = None;
                }
            }
        }

        // A session with no repo edits still matters while it is live -- it is
        // holding an agent slot, and if it is blocked on a question the delay is
        // yours to clear. Only drop it once it has gone quiet with nothing to
        // show, which is what a finished chat-only session looks like.
        if s.edits.is_empty() && status == Status::Clear {
            return None;
        }
        if status == Status::Clear && now.saturating_sub(s.last_activity) > DORMANT_AFTER_MS {
            return None;
        }

        Some(Row {
            id: s.id.clone(),
            id_short: s.short_id().to_string(),
            name: s.display_name(),
            branch: s.branch.clone(),
            last_activity: s.last_activity,
            turn_started: s.turn_started_ms,
            status,
            blocked_reason,
            error: s.error,
            pending,
            total_edits: s.edits.len(),
            tokens: s.total_tokens(),
            last_prompt: s.last_prompt.clone(),
            is_orc: s.is_orc,
            continue_cmd: s.continue_command(),
            // Drop the per-file timestamp here -- it did its job ordering the
            // previews during the fold; the card only needs the lines.
            previews: s
                .previews
                .into_iter()
                .map(|(path, (_, lines))| (path, lines))
                .collect(),
            edits: s.edits,
        })
    }

    /// Context-sensitive "I have handled this": on a waiting session (Blocked or
    /// AwaitingAck) it defers that waiting state; on an untested session it acks
    /// the write-set. Both re-surface if the agent does something new -- which is
    /// what separates this from `dismiss`, below.
    ///
    /// Addressed by id rather than index because the row order changes under
    /// every refresh, and acking by position is how you ack the wrong session.
    pub fn ack(&mut self, id: &str) {
        let Some(row) = self.row(id) else {
            return;
        };
        if matches!(row.status, Status::Blocked | Status::AwaitingAck) {
            let (id, ts) = (row.id.clone(), row.last_activity);
            self.store.defer(&id, ts);
        } else {
            let (id, edits) = (row.id.clone(), row.edits.clone());
            self.store.ack(&id, &edits);
        }
        self.persist();
        self.refresh();
    }

    /// Undo whichever suppression applies to this row.
    pub fn unack(&mut self, id: &str) {
        let Some(row) = self.row(id) else {
            return;
        };
        let (id, waiting) = (
            row.id.clone(),
            matches!(row.status, Status::Blocked | Status::AwaitingAck),
        );
        if waiting {
            self.store.undefer(&id);
        } else {
            self.store.unack(&id);
        }
        self.persist();
        self.refresh();
    }

    /// Take a finished session off the board for good.
    ///
    /// The gesture `ack` is not: no timestamp, no comparison, nothing the agent
    /// can do to bring it back. It is for the rows that are simply over -- a
    /// thread from Tuesday, a session whose terminal is long closed -- which
    /// `ack` handles badly, because on an untested row `ack` records the
    /// write-set as *tested at these timestamps*, and saying "I tested it" to
    /// clear away junk is exactly the lie the timestamped ack store exists to
    /// make impossible.
    ///
    /// Returns what happened so a caller can say so. Silence would be the worst
    /// outcome here: a key that does nothing on a running session is
    /// indistinguishable from a key that is not bound.
    pub fn dismiss(&mut self, id: &str) -> Dismissal {
        let Some(row) = self.row(id) else {
            return Dismissal::NoSuchRow;
        };
        if !dismissable(row.status) {
            return Dismissal::StillRunning;
        }
        let (id, name) = (row.id.clone(), crate::model::collapse_ws(&row.name));
        self.store.dismiss(&id);
        self.persist();
        self.refresh();
        Dismissal::Done(name)
    }

    /// Put a dismissed session back. The row returns on the next refresh at
    /// whatever status it now classifies as, which may not be the status it had
    /// when it was dismissed -- the dismissal suppressed the row, it did not
    /// freeze it.
    pub fn restore(&mut self, id: &str) {
        self.store.undismiss(id);
        self.persist();
        self.refresh();
    }

    /// Un-dismiss everything, for the `--restore-dismissed` escape hatch.
    pub fn restore_all_dismissed(&mut self) -> usize {
        let n = self.store.restore_all_dismissed();
        self.persist();
        self.refresh();
        n
    }

    pub fn dismissed_count(&self) -> usize {
        self.store.dismissed_count()
    }

    /// Ack every outstanding session, including ones hidden by the horizon.
    /// This is the cold-start move: declare the historical backlog tested so the
    /// queue starts empty and only new agent work appears.
    pub fn baseline(&mut self) {
        let saved = self.show_all;
        self.show_all = true;
        self.refresh();
        self.ack_all();
        self.show_all = saved;
        self.refresh();
    }

    pub fn ack_all(&mut self) {
        let all: Vec<(String, BTreeMap<String, i64>)> = self
            .rows
            .iter()
            .filter(|r| r.status == Status::NeedsTest)
            .map(|r| (r.id.clone(), r.edits.clone()))
            .collect();
        for (id, edits) in all {
            self.store.ack(&id, &edits);
        }
        self.persist();
        self.refresh();
    }

    /// Write the ack state through. Returns whether it landed, so a caller can
    /// flash a confirmation only when there is something to confirm.
    pub fn persist(&mut self) -> bool {
        self.store.save().is_ok()
    }

    /// Repo-relative paths a live session is touching. An orc must steer clear
    /// of every one of them, or its refactor collides with a hobbit mid-edit.
    ///
    /// Read straight off the rows already on screen, so it is the same hot set
    /// the user can see.
    pub fn hot_paths(&self) -> BTreeSet<String> {
        let mut hot = BTreeSet::new();
        for r in &self.rows {
            // `Stalled` is in this set because it was in it before the split --
            // those sessions arrived here as `Blocked` -- and because an open
            // tool call is the definition of the state. Dropping it would tell
            // the next orc that a file some Bash call is halfway through writing
            // is cold.
            if matches!(
                r.status,
                Status::Working
                    | Status::Delegated
                    | Status::Blocked
                    | Status::Stalled
                    | Status::NeedsTest
            ) {
                hot.extend(r.edits.keys().cloned());
            }
        }
        hot
    }

    /// The count that belongs in a header: sessions wanting a decision from you.
    /// Blocked and awaiting-ack both qualify -- both are stalled on a human.
    pub fn waiting_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.status, Status::Blocked | Status::AwaitingAck))
            .count()
    }

    /// Sessions mid-turn right now: computing, waiting on a sub-agent, or quiet
    /// with a tool call still open. `Stalled` belongs here rather than in
    /// `waiting_count` -- it is a guess about a slow command, and counting it as
    /// a demand on the human is exactly the over-reporting the split removed.
    pub fn working_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                matches!(
                    r.status,
                    Status::Working | Status::Delegated | Status::Stalled
                )
            })
            .count()
    }

    pub fn needs_test_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == Status::NeedsTest)
            .count()
    }
}

/// Walk up from the cwd looking for a .git entry, so the tool works from any
/// subdirectory of the repo.
pub fn git_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The in-flight sessions for a repo -- mid-turn (`Working`), waiting on a
/// background agent it spawned (`Delegated`), or quiet with a tool call still
/// open (`Stalled`) -- as `(session_id, display_name)`. The same set
/// `--list-working` prints and the TUI shows, so `sauron workspace` reopens
/// exactly the sessions the tool counts as live.
pub fn in_flight_tasks(repo_root: PathBuf, agent: Agent) -> Vec<(String, String)> {
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    Board::new(repo_root, agent)
        .rows
        .iter()
        .filter(|r| {
            matches!(
                r.status,
                Status::Working | Status::Delegated | Status::Stalled
            )
        })
        .map(|r| (r.id.clone(), crate::model::collapse_ws(&r.name)))
        .collect()
}

/// Repo-relative paths an *active* session is touching -- working, delegated,
/// blocked, or holding untested edits. This is the "hot" set an orc must steer
/// clear of, so its single-shot refactor never collides with a hobbit's work.
pub fn hot_files(repo_root: PathBuf, agent: Agent) -> BTreeSet<String> {
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    Board::new(repo_root, agent).hot_paths()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal is a safety property, so it is tested against the set it
    /// protects rather than by restating the match arm: every status that
    /// contributes to `hot_paths` while an agent is actually writing must be
    /// undismissable, and everything else must be dismissable -- a board you
    /// cannot clear of dead rows is the failure this key exists to fix.
    #[test]
    fn only_a_session_nobody_is_writing_through_can_be_dismissed() {
        for s in [Status::Working, Status::Delegated, Status::Stalled] {
            assert!(!dismissable(s), "{s:?} is mid-turn; its files are still hot");
        }
        for s in [
            Status::Errored,
            Status::Blocked,
            Status::AwaitingAck,
            Status::NeedsTest,
            Status::Clear,
        ] {
            assert!(dismissable(s), "{s:?} is exactly what the key is for");
        }
    }

    /// `NeedsTest` is the case that matters most, and the reason `ack` is not a
    /// substitute: it is the only dismissable status that also carries an
    /// untested write-set, so clearing it with `ack` would record those paths as
    /// tested. Dismissing writes nothing to the ack map at all.
    #[test]
    fn dismissing_untested_work_does_not_claim_it_was_tested() {
        let dir = scratch("claim");
        let mut store = AckStore::load_at(dir.clone());
        store.dismiss("dead-session");
        store.save().unwrap();

        let fresh = AckStore::load_at(dir.clone());
        assert!(fresh.is_dismissed("dead-session"));
        assert!(
            fresh.for_session("dead-session").is_none(),
            "a dismissal recorded an ack it had no business recording"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The filter itself, which is the one line the whole gesture rests on.
    ///
    /// Driven through `to_row` rather than through a running board, because the
    /// scanner reads logs out of `~/.claude` and a test that needed real ones
    /// would be testing the machine it happens to run on.
    #[test]
    fn a_dismissed_session_never_becomes_a_row_whatever_it_is_doing() {
        let dir = scratch("filter");
        let mut store = AckStore::load_at(dir.clone());
        store.dismiss("gone");

        let board = Board::with_store(dir.join("repo"), Agent::Claude, store);
        let now = now_ms();

        // A session that would otherwise be the loudest thing on the board:
        // recent, mid-turn, holding unacked edits. Dismissal outranks all of it.
        let mut s = Session {
            id: "gone".into(),
            last_activity: now,
            ..Default::default()
        };
        s.edits.insert("src/a.rs".into(), now);
        assert!(board.to_row(s, now).is_none(), "a dismissed row came back");

        // And the filter is keyed on the id, not on anything about the session:
        // an identical one that was never dismissed still shows.
        let mut kept = Session {
            id: "kept".into(),
            last_activity: now,
            ..Default::default()
        };
        kept.edits.insert("src/a.rs".into(), now);
        assert!(board.to_row(kept, now).is_some(), "the filter caught a bystander");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Owed work never ages out here, and a guess still does.
    ///
    /// This inverts the old test. The bug it names was real -- a misclassified
    /// row sat first on the board until a keypress removed it -- but the fix for
    /// it aged out the states a human genuinely owes as well, so a night's
    /// untested edits vanished before their author woke. Only `Stalled`, the one
    /// guess nobody owes anything on, ages out now; the owed states stay until a
    /// human clears them.
    #[test]
    fn only_a_stalled_guess_ages_out_and_owed_work_never_does() {
        // The guess still ages: a quiet tool call half a day old is almost
        // certainly a build that finished unobserved, not a prompt still waiting.
        assert!(within_horizon(Status::Stalled, ATTENTION_HORIZON_MS - 1), "fresh guess");
        assert!(
            !within_horizon(Status::Stalled, ATTENTION_HORIZON_MS + 1),
            "a stale guess outlived its evidence"
        );

        // Everything else is unbounded here. The owed states -- NeedsTest,
        // Blocked, Errored -- never age out at all, which is the fix; the live
        // states are re-derived every tick; AwaitingAck and Clear have their own
        // exit (DORMANT_AFTER_MS) inside the classifier.
        for s in [
            Status::NeedsTest,
            Status::Blocked,
            Status::Errored,
            Status::Working,
            Status::Delegated,
            Status::AwaitingAck,
            Status::Clear,
        ] {
            assert!(
                within_horizon(s, 30 * DORMANT_AFTER_MS),
                "{s:?} must not be aged out here"
            );
        }
    }

    /// A stalled session looks parked and is not: what defines the state is an
    /// open tool call, and the likeliest thing that call is doing is writing.
    /// `hot_paths` is what an orc consults before it is loosed on a file, so a
    /// gap here is a refactor landing on a file mid-edit.
    #[test]
    fn a_stalled_session_still_reports_its_files_as_hot() {
        let dir = scratch("hot");
        let store = AckStore::load_at(dir.clone());
        let mut board = Board::with_store(dir.join("repo"), Agent::Claude, store);
        let now = now_ms();
        let row = |id: &str, status: Status| Row {
            id: id.into(),
            id_short: id.into(),
            name: id.into(),
            branch: None,
            last_activity: now,
            turn_started: 0,
            status,
            blocked_reason: None,
            error: None,
            pending: Vec::new(),
            total_edits: 1,
            tokens: 0,
            last_prompt: None,
            is_orc: false,
            continue_cmd: String::new(),
            edits: [(format!("src/{id}.rs"), now)].into_iter().collect(),
            previews: BTreeMap::new(),
        };
        board.rows = vec![row("stalled", Status::Stalled), row("done", Status::Clear)];

        let hot = board.hot_paths();
        assert!(hot.contains("src/stalled.rs"), "a stalled session's files are hot");
        assert!(!hot.contains("src/done.rs"));

        // And it counts as live work, not as a demand on the human -- which is
        // the whole reason it was split out of the red band.
        assert_eq!(board.working_count(), 1);
        assert_eq!(board.waiting_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(what: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sauron-board-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
