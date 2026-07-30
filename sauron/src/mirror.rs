//! `sauron gui --mirror` -- a live picture of a GUI window, drawn inside a pane.
//!
//! The other half of `gui` docks a real window into a hole in the pane grid. This
//! half answers the different question -- *put it in the terminal* -- and the
//! answer is forced by what a pane is. A pane is a pty. The only thing that can
//! enter one is bytes. So an application can appear inside a pane in exactly one
//! way: capture what it is drawing and write the picture, frame after frame,
//! through iTerm2's inline-image protocol.
//!
//! What that buys, and what it costs, stated up front because both are inherent:
//!
//! * It is **a mirror, not the app**. Keys and clicks go to the pane, not to the
//!   program. Nothing here can change that -- there is no channel back.
//! * It is **paced in frames per second**, not in the app's own refresh. Every
//!   frame is a capture, a rescale, and a base64 blob down the pty.
//! * It **does not need the window to be visible**. Capture is by window id off
//!   the window server, so the app can sit behind the terminal, half off the
//!   screen, or under another window, and the mirror still shows it. That is the
//!   property that makes this worth having rather than just moving the window.
//!
//! grep targets:
//!   fn run           -- the command: find the window, split the pane, stream
//!   fn candidates    -- every mirrorable window, off the window server
//!   fn pick          -- name -> the one window to mirror (largest match wins)
//!   fn split_right   -- shove a shell into a new right pane, keep this one
//!   fn frame         -- one capture -> one inline image
//!   fn base64        -- shared with main.rs's OSC 52 clipboard write

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A window the mirror could show: what the window server knows about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Win {
    pub id: u32,
    pub owner: String,
    pub title: String,
    pub w: u32,
    pub h: u32,
}

/// Entry point for `sauron gui --mirror <args>`.
pub fn run(args: &[String], repo: &std::path::Path, conf_app: Option<&str>) -> std::io::Result<()> {
    let mut app: Option<String> = conf_app.map(str::to_string);
    let mut fps: u64 = 8;
    let mut px: u32 = 1200;
    let mut split = true;
    let mut probe = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--app" => {
                app = args.get(i + 1).cloned();
                i += 2;
            }
            a if a.starts_with("--app=") => {
                app = Some(a["--app=".len()..].to_string());
                i += 1;
            }
            "--fps" => {
                fps = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(8);
                i += 2;
            }
            a if a.starts_with("--fps=") => {
                fps = a["--fps=".len()..].parse().unwrap_or(8);
                i += 1;
            }
            "--px" => {
                px = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(1200);
                i += 2;
            }
            a if a.starts_with("--px=") => {
                px = a["--px=".len()..].parse().unwrap_or(1200);
                i += 1;
            }
            "--no-split" => {
                split = false;
                i += 1;
            }
            "--probe" => {
                probe = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    let fps = fps.clamp(1, 30);

    let found = candidates();
    if found.is_empty() {
        eprintln!("sauron gui --mirror: the window server listed no windows.");
        eprintln!("  Screen Recording permission is required to read other apps' windows:");
        eprintln!("  System Settings > Privacy & Security > Screen Recording > iTerm.");
        std::process::exit(2);
    }
    let Some(target) = pick(&found, app.as_deref()) else {
        eprintln!(
            "sauron gui --mirror: no window matching {:?}. Pass --app <name>, or set `app =` in .sauron/gui.conf.",
            app.unwrap_or_default()
        );
        eprintln!("  windows it can see:");
        for w in found.iter().take(20) {
            eprintln!("    {:<24} {}x{}  {}", w.owner, w.w, w.h, w.title);
        }
        std::process::exit(2);
    };

    if probe {
        let (cols, rows) = pane_cells().unwrap_or((80, 24));
        println!("WINDOW={} ({}) {}x{}", target.id, target.owner, target.w, target.h);
        println!("PANE={cols}x{rows} cells");
        println!("FPS={fps} PX={px}");
        let mut bytes = 0usize;
        let t0 = Instant::now();
        for _ in 0..3 {
            match capture(target.id, px) {
                Some(jpg) => bytes += jpg.len(),
                None => {
                    println!("CAPTURE=failed (Screen Recording permission?)");
                    return Ok(());
                }
            }
        }
        let per = t0.elapsed().as_secs_f64() / 3.0;
        println!(
            "CAPTURE={:.0}ms/frame  {:.0}KiB/frame  ceiling={:.1} fps  wire={:.1} MB/s at {fps} fps",
            per * 1000.0,
            bytes as f64 / 3.0 / 1024.0,
            1.0 / per,
            (bytes as f64 / 3.0 * 1.37) * fps as f64 / 1e6,
        );
        return Ok(());
    }

    // Split first, so the shell you typed this into keeps existing: the mirror
    // takes the pane it was launched in (the left one), and a fresh shell at the
    // repo goes to its right. `--no-split` is for a pane you already carved.
    if split {
        if let Err(e) = split_right(&repo.to_string_lossy()) {
            eprintln!("sauron gui --mirror: could not split ({e}); mirroring in place.");
        }
        // iTerm2 reflows the pane asynchronously, and the cell grid is read
        // next. Measuring before the reflow lands sizes every frame for the
        // pane's *old* width -- twice as wide as the pane it has to fit.
        std::thread::sleep(Duration::from_millis(400));
    }

    // Now that this pane is its final size, ask how many cells it has. Read
    // from iTerm2 rather than the tty, which may not have caught up either.
    let (cols, rows) = pane_cells().unwrap_or((80, 24));

    println!(
        "mirroring {} ({}x{}) at {fps} fps — ctrl-c to stop",
        target.owner, target.w, target.h
    );
    let interval = Duration::from_millis(1000 / fps);
    let mut out = std::io::stdout();
    loop {
        let t = Instant::now();
        if let Some(jpg) = capture(target.id, px) {
            let seq = frame(&jpg, cols, rows);
            // A dropped frame is not worth dying over -- the pane may be
            // scrolling, resizing, or gone.
            let _ = out.write_all(seq.as_bytes());
            let _ = out.flush();
        }
        if let Some(rest) = interval.checked_sub(t.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// One frame: home the cursor, then draw the image across the pane's cells.
///
/// Sized in *cells* rather than pixels so iTerm2 does the scaling on its own
/// terms -- it knows the pane's backing scale, and this does not.
pub fn frame(jpeg: &[u8], cols: u16, rows: u16) -> String {
    format!(
        "\x1b[H\x1b]1337;File=inline=1;width={cols};height={rows};preserveAspectRatio=1:{}\x07",
        base64(jpeg)
    )
}

/// Capture one window by id, scaled so the long edge is at most `px`.
///
/// `-l <id>` reads the window off the window server, which is why an occluded or
/// half-offscreen window still mirrors. `-o` drops the drop-shadow, which would
/// otherwise put a fat translucent border around every frame.
fn capture(id: u32, px: u32) -> Option<Vec<u8>> {
    let path = scratch_frame();
    let ok = Command::new("screencapture")
        .args(["-x", "-o", "-t", "jpg", &format!("-l{id}")])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    // Rescaling in `sips` rather than in here: a JPEG decoder would be the
    // largest dependency in the crate, and this is one process per frame either
    // way. Failure is not fatal -- send the full-size frame instead.
    let _ = Command::new("sips")
        .args(["-Z", &px.to_string()])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::fs::read(&path).ok()
}

/// Every window the window server will admit to, largest first.
pub fn candidates() -> Vec<Win> {
    // `CGWindowListCopyWindowInfo` hands back a raw CFArrayRef, and JXA will not
    // box that on its own -- without the cast the result has no `.count` and the
    // whole list silently reads as empty, which looks exactly like a denied
    // permission and is not one. Owner and bounds need no permission at all.
    const JS: &str = r#"ObjC.import('CoreGraphics');
ObjC.import('Foundation');
const info = ObjC.castRefToObject($.CGWindowListCopyWindowInfo(1 | 16, 0));
let out = [];
for (let i = 0; i < info.count; i++) {
  const w = info.objectAtIndex(i);
  if (ObjC.unwrap(w.objectForKey('kCGWindowLayer')) !== 0) continue;
  const b = ObjC.deepUnwrap(w.objectForKey('kCGWindowBounds'));
  if (!b || b.Width < 80 || b.Height < 80) continue;
  const owner = ObjC.unwrap(w.objectForKey('kCGWindowOwnerName')) || '';
  const name = ObjC.unwrap(w.objectForKey('kCGWindowName')) || '';
  const id = ObjC.unwrap(w.objectForKey('kCGWindowNumber'));
  out.push([id, Math.round(b.Width), Math.round(b.Height), owner, name].join('\t'));
}
out.join('\n')"#;
    let Ok(text) = osascript_js(JS) else {
        return Vec::new();
    };
    let mut v: Vec<Win> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(Win {
                id: f.next()?.trim().parse().ok()?,
                w: f.next()?.trim().parse().ok()?,
                h: f.next()?.trim().parse().ok()?,
                owner: f.next()?.to_string(),
                title: f.next().unwrap_or("").to_string(),
            })
        })
        .collect();
    v.sort_by_key(|w| std::cmp::Reverse(w.w as u64 * w.h as u64));
    v
}

/// Choose the window to mirror: the largest whose owner or title matches, or --
/// with no name given -- the largest window that is not a terminal.
///
/// The terminal exclusion is not politeness. Mirroring the window you are
/// drawing into feeds the pane its own picture, one frame stale, forever.
pub fn pick<'a>(found: &'a [Win], want: Option<&str>) -> Option<&'a Win> {
    match want {
        Some(name) if !name.is_empty() => {
            let needle = name.to_ascii_lowercase();
            found.iter().find(|w| {
                w.owner.to_ascii_lowercase().contains(&needle)
                    || w.title.to_ascii_lowercase().contains(&needle)
            })
        }
        _ => found.iter().find(|w| !is_terminal(&w.owner)),
    }
}

fn is_terminal(owner: &str) -> bool {
    matches!(
        owner.to_ascii_lowercase().as_str(),
        "iterm2" | "iterm" | "terminal" | "alacritty" | "kitty" | "wezterm" | "ghostty"
    )
}

/// Put a shell in a new pane to the right, leaving this pane -- the left one --
/// to the mirror. Answers `OK` or a message to print.
fn split_right(repo: &str) -> Result<(), String> {
    let raw = std::env::var("ITERM_SESSION_ID").map_err(|_| "not in an iTerm2 pane".to_string())?;
    let uuid = raw.rsplit_once(':').map(|(_, u)| u).unwrap_or(&raw).trim();
    let script = format!(
        r#"tell application "iTerm2"
  repeat with wi from 1 to (count of windows)
    set ts to tabs of (item wi of windows)
    repeat with ti from 1 to (count of ts)
      set ss to sessions of (item ti of ts)
      repeat with k from 1 to (count of ss)
        if (id of (item k of ss)) is "{uuid}" then
          tell (item k of ss) to set newP to (split vertically with default profile)
          tell newP to write text "cd {repo}"
          return "OK"
        end if
      end repeat
    end repeat
  end repeat
end tell
return "ERR this pane is not in an iTerm2 window""#
    );
    match osascript(&script)?.trim() {
        "OK" => Ok(()),
        other => Err(other.trim_start_matches("ERR ").to_string()),
    }
}

/// This pane's size in character cells, asked of iTerm2 rather than of the tty,
/// because it is read *after* the split and the tty may not have caught up.
fn pane_cells() -> Option<(u16, u16)> {
    let raw = std::env::var("ITERM_SESSION_ID").ok()?;
    let uuid = raw.rsplit_once(':').map(|(_, u)| u).unwrap_or(&raw).trim();
    let script = format!(
        r#"tell application "iTerm2"
  repeat with wi from 1 to (count of windows)
    set ts to tabs of (item wi of windows)
    repeat with ti from 1 to (count of ts)
      set ss to sessions of (item ti of ts)
      repeat with k from 1 to (count of ss)
        if (id of (item k of ss)) is "{uuid}" then
          return ((columns of (item k of ss)) as text) & "," & ((rows of (item k of ss)) as text)
        end if
      end repeat
    end repeat
  end repeat
end tell
return """#
    );
    let out = osascript(&script).ok()?;
    let n: Vec<u16> = out.trim().split(',').filter_map(|p| p.trim().parse().ok()).collect();
    match n[..] {
        [c, r] if c > 0 && r > 1 => Some((c, r - 1)), // leave the status line a row
        _ => None,
    }
}

/// The encoder moved to `plat` when the clipboard fallback -- which needs it on
/// every platform -- outlived this macOS-only module. Re-exported here because
/// `sauron::mirror::base64` was the public path and the mirror is still its
/// heaviest user, running it thousands of times.
pub use crate::plat::base64;

fn osascript(script: &str) -> Result<String, String> {
    run_impl(&[], script)
}

fn osascript_js(script: &str) -> Result<String, String> {
    run_impl(&["-l", "JavaScript"], script)
}

fn run_impl(args: &[&str], script: &str) -> Result<String, String> {
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

/// Where a frame is staged on its way from `screencapture` to the pty. One path
/// per process, reused every frame, so a long mirror session leaves one file.
fn scratch_frame() -> PathBuf {
    std::env::temp_dir().join(format!("sauron-mirror-{}.jpg", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, owner: &str, w: u32, h: u32) -> Win {
        Win { id, owner: owner.into(), title: String::new(), w, h }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_frame_is_an_inline_image_sized_in_cells() {
        let s = frame(b"xy", 40, 20);
        assert!(s.starts_with("\x1b[H"), "each frame homes the cursor first");
        assert!(s.contains("\x1b]1337;File=inline=1;width=40;height=20;preserveAspectRatio=1:"));
        assert!(s.ends_with('\x07'));
    }

    #[test]
    fn pick_prefers_the_named_window() {
        let found = vec![win(1, "iTerm2", 1920, 1080), win(2, "worldsmith", 1280, 720)];
        assert_eq!(pick(&found, Some("worldsmith")).unwrap().id, 2);
        assert_eq!(pick(&found, Some("WORLD")).unwrap().id, 2, "matching is case-insensitive");
        assert!(pick(&found, Some("nothing-like-this")).is_none());
    }

    /// Mirroring the terminal you are drawing into feeds the pane its own
    /// picture forever. With no name to go on, terminals are never the guess.
    #[test]
    fn pick_never_guesses_the_terminal_it_is_drawing_into() {
        let found = vec![win(1, "iTerm2", 1920, 1080), win(2, "stella-nova", 800, 600)];
        assert_eq!(pick(&found, None).unwrap().id, 2);
        // …but an explicit name is obeyed, because someone debugging the mirror
        // may well want exactly that.
        assert_eq!(pick(&found, Some("iTerm")).unwrap().id, 1);
    }

    #[test]
    fn pick_finds_nothing_when_only_terminals_are_open() {
        let found = vec![win(1, "iTerm2", 1920, 1080), win(2, "Terminal", 900, 600)];
        assert!(pick(&found, None).is_none());
    }
}
