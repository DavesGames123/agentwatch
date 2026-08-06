//! The tabs: what is open, what each one is running, and which board row it is.
//!
//! This is `workspace.rs`'s job done in a browser instead of in iTerm2, and it
//! is deliberately the *same* job. A pane opened here runs the command
//! `workspace::left_commands` would have built for it -- same agent word, same
//! `--session-id`, same `--name` -- so a tab and an iTerm pane are the same
//! thing launched two ways, and the board cannot tell which it is looking at.
//!
//! WHY A FRESH TAB IS GIVEN ITS SESSION ID
//! ---------------------------------------
//! Straight from `servant.rs`: the colour and the name are pure functions of the
//! session id, so a pane whose id does not exist yet cannot be coloured. Minting
//! it here and handing it over with `--session-id` means a tab is the right
//! colour from the frame it opens, matching the row the board will draw for it a
//! tick later. Without it the newest tab -- the one you just opened and are most
//! likely to be looking for -- would be the only grey one.
//!
//! WHAT HAPPENS WHEN THE AGENT WILL NOT TAKE THE ID
//! -----------------------------------------------
//! Codex has no `--session-id`: `Agent::fresh_cmd` drops the minted id on the
//! floor and returns a bare `codex`, and Codex then invents an id of its own.
//! The tab used to record the minted id anyway. That was a lie with
//! consequences -- the tab was coloured and named after a session that would
//! never exist, `open_sessions` reported it as open so the board offered to open
//! the real one again, and nothing the agent actually did could ever reach the
//! tab claiming to be it.
//!
//! A tab now claims a session only when the built command carries the id, which
//! is asked of the command itself rather than of a table that could drift from
//! `fresh_cmd`. A tab that claims none opens grey, labelled with the agent's own
//! word, and waits. `adopt` binds it on a later tick to the session that
//! appeared because of it -- see that function for how a new session is told
//! from one that was already there.
//!
//! WHY EVERY PANE GOES THROUGH A LOGIN *INTERACTIVE* SHELL
//! -------------------------------------------------------
//! `claude` and `codex` are usually installed by a version manager -- nvm, fnm,
//! mise, asdf -- and every one of them sets PATH from the *interactive* rc file,
//! not the login profile. `zsh -lc` reads `.zprofile` and `.zlogin` and skips
//! `.zshrc`; `bash -lc` reads `.bash_profile` and skips `.bashrc`. A pane opened
//! that way gets a PATH that is missing exactly the directory the agent lives
//! in, and the tab dies on "command not found" -- on a machine where typing the
//! same word in iTerm works. An iTerm pane is login *and* interactive, so the
//! tab has to be both as well: `-l -i -c`.
//!
//! WHY THE COMMAND IS PREFIXED WITH `exec`
//! ---------------------------------------
//! `-i` turns job control on, and a shell with job control forks the command
//! instead of replacing itself with it. That would put a shell between sauron
//! and the agent, and `Pty::kill` kills the child it spawned -- the shell --
//! leaving the agent alive, detached, and holding its memory with no tab left to
//! close it from. `exec` collapses the two back into one process: the rc files
//! are read, then the shell image is replaced by the agent, so the pid sauron
//! holds is the agent's pid and closing a tab ends it.
//!
//! grep targets:
//!   struct Pane        -- one tab: its pty, its title, the row it belongs to
//!   struct Workspace   -- the whole tab strip
//!   fn open_agent      -- resume a session, or start one under an id we chose
//!   fn open_orc        -- stage a maintenance agent on one cold file
//!   fn open_shell      -- a plain shell at the repo root
//!   fn close           -- end a tab and the agent in it
//!   fn json            -- the tab strip, for the page
//!   fn adopt           -- bind a waiting tab to the session it started
//!   fn adoptable       -- which ids are new *and* unclaimed, the pure part
//!   fn shell_argv      -- the flags, per shell, and the `exec` prefix
//!   fn rc_flags        -- which shells are known to accept `-l` and `-i`

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::Agent;
use crate::servant;

use super::pty::Pty;
use super::Clients;

/// What a tab is running. The page draws each differently, and `close` treats
/// them the same -- an orc is as killable as a hobbit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Agent,
    Orc,
    Shell,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Agent => "agent",
            Kind::Orc => "orc",
            Kind::Shell => "shell",
        }
    }
}

pub struct Pane {
    pub id: u8,
    pub title: String,
    pub kind: Kind,
    /// The agent session this tab is, when it is one and it is known. `None` for
    /// a shell, and `None` for an agent tab that has not been bound yet -- see
    /// the module header and `adopt`.
    pub session: Option<String>,
    /// Set on an agent tab launched without a session id, and cleared by
    /// `adopt`. A shell or an orc never carries it: neither is a session.
    awaiting_session: bool,
    pub pty: Pty,
}

pub struct Workspace {
    repo: PathBuf,
    agent: Agent,
    clients: Arc<Clients>,
    panes: Vec<Pane>,
    /// Monotonic. Ids are never reused, because a page holding a stale id for a
    /// tab that closed must miss, not land on whatever opened next.
    next: u8,
    /// Every session id the board has ever shown this process. `adopt` reads it
    /// to tell a session that appeared from one that was already there.
    seen: HashSet<String>,
    /// False until `adopt` has been called once. The first call only records
    /// what already exists; without that, every session on the board at startup
    /// would look new and the first waiting tab would take someone else's.
    primed: bool,
}

impl Workspace {
    pub fn new(repo: PathBuf, agent: Agent, clients: Arc<Clients>) -> Self {
        Self {
            repo,
            agent,
            clients,
            panes: Vec::new(),
            next: 1,
            seen: HashSet::new(),
            primed: false,
        }
    }

    pub fn get(&self, id: u8) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// The session ids that already have a tab, so the page can show a board row
    /// as "open" rather than offering to open it twice.
    pub fn open_sessions(&self) -> Vec<String> {
        self.panes.iter().filter_map(|p| p.session.clone()).collect()
    }

    /// Resume `session` in a new tab, or start a fresh agent when it is `None`.
    pub fn open_agent(
        &mut self,
        session: Option<String>,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<u8> {
        // Exactly `workspace::left_commands`' shape. Any divergence here would
        // mean a browser-launched agent behaving unlike an iTerm-launched one,
        // which is the failure this whole module exists to avoid.
        let (session, cmd) = match session {
            Some(id) => {
                let cmd = format!(
                    "{}{}",
                    self.agent.resume_cmd(&id),
                    self.agent.name_flag(servant::name_for(&id))
                );
                (Some(id), cmd)
            }
            None => {
                let minted = servant::mint_session_id();
                let cmd = format!(
                    "{}{}",
                    self.agent.fresh_cmd(&minted),
                    self.agent.name_flag(servant::name_for(&minted))
                );
                // Whether the agent took the id is asked of the command, not of
                // a list of which agents do. `fresh_cmd` is the only thing that
                // decides, and a second statement of the same fact is one edit
                // away from contradicting it.
                (cmd.contains(&minted).then_some(minted), cmd)
            }
        };
        let title = match &session {
            Some(id) => servant::name_for(id).to_string(),
            // No id means no servant name yet. The agent's own word is a
            // truthful label for a tab whose session is not known; `adopt`
            // replaces it with the servant's name once it is.
            None => self.agent.label().to_string(),
        };
        let awaiting = session.is_none();
        let pane = self.spawn(Kind::Agent, title, session, &cmd, cols, rows)?;
        if awaiting {
            if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane) {
                p.awaiting_session = true;
            }
        }
        Ok(pane)
    }

    /// Bind each waiting tab to the session that appeared because of it.
    ///
    /// Called on the board's tick with every session id the board now shows. A
    /// waiting tab is one whose agent would not take a minted id (Codex), so the
    /// only way to learn what it became is to watch for a session that was not
    /// there before it opened. `seen` is what "before" means -- a session
    /// already on the board when the tab opened is not a candidate, however
    /// recently it ran.
    ///
    /// Two known wrong answers, both preferred to the alternative of never
    /// binding at all. A session started outside this workspace -- an iTerm pane
    /// or a bare shell in the same repo -- while a tab is waiting will be
    /// adopted by that tab. And two tabs waiting when two sessions appear on the
    /// same tick may be bound the wrong way round, since nothing here can tell
    /// which pty produced which rollout. Both cost a name and a colour; neither
    /// sends anything to the wrong agent, because a tab writes to its own pty
    /// and never addresses an agent by session id.
    pub fn adopt(&mut self, ids: &[String]) {
        if !self.primed {
            self.primed = true;
            self.seen.extend(ids.iter().cloned());
            return;
        }
        let claimed: HashSet<String> = self.panes.iter().filter_map(|p| p.session.clone()).collect();
        let mut queue = adoptable(ids, &self.seen, &claimed).into_iter();
        for p in self.panes.iter_mut().filter(|p| p.awaiting_session) {
            let Some(id) = queue.next() else { break };
            p.title = servant::name_for(&id).to_string();
            p.session = Some(id);
            p.awaiting_session = false;
        }
        self.seen.extend(ids.iter().cloned());
    }

    /// Stage an orc on one cold file. Staged, not run: the tab opens holding the
    /// command, and the orc starts when you press Enter in it -- the same
    /// contract the iTerm orc panes have, and for the same reason. Loosing an
    /// agent on a file is not something a mis-click should do.
    pub fn open_orc(&mut self, target: &str, cols: u16, rows: u16) -> std::io::Result<u8> {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sauron"));
        let cmd = crate::orc::stage_command(&exe, &self.repo.to_string_lossy(), target, "");
        // No `-c`: the line is typed onto the prompt and left there, so the
        // shell has to sit at a prompt instead of running anything. The rc files
        // still have to be read -- the staged line names `sauron`, and the orc
        // it starts names the agent.
        let argv = prompt_argv(&login_shell());
        let id = self.spawn_argv(Kind::Orc, format!("orc · {target}"), None, &argv, cols, rows)?;
        if let Some(p) = self.get(id) {
            p.pty.write(cmd.as_bytes());
        }
        Ok(id)
    }

    pub fn open_shell(&mut self, cols: u16, rows: u16) -> std::io::Result<u8> {
        let argv = prompt_argv(&login_shell());
        self.spawn_argv(Kind::Shell, "shell".into(), None, &argv, cols, rows)
    }

    fn spawn(
        &mut self,
        kind: Kind,
        title: String,
        session: Option<String>,
        cmd: &str,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<u8> {
        let argv = shell_argv(&login_shell(), cmd);
        self.spawn_argv(kind, title, session, &argv, cols, rows)
    }

    fn spawn_argv(
        &mut self,
        kind: Kind,
        title: String,
        session: Option<String>,
        argv: &[String],
        cols: u16,
        rows: u16,
    ) -> std::io::Result<u8> {
        let id = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        let pty = Pty::spawn(id, argv, &self.repo, cols, rows, self.clients.clone())?;
        self.panes.push(Pane {
            id,
            title,
            kind,
            session,
            awaiting_session: false,
            pty,
        });
        Ok(id)
    }

    /// End a tab and the agent in it. Closing a tab in the *page* does not come
    /// here -- that is just a view detaching, and the agent keeps working. This
    /// is the explicit "I am done with this one".
    pub fn close(&mut self, id: u8) {
        if let Some(i) = self.panes.iter().position(|p| p.id == id) {
            self.panes[i].pty.kill();
            self.panes.remove(i);
        }
    }

    pub fn close_all(&mut self) {
        for p in &self.panes {
            p.pty.kill();
        }
        self.panes.clear();
    }

    /// The tab strip.
    pub fn json(&self) -> String {
        let tabs: Vec<String> = self
            .panes
            .iter()
            .map(|p| {
                let (r, g, b) = p
                    .session
                    .as_deref()
                    .map(servant::color_for)
                    .unwrap_or((136, 144, 156));
                let session = p
                    .session
                    .as_deref()
                    .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".into()))
                    .unwrap_or_else(|| "null".into());
                format!(
                    "{{\"id\":{},\"title\":{},\"kind\":\"{}\",\"session\":{},\
                     \"color\":[{r},{g},{b}],\"dead\":{}}}",
                    p.id,
                    serde_json::to_string(&p.title).unwrap_or_else(|_| "\"\"".into()),
                    p.kind.key(),
                    session,
                    p.pty.is_dead()
                )
            })
            .collect();
        format!("{{\"t\":\"tabs\",\"tabs\":[{}]}}", tabs.join(","))
    }
}

/// The ids a waiting tab may be bound to: on the board now, not on it before,
/// and not already another tab's.
///
/// Split out of `adopt` because it is the whole of the decision and none of the
/// state -- a `Workspace` cannot be built in a test without opening real
/// pseudo-terminals, and the rule is worth testing more than the plumbing is.
///
/// Sorted, so two tabs waiting on the same tick are bound in an order that does
/// not depend on how a `HashSet` happened to iterate.
fn adoptable(ids: &[String], seen: &HashSet<String>, claimed: &HashSet<String>) -> Vec<String> {
    let mut out: Vec<String> = ids
        .iter()
        .filter(|id| !seen.contains(*id) && !claimed.contains(*id))
        .cloned()
        .collect();
    out.sort();
    out
}

/// The user's shell, or a sane default. `$SHELL` is what iTerm honours, so a
/// tab lands in the same shell a pane would have.
fn login_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// The `-l -i` pair, but only for the shells known to accept both.
///
/// `-l` is not portable. `dash` -- which is `/bin/sh` on most Linux boxes and is
/// the fallback when `$SHELL` is unset -- rejects it outright, and `bash` in
/// `sh` mode accepts `-i` while printing "no job control in this shell" onto the
/// tab before anything else runs. Neither is worth a broken pane, so an
/// unrecognised shell gets the plain `-c` this module used to give everyone, and
/// loses only the rc file it may not have had.
///
/// Given as two separate words rather than a bundled `-li`. `zsh -lic` and
/// `bash -lic` both parse, but `fish` and `nu` take long options with a single
/// dash and do not bundle at all, so the separated form is the one that is
/// correct everywhere it is used.
fn rc_flags(shell: &str) -> &'static [&'static str] {
    let name = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    match name {
        "zsh" | "bash" | "fish" => &["-l", "-i"],
        _ => &[],
    }
}

/// The argv that runs `cmd` in the user's shell with the environment an iTerm
/// pane would have had. See the module header for `-i` and for `exec`.
fn shell_argv(shell: &str, cmd: &str) -> Vec<String> {
    let mut argv = vec![shell.to_string()];
    argv.extend(rc_flags(shell).iter().map(|f| f.to_string()));
    argv.push("-c".into());
    // `exec` is POSIX and is a builtin in every shell `rc_flags` admits, so this
    // is the same word in all of them.
    argv.push(format!("exec {cmd}"));
    argv
}

/// The argv for a shell that must stop at a prompt instead of running a command:
/// the orc tab, which is handed its line to press Enter on, and the plain shell
/// tab. No `-c`, and therefore no `exec` -- there is nothing yet to exec.
fn prompt_argv(shell: &str) -> Vec<String> {
    let mut argv = vec![shell.to_string()];
    let flags = rc_flags(shell);
    if flags.is_empty() {
        // An unrecognised shell reading no rc file is still a shell at a prompt;
        // `-i` alone is what makes it one when stdin's tty is not enough.
        argv.push("-i".into());
    } else {
        argv.extend(flags.iter().map(|f| f.to_string()));
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_tab_claims_a_session_only_when_its_command_carries_the_id() {
        // The rule `open_agent` applies, stated where `fresh_cmd` can break it.
        // Codex drops the minted id, so a fresh Codex tab must claim nothing
        // rather than name itself after a session that will never exist.
        let id = servant::mint_session_id();
        assert!(Agent::Claude.fresh_cmd(&id).contains(&id));
        assert!(!Agent::Codex.fresh_cmd(&id).contains(&id));
        // A resume carries the id in both, which is why a resumed tab never
        // waits for adoption.
        assert!(Agent::Claude.resume_cmd(&id).contains(&id));
        assert!(Agent::Codex.resume_cmd(&id).contains(&id));
    }

    #[test]
    fn only_a_session_that_was_not_there_before_can_be_adopted() {
        let seen = set(&["old-1", "old-2"]);
        let claimed = HashSet::new();
        // The two already on the board are not candidates however busy they are.
        assert_eq!(
            adoptable(&ids(&["old-1", "old-2", "new-1"]), &seen, &claimed),
            ids(&["new-1"])
        );
        // Nothing new, nothing to bind.
        assert!(adoptable(&ids(&["old-1"]), &seen, &claimed).is_empty());
    }

    #[test]
    fn a_session_another_tab_already_holds_is_not_offered_twice() {
        // A tab resumed from the board holds its id from launch. A waiting tab
        // must not be handed the same one just because the board only started
        // showing it now.
        let seen = HashSet::new();
        let claimed = set(&["held-by-tab-2"]);
        assert_eq!(
            adoptable(&ids(&["held-by-tab-2", "loose"]), &seen, &claimed),
            ids(&["loose"])
        );
    }

    #[test]
    fn two_new_sessions_are_offered_in_a_fixed_order() {
        // Two tabs waiting on one tick may still be bound the wrong way round --
        // nothing here knows which pty wrote which rollout -- but the order must
        // not also vary run to run.
        let (seen, claimed) = (HashSet::new(), HashSet::new());
        assert_eq!(
            adoptable(&ids(&["b", "c", "a"]), &seen, &claimed),
            ids(&["a", "b", "c"])
        );
    }

    #[test]
    fn a_known_shell_gets_login_and_interactive_and_the_exec_prefix() {
        // The bug this fixes: `-lc` skips `.zshrc`, which is where nvm/fnm/mise
        // put the directory `claude` and `codex` live in.
        assert_eq!(
            shell_argv("/bin/zsh", "codex resume abc"),
            vec!["/bin/zsh", "-l", "-i", "-c", "exec codex resume abc"]
        );
        assert_eq!(
            shell_argv("/opt/homebrew/bin/bash", "claude --resume abc"),
            vec![
                "/opt/homebrew/bin/bash",
                "-l",
                "-i",
                "-c",
                "exec claude --resume abc"
            ]
        );
    }

    #[test]
    fn an_unknown_shell_keeps_the_plain_c_rather_than_a_flag_it_rejects() {
        // `dash -l` is an error, not a no-op, and a pane that fails to open is
        // worse than one whose PATH is short.
        assert_eq!(
            shell_argv("/bin/sh", "claude"),
            vec!["/bin/sh", "-c", "exec claude"]
        );
        assert_eq!(shell_argv("/usr/bin/dash", "claude")[1], "-c");
    }

    #[test]
    fn the_exec_prefix_is_always_present_so_the_pty_child_is_the_agent() {
        // Without it, an interactive shell forks the agent and `Pty::kill` ends
        // the shell while the agent keeps running -- one orphan per closed tab.
        for shell in ["/bin/zsh", "/bin/bash", "/bin/sh", "/usr/local/bin/fish"] {
            let argv = shell_argv(shell, "codex");
            assert_eq!(argv.last().unwrap(), "exec codex", "shell {shell}");
        }
    }

    #[test]
    fn a_prompt_shell_runs_no_command_and_still_reads_its_rc() {
        assert_eq!(prompt_argv("/bin/zsh"), vec!["/bin/zsh", "-l", "-i"]);
        assert!(!prompt_argv("/bin/zsh").iter().any(|a| a == "-c"));
        // Unknown shell: interactive is the part that matters at a prompt.
        assert_eq!(prompt_argv("/bin/sh"), vec!["/bin/sh", "-i"]);
    }
}
