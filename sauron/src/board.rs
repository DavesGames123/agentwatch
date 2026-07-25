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
//!   fn Board::ack       -- context-sensitive ack/dismiss by session id
//!   fn Board::hot_paths -- paths a live session is touching, for orc safety

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::model::{
    now_ms, BlockedReason, ErrorKind, Session, Status, DORMANT_AFTER_MS, STALE_HORIZON_MS,
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

pub struct Board {
    scanner: Scanner,
    store: AckStore,
    agent: Agent,
    /// The rows to show, ranked. Rows filtered out by the horizon or the clear
    /// collapse are counted below rather than kept here.
    pub rows: Vec<Row>,
    /// Rows past the staleness horizon, kept out of `rows` but counted.
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
        let repo_label = repo_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());

        let mut board = Self {
            scanner: Scanner::new(repo_root, agent),
            store: AckStore::load(),
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

        // Collapse the historical backlog. Sessions from days ago were tested
        // (or abandoned) by whatever process preceded this tool; listing them as
        // outstanding buries today's actual work.
        let before = rows.len();
        if !self.show_all {
            rows.retain(|r| {
                r.status != Status::NeedsTest
                    || now.saturating_sub(r.last_activity) <= STALE_HORIZON_MS
            });
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
        let acked = self.store.for_session(&s.id);
        let mut status = s.status(now, acked);
        let mut blocked_reason = s.blocked_reason(now, acked);
        let pending: Vec<String> = s.pending(acked, now).into_iter().map(String::from).collect();

        // A dismissed "waiting on you" / "awaiting acknowledgement" session drops
        // off the board until the agent does something new. Compared against
        // last_activity, not a flag, so a fresh turn (which advances
        // last_activity) re-surfaces it. Both waiting states share the gesture:
        // dismissing is exactly "I have acknowledged this".
        if matches!(status, Status::Blocked | Status::AwaitingAck) {
            if let Some(d) = self.store.dismissed_at(&s.id) {
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
    /// AwaitingAck) it dismisses that waiting state; on an untested session it
    /// acks the write-set. Both re-surface if the agent does something new.
    ///
    /// Addressed by id rather than index because the row order changes under
    /// every refresh, and acking by position is how you ack the wrong session.
    pub fn ack(&mut self, id: &str) {
        let Some(row) = self.row(id) else {
            return;
        };
        if matches!(row.status, Status::Blocked | Status::AwaitingAck) {
            let (id, ts) = (row.id.clone(), row.last_activity);
            self.store.dismiss(&id, ts);
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
            self.store.undismiss(&id);
        } else {
            self.store.unack(&id);
        }
        self.persist();
        self.refresh();
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
            if matches!(
                r.status,
                Status::Working | Status::Delegated | Status::Blocked | Status::NeedsTest
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

    /// Sessions mid-turn right now, including ones waiting on a sub-agent.
    pub fn working_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.status, Status::Working | Status::Delegated))
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

/// The in-flight sessions for a repo -- mid-turn (`Working`) or waiting on a
/// background agent it spawned (`Delegated`) -- as `(session_id, display_name)`.
/// The same set `--list-working` prints and the TUI shows, so `sauron workspace`
/// reopens exactly the sessions the tool counts as live.
pub fn in_flight_tasks(repo_root: PathBuf, agent: Agent) -> Vec<(String, String)> {
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    Board::new(repo_root, agent)
        .rows
        .iter()
        .filter(|r| matches!(r.status, Status::Working | Status::Delegated))
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
