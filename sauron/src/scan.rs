//! Incremental reader over an agent's session logs for one repo.
//!
//! The logs are append-only JSONL and reach 10MB. Re-parsing every file on every
//! 2s tick would burn real CPU alongside four running agents, so each file keeps
//! a byte offset and only newly appended bytes are parsed. Which files to read,
//! how to name a session from its path, and how to fold one record all come from
//! the `Agent` -- this module is the mechanism; Claude Code is one agent (its
//! fold lives here as `fold_record`), Codex is another (see `codex`).
//!
//! grep targets:
//!   struct Scanner          -- owns per-file offsets and folded sessions
//!   fn Scanner::refresh     -- tail each session file, fold new records
//!   fn fold_record          -- one Claude Code record -> mutation on a Session
//!   fn is_human_prompt      -- a user record a person typed, vs a tool result
//!   fn advances_activity    -- which records are evidence a session is alive
//!   fn fold_usage           -- message.usage -> the session's token counters
//!   fn claude_session_files -- the jsonl for a repo under ~/.claude/projects
//!   fn project_dir_for      -- /a/b/c -> ~/.claude/projects/-a-b-c

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent::Agent;
use crate::model::{parse_rfc3339_ms, ErrorKind, Session};

struct Tracked {
    /// Bytes already folded into `session`. Only complete lines are counted.
    offset: u64,
    session: Session,
}

pub struct Scanner {
    agent: Agent,
    log_dir: PathBuf,
    repo_root: PathBuf,
    tracked: HashMap<PathBuf, Tracked>,
}

impl Scanner {
    pub fn new(repo_root: PathBuf, agent: Agent) -> Self {
        Self {
            agent,
            log_dir: agent.log_root(&repo_root),
            repo_root,
            tracked: HashMap::new(),
        }
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Tail each of the agent's session files for this repo and return the
    /// folded sessions.
    pub fn refresh(&mut self) -> Vec<Session> {
        for path in self.agent.session_files(&self.repo_root) {
            self.tail_file(&path);
        }
        self.tracked.values().map(|t| t.session.clone()).collect()
    }

    /// The jsonl files under a Claude Code project directory.
    pub(crate) fn claude_session_files(repo_root: &Path) -> Vec<PathBuf> {
        let dir = project_dir_for(repo_root);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect()
    }

    fn tail_file(&mut self, path: &Path) {
        let agent = self.agent;
        let repo = self.repo_root.clone();
        let id = agent.session_id(path);

        let entry = self.tracked.entry(path.to_path_buf()).or_insert_with(|| Tracked {
            offset: 0,
            session: Session {
                id,
                ..Default::default()
            },
        });

        let len = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => return,
        };

        // Shrunk means the file was rewritten or rotated; the accumulated session
        // no longer describes it, so start clean rather than splicing garbage.
        if len < entry.offset {
            entry.offset = 0;
            let kept_id = entry.session.id.clone();
            entry.session = Session {
                id: kept_id,
                ..Default::default()
            };
        }
        if len == entry.offset {
            return;
        }

        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        if f.seek(SeekFrom::Start(entry.offset)).is_err() {
            return;
        }

        let mut buf = Vec::with_capacity((len - entry.offset) as usize);
        if f.read_to_end(&mut buf).is_err() {
            return;
        }

        // A tick can land mid-write. Consume only through the last newline and
        // leave the partial tail for the next pass.
        let complete_to = match buf.iter().rposition(|b| *b == b'\n') {
            Some(i) => i + 1,
            None => return,
        };

        let text = String::from_utf8_lossy(&buf[..complete_to]).into_owned();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                agent.fold(&mut entry.session, &v, &repo);
            }
        }

        entry.offset += complete_to as u64;
    }
}

/// Apply one Claude Code log record to the session being accumulated.
pub(crate) fn fold_record(session: &mut Session, v: &Value, repo_root: &Path) {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    let record_ms = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(parse_rfc3339_ms);
    if advances_activity(kind, v) {
        if let Some(ms) = record_ms {
            if ms > session.last_activity {
                session.last_activity = ms;
            }
        }
    }

    if let Some(b) = v.get("gitBranch").and_then(|b| b.as_str()) {
        if !b.is_empty() {
            session.branch = Some(b.to_string());
        }
    }

    // Turn tracking. An assistant message carries `stop_reason`: `end_turn`
    // means it handed control back, anything else (`tool_use`, `max_tokens`)
    // means more is coming. A non-meta user record afterwards -- a tool result
    // or a fresh prompt -- puts the session back in flight.
    match kind {
        "assistant" => {
            let stop = v
                .get("message")
                .and_then(|m| m.get("stop_reason"))
                .and_then(|s| s.as_str());
            // `null` stop_reason appears on streaming partials; treat it as
            // in-flight rather than complete.
            session.turn_complete = stop == Some("end_turn");

            // Failure signals. Claude Code records a surfaced API error as an
            // assistant record flagged `isApiErrorMessage` (the field also
            // appears as `false` on ordinary messages, so match `true`
            // explicitly); truncation and refusal arrive as stop_reasons. Any of
            // them means the turn died rather than handed back. A healthy stop
            // (`end_turn`/`stop_sequence`) or continued work (`tool_use`) clears
            // the flag, so a session that recovered on a later record stops
            // reading as errored.
            let api_error = v.get("isApiErrorMessage").and_then(|b| b.as_bool()) == Some(true);
            if api_error {
                session.error = Some(ErrorKind::ApiError);
            } else {
                match stop {
                    Some("max_tokens") => session.error = Some(ErrorKind::Truncated),
                    Some("refusal") => session.error = Some(ErrorKind::Refusal),
                    Some("end_turn") | Some("stop_sequence") | Some("tool_use") => {
                        session.error = None;
                    }
                    // `null` (streaming partial) and anything unrecognised leave a
                    // prior error standing rather than papering over it.
                    _ => {}
                }
            }

            fold_usage(session, v);
        }
        "user" => {
            // isMeta records are harness bookkeeping (command caveats, hook
            // output), not the user or a tool actually driving the turn.
            if v.get("isMeta").and_then(|m| m.as_bool()) != Some(true) {
                let human = is_human_prompt(v);
                // A *human* prompt that arrives while the previous turn was
                // complete (or before any turn has started) opens a new task --
                // stamp its start. A tool result must never stamp it: it is the
                // agent's own work coming back, and re-stamping restarts the
                // elapsed timer on every tool call, so a task that has run an
                // hour reads as seconds old.
                //
                // `turn_complete` alone used to carry this, on the assumption
                // that a mid-turn tool result always finds it false. It does not:
                // the stop hook sets it true, and the tool result that follows an
                // aborted-looking turn then re-opened the clock.
                if human && (session.turn_complete || session.turn_started_ms == 0) {
                    session.turn_started_ms = record_ms.unwrap_or(session.last_activity);
                }
                // Both kinds put the session back in flight: a tool result means
                // the agent has more to do, a prompt means the human asked for
                // more. This one is right as it stands.
                session.turn_complete = false;
                // Deliberate: only a human prompt clears a recorded failure. The
                // rule was "the human already engaged it (a retry, a new
                // prompt)", and a tool result is neither -- it is the agent's own
                // machinery, and letting it clear the flag would hide an API
                // error behind the next tool call. A session that genuinely
                // recovered still clears it, on the next healthy assistant stop.
                if human {
                    session.error = None;
                }
                // The tool result for a spawned background agent is a user
                // record too: it starts the "waiting on the agent" clock. Only a
                // human prompt supersedes that wait.
                //
                // Clearing it on *any* user record is what lost the flag on 241
                // of 617 measured launches -- 39% -- because every tool result
                // the session logged afterwards counted as one, and the session
                // then reported "stopped -- your move" while its subagent was
                // still running.
                if mentions_async_launch(v) {
                    session.agent_launched_ms = v
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .and_then(parse_rfc3339_ms)
                        .unwrap_or(session.last_activity);
                } else if human {
                    session.agent_launched_ms = 0;
                }
            }
        }
        // The stop hook fires when a turn ends and emits these. They are a more
        // robust turn-end marker than end_turn alone: an agent that stopped to
        // ask a question in prose still triggers them, and they arrive after any
        // trailing assistant record. This is what catches the idle-at-prompt
        // case that end_turn tracking alone missed. Every one sampled follows an
        // end_turn within 60ms, so letting them carry the session's clock is
        // safe -- see `advances_activity` for the subtypes that do not.
        "system" => {
            let sub = v.get("subtype").and_then(|s| s.as_str());
            if matches!(sub, Some("stop_hook_summary") | Some("turn_duration")) {
                session.turn_complete = true;
            }
        }
        _ => {}
    }

    fold_questions(session, v);

    match kind {
        "ai-title" => {
            if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                session.title = Some(t.to_string());
            }
        }
        "last-prompt" => {
            if let Some(p) = v.get("lastPrompt").and_then(|p| p.as_str()) {
                if p.contains(crate::model::ORC_MARKER) {
                    session.is_orc = true;
                }
                session.last_prompt = Some(p.to_string());
            }
        }
        // The write-set. This is the test surface.
        "file-history-delta" => {
            let Some(raw) = v.get("trackingPath").and_then(|p| p.as_str()) else {
                return;
            };
            let Some(rel) = repo_relative(raw, repo_root) else {
                return;
            };
            let ts = v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(parse_rfc3339_ms)
                .unwrap_or(session.last_activity);
            let slot = session.edits.entry(rel).or_insert(ts);
            if ts > *slot {
                *slot = ts;
            }
        }
        _ => {}
    }

    // Tool results ride on `user` records regardless of the write-set deltas, so
    // harvest the edited text here, outside the `kind` match.
    fold_edit_preview(session, v, repo_root);
}

/// True when a `user` record is something a person typed, rather than a tool
/// result the agent's own machinery posted back.
///
/// The single distinction three separate misreads rested on. `isMeta` was doing
/// this job, and `isMeta` is absent from every real record on disk -- so every
/// tool result counted as a human turn: it cleared the background-agent flag
/// (39% of measured launches lost it and reported "your move" while the subagent
/// ran), it re-stamped the task clock, and it cleared recorded errors.
///
/// The observable that does hold: Claude Code posts a tool result as a user
/// record whose `message.content` is an *array* carrying `tool_result` blocks. A
/// typed prompt is a plain string, or an array of `text` blocks. Anything
/// unrecognised counts as human, which is the safe direction -- treating a
/// prompt as a tool result would freeze the task clock for the whole session.
fn is_human_prompt(v: &Value) -> bool {
    let Some(blocks) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return true;
    };
    !blocks
        .iter()
        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
}

/// Whether a record is evidence the session was alive at its timestamp.
///
/// Everything is, except a `system` record of an unrecognised subtype. Those are
/// harness events with no agent behind them: a `local_command` was observed at
/// 23:15:46 on a session whose last real activity was 21:15:05, two hours dead,
/// and advancing the clock from it pulled the session back inside RECENT_STOP_MS
/// and relit it as "AWAITING ACK -- your move". `away_summary` does the same.
///
/// The two turn-end subtypes are exempt because they are not independent events:
/// every one sampled follows an `end_turn` within 60ms, so the clock they set is
/// the clock the turn already had.
fn advances_activity(kind: &str, v: &Value) -> bool {
    if kind != "system" {
        return true;
    }
    matches!(
        v.get("subtype").and_then(|s| s.as_str()),
        Some("stop_hook_summary") | Some("turn_duration")
    )
}

/// Add one assistant record's `message.usage` into the session's token counters.
///
/// Deduplicated on `message.id`, which is the whole difficulty. Claude Code
/// writes the same assistant message to the log 2-4 times as it streams -- one
/// sampled transcript held 24 assistant records over 10 distinct ids -- and each
/// copy carries a `usage` block. Summing them reads roughly 2.4x high, and reads
/// *plausibly* high, which is worse: nothing on screen would look wrong.
///
/// A record with usage but no `message.id` is counted rather than dropped. That
/// direction can only over-count a record the log gave us no way to recognise
/// again; the other direction silently loses real tokens.
fn fold_usage(session: &mut Session, v: &Value) {
    let Some(msg) = v.get("message") else {
        return;
    };
    let Some(usage) = msg.get("usage") else {
        return;
    };
    if let Some(id) = msg.get("id").and_then(|i| i.as_str()) {
        if !session.counted_messages.insert(id.to_string()) {
            return;
        }
    }
    let n = |key: &str| usage.get(key).and_then(|x| x.as_i64()).unwrap_or(0);
    session.input_tokens += n("input_tokens");
    session.output_tokens += n("output_tokens");
    session.cache_creation_tokens += n("cache_creation_input_tokens");
    session.cache_read_tokens += n("cache_read_input_tokens");
}

/// True when this user record carries the "Async agent launched successfully"
/// tool result -- the trace of the session spinning up a background agent, which
/// returns immediately and lets the turn end while the agent keeps working.
fn mentions_async_launch(v: &Value) -> bool {
    const MARKER: &str = "Async agent launched successfully";
    // The text lives a few levels down (message -> content -> tool_result ->
    // content -> text), so walk every string in the message content.
    fn any_string_contains(val: &Value, needle: &str) -> bool {
        match val {
            Value::String(s) => s.contains(needle),
            Value::Array(a) => a.iter().any(|x| any_string_contains(x, needle)),
            Value::Object(o) => o.values().any(|x| any_string_contains(x, needle)),
            _ => false,
        }
    }
    v.get("message")
        .and_then(|m| m.get("content"))
        .map(|c| any_string_contains(c, MARKER))
        .unwrap_or(false)
}

/// Capture the actual text of an Edit/Write from its tool result, keyed by the
/// same repo-relative path as the write-set, so a selected card can preview the
/// most recent lines written to each file. A newer result supersedes an older
/// one; a result with no usable text is ignored.
fn fold_edit_preview(session: &mut Session, v: &Value, repo_root: &Path) {
    let Some(tur) = v.get("toolUseResult").filter(|t| t.is_object()) else {
        return;
    };
    let Some(fp) = tur.get("filePath").and_then(|p| p.as_str()) else {
        return;
    };
    let Some(rel) = repo_relative(fp, repo_root) else {
        return;
    };
    let lines = preview_lines(tur);
    if lines.is_empty() {
        return;
    }
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(parse_rfc3339_ms)
        .unwrap_or(session.last_activity);
    match session.previews.get(&rel) {
        // Keep whichever preview is newer; ties fall to the later record.
        Some((prev_ts, _)) if *prev_ts > ts => {}
        _ => {
            session.previews.insert(rel, (ts, lines));
        }
    }
}

/// The most recent lines of text an edit put into a file: the added (`+`) lines
/// of its structured patch, falling back to the new file contents when there is
/// no patch. Blank edges are trimmed and the count is capped so one large edit
/// cannot flood a card.
fn preview_lines(tur: &Value) -> Vec<String> {
    const CAP: usize = 8;
    let mut out: Vec<String> = Vec::new();

    if let Some(hunks) = tur.get("structuredPatch").and_then(|p| p.as_array()) {
        'hunks: for h in hunks {
            let Some(lines) = h.get("lines").and_then(|l| l.as_array()) else {
                continue;
            };
            for l in lines.iter().filter_map(|l| l.as_str()) {
                // Patch lines are prefixed ' '/'+'/'-'; only the added ones are
                // "the most recent text". Removals and the no-newline marker are
                // not new content, so they are skipped.
                if let Some(rest) = l.strip_prefix('+') {
                    out.push(rest.to_string());
                    if out.len() >= CAP {
                        break 'hunks;
                    }
                }
            }
        }
    }

    // A pure deletion, or a Write with no patch: fall back to the new content.
    if out.is_empty() {
        if let Some(ns) = tur.get("newString").and_then(|s| s.as_str()) {
            out = ns.lines().take(CAP).map(|s| s.to_string()).collect();
        }
    }

    while out.first().map(|s| s.trim().is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
}

/// Track questions that stop the agent until a human answers.
///
/// `AskUserQuestion` and `ExitPlanMode` are logged as ordinary `tool_use`
/// blocks, and receive a matching `tool_result` only once answered. An
/// unmatched pair therefore means the agent is parked. Every other tool
/// resolves on its own, so only these two names are tracked -- a pending `Bash`
/// means "busy", not "blocked".
fn fold_questions(session: &mut Session, v: &Value) {
    const BLOCKING: [&str; 2] = ["AskUserQuestion", "ExitPlanMode"];

    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return;
    };

    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("tool_use") => {
                let Some(id) = block.get("id").and_then(|i| i.as_str()) else {
                    continue;
                };
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                // Every tool is tracked: an unresolved call plus a silent log is
                // the only trace a permission prompt leaves. The named two are
                // additionally certain to be waiting on a human.
                session.pending_tools.insert(id.to_string());
                if BLOCKING.contains(&name) {
                    session.open_questions.insert(id.to_string());
                }
            }
            Some("tool_result") => {
                if let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) {
                    session.open_questions.remove(id);
                    session.pending_tools.remove(id);
                }
            }
            _ => {}
        }
    }
}

/// Keep only writes that landed inside the repo.
///
/// Sessions also log writes to scratchpad scripts and `~/.claude` memory files.
/// Those are not testable software changes, and including them roughly doubled
/// the apparent write-set in every session sampled.
pub(crate) fn repo_relative(raw: &str, repo_root: &Path) -> Option<String> {
    let p = Path::new(raw);
    if p.is_absolute() {
        return p
            .strip_prefix(repo_root)
            .ok()
            .map(|r| r.to_string_lossy().into_owned());
    }
    Some(raw.to_string())
}

/// Claude Code encodes the project path by replacing separators with dashes:
/// `/Users/you/code/my-repo` -> `-Users-you-code-my-repo`.
/// Fold a repo path into the flat directory name Claude Code files its logs
/// under: `/a/b/c.d` -> `-a-b-c-d`.
///
/// Windows adds two characters the macOS form never sees. Backslash is the
/// separator, and the drive letter brings a colon, which is not legal in a
/// filename at all -- so `C:\Users\d\repo` has to fold to `C--Users-d-repo` for
/// the name to exist. Both are folded on every platform rather than under a
/// `cfg`, because the function is also asked about paths that came out of a log
/// file rather than off this disk.
pub fn encode_path(path: &Path) -> String {
    path.to_string_lossy().replace(['/', '\\', ':', '.'], "-")
}

/// Repo root -> the directory holding its Claude Code session logs.
///
/// The encoding above is a *reconstruction* of Claude Code's, and on macOS it is
/// a verified one. On Windows it is not: nobody has yet pasted back a real
/// `~/.claude/projects` listing from a Windows machine, and the failure mode if
/// the guess is wrong is the worst kind -- a board that draws perfectly and is
/// simply empty, with nothing anywhere going red.
///
/// So the guess is checked, and when it misses, the directory is searched for a
/// name that matches the repo path once both sides are stripped to their
/// alphanumerics. That answers correctly whatever separator convention turns out
/// to be in use. When the probe also finds nothing the guess is returned anyway,
/// so callers report a concrete path they can go and look at rather than a
/// silence.
///
/// The probe is one `read_dir` of a directory with one entry per project, and it
/// only runs when the guess missed. Delete it, and this function's Windows
/// branch with it, once a real listing confirms the encoding.
pub fn project_dir_for(repo_root: &Path) -> PathBuf {
    let projects = home().join(".claude").join("projects");
    let guess = projects.join(encode_path(repo_root));
    if guess.is_dir() {
        return guess;
    }
    // macOS deliberately does not probe. There the encoding is verified, so a
    // miss means the repo genuinely has no sessions yet -- and the probe's
    // fuzzy match would happily marry `my-repo` to a `myrepo` sitting next to
    // it. Risk with no upside is not a fallback.
    #[cfg(windows)]
    {
        return probe_project_dir(&projects, repo_root).unwrap_or(guess);
    }
    #[cfg(not(windows))]
    guess
}

/// Find the entry of `projects` whose name is this repo path under some other
/// separator convention. `None` when nothing matches, or when the match is not
/// unique -- an ambiguous answer here would attach the board to another repo's
/// sessions, which is worse than showing none.
#[cfg_attr(not(windows), allow(dead_code))]
fn probe_project_dir(projects: &Path, repo_root: &Path) -> Option<PathBuf> {
    let want = alphanumeric_key(&repo_root.to_string_lossy());
    if want.is_empty() {
        return None;
    }
    let mut hits: Vec<PathBuf> = std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| alphanumeric_key(&e.file_name().to_string_lossy()) == want)
        .map(|e| e.path())
        .collect();
    hits.sort();
    match hits.len() {
        1 => hits.pop(),
        _ => None,
    }
}

/// A path reduced to its lowercase alphanumerics, so `/a/b/my-repo`,
/// `-a-b-my-repo`, and `C:\a\b\my-repo` all compare equal on the part that
/// carries the meaning. Separators, dots and dashes are exactly what the two
/// conventions disagree about, so they are exactly what this drops.
#[cfg_attr(not(windows), allow(dead_code))]
fn alphanumeric_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// The user's home directory. Delegates to `plat`, which knows that Windows
/// calls it `%USERPROFILE%` and that a domain-joined machine may redirect it.
pub fn home() -> PathBuf {
    crate::plat::home()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_project_dir_like_claude_code() {
        let d = project_dir_for(Path::new("/Users/you/code/my-repo"));
        assert!(d.ends_with("-Users-you-code-my-repo"));
    }

    #[test]
    fn a_windows_path_folds_its_backslashes_and_its_drive_colon() {
        // The bug this pins: folding only `/` and `.` left `\` and `:` in the
        // name, and `:` is not legal in a filename at all -- so the directory
        // could not exist, and the board would have drawn perfectly and been
        // empty with nothing going red.
        assert_eq!(
            encode_path(Path::new("C:\\Users\\dave\\code\\my-repo")),
            "C--Users-dave-code-my-repo"
        );
        // A dotted directory folds the same way it does on macOS.
        assert_eq!(
            encode_path(Path::new("D:\\src\\v1.2\\app")),
            "D--src-v1-2-app"
        );
        // And the macOS form is untouched by the added characters.
        assert_eq!(
            encode_path(Path::new("/Users/you/code/my-repo")),
            "-Users-you-code-my-repo"
        );
    }

    /// A `projects` directory holding `names`, in a fresh temp dir.
    fn projects_dir(tag: &str, names: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sauron-probe-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for name in names {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn the_probe_finds_a_log_dir_under_a_different_separator_convention() {
        // The whole point of the fallback: nobody has confirmed what Claude Code
        // names these on Windows. Whatever convention it turns out to use, the
        // repo path still has to reach its own sessions.
        let dir = projects_dir("found", &["C--Users-dave-code-my-repo"]);
        let hit = probe_project_dir(&dir, Path::new("C:\\Users\\dave\\code\\my-repo"));
        assert_eq!(hit, Some(dir.join("C--Users-dave-code-my-repo")));

        // A convention nobody has proposed, to show the match is not tuned to one
        // guess: underscores throughout.
        let dir = projects_dir("alt", &["C__Users_dave_code_my_repo"]);
        let hit = probe_project_dir(&dir, Path::new("C:\\Users\\dave\\code\\my-repo"));
        assert_eq!(hit, Some(dir.join("C__Users_dave_code_my_repo")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_probe_refuses_an_ambiguous_answer_rather_than_guessing() {
        // Two directories that both normalise to the repo path. Attaching the
        // board to the wrong repo's sessions is worse than showing none, so this
        // must answer nothing at all.
        let dir = projects_dir(
            "ambiguous",
            &["C--Users-dave-code-my-repo", "C__Users_dave_code_my_repo"],
        );
        let hit = probe_project_dir(&dir, Path::new("C:\\Users\\dave\\code\\my-repo"));
        assert_eq!(hit, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_neighbour_differing_only_in_punctuation_is_a_known_false_match() {
        // `my-repo` and `myrepo` are different repositories, and this matches the
        // wrong one. Pinned rather than fixed because it cannot be fixed here:
        // Claude Code's encoding is lossy -- `-a-b-my-repo` is equally a decoding
        // of `/a/b/my-repo` and of `/a/b/my/repo` -- so no probe over these names
        // can separate them, and a cleverer normalisation would only move which
        // pair it confuses.
        //
        // What bounds the risk is that the probe runs at all only when the exact
        // encoding missed *and* the convention differs *and* such a neighbour
        // exists. Confirm the real naming on a Windows box and this whole path,
        // test included, is deleted.
        let dir = projects_dir("neighbour", &["C--Users-dave-code-myrepo"]);
        let hit = probe_project_dir(&dir, Path::new("C:\\Users\\dave\\code\\my-repo"));
        assert_eq!(hit, Some(dir.join("C--Users-dave-code-myrepo")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_probe_answers_nothing_when_the_directory_is_missing_or_empty() {
        let dir = projects_dir("empty", &[]);
        assert_eq!(probe_project_dir(&dir, Path::new("C:\\r")), None);
        assert_eq!(
            probe_project_dir(&dir.join("absent"), Path::new("C:\\r")),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filters_write_set_to_repo_paths() {
        let root = Path::new("/Users/you/code/my-repo");
        // Relative paths are repo paths.
        assert_eq!(
            repo_relative("src/ecology/mod.rs", root).as_deref(),
            Some("src/ecology/mod.rs")
        );
        // Absolute inside the repo is stripped to relative.
        assert_eq!(
            repo_relative("/Users/you/code/my-repo/src/a.rs", root).as_deref(),
            Some("src/a.rs")
        );
        // Scratchpad and memory writes are not testable surface.
        assert!(repo_relative("/private/tmp/claude-501/x/scratchpad/cut.py", root).is_none());
        assert!(repo_relative("/Users/d/.claude/projects/x/memory/note.md", root).is_none());
    }

    #[test]
    fn folds_records_into_session() {
        let root = Path::new("/repo");
        let mut s = Session::default();

        fold_record(&mut s, &json!({"type":"ai-title","aiTitle":"Letters redesign"}), root);
        fold_record(
            &mut s,
            &json!({"type":"file-history-delta","trackingPath":"src/gui/letters.rs",
                    "timestamp":"2026-07-21T17:59:10.746Z"}),
            root,
        );
        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-21T18:00:00.000Z",
                    "gitBranch":"station_physics"}),
            root,
        );

        assert_eq!(s.title.as_deref(), Some("Letters redesign"));
        assert_eq!(s.branch.as_deref(), Some("station_physics"));
        assert_eq!(s.edits.len(), 1);
        assert!(s.edits.contains_key("src/gui/letters.rs"));
        // last_activity tracks the newest record, not the newest edit.
        assert_eq!(
            s.last_activity,
            parse_rfc3339_ms("2026-07-21T18:00:00.000Z").unwrap()
        );
    }

    #[test]
    fn background_agent_launch_sets_the_wait_and_a_later_turn_clears_it() {
        let root = Path::new("/repo");
        let mut s = Session::default();

        // The launch tool result: the marker is nested a few levels down under
        // message -> content -> tool_result -> content -> text.
        fold_record(
            &mut s,
            &json!({
                "type":"user","timestamp":"2026-07-22T05:00:00.000Z",
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t1","content":[
                        {"type":"text","text":"Async agent launched successfully. agentId: abc123"}
                    ]}
                ]}
            }),
            root,
        );
        assert!(s.agent_launched_ms > 0, "launching an agent starts the wait");

        // A later real user turn -- the agent reporting back, or a human prompt --
        // supersedes and clears it.
        fold_record(
            &mut s,
            &json!({
                "type":"user","timestamp":"2026-07-22T05:01:00.000Z",
                "message":{"role":"user","content":"thanks, carry on"}
            }),
            root,
        );
        assert_eq!(s.agent_launched_ms, 0, "a fresh user turn clears the wait");
    }

    #[test]
    fn harvests_the_added_lines_of_an_edit_and_keeps_the_newest() {
        let root = Path::new("/repo");
        let mut s = Session::default();

        // An Edit tool result: the structured patch carries context and added
        // lines; only the added ones are the "most recent text".
        fold_record(
            &mut s,
            &json!({
                "type":"user","timestamp":"2026-07-21T17:00:00.000Z",
                "toolUseResult":{
                    "filePath":"/repo/src/auth/mod.rs",
                    "structuredPatch":[{"lines":[
                        " unchanged context",
                        "-let store = old();",
                        "+let store = TokenStore::open()?;",
                        "+store.check(tok)"
                    ]}]
                }
            }),
            root,
        );
        assert_eq!(
            s.previews.get("src/auth/mod.rs").map(|(_, l)| l.clone()),
            Some(vec![
                "let store = TokenStore::open()?;".to_string(),
                "store.check(tok)".to_string(),
            ]),
            "only added lines, context and removals dropped"
        );

        // A later edit to the same file supersedes the earlier preview.
        fold_record(
            &mut s,
            &json!({
                "type":"user","timestamp":"2026-07-21T18:00:00.000Z",
                "toolUseResult":{
                    "filePath":"/repo/src/auth/mod.rs",
                    "structuredPatch":[{"lines":["+fn newer() {}"]}]
                }
            }),
            root,
        );
        assert_eq!(
            s.previews.get("src/auth/mod.rs").map(|(_, l)| l.clone()),
            Some(vec!["fn newer() {}".to_string()]),
            "newer edit wins"
        );

        // A Write with no patch falls back to the new content.
        fold_record(
            &mut s,
            &json!({
                "type":"user","timestamp":"2026-07-21T17:30:00.000Z",
                "toolUseResult":{
                    "filePath":"/repo/README.md",
                    "newString":"# Title\n\nbody line\n"
                }
            }),
            root,
        );
        assert_eq!(
            s.previews.get("README.md").map(|(_, l)| l.clone()),
            Some(vec!["# Title".to_string(), "".to_string(), "body line".to_string()]),
            "no patch -> new content, blank edges trimmed"
        );
    }

    #[test]
    fn an_orc_launch_prompt_marks_the_session() {
        let root = Path::new("/repo");
        let mut s = Session::default();
        assert!(!s.is_orc);
        // A hobbit's prompt leaves it unmarked.
        fold_record(
            &mut s,
            &json!({"type":"last-prompt","lastPrompt":"add a settings screen"}),
            root,
        );
        assert!(!s.is_orc);
        // An orc's prompt carries the marker -> tagged, and it sticks.
        fold_record(
            &mut s,
            &json!({"type":"last-prompt",
                    "lastPrompt":"This file is safe to refactor -- no other agent is touching it. Make one pass on x.rs"}),
            root,
        );
        assert!(s.is_orc);
    }

    #[test]
    fn turn_start_stamps_on_a_new_task_and_survives_mid_turn_tool_results() {
        let root = Path::new("/repo");
        let mut s = Session::default();

        // The user's prompt opens the task -- its timestamp is the start.
        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-24T22:00:00.000Z",
                    "message":{"role":"user","content":"do the thing"}}),
            root,
        );
        let start = parse_rfc3339_ms("2026-07-24T22:00:00.000Z").unwrap();
        assert_eq!(s.turn_started_ms, start);

        // A tool result arrives mid-turn (turn_complete already false). It must not
        // reset the clock, or the "running" timer would restart on every tool call.
        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-24T22:00:05.000Z",
                    "message":{"stop_reason":"tool_use"}}),
            root,
        );
        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-24T22:03:00.000Z",
                    "message":{"role":"user","content":[
                        {"type":"tool_result","tool_use_id":"t1"}]}}),
            root,
        );
        assert_eq!(s.turn_started_ms, start, "mid-turn result must not restart the clock");

        // The turn ends, then a fresh prompt opens a new task -- new start.
        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-24T22:05:00.000Z",
                    "message":{"stop_reason":"end_turn"}}),
            root,
        );
        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-24T22:10:00.000Z",
                    "message":{"role":"user","content":"next thing"}}),
            root,
        );
        assert_eq!(
            s.turn_started_ms,
            parse_rfc3339_ms("2026-07-24T22:10:00.000Z").unwrap(),
            "a prompt after a completed turn starts a new task clock"
        );
    }

    #[test]
    fn tracks_turn_completion_from_stop_reason() {
        let root = Path::new("/repo");
        let mut s = Session::default();

        // Mid-turn: assistant is about to run a tool.
        fold_record(
            &mut s,
            &json!({"type":"assistant","message":{"stop_reason":"tool_use"}}),
            root,
        );
        assert!(!s.turn_complete);

        // Handed control back.
        fold_record(
            &mut s,
            &json!({"type":"assistant","message":{"stop_reason":"end_turn"}}),
            root,
        );
        assert!(s.turn_complete);

        // Harness bookkeeping must not look like the user driving a new turn.
        fold_record(
            &mut s,
            &json!({"type":"user","isMeta":true,"message":{"role":"user"}}),
            root,
        );
        assert!(s.turn_complete, "isMeta record should not reopen the turn");

        // A real tool result or prompt does.
        fold_record(
            &mut s,
            &json!({"type":"user","message":{"role":"user"}}),
            root,
        );
        assert!(!s.turn_complete);
    }

    #[test]
    fn open_question_blocks_until_answered() {
        let root = Path::new("/repo");
        let mut s = Session::default();

        fold_record(
            &mut s,
            &json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[
                {"type":"tool_use","id":"toolu_q1","name":"AskUserQuestion"}]}}),
            root,
        );
        assert_eq!(s.open_questions.len(), 1, "pending question is tracked");

        // The answer arrives as an ordinary tool_result carrying the same id.
        fold_record(
            &mut s,
            &json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_q1"}]}}),
            root,
        );
        assert!(s.open_questions.is_empty(), "answered question clears");
    }

    #[test]
    fn ordinary_tool_is_pending_but_not_a_certain_question() {
        let root = Path::new("/repo");
        let mut s = Session::default();
        fold_record(
            &mut s,
            &json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[
                {"type":"tool_use","id":"toolu_b1","name":"Bash"}]}}),
            root,
        );
        // Not a question, but still unresolved -- a permission prompt on this
        // Bash call would look exactly like this in the log.
        assert!(s.open_questions.is_empty());
        assert_eq!(s.pending_tools.len(), 1);

        fold_record(
            &mut s,
            &json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_b1"}]}}),
            root,
        );
        assert!(s.pending_tools.is_empty(), "result clears the pending call");
    }

    #[test]
    fn exit_plan_mode_also_blocks() {
        let root = Path::new("/repo");
        let mut s = Session::default();
        fold_record(
            &mut s,
            &json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[
                {"type":"tool_use","id":"toolu_p1","name":"ExitPlanMode"}]}}),
            root,
        );
        assert_eq!(s.open_questions.len(), 1);
    }

    #[test]
    fn later_edit_advances_path_timestamp() {
        let root = Path::new("/repo");
        let mut s = Session::default();
        let rec = |ts: &str| {
            json!({"type":"file-history-delta","trackingPath":"src/a.rs","timestamp":ts})
        };
        fold_record(&mut s, &rec("2026-07-21T10:00:00.000Z"), root);
        fold_record(&mut s, &rec("2026-07-21T12:00:00.000Z"), root);
        assert_eq!(
            s.edits["src/a.rs"],
            parse_rfc3339_ms("2026-07-21T12:00:00.000Z").unwrap()
        );
    }

    #[test]
    fn api_error_outranks_the_stop_hook_that_follows_it() {
        use crate::model::Status;
        let root = Path::new("/repo");
        let mut s = Session::default();

        // Turn dies on an API error rendered as an assistant message.
        fold_record(
            &mut s,
            &json!({"type":"assistant","isApiErrorMessage":true,
                    "timestamp":"2026-07-21T18:00:00.000Z",
                    "message":{"role":"assistant","stop_reason":null}}),
            root,
        );
        assert_eq!(s.error, Some(ErrorKind::ApiError));

        // The stop hook fires afterward and sets turn_complete -- the exact record
        // that used to launder a dead agent into a polite "waiting on you".
        fold_record(
            &mut s,
            &json!({"type":"system","subtype":"stop_hook_summary",
                    "timestamp":"2026-07-21T18:00:01.000Z"}),
            root,
        );
        assert!(s.turn_complete);

        // Error still wins: recent stop, nothing pending, would have been
        // AwaitingInput/Blocked before. Now it is Errored.
        let now = parse_rfc3339_ms("2026-07-21T18:00:05.000Z").unwrap();
        assert_eq!(s.status(now, None), Status::Errored);

        // A real user turn (a retry) clears the stale failure.
        fold_record(&mut s, &json!({"type":"user","message":{"role":"user"}}), root);
        assert_eq!(s.error, None);
        assert_ne!(s.status(now, None), Status::Errored);
    }

    #[test]
    fn max_tokens_is_errored_but_a_clean_stop_clears_it() {
        use crate::model::Status;
        let root = Path::new("/repo");
        let mut s = Session::default();

        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-21T18:00:00.000Z",
                    "message":{"stop_reason":"max_tokens"}}),
            root,
        );
        assert_eq!(s.error, Some(ErrorKind::Truncated));

        // stop_sequence is a healthy completion, not a failure -- it must clear.
        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-21T18:00:02.000Z",
                    "message":{"stop_reason":"stop_sequence"}}),
            root,
        );
        assert_eq!(s.error, None, "stop_sequence must not read as an error");
        let now = parse_rfc3339_ms("2026-07-21T18:00:05.000Z").unwrap();
        assert_ne!(s.status(now, None), Status::Errored);
    }

    #[test]
    fn a_tool_result_is_not_a_human_prompt() {
        // The one distinction three misreads rested on. `isMeta` was doing this
        // job and is absent from every real record, so every tool result counted
        // as a person typing.
        assert!(!is_human_prompt(&json!({"message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":"ok"}]}})));
        // A typed prompt arrives as a bare string ...
        assert!(is_human_prompt(&json!({"message":{"role":"user","content":"do the thing"}})));
        // ... or as text blocks, which a tool result never is.
        assert!(is_human_prompt(&json!({"message":{"role":"user","content":[
            {"type":"text","text":"do the thing"}]}})));
        // Anything unrecognised counts as human: treating a prompt as a tool
        // result would freeze the task clock for the session's whole life.
        assert!(is_human_prompt(&json!({"message":{"role":"user"}})));
        assert!(is_human_prompt(&json!({})));
    }

    #[test]
    fn a_tool_result_does_not_clear_the_background_agent_flag() {
        // 241 of 617 measured launches lost the flag this way and reported
        // "stopped -- your move" while the subagent was still running: the
        // launch's own tool result set it, and the very next tool result of any
        // kind cleared it again.
        use crate::model::Status;
        let root = Path::new("/repo");
        let mut s = Session::default();

        fold_record(
            &mut s,
            &json!({
                "type":"user","timestamp":"2026-07-22T05:00:00.000Z",
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t1","content":[
                        {"type":"text","text":"Async agent launched successfully. agentId: abc"}
                    ]}
                ]}
            }),
            root,
        );
        assert!(s.agent_launched_ms > 0);

        // The session keeps working while the agent runs, and every tool result
        // it logs used to wipe the flag.
        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-22T05:00:30.000Z",
                    "message":{"role":"user","content":[
                        {"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}),
            root,
        );
        assert!(s.agent_launched_ms > 0, "a tool result is not a human turn");

        // With the turn settled it must read as Delegated, never AwaitingAck.
        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-22T05:01:00.000Z",
                    "message":{"stop_reason":"end_turn"}}),
            root,
        );
        let now = parse_rfc3339_ms("2026-07-22T05:01:10.000Z").unwrap();
        assert_eq!(s.status(now, None), Status::Delegated);

        // A person typing still supersedes the wait, which is the whole point of
        // keeping the clear at all.
        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-22T05:02:00.000Z",
                    "message":{"role":"user","content":"never mind, do this instead"}}),
            root,
        );
        assert_eq!(s.agent_launched_ms, 0);
    }

    #[test]
    fn a_tool_result_after_a_stop_hook_does_not_restart_the_task_clock() {
        // `turn_complete` alone used to guard the stamp, on the assumption that a
        // mid-turn tool result always finds it false. The stop hook sets it true,
        // and the next tool result then re-stamped the start -- an hour-old task
        // reading as seconds old.
        let root = Path::new("/repo");
        let mut s = Session::default();

        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-24T22:00:00.000Z",
                    "message":{"role":"user","content":"do the thing"}}),
            root,
        );
        let start = parse_rfc3339_ms("2026-07-24T22:00:00.000Z").unwrap();

        fold_record(
            &mut s,
            &json!({"type":"system","subtype":"stop_hook_summary",
                    "timestamp":"2026-07-24T22:00:01.000Z"}),
            root,
        );
        assert!(s.turn_complete);

        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-24T23:00:00.000Z",
                    "message":{"role":"user","content":[
                        {"type":"tool_result","tool_use_id":"t9"}]}}),
            root,
        );
        assert_eq!(s.turn_started_ms, start, "a tool result must never open a task");

        // A person typing after the turn has settled does open one -- which is
        // what the stamp is for, and what must survive the tighter guard.
        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-24T23:29:00.000Z",
                    "message":{"stop_reason":"end_turn"}}),
            root,
        );
        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-24T23:30:00.000Z",
                    "message":{"role":"user","content":"next thing"}}),
            root,
        );
        assert_eq!(
            s.turn_started_ms,
            parse_rfc3339_ms("2026-07-24T23:30:00.000Z").unwrap()
        );
    }

    #[test]
    fn an_unrecognised_system_subtype_does_not_resurrect_a_dead_session() {
        // Observed: a `local_command` at 23:15:46 on a session whose last real
        // activity was 21:15:05. Two hours dead, and the record pulled it back
        // inside RECENT_STOP_MS and relit it as "AWAITING ACK -- your move".
        use crate::model::Status;
        let root = Path::new("/repo");
        let mut s = Session::default();

        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-21T21:15:05.000Z",
                    "message":{"stop_reason":"end_turn"}}),
            root,
        );
        let last_real = parse_rfc3339_ms("2026-07-21T21:15:05.000Z").unwrap();
        assert_eq!(s.last_activity, last_real);

        for sub in ["local_command", "away_summary"] {
            fold_record(
                &mut s,
                &json!({"type":"system","subtype":sub,
                        "timestamp":"2026-07-21T23:15:46.000Z"}),
                root,
            );
            assert_eq!(s.last_activity, last_real, "{sub} moved the session's clock");
        }

        // Two hours after the real end, it is history -- not something waiting.
        let now = parse_rfc3339_ms("2026-07-21T23:15:50.000Z").unwrap();
        assert_eq!(s.status(now, None), Status::Clear);

        // The two turn-end subtypes still carry the clock: each follows an
        // end_turn within 60ms, so the time they set is the turn's own.
        fold_record(
            &mut s,
            &json!({"type":"system","subtype":"stop_hook_summary",
                    "timestamp":"2026-07-21T23:16:00.000Z"}),
            root,
        );
        assert_eq!(
            s.last_activity,
            parse_rfc3339_ms("2026-07-21T23:16:00.000Z").unwrap()
        );
    }

    #[test]
    fn assistant_usage_is_counted_once_per_message_id() {
        // The transcript on disk repeats one assistant message 2-4 times as it
        // streams, each copy carrying the same usage block. One sampled file held
        // 24 assistant records over 10 distinct ids; summing every copy reads
        // about 2.4x high, and reads plausibly high, which is worse.
        let root = Path::new("/repo");
        let mut s = Session::default();
        let rec = |id: &str, out: i64| {
            json!({"type":"assistant","timestamp":"2026-07-21T18:00:00.000Z",
                   "message":{"id":id,"stop_reason":"tool_use","usage":{
                       "input_tokens":10,
                       "output_tokens":out,
                       "cache_creation_input_tokens":100,
                       "cache_read_input_tokens":1_000}}})
        };

        fold_record(&mut s, &rec("msg_a", 5), root);
        fold_record(&mut s, &rec("msg_a", 5), root);
        fold_record(&mut s, &rec("msg_a", 5), root);
        assert_eq!(s.output_tokens, 5, "a repeated id must be counted once");
        assert_eq!(s.total_tokens(), 1_115);

        // A genuinely new message adds.
        fold_record(&mut s, &rec("msg_b", 7), root);
        assert_eq!(s.input_tokens, 20);
        assert_eq!(s.output_tokens, 12);
        assert_eq!(s.cache_creation_tokens, 200);
        assert_eq!(s.cache_read_tokens, 2_000);
        assert_eq!(s.total_tokens(), 2_232);

        // Usage with no id cannot be recognised again, so it is counted rather
        // than dropped -- over-counting an unidentifiable record beats silently
        // losing real tokens.
        fold_record(
            &mut s,
            &json!({"type":"assistant","message":{"usage":{"output_tokens":3}}}),
            root,
        );
        assert_eq!(s.output_tokens, 15);

        // A record with no usage at all leaves the counters alone.
        fold_record(
            &mut s,
            &json!({"type":"assistant","message":{"id":"msg_c","stop_reason":"end_turn"}}),
            root,
        );
        assert_eq!(s.total_tokens(), 2_235);
    }

    #[test]
    fn a_tool_result_does_not_clear_a_recorded_error() {
        // Deliberate, and the counterpart to `api_error_outranks_the_stop_hook`:
        // the rule was "the human already engaged it", and the agent's own
        // machinery posting a result back is not a human engaging anything.
        use crate::model::Status;
        let root = Path::new("/repo");
        let mut s = Session::default();

        fold_record(
            &mut s,
            &json!({"type":"assistant","isApiErrorMessage":true,
                    "timestamp":"2026-07-21T18:00:00.000Z",
                    "message":{"role":"assistant","stop_reason":null}}),
            root,
        );
        assert_eq!(s.error, Some(ErrorKind::ApiError));

        fold_record(
            &mut s,
            &json!({"type":"user","timestamp":"2026-07-21T18:00:02.000Z",
                    "message":{"role":"user","content":[
                        {"type":"tool_result","tool_use_id":"t1"}]}}),
            root,
        );
        assert_eq!(s.error, Some(ErrorKind::ApiError), "a tool result is not a retry");

        // A session that genuinely recovered clears it on the next healthy stop.
        fold_record(
            &mut s,
            &json!({"type":"assistant","timestamp":"2026-07-21T18:00:03.000Z",
                    "message":{"stop_reason":"end_turn"}}),
            root,
        );
        assert_eq!(s.error, None);
        let now = parse_rfc3339_ms("2026-07-21T18:00:05.000Z").unwrap();
        assert_ne!(s.status(now, None), Status::Errored);
    }

    #[test]
    fn is_api_error_false_is_not_an_error() {
        let root = Path::new("/repo");
        let mut s = Session::default();
        // The field appears as `false` on ordinary messages; must not trip.
        fold_record(
            &mut s,
            &json!({"type":"assistant","isApiErrorMessage":false,
                    "message":{"stop_reason":"end_turn"}}),
            root,
        );
        assert_eq!(s.error, None);
    }
}
