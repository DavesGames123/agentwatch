//! Session model and time helpers.
//!
//! grep targets:
//!   struct Session          -- one Claude Code session, folded from its jsonl
//!   fn Session::status      -- Working / NeedsTest / Clear derivation
//!   fn Session::pending     -- write-set entries not yet acked at their current ts
//!   fn parse_rfc3339_ms     -- ISO8601 -> epoch millis, no chrono dependency
//!   fn ago                  -- epoch millis -> "4m" / "2h" / "3d"
//!   fn fmt_duration         -- a task's elapsed run -> "4m 12s" / "1h 03m"
//!   fn local_offset_secs    -- the machine's UTC offset, read once from `date`
//!   fn local_time / fmt_clock -- epoch millis -> local "3:42 PM"

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Escape hatch for a session that died mid-turn.
///
/// `turn_complete` is the real signal, but an agent killed between a `tool_use`
/// and its result never logs an `end_turn`, so it would read as Working forever
/// and its writes would never surface for testing. Past this much silence,
/// assume the turn will never finish.
pub const STUCK_AFTER_MS: i64 = 30 * 60 * 1000;

/// Sessions quieter than this with nothing pending are hidden entirely.
pub const DORMANT_AFTER_MS: i64 = 24 * 60 * 60 * 1000;

/// Untested writes older than this are collapsed behind a counter rather than
/// listed. Without it the first launch shows every session ever run against the
/// repo -- 44 of them here -- which reproduces the overload this tool exists to
/// remove. Toggle with `o`, clear permanently with `--baseline`.
pub const STALE_HORIZON_MS: i64 = 12 * 60 * 60 * 1000;

/// A session with an unresolved tool call and no log activity for this long is
/// treated as wanting a human.
///
/// Claude Code never logs a permission prompt -- "Do you want to proceed?" is
/// live UI state that only reaches the log once answered -- so the sole
/// observable is a `tool_use` with no `tool_result` and a silent log. A long
/// `cargo build` produces the same shape, so this deliberately over-reports:
/// a false "check this one" costs a glance, while a false "still working" hides
/// an agent parked on an approval indefinitely, which is the failure that
/// actually wastes the day.
pub const STALL_AFTER_MS: i64 = 45_000;

/// A turn that ended this recently is treated as "the agent just handed the
/// conversation back and is idle at the prompt". Past this, a finished session
/// is old news, not something waiting on you -- which is what keeps yesterday's
/// completed sessions out of the attention band.
pub const RECENT_STOP_MS: i64 = 20 * 60 * 1000;

/// A phrase every orc's launch prompt carries, so sauron can recognise its own
/// maintenance agents in the session list and mark them distinct. Both the orc
/// prompt (`workspace::orc_command`) and the log folds match on this.
pub const ORC_MARKER: &str = "no other agent is touching it";

/// Why a session is waiting on the user, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BlockedReason {
    /// AskUserQuestion / ExitPlanMode with no result: certain.
    Question,
    /// Unresolved tool call plus a silent log: probable permission prompt, but
    /// indistinguishable from a genuinely slow command.
    MaybeApproval,
    /// Turn ended and the agent is idle at the prompt. The single most common
    /// "waiting on you": an agent that asked something in prose, or offered a
    /// plan, or simply finished and wants the next instruction, all end the turn
    /// the same way. Cannot be told apart from a clean completion in the log, so
    /// this deliberately surfaces both -- a glance settles which.
    AwaitingInput,
}

impl BlockedReason {
    pub fn detail(self) -> &'static str {
        match self {
            BlockedReason::Question => {
                "agent asked a question and is stopped until you answer"
            }
            BlockedReason::MaybeApproval => {
                "tool call unresolved and log quiet — likely a permission prompt, or a slow command"
            }
            BlockedReason::AwaitingInput => {
                "agent ended its turn and is idle — waiting for your reply, or finished and wants the next step"
            }
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            BlockedReason::Question => "asked you a question",
            BlockedReason::MaybeApproval => "may need approval",
            BlockedReason::AwaitingInput => "stopped — your move",
        }
    }
}

/// How a turn died, when it ended on a recorded failure rather than a handback.
///
/// Every failure mode previously collapsed into `AwaitingInput` because the
/// classifier read log *shape* (turn ended, nothing pending) and never *why* it
/// ended. These are the three failures Claude Code actually records in the jsonl.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// An assistant record carried `isApiErrorMessage: true` -- Claude Code
    /// rendered an API failure (overloaded, rate-limited, context length) as the
    /// turn's final message. The agent is dead until a human retries it.
    ApiError,
    /// `stop_reason: "max_tokens"` -- the response hit the output cap and was
    /// truncated mid-thought. Not a handback; the agent got cut off.
    Truncated,
    /// `stop_reason: "refusal"` -- the model declined to continue.
    Refusal,
}

impl ErrorKind {
    pub fn detail(self) -> &'static str {
        match self {
            ErrorKind::ApiError => {
                "API error ended the turn (overload / rate limit / context) — retry it"
            }
            ErrorKind::Truncated => {
                "response hit max_tokens and was cut off mid-turn — needs a nudge to continue"
            }
            ErrorKind::Refusal => "model refused to continue the turn",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            ErrorKind::ApiError => "API error — retry",
            ErrorKind::Truncated => "cut off (max_tokens)",
            ErrorKind::Refusal => "refused",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The turn ended on a recorded failure (API error / truncation / refusal),
    /// not a handback. Ranks above everything: a blocked agent resumes the moment
    /// you answer it, but an errored one will sit dead until a human rescues it,
    /// and it was previously indistinguishable from a polite "waiting on you".
    Errored,
    /// Asked a question (AskUserQuestion / ExitPlanMode) that has no tool_result
    /// yet, or an unresolved tool call gone quiet (a probable permission prompt).
    /// The agent has stopped and *cannot proceed* until a human answers it.
    /// Ranks above everything else: nothing else on screen is costing time right
    /// now the way this is.
    Blocked,
    /// The turn ended cleanly and the agent is idle at the prompt with *nothing to
    /// test* -- it wrote no repo edits this turn, it simply handed the conversation
    /// back and is waiting for your reply or your nod. Distinct from `Blocked` (a
    /// question or approval that has it genuinely stuck) and from `NeedsTest`
    /// (edits to verify): this one wants only acknowledgement -- a glance and a
    /// reply -- so a supervisor triaging the board can tell "look at this" apart
    /// from "run and verify this".
    AwaitingAck,
    /// Mid-turn -- the agent is computing.
    Working,
    /// The turn ended, but the session spun up a background agent and is waiting
    /// on *it*, not on you. It resumes on its own when the agent reports back, so
    /// it must never be mistaken for the "stopped, your move" that wants a human.
    Delegated,
    /// Idle, and has repo edits you have not acked at their current timestamp --
    /// work a human still has to run and verify.
    NeedsTest,
    /// Idle with nothing outstanding.
    Clear,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Errored => "ERRORED",
            Status::Blocked => "WAITING ON YOU",
            Status::AwaitingAck => "AWAITING ACK",
            Status::Working => "working",
            Status::Delegated => "running a background agent",
            Status::NeedsTest => "AWAITING TEST",
            Status::Clear => "clear",
        }
    }

    /// A compact, always-visible status word for a task card -- the lowercase
    /// counterpart to the loud section-header `label`. The section header names a
    /// group and scrolls away; this rides on every card so a task's state reads
    /// off the card itself, in its status colour, without leaning on the glyph or
    /// remembering the palette.
    pub fn tag(self) -> &'static str {
        match self {
            Status::Errored => "errored",
            Status::Blocked => "waiting",
            Status::AwaitingAck => "your move",
            Status::Working => "working",
            Status::Delegated => "delegated",
            Status::NeedsTest => "needs test",
            Status::Clear => "clear",
        }
    }

    /// Sort rank: what should demand attention first. An errored agent outranks a
    /// blocked one because it will not recover on its own; a blocked agent (stuck
    /// on a question) outranks one merely awaiting acknowledgement, which in turn
    /// outranks untested work -- both are stalled on you, but a stopped agent is
    /// burning a slot while done-but-untested work simply sits. Delegated work
    /// wants nothing from you, so it sits below everything actionable.
    pub fn rank(self) -> u8 {
        match self {
            Status::Errored => 0,
            Status::Blocked => 1,
            Status::AwaitingAck => 2,
            Status::NeedsTest => 3,
            Status::Working => 4,
            Status::Delegated => 5,
            Status::Clear => 6,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub branch: Option<String>,
    /// Latest timestamp seen on any record, epoch millis.
    pub last_activity: i64,
    /// True when the last assistant message ended with `stop_reason: end_turn`
    /// and no user turn followed. This -- not elapsed time -- is what makes a
    /// session's writes safe to test: an agent can sit for ten minutes inside a
    /// single tool call, and a timeout would call that "done" while it is still
    /// editing the very files you would go test.
    pub turn_complete: bool,
    /// tool_use ids of AskUserQuestion / ExitPlanMode calls with no tool_result
    /// yet. Non-empty means the agent is parked on a question.
    pub open_questions: BTreeSet<String>,
    /// tool_use ids of any kind with no tool_result yet. Combined with a silent
    /// log this is the only available trace of a pending permission prompt.
    pub pending_tools: BTreeSet<String>,
    /// Set when the turn ended on a recorded failure and cleared the moment a
    /// healthy assistant stop or a fresh user turn supersedes it -- so it names
    /// only a failure the session is *currently* parked on, never a stale one it
    /// already recovered from. `Some` forces `Status::Errored`.
    pub error: Option<ErrorKind>,
    /// Epoch millis of the most recent "background agent launched" that has not
    /// since been superseded by a fresh user turn. Non-zero means the session
    /// spun up a background agent and, once its turn settles, is waiting on that
    /// agent rather than on you -- the difference between `Delegated` and the
    /// "stopped, your move" `AwaitingInput`.
    pub agent_launched_ms: i64,
    /// Epoch millis of when the current turn (the "task") began: the timestamp of
    /// the user prompt that last put an idle session back in flight. Mid-turn tool
    /// results do not move it, so `now - turn_started_ms` on a `Working` session is
    /// how long that task has been running, and on a settled session
    /// `last_activity - turn_started_ms` is how long it took. Zero until the first
    /// user record is folded (a session reconstructed from a partial log may have
    /// none), which the UI reads as "no timer to show".
    pub turn_started_ms: i64,
    /// True once a prompt carrying `ORC_MARKER` is seen -- this session is one of
    /// sauron's own orcs (a single-shot maintenance agent), so the UI marks it
    /// distinct from the hobbits doing your directed work.
    pub is_orc: bool,
    /// Repo-relative path -> epoch millis of its most recent write by this session.
    pub edits: BTreeMap<String, i64>,
    /// Repo-relative path -> `(timestamp, lines)` of the most recent text an
    /// Edit/Write wrote to it, harvested from the tool result. The timestamp
    /// lets a newer edit supersede an older preview; the lines are what the
    /// selected card shows under each file. Keyed like `edits`, so a file's
    /// preview lines up with its write-set entry.
    pub previews: BTreeMap<String, (i64, Vec<String>)>,
}

impl Session {
    /// First 8 chars of the uuid -- enough to disambiguate, short enough to scan.
    pub fn short_id(&self) -> &str {
        let n = self.id.len().min(8);
        &self.id[..n]
    }

    /// Command to resume this dropped session, exactly as copied to the
    /// clipboard: just `claude --resume <id>`, nothing else. The id is the full
    /// uuid (the jsonl filename), not the short form. Run it from the repo it
    /// belongs to so `--resume` resolves the right project.
    pub fn continue_command(&self) -> String {
        format!("claude --resume {}", self.id)
    }

    /// Title if the session has earned one, else a trimmed last prompt, else the id.
    pub fn display_name(&self) -> String {
        if let Some(t) = &self.title {
            if !t.trim().is_empty() {
                return t.clone();
            }
        }
        if let Some(p) = &self.last_prompt {
            let one = collapse_ws(p);
            if !one.is_empty() {
                return truncate(&one, 60);
            }
        }
        self.short_id().to_string()
    }

    /// Edits whose current timestamp is newer than what was acked for that path.
    /// A path acked and then re-edited reappears here -- that is the whole point.
    ///
    /// Writes older than `STALE_HORIZON_MS` drop out on their own. Acking must
    /// stay optional: if the only way to clear the board were to press `a` on
    /// every session, the tool would replace "remember what to test" with
    /// "remember to file paperwork", which is the same overhead wearing a
    /// different hat. Ack is for saying "checked this one now"; anything you
    /// never got to simply ages out.
    pub fn pending<'a>(
        &'a self,
        acked: Option<&BTreeMap<String, i64>>,
        now: i64,
    ) -> Vec<&'a str> {
        let cutoff = now.saturating_sub(STALE_HORIZON_MS);
        self.edits
            .iter()
            .filter(|(_, ts)| **ts >= cutoff)
            .filter(|(path, ts)| match acked.and_then(|a| a.get(path.as_str())) {
                Some(acked_ts) => *ts > acked_ts,
                None => true,
            })
            .map(|(path, _)| path.as_str())
            .collect()
    }

    /// Why this session is waiting on the user, if it is. Single source of truth
    /// for the whole "needs a human" question; `status` is derived from it.
    ///
    /// The ladder, most urgent first:
    ///   1. an open question tool  -> certain, waiting.
    ///   2. an unresolved tool call gone quiet -> probable permission prompt.
    ///   3. mid-turn -> not waiting (the agent is computing, come back later).
    ///   4. turn settled with untested edits -> that is NeedsTest, not "waiting".
    ///   5. turn settled, nothing to test, stopped recently -> idle at the prompt.
    pub fn blocked_reason(
        &self,
        now: i64,
        acked: Option<&BTreeMap<String, i64>>,
    ) -> Option<BlockedReason> {
        if !self.open_questions.is_empty() {
            return Some(BlockedReason::Question);
        }
        let quiet = now.saturating_sub(self.last_activity);
        // Nothing logged for a while with a tool call still open. Most often a
        // permission prompt sitting unanswered on some other terminal.
        if !self.pending_tools.is_empty() && quiet > STALL_AFTER_MS {
            return Some(BlockedReason::MaybeApproval);
        }
        let stuck = quiet > STUCK_AFTER_MS;
        // Still mid-turn: computing, not waiting.
        if !self.turn_complete && !stuck {
            return None;
        }
        // Turn settled. Untested edits are their own state (NeedsTest); do not
        // also call them "waiting on you".
        if !self.pending(acked, now).is_empty() {
            return None;
        }
        // Settled, nothing to test, and the agent stopped recently -- it is
        // sitting idle at the prompt. Old finished sessions fall through to
        // Clear so they do not clutter the attention band forever.
        if self.turn_complete && quiet <= RECENT_STOP_MS {
            return Some(BlockedReason::AwaitingInput);
        }
        None
    }

    pub fn status(&self, now: i64, acked: Option<&BTreeMap<String, i64>>) -> Status {
        // A recorded failure outranks everything, including a blocked question:
        // the stop hook that fires after an error would otherwise set
        // turn_complete and let this session masquerade as a polite "waiting on
        // you", which is exactly the misread this state exists to end.
        if self.error.is_some() {
            return Status::Errored;
        }
        if let Some(reason) = self.blocked_reason(now, acked) {
            // An idle-at-prompt turn (AwaitingInput) is not "stuck": it ended
            // clean with nothing to test and simply wants your attention. Split
            // three ways by what that attention actually is:
            //   * spun up a background agent -> waiting on *it*, not you (Delegated);
            //     it resumes itself when the agent reports back.
            //   * otherwise -> AwaitingAck: a supervisor need only glance and reply.
            // A real Question or a probable approval is different in kind -- the
            // agent cannot proceed until a human answers -- so it stays Blocked.
            if reason == BlockedReason::AwaitingInput {
                if self.agent_launched_ms > 0 {
                    return Status::Delegated;
                }
                return Status::AwaitingAck;
            }
            return Status::Blocked;
        }
        // Mid-turn means the write set is still moving. Reporting it as testable
        // is the dangerous direction to be wrong in -- you go exercise a file the
        // agent is halfway through rewriting -- so only an explicit end_turn (or
        // a stuck-turn timeout) clears a session for testing.
        let stuck = now.saturating_sub(self.last_activity) > STUCK_AFTER_MS;
        if !self.turn_complete && !stuck {
            return Status::Working;
        }
        if self.pending(acked, now).is_empty() {
            Status::Clear
        } else {
            Status::NeedsTest
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse `2026-07-21T17:59:10.746Z` to epoch millis.
///
/// Hand-rolled rather than pulling chrono: the format is fixed by the log writer,
/// and a 30-line function is cheaper than a dependency tree in a sidecar. Returns
/// None on anything that does not match, so a malformed record is skipped rather
/// than poisoning the session's activity clock.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };

    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);

    // Fractional seconds are optional and of unspecified length.
    let ms = if b.len() > 20 && b[19] == b'.' {
        let frac: String = s[20..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(3)
            .collect();
        let scaled = format!("{:0<3}", frac);
        scaled.parse::<i64>().unwrap_or(0)
    } else {
        0
    };

    let days = days_from_civil(y, mo, d);
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + ms)
}

/// Howard Hinnant's days_from_civil: civil date -> days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Compact relative age: "12s", "4m", "2h", "3d".
pub fn ago(then_ms: i64, now_ms: i64) -> String {
    let s = (now_ms.saturating_sub(then_ms) / 1000).max(0);
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// A task's elapsed run, at second resolution: "12s", "4m 12s", "1h 03m".
///
/// Distinct from `ago` (which is a coarse one-unit age for the "last seen" tag):
/// a running task wants a clock that visibly moves, so seconds are shown until a
/// minute has passed and the sub-unit is kept once it has, rather than collapsing
/// to a single figure. Negative spans (clock skew between records and `now`) clamp
/// to zero rather than rendering a nonsense duration.
pub fn fmt_duration(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// The machine's UTC offset in seconds, read once from `date +%z` ("-0700").
///
/// std exposes no local timezone, and the tool already deliberately skips chrono
/// for a fixed-format problem, so the one datum needed to render a wall clock --
/// the offset -- is lifted from `date`, which is present on every macOS/Linux box
/// this runs on. Read once at startup and cached by the caller: a DST flip
/// mid-session would shift a displayed start time by an hour, which for a clock on
/// a live task is not worth re-shelling every frame to catch. Anything unparseable
/// falls back to 0 (UTC), so a missing `date` degrades to a still-useful UTC clock
/// rather than breaking the timer.
pub fn local_offset_secs() -> i64 {
    let out = match Command::new("date").arg("+%z").output() {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let s = String::from_utf8_lossy(&out.stdout);
    parse_offset(s.trim())
}

/// Parse a `±HHMM` UTC offset to signed seconds. Non-conforming input -> 0.
fn parse_offset(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() < 5 {
        return 0;
    }
    let sign = match b[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return 0,
    };
    let (Some(h), Some(m)) = (
        s.get(1..3).and_then(|x| x.parse::<i64>().ok()),
        s.get(3..5).and_then(|x| x.parse::<i64>().ok()),
    ) else {
        return 0;
    };
    sign * (h * 3600 + m * 60)
}

/// A local wall-clock breakdown of an epoch instant. Only the fields a clock and
/// a same-day check need; no timezone name, no weekday.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
}

/// Break an epoch-millis instant into local civil fields, given the UTC offset in
/// seconds. Euclidean div/rem keep pre-1970 and sub-hour negative offsets correct;
/// the civil conversion is Hinnant's, the inverse of `days_from_civil`.
pub fn local_time(epoch_ms: i64, offset_secs: i64) -> LocalTime {
    let total = epoch_ms.div_euclid(1000) + offset_secs;
    let days = total.div_euclid(86_400);
    let rem = total.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    LocalTime {
        year,
        month,
        day,
        hour: rem / 3600,
        min: (rem % 3600) / 60,
        sec: rem % 60,
    }
}

/// Howard Hinnant's civil_from_days: days since 1970-01-01 -> (year, month, day).
/// The exact inverse of `days_from_civil`, so a round-trip is lossless.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A 12-hour clock with AM/PM: "3:42 PM", "12:05 AM". The form people read a
/// start time in, unambiguous about time-of-day where a bare 24h "15:42" makes
/// the reader translate.
pub fn fmt_clock(t: LocalTime) -> String {
    let (h12, ap) = match t.hour {
        0 => (12, "AM"),
        1..=11 => (t.hour, "AM"),
        12 => (12, "PM"),
        _ => (t.hour - 12, "PM"),
    };
    format!("{}:{:02} {}", h12, t.min, ap)
}

pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A path with the user's home collapsed to `~`. Purely for display: the header
/// prints the watched repo's path so two same-named checkouts can be told
/// apart, and `/Users/somebody/` at the front of it is the part that never
/// distinguishes anything while costing the most columns.
pub fn tilde(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    let home = crate::scan::home();
    let home = home.to_string_lossy();
    match s.strip_prefix(home.as_ref()) {
        Some(rest) if !home.is_empty() && home != "." => format!("~{rest}"),
        _ => s.into_owned(),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// [`truncate`], from the other end: keeps the tail and marks the cut in front.
///
/// For paths, which is the only thing that uses it. The front of a path is the
/// part every path on the machine has in common, so cutting from the back --
/// `~/Documents/checkou…` -- throws away the only characters that were telling
/// two boards apart.
pub fn truncate_left(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n + 1 - max.max(1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_shows_seconds_then_keeps_the_sub_unit() {
        // Under a minute counts in seconds so a live timer visibly moves.
        assert_eq!(fmt_duration(12_000), "12s");
        // Past a minute the seconds stay, unlike the coarse `ago`.
        assert_eq!(fmt_duration(4 * 60_000 + 12_000), "4m 12s");
        // Past an hour it rolls to hours and minutes.
        assert_eq!(fmt_duration(3_600_000 + 3 * 60_000), "1h 03m");
        // A negative span (record clock ahead of `now`) clamps, never renders junk.
        assert_eq!(fmt_duration(-5_000), "0s");
    }

    #[test]
    fn offset_parses_sign_hours_and_minutes() {
        assert_eq!(parse_offset("-0700"), -7 * 3600);
        assert_eq!(parse_offset("+0530"), 5 * 3600 + 30 * 60);
        assert_eq!(parse_offset("+0000"), 0);
        // Anything malformed degrades to UTC rather than a wrong offset.
        assert_eq!(parse_offset("garbage"), 0);
        assert_eq!(parse_offset(""), 0);
    }

    #[test]
    fn local_time_round_trips_against_the_forward_conversion() {
        // Pick an instant, break it into local fields at a known offset, and check
        // the pieces reassemble the same epoch through the forward path. This pins
        // civil_from_days as the exact inverse of days_from_civil.
        let epoch_ms = parse_rfc3339_ms("2026-07-24T22:42:07.000Z").unwrap();
        let offset = -7 * 3600; // PDT
        let lt = local_time(epoch_ms, offset);
        assert_eq!((lt.year, lt.month, lt.day), (2026, 7, 24));
        assert_eq!((lt.hour, lt.min, lt.sec), (15, 42, 7));

        let days = days_from_civil(lt.year, lt.month, lt.day);
        let back = ((days * 86_400 + lt.hour * 3600 + lt.min * 60 + lt.sec) - offset) * 1000;
        assert_eq!(back, epoch_ms - (epoch_ms % 1000));
    }

    #[test]
    fn clock_is_twelve_hour_with_meridiem() {
        let at = |ms: i64| fmt_clock(local_time(ms, 0));
        assert_eq!(at(parse_rfc3339_ms("2026-07-24T15:42:00Z").unwrap()), "3:42 PM");
        assert_eq!(at(parse_rfc3339_ms("2026-07-24T00:05:00Z").unwrap()), "12:05 AM");
        assert_eq!(at(parse_rfc3339_ms("2026-07-24T12:00:00Z").unwrap()), "12:00 PM");
        assert_eq!(at(parse_rfc3339_ms("2026-07-24T09:07:00Z").unwrap()), "9:07 AM");
    }

    #[test]
    fn parses_log_timestamp_format() {
        // The exact shape emitted into the session jsonl.
        let ms = parse_rfc3339_ms("2026-07-21T17:59:10.746Z").unwrap();
        // 1970-01-01 sanity: round-trips through the same civil conversion.
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.000Z").unwrap(), 0);
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.500Z").unwrap(), 1500);
        // Monotonic within the same day.
        let later = parse_rfc3339_ms("2026-07-21T18:00:00.000Z").unwrap();
        assert!(later > ms);
        // Missing fractional part is tolerated.
        assert!(parse_rfc3339_ms("2026-07-21T17:59:10Z").is_some());
        // Garbage is rejected rather than silently zeroed.
        assert!(parse_rfc3339_ms("not-a-timestamp").is_none());
    }

    #[test]
    fn continue_command_is_just_the_resume_line() {
        let mut s = Session::default();
        s.id = "6c6f86f2-1234".into();
        // Only `claude --resume <id>` -- no cd, no extra text, so the clipboard
        // holds exactly the command and nothing else.
        assert_eq!(s.continue_command(), "claude --resume 6c6f86f2-1234");
    }

    #[test]
    fn reedited_path_becomes_pending_again() {
        let now = 1_000_000_000i64;
        let mut s = Session::default();
        s.edits.insert("src/a.rs".into(), now - 1000);

        let mut acked = BTreeMap::new();
        acked.insert("src/a.rs".to_string(), now - 1000);
        assert!(
            s.pending(Some(&acked), now).is_empty(),
            "acked at current ts"
        );

        // Agent rewrites the same file after the ack.
        s.edits.insert("src/a.rs".into(), now - 500);
        assert_eq!(
            s.pending(Some(&acked), now),
            vec!["src/a.rs"],
            "re-edit after ack must resurface"
        );
    }

    #[test]
    fn unacked_writes_expire_without_any_keypress() {
        let now = 1_000_000_000i64;
        let mut s = Session::default();
        s.turn_complete = true;

        // Written just now, never acked: outstanding.
        s.edits.insert("src/a.rs".into(), now - 1000);
        assert_eq!(s.status(now, None), Status::NeedsTest);

        // Same write, once it is older than the horizon: clears itself. Acking
        // is for "I checked this"; it must never be the only way off the list,
        // or the tool just trades one bookkeeping chore for another.
        let later = now + STALE_HORIZON_MS + 1;
        assert!(s.pending(None, later).is_empty());
        assert_eq!(s.status(later, None), Status::Clear);
    }

    #[test]
    fn unresolved_tool_plus_silence_reads_as_waiting_on_you() {
        let now = 1_000_000_000i64;
        let mut s = Session::default();
        s.last_activity = now;
        s.pending_tools.insert("toolu_1".into());

        // Just issued: the tool is presumably running.
        assert_eq!(s.blocked_reason(now, None), None);
        assert_eq!(s.status(now, None), Status::Working);

        // Still unresolved and nothing logged since: almost always a permission
        // prompt sitting on another terminal.
        let later = now + STALL_AFTER_MS + 1;
        assert_eq!(
            s.blocked_reason(later, None),
            Some(BlockedReason::MaybeApproval)
        );
        assert_eq!(s.status(later, None), Status::Blocked);
    }

    #[test]
    fn a_confirmed_question_outranks_a_guessed_approval() {
        let now = 1_000_000_000i64;
        let mut s = Session::default();
        s.last_activity = now - STALL_AFTER_MS - 1;
        s.pending_tools.insert("toolu_1".into());
        s.open_questions.insert("toolu_1".into());
        // Both conditions hold; report the one we are certain about.
        assert_eq!(s.blocked_reason(now, None), Some(BlockedReason::Question));
    }

    #[test]
    fn idle_at_prompt_after_a_finished_turn_is_surfaced() {
        // The warpcore-dossier case: turn ended, no edits, no pending tool. The
        // agent is sitting at the prompt waiting for a typed reply, and the old
        // model dropped this as Clear. It wants only a glance and a reply, so it
        // is AwaitingAck -- not Blocked (which is reserved for an agent that
        // cannot proceed until a human answers).
        let now = 1_000_000_000i64;
        let mut s = Session::default();
        s.turn_complete = true;
        s.last_activity = now - 5_000; // stopped 5s ago

        assert_eq!(
            s.blocked_reason(now, None),
            Some(BlockedReason::AwaitingInput)
        );
        assert_eq!(s.status(now, None), Status::AwaitingAck);

        // But an ancient finished session is not "waiting" -- it is history, and
        // must fall through to Clear so it does not clog the attention band.
        let stale = now + RECENT_STOP_MS + 1;
        assert_eq!(s.blocked_reason(stale, None), None);
        assert_eq!(s.status(stale, None), Status::Clear);
    }

    #[test]
    fn awaiting_ack_and_needs_test_are_distinct_supervisor_states() {
        // The whole point of the split: a supervisor triaging the board must be
        // able to tell "the agent stopped and wants a reply" apart from "the
        // agent left code I have to run and verify".
        let now = 1_000_000_000i64;

        // Turn ended, NO edits -> just wants acknowledgement.
        let mut idle = Session::default();
        idle.turn_complete = true;
        idle.last_activity = now - 5_000;
        assert_eq!(idle.status(now, None), Status::AwaitingAck);

        // Same finished turn, but it WROTE a file -> that is testing, not acking.
        let mut wrote = idle.clone();
        wrote.edits.insert("src/a.rs".into(), now - 5_000);
        assert_eq!(wrote.status(now, None), Status::NeedsTest);

        // The two are genuinely different states, and the tester ranks below the
        // ack -- a stopped agent burns a slot, done-but-untested work only sits.
        assert_ne!(idle.status(now, None), wrote.status(now, None));
        assert!(Status::AwaitingAck.rank() < Status::NeedsTest.rank());
    }

    #[test]
    fn a_spawned_background_agent_is_delegated_not_your_move() {
        let now = 1_000_000_000i64;
        let mut s = Session::default();
        s.turn_complete = true;
        s.last_activity = now - 5_000;

        // Without a launch, an idle finished turn just wants a nod (AwaitingAck).
        assert_eq!(
            s.blocked_reason(now, None),
            Some(BlockedReason::AwaitingInput)
        );
        assert_eq!(s.status(now, None), Status::AwaitingAck);

        // Having spun up a background agent, the session is waiting on the agent,
        // not on you -- it resumes itself, so it must not read as "your move".
        s.agent_launched_ms = now - 5_000;
        assert_eq!(s.status(now, None), Status::Delegated);

        // A real question still wins: the human is genuinely needed.
        s.open_questions.insert("toolu_1".into());
        assert_eq!(s.status(now, None), Status::Blocked);
        s.open_questions.clear();

        // Untested edits still surface as NeedsTest -- delegation never hides work.
        s.edits.insert("src/a.rs".into(), now - 5_000);
        assert_eq!(s.status(now, None), Status::NeedsTest);
        s.edits.clear();

        // And once it goes quiet past the attention window it ages out to Clear,
        // rather than claiming to be delegated forever.
        assert_eq!(s.status(now + RECENT_STOP_MS + 1, None), Status::Clear);
    }

    #[test]
    fn finished_turn_with_untested_edits_is_needstest_not_waiting() {
        // Edits to test are their own signal; do not double-count them as
        // "waiting on you".
        let now = 1_000_000_000i64;
        let mut s = Session::default();
        s.turn_complete = true;
        s.last_activity = now - 5_000;
        s.edits.insert("src/a.rs".into(), now - 5_000);

        assert_eq!(s.blocked_reason(now, None), None);
        assert_eq!(s.status(now, None), Status::NeedsTest);
    }

    #[test]
    fn resolved_tools_leave_the_session_working() {
        let now = 1_000_000_000i64;
        let mut s = Session::default();
        s.last_activity = now - STALL_AFTER_MS - 1;
        // Mid-turn, nothing pending: quiet just means between records, not
        // blocked, and not yet a finished turn.
        assert_eq!(s.blocked_reason(now, None), None);
    }

    #[test]
    fn mid_turn_session_is_never_reported_testable() {
        let mut s = Session::default();
        s.edits.insert("src/a.rs".into(), 1);
        s.last_activity = 1_000_000;
        s.turn_complete = false;

        // The old timeout rule flipped to NeedsTest after two minutes of quiet.
        // An agent thinking, compiling, or inside one long tool call routinely
        // exceeds that while still rewriting the files you would go test.
        assert_eq!(s.status(1_000_000, None), Status::Working);
        assert_eq!(s.status(1_000_000 + 10 * 60 * 1000, None), Status::Working);
    }

    #[test]
    fn completed_turn_is_testable_immediately() {
        let mut s = Session::default();
        s.edits.insert("src/a.rs".into(), 1);
        s.last_activity = 1_000_000;
        s.turn_complete = true;
        // No waiting period: end_turn means the write set has settled.
        assert_eq!(s.status(1_000_000, None), Status::NeedsTest);
    }

    #[test]
    fn stuck_turn_eventually_releases() {
        let mut s = Session::default();
        s.edits.insert("src/a.rs".into(), 1);
        s.last_activity = 1_000_000;
        s.turn_complete = false;

        // An agent killed between tool_use and its result never logs end_turn.
        // Without the escape hatch its writes would be invisible forever.
        assert_eq!(
            s.status(1_000_000 + STUCK_AFTER_MS - 1, None),
            Status::Working
        );
        assert_eq!(
            s.status(1_000_000 + STUCK_AFTER_MS + 1, None),
            Status::NeedsTest
        );
    }

    /// The header prints the watched repo's path so two same-named checkouts can
    /// be told apart -- which only works if the end of the path is what
    /// survives the cut.
    #[test]
    fn a_clipped_path_keeps_the_end_that_identifies_it() {
        assert_eq!(truncate_left("~/src/demo", 20), "~/src/demo");
        assert_eq!(truncate_left("~/work/clients/acme/api", 12), "…ts/acme/api");
        // Never wider than asked, whatever it had to throw away.
        for max in 1..30 {
            assert!(truncate_left("~/a/very/long/path/indeed", max).chars().count() <= max.max(1));
        }
    }

    #[test]
    fn tilde_collapses_home_and_leaves_everything_else_alone() {
        let home = crate::scan::home();
        assert_eq!(tilde(&home.join("src/demo")), "~/src/demo");
        assert_eq!(tilde(std::path::Path::new("/opt/demo")), "/opt/demo");
    }
}
