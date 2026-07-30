//! The operating system, behind one surface.
//!
//! sauron reads logs with std and draws with ratatui, and both of those are the
//! same on every platform. Everything else it does -- find the user's home, put
//! text on the clipboard, hand a pane over to an agent, restart itself after a
//! rebuild, open one more agent pane -- goes through the host, and the hosts do
//! not agree. This module is where they are made to.
//!
//! WHY A SURFACE AND NOT A FORK
//! ---------------------------
//! `model.rs` is the product: what counts as Working, what counts as Blocked,
//! when a session stops needing a test. A second copy of it on a second platform
//! would disagree with the first the day either was tuned, and nothing would go
//! red when it did. So there is one copy, and the parts underneath it that
//! genuinely differ are named here and implemented twice.
//!
//! WHAT DOES NOT LIVE HERE
//! -----------------------
//! The macOS window layer -- `gui`, `mirror`, and `reply`'s delivery hop -- is
//! not abstracted, because there is nothing on the other side to abstract it
//! against. iTerm2 exposes a session's `tty`, which is the only reliable join
//! between "a process I found with ps" and "a window I can type into"; Windows
//! Terminal exposes no such thing and has no send-text API at all. Pretending
//! otherwise with an empty trait impl would turn a missing feature into a silent
//! no-op, which in a tool whose whole job is telling you what you have not
//! noticed is the worst available failure. Those modules are compiled out and
//! say so out loud instead -- see `WORKSPACE_HOST` and the callers of
//! `unsupported`.
//!
//! KEEPING THE OTHER PLATFORM COMPILING
//! ------------------------------------
//! Only two modules are cut on Windows, and everything else in the crate --
//! including `host` and `web` -- is declared unconditionally. That is the right
//! default and it has one failure mode: a unix-only call added anywhere in the
//! portable majority breaks the Windows build, and nothing on a Mac notices,
//! because `cargo test` on a Mac never compiles for the other target.
//!
//! The check is one command and takes half a minute:
//!
//! ```text
//! rustup target add x86_64-pc-windows-gnu    # once; brew install mingw-w64
//! cargo check --all-targets --target x86_64-pc-windows-gnu
//! ```
//!
//! Run it after touching anything that reaches the machine -- a `Command`, a
//! path, an env var, a process. It is the only evidence available on a Mac that
//! the Windows half still builds, and it catches the whole class: `std::os::unix`
//! imports, `exec`, `$HOME`, and a `/`-joined path.
//!
//! What it cannot tell you is whether the result *works*. Every `wt.exe`
//! interaction, the clipboard, and PATHEXT resolution are unverified until
//! someone runs them on Windows.
//!
//! grep targets:
//!   fn home             -- $HOME / %USERPROFILE%
//!   fn clipboard_write  -- native clipboard; OSC 52 stays the caller's fallback
//!   fn run_in_place     -- become another program (exec, or spawn-wait-exit)
//!   fn reload           -- re-run this binary after a rebuild
//!   fn spawn_agent_pane -- one more agent pane in the running workspace
//!   fn spawn_orc_pane   -- one staged orc pane, never auto-run
//!   const WORKSPACE_HOST -- the terminal this platform drives, for messages

use std::path::PathBuf;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;

/// The user's home directory, or `.` if the host will not say.
///
/// The fallback is deliberately a relative path rather than a panic: sauron's
/// state lives under home and losing it is recoverable, but a watcher that
/// refuses to start because an environment variable is unset is not. Callers
/// that care -- anything that *writes* -- should surface where they landed.
pub fn home() -> PathBuf {
    home_impl()
}

/// Put `text` on the system clipboard using the platform's own mechanism.
///
/// Returns false when there is no native path or it failed, which is not an
/// error: every caller falls back to an OSC 52 escape, and that is the one that
/// works over SSH regardless of platform.
pub fn clipboard_write(text: &str) -> bool {
    clipboard_write_impl(text)
}

/// Become `prog`, replacing this process.
///
/// On Unix this is `exec`, which only returns on failure. Windows has no exec,
/// so there it is spawn-wait-exit: the parent stays alive for the child's
/// lifetime and then exits with the child's code. That difference is not
/// cosmetic and callers must not assume the pid survives -- but it *is* the
/// behaviour a pane wants either way, because what a pane cares about is that
/// it stays occupied until the agent is done.
///
/// Only returns when the handoff failed, and then it returns why.
pub fn run_in_place(prog: &str, argv: &[String], cwd: &std::path::Path) -> std::io::Error {
    run_in_place_impl(prog, argv, cwd)
}

/// Re-run this binary, same arguments, after it has been rebuilt underneath us.
///
/// The caller must have restored the terminal first: on the Unix path a failed
/// exec would otherwise strand the tty in raw mode, and on the Windows path the
/// child would start on a console the parent still thinks it owns.
pub fn reload(exe: PathBuf, args: Vec<String>) -> std::io::Error {
    reload_impl(exe, args)
}

/// What to say when a subcommand exists but this platform cannot carry it out.
///
/// Centralised so the answer is the same wherever it surfaces, and phrased as a
/// fact about the host rather than an apology: the user's next move is to run it
/// on a Mac or not at all, and a vaguer message would cost them the time it
/// takes to find that out.
pub fn unsupported(feature: &str) -> String {
    format!(
        "sauron: {feature} is macOS + iTerm2 only -- {WORKSPACE_HOST} exposes no \
         equivalent. The watcher, the board, and pane spawning all work here."
    )
}

/// Resolve a bare program name to something this platform's `Command` can spawn.
///
/// On Unix a name is a name and this returns it unchanged. On Windows it is not:
/// `Command::new` appends `.exe` and nothing else, while an npm-installed agent
/// is `claude.cmd` and a shim is `codex.bat`. The result is a `NotFound` for a
/// program that is plainly on the PATH and runs fine when typed -- so the search
/// PATHEXT describes is done here instead.
///
/// A name that resolves to nothing comes back unchanged, so the failure is still
/// the spawn's own "not found" rather than something invented here.
pub fn resolve_program(name: &str) -> String {
    #[cfg(not(windows))]
    {
        name.to_string()
    }
    #[cfg(windows)]
    {
        // An explicit path is already an answer.
        if name.contains('\\') || name.contains('/') {
            return name.to_string();
        }
        let exts = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let Some(path) = std::env::var_os("PATH") else {
            return name.to_string();
        };
        for dir in std::env::split_paths(&path) {
            for ext in exts.split(';').filter(|e| !e.is_empty()) {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return candidate.to_string_lossy().into_owned();
                }
            }
        }
        name.to_string()
    }
}

/// The PowerShell a pane runs: `pwsh.exe` when PowerShell 7 is installed, else
/// the 5.1 that ships with every Windows.
///
/// Lives here rather than in `win` because the Windows layout builder is
/// compiled on every platform so that its tests are -- and a test asserting on a
/// pane commandline needs this to answer without a Windows to ask. Off Windows
/// it answers `powershell.exe` flatly, with no probe: there is nothing there to
/// probe and the only caller is a test.
pub fn shell_exe() -> String {
    if let Some(over) = std::env::var_os("SAURON_POWERSHELL") {
        return over.to_string_lossy().into_owned();
    }
    #[cfg(windows)]
    if win::pwsh_present() {
        return "pwsh.exe".to_string();
    }
    "powershell.exe".to_string()
}

/// Translate a pane command this crate generated into PowerShell.
///
/// **Only commands this crate generated.** This is not a shell translator and
/// must never be handed arbitrary text. Everything it accepts comes out of
/// `workspace::left_commands`, `orc::stage_command`, or
/// `handoff::workspace_command`, all of which emit exactly one shape:
///
/// ```text
/// cd '<repo>' && [KEY=VAL ]* <program> [args...]
/// ```
///
/// Three mechanical rewrites turn that into the PowerShell equivalent:
///
/// 1. The `cd '<repo>' && ` prefix is dropped. The pane is opened with
///    `--startingDirectory`, which puts the shell there before it runs anything,
///    and PowerShell 5.1 has no `&&` at all -- so translating the `cd` rather
///    than deleting it would produce a line the older shell cannot parse.
/// 2. A leading run of `KEY=VAL` assignments -- the Mordor env prefix, and empty
///    in every other mode -- becomes `$env:KEY='VAL';` statements.
/// 3. POSIX single-quoting (`'\''` for an embedded quote) becomes PowerShell
///    single-quoting (`''`). The two agree on everything else.
///
/// Compiled on every platform so its tests run on every platform: it is a string
/// function, and the machine that most needs to check it is the one doing the
/// porting rather than the one running the result.
pub fn to_powershell(command: &str) -> String {
    let body = strip_cd_prefix(command);
    let (assignments, rest) = split_env_prefix(body);

    let mut out = String::new();
    for (key, value) in assignments {
        // The value came out of a POSIX context unquoted (a URL, a model name),
        // so it is quoted here rather than merely copied -- an empty one
        // (`ANTHROPIC_API_KEY=`) would otherwise become a syntax error.
        out.push_str(&format!("$env:{key}='{}'; ", value.replace('\'', "''")));
    }
    out.push_str(&posix_quotes_to_powershell(rest));
    out
}

/// Drop a leading `cd <dir> && `, quoted or not. Anything else is returned whole.
fn strip_cd_prefix(command: &str) -> &str {
    let rest = match command.strip_prefix("cd ") {
        Some(rest) => rest,
        None => return command,
    };
    match rest.find("&& ") {
        Some(at) => &rest[at + 3..],
        None => command,
    }
}

/// Peel `KEY=VAL ` pairs off the front. Stops at the first word that is not an
/// assignment, which is the program -- so `claude --resume a=b` keeps its
/// argument instead of losing it to the env.
fn split_env_prefix(command: &str) -> (Vec<(&str, &str)>, &str) {
    let mut assignments = Vec::new();
    let mut rest = command;
    loop {
        let word = match rest.split_once(' ') {
            Some((word, _)) => word,
            None => break,
        };
        let Some((key, value)) = word.split_once('=') else {
            break;
        };
        // An assignment's name is a name. This is what stops `--flag=value` and
        // any path containing `=` from being eaten as environment.
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            break;
        }
        assignments.push((key, value));
        rest = &rest[word.len() + 1..];
    }
    (assignments, rest)
}

/// `'\''` -> `''`. Both shells quote with `'`; they differ only in how the quote
/// escapes itself inside one.
fn posix_quotes_to_powershell(command: &str) -> String {
    command.replace("'\\''", "''")
}

/// Minimal standard-alphabet base64. Shared by the OSC 52 clipboard fallback and
/// (on macOS) the mirror's inline-image protocol -- one encoder, because two
/// would disagree the first time either was touched, and both feed escape
/// sequences to the same terminal.
///
/// It lives here rather than in `mirror` because the clipboard path is the one
/// that exists on every platform: leaving it in a macOS-only module would have
/// taken the Windows clipboard fallback down with it.
pub fn base64(input: &[u8]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cd_prefix_becomes_the_panes_starting_directory_instead() {
        // PowerShell 5.1 has no `&&`, so a translated `cd` would be worse than a
        // dropped one -- the pane would open on a parse error.
        let out = to_powershell("cd '/Users/d/repo' && claude --resume abc");
        assert_eq!(out, "claude --resume abc");
        assert!(!out.contains("&&"));
    }

    #[test]
    fn the_mordor_env_prefix_becomes_powershell_assignments() {
        let out = to_powershell(
            "cd '/r' && ANTHROPIC_BASE_URL=http://x:11434 ANTHROPIC_API_KEY= claude",
        );
        assert!(out.starts_with("$env:ANTHROPIC_BASE_URL='http://x:11434'; "));
        // An empty value still has to be quoted or PowerShell will not parse it.
        assert!(out.contains("$env:ANTHROPIC_API_KEY=''; "));
        assert!(out.ends_with("claude"));
    }

    #[test]
    fn an_argument_containing_equals_is_not_mistaken_for_environment() {
        // The guard that matters: `--resume` values and paths may contain `=`,
        // and eating one as an assignment would silently drop it from the argv.
        let out = to_powershell("cd '/r' && claude --resume a=b");
        assert_eq!(out, "claude --resume a=b");
        let out = to_powershell("cd '/r' && sauron handoff-run --key 'x=y'");
        assert!(out.contains("--key 'x=y'"));
    }

    #[test]
    fn posix_quote_escaping_becomes_powershell_quote_escaping() {
        // `handoff::shell_quote` emits '\'' for an embedded quote; PowerShell
        // wants ''. Getting this wrong ends the string early and the rest of the
        // command becomes stray tokens.
        let out = to_powershell("cd '/r' && sauron handoff-run --repo '/it'\\''s/repo'");
        assert!(out.contains("'/it''s/repo'"));
        assert!(!out.contains("\\'"));
    }

    #[test]
    fn a_command_with_no_prefix_survives_untouched() {
        assert_eq!(to_powershell("claude"), "claude");
    }
}
