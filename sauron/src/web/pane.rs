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
//!   fn shell_argv      -- the flags, per shell, and the `exec` prefix
//!   fn rc_flags        -- which shells are known to accept `-l` and `-i`

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
    /// The agent session this tab is, when it is one. `None` for a shell.
    /// Present from launch for agent tabs -- see the module header on minting.
    pub session: Option<String>,
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
}

impl Workspace {
    pub fn new(repo: PathBuf, agent: Agent, clients: Arc<Clients>) -> Self {
        Self {
            repo,
            agent,
            clients,
            panes: Vec::new(),
            next: 1,
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
        let (id, cmd) = match session {
            Some(id) => {
                let cmd = format!(
                    "{}{}",
                    self.agent.resume_cmd(&id),
                    self.agent.name_flag(servant::name_for(&id))
                );
                (id, cmd)
            }
            None => {
                let id = servant::mint_session_id();
                let cmd = format!(
                    "{}{}",
                    self.agent.fresh_cmd(&id),
                    self.agent.name_flag(servant::name_for(&id))
                );
                (id, cmd)
            }
        };
        let title = servant::name_for(&id).to_string();
        self.spawn(Kind::Agent, title, Some(id), &cmd, cols, rows)
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
