//! sauron -- a sidecar that answers one question: what did my agents change
//! that I have not tested yet?
//!
//! Everything it shows is read out of the Claude Code session logs at
//! ~/.claude/projects/<encoded-repo-path>/*.jsonl. It writes nothing to the repo
//! and never talks to a running agent.
//!
//! Usage:
//!   sauron                  # watch the repo containing the cwd
//!   sauron /path/to/repo    # watch a specific repo
//!   sauron serve            # the board as a web app, agents running inside it
//!
//! `serve` is not this file's business past the dispatch below: it does not use
//! `App`, it does not draw, and it owns the process once it is called. The two
//! front ends share `board::Board` and nothing else, which is the split that
//! lets one be a terminal and the other be a web application instead of forcing
//! either to pretend to be the other. See `web`'s header.
//!
//! The watching itself lives in the library (`board::Board`), shared with the
//! `muthur` multi-project front end; this file is the terminal around it.
//!
//! grep targets:
//!   struct App          -- a Board plus this window's cursor and banners
//!   fn App::refresh     -- rebuild rows, keeping the cursor on its session
//!   fn App::resync      -- refresh that also picks up another process's acks
//!   fn App::jump_table  -- Tab, from one of the board's tables to the next
//!   fn App::scroll_at   -- the wheel, which moves a table and not the cursor
//!   fn hit_test         -- a clicked row of the screen -> the session in it
//!   fn main             -- terminal lifecycle and event loop

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;

use sauron::agent::Agent;
use sauron::board::{Board, Dismissal};
use sauron::model::{self, now_ms, Status};
use sauron::plat;
#[cfg(unix)]
use sauron::{gui, reply};
use sauron::{beacon, clip, git_root, handoff, orc, panel, ui, web, workspace};

/// How often the logs are re-tailed. Only appended bytes are parsed, so this is
/// cheap even with 10MB session files.
const TICK: Duration = Duration::from_millis(2000);
// The data only changes on TICK, but the Eye of Sauron animates far faster, so
// the loop wakes on this shorter frame clock to redraw. ratatui diffs the
// buffer and writes only changed cells, so an idle frame -- the Eye holding
// still -- costs nothing on the wire.
const FRAME: Duration = Duration::from_millis(100);
/// How many cold targets the picker offers. The list is ranked, so anything past
/// the first screenful or two is noise -- and the cap keeps the per-frame copy of
/// it into the view cheap on a repository with thousands of source files.
const PICK_LIMIT: usize = 40;
/// The port `sauron serve` binds, on loopback.
///
/// Loopback is the default and not a suggestion. `serve` runs agents and shells
/// on this machine on behalf of whoever is holding the page, so an open port is
/// an open shell -- see the warning at the `--bind` site.
const DEFAULT_PORT: u16 = 7373;
/// How many in-flight sessions `serve` reopens as tabs when it starts, unless
/// `--agents` says otherwise. Matches what a `sauron workspace` launch does.
const DEFAULT_AGENTS: usize = 4;

/// The TUI's state: a `Board` plus everything about *this window* -- where the
/// cursor is, which banners are still up, where the last frame drew its rows.
///
/// The split is deliberate. Everything that answers "what is the state of this
/// repo" lives in `board`, so `muthur` can ask the same questions without a
/// terminal; everything here is unshareable by construction, because a second
/// front end has its own cursor and its own geometry.
struct App {
    board: Board,
    /// Index into `board.rows` of the row the cursor is on. The board draws that
    /// list as three tables in row order, so one index is still the whole
    /// cursor: `j`/`k` spill from the foot of one table into the head of the
    /// next without either side having to know where the boundary is.
    selected: usize,
    saved_until: Option<Instant>,
    /// Transient "copied" banner deadline.
    copied_until: Option<Instant>,
    /// Result of the last pane spawn and its banner deadline. Success and
    /// failure share the slot: both are the answer to a key just pressed, and
    /// only the last one matters.
    spawn_msg: Option<(String, bool)>,
    spawn_until: Option<Instant>,
    /// Per table, the first row it is showing. Rewritten from the frame geometry
    /// after every draw -- only the drawing knows how many rows a table got --
    /// and carried here so a wheel event has something to move.
    scroll: [usize; ui::GROUPS],
    /// Whether the next frame should scroll to keep the cursor on screen. Set by
    /// everything that moves the cursor, cleared by the wheel. Without it the
    /// wheel would appear dead: the frame after a scroll would chase the
    /// selection straight back to where it was.
    follow: bool,
    /// Where the last frame put each table, so a click or a wheel event resolves
    /// to the row -- or the table -- under the pointer.
    tables: Vec<ui::TableGeometry>,
    /// The machine's UTC offset in seconds, read once at launch so task start
    /// times render on the local wall clock without re-shelling `date` per frame.
    local_offset: i64,
    /// The cold-target picker, while it is open over the board. `Some` swallows
    /// the normal keymap, so j/k/Enter mean "inside the picker" and nothing can
    /// be acked by accident while choosing a file.
    orc_pick: Option<OrcPick>,
    /// The session `D` last dismissed, so `U` can put it back.
    ///
    /// One slot, not a stack. A dismissed row is off the board, so the only way
    /// back to it is a key that does not need a cursor -- and a *list* of them
    /// would be a second board showing exactly what you just said you did not
    /// want to see. One slot covers the case this is for, which is the mis-hit
    /// you notice immediately; `--restore-dismissed` covers the rest.
    last_dismissed: Option<String>,
}

/// The open cold-target picker: what an orc could be loosed on right now, and
/// what was ruled out. Rebuilt each time it opens rather than cached, because
/// the whole point of dispatching from the TUI is that the hot set is *live* --
/// a launch-time snapshot is exactly what made `--orcs N` unable to answer
/// "what is safe now".
struct OrcPick {
    cold: Vec<orc::Target>,
    /// Candidates excluded because a live session is editing them.
    hot: usize,
    /// Candidates excluded because git reports them dirty.
    dirty: usize,
    selected: usize,
}

impl App {
    fn new(repo_root: PathBuf, agent: Agent) -> Self {
        let mut app = Self {
            board: Board::new(repo_root, agent),
            selected: 0,
            saved_until: None,
            copied_until: None,
            spawn_msg: None,
            spawn_until: None,
            scroll: [0; ui::GROUPS],
            follow: true,
            tables: Vec::new(),
            local_offset: model::local_offset_secs(),
            orc_pick: None,
            last_dismissed: None,
        };
        app.focus_first_actionable();
        app
    }

    /// On launch, land the cursor on the thing most likely to want the user:
    /// a session waiting on them, else one actively working, else untested work.
    /// Runs once at startup; later refreshes preserve the selection by id, so the
    /// cursor is not yanked around as statuses churn.
    fn focus_first_actionable(&mut self) {
        let order = [
            Status::Blocked,
            Status::AwaitingAck,
            Status::Working,
            Status::NeedsTest,
        ];
        for want in order {
            if let Some(i) = self.board.rows.iter().position(|r| r.status == want) {
                self.selected = i;
                self.follow_selection();
                return;
            }
        }
    }

    /// Rebuild the board and put the cursor back on the same session.
    ///
    /// Selection is re-found by id, never by index: rows reorder as statuses
    /// change, and moving the cursor out from under the user is the fastest way
    /// to make them ack the wrong thing.
    fn refresh(&mut self) {
        let anchor = self.selected_id();
        self.board.refresh();
        self.reanchor(anchor);
    }

    /// A refresh that also picks up acks made elsewhere -- a muthur board, or a
    /// second sauron on the same repo. Runs on the tick rather than on a keypress
    /// because the whole point is that nobody pressed anything here.
    fn resync(&mut self) {
        let anchor = self.selected_id();
        self.board.resync();
        self.reanchor(anchor);
    }

    fn selected_id(&self) -> Option<String> {
        self.board.rows.get(self.selected).map(|r| r.id.clone())
    }

    fn reanchor(&mut self, anchor: Option<String>) {
        self.selected = anchor
            .and_then(|id| self.board.rows.iter().position(|r| r.id == id))
            .unwrap_or(0)
            .min(self.board.rows.len().saturating_sub(1));
        self.follow_selection();
    }

    /// Ask the next frame to bring the cursor back on screen.
    ///
    /// Called by everything that moves the cursor rather than scrolling a table
    /// under it. The tables clamp their own offsets, so this is a request and not
    /// a scroll: the drawing is the only thing that knows how many rows each
    /// table ended up with.
    fn follow_selection(&mut self) {
        self.follow = true;
    }

    /// `Tab` -- put the cursor on the head of the next table.
    ///
    /// `j`/`k` already cross the boundaries, but a board with a long WORKING
    /// table makes reaching the one below it a held key. Driven off the rows'
    /// own grouping rather than off the drawn geometry: a pane too short to draw
    /// every table still has to be able to reach the ones it left off.
    fn jump_table(&mut self, dir: isize) {
        let groups: Vec<ui::Group> = self
            .board
            .rows
            .iter()
            .map(|r| ui::group_of(r.status))
            .collect();
        if let Some(i) = next_table_row(&groups, self.selected, dir) {
            self.selected = i;
            self.follow_selection();
        }
    }

    /// Scroll the table under the pointer, leaving the cursor where it is.
    ///
    /// The wheel used to move the selection, which on a board of separately
    /// scrolled tables would mean the wheel could only ever reach whichever
    /// table the cursor happened to be in. It moves a viewport now; the cursor
    /// answers to the keyboard. With the pointer off the tables -- the gap
    /// between them, the detail pane -- the cursor's own table is scrolled, so a
    /// wheel roll is never simply ignored.
    fn scroll_at(&mut self, y: u16, delta: isize) {
        let target = table_at(y, &self.tables)
            .or_else(|| self.tables.iter().find(|t| t.rows.contains(&self.selected)))
            .map(|t| {
                (
                    t.group.index(),
                    t.rows.len().saturating_sub(t.body_height as usize),
                )
            });
        let Some((slot, max)) = target else {
            return;
        };
        self.scroll[slot] = (self.scroll[slot] as isize + delta).clamp(0, max as isize) as usize;
        self.follow = false;
    }

    fn move_by(&mut self, delta: isize) {
        if self.board.rows.is_empty() {
            return;
        }
        let last = self.board.rows.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.follow_selection();
    }

    /// `a` is context-sensitive: on a waiting session (Blocked or AwaitingAck) it
    /// defers that waiting state; on an untested session it acks the write-set.
    /// Both mean "I have handled this", and both re-surface if the agent does
    /// something new. `D` is the one that does not -- see `dismiss_selected`.
    fn ack_selected(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.board.ack(&id);
        self.flash_saved();
        self.reanchor(Some(id));
    }

    fn unack_selected(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.board.unack(&id);
        self.flash_saved();
        self.reanchor(Some(id));
    }

    fn ack_all(&mut self) {
        let anchor = self.selected_id();
        self.board.ack_all();
        self.flash_saved();
        self.reanchor(anchor);
    }

    /// `D` -- take the selected session off the board for good.
    ///
    /// The cursor is deliberately *not* re-anchored to the dismissed id: the row
    /// is gone, so `reanchor` on it would fall back to index 0 and throw you to
    /// the top of the list. It holds its position instead, which puts it on
    /// whatever moved up into the gap -- the same thing that happens when a row
    /// clears on its own.
    fn dismiss_selected(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        match self.board.dismiss(&id) {
            Dismissal::Done(name) => {
                self.last_dismissed = Some(id);
                self.flash_spawn(format!("dismissed {name} — U to undo"), true);
                self.selected = self
                    .selected
                    .min(self.board.rows.len().saturating_sub(1));
                self.follow_selection();
            }
            Dismissal::StillRunning => {
                self.flash_spawn("still running — dismiss is for finished work".into(), false)
            }
            Dismissal::NoSuchRow => {}
        }
    }

    /// `U` -- put back whatever `D` last took away.
    fn restore_last_dismissed(&mut self) {
        let Some(id) = self.last_dismissed.take() else {
            self.flash_spawn("nothing to restore".into(), false);
            return;
        };
        self.board.restore(&id);
        self.flash_spawn("restored".into(), true);
        self.reanchor(Some(id));
    }

    fn flash_saved(&mut self) {
        self.saved_until = Some(Instant::now() + Duration::from_millis(1200));
    }

    fn saved_flash(&self) -> bool {
        self.saved_until.map(|t| Instant::now() < t).unwrap_or(false)
    }

    fn copied_flash(&self) -> bool {
        self.copied_until.map(|t| Instant::now() < t).unwrap_or(false)
    }

    /// Copy the selected session's resume command to the clipboard.
    fn copy_selected_continue(&mut self) {
        self.copy_continue_for(self.selected);
    }

    fn copy_continue_for(&mut self, idx: usize) {
        let Some(row) = self.board.rows.get(idx) else {
            return;
        };
        if copy_to_clipboard(&row.continue_cmd) {
            self.selected = idx;
            self.follow_selection();
            self.copied_until = Some(Instant::now() + Duration::from_millis(1600));
        }
    }

    /// The last pane-spawn result, while its banner is still up.
    fn spawn_flash(&self) -> Option<(&str, bool)> {
        let live = self.spawn_until.map(|t| Instant::now() < t).unwrap_or(false);
        live.then_some(self.spawn_msg.as_ref())
            .flatten()
            .map(|(m, ok)| (m.as_str(), *ok))
    }

    /// Open one more agent pane in the workspace's left column, running a fresh
    /// agent at the repo root. Focus stays here, so the key can be held down to
    /// widen the swarm by several panes at once.
    fn spawn_agent(&mut self) {
        let cmd = format!(
            "cd {} && {}",
            self.board.repo_root().display(),
            self.board.agent().label()
        );
        self.spawn(cmd, false, format!("new {} pane", self.board.agent().label()));
    }

    /// Reopen the selected session in a new left-column pane, resumed. This is
    /// the counterpart to closing a pane you were done with: the session itself
    /// outlives its terminal, and this is how it gets one back. Focus follows,
    /// because you opened a named session in order to talk to it.
    fn spawn_selected(&mut self) {
        let Some(row) = self.board.rows.get(self.selected) else {
            return;
        };
        let (id, name) = (row.id.clone(), model::collapse_ws(&row.name));
        let cmd = format!(
            "cd {} && {}",
            self.board.repo_root().display(),
            self.board.agent().resume_cmd(&id)
        );
        let label: String = name.chars().take(32).collect();
        self.spawn(cmd, true, format!("opened {label}"));
    }

    fn spawn(&mut self, cmd: String, focus: bool, ok_msg: String) {
        let (msg, ok) = match workspace::spawn_left_pane(&cmd, focus) {
            Ok(()) => (ok_msg, true),
            Err(why) => (why, false),
        };
        self.flash_spawn(msg, ok);
    }

    // --- orcs ---

    /// Survey the repo and open the picker. Costs a `git ls-files` plus a read
    /// of every tracked source file, which is why it runs on the keypress rather
    /// than every tick.
    fn open_orc_pick(&mut self) {
        let repo = self.board.repo_root().to_path_buf();
        let mut survey = orc::survey(&repo, &self.board.hot_paths());
        // Nobody scrolls past the top of a ranked list to find a refactor
        // target, and the cap bounds both the scroll and the per-frame copy of
        // this list into the view.
        survey.cold.truncate(PICK_LIMIT);
        if survey.cold.is_empty() {
            self.flash_spawn(
                format!(
                    "no cold files to loose an orc on ({} hot, {} dirty)",
                    survey.hot, survey.dirty
                ),
                false,
            );
            return;
        }
        self.orc_pick = Some(OrcPick {
            cold: survey.cold,
            hot: survey.hot,
            dirty: survey.dirty,
            selected: 0,
        });
    }

    fn close_orc_pick(&mut self) {
        self.orc_pick = None;
    }

    fn orc_pick_move(&mut self, delta: isize) {
        let Some(p) = &mut self.orc_pick else {
            return;
        };
        let last = p.cold.len().saturating_sub(1);
        p.selected = (p.selected as isize + delta).clamp(0, last as isize) as usize;
    }

    /// Stage an orc on the picked file, in a new pane in sauron's own column.
    ///
    /// Staged, not run: the pane is left holding `sauron orc <file>` awaiting
    /// Enter, the same contract `--orcs N` has at launch. Outside a workspace
    /// window there is no pane to split, so the command goes to the clipboard
    /// instead -- the dispatch still works, it just needs somewhere to land.
    fn dispatch_orc(&mut self) {
        let Some(p) = &self.orc_pick else {
            return;
        };
        let Some(target) = p.cold.get(p.selected).map(|t| t.path.clone()) else {
            return;
        };
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sauron"));
        let repo = self.board.repo_root().to_string_lossy().into_owned();
        let cmd = orc::stage_command(&exe, &repo, &target, "");
        self.orc_pick = None;

        let (msg, ok) = match workspace::spawn_orc_pane(&cmd) {
            Ok(()) => (format!("orc staged on {target} — press Enter in its pane"), true),
            Err(why) if copy_to_clipboard(&cmd) => {
                (format!("{why}; orc command copied instead"), false)
            }
            Err(why) => (why, false),
        };
        self.flash_spawn(msg, ok);
    }

    /// Put a message in the footer's spawn slot for a beat.
    fn flash_spawn(&mut self, msg: String, ok: bool) {
        self.spawn_msg = Some((msg, ok));
        // Failures earn a longer read than confirmations -- they say what to fix.
        self.spawn_until =
            Some(Instant::now() + Duration::from_millis(if ok { 1600 } else { 3500 }));
    }

    /// Map a terminal cell to the row rendered under it, using the geometry
    /// captured on the last frame.
    fn row_at(&self, click_y: u16) -> Option<usize> {
        hit_test(click_y, &self.tables)
    }
}

/// The table drawn under a y-coordinate, if the pointer is over one at all.
///
/// Body rows only: a header, the blank row between two tables and the collapsed
/// clear count are all deliberately not part of any table, because a click there
/// picked a row nobody pointed at.
fn table_at(y: u16, tables: &[ui::TableGeometry]) -> Option<&ui::TableGeometry> {
    tables
        .iter()
        .find(|t| y >= t.body_top && y < t.body_top + t.body_height)
}

/// Resolve a click's y-coordinate to a row index, using the geometry of the last
/// frame. Each table scrolls separately, so the answer is "which table is under
/// the pointer, plus that table's own offset" -- one list walked from one offset
/// cannot answer it any more. Pure arithmetic, split out from `App` so it can be
/// tested without a terminal.
fn hit_test(click_y: u16, tables: &[ui::TableGeometry]) -> Option<usize> {
    let t = table_at(click_y, tables)?;
    t.rows.get(t.offset + (click_y - t.body_top) as usize).copied()
}

/// The row `Tab` lands on: the head of the next table round from the one holding
/// the cursor. It wraps, so Tab on its own walks the whole board.
///
/// Takes the group of every row, in board order, because that is what the tables
/// are: rows arrive ranked, and a table is a run of that ranking. Nothing here
/// consults the screen, so a table the pane was too short to draw is still on
/// the way round.
fn next_table_row(groups: &[ui::Group], selected: usize, dir: isize) -> Option<usize> {
    let mut order: Vec<ui::Group> = Vec::new();
    for g in groups {
        if !order.contains(g) {
            order.push(*g);
        }
    }
    if order.is_empty() {
        return None;
    }
    let here = groups
        .get(selected)
        .and_then(|g| order.iter().position(|o| o == g))
        .unwrap_or(0) as isize;
    let to = order[(here + dir).rem_euclid(order.len() as isize) as usize];
    groups.iter().position(|g| *g == to)
}

/// Put text on the system clipboard. Tries the platform's own mechanism first
/// (`pbcopy`, `clip.exe`), then falls back to an OSC 52 escape so it still works
/// over SSH or on a bare terminal.
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;

    if plat::clipboard_write(text) {
        return true;
    }

    // OSC 52: ESC ] 52 ; c ; <base64> BEL -- ask the terminal to set the
    // clipboard. Written straight to the tty; harmless if unsupported.
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    std::io::stdout().write_all(seq.as_bytes()).is_ok()
        && std::io::stdout().flush().is_ok()
}

/// OSC 52 and the mirror's OSC 1337 both base64 a payload for the same
/// terminal, so they share one encoder. It lives in `plat` rather than `mirror`
/// because the clipboard needs it on every platform and the mirror is macOS
/// only.
use sauron::plat::base64;
use sauron::route;

fn main() -> std::io::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // The clipboard is available both as `sauron clip ...` and as the
    // standalone `clip` binary. Keep it before the watcher argument grammar.
    if args.first().map(|s| s.as_str()) == Some("clip") {
        std::process::exit(clip::run(args[1..].to_vec()));
    }

    // Internal strict-lifecycle wrapper emitted by `workspace
    // --clipboard-handoff`; intentionally omitted from the public CLI surface.
    if args.first().map(|s| s.as_str()) == Some("handoff-run") {
        return handoff::run(&args[1..]);
    }

    // Which agent to watch: an explicit `--claude`/`--codex` flag; otherwise
    // `$SAURON_AGENT`, then auto-detect (resolved once the repo is known).
    let explicit_agent = if args.iter().any(|a| a == "--codex") {
        Some(Agent::Codex)
    } else if args.iter().any(|a| a == "--claude") {
        Some(Agent::Claude)
    } else {
        None
    };

    // `sauron workspace ...` -- open or manage the multi-agent iTerm layout.
    // Handled before the repo/TUI path since it has its own argument grammar.
    if args.first().map(|s| s.as_str()) == Some("workspace") {
        return workspace::run(&args[1..], explicit_agent);
    }

    // `sauron orc <file>` -- loose a single-shot maintenance agent on one cold
    // file. This is what an orc pane runs, whether the pane was staged at
    // workspace launch or dispatched from the TUI picker; it builds the charge
    // and execs the agent, so the pane ends up owned by the agent itself.
    if args.first().map(|s| s.as_str()) == Some("orc") {
        return orc::run(&args[1..], explicit_agent);
    }

    // `sauron gui [repo]` -- launch the repo's declared application and dock its
    // window into the hole the workspace layout leaves for it. This is what the
    // app-log pane runs, and it is inert in any repo with no `.sauron/gui.conf`.
    if args.first().map(|s| s.as_str()) == Some("gui") {
        #[cfg(unix)]
        return gui::run(&args[1..]);
        // Docking a window into the pane grid is the window server's job, and
        // this platform's has no scripting surface to ask. Said out loud rather
        // than treated as an unknown subcommand: the difference between "sauron
        // cannot do this here" and "you typed it wrong" is the difference
        // between one minute and twenty.
        #[cfg(not(unix))]
        {
            eprintln!("{}", plat::unsupported("sauron gui"));
            return Ok(());
        }
    }

    // `sauron panel ...` -- install the in-app attention pane into a Rust
    // project, or print what that pane would currently be showing. Handled
    // before the repo/TUI path: `install` writes to a project that may have no
    // agent logs at all, so it must not depend on a board being buildable.
    if args.first().map(|s| s.as_str()) == Some("panel") {
        return panel::run(&args[1..]);
    }

    // `sauron route ...` -- what to open in order to look at a change, read out
    // of the watched repo's own `.sauron/panels.toml`. Sits beside `panel`
    // rather than inside it: it answers a question about the repo, not about
    // the pane, and must work in a checkout that never installed one.
    if args.first().map(|s| s.as_str()) == Some("route") {
        return route::run(&args[1..]);
    }

    // `sauron reply ...` -- say something to a running agent in its own
    // terminal. Beside `route` for the same reason: it is about the board, not
    // the pane, and must work with no pane installed anywhere.
    if args.first().map(|s| s.as_str()) == Some("reply") {
        #[cfg(unix)]
        return reply::run(&args[1..]);
        // Delivery needs to find the window a pid is typing into. iTerm2 answers
        // that by publishing each session's tty; Windows Terminal publishes
        // nothing and accepts no text. Queuing the message anyway would be the
        // worse failure -- it would sit in the outbox looking sent.
        #[cfg(not(unix))]
        {
            eprintln!("{}", plat::unsupported("sauron reply"));
            return Ok(());
        }
    }

    // `sauron serve` -- the board as a web application, with the agents running
    // as ptys this process owns. Taken out of `args` here (flags and values
    // both) so the repo-path rule further down -- "the first argument that is
    // not a flag" -- cannot resolve to `serve` or to a port number.
    let serve = args.first().map(|s| s.as_str()) == Some("serve");
    if serve {
        args.remove(0);
    }
    let port: u16 = take_flag(&mut args, "--port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let bind = take_flag(&mut args, "--bind").unwrap_or_else(|| "127.0.0.1".to_string());
    // How many already-running sessions to reopen as tabs at launch, the way
    // `sauron workspace` reopens them as panes. Capped rather than unbounded: a
    // repo with thirty historical sessions must not spawn thirty agents because
    // someone opened a browser.
    let agents: usize = take_flag(&mut args, "--agents")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_AGENTS)
        .min(12);

    let once = args.iter().any(|a| a == "--once");
    let baseline = args.iter().any(|a| a == "--baseline");
    // Age-scoped baseline: ack only untested work older than N hours, keeping
    // recent work. Draws a fresh line without eating the night's edits.
    let baseline_older_h: Option<i64> = take_flag(&mut args, "--baseline-older")
        .and_then(|v| v.parse::<i64>().ok());
    let list_working = args.iter().any(|a| a == "--list-working");
    let restore_dismissed = args.iter().any(|a| a == "--restore-dismissed");
    let repo_root = match args.iter().find(|a| !a.starts_with("--")) {
        Some(p) => PathBuf::from(p),
        None => git_root().unwrap_or(std::env::current_dir()?),
    };
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    let agent = Agent::select(explicit_agent, &repo_root);

    // The web front end owns the process from here: no `App`, no terminal, no
    // event loop to come back to.
    if serve {
        if bind != "127.0.0.1" && bind != "localhost" {
            // Not a refusal -- `--bind` was typed on purpose. But this now hands
            // out live shells on this machine, not just a view of a board, so
            // the person who typed it is told once, in the words that matter.
            eprintln!(
                "sauron: bound to {bind} with no authentication — anyone who can reach this \
                 port can run commands on this machine as you."
            );
        }
        return web::serve(repo_root, agent, (bind.as_str(), port), agents);
    }

    let mut app = App::new(repo_root, agent);

    // A missing log directory is not an error: it is a repo no agent has run in
    // yet, which is exactly the state you are in when you open a fresh folder
    // and are about to start one. Bailing here meant sauron could never be left
    // running while that first session came up. The directory is re-read every
    // tick, so the board fills in on its own once it appears.

    // The way back from a dismissal made in a process that has since exited. `U`
    // only remembers the last one and only for as long as the TUI is up, and the
    // dismissed rows are by definition not drawn -- so without this the only
    // recovery would be hand-editing JSON under ~/.claude.
    if restore_dismissed {
        let n = app.board.restore_all_dismissed();
        println!("restored {n} dismissed session(s).");
        return Ok(());
    }

    if baseline {
        app.board.baseline();
        println!(
            "baselined: {} session(s) marked tested. Only new agent work will appear from here.",
            app.board.store_len()
        );
        return Ok(());
    }

    if let Some(hours) = baseline_older_h {
        let n = app.board.baseline_older(hours * 60 * 60 * 1000);
        println!(
            "baselined {n} session(s) older than {hours}h as tested. Newer untested work is kept."
        );
        return Ok(());
    }

    // Headless list of the in-flight sessions, one per line as
    // `session_id<TAB>display_name`. Consumed by the `workspace` tool to reopen
    // each into a pane. In-flight means mid-turn (`Working`) or waiting on a
    // background agent it spawned (`Delegated`) -- both are live tasks the user
    // would want reopened; reusing App::rows keeps the definition from drifting.
    if list_working {
        for r in &app.board.rows {
            if matches!(r.status, Status::Working | Status::Delegated) {
                println!("{}\t{}", r.id, model::collapse_ws(&r.name));
            }
        }
        return Ok(());
    }

    // Snapshot mode: print and exit without touching the terminal. Also the only
    // way to validate the scanner against real logs without an interactive TTY.
    if once {
        // Publish as well as print. `--once` is "take a reading"; a reading that
        // updates the beacon is what lets a headless caller -- a git hook, CI, a
        // script -- refresh the pane in a project nobody has a TUI open on.
        let _ = beacon::publish(&app.board);
        print_once(&app);
        return Ok(());
    }

    let mut terminal = ratatui::init();
    // Mouse capture lets a click copy a session's continue command. The cost is
    // that click-drag no longer selects terminal text -- hold Option (iTerm2) or
    // Shift to select while this runs.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let mut last_tick = Instant::now();
    // Launch instant: the Eye's animation is a pure function of elapsed ms off
    // this, so nothing about the animation has to be stored or advanced by hand.
    let anim_start = Instant::now();
    // Remember the binary's mtime at launch. When a rebuild lands (cargo writes
    // via atomic rename, so the mtime jumps once), the loop re-execs the new
    // binary in place -- no more staring at a stale build after a rebuild.
    let launch_exe_mtime = exe_mtime();
    // Publish before the first frame, not on the first TICK: a debug pane in the
    // watched project should light up the moment sauron starts, not two seconds
    // later.
    let _ = beacon::publish(&app.board);

    let result = loop {
        let now = now_ms();
        let rows = app.board.rows.clone();
        let selected = app.selected;
        let label = app.board.repo_label.clone();
        let repo_path = model::tilde(app.board.repo_root());
        let saved = app.saved_flash();
        let copied = app.copied_flash();
        let spawned = app.spawn_flash().map(|(m, ok)| (m.to_string(), ok));
        let mut geo = ui::FrameGeometry::default();
        // Copied out of `app` rather than borrowed, for the same reason `rows`
        // is: the view outlives the draw call, and `app` is mutated right after
        // it. The list is capped at survey time, so this stays small.
        let pick_cold: Vec<orc::Target> = app
            .orc_pick
            .as_ref()
            .map(|p| p.cold.clone())
            .unwrap_or_default();
        let pick_meta = app.orc_pick.as_ref().map(|p| (p.selected, p.hot, p.dirty));

        // Re-checked per frame, not cached at launch: the directory is created
        // the moment the user starts an agent in this repo, and the empty-state
        // hint must stop claiming otherwise as soon as that happens.
        let log_dir = app.board.log_dir().to_path_buf();
        let awaiting_log_dir = (!log_dir.exists()).then(|| log_dir.to_string_lossy().into_owned());

        let view = ui::View {
            rows: &rows,
            selected,
            now,
            repo: &label,
            repo_path: &repo_path,
            saved,
            hidden_stale: app.board.hidden_stale,
            clear_count: app.board.clear_count,
            show_clear: app.board.show_clear,
            copied,
            spawned: spawned.as_ref().map(|(m, ok)| (m.as_str(), *ok)),
            anim_ms: anim_start.elapsed().as_millis() as u64,
            local_offset: app.local_offset,
            awaiting_log_dir: awaiting_log_dir.as_deref(),
            pick: pick_meta.map(|(selected, hot, dirty)| ui::PickView {
                cold: &pick_cold,
                selected,
                hot,
                dirty,
            }),
            scroll: app.scroll,
            follow: app.follow,
        };
        if let Err(e) = terminal.draw(|f| ui::draw(f, &view, &mut geo)) {
            break Err(e);
        }
        // Take back the offsets the drawing settled on, and the table geometry a
        // mouse event next iteration will be resolved against. The offsets are a
        // round trip on purpose: the event loop owns them between frames, but
        // only the drawing knows how many rows each table was given.
        app.scroll = geo.scroll;
        app.tables = std::mem::take(&mut geo.tables);

        // Poll rather than block so the Eye keeps animating on an idle keyboard.
        // Wake every FRAME to redraw; the once-per-TICK data refresh below is
        // gated separately, so faster frames never mean more log scanning.
        let timeout = FRAME.min(TICK.saturating_sub(last_tick.elapsed()));
        match event::poll(timeout) {
            Ok(true) => match event::read() {
                // The picker owns the keyboard while it is up. Nothing falls
                // through to the board keymap: j/k must not move the session
                // cursor under a modal, and `a` must not ack an invisible row.
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press && app.orc_pick.is_some() => {
                    match k.code {
                        KeyCode::Char('j') | KeyCode::Down => app.orc_pick_move(1),
                        KeyCode::Char('k') | KeyCode::Up => app.orc_pick_move(-1),
                        KeyCode::Enter => app.dispatch_orc(),
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('O') => {
                            app.close_orc_pick()
                        }
                        _ => {}
                    }
                }
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('j') | KeyCode::Down => app.move_by(1),
                    KeyCode::Char('k') | KeyCode::Up => app.move_by(-1),
                    KeyCode::Tab => app.jump_table(1),
                    KeyCode::BackTab => app.jump_table(-1),
                    KeyCode::Char('a') => app.ack_selected(),
                    KeyCode::Char('u') => app.unack_selected(),
                    // Shift-only, unlike `a`/`u`. Dismissing is the one gesture
                    // here that nothing the agent does will undo, so it does not
                    // sit under a bare letter next to the keys you press all day.
                    KeyCode::Char('D') => app.dismiss_selected(),
                    KeyCode::Char('U') => app.restore_last_dismissed(),
                    KeyCode::Char('A') => app.ack_all(),
                    KeyCode::Char('y') => app.copy_selected_continue(),
                    KeyCode::Char('n') => app.spawn_agent(),
                    KeyCode::Char('O') => app.open_orc_pick(),
                    KeyCode::Enter => app.spawn_selected(),
                    KeyCode::Char('o') => {
                        app.board.show_all = !app.board.show_all;
                        app.refresh();
                    }
                    KeyCode::Char('c') => {
                        app.board.show_clear = !app.board.show_clear;
                        app.refresh();
                    }
                    KeyCode::Char('r') => app.refresh(),
                    _ => {}
                },
                // The picker is modal for the mouse too: a stray scroll must not
                // move the session cursor underneath it, and a click must not
                // copy a continue command for a row nobody can see.
                Ok(Event::Mouse(_)) if app.orc_pick.is_some() => {}
                Ok(Event::Mouse(m)) => match m.kind {
                    // Click a row to copy its continue command (and select it).
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(idx) = app.row_at(m.row) {
                            app.copy_continue_for(idx);
                        }
                    }
                    // The wheel scrolls the table under the pointer; it does not
                    // move the cursor. See `App::scroll_at`.
                    MouseEventKind::ScrollDown => app.scroll_at(m.row, 1),
                    MouseEventKind::ScrollUp => app.scroll_at(m.row, -1),
                    _ => {}
                },
                Ok(_) => {}
                Err(e) => break Err(e),
            },
            Ok(false) => {}
            Err(e) => break Err(e),
        }

        if last_tick.elapsed() >= TICK {
            // Deliver anything a reader queued BEFORE resyncing, so a reply
            // typed into the in-app pane reaches the agent on the same tick it
            // was written rather than the one after. The agent's own log record
            // of receiving it is then picked up by the resync below, which is
            // what makes the board move without a second round trip.
            //   grep -n "fn drain"  src/reply.rs
            //
            // Nothing to drain where there is no delivery hop -- see the `reply`
            // subcommand above, which refuses rather than queues, so the outbox
            // stays empty on this platform instead of filling with messages
            // no tick will ever carry.
            #[cfg(unix)]
            for id in reply::drain() {
                app.board.ack(&id);
            }

            // resync, not refresh: this also re-reads the ack file, so work you
            // acked in a muthur board or another sauron stops reading as
            // untested here without a relaunch.
            app.resync();
            last_tick = Instant::now();

            // Republish the beacon so the watched project's own debug pane sees
            // this tick. Errors are swallowed: a full disk or a read-only home
            // must not take down the watcher over a courtesy file, and a reader
            // that stops seeing fresh writes correctly concludes sauron is gone.
            let _ = beacon::publish(&app.board);

            // Rebuilt on disk? Re-exec so the running window becomes the new
            // build. exec() only returns on failure, in which case we keep
            // running the current binary and surface the error.
            if let (Some(launch), Some(current)) = (launch_exe_mtime, exe_mtime()) {
                if current > launch {
                    break Err(reload());
                }
            }
        }
    };

    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    // Clean exit: drop the beacon now so the watched project's pane closes with
    // the window instead of aging out over STALE_MS. A crash skips this, which
    // is exactly what the staleness gate is for.
    beacon::retire(app.board.repo_root());
    result
}

/// Plain-text snapshot for `--once`.
fn print_once(app: &App) {
    let now = now_ms();
    if app.board.rows.is_empty() {
        // Snapshot mode exits immediately, so unlike the TUI it gets no later
        // chance to fill in. Say which directory was empty -- a wrong repo and a
        // never-used one print the same single line otherwise.
        if !app.board.log_dir().exists() {
            println!(
                "no agent sessions yet for {}\n  watching: {}",
                app.board.repo_root().display(),
                app.board.log_dir().display()
            );
        } else {
            println!("no sessions with repo edits");
        }
        return;
    }
    let errored = app.board.rows.iter().filter(|r| r.status == Status::Errored).count();
    let blocked = app.board.rows.iter().filter(|r| r.status == Status::Blocked).count();
    let ack = app.board.rows.iter().filter(|r| r.status == Status::AwaitingAck).count();
    let needs = app.board.rows.iter().filter(|r| r.status == Status::NeedsTest).count();
    let mut banner = Vec::new();
    if errored > 0 {
        banner.push(format!("{} ERRORED", errored));
    }
    if blocked > 0 {
        banner.push(format!("{} WAITING ON YOU", blocked));
    }
    if ack > 0 {
        banner.push(format!("{} to acknowledge", ack));
    }
    if needs > 0 {
        banner.push(format!("{} to test", needs));
    }
    println!("{}\n", if banner.is_empty() { "all caught up".into() } else { banner.join("  ·  ") });

    for r in &app.board.rows {
        // The board's own glyph table, not a copy of it: a snapshot that marked
        // a state differently from the window would be two answers to the same
        // question.
        let glyph = ui::glyph_of(r.status);
        println!(
            "{} {:<52} {:>4}  {}",
            glyph,
            model::truncate(&r.name, 52),
            model::ago(r.last_activity, now),
            r.status.label()
        );
        // Errored / blocked sessions: what's wrong matters more than a file count.
        if r.status == Status::Errored {
            if let Some(e) = r.error {
                println!("    {}", e.detail());
            }
        } else if r.status == Status::Blocked {
            if let Some(reason) = r.blocked_reason {
                println!("    {}", reason.detail());
            }
        } else if r.pending.is_empty() {
            println!("    {} file(s), all acked", r.total_edits);
        } else {
            for p in r.pending.iter().take(6) {
                println!("    {}", p);
            }
            if r.pending.len() > 6 {
                println!("    … and {} more", r.pending.len() - 6);
            }
        }
        println!();
    }
}

/// `--port 7373`, `--port=7373`, or nothing -- and the flag *and its value* come
/// out of `args` on the way.
///
/// Removing them is the point. The repo path is found as "the first argument
/// that does not start with `--`", and a port left in the vector is exactly
/// that: sauron would go looking for a repo called `7373`.
fn take_flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    if let Some(i) = args.iter().position(|a| a == name) {
        args.remove(i);
        return (i < args.len() && !args[i].starts_with("--")).then(|| args.remove(i));
    }
    let prefix = format!("{name}=");
    let i = args.iter().position(|a| a.starts_with(&prefix))?;
    Some(args.remove(i)[prefix.len()..].to_string())
}

/// Modification time of this process's own executable, following a symlink to
/// the real binary. None if it cannot be read (then auto-reload is disabled).
fn exe_mtime() -> Option<SystemTime> {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
}

/// Replace this process with a fresh copy of the (rebuilt) binary, same args.
/// Terminal state is restored first so the new process starts on a clean tty and
/// a failed handoff does not strand the terminal in raw mode. Only returns if it
/// failed; the returned error is surfaced by the caller.
fn reload() -> std::io::Error {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sauron"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    plat::reload(exe, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Geometry used by these tests, as the board would have drawn it:
    //
    //   y=10  YOUR MOVE (5) ────   header, in no table
    //   y=11..13                   three of the table's five rows
    //   y=14                       the blank row between two tables
    //   y=15  WORKING (3) ─────    header
    //   y=16..17                   two of the table's three rows
    fn fixture() -> Vec<ui::TableGeometry> {
        vec![
            ui::TableGeometry {
                group: ui::Group::YourMove,
                body_top: 11,
                body_height: 3,
                rows: vec![0, 1, 2, 3, 4],
                offset: 0,
            },
            ui::TableGeometry {
                group: ui::Group::Working,
                body_top: 16,
                body_height: 2,
                rows: vec![5, 6, 7],
                offset: 0,
            },
        ]
    }

    #[test]
    fn click_lands_on_the_row_under_the_cursor() {
        let g = fixture();
        assert_eq!(hit_test(11, &g), Some(0));
        assert_eq!(hit_test(13, &g), Some(2));
        // The second table's rows are the same click arithmetic against its own
        // top -- which is the whole reason this is per table now.
        assert_eq!(hit_test(16, &g), Some(5));
        assert_eq!(hit_test(17, &g), Some(6));
    }

    #[test]
    fn clicks_off_the_rows_resolve_to_nothing() {
        let g = fixture();
        assert_eq!(hit_test(10, &g), None); // a table header
        assert_eq!(hit_test(14, &g), None); // the gap between two tables
        assert_eq!(hit_test(15, &g), None); // the second header
        assert_eq!(hit_test(5, &g), None); // above the board
        assert_eq!(hit_test(40, &g), None); // below it
    }

    #[test]
    fn each_table_is_hit_tested_through_its_own_offset() {
        let mut g = fixture();
        // Scroll the first table two rows on: its top line is now row 2, and the
        // second table -- which nobody scrolled -- must not move with it.
        g[0].offset = 2;
        assert_eq!(hit_test(11, &g), Some(2));
        assert_eq!(hit_test(12, &g), Some(3));
        assert_eq!(hit_test(16, &g), Some(5));

        // Scrolled past its last row, a body line resolves to nothing rather
        // than to whatever index the arithmetic ran off the end into.
        g[1].offset = 2;
        assert_eq!(hit_test(16, &g), Some(7));
        assert_eq!(hit_test(17, &g), None);
    }

    #[test]
    fn tab_walks_the_tables_and_wraps_round() {
        use ui::Group::*;
        // Three rows in YOUR MOVE, one awaiting testing, two working -- the
        // grouping of a ranked row set, which is what the board draws.
        let g = [YourMove, YourMove, YourMove, AwaitingTesting, Working, Working];
        assert_eq!(next_table_row(&g, 1, 1), Some(3));
        assert_eq!(next_table_row(&g, 3, 1), Some(4));
        // Round from the last table to the first, so Tab alone reaches every
        // table rather than dead-ending at the bottom one.
        assert_eq!(next_table_row(&g, 5, 1), Some(0));
        assert_eq!(next_table_row(&g, 0, -1), Some(4));
        // A cursor pointing at no row at all still gets somewhere.
        assert_eq!(next_table_row(&g, 99, 1), Some(3));
        assert_eq!(next_table_row(&[], 0, 1), None);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
