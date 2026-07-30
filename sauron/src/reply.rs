//! Talking back: a reply typed somewhere else, delivered into the agent's own
//! live terminal.
//!
//! The beacon is one-way. sauron watches logs and publishes what it sees, and
//! everything downstream of it -- the TUI, the in-app pane -- is a reader. That
//! is fine right up until the reader notices something the agent needs to know,
//! at which point the only route back is to find the right terminal window
//! yourself and type into it. For a pane floating over a fullscreen game, that
//! means leaving the thing you were looking at in order to describe it.
//!
//! This is the return path. A reader drops a message in
//! `~/.claude/sauron/outbox/`, sauron picks it up on the next TICK and types it
//! into the session's terminal exactly as if you had typed it there.
//!
//! WHY THE LIVE TERMINAL AND NOT A HEADLESS TURN
//! ---------------------------------------------
//! `claude --resume <id> -p "..."` would also work and would need no AppleScript
//! at all. It is the wrong answer here: it runs the turn detached, so the agent
//! you are talking to is not the one you are watching, and its output lands in a
//! log rather than on your screen. Delivering into the live session means the
//! reply is just more text in the conversation already in front of you -- the
//! agent picks it up and keeps going while you carry on elsewhere.
//!
//! HOW A SESSION ID BECOMES A TERMINAL
//! -----------------------------------
//! Three hops, none of which require the pane to have been launched by
//! `sauron workspace`:
//!
//!   1. `ps` for the agent process running that session id -> its pid
//!   2. the pid's controlling terminal -> `/dev/ttysNNN`
//!   3. the iTerm2 session whose `tty` matches -> `write text`
//!
//! Hop 3 is why this is iTerm2-specific and macOS-only. iTerm exposes `tty` per
//! session, which is the only reliable join between "a process I found with ps"
//! and "a window I can type into" -- TIOCSTI, the portable way to shove
//! characters at a terminal, has been disabled on macOS for years and is a
//! security hole everywhere it still exists.
//!
//! A session with no live process is not an error and not deliverable: the
//! message stays in the outbox for a later TICK, because an agent you replied
//! to ten seconds before it was resumed should still hear you.
//!
//! grep targets:
//!   fn drain          -- called each TICK; the whole outbox in one pass
//!   fn deliver        -- one message, all three hops
//!   fn pid_for        -- session id -> pid, via ps
//!   fn tty_for        -- pid -> /dev/ttysNNN
//!   fn write_to_tty   -- the AppleScript that types it
//!   fn queue          -- the writer half, for readers linking this crate
//!   fn outbox_dir     -- where messages live
//!   const MAX_AGE_MS  -- how long an undeliverable message keeps trying

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::now_ms;
use crate::scan::home;

/// How long a message that cannot be delivered keeps trying before it is
/// dropped.
///
/// Ten minutes, because the common undeliverable case is "that agent's terminal
/// is not open right now" and the common fix is opening it. Retrying forever
/// would mean a reply written on Monday arriving in the middle of Thursday's
/// unrelated conversation, which is worse than losing it.
const MAX_AGE_MS: i64 = 10 * 60 * 1000;

/// Messages waiting to be delivered.
///
/// In sauron's own state dir, never the watched repo -- same invariant as the
/// beacon. A reader that can find the beacon can find this by construction,
/// which is what lets the in-app pane write here with no extra configuration.
pub fn outbox_dir() -> PathBuf {
    home().join(".claude").join("sauron").join("outbox")
}

/// One queued message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    /// What to do: `reply` types the text at the agent, `ack` marks its
    /// write-set tested.
    ///
    /// Acking rides this channel rather than having the reader write
    /// `acks.json` directly, because the ack file is JSON and a reader that
    /// must not take a JSON dependency to speak the beacon must not need one to
    /// answer it either. One queue, one format, two verbs.
    pub kind: String,
    /// The session to talk to. Full id, not the short form the pane displays --
    /// the short form is for eyes and collides eventually.
    pub session: String,
    /// What to say. Newlines are collapsed on delivery, not here, so the file
    /// stays readable if somebody opens it while debugging.
    pub text: String,
    /// Absolute path to an image to reference, if any.
    pub attach: String,
    /// When it was queued, for the age check.
    pub queued_ms: i64,
}

// ───────────────────────────────────────────────────────────────────────────
//  Wire format
// ───────────────────────────────────────────────────────────────────────────
//
//  The beacon's format, for the beacon's reason: a reader or writer must not
//  need a JSON dependency to speak it. Tab-separated, one field per line, `\`
//  `\t` `\n` escaped in free text.
//
//      session  4dcde696-4bca-4371-98cb-cd9edefc9157
//      queued   1785113886015
//      attach   /tmp/sauron-shot-1785113886.png
//      text     the flag clips through the hull on the port side
//      end

pub fn render(m: &Message) -> String {
    let mut s = String::new();
    s.push_str(&format!("kind\t{}\n", esc(m.kind_or_default())));
    s.push_str(&format!("session\t{}\n", esc(&m.session)));
    s.push_str(&format!("queued\t{}\n", m.queued_ms));
    if !m.attach.is_empty() {
        s.push_str(&format!("attach\t{}\n", esc(&m.attach)));
    }
    s.push_str(&format!("text\t{}\n", esc(&m.text)));
    s.push_str("end\n");
    s
}

pub fn parse(text: &str) -> Option<Message> {
    let mut m = Message::default();
    let mut sealed = false;
    for line in text.lines() {
        if line == "end" {
            sealed = true;
            break;
        }
        let Some((tag, rest)) = line.split_once('\t') else {
            continue;
        };
        match tag {
            "kind" => m.kind = unesc(rest),
            "session" => m.session = unesc(rest),
            "queued" => m.queued_ms = rest.parse().unwrap_or(0),
            "attach" => m.attach = unesc(rest),
            "text" => m.text = unesc(rest),
            _ => {}
        }
    }
    // Same torn-read guard as the beacon. A half-written message delivered as
    // a truncated sentence would be worse than one delivered late.
    let says_something = m.kind_or_default() == "ack" || !m.text.is_empty();
    (sealed && !m.session.is_empty() && says_something).then_some(m)
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

impl Message {
    /// The verb, defaulting to `reply` for a message that predates the field.
    pub fn kind_or_default(&self) -> &str {
        if self.kind.is_empty() { "reply" } else { &self.kind }
    }
}

// ───────────────────────────────────────────────────────────────────────────
//  Writing
// ───────────────────────────────────────────────────────────────────────────

/// Queue a message. Written to a temp name and renamed, so a TICK that lands
/// mid-write reads nothing rather than half a sentence.
///
///   grep -n "fn queue"  src/reply.rs
pub fn queue(m: &Message) -> std::io::Result<PathBuf> {
    let dir = outbox_dir();
    std::fs::create_dir_all(&dir)?;
    // Name from the clock plus the session, which is unique enough for a queue
    // that drains every two seconds and keeps the directory readable.
    let stem = format!("{}-{}", m.queued_ms, m.session.split('-').next().unwrap_or("x"));
    let tmp = dir.join(format!(".{stem}.tmp"));
    let final_path = dir.join(format!("{stem}.msg"));
    std::fs::write(&tmp, render(m))?;
    std::fs::rename(&tmp, &final_path)?;
    Ok(final_path)
}

// ───────────────────────────────────────────────────────────────────────────
//  Delivering
// ───────────────────────────────────────────────────────────────────────────

/// Deliver everything deliverable. Called once per TICK.
///
/// Returns the session ids of any queued ACKS, for the caller to apply.
///
/// Acks are handed back rather than performed here because the caller already
/// owns a `Board`, and `Board::ack` is the path the TUI's own `a` key takes --
/// it resolves the write-set, defers a waiting state, and journals the op. A
/// second implementation reading `acks.json` directly would be a second set of
/// rules about what acking means, and the two would drift.
///
/// Replies that cannot be delivered are not errors: the common case is "that
/// terminal is not open", which is a state, not a fault, so the message is left
/// for a later pass.
///
///   grep -n "fn drain"  src/reply.rs
pub fn drain() -> Vec<String> {
    let dir = outbox_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let now = now_ms();
    let mut acks = Vec::new();

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "msg"))
        .collect();
    // Oldest first, so a conversation queued in order arrives in order.
    paths.sort();

    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(m) = parse(&text) else {
            // Unparseable: not going to improve on the next pass.
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if m.queued_ms > 0 && now.saturating_sub(m.queued_ms) > MAX_AGE_MS {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if m.kind_or_default() == "ack" {
            // No terminal and no live process needed: an ack is a note about
            // work already done, so a session that has since exited is the
            // normal case rather than a failure. Always consumed.
            acks.push(m.session.clone());
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if deliver(&m).is_ok() {
            let _ = std::fs::remove_file(&path);
        }
    }
    acks
}

/// One message, all three hops.
///
///   grep -n "fn deliver"  src/reply.rs
pub fn deliver(m: &Message) -> Result<(), String> {
    let pid = pid_for(&m.session).ok_or("no live process for that session")?;
    let tty = tty_for(pid).ok_or("process has no controlling terminal")?;
    write_to_tty(&tty, &line_for(m))
}

/// The message as one line.
///
/// Collapsed, because `write text` delivers a line and the agent's prompt reads
/// a line: a newline mid-message would submit half of it and leave the rest
/// sitting in the box. The attachment is appended as a plain path, which is
/// what an agent needs in order to go and look at it.
fn line_for(m: &Message) -> String {
    let mut s = collapse(&m.text);
    if !m.attach.is_empty() {
        s.push_str(&format!(" (screenshot: {})", m.attach));
    }
    s
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The pid of the agent process running `session`, if one is live.
///
/// MATCHED ON THE RESUME COMMAND, NOT ON THE ID APPEARING SOMEWHERE
/// ---------------------------------------------------------------
/// The obvious implementation -- find a line containing the session id -- is
/// wrong in a way that only shows up in a live process table, and it is not a
/// near miss. Anything that merely mentions the id wins: a `grep` for it, an
/// editor with the log open, a shell wrapper whose argv quotes a command that
/// contains it, or this very process. The first version of this function
/// resolved a real session to a `/bin/zsh -c ...` wrapper, and had it been
/// allowed to deliver, it would have typed the reply into a shell.
///
/// So the line must actually look like an agent resuming that session: the
/// program's basename is `claude` or `codex`, and the id follows the subcommand
/// that takes it. Anything else is not an agent and must not be typed at.
///
/// A session started fresh rather than resumed does not carry its id in argv
/// and is therefore not addressable this way. That is a real limitation, and
/// the reason `deliver` reports failure rather than falling back to a guess --
/// a wrong window is worse than no delivery.
///
///   grep -n "fn pid_for"  src/reply.rs
fn pid_for(session: &str) -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-Ao", "pid=,command="])
        .output()
        .ok()?;
    let me = std::process::id();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let (pid, cmd) = l.split_once(char::is_whitespace)?;
            let pid: u32 = pid.parse().ok()?;
            (pid != me).then_some((pid, cmd.trim()))
        })
        .find(|(_, cmd)| is_resume_of(cmd, session))
        .map(|(pid, _)| pid)
}

/// Whether `cmd` is an agent CLI resuming `session`, rather than any old
/// process that happens to mention it.
///
///   grep -n "fn is_resume_of"  src/reply.rs
fn is_resume_of(cmd: &str, session: &str) -> bool {
    let mut words = cmd.split_whitespace();
    let Some(prog) = words.next() else { return false };
    // Basename, so an absolute path to the binary still matches.
    let prog = prog.rsplit('/').next().unwrap_or(prog);

    let rest: Vec<&str> = words.collect();
    // The id must be the argument OF the resume flag/subcommand, not merely
    // present somewhere later in a long quoted command.
    let follows = |kw: &str| {
        rest.windows(2)
            .any(|w| w[0] == kw && w[1] == session)
    };
    match prog {
        "claude" => follows("--resume") || follows("-r"),
        "codex" => follows("resume"),
        _ => false,
    }
}

/// The controlling terminal of a pid, as an absolute device path.
///
///   grep -n "fn tty_for"  src/reply.rs
fn tty_for(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // `??` is ps for "no controlling terminal" -- a daemonised agent, which
    // there is no window to type into.
    (!t.is_empty() && t != "??").then(|| format!("/dev/{t}"))
}

/// Type `text` into the iTerm2 session attached to `tty`.
///
/// `write text` is iTerm's own "as if typed, then Enter", so the agent receives
/// it through exactly the path a human keystroke takes -- no special-casing in
/// the agent, and no assumption about what its prompt looks like.
///
///   grep -n "fn write_to_tty"  src/reply.rs
fn write_to_tty(tty: &str, text: &str) -> Result<(), String> {
    let script = format!(
        r#"tell application "iTerm2"
  repeat with w in windows
    repeat with t in tabs of w
      repeat with s in sessions of t
        if tty of s is "{tty}" then
          tell s to write text "{text}"
          return "ok"
        end if
      end repeat
    end repeat
  end repeat
  return "notfound"
end tell"#,
        tty = as_applescript(tty),
        text = as_applescript(text),
    );

    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    let reply = String::from_utf8_lossy(&out.stdout);
    if reply.trim() == "ok" {
        Ok(())
    } else if reply.trim() == "notfound" {
        Err(format!("no iTerm2 session on {tty}"))
    } else {
        Err(format!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The name of the iTerm2 session attached to `tty`, if there is one.
///
/// Read-only counterpart to `write_to_tty`, used by `--where`. Same traversal,
/// so a successful lookup here is real evidence that a delivery would land --
/// which is the only way to test this plumbing without typing into a live
/// conversation.
///
///   grep -n "fn iterm_name_for"  src/reply.rs
fn iterm_name_for(tty: &str) -> Option<String> {
    let script = format!(
        r#"tell application "iTerm2"
  repeat with w in windows
    repeat with t in tabs of w
      repeat with s in sessions of t
        if tty of s is "{tty}" then return name of s
      end repeat
    end repeat
  end repeat
  return ""
end tell"#,
        tty = as_applescript(tty),
    );
    let out = std::process::Command::new("osascript").arg("-e").arg(&script).output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Escape for embedding inside an AppleScript double-quoted literal.
///
/// Backslash first, or escaping the quotes would then escape their own
/// backslashes. The same ordering bug the beacon's `esc` documents, and the
/// same fix.
fn as_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ───────────────────────────────────────────────────────────────────────────
//  Acknowledging
// ───────────────────────────────────────────────────────────────────────────

/// Ack a session's whole outstanding write-set from outside the TUI.
///
/// `store.rs` already treats acks as something another process may make -- the
/// board re-reads them every TICK for exactly that reason -- so this needs no
/// new mechanism, only a caller.
///
///   grep -n "fn ack_session"  src/reply.rs
pub fn ack_session(id: &str, edits: &BTreeMap<String, i64>) -> std::io::Result<()> {
    let mut store = crate::store::AckStore::load();
    store.ack(id, edits);
    store.save()
}



// ───────────────────────────────────────────────────────────────────────────
//  CLI
// ───────────────────────────────────────────────────────────────────────────

const USAGE: &str = "\
sauron reply -- say something to a running agent, in its own terminal

  sauron reply <session-id> <text>...
      Deliver now. Finds the agent's process, its terminal, and the iTerm2
      session attached to it, then types the text there as if you had.

  sauron reply --queue <session-id> <text>...
      Drop it in the outbox instead; the running sauron delivers it on its
      next tick. This is what the in-app pane does.

  sauron reply --drain
      Deliver everything queued, once, and report the count.

  sauron reply --list
      Show what is waiting.

  sauron reply --where <session-id>
      Resolve the session to a pid, a terminal and an iTerm2 session, and print
      each hop -- WITHOUT delivering anything. The way to check the plumbing
      against a live agent without typing into somebody's conversation.
";

pub fn run(args: &[String]) -> std::io::Result<()> {
    match args.first().map(|s| s.as_str()) {
        None | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("--drain") => {
            let acks = drain();
            println!("drained; {} ack(s) pending for the board", acks.len());
            Ok(())
        }
        Some("--list") => {
            let dir = outbox_dir();
            let Ok(entries) = std::fs::read_dir(&dir) else {
                println!("outbox is empty ({})", dir.display());
                return Ok(());
            };
            let mut any = false;
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "msg") {
                    continue;
                }
                if let Some(m) = std::fs::read_to_string(&p).ok().as_deref().and_then(parse) {
                    any = true;
                    println!("  {} -> {}", &m.session[..8.min(m.session.len())], collapse(&m.text));
                }
            }
            if !any {
                println!("outbox is empty ({})", dir.display());
            }
            Ok(())
        }
        Some("--where") => {
            let Some(id) = args.get(1) else {
                print!("{USAGE}");
                std::process::exit(2);
            };
            match pid_for(id) {
                None => println!("  no live process running session {id}"),
                Some(pid) => {
                    println!("  pid   {pid}");
                    match tty_for(pid) {
                        None => println!("  tty   none (no controlling terminal)"),
                        Some(tty) => {
                            println!("  tty   {tty}");
                            match iterm_name_for(&tty) {
                                Some(n) => println!("  iterm {n}  -- deliverable"),
                                None => println!("  iterm no session on that tty -- NOT deliverable"),
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        Some("--queue") => {
            if args.len() < 3 {
                print!("{USAGE}");
                std::process::exit(2);
            }
            let m = Message {
                kind: "reply".into(),
                session: args[1].clone(),
                text: args[2..].join(" "),
                attach: String::new(),
                queued_ms: now_ms(),
            };
            let p = queue(&m)?;
            println!("queued {}", p.display());
            Ok(())
        }
        Some(session) => {
            if args.len() < 2 {
                print!("{USAGE}");
                std::process::exit(2);
            }
            let m = Message {
                kind: "reply".into(),
                session: session.to_string(),
                text: args[1..].join(" "),
                attach: String::new(),
                queued_ms: now_ms(),
            };
            match deliver(&m) {
                Ok(()) => {
                    println!("delivered to {session}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("sauron reply: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
//  Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Message {
        Message {
            kind: "reply".into(),
            session: "4dcde696-4bca-4371-98cb-cd9edefc9157".into(),
            text: "the flag\tclips\nthrough the hull".into(),
            attach: "/tmp/shot.png".into(),
            queued_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn round_trips_through_the_wire_format() {
        let m = sample();
        let back = parse(&render(&m)).expect("parses");
        assert_eq!(back, m);
        // The separators must survive as themselves, or a message containing a
        // tab would silently become two fields on re-read.
        assert!(back.text.contains('\t'));
        assert!(back.text.contains('\n'));
    }

    #[test]
    fn rejects_an_unsealed_message() {
        let torn = render(&sample()).replace("end\n", "");
        assert!(parse(&torn).is_none());
    }

    #[test]
    fn rejects_a_message_with_nothing_to_say() {
        let empty = render(&Message { text: String::new(), ..sample() });
        assert!(parse(&empty).is_none());
    }

    #[test]
    fn the_delivered_format_is_pinned_because_the_pane_mirrors_it() {
        // `sauron_panel.rs.in::delivered_line` reproduces this exactly, so the
        // pane can show you what was actually typed at the agent rather than
        // what you typed into the box -- the two differ, because the editor is
        // multiline and the prompt reads a line. Change the format here and the
        // pane will echo a message that was never sent.
        //   grep -n "fn delivered_line"  assets/sauron_panel.rs.in
        let m = Message {
            text: "one\n\ntwo   three".into(),
            attach: "/tmp/a.png".into(),
            ..sample()
        };
        assert_eq!(line_for(&m), "one two three (screenshot: /tmp/a.png)");

        let bare = Message { attach: String::new(), ..m };
        assert_eq!(line_for(&bare), "one two three");
    }

    #[test]
    fn delivered_line_is_one_line() {
        // A newline mid-message would submit half of it at the agent's prompt
        // and leave the rest in the box.
        let line = line_for(&sample());
        assert!(!line.contains('\n'));
        assert!(!line.contains('\t'));
        assert!(line.contains("/tmp/shot.png"));
    }

    #[test]
    fn applescript_escaping_does_not_double_back() {
        // Backslash must be escaped before quotes, or the quote's own escape
        // gets escaped and the literal ends early.
        assert_eq!(as_applescript(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn only_a_real_resume_command_is_a_delivery_target() {
        let id = "4dcde696-4bca-4371-98cb-cd9edefc9157";

        assert!(is_resume_of(&format!("claude --resume {id}"), id));
        assert!(is_resume_of(&format!("/usr/local/bin/claude --resume {id}"), id));
        assert!(is_resume_of(&format!("codex resume {id}"), id));

        // The regression this exists for. A shell wrapper quoting a command
        // that mentions the id resolved as the agent itself, and delivering to
        // it would have typed the reply into a shell.
        assert!(!is_resume_of(&format!("/bin/zsh -c ps -Ao pid= | grep {id}"), id));
        assert!(!is_resume_of(&format!("grep {id}"), id));
        assert!(!is_resume_of(&format!("sauron reply --where {id}"), id));
        // Right program, but the id is not what it is resuming.
        assert!(!is_resume_of(&format!("claude --resume other-id --add-dir {id}"), id));
        assert!(!is_resume_of("claude", id));
    }

    #[test]
    fn a_missing_outbox_drains_to_nothing() {
        // Not an error: it is the state on a machine where nobody has replied.
        if std::fs::read_dir(outbox_dir()).is_err() {
            assert!(drain().is_empty());
        }
    }

    #[test]
    fn an_ack_needs_no_text_but_a_reply_does() {
        // An ack is a verb about a whole card; a reply with nothing in it is a
        // dropped keystroke, not a message.
        let ack = Message { kind: "ack".into(), text: String::new(), ..sample() };
        assert!(parse(&render(&ack)).is_some());
        let empty_reply = Message { kind: "reply".into(), text: String::new(), ..sample() };
        assert!(parse(&render(&empty_reply)).is_none());
    }

    #[test]
    fn a_message_without_a_kind_reads_as_a_reply() {
        // Forward compatibility with anything queued before the field existed.
        let m = Message { kind: String::new(), ..sample() };
        assert_eq!(m.kind_or_default(), "reply");
    }

}
