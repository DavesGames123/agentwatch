//! One agent, running under a pseudo-terminal sauron owns.
//!
//! This is the difference between the web board watching your agents and the
//! web board *being* where they live. `sauron workspace` asks iTerm2 to open
//! panes and then has no further relationship with them -- it cannot read their
//! output, and the only way back in is `reply.rs` typing at a window through
//! AppleScript. A pty opened here is held by this process: its bytes come out on
//! a file descriptor, keystrokes go in on the same one, and neither depends on a
//! terminal emulator existing anywhere on the machine.
//!
//! WHY THE PROCESS OUTLIVES THE TAB
//! --------------------------------
//! The pty and the child belong to sauron, not to a websocket. Closing the tab,
//! reloading it, losing the wifi, or opening the board on a second machine
//! changes nothing about the agent -- it is mid-turn on a file descriptor that
//! nobody dropped. This is the property that makes a browser an acceptable place
//! to run agents at all; a design where the child dies with the socket would
//! lose a twenty-minute turn to a stray Cmd-W.
//!
//! WHY THERE IS A SCROLLBACK BUFFER HERE
//! -------------------------------------
//! A tab that attaches to a pty that has been running for an hour has missed the
//! hour. The alternative to replaying it is a blank rectangle under a tab
//! labelled with a session that is visibly working, which reads as broken. The
//! ring keeps the last `SCROLLBACK` bytes and hands them over on attach.
//!
//! It is a byte ring and not a screen: replaying from a truncation point can
//! start mid-escape-sequence, and xterm.js will discard the fragment and carry
//! on. The cost is a possible smudge on the first screenful after a reattach to
//! a very chatty agent; the alternative is a terminal emulator in this process
//! as well as in the page, which is a second implementation of the hardest part.
//!
//! grep targets:
//!   struct Pty        -- the child, its fds, and what it has said lately
//!   fn Pty::spawn     -- open a pty, start a command in it, start the reader
//!   fn Pty::write     -- keystrokes in
//!   fn Pty::resize    -- the tab's geometry, forwarded as SIGWINCH
//!   fn Pty::history   -- what to replay to a tab that just attached
//!   fn Pty::kill      -- close a tab for good
//!   const SCROLLBACK  -- how much is kept, and why that much

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, MasterPty, PtySize};

use super::Clients;

/// How much of each agent's output is kept for a tab that attaches late.
///
/// 256KB is roughly a few hundred screenfuls of Claude Code output -- enough to
/// scroll back through the turn you missed, not enough that twenty idle agents
/// cost anything worth measuring.
const SCROLLBACK: usize = 256 * 1024;

/// How much is read off the pty at a time. Large enough that a full-screen
/// repaint arrives in one or two frames rather than thirty.
const CHUNK: usize = 16 * 1024;

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    scrollback: Arc<Mutex<Vec<u8>>>,
    /// Set by the reader thread when the child's output ends, which is the only
    /// reliable "this agent is gone" signal -- `try_wait` races with a child
    /// that has exited but whose buffered output has not been drained.
    dead: Arc<AtomicBool>,
    pub cols: Mutex<u16>,
    pub rows: Mutex<u16>,
}

impl Pty {
    /// Open a pty, run `argv` in it at `cwd`, and start pumping its output at
    /// every attached browser.
    pub fn spawn(
        pane: u8,
        argv: &[String],
        cwd: &Path,
        cols: u16,
        rows: u16,
        clients: Arc<Clients>,
    ) -> std::io::Result<Self> {
        let (prog, rest) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;

        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(other)?;

        let mut cmd = CommandBuilder::new(prog);
        cmd.args(rest);
        cmd.cwd(cwd);
        // Claude Code draws a full TUI and checks these two before it decides
        // how. Without TERM it falls back to something line-oriented and the tab
        // shows a different program than the one iTerm would have shown.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair.slave.spawn_command(cmd).map_err(other)?;
        // The slave fd must be dropped or the master never sees EOF when the
        // child exits, and the tab hangs on a dead agent forever.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(other)?;
        let writer = pair.master.take_writer().map_err(other)?;

        let scrollback = Arc::new(Mutex::new(Vec::with_capacity(8 * 1024)));
        let dead = Arc::new(AtomicBool::new(false));

        {
            let (scrollback, dead) = (scrollback.clone(), dead.clone());
            std::thread::spawn(move || {
                let mut buf = vec![0u8; CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            if let Ok(mut sb) = scrollback.lock() {
                                sb.extend_from_slice(chunk);
                                if sb.len() > SCROLLBACK {
                                    // Drop from the front in one splice rather
                                    // than draining per byte; this runs on every
                                    // chunk once an agent has been going a while.
                                    let excess = sb.len() - SCROLLBACK;
                                    sb.drain(..excess);
                                }
                            }
                            clients.send_pty(pane, chunk);
                        }
                    }
                }
                dead.store(true, Ordering::Relaxed);
                clients.send_text(&format!("{{\"t\":\"exit\",\"pane\":{pane}}}"));
            });
        }

        Ok(Self {
            master: pair.master,
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            scrollback,
            dead,
            cols: Mutex::new(cols),
            rows: Mutex::new(rows),
        })
    }

    /// Keystrokes in, verbatim. What the browser sent is what the tty receives:
    /// no line discipline, no echo, no interpretation -- the child's own
    /// terminal settings decide all of that, exactly as under iTerm.
    pub fn write(&self, data: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    /// Forward the tab's geometry. This is what makes the agent's TUI reflow --
    /// portable-pty raises SIGWINCH, and a Claude Code that never gets one draws
    /// an 80x24 box in the middle of a wide window.
    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(20), rows.max(4));
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut c) = self.cols.lock() {
            *c = cols;
        }
        if let Ok(mut r) = self.rows.lock() {
            *r = rows;
        }
    }

    /// What a newly attached tab should be shown before it sees anything live.
    pub fn history(&self) -> Vec<u8> {
        self.scrollback.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// End the agent. Used when a tab is closed for good, not when it is merely
    /// navigated away from -- see the module header.
    pub fn kill(&self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn other(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}
