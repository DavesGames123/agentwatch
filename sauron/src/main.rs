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
//!
//! The watching itself lives in the library (`board::Board`), shared with the
//! `muthur` multi-project front end; this file is the terminal around it.
//!
//! grep targets:
//!   struct App          -- a Board plus this window's cursor and banners
//!   fn App::refresh     -- rebuild rows, keeping the cursor on its session
//!   fn App::resync      -- refresh that also picks up another process's acks
//!   fn main             -- terminal lifecycle and event loop

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::widgets::ListState;

use sauron::agent::Agent;
use sauron::board::Board;
use sauron::model::{self, now_ms, Status};
use sauron::{clip, git_root, gui, handoff, orc, ui, workspace};

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

/// The TUI's state: a `Board` plus everything about *this window* -- where the
/// cursor is, which banners are still up, where the last frame drew its rows.
///
/// The split is deliberate. Everything that answers "what is the state of this
/// repo" lives in `board`, so `muthur` can ask the same questions without a
/// terminal; everything here is unshareable by construction, because a second
/// front end has its own cursor and its own geometry.
struct App {
    board: Board,
    list_state: ListState,
    selected: usize,
    saved_until: Option<Instant>,
    /// Transient "copied" banner deadline.
    copied_until: Option<Instant>,
    /// Result of the last pane spawn and its banner deadline. Success and
    /// failure share the slot: both are the answer to a key just pressed, and
    /// only the last one matters.
    spawn_msg: Option<(String, bool)>,
    spawn_until: Option<Instant>,
    /// Per-frame list geometry, refreshed each draw, so a mouse click can be
    /// mapped back to the row under it. Heights and row-mapping run parallel to
    /// the items the list widget draws, in the same order.
    frame_item_heights: Vec<u16>,
    frame_item_rows: Vec<Option<usize>>,
    list_top: u16,
    list_height: u16,
    /// The machine's UTC offset in seconds, read once at launch so task start
    /// times render on the local wall clock without re-shelling `date` per frame.
    local_offset: i64,
    /// The cold-target picker, while it is open over the board. `Some` swallows
    /// the normal keymap, so j/k/Enter mean "inside the picker" and nothing can
    /// be acked by accident while choosing a file.
    orc_pick: Option<OrcPick>,
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
            list_state: ListState::default(),
            selected: 0,
            saved_until: None,
            copied_until: None,
            spawn_msg: None,
            spawn_until: None,
            frame_item_heights: Vec::new(),
            frame_item_rows: Vec::new(),
            list_top: 0,
            list_height: 0,
            local_offset: model::local_offset_secs(),
            orc_pick: None,
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
                self.sync_list_state();
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
        self.sync_list_state();
    }

    fn sync_list_state(&mut self) {
        if self.board.rows.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(self.selected));
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.board.rows.is_empty() {
            return;
        }
        let last = self.board.rows.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.sync_list_state();
    }

    /// `a` is context-sensitive: on a waiting session (Blocked or AwaitingAck) it
    /// dismisses that waiting state; on an untested session it acks the write-set.
    /// Both mean "I have handled this", and both re-surface if the agent does
    /// something new.
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
            self.sync_list_state();
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
        hit_test(
            click_y,
            self.list_top,
            self.list_height,
            self.list_state.offset(),
            &self.frame_item_heights,
            &self.frame_item_rows,
        )
    }
}

/// Resolve a click's y-coordinate to a row index, walking the visible items from
/// the scroll offset and summing their heights. Returns None for a click on a
/// section header, the clear-collapse line, or empty space below the list.
/// Pure arithmetic, split out from `App` so it can be tested without a terminal.
fn hit_test(
    click_y: u16,
    list_top: u16,
    list_height: u16,
    offset: usize,
    heights: &[u16],
    rows: &[Option<usize>],
) -> Option<usize> {
    if click_y < list_top || click_y >= list_top + list_height {
        return None;
    }
    let mut y = list_top;
    for i in offset..heights.len() {
        let h = heights[i];
        if click_y >= y && click_y < y + h {
            return rows.get(i).copied().flatten();
        }
        y += h;
        if y >= list_top + list_height {
            break;
        }
    }
    None
}

/// Put text on the system clipboard. Tries pbcopy first (macOS), then falls
/// back to an OSC 52 escape so it still works over SSH or on a bare terminal.
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }

    // OSC 52: ESC ] 52 ; c ; <base64> BEL -- ask the terminal to set the
    // clipboard. Written straight to the tty; harmless if unsupported.
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    std::io::stdout().write_all(seq.as_bytes()).is_ok()
        && std::io::stdout().flush().is_ok()
}

/// Minimal standard-alphabet base64, to avoid a dependency for one escape code.
fn base64(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

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
        return gui::run(&args[1..]);
    }

    let once = args.iter().any(|a| a == "--once");
    let baseline = args.iter().any(|a| a == "--baseline");
    let list_working = args.iter().any(|a| a == "--list-working");
    let repo_root = match args.iter().find(|a| !a.starts_with("--")) {
        Some(p) => PathBuf::from(p),
        None => git_root().unwrap_or(std::env::current_dir()?),
    };
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    let agent = Agent::select(explicit_agent, &repo_root);

    let mut app = App::new(repo_root, agent);

    // A missing log directory is not an error: it is a repo no agent has run in
    // yet, which is exactly the state you are in when you open a fresh folder
    // and are about to start one. Bailing here meant sauron could never be left
    // running while that first session came up. The directory is re-read every
    // tick, so the board fills in on its own once it appears.

    if baseline {
        app.board.baseline();
        println!(
            "baselined: {} session(s) marked tested. Only new agent work will appear from here.",
            app.board.store_len()
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

    let result = loop {
        let now = now_ms();
        let rows = app.board.rows.clone();
        let selected = app.selected;
        let label = app.board.repo_label.clone();
        let repo_path = model::tilde(app.board.repo_root());
        let saved = app.saved_flash();
        let copied = app.copied_flash();
        let spawned = app.spawn_flash().map(|(m, ok)| (m.to_string(), ok));
        let mut list_state = app.list_state.clone();
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
        };
        if let Err(e) = terminal.draw(|f| ui::draw(f, &view, &mut list_state, &mut geo)) {
            break Err(e);
        }
        app.list_state = list_state;
        // Stash the frame's list geometry so a mouse click next iteration can be
        // mapped to the row under the cursor.
        app.frame_item_heights = geo.item_heights;
        app.frame_item_rows = geo.item_rows;
        app.list_top = geo.list_top;
        app.list_height = geo.list_height;

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
                    KeyCode::Char('a') => app.ack_selected(),
                    KeyCode::Char('u') => app.unack_selected(),
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
                    MouseEventKind::ScrollDown => app.move_by(1),
                    MouseEventKind::ScrollUp => app.move_by(-1),
                    _ => {}
                },
                Ok(_) => {}
                Err(e) => break Err(e),
            },
            Ok(false) => {}
            Err(e) => break Err(e),
        }

        if last_tick.elapsed() >= TICK {
            // resync, not refresh: this also re-reads the ack file, so work you
            // acked in a muthur board or another sauron stops reading as
            // untested here without a relaunch.
            app.resync();
            last_tick = Instant::now();

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
        let glyph = match r.status {
            Status::Errored => "✖",
            Status::Blocked => "▲",
            Status::AwaitingAck => "❯",
            Status::NeedsTest => "█",
            Status::Working => "◐",
            Status::Delegated => "◇",
            Status::Clear => "·",
        };
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
/// a failed exec does not strand the terminal in raw mode. On Unix, exec() only
/// returns if it failed; the returned error is surfaced by the caller.
fn reload() -> std::io::Error {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sauron"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::Command::new(exe).args(args).exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Layout used by these tests: list starts at y=10, is 20 rows tall.
    // Items (draw order): section header (h=2), card A (h=3), card B (h=3),
    // section header (h=2), card C (h=3). Rows map: A->0, B->1, C->2.
    fn fixture() -> (Vec<u16>, Vec<Option<usize>>) {
        (
            vec![2, 3, 3, 2, 3],
            vec![None, Some(0), Some(1), None, Some(2)],
        )
    }

    #[test]
    fn click_lands_on_the_card_under_the_cursor() {
        let (h, r) = fixture();
        // y layout from top=10: header 10-11, A 12-14, B 15-17, header 18-19, C 20-22.
        assert_eq!(hit_test(13, 10, 20, 0, &h, &r), Some(0)); // inside card A
        assert_eq!(hit_test(16, 10, 20, 0, &h, &r), Some(1)); // inside card B
        assert_eq!(hit_test(21, 10, 20, 0, &h, &r), Some(2)); // inside card C
    }

    #[test]
    fn clicks_on_headers_and_outside_resolve_to_nothing() {
        let (h, r) = fixture();
        assert_eq!(hit_test(10, 10, 20, 0, &h, &r), None); // section header
        assert_eq!(hit_test(18, 10, 20, 0, &h, &r), None); // second header
        assert_eq!(hit_test(5, 10, 20, 0, &h, &r), None); // above the list
        assert_eq!(hit_test(40, 10, 20, 0, &h, &r), None); // below the list
    }

    #[test]
    fn scroll_offset_shifts_the_hit_test() {
        let (h, r) = fixture();
        // With offset=1, the first drawn item is card A at the list top (y=10).
        // header 10-11? no -- offset skips item 0, so A(h=3) is 10-12, B 13-15...
        assert_eq!(hit_test(11, 10, 20, 1, &h, &r), Some(0));
        assert_eq!(hit_test(14, 10, 20, 1, &h, &r), Some(1));
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
