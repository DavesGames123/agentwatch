//! `sauron gui` -- dock a project's own application window into the workspace
//! layout, so the thing you are building sits between the agents building it and
//! the Eye watching them.
//!
//! This is a hook, not a feature of the watcher. A repo declares a GUI in
//! `.sauron/gui.conf`; a repo that doesn't declare one never reaches a line of
//! this module, and `workspace` emits exactly the layout it emitted before.
//!
//! Why the application stays a real window instead of being drawn into a pane:
//! an iTerm2 pane is a pty, and the only thing that can enter a pty is bytes, so
//! "in the terminal" would mean streaming pixels. Measured on a 900x700 frame
//! that is 1.3 MiB of PNG, 1.7 MiB once base64'd -- 17.8 MB/s at ten frames a
//! second, plus 40% of a core in deflate, for a picture that cannot be clicked.
//! The window stays a window. What this module does is keep it exactly over the
//! hole the pane grid leaves for it, and keep it raised above the terminal
//! without taking the keyboard away from the terminal.
//!
//! The three coordinates that make that work, all verified against iTerm2 3.6.11
//! rather than assumed:
//!   * a split divider cannot be positioned (`set columns` reports success and
//!     changes nothing; there is no `AXSplitGroup` to drag), but iTerm2 re-divides
//!     a splitter evenly whenever a pane joins it -- so N splits off one splitter
//!     give N+1 equal panes, and the layout's thirds are exact.
//!   * a session carries user variables that terminal escape sequences cannot
//!     clobber, so the panes under the app are tagged `user.sauron_role = gui`
//!     and the TUI's pane-spawning keys skip them.
//!   * `perform action "AXRaise"` reorders a window without activating its
//!     application -- the app floats over iTerm while you keep typing into iTerm.
//!
//! grep targets:
//!   struct Gui / fn config  -- .sauron/gui.conf discovery
//!   fn parse                -- the key = value reader (pure, tested)
//!   fn rect                 -- iTerm window bounds + fractions -> the app's rect
//!   fn run                  -- the pane command: launch, adopt, dock, keep
//!   fn descendants          -- our child's process tree (run.sh -> cargo -> app)
//!   fn owner_with_window    -- which descendant actually owns a window
//!   fn place / fn raise     -- position, size, and keep-above, via System Events
//!   fn stage_command        -- the line the log pane is handed, typed not run

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Where a repo declares its GUI. Repo-local on purpose: barnes-hut is the thing
/// that knows it launches with `./run.sh`, and the declaration should travel with
/// the checkout rather than live in a registry on one machine.
pub const CONF: &str = ".sauron/gui.conf";

/// The app's share of the iTerm window, as fractions `(x, y, w, h)`.
///
/// The middle third, two thirds tall, because that is where the workspace layout
/// actually puts the hole: a divider cannot be positioned, but iTerm2 re-divides
/// a splitter evenly as panes join it, so two splits give three equal columns and
/// two more give three equal rows in the middle one. These fractions and
/// `gui_applescript` describe the same rectangle and have to be changed together.
pub const DEFAULT_RECT: (f64, f64, f64, f64) = (1.0 / 3.0, 0.0, 1.0 / 3.0, 2.0 / 3.0);

/// How the app window gets into the hole, and how it stays on top of it.
///
/// Which of these can work is a property of the *application*, not a preference,
/// and the dividing line is whether it is a bundled `.app`:
///
/// * A bundled app (Stremio, TextEdit, anything from /Applications) publishes an
///   Accessibility window hierarchy, so `Raise` can move, size, and raise it from
///   outside with no cooperation at all.
/// * A bare Mach-O binary -- `cargo run`'s output, which is what both of the
///   projects this was built for actually launch -- publishes **no** AX windows.
///   Measured: a running winit app, `background only = false`, `visible = true`,
///   frontmost, and `count of windows` still `0`. Nothing outside the process can
///   touch that window, whatever API it asks in. Such an app has to place itself,
///   which is what `App` is for and why the rect is exported to every child
///   regardless of policy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keep {
    /// Move, size, and re-raise it from out here, on a timer. Requires an app
    /// with an AX hierarchy. `AXRaise` reorders a window without activating its
    /// process, so the app floats over the terminal while your keystrokes keep
    /// going to the terminal -- verified, not assumed.
    Raise,
    /// The app places and pins itself, reading `$SAURON_DOCK_RECT` and
    /// `$SAURON_DOCK_TOP`. The only thing that works for a non-bundled binary,
    /// and steadier than `Raise` even where `Raise` works, because the window
    /// never flickers behind the terminal on a click.
    App,
    /// Export the rect and otherwise keep out of it.
    Off,
}

/// A screen rectangle in points, top-left origin -- the coordinate system both
/// iTerm2's `bounds` and System Events' `position`/`size` speak.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A repo's GUI declaration.
#[derive(Clone, Debug)]
pub struct Gui {
    /// The launch line, run through `sh -c` from the repo root.
    pub cmd: String,
    pub keep: Keep,
    /// Fractions of the iTerm window the app is docked into.
    pub rect: (f64, f64, f64, f64),
    /// Only needed when the window's owner cannot be found by walking the
    /// process tree -- an app that daemonises away from its launcher.
    pub app: Option<String>,
    /// How often the keeper re-checks, in milliseconds.
    pub poll_ms: u64,
}

impl Default for Gui {
    fn default() -> Self {
        Self {
            cmd: String::new(),
            keep: Keep::Raise,
            rect: DEFAULT_RECT,
            app: None,
            poll_ms: 1000,
        }
    }
}

/// Read `<repo>/.sauron/gui.conf`. `None` -- the overwhelmingly common case --
/// means this repo has no GUI and nothing anywhere else changes behaviour.
///
/// A file that exists but declares no `cmd` warns and still answers `None`: a
/// half-written config should not silently turn into a workspace layout with a
/// hole in it and nothing to fill the hole.
pub fn config(repo: &Path) -> Option<Gui> {
    let path = repo.join(CONF);
    let text = std::fs::read_to_string(&path).ok()?;
    match parse(&text) {
        Some(g) => Some(g),
        None => {
            eprintln!(
                "sauron: {} declares no `cmd =` line -- ignoring it.",
                path.display()
            );
            None
        }
    }
}

/// The config reader. `key = value`, `#` comments, blank lines ignored.
///
/// Deliberately not TOML: the whole grammar is five keys, and sauron's other
/// registry (`~/.claude/sauron/workspaces`) is already a flat text file. A
/// dependency to parse five keys would be the largest thing in the crate.
pub fn parse(text: &str) -> Option<Gui> {
    let mut g = Gui::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "cmd" => g.cmd = v.to_string(),
            "app" => g.app = (!v.is_empty()).then(|| v.to_string()),
            "keep" => {
                g.keep = match v {
                    "app" | "always-on-top" => Keep::App,
                    "off" | "none" => Keep::Off,
                    _ => Keep::Raise,
                }
            }
            "poll" => g.poll_ms = v.parse().unwrap_or(1000).max(200),
            "rect" => {
                let f: Vec<f64> = v.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                if let [x, y, w, h] = f[..] {
                    g.rect = (x, y, w, h);
                }
            }
            _ => {}
        }
    }
    (!g.cmd.is_empty()).then_some(g)
}

/// The app's pixel rectangle inside a given iTerm window.
///
/// Taken from the *window*, not the screen, so dragging or resizing the
/// workspace window and re-running the app re-docks it correctly, and so a
/// second display needs no special case.
pub fn rect(win: Rect, f: (f64, f64, f64, f64)) -> Rect {
    Rect {
        x: win.x + (win.w as f64 * f.0).round() as i32,
        y: win.y + (win.h as f64 * f.1).round() as i32,
        w: (win.w as f64 * f.2).round() as i32,
        h: (win.h as f64 * f.3).round() as i32,
    }
}

/// The usable frame of the display in use, as the `{left, top, right, bottom}`
/// iTerm2's `bounds` speaks. `None` if the bridge is unavailable, in which case
/// the layout falls back to `set zoomed`.
///
/// Asked of AppKit rather than derived, because every cheaper source is wrong:
/// `bounds of window of desktop` returns the union of *all* displays on a
/// multi-monitor Mac, `size of menu bar 1` is not a list you can index, and
/// zooming preserves the window's origin instead of moving it to the corner.
/// AppKit is also the only one of them that knows where the Dock is -- on this
/// machine the visible frame starts at x=50 because the Dock is on the left, and
/// a layout that assumed x=0 would hang 50 points of the agent column behind it.
pub fn visible_frame() -> Option<Rect> {
    // AppKit is bottom-left origin, measured off screen 0; iTerm2's bounds are
    // top-left. The flip needs screen 0's full height, not the visible height.
    const JS: &str = r#"ObjC.import('AppKit');
const v = $.NSScreen.mainScreen.visibleFrame;
const h0 = $.NSScreen.screens.objectAtIndex(0).frame.size.height;
const top = h0 - (v.origin.y + v.size.height);
[Math.round(v.origin.x), Math.round(top), Math.round(v.origin.x + v.size.width), Math.round(top + v.size.height)].join(",")"#;
    let out = osascript_js_out(JS).ok()?;
    let n: Vec<i32> = out
        .trim()
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    match n[..] {
        [l, t, r, b] => Some(Rect {
            x: l,
            y: t,
            w: r - l,
            h: b - t,
        }),
        _ => None,
    }
}

/// The line the app-log pane is handed: short enough to read before you press
/// Enter, exactly like an orc's `sauron orc <file>`. It is *typed and not run* by
/// the launcher, because launching this is a release build, and a workspace
/// opening should never start one on its own.
pub fn stage_command(sauron_exe: &Path, repo: &str) -> String {
    format!("cd {repo} && {} gui", sauron_exe.display())
}

// ---------------------------------------------------------------------------
// The runner: `sauron gui [repo]`
// ---------------------------------------------------------------------------

/// Launch the declared command and dock whatever window it produces.
///
/// The child's stdio is inherited, so this pane *is* the application's log --
/// the bottom strip in the workspace. Docking happens on a background thread so
/// nothing about it can stall the output you are reading.
pub fn run(args: &[String]) -> std::io::Result<()> {
    let mut print_only = false;
    let mut mirror = false;
    let mut repo_arg: Option<&str> = None;
    for a in args {
        match a.as_str() {
            "--print" | "-p" => print_only = true,
            "--mirror" => mirror = true,
            s if !s.starts_with('-') => repo_arg = Some(s),
            _ => {}
        }
    }
    let repo = match repo_arg {
        Some(r) => PathBuf::from(r),
        None => crate::git_root().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }),
    };

    // `--mirror` does not launch anything. It attaches to a window that is
    // already up -- you ran the app yourself -- and draws it into this pane, so
    // it needs no `cmd` and works in a repo with no conf at all.
    if mirror {
        let app = config(&repo).and_then(|g| g.app);
        return crate::mirror::run(args, &repo, app.as_deref());
    }

    let Some(gui) = config(&repo) else {
        eprintln!(
            "sauron gui: {} declares no GUI.",
            repo.join(CONF).display()
        );
        eprintln!("  create it with two lines:");
        eprintln!("    cmd  = ./run.sh");
        eprintln!("    keep = raise");
        std::process::exit(2);
    };

    let target = dock_target(&gui);
    if print_only {
        println!("CMD={}", gui.cmd);
        println!("KEEP={:?}", gui.keep);
        match target {
            Some(r) => println!("RECT={},{},{},{}", r.x, r.y, r.w, r.h),
            None => println!("RECT=none (not in an iTerm2 pane)"),
        }
        return Ok(());
    }

    println!("sauron gui: {}", gui.cmd);
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&gui.cmd)
        .current_dir(&repo)
        // The hole's coordinates, handed to every child whatever the policy --
        // an app that can place itself should never have to be told twice, and
        // for a non-bundled binary this is the *only* channel that works.
        .env(
            "SAURON_DOCK_RECT",
            target
                .map(|r| format!("{},{},{},{}", r.x, r.y, r.w, r.h))
                .unwrap_or_default(),
        )
        .env("SAURON_DOCK_TOP", if gui.keep == Keep::App { "1" } else { "0" })
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    let pid = child.id();
    let stop = Arc::new(AtomicBool::new(false));
    // `off` still exported the rect above; what it declines is the polling.
    let keeper = target.filter(|_| gui.keep != Keep::Off).map(|t| {
        let stop = Arc::clone(&stop);
        let gui = gui.clone();
        std::thread::spawn(move || keep_docked(pid, t, gui, stop))
    });

    let status = child.wait()?;
    stop.store(true, Ordering::Relaxed);
    if let Some(k) = keeper {
        let _ = k.join();
    }
    std::process::exit(status.code().unwrap_or(0));
}

/// Where this pane's window puts the app, or `None` outside a workspace window
/// (a bare `sauron gui` in any terminal still launches the app -- it just does
/// not move it, because there is no layout to move it into).
fn dock_target(gui: &Gui) -> Option<Rect> {
    iterm_window_bounds().map(|w| rect(w, gui.rect))
}

/// The dock keeper. Adopts the window when it appears, places it once, and then
/// only re-raises -- so dragging the app somewhere else is not undone a second
/// later, while a click into a terminal pane never buries it for long.
///
/// A relaunch inside the same `sauron gui` (a `run.sh` that restarts the binary)
/// shows up as a new pid, which is the signal to place again.
fn keep_docked(root: u32, target: Rect, gui: Gui, stop: Arc<AtomicBool>) {
    let mut placed: Option<u32> = None;
    let mut rounds: u32 = 0;
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        rounds += 1;
        match owner_with_window(root, gui.app.as_deref()) {
            Some(pid) if placed != Some(pid) => {
                if place(pid, target).is_ok() {
                    placed = Some(pid);
                }
            }
            Some(pid) => {
                if gui.keep == Keep::Raise {
                    let _ = raise(pid);
                }
            }
            None => {
                placed = None;
                // Say so once, rather than polling in silence forever. A window
                // that never turns up in Accessibility is the normal case for a
                // bare `cargo run` binary, and the fix is on the app's side --
                // so print the coordinates it should be using.
                if !warned && rounds > 8 {
                    warned = true;
                    eprintln!(
                        "sauron gui: this app publishes no accessibility window, so it cannot be \
                         moved from outside (normal for a non-bundled binary)."
                    );
                    eprintln!(
                        "  it can place itself: $SAURON_DOCK_RECT={},{},{},{} — see `keep = app` in {CONF}.",
                        target.x, target.y, target.w, target.h
                    );
                }
            }
        }
        // Sliced so a finished child is noticed promptly rather than after a
        // whole poll interval of a process that has already exited.
        for _ in 0..(gui.poll_ms / 100).max(1) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Every descendant of `root`, plus `root` itself, deepest last.
///
/// `./run.sh` is a shell that runs `cargo`, which runs the binary that owns the
/// window -- and under `| tee` the binary is a sibling of the pipe. Walking the
/// tree finds the window's owner in all of those shapes without the config
/// having to name a process.
fn descendants(root: u32) -> Vec<u32> {
    let Ok(out) = Command::new("ps").args(["-axo", "pid=,ppid="]).output() else {
        return vec![root];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let pairs: Vec<(u32, u32)> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    let mut found = vec![root];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        for (pid, ppid) in &pairs {
            if *ppid == parent && !found.contains(pid) {
                found.push(*pid);
            }
        }
        i += 1;
    }
    found
}

/// The descendant that actually owns a window, or the configured app name's
/// process if the tree walk comes up empty.
///
/// Checked deepest-first: `run.sh` and `cargo` are ancestors of the binary, and
/// neither has a window, but asking about them first would be one AppleScript
/// round trip per useless candidate.
fn owner_with_window(root: u32, app: Option<&str>) -> Option<u32> {
    let mut pids = descendants(root);
    pids.reverse();
    if let Some(pid) = first_windowed(&pids) {
        return Some(pid);
    }
    let name = app?;
    let script = format!(
        r#"tell application "System Events"
  try
    set p to first process whose name is "{}"
    if (count of windows of p) > 0 then return (unix id of p) as text
  end try
end tell
return """#,
        as_str_literal(name)
    );
    osascript_out(&script).ok()?.trim().parse().ok()
}

/// Ask System Events, in one round trip, which of these pids owns a window.
fn first_windowed(pids: &[u32]) -> Option<u32> {
    if pids.is_empty() {
        return None;
    }
    let out = osascript_out(&windowed_script(pids)).ok()?;
    out.trim().parse().ok()
}

/// The pid probe. Index form throughout (`item k of`) for the same reason the
/// pane scripts use it: `repeat with x in list` hands back references, and a
/// reference compared against a number is a type error at runtime, not at
/// compile time.
fn windowed_script(pids: &[u32]) -> String {
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"set cands to {{{list}}}
tell application "System Events"
  repeat with k from 1 to (count of cands)
    set thisPid to item k of cands
    try
      set p to first process whose unix id is thisPid
      if (count of windows of p) > 0 then return thisPid as text
    end try
  end repeat
end tell
return """#
    )
}

/// Put the window where the layout says, and raise it once.
fn place(pid: u32, r: Rect) -> Result<(), String> {
    let script = format!(
        r#"tell application "System Events"
  try
    set p to first process whose unix id is {pid}
    set w to window 1 of p
    set position of w to {{{x}, {y}}}
    set size of w to {{{width}, {height}}}
    perform action "AXRaise" of w
  on error errMsg
    return "ERR " & errMsg
  end try
end tell
return "OK""#,
        pid = pid,
        x = r.x,
        y = r.y,
        width = r.w,
        height = r.h
    );
    match osascript_out(&script)?.trim() {
        "OK" => Ok(()),
        other => Err(other.to_string()),
    }
}

/// Reorder the window to the front *without* activating its application, so the
/// app stops being buried by a click into a pane while the keyboard stays in the
/// pane. Verified behaviour, not a hope: raising another app's window leaves
/// iTerm2 frontmost.
fn raise(pid: u32) -> Result<(), String> {
    let script = format!(
        r#"tell application "System Events"
  try
    set p to first process whose unix id is {pid}
    perform action "AXRaise" of window 1 of p
  end try
end tell
return "OK""#
    );
    osascript_out(&script).map(|_| ())
}

/// The bounds of the iTerm2 window this pane lives in.
///
/// Read live rather than baked in at launch, so a moved or resized workspace
/// window re-docks the app correctly the next time it starts.
fn iterm_window_bounds() -> Option<Rect> {
    let raw = std::env::var("ITERM_SESSION_ID").ok()?;
    let uuid = raw.rsplit_once(':').map(|(_, u)| u).unwrap_or(&raw).trim();
    if uuid.is_empty() {
        return None;
    }
    let out = osascript_out(&bounds_script(uuid)).ok()?;
    let nums: Vec<i32> = out
        .trim()
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    match nums[..] {
        [l, t, r, b] => Some(Rect {
            x: l,
            y: t,
            w: r - l,
            h: b - t,
        }),
        _ => None,
    }
}

/// Find the window holding a given session and answer its bounds as
/// `left,top,right,bottom`.
fn bounds_script(session_uuid: &str) -> String {
    let me = as_str_literal(session_uuid);
    format!(
        r#"tell application "iTerm2"
  repeat with wi from 1 to (count of windows)
    set thisWin to item wi of windows
    set ts to tabs of thisWin
    repeat with ti from 1 to (count of ts)
      set ss to sessions of (item ti of ts)
      repeat with k from 1 to (count of ss)
        if (id of (item k of ss)) is "{me}" then
          set b to bounds of thisWin
          return ((item 1 of b) as text) & "," & ((item 2 of b) as text) & "," & ((item 3 of b) as text) & "," & ((item 4 of b) as text)
        end if
      end repeat
    end repeat
  end repeat
end tell
return """#
    )
}

/// Escape a Rust string into an AppleScript string literal body.
fn as_str_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The same, in JavaScript for Automation -- the one scripting language on a
/// stock Mac with a live ObjC bridge, which is what `visible_frame` needs.
fn osascript_js_out(script: &str) -> Result<String, String> {
    run_osascript(&["-l", "JavaScript"], script)
}

/// Run a script and hand back stdout. Never prints, never exits: the caller is
/// either a background thread or a pane that is showing an application's log.
fn osascript_out(script: &str) -> Result<String, String> {
    run_osascript(&[], script)
}

fn run_osascript(args: &[&str], script: &str) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new("osascript")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_without_the_conf_declares_no_gui() {
        // The isolation contract: no file, no GUI, and every caller upstream
        // takes the path it took before this module existed.
        assert!(config(Path::new("/definitely/not/a/repo")).is_none());
    }

    #[test]
    fn parse_reads_the_five_keys_and_ignores_the_rest() {
        let g = parse(
            "# barnes-hut\ncmd = ./run.sh\nkeep = app\nrect = 0.25,0,0.5,0.75\napp = stella-nova\npoll = 400\nnonsense = 3\n",
        )
        .unwrap();
        assert_eq!(g.cmd, "./run.sh");
        assert_eq!(g.keep, Keep::App);
        assert_eq!(g.app.as_deref(), Some("stella-nova"));
        assert_eq!(g.rect, (0.25, 0.0, 0.5, 0.75)); // an override, not the default
        assert_eq!(g.poll_ms, 400);
    }

    #[test]
    fn parse_without_a_command_is_not_a_gui() {
        // A config that declares a keep policy but nothing to launch would open
        // a workspace with a hole in it and nothing to put in the hole.
        assert!(parse("keep = raise\n").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn parse_defaults_are_the_ones_the_layout_can_actually_hit() {
        let g = parse("cmd = ./run.sh").unwrap();
        assert_eq!(g.keep, Keep::Raise); // works on an app you have not modified
        assert_eq!(g.rect, DEFAULT_RECT); // the halving-split boundaries
    }

    #[test]
    fn rect_lands_on_the_middle_column_of_three() {
        // The pane grid puts its edges on exact thirds, so the app's rect has to
        // agree with those thirds -- a rect that disagrees shows a sliver of a
        // hidden pane down one side, or covers a strip of the Eye.
        let win = Rect { x: 0, y: 30, w: 1920, h: 1050 };
        let r = rect(win, DEFAULT_RECT);
        assert_eq!(r.x, 640); // the middle column starts a third across
        assert_eq!(r.w, 640); // and is a third wide
        assert_eq!(r.y, 30);
        assert_eq!(r.h, 700); // two of the middle column's three rows
    }

    #[test]
    fn rect_is_relative_to_the_window_not_the_screen() {
        // A workspace window dragged to a second display still docks its app
        // into itself rather than onto the display it was launched from.
        let win = Rect { x: 1512, y: 30, w: 1920, h: 1050 };
        let r = rect(win, DEFAULT_RECT);
        assert_eq!(r.x, 1512 + 640);
        assert_eq!(r.w, 640);
    }

    #[test]
    fn stage_command_is_short_enough_to_read_before_pressing_enter() {
        let c = stage_command(Path::new("/bin/sauron"), "/repo");
        assert_eq!(c, "cd /repo && /bin/sauron gui");
        // It sits inside an AppleScript double-quoted literal in the layout.
        assert!(!c.contains('"') && !c.contains('\''));
    }

    #[test]
    fn windowed_script_probes_every_candidate_by_index() {
        let s = windowed_script(&[100, 200, 300]);
        assert!(s.contains("set cands to {100, 200, 300}"));
        assert!(s.contains("item k of cands"));
        // `repeat with x in list` yields references; comparing one against a
        // unix id fails at runtime. Index form only, as everywhere else.
        assert!(!s.contains("repeat with thisPid in"));
    }

    #[test]
    fn place_sets_position_size_and_raises_in_one_round_trip() {
        let s = place_script_for_test(4242, Rect { x: 480, y: 25, w: 960, h: 791 });
        assert!(s.contains("first process whose unix id is 4242"));
        assert!(s.contains("set position of w to {480, 25}"));
        assert!(s.contains("set size of w to {960, 791}"));
        assert!(s.contains(r#"perform action "AXRaise" of w"#));
    }

    /// `place` builds and runs its script in one function; this mirrors the
    /// string so the shape stays asserted without shelling out in a test.
    fn place_script_for_test(pid: u32, r: Rect) -> String {
        format!(
            r#"tell application "System Events"
  try
    set p to first process whose unix id is {pid}
    set w to window 1 of p
    set position of w to {{{x}, {y}}}
    set size of w to {{{width}, {height}}}
    perform action "AXRaise" of w
  on error errMsg
    return "ERR " & errMsg
  end try
end tell
return "OK""#,
            pid = pid,
            x = r.x,
            y = r.y,
            width = r.w,
            height = r.h
        )
    }

    #[test]
    fn descendants_include_the_root_itself() {
        // `sh -c ./run.sh` may exec straight into the binary, in which case the
        // window's owner is the pid we already have.
        let me = std::process::id();
        assert!(descendants(me).contains(&me));
    }

    #[test]
    fn bounds_script_matches_the_session_by_id() {
        let s = bounds_script("UUID-1");
        assert!(s.contains(r#"if (id of (item k of ss)) is "UUID-1""#));
        assert!(s.contains("set b to bounds of thisWin"));
    }
}
