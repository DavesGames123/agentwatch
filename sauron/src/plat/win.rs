//! The Windows half of `plat`: PowerShell and Windows Terminal standing in for
//! the shell and iTerm2.
//!
//! WHAT WINDOWS TERMINAL WILL AND WILL NOT DO
//! ------------------------------------------
//! The iTerm2 layer this replaces asks questions and gets answers: enumerate the
//! sessions of a tab, read each one's height, split the tallest, write text into
//! a specific pane. Windows Terminal has no scripting API at all. It has a
//! command line, `wt.exe`, which can only *do* things to the window it targets,
//! and only ever to whichever pane currently has focus.
//!
//! Three consequences, all of them visible to the user, none of them fixable
//! from here:
//!
//! * **No geometry.** The iTerm layer splits the tallest pane in a column,
//!   because repeatedly splitting the newest drives it under the minimum height
//!   and the split throws. `wt` cannot report a pane's size, so this splits the
//!   first pane in the column instead. On a column that is already crowded the
//!   split will fail where iTerm's would have succeeded.
//! * **Focus is positional, not addressed.** There is no "focus the pane running
//!   sauron". `sauron workspace` therefore records the pane index it assigned
//!   itself in `SAURON_WT_PANE` at launch and focuses back by index. Close a pane
//!   by hand and the indices shift underneath that.
//! * **No send-text.** Nothing can type into a pane that already exists, which is
//!   why `reply` is compiled out on this platform rather than approximated.
//!
//! STAGING AN ORC WITHOUT RUNNING IT
//! ---------------------------------
//! An orc is staged, never auto-run: the target has to be readable before
//! anything starts editing it. iTerm gets this with `write text ... newline no`,
//! which types the line and leaves it awaiting Enter. `wt split-pane <cmd>` has
//! no such mode -- it runs what you give it. So the pane opens on an interactive
//! shell with the command pushed onto the PSReadLine history and a banner saying
//! so: the command is one Up-arrow from running and zero risk of running itself.
//! That is the same contract, reached differently, and the difference is worth
//! knowing about because the keystroke is not the same keystroke.
//!
//! grep targets:
//!   fn powershell_exe   -- pwsh if present, else the always-installed 5.1
//!   fn wt_window        -- which WT window this process's workspace owns
//!   fn agent_pane_argv  -- the split that grows the agent column
//!   fn orc_pane_argv    -- the split that stages an orc, unrun
//!   fn ps_single_quote  -- PowerShell literal escaping (double the quote)

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The terminal this platform's workspace layer drives. Used in user-facing
/// messages so a failure names something the user can go and check.
pub const WORKSPACE_HOST: &str = "Windows Terminal";

/// Env var naming the `wt` window `sauron workspace` launched into. Set on every
/// pane that launch creates, so a sauron running in one of them can grow the
/// layout it is part of rather than guessing at a window.
pub const WT_WINDOW_ENV: &str = "SAURON_WT_WINDOW";

/// Env var holding the pane index `sauron workspace` assigned to sauron itself,
/// so a spawn can hand focus back. Absent when sauron was started by hand.
pub const WT_PANE_ENV: &str = "SAURON_WT_PANE";

pub(super) fn home_impl() -> PathBuf {
    // USERPROFILE is what every Windows shell sets and what Claude Code itself
    // resolves `~` to. HOMEDRIVE+HOMEPATH is the older pair, still set on
    // domain-joined machines where USERPROFILE may point at a redirected
    // profile; taking it second means a redirected profile wins, which is the
    // one the agent logs will actually be under.
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(p);
    }
    if let (Some(drive), Some(path)) = (
        std::env::var_os("HOMEDRIVE"),
        std::env::var_os("HOMEPATH"),
    ) {
        let mut joined = PathBuf::from(drive);
        joined.push(PathBuf::from(path).strip_prefix("\\").unwrap_or(Path::new("")));
        if joined.as_os_str().len() > 2 {
            return joined;
        }
    }
    PathBuf::from(".")
}

pub(super) fn clipboard_write_impl(text: &str) -> bool {
    // clip.exe reads stdin and is present on every Windows install, which
    // `pwsh -c Set-Clipboard` is not -- and it starts in single-digit
    // milliseconds where a PowerShell would cost a few hundred.
    //
    // The encoding is the catch. clip.exe interprets raw bytes in the console
    // codepage, which mangles anything non-ASCII, but it honours a UTF-16LE BOM.
    // A repo path with an accent in it is not exotic, so encode rather than hope.
    let Ok(mut child) = Command::new("clip.exe").stdin(Stdio::piped()).spawn() else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let mut buf = vec![0xFF, 0xFE]; // UTF-16LE BOM
        for unit in text.encode_utf16() {
            buf.extend_from_slice(&unit.to_le_bytes());
        }
        let _ = stdin.write_all(&buf);
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

pub(super) fn run_in_place_impl(prog: &str, argv: &[String], cwd: &Path) -> std::io::Error {
    // Windows has no exec. Spawn-wait-exit is the closest thing, and for the one
    // caller that matters -- a pane handing itself to an agent -- it is actually
    // the right shape: what the pane needs is to stay occupied until the agent
    // is done, and an exec'd process and a waited-on child both do that.
    //
    // The pid does not survive, unlike on Unix. Nothing currently depends on it.
    // Resolved rather than spawned bare: the one caller hands over to an agent,
    // and an npm-installed agent is a `.cmd` that `Command::new` cannot find.
    let prog = super::resolve_program(prog);
    match Command::new(prog).args(argv).current_dir(cwd).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}

pub(super) fn reload_impl(exe: PathBuf, args: Vec<String>) -> std::io::Error {
    match Command::new(exe).args(args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(0)),
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// Growing the workspace from inside the running TUI.
// ---------------------------------------------------------------------------

/// Open one more agent pane in the agent column of the workspace window this
/// process is running in, running `cmd`.
///
/// `focus` selects the new pane; with it off, focus returns to sauron by the
/// index recorded at launch. See the module header for why that index exists and
/// what invalidates it.
pub fn spawn_agent_pane(cmd: &str, focus: bool) -> Result<(), String> {
    run_wt(&agent_pane_argv(&wt_window(), wt_pane(), cmd, focus))
}

/// Stage an orc in sauron's own column: a pane that opens with the command
/// loaded and waiting, never run.
pub fn spawn_orc_pane(cmd: &str) -> Result<(), String> {
    run_wt(&orc_pane_argv(&wt_window(), cmd))
}

/// Run the launch layout. Unlike the spawn paths this one is allowed to fail
/// loudly -- it is a command the user typed, not a keystroke inside the TUI.
pub fn run_wt_layout(argv: &[String]) -> std::io::Result<()> {
    run_wt(argv).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("sauron workspace: {e}"),
        )
    })
}

/// Hand the argv to `wt.exe`. Its exit code is the only answer it gives -- there
/// is no equivalent of iTerm's `ERR <why>` string, so a failure here can say
/// that the split was refused but not why.
fn run_wt(argv: &[String]) -> Result<(), String> {
    match Command::new("wt.exe").args(argv).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "{WORKSPACE_HOST} refused the split (exit {})",
            s.code().unwrap_or(-1)
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("{WORKSPACE_HOST} (wt.exe) is not on PATH"))
        }
        Err(e) => Err(format!("could not run wt.exe: {e}")),
    }
}

/// The `wt` window to target. The one `sauron workspace` launched, when this
/// process is in it; otherwise `0`, which `wt` reads as "the most recently used
/// window" -- a guess, but the only one available, and a wrong guess opens a
/// visible pane in the wrong window rather than doing something silent.
fn wt_window() -> String {
    std::env::var(WT_WINDOW_ENV)
        .ok()
        .filter(|w| !w.trim().is_empty())
        .unwrap_or_else(|| "0".to_string())
}

/// The pane index sauron holds, if launch recorded one.
fn wt_pane() -> Option<u32> {
    std::env::var(WT_PANE_ENV).ok()?.trim().parse().ok()
}

/// The split that grows the agent column.
///
/// `move-focus first` is the stand-in for iTerm's "the sessions ahead of me":
/// the launch layout builds the agent column first, so pane 0 is in it. The
/// split is horizontal, which in `wt`'s vocabulary means the new pane lands
/// *below* the one being split -- the column grows downward, as it does on macOS.
fn agent_pane_argv(window: &str, sauron_pane: Option<u32>, cmd: &str, focus: bool) -> Vec<String> {
    let mut argv = vec![
        "-w".to_string(),
        window.to_string(),
        "move-focus".to_string(),
        "first".to_string(),
        ";".to_string(),
        "split-pane".to_string(),
        "--horizontal".to_string(),
    ];
    argv.push("--".to_string());
    argv.extend(shell_command(cmd));

    // Focus lands on the new pane by default, so only the "leave me where I was"
    // case needs anything more -- and it can only be honoured when launch told us
    // where that was.
    if !focus {
        if let Some(idx) = sauron_pane {
            argv.push(";".to_string());
            argv.push("focus-pane".to_string());
            argv.push("--target".to_string());
            argv.push(idx.to_string());
        }
    }
    argv
}

/// The split that stages an orc: a shell with the command in its history and a
/// banner saying how to run it. Nothing executes until the user says so.
fn orc_pane_argv(window: &str, cmd: &str) -> Vec<String> {
    let staged = format!(
        "[Microsoft.PowerShell.PSConsoleReadLine]::AddToHistory('{}'); \
         Write-Host 'sauron: orc staged -- press Up then Enter to loose it' \
         -ForegroundColor Yellow; Write-Host '{}' -ForegroundColor DarkGray",
        ps_single_quote(cmd),
        ps_single_quote(cmd),
    );
    vec![
        "-w".to_string(),
        window.to_string(),
        "split-pane".to_string(),
        "--horizontal".to_string(),
        "--".to_string(),
        powershell_exe(),
        "-NoExit".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        staged,
    ]
}

/// Run `cmd` through a shell, so a pane command can be the same string on both
/// platforms. `-NoExit` keeps the pane alive after the agent exits, matching
/// iTerm's behaviour of leaving the session open.
fn shell_command(cmd: &str) -> Vec<String> {
    vec![
        powershell_exe(),
        "-NoExit".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        cmd.to_string(),
    ]
}

/// Is PowerShell 7 installed? Probing costs one process spawn, and only happens
/// when a pane is opened. The choice itself is `plat::shell_exe`'s -- it is
/// needed by the layout builder, which is compiled off Windows too.
pub(super) fn pwsh_present() -> bool {
    Command::new("pwsh.exe")
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("exit 0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The PowerShell a pane runs, as `plat::shell_exe` decides it.
fn powershell_exe() -> String {
    super::shell_exe()
}

/// Escape a string for a PowerShell single-quoted literal, where the only
/// metacharacter is the quote itself and it escapes by doubling. Single quotes
/// are used rather than double precisely because of this: inside double quotes
/// PowerShell would expand `$`, and a repo path or a brief is allowed to contain
/// one.
fn ps_single_quote(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_split_targets_the_named_window_and_grows_the_column_down() {
        let argv = agent_pane_argv("sauron-worldsmith", None, "claude", true);
        assert_eq!(argv[0], "-w");
        assert_eq!(argv[1], "sauron-worldsmith");
        // The column is found by walking to its first pane, exactly as the iTerm
        // path finds "everything ahead of me".
        assert!(argv.windows(2).any(|w| w == ["move-focus", "first"]));
        assert!(argv.contains(&"--horizontal".to_string()));
    }

    #[test]
    fn agent_split_hands_focus_back_only_when_launch_recorded_a_pane() {
        let with = agent_pane_argv("w", Some(3), "claude", false);
        assert!(with.windows(2).any(|w| w == ["--target", "3"]));

        // No recorded index means no focus restore -- and specifically not a
        // guessed one, which would drop the user into someone else's pane.
        let without = agent_pane_argv("w", None, "claude", false);
        assert!(!without.contains(&"focus-pane".to_string()));

        // Asking for focus never restores it, whatever launch recorded.
        let focused = agent_pane_argv("w", Some(3), "claude", true);
        assert!(!focused.contains(&"focus-pane".to_string()));
    }

    #[test]
    fn orc_pane_stages_the_command_without_running_it() {
        let argv = orc_pane_argv("w", "sauron orc src\\big.rs");
        let joined = argv.join(" ");
        // The command reaches the pane as history, never as the thing the pane
        // was told to execute. If this ever becomes the pane's own commandline,
        // the orc runs unreviewed -- which is the failure this test exists for.
        assert!(joined.contains("AddToHistory('sauron orc src\\big.rs')"));
        assert!(argv.iter().any(|a| a == "-NoExit"));
        assert!(!argv.iter().any(|a| a == "sauron orc src\\big.rs"));
    }

    #[test]
    fn a_quote_in_the_command_cannot_break_out_of_the_literal() {
        let argv = orc_pane_argv("w", "sauron orc it's.rs");
        let joined = argv.join(" ");
        assert!(joined.contains("AddToHistory('sauron orc it''s.rs')"));
    }

    #[test]
    fn a_semicolon_in_the_command_is_not_a_wt_separator() {
        // wt splits its own argv on a bare `;` element. The command travels as
        // one element, so an embedded semicolon is text -- assert that rather
        // than trusting it, because the failure mode is wt running half a
        // command as a second subcommand.
        let argv = shell_command("claude --resume a;b");
        assert!(argv.iter().any(|a| a == "claude --resume a;b"));
        assert!(!argv.iter().any(|a| a == ";"));
    }
}
