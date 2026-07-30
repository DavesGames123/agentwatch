//! The Unix half of `plat`. macOS is the platform sauron was built on, so every
//! function here is the original code -- moved, not rewritten. If any of it
//! starts behaving differently from the way it did before the port, that is a
//! bug in the move and not a design change.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The terminal this platform's workspace layer drives. Used in user-facing
/// messages so a failure names something the user can go and check.
pub const WORKSPACE_HOST: &str = "iTerm2";

pub(super) fn home_impl() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(super) fn clipboard_write_impl(text: &str) -> bool {
    let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

pub(super) fn run_in_place_impl(prog: &str, argv: &[String], cwd: &Path) -> std::io::Error {
    // exec only returns on failure, and then it returns the error.
    Command::new(prog).args(argv).current_dir(cwd).exec()
}

pub(super) fn reload_impl(exe: PathBuf, args: Vec<String>) -> std::io::Error {
    Command::new(exe).args(args).exec()
}
