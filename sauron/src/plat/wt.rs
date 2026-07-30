//! Windows Terminal command construction: what `wt.exe` is actually handed.
//!
//! WHY THIS IS NOT IN `win`
//! -----------------------
//! Everything here is pure string building, and `win` is compiled only on
//! Windows -- which meant these functions and their tests existed on a Mac as
//! text that `cargo test` never ran. They compiled for the target and executed
//! nowhere, which is indistinguishable from not being tested at all.
//!
//! Split out so they are compiled and *run* on every platform. The shell to
//! launch arrives as a parameter rather than being probed, so a test states the
//! answer instead of depending on which PowerShell the machine happens to have.
//!
//! The one thing this cannot establish is that `wt.exe` accepts the result. That
//! needs a Windows machine, and no amount of restructuring substitutes for it.
//!
//! grep targets:
//!   fn agent_pane_argv  -- the split that grows the agent column
//!   fn orc_pane_argv    -- the split that stages an orc, unrun
//!   fn shell_command    -- a pane's commandline, run by PowerShell
//!   fn ps_single_quote  -- PowerShell literal escaping (double the quote)

/// The split that grows the agent column.
///
/// `move-focus first` is the stand-in for iTerm's "the sessions ahead of me":
/// the launch layout builds the agent column first, so pane 0 is in it. The
/// split is horizontal, which in `wt`'s vocabulary means the new pane lands
/// *below* the one being split -- the column grows downward, as it does on macOS.
pub fn agent_pane_argv(
    window: &str,
    sauron_pane: Option<u32>,
    cmd: &str,
    focus: bool,
    shell: &str,
) -> Vec<String> {
    let mut argv = vec![
        "-w".to_string(),
        window.to_string(),
        "move-focus".to_string(),
        "first".to_string(),
        ";".to_string(),
        "split-pane".to_string(),
        "--horizontal".to_string(),
        "--".to_string(),
    ];
    argv.extend(shell_command(cmd, shell));

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
pub fn orc_pane_argv(window: &str, cmd: &str, shell: &str) -> Vec<String> {
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
        shell.to_string(),
        "-NoExit".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        staged,
    ]
}

/// Run `cmd` through a shell, so a pane command can be the same string on both
/// platforms. `-NoExit` keeps the pane alive after the agent exits, matching
/// iTerm's behaviour of leaving the session open.
pub fn shell_command(cmd: &str, shell: &str) -> Vec<String> {
    vec![
        shell.to_string(),
        "-NoExit".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        cmd.to_string(),
    ]
}

/// Escape a string for a PowerShell single-quoted literal, where the only
/// metacharacter is the quote itself and it escapes by doubling. Single quotes
/// are used rather than double precisely because of this: inside double quotes
/// PowerShell would expand `$`, and a repo path or a brief is allowed to contain
/// one.
pub fn ps_single_quote(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SH: &str = "powershell.exe";

    #[test]
    fn agent_split_targets_the_named_window_and_grows_the_column_down() {
        let argv = agent_pane_argv("sauron-worldsmith", None, "claude", true, SH);
        assert_eq!(argv[0], "-w");
        assert_eq!(argv[1], "sauron-worldsmith");
        // The column is found by walking to its first pane, exactly as the iTerm
        // path finds "everything ahead of me".
        assert!(argv.windows(2).any(|w| w == ["move-focus", "first"]));
        assert!(argv.contains(&"--horizontal".to_string()));
    }

    #[test]
    fn agent_split_hands_focus_back_only_when_launch_recorded_a_pane() {
        let with = agent_pane_argv("w", Some(3), "claude", false, SH);
        assert!(with.windows(2).any(|w| w == ["--target", "3"]));

        // No recorded index means no focus restore -- and specifically not a
        // guessed one, which would drop the user into someone else's pane.
        let without = agent_pane_argv("w", None, "claude", false, SH);
        assert!(!without.contains(&"focus-pane".to_string()));

        // Asking for focus never restores it, whatever launch recorded.
        let focused = agent_pane_argv("w", Some(3), "claude", true, SH);
        assert!(!focused.contains(&"focus-pane".to_string()));
    }

    #[test]
    fn orc_pane_stages_the_command_without_running_it() {
        let argv = orc_pane_argv("w", "sauron orc src\\big.rs", SH);
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
        let argv = orc_pane_argv("w", "sauron orc it's.rs", SH);
        let joined = argv.join(" ");
        assert!(joined.contains("AddToHistory('sauron orc it''s.rs')"));
    }

    #[test]
    fn a_semicolon_in_the_command_is_not_a_wt_separator() {
        // wt splits its own argv on a bare `;` element. The command travels as
        // one element, so an embedded semicolon is text -- assert that rather
        // than trusting it, because the failure mode is wt running half a
        // command as a second subcommand.
        let argv = shell_command("claude --resume a;b", SH);
        assert!(argv.iter().any(|a| a == "claude --resume a;b"));
        assert!(!argv.iter().any(|a| a == ";"));
    }

    #[test]
    fn every_pane_command_arrives_as_exactly_one_argv_element() {
        // The general form of the rule above, checked across all three builders:
        // `wt` reads a lone `;` as a separator, so no builder may ever emit the
        // user's command split across elements.
        let nasty = "claude --resume a;b ; rm -rf /";
        for argv in [
            agent_pane_argv("w", None, nasty, true, SH),
            orc_pane_argv("w", nasty, SH),
            shell_command(nasty, SH),
        ] {
            let separators = argv.iter().filter(|a| a.as_str() == ";").count();
            let carriers = argv.iter().filter(|a| a.contains("a;b")).count();
            assert_eq!(carriers, 1, "the command must ride in exactly one element");
            // Whatever separators the builder emits are its own, not the
            // command's -- the command contributed none.
            assert!(separators <= 1, "argv: {argv:?}");
        }
    }
}
