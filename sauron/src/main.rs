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
//! grep targets:
//!   struct Row          -- one session flattened for rendering
//!   struct App          -- scanner + ack store + selection
//!   fn App::refresh     -- rescan logs and rebuild rows
//!   fn main             -- terminal lifecycle and event loop

mod agent;
mod clip;
mod codex;
mod handoff;
mod model;
mod scan;
mod scene;
mod store;
mod ui;
mod workspace;

use agent::Agent;

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::widgets::ListState;

use model::{now_ms, BlockedReason, ErrorKind, Session, Status, DORMANT_AFTER_MS, STALE_HORIZON_MS};
use scan::Scanner;
use store::AckStore;

/// How often the logs are re-tailed. Only appended bytes are parsed, so this is
/// cheap even with 10MB session files.
const TICK: Duration = Duration::from_millis(2000);
// The data only changes on TICK, but the Eye of Sauron animates far faster, so
// the loop wakes on this shorter frame clock to redraw. ratatui diffs the
// buffer and writes only changed cells, so an idle frame -- the Eye holding
// still -- costs nothing on the wire.
const FRAME: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub id_short: String,
    pub name: String,
    pub branch: Option<String>,
    pub last_activity: i64,
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

struct App {
    scanner: Scanner,
    store: AckStore,
    /// Rows past the staleness horizon, kept out of `rows` but counted.
    rows: Vec<Row>,
    hidden_stale: usize,
    /// Clear sessions are counted but kept out of `rows` unless `show_clear`.
    clear_count: usize,
    show_clear: bool,
    show_all: bool,
    list_state: ListState,
    selected: usize,
    repo_label: String,
    saved_until: Option<Instant>,
    /// Transient "copied" banner deadline.
    copied_until: Option<Instant>,
    /// Per-frame list geometry, refreshed each draw, so a mouse click can be
    /// mapped back to the row under it. Heights and row-mapping run parallel to
    /// the items the list widget draws, in the same order.
    frame_item_heights: Vec<u16>,
    frame_item_rows: Vec<Option<usize>>,
    list_top: u16,
    list_height: u16,
}

impl App {
    fn new(repo_root: PathBuf, agent: Agent) -> Self {
        let repo_label = repo_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());

        let mut app = Self {
            scanner: Scanner::new(repo_root, agent),
            store: AckStore::load(),
            rows: Vec::new(),
            hidden_stale: 0,
            clear_count: 0,
            show_clear: false,
            show_all: false,
            list_state: ListState::default(),
            selected: 0,
            repo_label,
            saved_until: None,
            copied_until: None,
            frame_item_heights: Vec::new(),
            frame_item_rows: Vec::new(),
            list_top: 0,
            list_height: 0,
        };
        app.refresh();
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
            if let Some(i) = self.rows.iter().position(|r| r.status == want) {
                self.selected = i;
                self.sync_list_state();
                return;
            }
        }
    }

    fn refresh(&mut self) {
        let now = now_ms();
        let sessions = self.scanner.refresh();

        // Preserve the selected session across rebuilds -- rows reorder as
        // statuses change, and moving the cursor out from under the user is the
        // fastest way to make them ack the wrong thing.
        let anchor = self.rows.get(self.selected).map(|r| r.id.clone());

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
        self.selected = anchor
            .and_then(|id| self.rows.iter().position(|r| r.id == id))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        self.sync_list_state();
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

    fn sync_list_state(&mut self) {
        if self.rows.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(self.selected));
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.sync_list_state();
    }

    /// `a` is context-sensitive: on a waiting session (Blocked or AwaitingAck) it
    /// dismisses that waiting state; on an untested session it acks the write-set.
    /// Both mean "I have handled this", and both re-surface if the agent does
    /// something new.
    fn ack_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
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

    fn unack_selected(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let id = row.id.clone();
        // Undo whichever suppression applies to this row.
        if matches!(row.status, Status::Blocked | Status::AwaitingAck) {
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
    fn baseline(&mut self) {
        let saved = self.show_all;
        self.show_all = true;
        self.refresh();
        self.ack_all();
        self.show_all = saved;
        self.refresh();
    }

    fn ack_all(&mut self) {
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

    fn persist(&mut self) {
        if self.store.save().is_ok() {
            self.saved_until = Some(Instant::now() + Duration::from_millis(1200));
        }
    }

    fn store_len(&self) -> usize {
        self.store.session_count()
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
        let Some(row) = self.rows.get(idx) else {
            return;
        };
        if copy_to_clipboard(&row.continue_cmd) {
            self.selected = idx;
            self.sync_list_state();
            self.copied_until = Some(Instant::now() + Duration::from_millis(1600));
        }
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

/// The in-flight sessions for a repo -- mid-turn (`Working`) or waiting on a
/// background agent it spawned (`Delegated`) -- as `(session_id, display_name)`.
/// The same set `--list-working` prints and the TUI shows, so `sauron workspace`
/// reopens exactly the sessions the tool counts as live.
fn in_flight_tasks(repo_root: PathBuf, agent: Agent) -> Vec<(String, String)> {
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    App::new(repo_root, agent)
        .rows
        .iter()
        .filter(|r| matches!(r.status, Status::Working | Status::Delegated))
        .map(|r| (r.id.clone(), model::collapse_ws(&r.name)))
        .collect()
}

/// Repo-relative paths an *active* session is touching -- working, delegated,
/// blocked, or holding untested edits. This is the "hot" set an orc must steer
/// clear of, so its single-shot refactor never collides with a hobbit's work.
fn hot_files(repo_root: PathBuf, agent: Agent) -> std::collections::BTreeSet<String> {
    let repo_root = repo_root.canonicalize().unwrap_or(repo_root);
    let mut hot = std::collections::BTreeSet::new();
    for r in &App::new(repo_root, agent).rows {
        if matches!(
            r.status,
            Status::Working | Status::Delegated | Status::Blocked | Status::NeedsTest
        ) {
            hot.extend(r.edits.keys().cloned());
        }
    }
    hot
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

    if !app.scanner.log_dir().exists() {
        eprintln!(
            "no {} session logs for {}\n  looked in: {}",
            agent.label(),
            app.scanner.repo_root().display(),
            app.scanner.log_dir().display()
        );
        return Ok(());
    }

    if baseline {
        app.baseline();
        println!(
            "baselined: {} session(s) marked tested. Only new agent work will appear from here.",
            app.store_len()
        );
        return Ok(());
    }

    // Headless list of the in-flight sessions, one per line as
    // `session_id<TAB>display_name`. Consumed by the `workspace` tool to reopen
    // each into a pane. In-flight means mid-turn (`Working`) or waiting on a
    // background agent it spawned (`Delegated`) -- both are live tasks the user
    // would want reopened; reusing App::rows keeps the definition from drifting.
    if list_working {
        for r in &app.rows {
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
        let rows = app.rows.clone();
        let selected = app.selected;
        let label = app.repo_label.clone();
        let saved = app.saved_flash();
        let copied = app.copied_flash();
        let mut list_state = app.list_state.clone();
        let mut geo = ui::FrameGeometry::default();

        let view = ui::View {
            rows: &rows,
            selected,
            now,
            repo: &label,
            saved,
            hidden_stale: app.hidden_stale,
            clear_count: app.clear_count,
            show_clear: app.show_clear,
            copied,
            anim_ms: anim_start.elapsed().as_millis() as u64,
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
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('j') | KeyCode::Down => app.move_by(1),
                    KeyCode::Char('k') | KeyCode::Up => app.move_by(-1),
                    KeyCode::Char('a') => app.ack_selected(),
                    KeyCode::Char('u') => app.unack_selected(),
                    KeyCode::Char('A') => app.ack_all(),
                    KeyCode::Char('y') => app.copy_selected_continue(),
                    KeyCode::Char('o') => {
                        app.show_all = !app.show_all;
                        app.refresh();
                    }
                    KeyCode::Char('c') => {
                        app.show_clear = !app.show_clear;
                        app.refresh();
                    }
                    KeyCode::Char('r') => app.refresh(),
                    _ => {}
                },
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
            app.refresh();
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
    if app.rows.is_empty() {
        println!("no sessions with repo edits");
        return;
    }
    let errored = app.rows.iter().filter(|r| r.status == Status::Errored).count();
    let blocked = app.rows.iter().filter(|r| r.status == Status::Blocked).count();
    let ack = app.rows.iter().filter(|r| r.status == Status::AwaitingAck).count();
    let needs = app.rows.iter().filter(|r| r.status == Status::NeedsTest).count();
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

    for r in &app.rows {
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

/// Walk up from the cwd looking for a .git entry, so the tool works from any
/// subdirectory of the repo.
fn git_root() -> Option<PathBuf> {
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
