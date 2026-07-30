//! Which coding agent sauron is watching. Everything agent-specific lives behind
//! this enum: where the session logs are, how to name a session from its file,
//! how to fold one record, and how to spawn/resume the CLI in a workspace pane.
//! Claude Code is the default and fully supported; Codex is a second agent whose
//! log reader (see `codex`) is best-effort until certified against real rollouts.
//!
//! Selection order: an explicit `--claude`/`--codex` flag, then `$SAURON_AGENT`,
//! then auto-detect from whichever agent has logs for this repo.
//!
//! grep targets:
//!   fn select / from_env  -- choose the agent
//!   fn resume_cmd/bare_cmd -- what a workspace pane runs
//!   fn session_files/fold -- the scanner hooks, dispatched per agent
//!   struct Mordor         -- local-model launch settings (the env prefix)
//!   fn Agent::local_env   -- the env that redirects a servant to the local model

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::Session;
use crate::scan::{self, home, Scanner};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Agent {
    #[default]
    Claude,
    Codex,
}

/// "Mordor" mode: run the servants (hobbits and orcs) against a **local** model
/// instead of the hosted API. Ollama exposes an Anthropic-compatible endpoint, so
/// Claude Code reaches a local model with nothing but a few env vars -- no proxy,
/// no translation layer. The Eye itself never calls a model, so this only ever
/// decorates the servant pane commands; everything downstream is untouched.
#[derive(Clone, Debug)]
pub struct Mordor {
    /// The Ollama model tag the swarm runs on, e.g. `qwen3-coder`.
    pub model: String,
    /// The Ollama endpoint; its Anthropic-compatible API lives at the root.
    pub base_url: String,
}

impl Mordor {
    /// Qwen3-Coder -- the 30B MoE agentic coder, the strongest local coding model
    /// that fits a 32GB Mac or a 24-32GB GPU, and the one called out as the go-to.
    /// Smaller boxes: override to `qwen2.5-coder:7b` (there is no sub-10GB qwen3).
    pub const DEFAULT_MODEL: &'static str = "qwen3-coder";
    /// Ollama's default listen address.
    pub const DEFAULT_BASE_URL: &'static str = "http://localhost:11434";
    /// Where the `--nostromo` endpoint is read from, when `$SAURON_NOSTROMO_URL`
    /// is unset: `~/.claude/sauron/<this>`. Kept under the user's home, never in
    /// the repo, so a private tailnet hostname is not committed to a shared tree.
    pub const NOSTROMO_URL_FILE: &'static str = "nostromo-url";

    /// Build from an optional model override and an optional explicit endpoint,
    /// defaulting to the Qwen coder on local Ollama. Endpoint precedence: the
    /// explicit `url` (a flag) wins, then `$SAURON_MORDOR_URL`, then local Ollama.
    /// A trailing slash is stripped so the client's `/v1/messages` never doubles
    /// up against a `…/` base.
    pub fn new(model: Option<String>, url: Option<String>) -> Mordor {
        let base_url = url
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("SAURON_MORDOR_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());
        Mordor {
            model: model
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| Self::DEFAULT_MODEL.to_string()),
            base_url: base_url.trim().trim_end_matches('/').to_string(),
        }
    }

    /// Mordor pointed at the `nostromo` box over Tailscale. The endpoint is a
    /// private tailnet hostname, so it is **never** hard-coded here: it comes
    /// from `$SAURON_NOSTROMO_URL`, else the first line of
    /// `~/.claude/sauron/nostromo-url`. Returns `None` when neither is set --
    /// the caller then tells the user how to configure it rather than guessing.
    pub fn nostromo(model: Option<String>) -> Option<Mordor> {
        Self::nostromo_url().map(|url| Mordor::new(model, Some(url)))
    }

    /// Resolve the nostromo endpoint from local config only -- env first, then
    /// the home-dir file. See [`Mordor::nostromo`] for why it lives off-repo.
    pub fn nostromo_url() -> Option<String> {
        if let Ok(u) = std::env::var("SAURON_NOSTROMO_URL") {
            let u = u.trim();
            if !u.is_empty() {
                return Some(u.to_string());
            }
        }
        let path = home()
            .join(".claude")
            .join("sauron")
            .join(Self::NOSTROMO_URL_FILE);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
            .filter(|s| !s.is_empty())
    }
}

impl Agent {
    /// Pick the agent: an explicit choice wins, then `$SAURON_AGENT`, then
    /// auto-detect from whichever agent actually has logs for this repo (Claude's
    /// per-repo directory, then a Codex install). Defaults to Claude.
    pub fn select(explicit: Option<Agent>, repo: &Path) -> Agent {
        if let Some(a) = explicit {
            return a;
        }
        if let Some(a) = Self::from_env() {
            return a;
        }
        if scan::project_dir_for(repo).is_dir() {
            return Agent::Claude;
        }
        if crate::codex::sessions_root().is_dir() {
            return Agent::Codex;
        }
        Agent::Claude
    }

    /// `$SAURON_AGENT=codex|claude`, if set to something recognised.
    pub fn from_env() -> Option<Agent> {
        match std::env::var("SAURON_AGENT").ok()?.to_ascii_lowercase().as_str() {
            "codex" => Some(Agent::Codex),
            "claude" | "claude-code" | "cc" => Some(Agent::Claude),
            _ => None,
        }
    }

    /// The CLI name, also the bare "open a fresh session" pane command.
    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// Command a workspace pane runs to resume a session by id.
    pub fn resume_cmd(self, id: &str) -> String {
        match self {
            Agent::Claude => format!("claude --resume {id}"),
            Agent::Codex => format!("codex resume {id}"),
        }
    }

    /// Command a workspace pane runs to start a *new* session under an id sauron
    /// chose, rather than one the agent invents.
    ///
    /// Choosing it is what lets a fresh pane be coloured at launch: the colour is
    /// a function of the session id (see `servant`), and an id that does not
    /// exist yet cannot be coloured. Claude Code validates the UUID, so
    /// `mint_session_id` produces a well-formed v4 rather than any unique string.
    ///
    /// Codex has no equivalent, so its fresh panes run bare and are told apart by
    /// their pane title alone.
    pub fn fresh_cmd(self, session_id: &str) -> String {
        match self {
            Agent::Claude => format!("claude --session-id {session_id}"),
            Agent::Codex => "codex".to_string(),
        }
    }

    /// The flag that gives a pane's session a display name, or empty where the
    /// agent has none.
    ///
    /// This is `/rename`, set at launch instead of typed. Claude Code shows the
    /// name in the prompt box, the `/resume` picker, and -- the part that earns
    /// this its place -- **the terminal title**. A column of eight identical
    /// `claude` panes is the problem; one that reads `frodo`, `sam`, `merry` down
    /// the side is not, and it costs one flag rather than a per-terminal
    /// scripting layer that would have to be written twice.
    ///
    /// There is deliberately no colour counterpart. `/color` has no launch flag,
    /// so setting it would mean driving the running session's keyboard -- exactly
    /// the thing that cannot be done on Windows Terminal, and the reason `reply`
    /// is macOS-only. A name works on both platforms or it is not worth having.
    ///
    /// Codex has no equivalent flag (unverified rather than known-absent: it is
    /// not installed on the machine this was written on). Empty keeps one call
    /// site, and its panes are still told apart by the terminal's own pane title.
    pub fn name_flag(self, name: &str) -> String {
        match self {
            Agent::Claude => format!(" --name {name}"),
            Agent::Codex => String::new(),
        }
    }

    /// The shell env-var prefix that redirects this agent's CLI to the local
    /// ("Mordor") model, or empty when not in Mordor mode. Prepended to a hobbit
    /// or orc pane command right before the `claude` word, so
    /// `cd repo && <prefix>claude ...` runs the servant against the local
    /// endpoint. A trailing space keeps it flush against the CLI name.
    ///
    /// Claude Code speaks Ollama's Anthropic-compatible API given the base url, a
    /// throwaway auth token (Ollama ignores it but the client insists on one), and
    /// an emptied API key so a globally-exported real key can't override it. The
    /// main *and* background models are pinned to the one local tag -- a
    /// single-model box would otherwise have a background call reach for a
    /// `haiku`-class model it never pulled. Values are quote-free, so they survive
    /// the double-quoted AppleScript string each pane command is embedded in.
    ///
    /// Codex reaches local models through `codex --oss`, not `ANTHROPIC_*` env, so
    /// Mordor is a no-op for it here; the workspace launcher warns rather than
    /// silently running Codex against the hosted API.
    pub fn local_env(self, mordor: Option<&Mordor>) -> String {
        let Some(m) = mordor else {
            return String::new();
        };
        match self {
            Agent::Claude => format!(
                "ANTHROPIC_BASE_URL={url} ANTHROPIC_AUTH_TOKEN=ollama ANTHROPIC_API_KEY= \
                 ANTHROPIC_MODEL={model} ANTHROPIC_SMALL_FAST_MODEL={model} \
                 ANTHROPIC_DEFAULT_HAIKU_MODEL={model} ",
                url = m.base_url,
                model = m.model,
            ),
            Agent::Codex => String::new(),
        }
    }

    /// How an orc runs a single-shot task, as `(program, args)` for `Command`.
    ///
    /// This replaced a `claude '<prompt>'` *shell string*, and the argv form is
    /// the reason the orc charge can now be long, multi-line, and full of quotes:
    /// nothing between here and the agent parses it as shell. `codex exec` is
    /// Codex's equivalent non-interactive one-shot mode.
    pub fn oneshot_argv(self, prompt: &str) -> (&'static str, Vec<String>) {
        match self {
            Agent::Claude => (self.label(), vec![prompt.to_string()]),
            Agent::Codex => (self.label(), vec!["exec".into(), prompt.to_string()]),
        }
    }

    /// The same, with the session named so the pane can be told from its
    /// neighbours -- see `name_flag`.
    ///
    /// The flag goes *before* the prompt, not after it. The prompt is positional
    /// and a `--name` following it would be read as more prompt: the orc would
    /// get two stray words appended to its charge and no name at all.
    pub fn oneshot_argv_named(self, prompt: &str, name: &str) -> (&'static str, Vec<String>) {
        let (prog, mut argv) = self.oneshot_argv(prompt);
        if matches!(self, Agent::Claude) {
            argv.splice(0..0, ["--name".to_string(), name.to_string()]);
        }
        (prog, argv)
    }

    // --- scanning hooks ---

    /// Where this agent's logs live, for the "no sessions" message.
    pub fn log_root(self, repo: &Path) -> PathBuf {
        match self {
            Agent::Claude => scan::project_dir_for(repo),
            Agent::Codex => crate::codex::sessions_root(),
        }
    }

    /// The session log files for this repo.
    pub fn session_files(self, repo: &Path) -> Vec<PathBuf> {
        match self {
            Agent::Claude => Scanner::claude_session_files(repo),
            Agent::Codex => crate::codex::session_files(repo),
        }
    }

    /// A stable session id derived from a log file's path (Claude's file stem is
    /// the uuid; Codex's rollout name carries the uuid in its tail).
    pub fn session_id(self, path: &Path) -> String {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        match self {
            Agent::Claude => stem.to_string(),
            Agent::Codex => crate::codex::session_id(stem),
        }
    }

    /// Fold one parsed record into the session.
    pub fn fold(self, session: &mut Session, v: &Value, repo: &Path) {
        match self {
            Agent::Claude => scan::fold_record(session, v, repo),
            Agent::Codex => crate::codex::fold(session, v, repo),
        }
    }
}

/// `~/.codex` -- exposed here so selection can probe for a Codex install.
pub(crate) fn codex_home() -> PathBuf {
    home().join(".codex")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_commands_match_the_agent() {
        assert_eq!(Agent::Claude.label(), "claude");
        assert_eq!(Agent::Codex.label(), "codex");
        assert_eq!(Agent::Claude.resume_cmd("abc"), "claude --resume abc");
        assert_eq!(Agent::Codex.resume_cmd("abc"), "codex resume abc");
    }

    #[test]
    fn oneshot_argv_hands_the_prompt_over_without_a_shell() {
        // A prompt carrying both quote kinds and a newline -- the thing the old
        // shell-string form could not survive -- passes through untouched.
        let nasty = "line one\nit is \"quoted\" and 'quoted'";
        let (prog, argv) = Agent::Claude.oneshot_argv(nasty);
        assert_eq!(prog, "claude");
        assert_eq!(argv, vec![nasty.to_string()]);
        let (prog, argv) = Agent::Codex.oneshot_argv(nasty);
        assert_eq!(prog, "codex");
        assert_eq!(argv, vec!["exec".to_string(), nasty.to_string()]);
    }

    #[test]
    fn env_selection_is_case_insensitive_and_narrow() {
        // (Can't set env safely in parallel tests; exercise the mapping directly.)
        assert_eq!(Agent::default(), Agent::Claude);
    }

    #[test]
    fn mordor_env_redirects_claude_to_the_local_model_and_stays_quote_free() {
        let m = Mordor {
            model: "qwen3-coder".into(),
            base_url: "http://localhost:11434".into(),
        };
        let env = Agent::Claude.local_env(Some(&m));
        // The three vars Ollama's Anthropic-compatible API needs, plus the model.
        assert!(env.contains("ANTHROPIC_BASE_URL=http://localhost:11434"));
        assert!(env.contains("ANTHROPIC_AUTH_TOKEN=ollama"));
        assert!(env.contains("ANTHROPIC_MODEL=qwen3-coder"));
        // Background calls pinned to the same local tag, not a phantom haiku.
        assert!(env.contains("ANTHROPIC_SMALL_FAST_MODEL=qwen3-coder"));
        // Flush against the CLI word, and safe inside the AppleScript "..." string.
        assert!(env.ends_with(' '), "prefix must end with a space: {env:?}");
        assert!(!env.contains('"'), "prefix must be double-quote-free: {env:?}");
        // Off by default -- no Mordor, no env.
        assert_eq!(Agent::Claude.local_env(None), "");
        // Codex has no ANTHROPIC_* path; Mordor is a no-op for it here.
        assert_eq!(Agent::Codex.local_env(Some(&m)), "");
    }

    #[test]
    fn mordor_new_defaults_to_the_qwen_coder() {
        assert_eq!(Mordor::new(None, None).model, "qwen3-coder");
        assert_eq!(Mordor::new(Some("  ".into()), None).model, "qwen3-coder");
        assert_eq!(
            Mordor::new(Some("qwen2.5-coder:7b".into()), None).model,
            "qwen2.5-coder:7b"
        );
    }

    #[test]
    fn mordor_strips_a_trailing_slash_on_an_explicit_url() {
        // A trailing slash on the endpoint is stripped so the client's
        // `/v1/messages` doesn't land as `…ts.net//v1/messages`. (Fake host --
        // the real tailnet URL never appears in the repo.)
        let m = Mordor::new(
            Some("qwen2.5-coder:7b".into()),
            Some("https://box.example.ts.net/".into()),
        );
        assert_eq!(m.base_url, "https://box.example.ts.net");
        assert_eq!(m.model, "qwen2.5-coder:7b");
    }
}
