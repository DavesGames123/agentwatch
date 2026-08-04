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
//! WHY EVERY PANE GOES THROUGH A LOGIN SHELL
//! -----------------------------------------
//! `claude` is usually installed by a version manager whose PATH is set in a
//! profile. An iTerm pane gets that for free because iTerm runs a login shell;
//! spawning the agent binary directly would work on the developer's machine and
//! fail on everyone else's with "command not found". Running `$SHELL -lc` is
//! what makes a tab and a pane the same environment as well as the same command.
//!
//! grep targets:
//!   struct Pane        -- one tab: its pty, its title, the row it belongs to
//!   struct Workspace   -- the whole tab strip
//!   fn open_agent      -- resume a session, or start one under an id we chose
//!   fn open_orc        -- stage a maintenance agent on one cold file
//!   fn open_shell      -- a plain shell at the repo root
//!   fn close           -- end a tab and the agent in it
//!   fn json            -- the tab strip, for the page

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
        // `-i` rather than `-lc`: the line is typed onto the prompt and left
        // there, so the shell has to stay interactive instead of running it.
        let shell = login_shell();
        let argv = vec![shell.clone(), "-i".into()];
        let id = self.spawn_argv(Kind::Orc, format!("orc · {target}"), None, &argv, cols, rows)?;
        if let Some(p) = self.get(id) {
            p.pty.write(cmd.as_bytes());
        }
        Ok(id)
    }

    pub fn open_shell(&mut self, cols: u16, rows: u16) -> std::io::Result<u8> {
        let argv = vec![login_shell(), "-l".into()];
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
        let argv = vec![login_shell(), "-lc".into(), cmd.to_string()];
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
