//! Strict, opt-in clipboard lifecycle for workspace agents.
//!
//! The wrapper loads project context before spawning an agent and verifies a
//! nonce-bearing handoff after the child exits. Without `--clipboard-handoff`,
//! workspace commands never enter this module.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::agent::Agent;
use crate::clip::store::{ListOptions, Store};

pub fn run(args: &[String]) -> io::Result<()> {
    let parsed = HandoffArgs::parse(args).map_err(invalid)?;
    let repo = std::fs::canonicalize(&parsed.repo).unwrap_or(parsed.repo);
    let store = Store::open(parsed.db.as_deref()).map_err(other)?;
    let namespace = project_namespace(&repo);
    let prior = store.get(&parsed.key).map_err(other)?;
    let prior_version = prior.as_ref().map_or(0, |item| item.version);
    let nonce = pass_nonce(&parsed.key);
    let context = load_context(&store, prior.as_ref(), &namespace).map_err(other)?;
    eprintln!(
        "sauron handoff: db={} key={} prior_version={}",
        store.path().display(),
        parsed.key,
        prior_version
    );
    let exe = std::env::current_exe()?;
    let prompt = lifecycle_prompt(
        parsed.task.as_deref(),
        &context,
        &exe,
        store.path(),
        &parsed.key,
        &namespace,
        &nonce,
    );

    let status = spawn_agent(
        parsed.agent,
        parsed.resume.as_deref(),
        parsed.non_interactive,
        &repo,
        &prompt,
    )?;

    // Re-open so verification proves the child write is visible to an
    // independent connection, not merely cached in this process.
    drop(store);
    let verify = Store::open(parsed.db.as_deref()).map_err(other)?;
    let handoff = verify.get(&parsed.key).map_err(other)?;
    let valid = handoff.as_ref().is_some_and(|item| {
        item.version > prior_version
            && serde_json::from_str::<Value>(&item.value)
                .ok()
                .and_then(|value| {
                    value
                        .get("pass_nonce")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .as_deref()
                == Some(nonce.as_str())
    });
    if !valid {
        eprintln!(
            "sauron handoff: pass incomplete; {} was not updated with nonce {}",
            parsed.key, nonce
        );
        std::process::exit(3);
    }
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

pub fn workspace_command(
    repo: &Path,
    sauron_exe: &Path,
    agent: Agent,
    key: &str,
    resume: Option<&str>,
    task: Option<&str>,
    non_interactive: bool,
) -> String {
    let mut parts = vec![
        shell_quote(sauron_exe.to_string_lossy().as_ref()),
        "handoff-run".into(),
        "--agent".into(),
        agent.label().into(),
        "--repo".into(),
        shell_quote(repo.to_string_lossy().as_ref()),
        "--key".into(),
        shell_quote(key),
        "--db".into(),
        shell_quote(
            crate::clip::store::db_path_from(repo)
                .to_string_lossy()
                .as_ref(),
        ),
    ];
    if let Some(resume) = resume {
        parts.push("--resume".into());
        parts.push(shell_quote(resume));
    }
    if let Some(task) = task {
        parts.push("--task".into());
        parts.push(shell_quote(task));
    }
    if non_interactive {
        parts.push("--non-interactive".into());
    }
    format!(
        "cd {} && {}",
        shell_quote(repo.to_string_lossy().as_ref()),
        parts.join(" ")
    )
}

pub fn project_namespace(repo: &Path) -> String {
    let canonical = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    let slug = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let digest = format!(
        "{:x}",
        Sha256::digest(canonical.to_string_lossy().as_bytes())
    );
    format!("sauron.{slug}.{}", &digest[..12])
}

pub fn handoff_key(repo: &Path, lane: &str) -> String {
    format!("{}.{}.handoff", project_namespace(repo), lane)
}

fn load_context(
    store: &Store,
    prior: Option<&crate::clip::store::Item>,
    namespace: &str,
) -> Result<String, String> {
    let mut items = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(item) = prior {
        seen.insert(item.key.clone());
        items.push(item.clone());
    }
    for item in store.list(
        12,
        &ListOptions {
            namespace: Some(namespace.into()),
            ..ListOptions::default()
        },
    )? {
        if seen.insert(item.key.clone()) {
            items.push(item);
        }
    }
    let mut context = prior_status(
        prior.map(|item| item.key.as_str()).unwrap_or_default(),
        prior.map(|item| item.version),
    );
    if items.is_empty() {
        context.push_str("\n(no prior project clipboard entries; this is the first pass)");
        return Ok(context);
    }
    for item in items {
        let block = format!(
            "\n--- {} [{}; v{}; validated={}; pinned={}] ---\n{}\n",
            item.key, item.kind, item.version, item.validated, item.pinned, item.value
        );
        if context.len() + block.len() > 24_000 {
            context.push_str("\n[remaining clipboard context omitted at 24 KiB]\n");
            break;
        }
        context.push_str(&block);
    }
    Ok(context)
}

fn prior_status(key: &str, version: Option<i64>) -> String {
    match version {
        Some(version) => format!(
            "SAURON_PRIOR_HANDOFF_STATUS=loaded\n\
             SAURON_PRIOR_HANDOFF_KEY={key}\n\
             SAURON_PRIOR_HANDOFF_VERSION={version}\n"
        ),
        None => "SAURON_PRIOR_HANDOFF_STATUS=absent_first_pass\n\
                 SAURON_PRIOR_HANDOFF_VERSION=0\n"
            .into(),
    }
}

fn lifecycle_prompt(
    task: Option<&str>,
    context: &str,
    sauron_exe: &Path,
    db: &Path,
    key: &str,
    namespace: &str,
    nonce: &str,
) -> String {
    let task = task
        .map(|task| format!("\nYour assigned task for this pass:\n{task}\n"))
        .unwrap_or_default();
    let command = format!(
        "{} clip --db {} put {} --namespace {} --kind checkpoint --tags handoff --validated",
        shell_quote(sauron_exe.to_string_lossy().as_ref()),
        shell_quote(db.to_string_lossy().as_ref()),
        shell_quote(key),
        shell_quote(namespace),
    );
    format!(
        "SAURON STRICT CLIPBOARD PASS\n\
         Before inspecting the repository, absorb the controller-loaded clipboard context below. \
         Treat it as continuity evidence, but verify load-bearing claims against the repo.\n\
         {context}\n\
         {task}\n\
         Before ending this pass, write one JSON object to the exact handoff key. It must contain: \
         pass_nonce, work_completed, repo_state, tests, invariants_and_decisions, blockers, and next_action. \
         The pass_nonce must be exactly {nonce}. Pipe the JSON on stdin to this command:\n\
         {command}\n\
         Then read the exact key back with the same executable and database. Do not report completion \
         until both commands succeed. Sauron will mark the pass incomplete if the nonce-bearing handoff \
         is absent or unreadable when the agent process exits."
    )
}

fn spawn_agent(
    agent: Agent,
    resume: Option<&str>,
    non_interactive: bool,
    repo: &Path,
    prompt: &str,
) -> io::Result<ExitStatus> {
    // Not `agent.label()` directly: on Windows the agent is usually a `.cmd`
    // shim, which `Command::new` will not find on its own.
    let mut command = Command::new(crate::plat::resolve_program(agent.label()));
    match (agent, non_interactive, resume) {
        (Agent::Claude, _, Some(id)) => {
            command.args(["--resume", id]).arg(prompt);
        }
        (Agent::Claude, _, None) => {
            command.arg(prompt);
        }
        (Agent::Codex, true, _) => {
            command.arg("exec").arg(prompt);
        }
        (Agent::Codex, false, Some(id)) => {
            command.args(["resume", id]).arg(prompt);
        }
        (Agent::Codex, false, None) => {
            command.arg(prompt);
        }
    }
    command.current_dir(repo).status()
}

fn pass_nonce(key: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let material = format!("{key}:{}:{nanos}", std::process::id());
    format!("{:x}", Sha256::digest(material.as_bytes()))[..20].to_string()
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[derive(Debug)]
struct HandoffArgs {
    repo: PathBuf,
    key: String,
    db: Option<PathBuf>,
    agent: Agent,
    resume: Option<String>,
    task: Option<String>,
    non_interactive: bool,
}

impl HandoffArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut repo = None;
        let mut key = None;
        let mut db = None;
        let mut agent = None;
        let mut resume = None;
        let mut task = None;
        let mut non_interactive = false;
        let mut index = 0;
        while index < args.len() {
            let name = args[index].as_str();
            if name == "--non-interactive" {
                non_interactive = true;
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("{name} requires a value"))?
                .clone();
            match name {
                "--repo" => repo = Some(PathBuf::from(value)),
                "--key" => key = Some(value),
                "--db" => db = Some(PathBuf::from(value)),
                "--agent" => {
                    agent = Some(match value.as_str() {
                        "claude" => Agent::Claude,
                        "codex" => Agent::Codex,
                        _ => return Err(format!("unknown agent: {value}")),
                    })
                }
                "--resume" => resume = Some(value),
                "--task" => task = Some(value),
                _ => return Err(format!("unknown handoff option: {name}")),
            }
            index += 2;
        }
        Ok(Self {
            repo: repo.ok_or_else(|| "--repo is required".to_string())?,
            key: key.ok_or_else(|| "--key is required".to_string())?,
            db,
            agent: agent.ok_or_else(|| "--agent is required".to_string())?,
            resume,
            task,
            non_interactive,
        })
    }
}

fn invalid(error: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

fn other(error: String) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_is_stable_and_keys_are_lane_specific() {
        let repo = Path::new("/tmp/example repo");
        let namespace = project_namespace(repo);
        assert!(namespace.starts_with("sauron.example-repo."));
        assert_eq!(
            handoff_key(repo, "slot.2"),
            format!("{namespace}.slot.2.handoff")
        );
    }

    #[test]
    fn workspace_command_routes_through_strict_wrapper() {
        let command = workspace_command(
            Path::new("/repo"),
            Path::new("/bin/sauron"),
            Agent::Codex,
            "key",
            Some("session"),
            None,
            false,
        );
        assert!(command.contains("handoff-run"));
        assert!(command.contains("--resume 'session'"));
        assert!(command.contains("--agent codex"));
        assert!(command.contains("--db"));
    }

    #[test]
    fn prompt_requires_structured_nonce_handoff() {
        let prompt = lifecycle_prompt(
            None,
            "SAURON_PRIOR_HANDOFF_STATUS=absent_first_pass\ncontext",
            Path::new("/bin/sauron"),
            Path::new("/tmp/clip.sqlite3"),
            "key",
            "namespace",
            "nonce",
        );
        assert!(prompt.contains("pass_nonce must be exactly nonce"));
        assert!(prompt.contains("clip --db"));
        assert!(prompt.contains("repo_state"));
        assert!(prompt.contains("SAURON_PRIOR_HANDOFF_STATUS=absent_first_pass"));
    }

    #[test]
    fn prompt_marks_an_exact_prior_handoff_as_loaded() {
        let status = prior_status("lane.key", Some(3));
        assert!(status.contains("SAURON_PRIOR_HANDOFF_STATUS=loaded"));
        assert!(status.contains("SAURON_PRIOR_HANDOFF_KEY=lane.key"));
        assert!(status.contains("SAURON_PRIOR_HANDOFF_VERSION=3"));
    }
}
