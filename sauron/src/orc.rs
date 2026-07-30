//! The orc charge -- what a single-shot maintenance agent is told to do, which
//! file it is pointed at, and how that dispatch reaches a pane.
//!
//! An orc is loosed on a *cold* file: one no live session is editing and git
//! reports clean. Its brief is built here rather than assembled into a shell
//! string by the launcher, because the pane command is then short enough to read
//! before you press Enter (`sauron orc src/big.rs`), and the brief itself is free
//! of shell and AppleScript quoting -- it reaches the agent through
//! `Command::arg`, never through `sh -c`. The old inline-prompt form had to stay
//! single-line and free of both quote kinds; nothing here does.
//!
//! The charge is *ranked*, not a list of equal wishes. Decomposition outranks
//! shrinking, which outranks speed -- and all three are subordinate to the hard
//! constraint that the tree keep compiling while the orc works. A hobbit is
//! building this repo the whole time, so a red build is not a private
//! intermediate state.
//!
//! grep targets:
//!   fn brief             -- the metaprompt, priority-ordered
//!   fn brief_oneline     -- the same text flattened for AppleScript embedding
//!   struct Checks/detect -- the build/test/boot commands the brief cites
//!   struct Target/survey -- cold-file ranking (LOC + churn, not bytes)
//!   fn stage_command     -- the one-liner a pane runs: `sauron orc <file>`
//!   fn run               -- the `sauron orc <file>` subcommand itself

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use crate::agent::Agent;

// ---------------------------------------------------------------------------
// The charge
// ---------------------------------------------------------------------------

/// The single-shot brief an orc is handed. Carries `model::ORC_MARKER` in its
/// first line so the scanner recognises the session as one of sauron's own and
/// badges it apart from the hobbits -- see the test that pins this, because the
/// marker is prose and a well-meaning reword would otherwise disable the badge
/// with nothing going red.
///
/// The text is deliberately free of both quote kinds, so `brief_oneline` can be
/// embedded in a shell command inside an AppleScript string literal without
/// escaping. Keep it that way when editing.
pub fn brief(target: &str, checks: &Checks) -> String {
    format!(
        "Single-shot maintenance pass on {target}. It is safe to refactor: {marker}.

HARD CONSTRAINT, above every goal below. This repository must compile, and the
program must still boot, at every moment you are working -- not merely when you
finish. Someone else is building and running this tree while you edit it, so a
red build is not a private intermediate state you can clean up later.

  - Before your first edit, run {build} and record the result. If the tree is
    already red, stop and report that. Do not refactor on top of a broken build.
  - Work in steps that each leave the build green. Additive first: create the new
    module, wire it into its parent, and compile, BEFORE you delete anything from
    the original file.
  - Re-run {build} after every step. If a step turns the build red and your very
    next edit does not make it green, revert that step and move on.
  - Never edit the build manifest, the entrypoint, or any file outside your
    target set in order to make your change compile. If your change needs that,
    it is out of scope: stop and report it.
  - Finish with {build} and {test} green, {boot} and paste the output of each.

PRIORITIES, highest first. Do the highest-ranked work that genuinely applies. Do
not drop to a lower rank because it is easier, and do not spread yourself thinly
across all three.

  1. DECOMPOSE -- this outranks the other two combined. If {target} carries more
     than one responsibility, split it into a nested module tree: one concern per
     file, each file opening with a header saying what it is for and listing the
     grep targets that reach its parts. A correct split is a complete result on
     its own. Ship it and stop rather than starting something else.
  2. SHRINK. Cut the line count of what remains without losing behaviour or
     documentation: delete dead code and unused imports, collapse duplicated
     branches and near-identical functions, replace hand-rolled loops with the
     standard idiom of this language. Report the before and after line count of
     every file you touched.
  3. SPEED, and only where the code itself makes the win evident: hoist repeated
     work out of a loop, drop a clone or an allocation of something large, replace
     a repeated linear scan with a lookup. State in one line why each change is
     faster. Do not guess at micro-optimizations you cannot argue for from the
     code in front of you, and never trade clarity for a speculative gain.

Behaviour must be identical. If a behaviour you are about to move has no test,
write one first. Confine every edit to {target} and the files you split out of it.",
        marker = crate::model::ORC_MARKER,
        build = checks.build_phrase(),
        test = checks.test_phrase(),
        boot = checks.boot_phrase(),
    )
}

/// [`brief`] flattened to a single line. Only for the one path that still has to
/// carry the prompt as text: `--clipboard-handoff` embeds it as a shell argument
/// inside an AppleScript double-quoted string, and a literal newline there is a
/// syntax error. Every other caller should hand over `sauron orc <file>` instead.
pub fn brief_oneline(target: &str, checks: &Checks) -> String {
    brief(target, checks)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// The commands the brief tells the orc to keep green
// ---------------------------------------------------------------------------

/// Env var naming the command that proves the program still *boots*. There is no
/// ecosystem default for this one on purpose -- see [`detect`].
pub const SMOKE_ENV: &str = "SAURON_ORC_SMOKE";

/// The build, test, and boot commands an orc is told to run. Any of them may be
/// unknown, in which case the brief asks for the repository's own equivalent
/// rather than naming a command that does not exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checks {
    pub build: Option<String>,
    pub test: Option<String>,
    pub smoke: Option<String>,
    /// Repo-relative directory the commands run in, when that is not the repo
    /// root. A crate in a subdirectory is the common case (this repo is one), and
    /// `cargo build` from the wrong directory is exactly the kind of failure that
    /// would teach an orc to give up on the build check.
    pub dir: Option<String>,
}

impl Checks {
    /// A command with the directory it must run in, when that is not the root.
    fn located(&self, cmd: &str) -> String {
        match &self.dir {
            Some(d) => format!("{cmd} (from {d}/)"),
            None => cmd.to_string(),
        }
    }

    fn build_phrase(&self) -> String {
        match &self.build {
            Some(c) => self.located(c),
            None => "the repository build".into(),
        }
    }

    fn test_phrase(&self) -> String {
        match &self.test {
            Some(c) => self.located(c),
            None => "the repository test suite".into(),
        }
    }

    /// The clause that closes the finish line. Named command when one is
    /// configured; otherwise an explicit instruction to find the repo's own way
    /// of starting the program -- never a silent omission, because the boot
    /// check is half of what the hard constraint is promising.
    fn boot_phrase(&self) -> String {
        match &self.smoke {
            Some(cmd) => format!("run {cmd} to prove the program still boots,"),
            None => {
                "prove the program still boots by whatever means this repository \
                 documents,"
                    .into()
            }
        }
    }
}

/// Detect the build and test commands for the project `target` belongs to, and
/// read the boot check from `$SAURON_ORC_SMOKE`.
///
/// The manifest is looked for from the target's own directory *upward*, not at
/// the repo root, because the crate is very often in a subdirectory -- this repo
/// keeps its whole Rust project under `sauron/`. Probing only the root found
/// nothing here and quietly downgraded the brief to "the repository build",
/// which is the one instruction an orc cannot act on.
///
/// Build and test are safe to infer: a `Cargo.toml` really does mean `cargo
/// build`. Whether a program *boots* is a property of the program, not of the
/// toolchain -- `cargo build` succeeding says nothing about whether `main`
/// reaches its event loop -- so guessing one would have the orc certify a boot it
/// never performed. An honest gap beats a fabricated check, and the brief adapts.
pub fn detect(repo: &Path, target: &str) -> Checks {
    let smoke = std::env::var(SMOKE_ENV).ok().filter(|s| !s.trim().is_empty());

    // Walk up from the target's directory to the repo root, first manifest wins.
    let mut dir = repo.join(target);
    dir.pop();
    loop {
        let has = |f: &str| dir.join(f).exists();
        let found = if has("Cargo.toml") {
            // `--all-targets` so a refactor that breaks a test or bench module is
            // caught by the *build* step, not left for the test step to find.
            Some(("cargo build --all-targets", "cargo test"))
        } else if has("go.mod") {
            Some(("go build ./...", "go test ./..."))
        } else if has("package.json") {
            Some(("npm run build --if-present", "npm test"))
        } else if has("pyproject.toml") || has("setup.py") {
            Some(("python -m compileall -q .", "pytest -q"))
        } else {
            None
        };
        if let Some((build, test)) = found {
            let rel = dir
                .strip_prefix(repo)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty());
            return Checks {
                build: Some(build.into()),
                test: Some(test.into()),
                smoke,
                dir: rel,
            };
        }
        if dir == repo || !dir.pop() {
            return Checks {
                smoke,
                ..Checks::default()
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Cold-code detection: the safe, uncontested files an orc can be handed
// ---------------------------------------------------------------------------

/// How many lines of code one recent commit touching a file is worth when
/// ranking targets. Churn is the signal that a file is structurally wrong -- it
/// gets reopened over and over because nobody can find anything in it -- but it
/// is a weaker signal than sheer length, which is what decomposition actually
/// acts on. At 40, a 300-line file touched ten times edges out a 650-line file
/// nobody has needed to open.
const CHURN_LOC_EQUIV: usize = 40;

/// How many commits back churn is counted over.
const CHURN_DEPTH: &str = "200";

/// One candidate file, with the evidence behind its rank so the picker can show
/// *why* it is at the top rather than asking to be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: String,
    pub loc: usize,
    /// Commits touching this path within [`CHURN_DEPTH`].
    pub churn: usize,
    pub score: usize,
}

/// The cold candidates, best first, plus a count of what was excluded and why.
/// The counts exist so the picker can say "12 hot, 3 dirty excluded" -- silent
/// filtering reads as "there was nothing else", which is a different claim.
#[derive(Debug, Clone, Default)]
pub struct Survey {
    pub cold: Vec<Target>,
    pub hot: usize,
    pub dirty: usize,
}

/// Rank the tracked source files no live session is touching and git reports
/// clean. `hot` is the set of repo-relative paths active sessions have edited.
///
/// Ranking is `loc + 40*churn`, not file size in bytes. Bytes penalise a
/// well-documented file for its header, which in this codebase is exactly
/// backwards -- the best-commented file would sort as the fattest target.
///
/// This reads every tracked source file to count its lines. That is fine at the
/// scale it runs (a keypress, or one workspace launch) and would want a cap on a
/// very large repository.
pub fn survey(repo: &Path, hot: &BTreeSet<String>) -> Survey {
    let dirty: BTreeSet<String> = git_lines(repo, &["status", "--porcelain"])
        .iter()
        .filter_map(|l| l.get(3..).map(|s| s.trim().to_string()))
        .collect();

    let mut churn: BTreeMap<String, usize> = BTreeMap::new();
    for line in git_lines(
        repo,
        &["log", "--format=", "--name-only", "-n", CHURN_DEPTH],
    ) {
        let path = line.trim();
        if !path.is_empty() {
            *churn.entry(path.to_string()).or_default() += 1;
        }
    }

    let mut survey = Survey::default();
    for path in git_lines(repo, &["ls-files"]) {
        if !is_code(&path) {
            continue;
        }
        if hot.contains(&path) {
            survey.hot += 1;
            continue;
        }
        if dirty.contains(&path) {
            survey.dirty += 1;
            continue;
        }
        let Ok(text) = std::fs::read_to_string(repo.join(&path)) else {
            continue;
        };
        let loc = text.lines().count();
        let churn = churn.get(&path).copied().unwrap_or(0);
        survey.cold.push(Target {
            path,
            loc,
            churn,
            score: loc + CHURN_LOC_EQUIV * churn,
        });
    }
    // Best first; ties broken by path so the order is stable across runs.
    survey
        .cold
        .sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    survey
}

/// Whether a path is source an orc should refactor -- a known code extension,
/// never a lockfile.
pub fn is_code(path: &str) -> bool {
    const EXT: &[&str] = &[
        "rs", "ts", "tsx", "js", "jsx", "py", "go", "rb", "java", "kt", "kts", "swift", "c", "cc",
        "cpp", "cxx", "h", "hpp", "hh", "cs", "php", "scala", "lua", "sh", "zig", "ml", "ex", "exs",
    ];
    if path.ends_with(".lock") {
        return false;
    }
    matches!(path.rsplit_once('.'), Some((_, ext)) if EXT.contains(&ext))
}

/// Lines of `git -C <repo> <args>` stdout, empty on any failure.
fn git_lines(repo: &Path, args: &[&str]) -> Vec<String> {
    match Command::new("git").arg("-C").arg(repo).args(args).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The line a pane runs to loose an orc. Short enough to read and approve before
/// pressing Enter, which is the whole point of staging it rather than running it.
/// `env` is the Mordor prefix (or empty); it sits before the sauron word so the
/// local-model vars are inherited by the agent this subcommand goes on to exec.
pub fn stage_command(sauron_exe: &Path, repo: &str, target: &str, env: &str) -> String {
    format!("cd {repo} && {env}{} orc {target}", sauron_exe.display())
}

/// `sauron orc <file> [--print] [--force]` -- build the charge and hand it to the
/// agent. Execs, so the agent replaces this process and owns the pane.
pub fn run(args: &[String], explicit_agent: Option<Agent>) -> std::io::Result<()> {
    let mut print_only = false;
    let mut force = false;
    let mut target: Option<&str> = None;
    for a in args {
        match a.as_str() {
            "--print" => print_only = true,
            "--force" => force = true,
            // Consumed at the top level into `explicit_agent`; skip here.
            "--claude" | "--codex" => {}
            other if other.starts_with("--") => return usage(&format!("unknown flag {other}")),
            other => target = Some(other),
        }
    }
    let Some(target) = target else {
        return usage("no target file given");
    };

    let repo = crate::git_root().unwrap_or(std::env::current_dir()?);
    if !repo.join(target).is_file() {
        eprintln!("sauron orc: no such file in this repo: {target}");
        std::process::exit(1);
    }

    // The premise of an orc is that its target is cold. A picker list can go
    // stale between opening and pressing Enter -- a hobbit may have grabbed the
    // file in the meantime -- so the check is re-run here, at the last moment
    // before the agent starts, rather than trusted from dispatch time.
    if !force && !print_only {
        let dirty = git_lines(&repo, &["status", "--porcelain", "--", target]);
        if !dirty.is_empty() {
            eprintln!("sauron orc: {target} has uncommitted changes -- it is no longer cold.");
            eprintln!("  another agent or you may be mid-edit; pick a different file,");
            eprintln!("  or re-run with --force if you know the change is yours and settled.");
            std::process::exit(1);
        }
    }

    let checks = detect(&repo, target);
    let charge = brief(target, &checks);
    if print_only {
        println!("{charge}");
        return Ok(());
    }

    let agent = Agent::select(explicit_agent, &repo);
    eprintln!("sauron orc: {target} -- {}, decompose first", agent.label());
    match (&checks.build, &checks.smoke) {
        (Some(b), Some(s)) => eprintln!("  green at every step: {b}   boot: {s}"),
        (Some(b), None) => eprintln!(
            "  green at every step: {b}   boot: unset (export {SMOKE_ENV} to name one)"
        ),
        _ => eprintln!("  no build command detected -- the charge asks for the repo equivalent"),
    }

    let (prog, argv) = agent.oneshot_argv(&charge);
    // This process becomes the agent. Only returns on failure, and then it
    // returns why -- `plat` picks exec or spawn-wait-exit per platform, and the
    // pane cannot tell the difference.
    Err(crate::plat::run_in_place(prog, &argv, &repo))
}

fn usage(why: &str) -> std::io::Result<()> {
    eprintln!("sauron orc: {why}");
    eprintln!("usage: sauron orc <file> [--print] [--force]");
    eprintln!("  <file>   repo-relative path to a cold source file");
    eprintln!("  --print  write the charge to stdout instead of running the agent");
    eprintln!("  --force  dispatch even though the file has uncommitted changes");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks() -> Checks {
        Checks {
            build: Some("cargo build --all-targets".into()),
            test: Some("cargo test".into()),
            smoke: Some("cargo run -- --once".into()),
            dir: None,
        }
    }

    /// The badge depends on a prose substring surviving every future reword of
    /// the charge. Nothing else in the build would go red if it stopped matching,
    /// so this test is the only thing standing between an edit and silently
    /// un-badging every orc in the TUI.
    #[test]
    fn the_charge_carries_the_orc_marker() {
        let b = brief("src/big.rs", &checks());
        assert!(
            b.contains(crate::model::ORC_MARKER),
            "the brief must contain ORC_MARKER verbatim or `is_orc` never fires"
        );
        assert!(brief_oneline("src/big.rs", &checks()).contains(crate::model::ORC_MARKER));
    }

    #[test]
    fn the_charge_ranks_decomposition_above_shrink_above_speed() {
        let b = brief("src/big.rs", &checks());
        let at = |needle: &str| b.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        assert!(at("1. DECOMPOSE") < at("2. SHRINK"));
        assert!(at("2. SHRINK") < at("3. SPEED"));
        // The hard constraint outranks all three, so it has to come first.
        assert!(at("HARD CONSTRAINT") < at("1. DECOMPOSE"));
        assert!(b.contains("outranks the other two combined"));
    }

    #[test]
    fn the_charge_names_the_build_and_boot_checks() {
        let b = brief("src/big.rs", &checks());
        assert!(b.contains("cargo build --all-targets"));
        assert!(b.contains("cargo test"));
        assert!(b.contains("run cargo run -- --once to prove the program still boots"));
        // …and says so honestly when the boot check is not configured, rather
        // than dropping the requirement.
        let bare = brief("src/big.rs", &Checks::default());
        assert!(bare.contains("prove the program still boots by whatever means"));
        assert!(bare.contains("the repository build"));
    }

    #[test]
    fn the_oneline_charge_survives_shell_and_applescript_embedding() {
        let one = brief_oneline("src/big.rs", &checks());
        assert!(!one.contains('\n'), "a newline breaks the AppleScript literal");
        assert!(!one.contains('"'), "a double quote breaks the AppleScript literal");
        assert!(!one.contains('\''), "a single quote breaks the shell quoting");
        // Flattening must not lose the priority ladder.
        assert!(one.contains("1. DECOMPOSE"));
        assert!(one.contains("3. SPEED"));
    }

    #[test]
    fn is_code_filters_to_source_files() {
        assert!(is_code("src/main.rs"));
        assert!(is_code("app/components/Foo.tsx"));
        assert!(!is_code("Cargo.lock"));
        assert!(!is_code("README.md"));
        assert!(!is_code("assets/logo.png"));
        assert!(!is_code("Makefile"));
    }

    #[test]
    // The `* 0` is the point: both sides are written as the same formula so the
    // difference between them is visibly the commit count and nothing else.
    // Folding it to `650` would state the number instead of deriving it.
    #[allow(clippy::erasing_op)]
    fn churn_lifts_a_smaller_but_much_edited_file_over_a_bigger_quiet_one() {
        // The ranking rule, exercised directly: 300 lines touched 10 times beats
        // 650 lines nobody has reopened. Bytes-based ranking got this backwards.
        let busy = 300 + CHURN_LOC_EQUIV * 10;
        let quiet = 650 + CHURN_LOC_EQUIV * 0;
        assert!(busy > quiet);
    }

    #[test]
    fn stage_command_is_short_enough_to_read_before_pressing_enter() {
        let c = stage_command(Path::new("/bin/sauron"), "/repo", "src/big.rs", "");
        assert_eq!(c, "cd /repo && /bin/sauron orc src/big.rs");
        // The Mordor prefix rides ahead of the sauron word, so the local-model
        // env is inherited by the agent this subcommand execs.
        let m = stage_command(Path::new("/bin/sauron"), "/repo", "src/big.rs", "A=1 ");
        assert_eq!(m, "cd /repo && A=1 /bin/sauron orc src/big.rs");
        // Short: the whole point of staging is that you can read it first.
        assert!(c.len() < 80, "stage command must stay reviewable: {c}");
    }

    #[test]
    fn detect_infers_nothing_when_there_is_no_manifest() {
        // No manifest anywhere above the target -> nothing inferred, and
        // crucially no guessed boot command.
        let c = detect(Path::new("/nonexistent-repo-path-for-test"), "a/b.rs");
        assert_eq!(c.build, None);
        assert_eq!(c.test, None);
        assert_eq!(c.dir, None);
    }

    /// The crate lives in a subdirectory here, so this is the case that matters:
    /// probing only the repo root found no manifest and silently downgraded the
    /// brief to "the repository build" -- an instruction no orc can act on.
    #[test]
    fn detect_finds_a_manifest_in_a_subdirectory_and_says_where_to_run_it() {
        let repo = crate::git_root().expect("tests run inside the repo");
        let c = detect(&repo, "sauron/src/scan.rs");
        assert_eq!(c.build.as_deref(), Some("cargo build --all-targets"));
        assert_eq!(c.dir.as_deref(), Some("sauron"));
        // …and the brief tells the orc which directory to run it from.
        assert!(brief("sauron/src/scan.rs", &c)
            .contains("cargo build --all-targets (from sauron/)"));
    }
}
