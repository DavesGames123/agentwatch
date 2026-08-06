//! Reader for OpenAI Codex CLI rollout logs (`~/.codex/sessions/**/*.jsonl`).
//!
//! ⚠ BEST-EFFORT, PENDING CERTIFICATION. This machine has no Codex install, so
//! the format below is implemented from the documented rollout shape, not
//! validated against real files. It is deliberately defensive -- it unwraps a
//! `{type, payload}` envelope if present, reads fields at either level, and
//! degrades to "session exists, fewer signals" rather than crashing on anything
//! unexpected. To certify it, run sauron with `SAURON_AGENT=codex` (or `--codex`)
//! against a repo you've used Codex in; if edits/prompts look wrong, one real
//! rollout jsonl pins the exact field names.
//!
//! What it maps into the shared `Session` model:
//!   - session cwd (for discovery) and id (from the rollout filename)
//!   - user / assistant messages -> last prompt + turn completion
//!   - apply_patch tool calls -> the write-set (files touched)
//!
//! WHY THE CWD PROBE IS CACHED
//! ---------------------------
//! Claude Code keeps one directory per repo, so "which files are mine" is a
//! single `read_dir`. Codex keeps one flat store for every repo on the machine,
//! so the same question means opening every rollout and reading its header --
//! and `Scanner::refresh` asks it on every 2s tick. On a store with a few
//! hundred rollouts that is a few hundred opens and up to eight parsed JSON
//! lines each, twice a second, forever; a codex session-meta header carries the
//! whole instructions blob, so those lines are not small. The header of a
//! rollout never changes once written, so the verdict is remembered per file and
//! the probe runs once per rollout instead of once per tick.
//!
//! grep targets:
//!   fn session_files   -- rollouts whose cwd is this repo
//!   fn has_session_for -- the same question, answered on the first hit
//!   fn rollout_cwd     -- the cached header probe
//!   fn fold            -- one rollout record -> Session mutation
//!   fn patch_paths     -- files out of an apply_patch envelope

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::agent::codex_home;
use crate::model::{parse_rfc3339_ms, Session};
use crate::scan::repo_relative;

/// `<codex home>/sessions` -- `~/.codex/sessions` unless `$CODEX_HOME` moved it.
pub fn sessions_root() -> PathBuf {
    codex_home().join("sessions")
}

/// The session id from a `rollout-<date>-<uuid>` file stem: the trailing five
/// dash-groups (a uuid), else the whole stem.
pub fn session_id(stem: &str) -> String {
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 {
        parts[parts.len() - 5..].join("-")
    } else {
        stem.to_string()
    }
}

/// Rollout files under `~/.codex/sessions` whose recorded cwd is this repo.
pub fn session_files(repo: &Path) -> Vec<PathBuf> {
    let repo_s = repo.to_string_lossy();
    let mut files = Vec::new();
    collect_jsonl(&sessions_root(), &mut files, 5);
    files.retain(|p| rollout_cwd(p).as_deref() == Some(repo_s.as_ref()));
    files
}

/// Whether Codex has ever run in this repo. This is the agent-selection
/// question, and it is not the same as "is Codex installed": a `~/.codex`
/// directory says the user has the CLI, not that *this* repo is one they use it
/// in. Answering the weaker question is what made a repo with no Claude logs
/// open a column of Codex panes the user never asked for.
///
/// Stops at the first match rather than building the whole list -- the answer is
/// a boolean and the store is every repo on the machine.
pub fn has_session_for(repo: &Path) -> bool {
    let repo_s = repo.to_string_lossy();
    let mut files = Vec::new();
    collect_jsonl(&sessions_root(), &mut files, 5);
    files
        .iter()
        .any(|p| rollout_cwd(p).as_deref() == Some(repo_s.as_ref()))
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                collect_jsonl(&p, out, depth - 1);
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

/// Remembered answers from [`rollout_cwd`], keyed by rollout path.
///
/// `None` is cached as well as `Some`. A rollout belonging to another repo is
/// the common case and is exactly the one worth not re-reading; caching only the
/// hits would leave every miss re-parsed on every tick, which is the whole cost.
fn cwd_cache() -> &'static Mutex<HashMap<PathBuf, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cwd a rollout was recorded in, from its session-meta header (scanned over
/// the first few lines, wherever the meta lands). Read once per rollout; see the
/// module header.
fn rollout_cwd(path: &Path) -> Option<String> {
    if let Ok(cache) = cwd_cache().lock() {
        if let Some(hit) = cache.get(path) {
            return hit.clone();
        }
    }
    let found = read_rollout_cwd(path);
    // A file whose header has not been written yet must not be remembered as
    // "no cwd" -- codex creates the rollout and writes the meta line a moment
    // later, and a cached `None` would hide that session for the process's life.
    if found.is_some() {
        if let Ok(mut cache) = cwd_cache().lock() {
            cache.insert(path.to_path_buf(), found.clone());
        }
    }
    found
}

fn read_rollout_cwd(path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    for _ in 0..8 {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
            if let Some(c) = field(&v, "cwd") {
                return Some(c.to_string());
            }
        }
    }
    None
}

/// Fold one Codex rollout record into the session.
pub fn fold(session: &mut Session, v: &Value, repo: &Path) {
    if let Some(ms) = field(v, "timestamp").and_then(parse_rfc3339_ms) {
        if ms > session.last_activity {
            session.last_activity = ms;
        }
    }

    // A record may be a raw item or wrapped as {type, payload}; fold the item.
    let item = v.get("payload").unwrap_or(v);
    match item.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "message" => match item.get("role").and_then(|r| r.as_str()).unwrap_or("") {
            "user" => {
                session.turn_complete = false;
                let text = message_text(item);
                let t = text.trim();
                if t.contains(crate::model::ORC_MARKER) {
                    session.is_orc = true;
                }
                if !t.is_empty() {
                    session.last_prompt = Some(t.to_string());
                }
            }
            "assistant" => session.turn_complete = true,
            _ => {}
        },
        // Any tool call means the turn is still in flight; an apply_patch also
        // tells us which files were written.
        "function_call" | "local_shell_call" | "custom_tool_call" => {
            session.turn_complete = false;
            let ts = session.last_activity;
            for path in patch_paths(item) {
                if let Some(rel) = repo_relative(&path, repo) {
                    session
                        .edits
                        .entry(rel)
                        .and_modify(|t| {
                            if ts > *t {
                                *t = ts;
                            }
                        })
                        .or_insert(ts);
                }
            }
        }
        _ => {}
    }
}

/// A string field at the top level or one level into `payload`.
fn field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .or_else(|| v.get("payload").and_then(|p| p.get(key)).and_then(|x| x.as_str()))
}

/// Concatenate a message item's content blocks into plain text.
fn message_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// The repo files an apply_patch touched. Codex wraps patches in an
/// `*** Begin Patch / *** Update|Add|Delete File: <path> / *** End Patch`
/// envelope, which can sit in the call's `arguments`/`input` directly or nested
/// inside a JSON string there -- so hunt every string in the item for it.
fn patch_paths(item: &Value) -> Vec<String> {
    let Some(patch) = find_patch_text(item) else {
        return Vec::new();
    };
    patch
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                .iter()
                .find_map(|tag| l.strip_prefix(tag))
                .map(|rest| rest.trim().to_string())
        })
        .collect()
}

fn find_patch_text(item: &Value) -> Option<String> {
    for s in collect_strings(item) {
        // `arguments` is often itself a JSON string carrying the patch under a
        // field -- parse it first, since the parsed inner string has real
        // newlines while the outer wrapper keeps them escaped.
        if let Ok(j) = serde_json::from_str::<Value>(&s) {
            if let Some(inner) = collect_strings(&j).into_iter().find(|x| is_patch(x)) {
                return Some(inner);
            }
        }
        if is_patch(&s) {
            return Some(s);
        }
    }
    None
}

fn is_patch(s: &str) -> bool {
    s.contains("*** ") && s.contains(" File: ")
}

fn collect_strings(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(v, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_id_takes_the_uuid_off_a_rollout_name() {
        assert_eq!(
            session_id("rollout-2026-07-22T10-30-00-6c6f86f2-1234-4abc-8def-0123456789ab"),
            "6c6f86f2-1234-4abc-8def-0123456789ab"
        );
        assert_eq!(session_id("plainname"), "plainname");
    }

    #[test]
    fn apply_patch_arguments_yield_the_touched_files() {
        // arguments as a JSON string carrying the patch (the common shape).
        let item = json!({
            "type": "function_call",
            "name": "apply_patch",
            "arguments": "{\"input\":\"*** Begin Patch\\n*** Update File: src/a.rs\\n@@\\n-old\\n+new\\n*** Add File: src/b.rs\\n+fn x() {}\\n*** End Patch\"}"
        });
        let mut paths = patch_paths(&item);
        paths.sort();
        assert_eq!(paths, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
    }

    #[test]
    fn apply_patch_as_raw_arguments_also_parses() {
        let item = json!({
            "type": "custom_tool_call",
            "input": "*** Begin Patch\n*** Update File: lib/x.ts\n+a\n*** End Patch"
        });
        assert_eq!(patch_paths(&item), vec!["lib/x.ts".to_string()]);
    }

    #[test]
    fn a_rollout_header_is_read_once_and_a_missing_one_is_not_remembered() {
        let dir = std::env::temp_dir().join(format!("sauron-codex-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-08-06T10-00-00-11111111-2222-4333-8444-555555555555.jsonl");

        // A rollout whose meta line has not landed yet answers "unknown", and
        // that answer must not stick -- codex writes the header a moment after
        // it creates the file, and a cached `None` would hide the session.
        std::fs::write(&path, b"").unwrap();
        assert_eq!(rollout_cwd(&path), None);
        std::fs::write(
            &path,
            b"{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/repo/one\"}}\n",
        )
        .unwrap();
        assert_eq!(rollout_cwd(&path).as_deref(), Some("/repo/one"));

        // A header that *was* read is not read again: rewriting the file with a
        // different cwd changes nothing, which is the cache doing its job.
        std::fs::write(
            &path,
            b"{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/repo/two\"}}\n",
        )
        .unwrap();
        assert_eq!(rollout_cwd(&path).as_deref(), Some("/repo/one"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fold_tracks_prompt_edits_and_turn_completion() {
        let repo = Path::new("/repo");
        let mut s = Session::default();

        // A wrapped user message opens a turn and sets the prompt.
        fold(
            &mut s,
            &json!({"type":"response_item","timestamp":"2026-07-22T10:00:00.000Z",
                    "payload":{"type":"message","role":"user",
                               "content":[{"type":"input_text","text":"refactor the parser"}]}}),
            repo,
        );
        assert_eq!(s.last_prompt.as_deref(), Some("refactor the parser"));
        assert!(!s.turn_complete);

        // An apply_patch records the write-set and keeps the turn in flight.
        fold(
            &mut s,
            &json!({"type":"response_item","timestamp":"2026-07-22T10:00:05.000Z",
                    "payload":{"type":"function_call","name":"apply_patch",
                               "arguments":"*** Begin Patch\n*** Update File: src/parser.rs\n+x\n*** End Patch"}}),
            repo,
        );
        assert!(s.edits.contains_key("src/parser.rs"));
        assert!(!s.turn_complete);

        // A final assistant message settles the turn.
        fold(
            &mut s,
            &json!({"type":"response_item",
                    "payload":{"type":"message","role":"assistant",
                               "content":[{"type":"output_text","text":"done"}]}}),
            repo,
        );
        assert!(s.turn_complete);
    }
}
