//! `sauron workspace` -- open a fullscreen iTerm2 multi-agent layout on its own
//! macOS Space, or manage the saved-project registry it launches from. This is
//! the whole of what used to be `workspace/workspace.sh`, moved into the binary
//! so there is one command and no shell-out: the in-flight sessions come
//! straight from the scanner, not from `sauron --list-working`.
//!
//! Left column  : one pane per in-flight session (Working or Delegated), each
//!                resumed with `claude --resume <id>`; extra panes are bare
//!                `claude`. Right column: sauron (top) + two shells at the repo.
//!
//! grep targets:
//!   fn run              -- entry point: alias subcommands, then launch
//!   fn resolve          -- a project arg (path or saved alias) -> repo dir
//!   fn store_* / alias  -- the name->path registry (~/.claude/sauron/workspaces)
//!   fn applescript      -- the iTerm layout script
//!   fn gui_applescript  -- the same, with a hole in it for the project's app
//!   fn spawn_left_pane  -- grow the agent column from inside the running TUI
//!   fn spawn_script     -- the split script that does it
//!   fn spawn_orc_pane   -- stage an orc in sauron's own column, from the TUI
//!   fn orc_command      -- the short line an orc pane runs (`sauron orc <file>`)
//!   fn osascript        -- pipe the script to `osascript`
//!
//! The orc *charge* -- what the agent is actually told to do, and which files are
//! cold enough to hand it -- lives in `orc`, not here. This module only decides
//! where the pane goes.
//!
//! CHECKING THE APPLESCRIPT ACTUALLY PARSES
//! ----------------------------------------
//! The tests here assert on the *text* of a script in another language, which
//! catches a missing pane and not a syntax error. `osacompile` compiles without
//! running, and is the only cheap way to learn that iTerm2 will accept what this
//! emits:
//!
//! ```text
//! WORKSPACE_PRINT_SCRIPT=1 sauron workspace 3 <repo> --yes | osacompile -o /tmp/x.scpt
//! WORKSPACE_PRINT_SCRIPT=1 sauron workspace 3 <repo> --yes --gui=./run.sh | osacompile -o /tmp/x.scpt
//! ```
//!
//! Worth doing after any edit to a script template. It caught a handler that
//! referred to iTerm2's `background color` from outside a `tell application`
//! block -- valid-looking text that every string assertion passed and that
//! AppleScript rejects on sight.
//!
//! A repo that declares a GUI (`.sauron/gui.conf`, see `gui`) gets the second
//! layout instead: four quarter-width columns, with the middle two left as a
//! hole for the application's own window. Every other repo takes the original
//! path, native fullscreen and all, byte for byte.

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};

use crate::agent::{Agent, Mordor};
#[cfg(unix)]
use crate::gui::Gui;
use crate::scan::home;

/// Entry point for `sauron workspace <args>` (args are everything after the
/// `workspace` word). `explicit_agent` is the `--claude`/`--codex` choice from
/// the top level, if any.
pub fn run(args: &[String], explicit_agent: Option<Agent>) -> std::io::Result<()> {
    // Registry subcommands run and return before any launch work.
    match args.first().map(|s| s.as_str()) {
        Some("alias") | Some("aliases") => {
            match (args.get(1), args.get(2)) {
                (Some(name), Some(path)) => alias_set(name, path)?,
                (Some(_), None) => {
                    eprintln!("usage: sauron workspace alias <name> <path>");
                    std::process::exit(2);
                }
                _ => alias_list(false),
            }
            return Ok(());
        }
        Some("unalias") | Some("forget") => {
            match args.get(1) {
                Some(name) => alias_del(name)?,
                None => {
                    eprintln!("usage: sauron workspace unalias <name>");
                    std::process::exit(2);
                }
            }
            return Ok(());
        }
        Some("ls") | Some("list") => {
            alias_list(false);
            return Ok(());
        }
        _ => {}
    }

    // Pull `--orcs N` (or `--orcs=N`) out first: N single-shot maintenance agents
    // that refactor / decompose / de-warn the cold, uncontested parts of the repo
    // while the hobbits do the directed work. The rest is [init] [N] [project],
    // order-independent -- a purely-numeric arg is the pane count, else the project.
    let mut orcs = 0usize;
    let mut clipboard_handoff = false;
    let mut yes = false; // skip the confirmation dialogue
    // Mordor mode: run the servants against a local model. `--mordor` takes the
    // Qwen default on local Ollama; `--mordor=<tag>` picks another Ollama model.
    // `--nostromo[=<tag>]` is the same, but pointed at the nostromo box over
    // Tailscale instead of localhost -- a remote local-swarm in one word.
    let mut mordor: Option<Mordor> = None;
    // GUI docking: `--gui` forces it on for a repo with no conf (taking the
    // command inline), `--no-gui` opens the ordinary layout in a repo that has
    // one. Neither is needed in the normal case -- the repo's conf decides.
    let mut gui_flag: Option<Option<String>> = None;
    let mut no_gui = false;
    let mut pos: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--orcs" {
            match args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                Some(n) => orcs = n,
                None => {
                    eprintln!("usage: sauron workspace [N] [project] --orcs <N>");
                    std::process::exit(2);
                }
            }
            i += 2;
        } else if let Some(rest) = a.strip_prefix("--orcs=") {
            orcs = rest.parse().unwrap_or(0);
            i += 1;
        } else if a == "--mordor" {
            mordor = Some(Mordor::new(None, None));
            i += 1;
        } else if let Some(rest) = a.strip_prefix("--mordor=") {
            mordor = Some(Mordor::new(Some(rest.to_string()), None));
            i += 1;
        } else if a == "--nostromo" || a.starts_with("--nostromo=") {
            // Same as --mordor, but pointed at the nostromo box over Tailscale.
            // The URL is private, so it is read from local config, never source;
            // if it isn't set, say how to set it rather than guessing an endpoint.
            let tag = a.strip_prefix("--nostromo=").map(str::to_string);
            match Mordor::nostromo(tag) {
                Some(m) => mordor = Some(m),
                None => {
                    eprintln!(
                        "sauron workspace: --nostromo needs the box's Ollama URL, and it is not set."
                    );
                    eprintln!(
                        "  export SAURON_NOSTROMO_URL=https://<your-box>.<tailnet>.ts.net, or write that"
                    );
                    eprintln!(
                        "  URL to ~/.claude/sauron/nostromo-url (kept out of the repo)."
                    );
                    std::process::exit(2);
                }
            }
            i += 1;
        } else if a == "--gui" {
            gui_flag = Some(None);
            i += 1;
        } else if let Some(rest) = a.strip_prefix("--gui=") {
            gui_flag = Some(Some(rest.to_string()));
            i += 1;
        } else if a == "--no-gui" {
            no_gui = true;
            i += 1;
        } else if a == "--yes" || a == "-y" {
            yes = true;
            i += 1;
        } else if a == "--clipboard-handoff" {
            clipboard_handoff = true;
            i += 1;
        } else if a == "--codex" || a == "--claude" {
            // Consumed at the top level into `explicit_agent`; skip here.
            i += 1;
        } else {
            pos.push(a);
            i += 1;
        }
    }
    let pos: &[&str] = if pos.first() == Some(&"init") { &pos[1..] } else { &pos };
    let mut n_arg: Option<usize> = None;
    let mut project: Option<&str> = None;
    for a in pos {
        match a.parse::<usize>() {
            Ok(n) => n_arg = Some(n),
            Err(_) => project = Some(a),
        }
    }
    if n_arg == Some(0) {
        eprintln!("sauron workspace: agent count must be a positive integer");
        std::process::exit(1);
    }

    // Resolve which project to open. Explicit arg (path or alias) wins, else the
    // `default` alias, else $WORKSPACE_REPO, else the git repo of the cwd.
    let repo = match project {
        Some(p) => match resolve(p) {
            Some(r) => r,
            None => {
                eprintln!("sauron workspace: '{p}' is not a directory or a saved alias.");
                eprintln!("  saved aliases:");
                alias_list(true);
                std::process::exit(1);
            }
        },
        None => default_repo(),
    };
    if !repo.is_dir() {
        eprintln!("sauron workspace: repo not found: {}", repo.display());
        eprintln!("  pass a path or alias:  sauron workspace [N] <project>");
        eprintln!("  or save a default:     sauron workspace alias default /path/to/repo");
        std::process::exit(1);
    }

    // Which agent's sessions to reopen and spawn: the flag, else $SAURON_AGENT,
    // else auto-detect from this repo's logs.
    let agent = Agent::select(explicit_agent, &repo);

    // Mordor targets Claude Code, which reaches a local model through Ollama's
    // Anthropic-compatible API. Codex's local path is `codex --oss`, a different
    // wiring not plumbed here -- so refuse rather than silently launch Codex
    // against the hosted API under a flag that promised local.
    if mordor.is_some() && agent != Agent::Claude {
        eprintln!(
            "sauron workspace: --mordor (local models) currently targets Claude Code via Ollama's Anthropic-compatible API."
        );
        eprintln!(
            "  for {}, run its panes with `{} --oss` instead. Ignoring --mordor.",
            agent.label(),
            agent.label()
        );
        mordor = None;
    }

    // Does this repo have an application to dock? Almost none do, and the ones
    // that don't never touch the GUI layout at all. `--gui` without a command
    // still needs the conf for the rest of its settings, so an inline command is
    // the only way to dock a repo that has declared nothing.
    let forced = matches!(gui_flag, Some(None));
    #[cfg(unix)]
    let gui: Option<Gui> = if no_gui {
        None
    } else {
        match gui_flag {
            Some(Some(cmd)) => Some(Gui {
                cmd,
                ..Gui::default()
            }),
            Some(None) | None => crate::gui::config(&repo),
        }
    };
    #[cfg(unix)]
    if gui.is_none() && forced {
        eprintln!(
            "sauron workspace: --gui, but {} declares nothing to launch.",
            repo.join(crate::gui::CONF).display()
        );
        eprintln!("  pass the command inline instead:  --gui='./run.sh'");
        std::process::exit(2);
    }
    // The docked-window layout needs a window server this platform does not
    // expose (see `plat`). A repo that declares a GUI still opens -- it opens the
    // ordinary layout, and is told why, rather than opening a four-column layout
    // with a permanent hole in the middle of it. There is deliberately no `gui`
    // binding on this path: the type belongs to the module that was compiled
    // out, and the three places that read it are cut with it.
    #[cfg(not(unix))]
    {
        let _ = no_gui;
        if gui_flag.is_some() || forced {
            eprintln!("{}", crate::plat::unsupported("the docked-window layout"));
            eprintln!("  opening the ordinary layout instead.");
        }
    }

    let work = crate::in_flight_tasks(repo.clone(), agent);
    // Pane count: explicit arg wins; else one per in-flight task; else 4 bare.
    let default_panes = n_arg.unwrap_or(if work.is_empty() { 4 } else { work.len() });

    // The panes run this very sauron binary for the TUI, by its real path, so a
    // restored iTerm session keeps resolving it.
    let sauron_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sauron"));
    let repo_s = repo.to_string_lossy().into_owned();

    // A quick confirmation of what's about to open -- unless a dry run, `--yes`,
    // or a non-interactive stdin. The dialogue can adjust the pane and orc counts.
    let dry = std::env::var_os("WORKSPACE_DRYRUN").is_some();
    let (total, orcs) = if dry || yes || !std::io::stdin().is_terminal() {
        (default_panes, orcs)
    } else {
        match confirm(&repo_s, agent.label(), mordor.as_ref(), default_panes, orcs) {
            Some(v) => v,
            None => {
                println!("sauron workspace: cancelled — nothing opened.");
                return Ok(());
            }
        }
    };
    let total = total.max(1); // always at least the one agent pane

    // Assign each orc a cold target: the largest source files no active session
    // is touching. Fewer safe targets than asked -> fewer orcs (nothing else is
    // safe to hand out without risking a collision with a hobbit).
    let orc_targets = if orcs > 0 {
        let hot = crate::hot_files(repo.clone(), agent);
        let targets = cold_targets(&repo, &hot, orcs);
        if targets.len() < orcs {
            eprintln!(
                "sauron workspace: only {} cold file(s) safe for orcs (asked {})",
                targets.len(),
                orcs
            );
        }
        targets
    } else {
        Vec::new()
    };
    let orc_cmds: Vec<String> = orc_targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            if clipboard_handoff {
                let key = crate::handoff::handoff_key(&repo, &format!("orc.{index}"));
                crate::handoff::workspace_command(
                    &repo,
                    &sauron_exe,
                    agent,
                    &key,
                    None,
                    // The strict-lifecycle wrapper takes the charge as text, and
                    // that text ends up inside an AppleScript string literal --
                    // so this one path gets the flattened form.
                    Some(&crate::orc::brief_oneline(
                        target,
                        &crate::orc::detect(&repo, target),
                    )),
                    true,
                )
            } else {
                orc_command(&repo_s, &sauron_exe, target, agent, mordor.as_ref())
            }
        })
        .collect();

    // Dry run: report the plan and stop, before touching iTerm (used by tests
    // and handy for "what would `sauron workspace X` actually open?").
    if dry {
        println!("REPO={repo_s}");
        println!("AGENT={}", agent.label());
        if let Some(m) = &mordor {
            println!("MORDOR={}@{}", m.model, m.base_url);
        }
        println!("TOTAL={total}");
        println!("CLIPBOARD_HANDOFF={clipboard_handoff}");
        if clipboard_handoff {
            println!(
                "CLIPBOARD_DB={}",
                crate::clip::store::db_path_from(&repo).display()
            );
        }
        println!("SAURON={}", sauron_exe.display());
        #[cfg(unix)]
        if let Some(g) = &gui {
            println!("GUI={}", g.cmd);
            println!("GUI_KEEP={:?}", g.keep);
        }
        for t in &orc_targets {
            println!("ORC={t}");
        }
        return Ok(());
    }

    // The layout is the one genuinely per-platform thing left in this function.
    // Everything above -- which repo, which agent, how many panes, which files
    // the orcs get -- is policy and is decided identically everywhere; only the
    // act of carving a window into panes differs, and it differs completely.
    #[cfg(unix)]
    {
        let script = match &gui {
            Some(_) => gui_applescript(
                &repo_s,
                &sauron_exe.to_string_lossy(),
                total,
                &work,
                &orc_cmds,
                agent,
                mordor.as_ref(),
                clipboard_handoff,
                &crate::gui::stage_command(&sauron_exe, &repo_s),
            ),
            None => applescript(
                &repo_s,
                &sauron_exe.to_string_lossy(),
                total,
                &work,
                &orc_cmds,
                agent,
                mordor.as_ref(),
                clipboard_handoff,
            ),
        };
        // The layout is a program in another language, and a unit test on the
        // string it produces is not evidence that iTerm2 accepts it. This prints
        // the script instead of running it, so the real thing can be executed and
        // the resulting window measured.
        if std::env::var_os("WORKSPACE_PRINT_SCRIPT").is_some() {
            print!("{script}");
            return Ok(());
        }
        osascript(&script)?;
    }
    #[cfg(not(unix))]
    {
        let argv = wt_layout_argv(
            &repo_s,
            &sauron_exe.to_string_lossy(),
            total,
            &work,
            &orc_cmds,
            agent,
            mordor.as_ref(),
            clipboard_handoff,
        );
        // Same escape hatch, same reason: the argv is checked by unit tests, and
        // a unit test on an argv is not evidence that Windows Terminal accepts
        // it. Printed one element per line, because `wt`'s own `;` separators are
        // argv elements and a single joined line would hide where they fall.
        if std::env::var_os("WORKSPACE_PRINT_SCRIPT").is_some() {
            for a in &argv {
                println!("{a}");
            }
            return Ok(());
        }
        crate::plat::run_wt_layout(&argv)?;
    }

    let resumed = total.min(work.len());
    let orc_note = if orc_cmds.is_empty() {
        String::new()
    } else {
        format!(", {} orc(s) loosed on cold files", orc_cmds.len())
    };
    println!(
        "sauron workspace: opened {total}-pane layout on a new Space ({resumed} resumed working task(s), {} new{orc_note}) — repo: {repo_s}",
        total - resumed
    );
    if let Some(m) = &mordor {
        println!(
            "  Mordor: hobbits & orcs run the local model '{}' via Ollama ({}) — the Eye stays on the hosted API.",
            m.model, m.base_url
        );
    }
    #[cfg(unix)]
    if let Some(g) = &gui {
        println!(
            "  GUI: the middle two columns are held open for '{}' — press Enter in the app pane to launch it.",
            g.cmd
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Workspace memory: a  name<TAB>path  registry so projects launch by a short
// alias. The special alias `default` is what a bare `sauron workspace` opens.
// ---------------------------------------------------------------------------

fn store_path() -> PathBuf {
    std::env::var_os("WORKSPACE_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude").join("sauron").join("workspaces"))
}

fn store_rows() -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(store_path()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let (n, p) = l.split_once('\t')?;
            (!n.is_empty()).then(|| (n.to_string(), p.to_string()))
        })
        .collect()
}

fn alias_lookup(name: &str) -> Option<String> {
    store_rows().into_iter().find(|(n, _)| n == name).map(|(_, p)| p)
}

fn write_rows(rows: &[(String, String)]) -> std::io::Result<()> {
    let path = store_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body: String = rows.iter().map(|(n, p)| format!("{n}\t{p}\n")).collect();
    std::fs::write(path, body)
}

fn alias_set(name: &str, raw: &str) -> std::io::Result<()> {
    let expanded = expand(raw);
    let abs = std::fs::canonicalize(&expanded).unwrap_or(expanded);
    if !abs.is_dir() {
        eprintln!("sauron workspace: not a directory: {}", abs.display());
        std::process::exit(1);
    }
    let abs = abs.to_string_lossy().into_owned();
    // Upsert: drop any existing row for this name, then append.
    let mut rows: Vec<_> = store_rows().into_iter().filter(|(n, _)| n != name).collect();
    rows.push((name.to_string(), abs.clone()));
    write_rows(&rows)?;
    println!("sauron workspace: alias '{name}' -> {abs}");
    Ok(())
}

fn alias_del(name: &str) -> std::io::Result<()> {
    let rows: Vec<_> = store_rows().into_iter().filter(|(n, _)| n != name).collect();
    write_rows(&rows)?;
    println!("sauron workspace: removed alias '{name}'");
    Ok(())
}

fn alias_list(to_stderr: bool) {
    let rows = store_rows();
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("  (no workspaces saved yet — add one with: sauron workspace alias <name> <path>)\n");
    } else {
        for (n, p) in rows {
            out.push_str(&format!("  {n:<16} {p}\n"));
        }
    }
    if to_stderr {
        eprint!("{out}");
    } else {
        print!("{out}");
    }
}

/// Expand a leading `~` to the home directory.
fn expand(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix('~') {
        home().join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(raw)
    }
}

/// A project arg -> an absolute repo directory, or None.
fn resolve(project: &str) -> Option<PathBuf> {
    // Path-like (a slash, or . / .., or ~): strictly a directory.
    if project.contains('/') || project == "." || project == ".." || project.starts_with('~') {
        let p = expand(project);
        return std::fs::canonicalize(&p).ok().filter(|p| p.is_dir());
    }
    // A bare word means the alias first -- so `sauron workspace sauron` opens the
    // saved project, not a coincidental ./sauron subdir -- then a same-named dir.
    if let Some(hit) = alias_lookup(project) {
        let pb = PathBuf::from(hit);
        if pb.is_dir() {
            return Some(pb);
        }
    }
    let pb = PathBuf::from(project);
    std::fs::canonicalize(&pb).ok().filter(|p| p.is_dir())
}

fn default_repo() -> PathBuf {
    // Start from the repository you're standing in: $WORKSPACE_REPO if set, else
    // the git repo containing the cwd, else the cwd itself. (The `default` alias
    // is no longer special -- open it by name, `sauron workspace default`.)
    if let Some(r) = std::env::var_os("WORKSPACE_REPO") {
        return PathBuf::from(r);
    }
    crate::git_root().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// A quick interactive confirmation before opening a cockpit. Shows the repo and
/// agent, lets you adjust the pane and orc counts, and confirms. Returns the
/// `(panes, orcs)` to open, or `None` to cancel.
fn confirm(
    repo: &str,
    agent: &str,
    mordor: Option<&Mordor>,
    panes: usize,
    orcs: usize,
) -> Option<(usize, usize)> {
    println!();
    // Name the realm so a Mordor launch never looks like an ordinary one -- local
    // model and endpoint, right where you confirm the count.
    let realm = match mordor {
        Some(m) => format!("{agent} · Mordor: {} @ {}", m.model, m.base_url),
        None => agent.to_string(),
    };
    println!("  sauron workspace  →  {repo}   ({realm})");
    let panes = ask_count("panes", panes)?;
    let orcs = ask_count("orcs ", orcs)?;
    print!("  launch {panes} pane(s), {orcs} orc(s)? [Y/n] ");
    std::io::stdout().flush().ok();
    match read_line()?.trim().to_ascii_lowercase().as_str() {
        "n" | "no" | "q" | "cancel" => None,
        _ => Some((panes, orcs)),
    }
}

/// Prompt for a count with a default (blank accepts it, `q` cancels).
fn ask_count(label: &str, default: usize) -> Option<usize> {
    loop {
        print!("  {label} [{default}]: ");
        std::io::stdout().flush().ok();
        let line = read_line()?;
        let t = line.trim();
        if t.is_empty() {
            return Some(default);
        }
        if matches!(t, "q" | "cancel") {
            return None;
        }
        match t.parse::<usize>() {
            Ok(n) => return Some(n),
            Err(_) => println!("    enter a number, or q to cancel"),
        }
    }
}

/// Read one line from stdin; `None` on EOF (Ctrl-D) or error, treated as cancel.
fn read_line() -> Option<String> {
    let mut s = String::new();
    match std::io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s),
    }
}

// ---------------------------------------------------------------------------
// iTerm layout
// ---------------------------------------------------------------------------

/// Quote a shell command for embedding in an AppleScript string list. Repo/exe
/// paths and commands are assumed double-quote-free (the shell version assumed
/// the same), so they drop straight in.
#[cfg(unix)]
fn as_list(cmds: &[String]) -> String {
    cmds.iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How much of the servant colour survives into a pane background.
///
/// The colour has to be recognisably the one underlining the name on the board
/// while leaving the pane readable, and those pull opposite ways. At full
/// strength a teal background makes an agent's output unreadable; mixed this far
/// into the dark base it is a tint you can name at a glance and never notice
/// while working. Measured by eye against iTerm2's default dark profile.
#[cfg(unix)]
const TINT: f32 = 0.16;

/// What the tint is mixed *into*: the near-black the panes sit on, and the same
/// base the browser front end mixes over, so a pane and a web panel running the
/// same servant are the same shade rather than two guesses at one.
#[cfg(unix)]
const TINT_BASE: (u8, u8, u8) = (10, 12, 16);

/// One pane's background: the servant colour mixed `TINT` of the way from the
/// dark base towards it.
///
/// Mixing rather than scaling is the whole of this function. `colour * 0.16`
/// walks towards *black*, so every servant arrives at the same near-black and
/// the pane looks untinted -- which is what it did. Interpolating from the base
/// keeps the hue at low brightness, which is the only thing that makes two panes
/// tellable apart.
#[cfg(unix)]
fn tinted(c: (u8, u8, u8)) -> (u8, u8, u8) {
    let mix = |c: u8, base: u8| (base as f32 + (c as f32 - base as f32) * TINT) as u8;
    (
        mix(c.0, TINT_BASE.0),
        mix(c.1, TINT_BASE.1),
        mix(c.2, TINT_BASE.2),
    )
}

/// One colour as an AppleScript RGB triple. iTerm2's colour components are
/// 16-bit, not 8-bit; passing 0-255 here yields a pane so near black that the
/// whole feature looks broken rather than subtle.
#[cfg(unix)]
fn as_rgb(c: (u8, u8, u8)) -> String {
    let wide = |c: u8| c as u32 * 257;
    format!("{{{}, {}, {}}}", wide(c.0), wide(c.1), wide(c.2))
}

/// The pane colours, as an AppleScript list of `{background, cursor}` pairs, one
/// per command.
///
/// Two colours and not one, because a background dark enough to read against is
/// too dark to *identify*, and the cursor is the one glyph on screen that can
/// carry the colour at full strength without touching the agent's own output.
/// The browser panels do exactly this (`sauron_web.html`, `fn theme`), so a pane
/// and a panel are recognisably the same servant.
///
/// A pane whose command carries no session id keeps the profile's own colours
/// (`missing value`), because there is no honest colour for it: the board cannot
/// colour that row either, and inventing one here would put a colour on screen
/// that nothing on the board agrees with.
#[cfg(unix)]
fn as_tint_list(cmds: &[String], agent: Agent) -> String {
    pane_session_ids(cmds, agent)
        .into_iter()
        .map(|id| match id {
            Some(id) => {
                let c = crate::servant::color_for(&id);
                format!("{{{}, {}}}", as_rgb(tinted(c)), as_rgb(c))
            }
            None => "missing value".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the AppleScript that opens the window, fullscreens it onto its own
/// Space, and lays out the panes: the left column is one hobbit pane per
/// `total`, and the right column is sauron on top with the orcs stacked beneath
/// it (plus a shell). Both columns split the currently-tallest pane each step so
/// they stay balanced rather than shrinking geometrically.
#[cfg(unix)]
fn applescript(
    repo: &str,
    sauron_exe: &str,
    total: usize,
    work: &[(String, String)],
    orc_cmds: &[String],
    agent: Agent,
    mordor: Option<&Mordor>,
    clipboard_handoff: bool,
) -> String {
    let left = left_commands(repo, sauron_exe, total, work, agent, mordor, clipboard_handoff);
    let sauron_flag = agent.label(); // watcher pane matches the chosen agent

    // Right column beneath sauron: the orcs, then a shell. With no orcs, two
    // plain shells, as before.
    let right: Vec<String> = if orc_cmds.is_empty() {
        vec![format!("cd {repo}"), format!("cd {repo}")]
    } else {
        let mut v = orc_cmds.to_vec();
        v.push(format!("cd {repo}"));
        v
    };

    let left_list = as_list(&left);
    let right_list = as_list(&right);
    let left_tints = as_tint_list(&left, agent);
    // The orcs are the leading right-column commands. They are staged (typed but
    // not run) so you review the target and press Enter to loose each one -- they
    // never begin refactoring the moment the window opens.
    let orc_count = orc_cmds.len();

    format!(
        r#"tell application "iTerm2"
  activate
  set w to (create window with default profile)
end tell
delay 0.6

-- Native fullscreen -> own Space. Target the frontmost (just-created) window.
tell application "System Events" to tell process "iTerm2"
  set value of attribute "AXFullScreen" of window 1 to true
end tell
delay 1.5

tell application "iTerm2"
  set t to current tab of w
  set leftTop to current session of t
  set leftCmds to {{{left_list}}}
  set leftTints to {{{left_tints}}}
  set rightCmds to {{{right_list}}}

  -- Carve the right column off the left; sauron on top, the rest stacked below.
  -- Split the CURRENTLY-TALLEST pane in a column each step (not the newest), so
  -- panes stay balanced -- repeatedly splitting the newest drives it below
  -- iTerm2's minimum height, which throws and aborts the remaining splits.
  tell leftTop to set rTop to (split vertically with default profile)
  tell rTop to write text "cd {repo} && {sauron_exe} --{sauron_flag}"
  set rightPanes to {{rTop}}
  set orcCount to {orc_count}
  repeat with i from 1 to (count of rightCmds)
    set tallest to item 1 of rightPanes
    repeat with p in rightPanes
      if (rows of p) > (rows of tallest) then set tallest to contents of p
    end repeat
    tell tallest to set newP to (split horizontally with default profile)
    -- Orcs (the first orcCount) are staged, not run: typed in, awaiting Enter.
    if i is less than or equal to orcCount then
      tell newP to write text (item i of rightCmds) newline no
    else
      tell newP to write text (item i of rightCmds)
    end if
    set end of rightPanes to newP
  end repeat

  -- Left column: one pane per hobbit command.
  --
  -- Each pane is tinted with its servant's colour -- the same colour the board
  -- underlines that session's name with, computed from the session id by both
  -- sides rather than agreed between them. That is what lets you look at a row
  -- and know which window it is without reading anything.
  tell leftTop to write text (item 1 of leftCmds)
  my tint(leftTop, item 1 of leftTints)
  set leftPanes to {{leftTop}}
  repeat with i from 2 to (count of leftCmds)
    set tallest to item 1 of leftPanes
    repeat with p in leftPanes
      if (rows of p) > (rows of tallest) then set tallest to contents of p
    end repeat
    tell tallest to set newP to (split horizontally with default profile)
    tell newP to write text (item i of leftCmds)
    my tint(newP, item i of leftTints)
    set end of leftPanes to newP
  end repeat

  -- Land focus on the first agent pane.
  select leftTop
end tell

{TINT_HANDLER}
"#
    )
}

/// Tint one pane, or leave the profile alone when there is no colour for it.
///
/// A handler rather than an inline `set`, because it is called from two loops in
/// two layouts and the `missing value` guard has to be in all four places. Each
/// `set` is wrapped in its own `try` for the same reason every other iTerm call
/// in this file is, and separately: a profile that refuses the cursor colour must
/// still get the background, and neither may take the whole layout down with it
/// having already opened half the window.
#[cfg(unix)]
const TINT_HANDLER: &str = r#"on tint(sess, c)
  if c is missing value then return
  try
    tell application "iTerm2" to tell sess to set background color to (item 1 of c)
  end try
  try
    tell application "iTerm2" to tell sess to set cursor color to (item 2 of c)
  end try
end tint"#;

/// The agent column's commands: resume each in-flight session, a fresh agent for
/// the rest. In Mordor mode each hobbit carries the local-model env before the
/// agent word; the orcs already carry theirs (built in `orc_command`), and the
/// sauron watcher pane deliberately does not -- the Eye calls no model.
///
/// Shared by both layouts, because the *column* is the same thing in each; only
/// the geometry around it differs.
/// The servants, in the order the agent column is filled.
///
/// A name per pane, not a number, because the point is telling them apart at a
/// glance and `frodo` reads at a glance where `agent-4` does not. The roster is
/// the repo's own cast (see the README) rather than an invention: the panes have
/// been "hobbits" in the prose since before they had names.
///
/// Order is fixed and slot-indexed, so the pane in a given position is called
/// the same thing every launch -- a name that moved between runs would be worse
/// than a number.
const HOBBITS: &[&str] = &[
    "frodo", "sam", "merry", "pippin", "gandalf", "aragorn", "legolas", "gimli", "boromir",
    "bilbo", "faramir", "eowyn", "theoden", "treebeard", "elrond", "galadriel", "radagast",
    "beregond",
];

/// The orcs, kept apart from the hobbits so a glance at a name says which kind
/// of servant a pane holds as well as which one.
const ORCS: &[&str] = &[
    "grishnakh", "ugluk", "shagrat", "gorbag", "lugdush", "muzgash", "snaga", "mauhur", "azog",
    "bolg",
];

/// Slot index -> the name that pane's session carries.
///
/// Past the end of a roster the names repeat with a company number rather than
/// wrapping silently onto a duplicate -- two panes both called `frodo` would
/// defeat the whole purpose.
fn servant_name(roster: &[&str], index: usize) -> String {
    let name = roster[index % roster.len()];
    match index / roster.len() {
        0 => name.to_string(),
        n => format!("{name}-{}", n + 1),
    }
}

/// The name the agent pane in slot `index` is given.
pub fn hobbit_name(index: usize) -> String {
    servant_name(HOBBITS, index)
}

/// The name the orc pane in slot `index` is given.
pub fn orc_name(index: usize) -> String {
    servant_name(ORCS, index)
}

fn left_commands(
    repo: &str,
    sauron_exe: &str,
    total: usize,
    work: &[(String, String)],
    agent: Agent,
    mordor: Option<&Mordor>,
    clipboard_handoff: bool,
) -> Vec<String> {
    let env = agent.local_env(mordor);
    (0..total)
        .map(|i| match work.get(i) {
            Some((id, _)) if clipboard_handoff => {
                let key = crate::handoff::handoff_key(Path::new(repo), &format!("slot.{i}"));
                crate::handoff::workspace_command(
                    Path::new(repo),
                    Path::new(sauron_exe),
                    agent,
                    &key,
                    Some(id),
                    None,
                    false,
                )
            }
            None if clipboard_handoff => {
                let key = crate::handoff::handoff_key(Path::new(repo), &format!("slot.{i}"));
                crate::handoff::workspace_command(
                    Path::new(repo),
                    Path::new(sauron_exe),
                    agent,
                    &key,
                    None,
                    None,
                    false,
                )
            }
            // The name rides on the end of the agent's own command, which is why
            // it survives Mordor mode and resume alike: both are still the agent
            // word plus flags, and this adds one more.
            Some((id, _)) => format!(
                "cd {repo} && {env}{}{}",
                agent.resume_cmd(id),
                agent.name_flag(crate::servant::name_for(id))
            ),
            // A fresh pane is given its session id rather than left to invent
            // one, so its colour is knowable now instead of one tick after the
            // agent first writes a log. Without this the newest pane -- the one
            // you are most likely to be looking for -- is the only one the board
            // cannot colour.
            None => {
                let id = crate::servant::mint_session_id();
                format!(
                    "cd {repo} && {env}{}{}",
                    agent.fresh_cmd(&id),
                    agent.name_flag(crate::servant::name_for(&id))
                )
            }
        })
        .collect()
}

/// The panes' session ids, in column order, so the launcher can colour each pane
/// to match the row the board will draw for it.
///
/// Recomputed from `left_commands`' own output rather than threaded alongside
/// it: two lists that must stay in step are one bug away from being wrong, and
/// the id is already in the command text.
/// The markers are the agent's own (`Agent::id_markers`), because the command
/// this reads was built from that agent's launch forms. Reading Claude's flags
/// out of a Codex pane found nothing, which is why Codex panes were the grey
/// ones.
#[cfg_attr(not(unix), allow(dead_code))]
fn pane_session_ids(commands: &[String], agent: Agent) -> Vec<Option<String>> {
    commands
        .iter()
        .map(|cmd| {
            let (at, marker) = agent
                .id_markers()
                .iter()
                .find_map(|m| cmd.find(m).map(|at| (at, *m)))?;
            // Past the marker, not past its first word: Codex's marker is two
            // words (`codex resume `) and Claude's is one.
            cmd[at + marker.len()..]
                .split_whitespace()
                .next()
                .map(|id| id.to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The Windows Terminal layout.
//
// Same two columns as the iTerm layout -- agents on the left, the Eye and its
// shells on the right -- reached by a completely different route, because `wt`
// can only ever split *the pane that currently has focus*.
//
// That forces the build order. The right column is carved off FIRST, as a single
// vertical split of the one pane the window opens with, so that it runs the full
// height. Subdividing it afterwards keeps it that way. Doing it the other way
// round -- growing the agent column and then splitting for the Eye -- would give
// a right column only as tall as whichever agent pane happened to be focused.
//
// Pane indices come out of that order, and `focus-pane --target` is the only way
// to hand focus to a specific pane later, so sauron's index is a fact the layout
// knows and the panes are told (`SAURON_WT_PANE`). Close a pane by hand and the
// indices behind it shift; nothing here can observe that.
//
// Compiled on every platform, called only on Windows, so that the tests below
// run on the machine doing the porting rather than only on the target.
// ---------------------------------------------------------------------------

/// The `wt.exe` argv that opens the workspace. `;` elements are `wt`'s own
/// subcommand separators and must stay separate argv entries.
#[cfg_attr(unix, allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn wt_layout_argv(
    repo: &str,
    sauron_exe: &str,
    total: usize,
    work: &[(String, String)],
    orc_cmds: &[String],
    agent: Agent,
    mordor: Option<&Mordor>,
    clipboard_handoff: bool,
) -> Vec<String> {
    let lefts = left_commands(repo, sauron_exe, total, work, agent, mordor, clipboard_handoff);
    let window = wt_window_name(repo);
    // Windows Terminal cannot tint one pane -- `--tabColor` colours the whole
    // tab, and `--colorScheme` needs a scheme already defined in the user's
    // settings.json, which sauron has no business writing. So the servant is
    // carried by the pane *title* here rather than by a colour, and the board's
    // underline is the only place the colour appears on this platform.
    let titles: Vec<String> = pane_session_ids(&lefts, agent)
        .into_iter()
        .enumerate()
        .map(|(i, id)| match id {
            Some(id) => crate::servant::name_for(&id).to_string(),
            None => format!("agent-{}", i + 1),
        })
        .collect();

    // Pane 0 is the first agent pane; pane 1 is the Eye. Both are consequences of
    // the build order above, and `SAURON_WT_PANE` below hands the second one to
    // the TUI so a later spawn can give focus back.
    const SAURON_PANE: u32 = 1;

    let mut argv = vec![
        "-w".to_string(),
        window.clone(),
        "--fullscreen".to_string(),
        // The agent column starts as the whole window.
        "new-tab".to_string(),
        "--title".to_string(),
        titles[0].clone(),
        "-d".to_string(),
        repo.to_string(),
        "--".to_string(),
    ];
    argv.extend(pane_shell(&lefts[0], &window, SAURON_PANE));

    // Carve the Eye's column off the whole height, before anything subdivides.
    push_split(
        &mut argv,
        "--vertical",
        repo,
        "sauron",
        pane_shell(&format!("{sauron_exe} {repo}"), &window, SAURON_PANE),
    );

    // The rest of the right column, beneath the Eye: the orcs, then two shells --
    // the same contents and the same order as the iTerm layout puts there.
    for cmd in orc_cmds {
        push_split(
            &mut argv,
            "--horizontal",
            repo,
            "orc",
            pane_shell(cmd, &window, SAURON_PANE),
        );
    }
    for _ in 0..2 {
        push_split(
            &mut argv,
            "--horizontal",
            repo,
            "shell",
            vec![crate::plat::shell_exe()],
        );
    }

    // Back to the agent column for the remaining hobbits. `move-focus first`
    // returns to pane 0, which the build order guarantees is in that column.
    for (i, cmd) in lefts.iter().enumerate().skip(1) {
        argv.push(";".to_string());
        argv.push("move-focus".to_string());
        argv.push("first".to_string());
        push_split(
            &mut argv,
            "--horizontal",
            repo,
            &titles[i],
            pane_shell(cmd, &window, SAURON_PANE),
        );
    }

    // Leave the user looking at the Eye, which is where the iTerm layout leaves
    // them too.
    argv.push(";".to_string());
    argv.push("focus-pane".to_string());
    argv.push("--target".to_string());
    argv.push(SAURON_PANE.to_string());
    argv
}

/// Append one `; split-pane <axis> -d <repo> --title <title> -- <cmd...>`.
#[cfg_attr(unix, allow(dead_code))]
fn push_split(argv: &mut Vec<String>, axis: &str, repo: &str, title: &str, cmd: Vec<String>) {
    argv.push(";".to_string());
    argv.push("split-pane".to_string());
    argv.push(axis.to_string());
    argv.push("-d".to_string());
    argv.push(repo.to_string());
    argv.push("--title".to_string());
    argv.push(title.to_string());
    argv.push("--".to_string());
    argv.extend(cmd);
}

/// A pane's commandline: a PowerShell that publishes which window and which pane
/// this workspace is, then runs the pane's own command.
///
/// The two env vars are the whole of what replaces `$ITERM_SESSION_ID`. iTerm
/// tells a process which session it is running in; `wt` tells it nothing, so the
/// launch that *knows* has to say so here, and a sauron started by hand outside a
/// workspace correctly finds neither.
#[cfg_attr(unix, allow(dead_code))]
fn pane_shell(cmd: &str, window: &str, sauron_pane: u32) -> Vec<String> {
    let prelude = format!(
        "$env:SAURON_WT_WINDOW='{}'; $env:SAURON_WT_PANE='{sauron_pane}'; ",
        window.replace('\'', "''"),
    );
    vec![
        crate::plat::shell_exe(),
        "-NoExit".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        format!("{prelude}{}", crate::plat::to_powershell(cmd)),
    ]
}

/// The `wt` window name this repo's workspace owns. Named rather than `0` (the
/// most-recently-used window) so that two workspaces open at once each grow
/// themselves instead of racing for whichever the user last touched.
#[cfg_attr(unix, allow(dead_code))]
fn wt_window_name(repo: &str) -> String {
    let leaf: String = repo
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("repo")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    format!("sauron-{leaf}")
}

/// The layout for a repo that has an application of its own: three equal
/// columns -- agents, the application, the Eye -- with the middle one held open
/// and the app's own window docked into it by `sauron gui`.
///
/// Three things here are forced by iTerm2 rather than chosen:
///
/// **No native fullscreen.** A fullscreen Space holds exactly one window, so a
/// natively-fullscreened workspace could never share a screen with the
/// application. `set zoomed to true` fills the display the window opened on
/// instead -- and it needs no screen arithmetic, which matters because
/// `bounds of window of desktop` spans *all* displays on a multi-monitor Mac.
///
/// **Thirds, from equal siblings.** A divider cannot be placed: `set columns` on
/// a split pane is accepted and does nothing, and iTerm2's windows expose no
/// `AXSplitGroup`. What iTerm2 *does* do is redistribute a splitter evenly every
/// time a pane joins it, so N splits off one splitter give N+1 equal panes --
/// measured, not assumed: three vertical splits produced four 67-column panes in
/// a 271-column window. Two splits give the thirds this layout wants, and the
/// middle column's two splits give a hole two thirds tall over a log strip.
///
/// **Tagged panes.** The panes behind the app still exist and still sort ahead
/// of the Eye, so the TUI's `n` / `Enter` / `O` would happily split one and hide
/// an agent behind a game. Each is marked with a session variable, which --
/// unlike a pane title -- no program's escape sequences can overwrite.
#[allow(clippy::too_many_arguments)]
#[cfg(unix)]
fn gui_applescript(
    repo: &str,
    sauron_exe: &str,
    total: usize,
    work: &[(String, String)],
    orc_cmds: &[String],
    agent: Agent,
    mordor: Option<&Mordor>,
    clipboard_handoff: bool,
    gui_cmd: &str,
) -> String {
    let left = left_commands(repo, sauron_exe, total, work, agent, mordor, clipboard_handoff);
    let left_list = as_list(&left);
    let left_tints = as_tint_list(&left, agent);
    let orc_list = as_list(orc_cmds);
    let sauron_flag = agent.label();

    // Set the frame outright when AppKit will tell us what it is. `zoomed` is
    // the fallback and not the default because it keeps the window's origin and
    // only grows to the screen edge -- measured at 1462 points wide on a 1512
    // point display, with the left column hanging off the side.
    let fill = match crate::gui::visible_frame() {
        Some(f) => format!(
            "set bounds of w to {{{}, {}, {}, {}}}",
            f.x,
            f.y,
            f.x + f.w,
            f.y + f.h
        ),
        None => "set zoomed of w to true".to_string(),
    };

    format!(
        r#"tell application "iTerm2"
  activate
  set w to (create window with default profile)
end tell
delay 0.6

-- Fill the display this window opened on, rather than going native-fullscreen:
-- a fullscreen Space accepts exactly one window, and this layout has to share
-- the screen with the application it is holding a hole open for.
tell application "iTerm2"
  {fill}
end tell
delay 0.8

tell application "iTerm2"
  set t to current tab of w
  set agentTop to current session of t
  set leftCmds to {{{left_list}}}
  set leftTints to {{{left_tints}}}
  set orcCmds to {{{orc_list}}}

  -- Three equal columns: agents | the app's hole | the Eye. Both splits come
  -- off the same splitter, and iTerm2 re-divides a splitter evenly whenever a
  -- pane joins it, so two splits are exactly thirds.
  tell agentTop to set hole to (split vertically with default profile)
  tell hole to set eye to (split vertically with default profile)

  -- The middle column: three equal rows. The application covers the top two,
  -- and the bottom one stays visible -- that strip is where its own output goes.
  tell hole to set holeMid to (split horizontally with default profile)
  tell holeMid to set appLog to (split horizontally with default profile)

  -- Mark every pane the application sits over. A session variable survives what
  -- a pane title does not -- any program can rewrite its title with an escape
  -- sequence -- and this mark is what lets the TUI's pane-spawning keys skip
  -- these panes and still find the agent column.
  set guiPanes to {{hole, holeMid, appLog}}
  repeat with k from 1 to (count of guiPanes)
    tell (item k of guiPanes)
      set variable named "user.sauron_role" to "gui"
    end tell
  end repeat

  -- The app's launcher is STAGED, not run: `run.sh` is a release build, and a
  -- window opening is not consent to start one. Press Enter to loose it.
  tell appLog to write text "{gui_cmd}" newline no

  -- The Eye, full height in the last quarter, with any orcs stacked under it.
  tell eye to write text "cd {repo} && {sauron_exe} --{sauron_flag}"
  set eyePanes to {{eye}}
  repeat with i from 1 to (count of orcCmds)
    set tallest to item 1 of eyePanes
    repeat with k from 1 to (count of eyePanes)
      if (rows of (item k of eyePanes)) > (rows of tallest) then set tallest to (item k of eyePanes)
    end repeat
    tell tallest to set newP to (split horizontally with default profile)
    tell newP to write text (item i of orcCmds) newline no
    set end of eyePanes to newP
  end repeat

  -- Agent column: one pane per hobbit command, splitting the tallest each step
  -- so the column stays even instead of halving the newest pane every time.
  tell agentTop to write text (item 1 of leftCmds)
  my tint(agentTop, item 1 of leftTints)
  set leftPanes to {{agentTop}}
  repeat with i from 2 to (count of leftCmds)
    set tallest to item 1 of leftPanes
    repeat with k from 1 to (count of leftPanes)
      if (rows of (item k of leftPanes)) > (rows of tallest) then set tallest to (item k of leftPanes)
    end repeat
    tell tallest to set newP to (split horizontally with default profile)
    tell newP to write text (item i of leftCmds)
    my tint(newP, item i of leftTints)
    set end of leftPanes to newP
  end repeat

  select agentTop
end tell

{TINT_HANDLER}
"#
    )
}

// ---------------------------------------------------------------------------
// Cold-code detection: the safe, uncontested files an orc can be handed.
// ---------------------------------------------------------------------------

/// The best single-shot targets, best first: source files no active session is
/// touching and no uncommitted change has dirtied. Ranking and the cold/hot
/// filtering both live in `orc`, so the launcher and the TUI picker choose from
/// exactly the same list by exactly the same rule.
fn cold_targets(repo: &Path, hot: &BTreeSet<String>, want: usize) -> Vec<String> {
    if want == 0 {
        return Vec::new();
    }
    crate::orc::survey(repo, hot)
        .cold
        .into_iter()
        .take(want)
        .map(|t| t.path)
        .collect()
}

/// The line an orc pane is handed: `cd repo && sauron orc <file>`. It used to be
/// `claude '<the entire brief>'`, which forced the brief to stay single-line and
/// quote-free and buried the target in a wall of prose the user was supposed to
/// review before pressing Enter. The charge now lives in `orc::brief` and is
/// passed to the agent as an argv element by `sauron orc`.
///
/// In Mordor mode the env prefix rides ahead of the sauron word; the vars are
/// inherited straight through to the agent that subcommand execs.
fn orc_command(
    repo: &str,
    sauron_exe: &Path,
    target: &str,
    agent: Agent,
    mordor: Option<&Mordor>,
) -> String {
    crate::orc::stage_command(sauron_exe, repo, target, &agent.local_env(mordor))
}

// ---------------------------------------------------------------------------
// Growing the left column from inside the running TUI.
//
// Closing an agent pane is one keystroke of iTerm2's; opening one back up was a
// split, a cd, and a typed command. `spawn_left_pane` is the other half of that
// gesture, driven from sauron itself.
//
// Finding the left column needs no bookkeeping, because iTerm2 enumerates
// `sessions of tab` in split-tree order and `applescript` above carves the right
// column off as the *second* child of the root vertical split. So every
// left-column pane sorts before the sauron pane and every right-column pane
// (sauron, the orcs, the shells) sorts after it. sauron knows which session is
// its own from $ITERM_SESSION_ID, so "the agent column" is exactly "everything
// ahead of me", recomputed live -- panes you closed are simply not there any
// more, and panes you split by hand are.
// ---------------------------------------------------------------------------

/// Open one more agent pane in the left column of the workspace window this
/// process is running in, running `cmd`, and keep the column balanced by
/// splitting whichever left pane is currently tallest -- the same rule the
/// launch layout uses, and for the same reason (repeatedly splitting the newest
/// pane drives it under iTerm2's minimum height and the split throws).
///
/// `focus` selects the new pane. Off for a bare spawn, so repeated presses all
/// land in sauron; on when the user is opening a specific session to talk to it.
///
/// Returns the message to show on failure -- this runs inside the TUI event
/// loop, so nothing here may print or exit.
pub fn spawn_left_pane(cmd: &str, focus: bool) -> Result<(), String> {
    // `wt` cannot be asked which pane is tallest, or which are ahead of this
    // one, so the Windows path splits the column's first pane instead. The
    // difference shows up on a crowded column, where iTerm would have found room
    // and this will be refused -- see `plat::win`'s header.
    #[cfg(not(unix))]
    return crate::plat::spawn_agent_pane(&crate::plat::to_powershell(cmd), focus);

    #[cfg(unix)]
    {
        spawn_left_pane_iterm(cmd, focus)
    }
}

#[cfg(unix)]
fn spawn_left_pane_iterm(cmd: &str, focus: bool) -> Result<(), String> {
    let Some(me) = iterm_session_id() else {
        return Err("not running in an iTerm2 pane".into());
    };
    let out = osascript_out(&spawn_script(&me, cmd, focus))
        .map_err(|e| format!("osascript failed: {e}"))?;
    match out.trim() {
        "OK" => Ok(()),
        "" => Err("iTerm2 did not answer".into()),
        other => Err(other.trim_start_matches("ERR ").to_string()),
    }
}

/// Stage an orc in the **right** column -- sauron's own column -- of the
/// workspace window this process is running in.
///
/// The mirror image of [`spawn_left_pane`], and it differs in the two ways that
/// matter. It splits the sessions *at or after* sauron rather than those ahead
/// of it, because the launch layout carves the right column off as the second
/// child of the root split, so the orcs live behind the Eye and the hobbits in
/// front of it. And it types the command **without** pressing Enter: an orc is
/// staged, never auto-run, so the target can be read and approved first. That is
/// the same contract `--orcs N` has at launch, kept identical here so a
/// GUI-dispatched orc is not a more dangerous thing than a launch-dispatched one.
pub fn spawn_orc_pane(cmd: &str) -> Result<(), String> {
    // Staged, not run, on both platforms -- reached differently. iTerm types the
    // line and withholds the Enter; `wt` cannot type into a pane at all, so the
    // pane opens with the command on the history stack and a banner saying so.
    // One keystroke either way, and neither runs by itself.
    #[cfg(not(unix))]
    return crate::plat::spawn_orc_pane(&crate::plat::to_powershell(cmd));

    #[cfg(unix)]
    {
        spawn_orc_pane_iterm(cmd)
    }
}

#[cfg(unix)]
fn spawn_orc_pane_iterm(cmd: &str) -> Result<(), String> {
    let Some(me) = iterm_session_id() else {
        return Err("not running in an iTerm2 pane".into());
    };
    let out = osascript_out(&orc_spawn_script(&me, cmd))
        .map_err(|e| format!("osascript failed: {e}"))?;
    match out.trim() {
        "OK" => Ok(()),
        "" => Err("iTerm2 did not answer".into()),
        other => Err(other.trim_start_matches("ERR ").to_string()),
    }
}

/// The right-column split script. Answers `OK`, or `ERR <why>`.
#[cfg(unix)]
fn orc_spawn_script(session_uuid: &str, cmd: &str) -> String {
    let me = as_str_literal(session_uuid);
    let cmd = as_str_literal(cmd);
    format!(
        r#"tell application "iTerm2"
  -- Walk to the tab holding this pane and keep this session and everything
  -- behind it: that is exactly sauron's column. Indexed with `item k of`
  -- throughout, because `repeat with x in` hands back a reference and
  -- `contents of` a session reference reads its visible TEXT, not the object.
  set rights to {{}}
  set found to false
  repeat with wi from 1 to (count of windows)
    set ts to tabs of (item wi of windows)
    repeat with ti from 1 to (count of ts)
      set ss to sessions of (item ti of ts)
      set idx to 0
      repeat with k from 1 to (count of ss)
        if (id of (item k of ss)) is "{me}" then
          set idx to k
          exit repeat
        end if
      end repeat
      if idx > 0 then
        set found to true
        repeat with k from idx to (count of ss)
          -- Same exclusion as the agent column: an orc staged into a pane the
          -- application covers would be doing its work invisibly.
          if not (my isGuiPane(item k of ss)) then set end of rights to (item k of ss)
        end repeat
        exit repeat
      end if
    end repeat
    if found then exit repeat
  end repeat
  if not found then return "ERR this pane is not in an iTerm2 window"

  -- Split the tallest pane in the column, not the newest: repeatedly splitting
  -- the newest drives it under iTerm2's minimum height and the split throws.
  set tallest to item 1 of rights
  repeat with k from 1 to (count of rights)
    if (rows of (item k of rights)) > (rows of tallest) then set tallest to (item k of rights)
  end repeat
  try
    tell tallest to set newP to (split horizontally with default profile)
  on error errMsg
    return "ERR " & errMsg
  end try
  -- STAGED, not run: typed in and left awaiting Enter, so the target gets read
  -- before anything starts editing it.
  tell newP to write text "{cmd}" newline no
  select newP
end tell
return "OK"

{GUI_HANDLER}
"#
    )
}

/// This pane's session UUID. iTerm2 exports `w<win>t<tab>p<pane>:<uuid>`, and
/// the uuid tail is exactly the `id` the AppleScript session class reports.
#[cfg(unix)]
fn iterm_session_id() -> Option<String> {
    let raw = std::env::var("ITERM_SESSION_ID").ok()?;
    let uuid = raw.rsplit_once(':').map(|(_, u)| u).unwrap_or(&raw).trim();
    (!uuid.is_empty()).then(|| uuid.to_string())
}

/// Escape a Rust string into an AppleScript string literal body.
#[cfg(unix)]
fn as_str_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The split script. Answers `OK`, or `ERR <why>` -- every failure is a message
/// the footer can show, never a silent no-op.
#[cfg(unix)]
fn spawn_script(session_uuid: &str, cmd: &str, focus: bool) -> String {
    let me = as_str_literal(session_uuid);
    let cmd = as_str_literal(cmd);
    let select = if focus { "  select newP\n" } else { "" };
    format!(
        r#"tell application "iTerm2"
  -- Walk to the tab holding this pane and keep the sessions ahead of it.
  -- Everything here indexes with `item k of` rather than `repeat with x in`:
  -- the loop form hands back a reference, and `contents of` a session reference
  -- reads the session's visible TEXT (it is a real property of the class), not
  -- the object -- which is how this first tried to compare a screenful of
  -- scrollback against a pane height.
  set lefts to {{}}
  set found to false
  repeat with wi from 1 to (count of windows)
    set ts to tabs of (item wi of windows)
    repeat with ti from 1 to (count of ts)
      set ss to sessions of (item ti of ts)
      set idx to 0
      repeat with k from 1 to (count of ss)
        if (id of (item k of ss)) is "{me}" then
          set idx to k
          exit repeat
        end if
      end repeat
      if idx > 0 then
        set found to true
        repeat with k from 1 to (idx - 1)
          -- Skip the panes a docked application is sitting on top of: they are
          -- ahead of sauron in split order like the agent column is, but
          -- splitting one would file a live agent away behind a game window.
          if not (my isGuiPane(item k of ss)) then set end of lefts to (item k of ss)
        end repeat
        exit repeat
      end if
    end repeat
    if found then exit repeat
  end repeat
  if not found then return "ERR this pane is not in an iTerm2 window"
  if (count of lefts) is 0 then return "ERR no agent column left of sauron"

  -- Split the tallest left pane, not the newest, so the column stays even.
  set tallest to item 1 of lefts
  repeat with k from 1 to (count of lefts)
    if (rows of (item k of lefts)) > (rows of tallest) then set tallest to (item k of lefts)
  end repeat
  try
    tell tallest to set newP to (split horizontally with default profile)
  on error errMsg
    return "ERR " & errMsg
  end try
  tell newP to write text "{cmd}"
{select}end tell
return "OK"

{GUI_HANDLER}
"#
    )
}

/// The one shared AppleScript handler: is this pane part of a docked
/// application's hole?
///
/// Read from a session variable rather than the pane's name because any program
/// running in a pane can rewrite its title with an escape sequence -- and the
/// panes in question are running shells, which do exactly that on every prompt.
/// In a workspace with no GUI nothing carries the mark, so every pane answers
/// false and both scripts behave exactly as they did before.
#[cfg(unix)]
const GUI_HANDLER: &str = r#"on isGuiPane(s)
  set paneRole to ""
  try
    tell application "iTerm2"
      tell s
        set paneRole to variable named "user.sauron_role"
      end tell
    end tell
  end try
  return (paneRole is "gui")
end isGuiPane"#;

/// Pipe the AppleScript to `osascript` on stdin and hand back its stdout. Unlike
/// `osascript`, this never prints or exits -- its caller is the TUI.
#[cfg(unix)]
fn osascript_out(script: &str) -> std::io::Result<String> {
    let mut child = Command::new("osascript")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Ok(if err.is_empty() {
            "ERR osascript refused".into()
        } else {
            format!("ERR {err}")
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pipe the AppleScript to `osascript` on stdin, exactly as the shell heredoc did.
#[cfg(unix)]
fn osascript(script: &str) -> std::io::Result<()> {
    let mut child = match Command::new("osascript").stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sauron workspace: could not run osascript ({e}). macOS + iTerm2 only.");
            std::process::exit(1);
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        eprintln!("sauron workspace: osascript exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::servant;

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn spawn_script_targets_this_pane_and_splits_the_column_left_of_it() {
        let s = spawn_script("UUID-1", "cd /repo && claude", false);
        // The pane is found by its own session id, and the agent column is
        // whatever precedes it -- collected before the id match, never after.
        assert!(s.contains(r#"if (id of (item k of ss)) is "UUID-1""#));
        assert!(s.contains("no agent column left of sauron"));
        // Balanced growth: the tallest left pane is split, not the newest.
        assert!(s.contains("if (rows of (item k of lefts)) > (rows of tallest)"));
        assert!(s.contains("split horizontally with default profile"));
        assert!(s.contains(r#"write text "cd /repo && claude""#));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn spawn_script_only_steals_focus_when_asked() {
        let bare = spawn_script("UUID-1", "cd /repo && claude", false);
        let opened = spawn_script("UUID-1", "cd /repo && claude --resume abc", true);
        // A bare spawn leaves the keyboard in sauron so the key can be repeated;
        // opening a named session moves you to it, which is why you opened it.
        assert!(!bare.contains("select newP"));
        assert!(opened.contains("select newP"));
    }

    /// The safety contract of an orc, at the script level: staged, never run.
    /// If this ever writes the command with a newline, a GUI-dispatched orc
    /// starts rewriting a file the instant the pane appears, with nobody having
    /// read which file it picked.
    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn orc_spawn_script_stages_the_command_instead_of_running_it() {
        let s = orc_spawn_script("UUID-1", "cd /repo && /bin/sauron orc src/big.rs");
        assert!(
            s.contains(r#"write text "cd /repo && /bin/sauron orc src/big.rs" newline no"#),
            "an orc must be typed and left awaiting Enter: {s}"
        );
        // Focus follows, so the Enter you are about to press lands in the orc.
        assert!(s.contains("select newP"));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn orc_spawn_script_takes_sauron_own_column_not_the_hobbits() {
        let s = orc_spawn_script("UUID-1", "cd /repo && /bin/sauron orc src/big.rs");
        // Everything from this pane onward is the right column; the left-column
        // spawn walks the other way (`1 to (idx - 1)`), and mixing them up would
        // stage an orc in the middle of the hobbits.
        assert!(s.contains("repeat with k from idx to (count of ss)"));
        assert!(!s.contains("repeat with k from 1 to (idx - 1)"));
        // Same balance rule as everywhere else: split the tallest, not the newest.
        assert!(s.contains("if (rows of (item k of rights)) > (rows of tallest)"));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn spawn_script_never_dereferences_a_session_with_contents_of() {
        // `contents` is a real property of iTerm2's session class -- the visible
        // TEXT of the pane. `contents of s` on a session reference therefore
        // hands back a screenful of scrollback, not the object, and the height
        // comparison below it fails with -1728. Index form is the only safe way
        // to walk these lists, so keep the loop form out of this script.
        let s = spawn_script("UUID-1", "cd /repo && claude", true);
        let code: String = s
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains("contents of"));
        assert!(code.contains("item k of ss"));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn spawn_script_escapes_the_command_into_the_applescript_literal() {
        let s = spawn_script("UUID-1", r#"cd "/re po" && claude"#, false);
        assert!(s.contains(r#"write text "cd \"/re po\" && claude""#));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn applescript_lists_one_command_per_pane() {
        let work = vec![("id-1".to_string(), "task one".to_string())];
        let s = applescript("/repo", "/bin/sauron", 3, &work, &[], Agent::Claude, None, false);
        // First hobbit pane resumes the working task; the rest are bare claude.
        assert!(s.contains("cd /repo && claude --resume id-1"));
        assert_eq!(s.matches("cd /repo && claude").count(), 3); // resume line counts too
        // The sauron pane runs this binary against the repo, watching the agent.
        assert!(s.contains("cd /repo && /bin/sauron --claude"));
        // Every pane's session is named, so the column is eight distinguishable
        // windows rather than eight identical ones.
        let named = servant::name_for("id-1");
        assert!(s.contains(&format!(
            "set leftCmds to {{\"cd /repo && claude --resume id-1 --name {named}\", "
        )));
    }

    #[test]
    fn every_pane_gets_its_own_session_and_its_own_name() {
        // The differentiation itself: no two panes may share a session id or a
        // display name, or the terminal titles collide and the colours with them.
        let work = vec![("id-1".to_string(), "t".to_string())];
        let cmds = left_commands("/repo", "/bin/sauron", 6, &work, Agent::Claude, None, false);
        assert_eq!(cmds.len(), 6);

        let ids = pane_session_ids(&cmds, Agent::Claude);
        assert!(ids.iter().all(|i| i.is_some()), "every pane needs an id: {ids:?}");
        let unique: std::collections::BTreeSet<_> = ids.iter().flatten().collect();
        assert_eq!(unique.len(), 6, "session ids must not repeat: {ids:?}");

        // The resumed one keeps the id it was resumed from -- minting a new one
        // would start a second conversation and orphan the work being watched.
        assert_eq!(ids[0].as_deref(), Some("id-1"));

        // And each command names its session after that id, so the board and the
        // terminal title agree without either being told.
        for (cmd, id) in cmds.iter().zip(ids.iter().flatten()) {
            assert!(
                cmd.ends_with(&format!("--name {}", servant::name_for(id))),
                "{cmd}"
            );
        }
    }

    // Windows Terminal cannot tint a single pane, so there is no tint list off
    // macOS -- the servant travels as the pane title there instead.
    #[cfg(unix)]
    #[test]
    fn a_panes_tint_is_the_colour_the_board_underlines_it_with() {
        // The join this whole feature rests on: two processes, no shared state,
        // same answer. If these ever disagree the colours become noise.
        let work = vec![("id-1".to_string(), "t".to_string())];
        let cmds = left_commands("/repo", "/bin/sauron", 3, &work, Agent::Claude, None, false);
        let tints = as_tint_list(&cmds, Agent::Claude);

        for id in pane_session_ids(&cmds, Agent::Claude).iter().flatten() {
            let c = servant::color_for(id);
            assert!(
                tints.contains(&format!("{{{}, {}}}", as_rgb(tinted(c)), as_rgb(c))),
                "no tint for {id} in {tints}"
            );
        }
        // iTerm2's components are 16-bit. An 8-bit value here is a pane that
        // looks black and a feature that looks broken.
        assert!(
            !tints.contains("missing value"),
            "every pane here has an id, so every pane has a colour"
        );
    }

    // The bug this replaced: `colour * TINT` walks every servant towards black,
    // so ten distinct hues arrive as ten shades of the same near-black and no
    // pane on screen looks tinted at all.
    #[cfg(unix)]
    #[test]
    fn a_tint_keeps_the_hue_instead_of_walking_it_to_black() {
        let mut seen = std::collections::BTreeSet::new();
        for c in servant::PALETTE {
            let t = tinted(*c);
            // Dark enough to read an agent's output against.
            assert!(
                t.0 as u16 + t.1 as u16 + t.2 as u16 <= 3 * 60,
                "{c:?} tinted to {t:?}, which is too bright to work in"
            );
            // Light enough to be a colour. Every palette entry has a channel at
            // 214 or above, and scaling drove the brightest of them to 25 --
            // ten hues arriving as one near-black. Mixing floors that channel at
            // 44, so the threshold here separates the two mechanisms rather than
            // restating whichever one is compiled.
            assert!(
                t.0.max(t.1).max(t.2) >= 35,
                "{c:?} tinted to {t:?}, which is indistinguishable from black"
            );
            seen.insert(t);
        }
        assert_eq!(
            seen.len(),
            servant::PALETTE.len(),
            "two servants share a pane colour: {seen:?}"
        );
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn every_script_that_tints_a_pane_also_defines_the_handler() {
        // An undefined handler is not a colour that fails to appear -- it is an
        // AppleScript error partway through the layout, with the window already
        // open and half its panes missing. Both layouts call `my tint`, and the
        // two spawn scripts must not, so this checks the implication in both
        // directions rather than that some string is present somewhere.
        let work = vec![("id-1".to_string(), "t".to_string())];
        let gui = crate::gui::Gui {
            cmd: "./run.sh".into(),
            ..crate::gui::Gui::default()
        };
        let scripts = [
            applescript("/repo", "/bin/sauron", 2, &work, &[], Agent::Claude, None, false),
            gui_applescript(
                "/repo", "/bin/sauron", 2, &work, &[], Agent::Claude, None, false, &gui.cmd,
            ),
            spawn_script("UUID-1", "cd /repo && claude", false),
            orc_spawn_script("UUID-1", "cd /repo && /bin/sauron orc src/big.rs"),
        ];
        for s in &scripts {
            if s.contains("my tint(") {
                assert!(
                    s.contains("on tint(sess, c)"),
                    "calls my tint but never defines it"
                );
            }
        }
        // And the two layouts really do tint, so this test cannot pass by the
        // feature having quietly disappeared.
        assert!(scripts[0].contains("my tint("), "ordinary layout must tint");
        assert!(scripts[1].contains("my tint("), "gui layout must tint");
    }

    #[test]
    fn codex_panes_are_left_unnamed_rather_than_given_a_flag_it_lacks() {
        // Codex has no --name, and inventing one would make every pane fail to
        // start. They are told apart by the pane title instead.
        let cmds = left_commands("/repo", "/bin/sauron", 2, &[], Agent::Codex, None, false);
        assert!(cmds.iter().all(|c| !c.contains("--name")), "{cmds:?}");
        assert!(cmds.iter().all(|c| !c.contains("--session-id")), "{cmds:?}");
    }

    #[test]
    fn a_resumed_codex_pane_yields_its_id_the_way_a_claude_one_does() {
        // The bug: `pane_session_ids` looked for `--session-id ` and `--resume `,
        // which are Claude's spellings. `codex resume id-1` matched neither, so
        // every Codex pane came back with no id -- untinted under iTerm2 and
        // titled `agent-1` under Windows Terminal, even when sauron knew exactly
        // which session the pane was.
        let work = vec![("id-1".into(), "t".into())];
        let cmds = left_commands("/repo", "/bin/sauron", 2, &work, Agent::Codex, None, false);
        let ids = pane_session_ids(&cmds, Agent::Codex);
        assert_eq!(ids[0].as_deref(), Some("id-1"), "{cmds:?}");
        // A *fresh* Codex pane still has none, and that is not this bug: Codex
        // takes no session id at launch, so there is nothing in the command to
        // read. See `Agent::fresh_cmd`.
        assert_eq!(ids[1], None, "{cmds:?}");

        // Claude's reader must not have widened. `codex resume ` carries the
        // program word so a repo path cannot be mistaken for a launch form.
        let claude = left_commands("/repo", "/bin/sauron", 1, &work, Agent::Claude, None, false);
        assert_eq!(
            pane_session_ids(&claude, Agent::Claude)[0].as_deref(),
            Some("id-1")
        );
        let trap = vec!["cd /work/resume drafts && codex".to_string()];
        assert_eq!(pane_session_ids(&trap, Agent::Codex)[0], None, "{trap:?}");
    }

    // Tint is macOS-only; Windows Terminal carries the servant as a pane title.
    #[cfg(unix)]
    #[test]
    fn a_resumed_codex_pane_is_tinted_like_the_board_row_it_is() {
        let work = vec![("id-1".into(), "t".into())];
        let cmds = left_commands("/repo", "/bin/sauron", 2, &work, Agent::Codex, None, false);
        let tints = as_tint_list(&cmds, Agent::Codex);
        let c = servant::color_for("id-1");
        assert!(
            tints.contains(&format!("{{{}, {}}}", as_rgb(tinted(c)), as_rgb(c))),
            "no tint for the resumed pane in {tints}"
        );
        // The fresh pane has no id and therefore no colour; `missing value` is
        // the handler's "leave the profile alone", not a failure.
        assert!(tints.contains("missing value"), "{tints}");
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn applescript_stacks_orcs_beneath_sauron() {
        let work = vec![("id-1".into(), "t".into())];
        let orcs = vec![orc_command(
            "/repo",
            Path::new("/bin/sauron"),
            "src/big.rs",
            Agent::Claude,
            None,
        )];
        let s = applescript("/repo", "/bin/sauron", 2, &work, &orcs, Agent::Claude, None, false);
        assert!(s.contains("cd /repo && /bin/sauron --claude")); // watcher on top-right
        assert!(s.contains("claude --resume id-1")); // a hobbit on the left
        assert!(s.contains("src/big.rs")); // the orc's target
        // The orc rides in the right column, carrying the short reviewable line
        // rather than the whole charge -- the charge itself lives in `orc`.
        assert!(s.contains("set rightCmds to {\"cd /repo && /bin/sauron orc src/big.rs\""));
        // …and it is STAGED, not run: one orc, typed in but awaiting Enter.
        assert!(s.contains("set orcCount to 1"));
        assert!(s.contains("write text (item i of rightCmds) newline no"));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn codex_agent_swaps_the_spawn_commands() {
        let work = vec![("id-1".into(), "t".into())];
        let orcs = vec![orc_command(
            "/repo",
            Path::new("/bin/sauron"),
            "src/big.rs",
            Agent::Codex,
            None,
        )];
        let s = applescript("/repo", "/bin/sauron", 1, &work, &orcs, Agent::Codex, None, false);
        assert!(s.contains("codex resume id-1")); // hobbit resumes via codex
        // The orc line is agent-agnostic now -- `sauron orc` picks the agent up
        // itself and execs `codex exec` with the charge as an argv element.
        assert!(s.contains("cd /repo && /bin/sauron orc src/big.rs"));
        assert!(s.contains("/bin/sauron --codex")); // watcher pane watches codex
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn mordor_wires_hobbits_and_orcs_to_the_local_model_but_not_the_eye() {
        let m = Mordor {
            model: "qwen3-coder".into(),
            base_url: "http://localhost:11434".into(),
        };
        let work = vec![("id-1".into(), "t".into())];
        let orcs = vec![orc_command(
            "/repo",
            Path::new("/bin/sauron"),
            "src/big.rs",
            Agent::Claude,
            Some(&m),
        )];
        let s = applescript("/repo", "/bin/sauron", 2, &work, &orcs, Agent::Claude, Some(&m), false);

        // The hobbit pane carries the local endpoint before the `claude` word.
        assert!(s.contains("cd /repo && ANTHROPIC_BASE_URL=http://localhost:11434"));
        assert!(s.contains("ANTHROPIC_MODEL=qwen3-coder ANTHROPIC_SMALL_FAST_MODEL"));
        // …and it still resumes the working session, now through the local model.
        assert!(s.contains("ANTHROPIC_DEFAULT_HAIKU_MODEL=qwen3-coder claude --resume id-1"));
        // The orc too -- the env rides ahead of the sauron word, and `sauron orc`
        // passes it straight through to the agent it execs.
        assert!(s.contains("=qwen3-coder /bin/sauron orc src/big.rs"));
        // But the Eye pane never gets the env -- sauron calls no model.
        assert!(s.contains("cd /repo && /bin/sauron --claude"));
        assert!(!s.contains("ANTHROPIC_BASE_URL=http://localhost:11434 /bin/sauron"));
    }

    #[test]
    fn orc_command_stages_a_short_reviewable_line() {
        let c = orc_command("/repo", Path::new("/bin/sauron"), "src/big.rs", Agent::Claude, None);
        assert_eq!(c, "cd /repo && /bin/sauron orc src/big.rs");
        // No quotes of either kind: this sits in a shell command inside an
        // AppleScript double-quoted string literal.
        assert!(!c.contains('"'), "orc command must be double-quote-free: {c}");
        assert!(!c.contains('\''), "orc command must be single-quote-free: {c}");
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn strict_clipboard_mode_wraps_fresh_and_resumed_panes() {
        let work = vec![("id-1".to_string(), "task one".to_string())];
        let s = applescript(
            "/repo",
            "/bin/sauron",
            2,
            &work,
            &[],
            Agent::Claude,
            None,
            true,
        );
        assert_eq!(s.matches("handoff-run").count(), 2);
        assert!(s.contains("--resume 'id-1'"));
        assert!(s.contains("slot.0.handoff"));
        assert!(s.contains("slot.1.handoff"));
    }

    /// The isolation contract, stated as a test: a repo with no GUI gets the
    /// layout it always got. If this fails, the hook has leaked.
    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn a_repo_without_a_gui_gets_the_original_layout_untouched() {
        let work = vec![("id-1".to_string(), "task one".to_string())];
        let s = applescript("/repo", "/bin/sauron", 3, &work, &[], Agent::Claude, None, false);
        assert!(s.contains(r#"set value of attribute "AXFullScreen" of window 1 to true"#));
        assert!(!s.contains("set zoomed"));
        assert!(!s.contains("user.sauron_role"));
        assert!(!s.contains("sauron gui"));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn gui_layout_holds_the_middle_column_open_for_the_app() {
        let work = vec![("id-1".into(), "t".into())];
        let s = gui_applescript(
            "/repo",
            "/bin/sauron",
            2,
            &work,
            &[],
            Agent::Claude,
            None,
            false,
            "cd /repo && /bin/sauron gui",
        );
        // agents | hole | eye. Two splits off one splitter, which iTerm2 then
        // divides evenly -- that, not halving, is what makes them exact thirds,
        // and it is why there are two splits here and not three.
        assert!(s.contains("tell agentTop to set hole to (split vertically with default profile)"));
        assert!(s.contains("tell hole to set eye to (split vertically with default profile)"));
        // The middle column is three equal rows; the app covers the top two.
        assert!(s.contains("tell hole to set holeMid to (split horizontally with default profile)"));
        assert!(s.contains("tell holeMid to set appLog to (split horizontally with default profile)"));
        // The Eye keeps the last column, and still watches the chosen agent.
        assert!(s.contains("tell eye to write text \"cd /repo && /bin/sauron --claude\""));
        // A hobbit still resumes its session in the agent column.
        assert!(s.contains("cd /repo && claude --resume id-1"));
    }

    /// The layout and `gui::DEFAULT_RECT` describe the same rectangle in two
    /// languages. If one moves without the other, the application lands over the
    /// Eye or over the agent column, which is the failure that looks like a bug
    /// in the docking rather than in the layout.
    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn the_hole_and_the_docking_rect_agree_on_thirds() {
        let (x, y, w, h) = crate::gui::DEFAULT_RECT;
        assert!((x - 1.0 / 3.0).abs() < 1e-9, "hole starts one column across");
        assert!((w - 1.0 / 3.0).abs() < 1e-9, "and is one column wide");
        assert_eq!(y, 0.0);
        assert!((h - 2.0 / 3.0).abs() < 1e-9, "two of the column's three rows");
        // Exactly one pane in the middle column is left uncovered, and it is the
        // one the launcher was typed into.
        let s = gui_applescript(
            "/repo", "/bin/sauron", 1, &[], &[], Agent::Claude, None, false, "cd /repo && /bin/sauron gui",
        );
        assert!(s.contains("set guiPanes to {hole, holeMid, appLog}"));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn gui_layout_cannot_use_native_fullscreen() {
        // A fullscreen Space holds one window. Going fullscreen here would put
        // the application on a different Space than the workspace holding a hole
        // open for it -- the one failure that makes the whole feature pointless.
        let s = gui_applescript(
            "/repo", "/bin/sauron", 1, &[], &[], Agent::Claude, None, false, "cd /repo && /bin/sauron gui",
        );
        assert!(!s.contains("AXFullScreen"));
        // Either frame source is acceptable -- which one appears depends on
        // whether AppKit answered on the machine running the test.
        assert!(s.contains("set bounds of w to {") || s.contains("set zoomed of w to true"));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn gui_layout_tags_every_pane_the_app_covers() {
        let s = gui_applescript(
            "/repo", "/bin/sauron", 1, &[], &[], Agent::Claude, None, false, "cd /repo && /bin/sauron gui",
        );
        assert!(s.contains("set guiPanes to {hole, holeMid, appLog}"));
        assert!(s.contains(r#"set variable named "user.sauron_role" to "gui""#));
    }

    /// Same contract as an orc, for the same reason: `run.sh` is a release
    /// build, and opening a window is not consent to start one.
    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn gui_launcher_is_staged_not_run() {
        let s = gui_applescript(
            "/repo", "/bin/sauron", 1, &[], &[], Agent::Claude, None, false, "cd /repo && /bin/sauron gui",
        );
        assert!(
            s.contains(r#"tell appLog to write text "cd /repo && /bin/sauron gui" newline no"#),
            "the app launcher must be typed and left awaiting Enter: {s}"
        );
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn gui_layout_stacks_orcs_under_the_eye_not_in_the_hole() {
        let orcs = vec![orc_command(
            "/repo",
            Path::new("/bin/sauron"),
            "src/big.rs",
            Agent::Claude,
            None,
        )];
        let s = gui_applescript(
            "/repo", "/bin/sauron", 1, &[], &orcs, Agent::Claude, None, false, "cd /repo && /bin/sauron gui",
        );
        assert!(s.contains("set eyePanes to {eye}"));
        assert!(s.contains("set orcCmds to {\"cd /repo && /bin/sauron orc src/big.rs\"}"));
        // Still staged, exactly as in the ordinary layout.
        assert!(s.contains("tell newP to write text (item i of orcCmds) newline no"));
    }

    // Asserts on generated AppleScript; there is none off macOS.
    #[cfg(unix)]
    #[test]
    fn both_spawn_scripts_skip_the_panes_under_a_docked_app() {
        let left = spawn_script("UUID-1", "cd /repo && claude", false);
        let orc = orc_spawn_script("UUID-1", "cd /repo && /bin/sauron orc src/big.rs");
        for s in [&left, &orc] {
            assert!(s.contains("my isGuiPane(item k of ss)"));
            assert!(s.contains("on isGuiPane(s)"));
            // Read from a session variable, not the pane title: a shell rewrites
            // its title on every prompt, and would erase a name-based mark.
            assert!(s.contains(r#"variable named "user.sauron_role""#));
        }
    }

    #[test]
    fn expand_handles_tilde() {
        assert_eq!(expand("/abs/path"), PathBuf::from("/abs/path"));
        assert!(expand("~/x").starts_with(home()));
        assert_eq!(expand("~"), home());
    }

    // -----------------------------------------------------------------------
    // The Windows Terminal layout. These run on every platform, deliberately:
    // the builder is a string function, and the machine doing the porting is
    // the one that needs to be able to check it. What they cannot establish is
    // that `wt` accepts the result -- only a Windows run does that.
    // -----------------------------------------------------------------------

    /// Index of the first argv element equal to `needle`.
    fn at(argv: &[String], needle: &str) -> usize {
        argv.iter().position(|a| a == needle).expect(needle)
    }

    #[test]
    fn wt_layout_carves_the_eyes_column_before_it_subdivides_anything() {
        // The load-bearing ordering constraint. `wt` splits whatever pane has
        // focus, so the full-height right column can only be made by splitting
        // the window's one original pane. Any horizontal split reaching `wt`
        // first would leave the Eye's column as tall as one agent pane.
        let work = vec![("s1".to_string(), "t".to_string())];
        let argv = wt_layout_argv(
            "C:\\code\\repo",
            "sauron.exe",
            3,
            &work,
            &["orc one".to_string()],
            Agent::Claude,
            None,
            false,
        );
        let vertical = at(&argv, "--vertical");
        let first_horizontal = at(&argv, "--horizontal");
        assert!(
            vertical < first_horizontal,
            "the vertical carve must precede every horizontal split"
        );
    }

    #[test]
    fn wt_layout_opens_one_named_fullscreen_window_per_repo() {
        let argv = wt_layout_argv(
            "C:\\code\\my repo",
            "sauron.exe",
            1,
            &[],
            &[],
            Agent::Claude,
            None,
            false,
        );
        assert_eq!(argv[0], "-w");
        // Named, not `0`: two workspaces open at once must each grow themselves
        // rather than race for whichever window was last touched.
        assert_eq!(argv[1], "sauron-my-repo");
        assert!(argv.contains(&"--fullscreen".to_string()));
    }

    #[test]
    fn wt_layout_gives_every_pane_the_agent_column_asked_for() {
        let argv = wt_layout_argv(
            "C:\\r",
            "sauron.exe",
            4,
            &[],
            &[],
            Agent::Claude,
            None,
            false,
        );
        // Four agent panes, each titled with its own servant rather than a
        // shared word -- the pane title is the only servant marker Windows
        // Terminal can carry, so a repeated one loses the distinction entirely.
        let lefts = left_commands("C:\\r", "sauron.exe", 4, &[], Agent::Claude, None, false);
        let want: Vec<String> = pane_session_ids(&lefts, Agent::Claude)
            .into_iter()
            .flatten()
            .map(|id| servant::name_for(&id).to_string())
            .collect();
        assert_eq!(want.len(), 4);
        let titled = argv
            .iter()
            .filter(|a| servant::NAMES.contains(&a.as_str()))
            .count();
        assert_eq!(titled, 4, "argv: {argv:?}");
        // Each of the later three is reached by returning to the column first.
        let returns = argv.windows(2).filter(|w| *w == ["move-focus", "first"]).count();
        assert_eq!(returns, 3);
    }

    #[test]
    fn wt_layout_tells_every_pane_which_window_and_which_pane_the_eye_is() {
        // This pair is the whole of what stands in for $ITERM_SESSION_ID. Without
        // it a sauron inside the workspace cannot tell which window to grow, and
        // silently falls back to "whichever was last used".
        let argv = wt_layout_argv(
            "C:\\r",
            "sauron.exe",
            2,
            &[],
            &[],
            Agent::Claude,
            None,
            false,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("$env:SAURON_WT_WINDOW='sauron-r';"));
        assert!(joined.contains("$env:SAURON_WT_PANE='1';"));
        // And the layout leaves the user on that pane.
        assert!(argv.windows(2).any(|w| w == ["--target", "1"]));
    }

    #[test]
    fn wt_layout_puts_the_orcs_in_the_eyes_column_not_the_hobbits() {
        let argv = wt_layout_argv(
            "C:\\r",
            "sauron.exe",
            2,
            &[],
            &["orc a".to_string(), "orc b".to_string()],
            Agent::Claude,
            None,
            false,
        );
        // Both orcs are placed after the vertical carve and before the layout
        // goes back to the agent column -- which is what "beneath the Eye" means
        // in a tree that can only be built by splitting the focused pane.
        let vertical = at(&argv, "--vertical");
        let back_to_agents = at(&argv, "move-focus");
        let orcs: Vec<usize> = argv
            .iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == "orc")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(orcs.len(), 2);
        assert!(orcs.iter().all(|&i| i > vertical && i < back_to_agents));
    }

    #[test]
    fn wt_layout_never_hands_a_bare_semicolon_to_a_pane_command() {
        // `wt` reads a lone `;` argv element as a subcommand separator. A pane
        // command carrying one as *text* would be torn in half and the tail run
        // as a second wt subcommand, so every command must arrive as one element.
        let argv = wt_layout_argv(
            "C:\\r",
            "sauron.exe",
            1,
            &[],
            &[],
            Agent::Claude,
            None,
            false,
        );
        // The prelude that sets the env vars contains semicolons by construction;
        // assert it survives as a single argument rather than as separators.
        let prelude = argv
            .iter()
            .find(|a| a.contains("$env:SAURON_WT_WINDOW"))
            .expect("prelude");
        assert!(prelude.contains(';'));
        assert_ne!(prelude.as_str(), ";");
    }

    #[test]
    fn wt_layout_translates_the_panes_shell_syntax_rather_than_shipping_it() {
        // `left_commands` emits `cd '<repo>' && ...`, which PowerShell 5.1 cannot
        // parse. The pane's starting directory replaces it.
        let work = vec![("abc".to_string(), "t".to_string())];
        let argv = wt_layout_argv(
            "C:\\r",
            "sauron.exe",
            1,
            &work,
            &[],
            Agent::Claude,
            None,
            false,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("claude --resume abc"));
        assert!(!joined.contains("&&"));
        // The directory did not simply vanish -- it moved to `-d`.
        assert!(argv.windows(2).any(|w| w == ["-d", "C:\\r"]));
    }
}
