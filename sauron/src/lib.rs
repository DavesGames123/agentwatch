//! sauron as a library -- the watcher's machinery, without the terminal
//! lifecycle that `main.rs` owns.
//!
//! The library exists because there is a second front end: `muthur`, a
//! multi-project board that aggregates several repos' worth of the same rows
//! side by side. It links this crate rather than re-deriving session status,
//! which is the whole point -- `model.rs` is the product, and two copies of it
//! would disagree the first time either was tuned.
//!
//! `ui` and `scene` are here too, even though muthur draws its own screen.
//! Keeping them in the crate means every existing `crate::` path inside them
//! resolves unchanged; moving them into the binary would have bought nothing and
//! cost a rewrite of their imports.
//!
//! The re-exports below are load-bearing, not convenience: `ui.rs` refers to
//! `crate::Row`, and `workspace.rs`/`orc.rs` refer to `crate::git_root`,
//! `crate::in_flight_tasks`, and `crate::hot_files` -- all of which lived in
//! `main.rs` before the split. Re-exporting them at the root is what let those
//! three files move into the library without a single line changing.
//!
//! PLATFORM
//! --------
//! The watcher proper -- `scan`, `model`, `board`, `store`, `ui`, `scene` -- is
//! plain std and ratatui, and runs anywhere. Everything that reaches the host
//! goes through `plat`, which is implemented twice.
//!
//! Three modules are macOS-only and compiled out elsewhere: `gui` and `mirror`
//! drive the window server, and `reply`'s delivery hop needs a session's `tty`,
//! which iTerm2 publishes and Windows Terminal has no concept of. They are cut
//! rather than stubbed -- see `plat`'s header for why an empty impl would be the
//! worse failure -- and the subcommands that reach them say so.
//!
//! WHERE THE AGENTS RUN
//! --------------------
//! `workspace` opens agents as iTerm2 panes and then has no further relationship
//! with them. `web` opens them as pseudo-terminals this process owns, and serves
//! a browser that is the terminal on the end of them -- so the same agents can
//! be run and watched with no terminal emulator and no macOS at all. Both drive
//! the same `board`; neither re-derives what a session's state is.
//!
//! grep targets:
//!   mod beacon      -- publishing the board where a watched project can read it
//!   mod board       -- the headless per-repo watcher, shared by both front ends
//!   mod model       -- session model and status classification
//!   mod plat        -- the host OS, behind one surface: home, clipboard, panes
//!   mod scan        -- incremental log tailer
//!   mod servant     -- which servant a session is: its name and its colour
//!   mod store       -- ack persistence, safe against concurrent writers
//!   mod web         -- the board as a web app, with the agents running inside it
//!   mod gui         -- docking a project's own app window into the workspace [macOS]
//!   mod mirror      -- drawing that window *inside* a pane, frame by frame [macOS]

pub mod agent;
pub mod beacon;
pub mod board;
pub mod clip;
pub mod codex;
#[cfg(unix)]
pub mod gui;
pub mod handoff;
#[cfg(unix)]
pub mod mirror;
pub mod model;
pub mod orc;
pub mod panel;
pub mod plat;
#[cfg(unix)]
pub mod reply;
pub mod route;
pub mod scan;
pub mod servant;
pub mod scene;
pub mod store;
pub mod ui;
pub mod web;
pub mod workspace;

pub use board::{git_root, hot_files, in_flight_tasks, Board, Row};
