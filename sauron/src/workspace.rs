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
//!   fn spawn_left_pane  -- grow the agent column from inside the running TUI
//!   fn spawn_script     -- the split script that does it
//!   fn spawn_orc_pane   -- stage an orc in sauron's own column, from the TUI
//!   fn orc_command      -- the short line an orc pane runs (`sauron orc <file>`)
//!   fn osascript        -- pipe the script to `osascript`
//!
//! The orc *charge* -- what the agent is actually told to do, and which files are
//! cold enough to hand it -- lives in `orc`, not here. This module only decides
//! where the pane goes.

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::agent::{Agent, Mordor};
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
        for t in &orc_targets {
            println!("ORC={t}");
        }
        return Ok(());
    }

    let script = applescript(
        &repo_s,
        &sauron_exe.to_string_lossy(),
        total,
        &work,
        &orc_cmds,
        agent,
        mordor.as_ref(),
        clipboard_handoff,
    );
    osascript(&script)?;

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
fn as_list(cmds: &[String]) -> String {
    cmds.iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the AppleScript that opens the window, fullscreens it onto its own
/// Space, and lays out the panes: the left column is one hobbit pane per
/// `total`, and the right column is sauron on top with the orcs stacked beneath
/// it (plus a shell). Both columns split the currently-tallest pane each step so
/// they stay balanced rather than shrinking geometrically.
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
    // Left column: resume each in-flight session, a fresh agent for the rest. In
    // Mordor mode each hobbit carries the local-model env before the agent word;
    // the orcs already carry theirs (built in `orc_command`), and the sauron
    // watcher pane deliberately does not -- the Eye calls no model.
    let env = agent.local_env(mordor);
    let left: Vec<String> = (0..total)
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
            Some((id, _)) => format!("cd {repo} && {env}{}", agent.resume_cmd(id)),
            None => format!("cd {repo} && {env}{}", agent.label()),
        })
        .collect();
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
  tell leftTop to write text (item 1 of leftCmds)
  set leftPanes to {{leftTop}}
  repeat with i from 2 to (count of leftCmds)
    set tallest to item 1 of leftPanes
    repeat with p in leftPanes
      if (rows of p) > (rows of tallest) then set tallest to contents of p
    end repeat
    tell tallest to set newP to (split horizontally with default profile)
    tell newP to write text (item i of leftCmds)
    set end of leftPanes to newP
  end repeat

  -- Land focus on the first agent pane.
  select leftTop
end tell
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
          set end of rights to (item k of ss)
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
"#
    )
}

/// This pane's session UUID. iTerm2 exports `w<win>t<tab>p<pane>:<uuid>`, and
/// the uuid tail is exactly the `id` the AppleScript session class reports.
fn iterm_session_id() -> Option<String> {
    let raw = std::env::var("ITERM_SESSION_ID").ok()?;
    let uuid = raw.rsplit_once(':').map(|(_, u)| u).unwrap_or(&raw).trim();
    (!uuid.is_empty()).then(|| uuid.to_string())
}

/// Escape a Rust string into an AppleScript string literal body.
fn as_str_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The split script. Answers `OK`, or `ERR <why>` -- every failure is a message
/// the footer can show, never a silent no-op.
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
          set end of lefts to (item k of ss)
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
"#
    )
}

/// Pipe the AppleScript to `osascript` on stdin and hand back its stdout. Unlike
/// `osascript`, this never prints or exits -- its caller is the TUI.
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

    #[test]
    fn spawn_script_escapes_the_command_into_the_applescript_literal() {
        let s = spawn_script("UUID-1", r#"cd "/re po" && claude"#, false);
        assert!(s.contains(r#"write text "cd \"/re po\" && claude""#));
    }

    #[test]
    fn applescript_lists_one_command_per_pane() {
        let work = vec![("id-1".to_string(), "task one".to_string())];
        let s = applescript("/repo", "/bin/sauron", 3, &work, &[], Agent::Claude, None, false);
        // First hobbit pane resumes the working task; the rest are bare claude.
        assert!(s.contains("cd /repo && claude --resume id-1"));
        assert_eq!(s.matches("cd /repo && claude").count(), 3); // resume line counts too
        // The sauron pane runs this binary against the repo, watching the agent.
        assert!(s.contains("cd /repo && /bin/sauron --claude"));
        assert!(s.contains("set leftCmds to {\"cd /repo && claude --resume id-1\", "));
    }

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

    #[test]
    fn expand_handles_tilde() {
        assert_eq!(expand("/abs/path"), PathBuf::from("/abs/path"));
        assert!(expand("~/x").starts_with(home()));
        assert_eq!(expand("~"), home());
    }
}
