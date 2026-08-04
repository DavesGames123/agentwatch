//! sauron in a browser: the board, and the agents themselves.
//!
//! WHAT CHANGED, AND WHY THE FIRST ATTEMPT WAS WRONG
//! -------------------------------------------------
//! The first version of this module streamed ratatui's cell grid to a page and
//! drew it there. It worked, and it was the wrong thing: it made a browser into
//! a worse terminal instead of making sauron into a web application, and it
//! could only ever *watch* -- the agents still lived in iTerm2, out of reach.
//!
//! This is the other design. The board crosses as data (`json`) and the page
//! lays it out as HTML it owns. The agents cross as bytes (`pty`, `ws`), because
//! sauron opens the pseudo-terminals now and the page is the terminal on the end
//! of them. The result is that `sauron workspace` and iTerm2 become one way to
//! run agents rather than the only way, and macOS stops being a requirement for
//! the part of sauron that does the work.
//!
//! THE SHAPE
//! ---------
//! ```text
//!   browser ──ws──▶ Workspace ──pty──▶ claude --resume 1d935d15
//!      ▲                       ├─pty──▶ claude --session-id <minted>
//!      │                       └─pty──▶ sauron orc src/scan.rs
//!      └──── board json ◀── Board (the same scanner the TUI uses)
//! ```
//!
//! Two clocks, deliberately. The board is rebuilt on `TICK` because that is how
//! often logs are worth re-tailing; pty output goes out the instant it is read,
//! because a terminal that batches keystrokes to a 2Hz clock is unusable. They
//! share one websocket and never wait on each other.
//!
//! WHAT IS SHARED AND WHAT IS NOT
//! ------------------------------
//! `Board`, `model`, `scan`, `store`, `servant` -- all the machinery that
//! decides what a session's state is and what colour says so -- are the same
//! code the TUI runs. Nothing here re-derives any of it. What is *not* shared is
//! the layout, and that is the point: `ui.rs` lays out for a terminal, and this
//! lays out for a browser, and neither is a translation of the other.
//!
//! grep targets:
//!   fn serve         -- bind, start the tick loop, own the process
//!   struct State     -- the board, the tabs, and everyone watching
//!   struct Clients   -- fan-out to every open browser
//!   fn on_text       -- one JSON control message from a page
//!   fn on_binary     -- keystrokes for a pane
//!   fn connection    -- one browser, start to finish
//!   const TICK       -- how often the board is rebuilt

pub mod http;
pub mod json;
pub mod pane;
pub mod pty;
pub mod sha1;
pub mod ws;

use std::io;
use std::net::{TcpListener, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::agent::Agent;
use crate::board::Board;
use crate::model;

use pane::Workspace;
use ws::{Msg, Ws, WsOut};

/// How often the logs are re-tailed and the board rebuilt. The same 2s the TUI
/// uses -- this is a property of how fast agent state changes, not of the front
/// end looking at it.
const TICK: Duration = Duration::from_millis(2000);

/// The page, with its vendored terminal emulator alongside.
pub const PAGE: &str = include_str!("../../assets/sauron_web.html");
pub const XTERM_JS: &str = include_str!("../../assets/vendor/xterm.js");
pub const XTERM_CSS: &str = include_str!("../../assets/vendor/xterm.css");
pub const FIT_JS: &str = include_str!("../../assets/vendor/addon-fit.js");

/// Every browser currently watching.
///
/// One list, not one per pane: a browser attaches once and receives everything
/// on that socket -- board updates, tab changes, and the output of every pty at
/// the same time. Tabs are a *view* concept, so a background tab's agent still
/// streams and its scrollback stays live without the page asking.
#[derive(Default)]
pub struct Clients {
    subs: Mutex<Vec<WsOut>>,
}

impl Clients {
    pub fn add(&self, out: WsOut) {
        if let Ok(mut s) = self.subs.lock() {
            s.push(out);
        }
    }

    /// A JSON control message to everyone. Sockets that refuse a write are
    /// dropped here -- a failed write is the only notification a closed tab
    /// gives, and polling for one would be a timer that learns nothing new.
    pub fn send_text(&self, s: &str) {
        if let Ok(mut subs) = self.subs.lock() {
            subs.retain(|c| c.text(s).is_ok());
        }
    }

    /// Pty output for one pane, to everyone.
    pub fn send_pty(&self, pane: u8, data: &[u8]) {
        if let Ok(mut subs) = self.subs.lock() {
            subs.retain(|c| c.binary(pane, data).is_ok());
        }
    }

    pub fn count(&self) -> usize {
        self.subs.lock().map(|s| s.len()).unwrap_or(0)
    }
}

pub struct State {
    pub board: Mutex<Board>,
    pub workspace: Mutex<Workspace>,
    pub clients: Arc<Clients>,
    pub repo: PathBuf,
    pub local_offset: i64,
    pub quit: Arc<AtomicBool>,
}

impl State {
    /// Push the board and the tab strip at every page. Called on the tick, and
    /// again immediately after anything that changes either -- an ack should
    /// land on screen when you click it, not up to two seconds later.
    pub fn broadcast(&self) {
        if let Ok(b) = self.board.lock() {
            self.clients.send_text(&json::board(&b, self.local_offset));
        }
        self.broadcast_tabs();
    }

    pub fn broadcast_tabs(&self) {
        if let Ok(w) = self.workspace.lock() {
            self.clients.send_text(&w.json());
        }
    }
}

/// Run the board and the agents until someone says stop.
///
/// Owns the process: unlike the TUI there is no event loop to hand control to,
/// so this *is* the program once it is called.
pub fn serve<A: ToSocketAddrs>(
    repo: PathBuf,
    agent: Agent,
    addr: A,
    open_agents: usize,
) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;

    let clients = Arc::new(Clients::default());
    let board = Board::new(repo.clone(), agent);
    let label = board.repo_label.clone();
    let state = Arc::new(State {
        board: Mutex::new(board),
        workspace: Mutex::new(Workspace::new(repo.clone(), agent, clients.clone())),
        clients,
        repo,
        local_offset: model::local_offset_secs(),
        quit: Arc::new(AtomicBool::new(false)),
    });

    // Reopen whatever is already mid-turn, the way `sauron workspace` does at
    // launch. A board with three working agents and an empty tab strip would be
    // asking the user to re-open by hand what sauron already knows about.
    if open_agents > 0 {
        let live: Vec<String> = state
            .board
            .lock()
            .map(|b| {
                b.rows
                    .iter()
                    .filter(|r| {
                        matches!(
                            r.status,
                            crate::model::Status::Working | crate::model::Status::Delegated
                        )
                    })
                    .take(open_agents)
                    .map(|r| r.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if let Ok(mut w) = state.workspace.lock() {
            for id in live {
                let _ = w.open_agent(Some(id), 120, 32);
            }
        }
    }

    let url = format!(
        "http://{}:{}",
        if local.ip().is_unspecified() {
            "localhost".to_string()
        } else {
            local.ip().to_string()
        },
        local.port()
    );
    println!("sauron: watching {label} — {url}");
    println!("  the agents run here now. closing the tab does not stop them.");

    http::spawn(listener, state.clone(), label);

    let launch_mtime = exe_mtime();
    let _ = crate::beacon::publish(&state.board.lock().unwrap());

    while !state.quit.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);

        // Deliver anything a reader queued before resyncing, so a reply typed
        // into an in-app pane reaches the agent on the tick it was written.
        #[cfg(unix)]
        {
            let acked: Vec<String> = crate::reply::drain();
            if let Ok(mut b) = state.board.lock() {
                for id in acked {
                    b.ack(&id);
                }
            }
        }

        if let Ok(mut b) = state.board.lock() {
            b.resync();
            let _ = crate::beacon::publish(&b);
        }
        state.broadcast();

        // Rebuilt on disk? Re-exec, exactly as the TUI does. The pages reconnect
        // by themselves -- but the agents do not survive it, because their ptys
        // belong to this process, so this is announced rather than silent.
        if let (Some(launch), Some(current)) = (launch_mtime, exe_mtime()) {
            if current > launch {
                let running = state.workspace.lock().map(|w| !w.is_empty()).unwrap_or(false);
                if running {
                    // A rebuild while agents are running would kill them. The
                    // TUI can re-exec freely because it owns nothing; this
                    // cannot, so it says so and keeps running the old binary.
                    state.clients.send_text(
                        "{\"t\":\"notice\",\"level\":\"warn\",\"text\":\
                         \"sauron was rebuilt — restart it when your agents are idle\"}",
                    );
                    break;
                }
                return Err(reload());
            }
        }
    }

    farewell(&state);
    Ok(())
}

/// Stop cleanly: tell the pages, end the agents, drop the beacon.
fn farewell(state: &State) {
    state.clients.send_text("{\"t\":\"bye\"}");
    if let Ok(mut w) = state.workspace.lock() {
        w.close_all();
    }
    crate::beacon::retire(&state.repo);
    std::thread::sleep(Duration::from_millis(120));
}

/// One browser, from upgrade to hangup.
pub fn connection(stream: std::net::TcpStream, key: &str, state: Arc<State>) -> io::Result<()> {
    use std::io::Write;
    let mut sock = stream.try_clone()?;
    sock.write_all(ws::accept(key).as_bytes())?;
    sock.flush()?;
    let _ = stream.set_nodelay(true);

    let mut conn = Ws::new(stream)?;
    let out = conn.out();
    state.clients.add(out.clone());

    // Everything this page needs to draw itself, before it asks.
    state.broadcast();

    loop {
        match conn.read() {
            Ok(Msg::Text(s)) => on_text(&state, &s, &out),
            Ok(Msg::Binary(b)) => on_binary(&state, &b),
            Ok(Msg::Close) | Err(_) => break,
        }
    }
    out.close();
    Ok(())
}

/// Keystrokes: the first byte names the pane, the rest is for its tty.
fn on_binary(state: &State, data: &[u8]) {
    let Some((&pane, keys)) = data.split_first() else {
        return;
    };
    if let Ok(w) = state.workspace.lock() {
        if let Some(p) = w.get(pane) {
            p.pty.write(keys);
        }
    }
}

/// One JSON control message.
///
/// Every arm that changes something broadcasts afterwards rather than waiting
/// for the tick. Clicking "acknowledge" and watching the card sit there for two
/// seconds reads as a dropped click, and the second click acks the wrong row
/// once the first one lands.
fn on_text(state: &State, body: &str, out: &WsOut) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u16;

    match v.get("t").and_then(|x| x.as_str()).unwrap_or("") {
        // --- the tabs ---
        "open" => {
            let (cols, rows) = (n("cols").max(20), n("rows").max(8));
            let opened = state.workspace.lock().ok().and_then(|mut w| {
                match v.get("kind").and_then(|x| x.as_str()).unwrap_or("agent") {
                    "shell" => w.open_shell(cols, rows).ok(),
                    "orc" => s("target").and_then(|t| w.open_orc(&t, cols, rows).ok()),
                    _ => w.open_agent(s("session"), cols, rows).ok(),
                }
            });
            if let Some(id) = opened {
                state.broadcast_tabs();
                let _ = out.text(&format!("{{\"t\":\"opened\",\"pane\":{id}}}"));
            } else {
                let _ = out.text(
                    "{\"t\":\"notice\",\"level\":\"error\",\"text\":\"could not open that tab\"}",
                );
            }
        }
        "close" => {
            if let Ok(mut w) = state.workspace.lock() {
                w.close(n("pane") as u8);
            }
            state.broadcast_tabs();
        }
        "resize" => {
            if let Ok(w) = state.workspace.lock() {
                if let Some(p) = w.get(n("pane") as u8) {
                    p.pty.resize(n("cols"), n("rows"));
                }
            }
        }
        // A tab that just became visible has missed whatever ran while it was
        // hidden -- or, after a reload, the entire session. Replay the ring.
        "attach" => {
            let pane = n("pane") as u8;
            let history = state
                .workspace
                .lock()
                .ok()
                .and_then(|w| w.get(pane).map(|p| p.pty.history()));
            if let Some(h) = history {
                let _ = out.text(&format!("{{\"t\":\"replay\",\"pane\":{pane}}}"));
                if !h.is_empty() {
                    let _ = out.binary(pane, &h);
                }
            }
        }
        // --- the board ---
        "ack" | "unack" | "dismiss" | "restore" | "ackAll" => {
            if let (Ok(mut b), Some(kind)) = (
                state.board.lock(),
                v.get("t").and_then(|x| x.as_str()),
            ) {
                let id = s("id").unwrap_or_default();
                match kind {
                    "ack" => b.ack(&id),
                    "unack" => b.unack(&id),
                    "dismiss" => {
                        b.dismiss(&id);
                    }
                    "restore" => b.restore(&id),
                    _ => b.ack_all(),
                }
            }
            state.broadcast();
        }
        "toggle" => {
            if let Ok(mut b) = state.board.lock() {
                match s("what").as_deref() {
                    Some("clear") => b.show_clear = !b.show_clear,
                    Some("stale") => b.show_all = !b.show_all,
                    _ => {}
                }
                b.refresh();
            }
            state.broadcast();
        }
        // What could an orc safely be loosed on right now. Surveyed on demand,
        // never on the tick: it costs a `git ls-files` and a read of every
        // tracked source file.
        "survey" => {
            let hot = state.board.lock().map(|b| b.hot_paths()).unwrap_or_default();
            let survey = crate::orc::survey(&state.repo, &hot);
            let cold: Vec<String> = survey
                .cold
                .iter()
                .take(40)
                .map(|t| serde_json::to_string(&t.path).unwrap_or_else(|_| "\"\"".into()))
                .collect();
            let _ = out.text(&format!(
                "{{\"t\":\"survey\",\"cold\":[{}],\"hot\":{},\"dirty\":{}}}",
                cold.join(","),
                survey.hot,
                survey.dirty
            ));
        }
        "quit" => state.quit.store(true, Ordering::Relaxed),
        _ => {}
    }
}

fn exe_mtime() -> Option<SystemTime> {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
}

fn reload() -> io::Error {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sauron"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    crate::plat::reload(exe, args)
}

/// Unused outside the tick loop, but named so the intent survives: the process
/// stays up as long as `serve` is in its loop, regardless of how many browsers
/// are attached. Closing the last tab must not stop the agents.
#[allow(dead_code)]
fn watchers(state: &State) -> usize {
    state.clients.count()
}

#[allow(dead_code)]
fn _assert_instant(_: Instant) {}
