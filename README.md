<div align="center">

# 👁️ sauron

### Know what your agents changed — before you ship it.

A read-only sidecar for running **many coding agents at once** — [Claude Code](https://claude.com/claude-code) and [OpenAI Codex](https://github.com/openai/codex) — and never losing track of which ones left work you haven't tested.

</div>

```
                    ▄▟█████▙▄
                  █▟███████▙█        The lidless Eye, wreathed in fire,
                  █▐███████▌█        watches every agent in your swarm
                  █▜███████▛█        — and tells you which one left work
 ▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▟███████████▙▁▁▁▁    you still need to test.
```

<div align="center">

![Rust](https://img.shields.io/badge/built_with-Rust-CE422B?style=for-the-badge&logo=rust&logoColor=white)
![ratatui](https://img.shields.io/badge/TUI-ratatui-7C3AED?style=for-the-badge)
![Agents](https://img.shields.io/badge/agents-Claude_·_Codex-FF7A18?style=for-the-badge)
![macOS](https://img.shields.io/badge/workspace-macOS_·_iTerm2-000000?style=for-the-badge&logo=apple&logoColor=white)
![Read only](https://img.shields.io/badge/watcher_repo_writes-none-2EA043?style=for-the-badge)

</div>

<br>

```
┌─ sauron ─────────────────────────────────────────────┐
│  ● letters-redesign    NEEDS TEST   src/gui/letters.rs    │
│  ▲ flora-field-tick    BLOCKED      waiting on you        │
│  ● combat-armory       NEEDS TEST   3 files               │
│  ○ crossword-fetch     clear                              │
└──────────────────────────────────────────────────────────┘
     j/k move   ·   a ack   ·   b baseline   ·   q quit
```

---

## Why

When you fan out a swarm of coding agents, the bottleneck stops being *writing*
code and becomes *trusting* it. Which sessions actually touched the repo? Which
are quietly **blocked on a question only you can answer**? What have you already
reviewed? **sauron** answers exactly that — and nothing else.

The watcher reads each agent's session logs, re-tailing every two seconds. It
**writes nothing to your repo** and **never talks to a running agent**.

---

## 🎭 The cast

You are Sauron. Your servants get the work done; the Eye keeps watch.

| | Who | What they do |
|:-:|:--|:--|
| 🧝 | **hobbits** | the agents you direct — resumed into the `workspace` panes to do the real quests |
| 👹 | **orcs** | single-shot maintenance agents you loose on the *cold* corners: decompose, document, de-warn |
| 👁️ | **the Eye** | sauron itself, watching every session and flagging what needs you |
| 💍 | **Nazgûl** | *reserved* — the Nine are for something worse, later |

<br>

<table>
<tr>
<td width="50%" valign="top">

### 🛰️ `sauron`
The terminal sidecar. One question, answered live:
> *what did my agents change that I haven't tested yet?*

- Flags each session **needs-test · blocked · clear**
- **Acknowledge** work as you verify it — progress persists
- Surfaces sessions **blocked on a question** first
- Collapses the historical backlog so *today's* work stays visible
- Quotes your **last message** to each session, and previews the **recent
  edits** of the selected one — re-brief at a glance
- And a lidless **Eye** keeps watch up top, in runes, while hobbits scurry past

</td>
<td width="50%" valign="top">

### 🪟 `sauron workspace`
A subcommand of the same binary. One command, a whole cockpit:

- Opens a **fullscreen layout on its own Space**
- A column of **bare `claude` panes** (pick how many)
- `sauron` wired into the top-right, shells below
- Panes stay evenly sized no matter the agent count

</td>
</tr>
</table>

---

## 🚀 Quick start

```bash
# 1 · build the sidecar and Agent Clipboard
cargo build --release --manifest-path sauron/Cargo.toml

# 2 · watch the repo you're standing in
./sauron/target/release/sauron

# …or watch a specific repo
./sauron/target/release/sauron /path/to/repo
```

Drop it on your `PATH` to call it from anywhere:

```bash
cp sauron/target/release/sauron sauron/target/release/clip /usr/local/bin/
```

---

## ⌨️ Keys

| Key | Action |
|:---:|:-------|
| <kbd>j</kbd> / <kbd>k</kbd> | move selection |
| <kbd>a</kbd> | acknowledge selected session — mark its edits tested |
| <kbd>u</kbd> | un-acknowledge |
| <kbd>n</kbd> | **new agent pane** — split one into the workspace's left column |
| <kbd>⏎</kbd> | **open the selected session** in a new left pane, resumed |
| <kbd>b</kbd> | **baseline** — ack the whole historical backlog, start empty |
| <kbd>c</kbd> | toggle cleared / idle sessions |
| <kbd>A</kbd> | toggle the stale backlog |
| <kbd>q</kbd> | quit |

---

## 📋 Agent Clipboard

Sauron includes a Rust implementation of the
[Forge Agent Clipboard](https://github.com/alvinlu7/forge). It preserves the
SQLite schema, FTS5 index, JSON record shape, TTL/version/checksum behavior, and
validated-before-pin policy, so the Python and Rust tools can share one database.

Use either entry point:

```bash
clip put project.current.invariants --file invariants.md --namespace project
clip search "project invariants" --namespace project --json
clip get project.current.invariants

# The same CLI is available through the Sauron binary.
sauron clip copy project.current.invariants
```

The complete command surface is `put`, `update`, `get`, `copy`, `search`, `list`,
`recent`, `pin`, `delete`, `export`, `import`, `stats`, `doctor`, and `gc`.
Override storage with `--db PATH` / `--db-path PATH` or
`$AGENT_CLIPBOARD_DB`. Otherwise Sauron reuses an ancestor
`.agent-clipboard/clipboard.sqlite3` when present, then falls back to the normal
platform data directory.

---

## 🪟 `sauron workspace` — the multi-agent cockpit

> **macOS + iTerm2 only.**

```bash
sauron workspace                  # the repo you're standing in, with a quick prompt
sauron workspace 8                # …suggesting 8 panes
sauron workspace 8 <project>      # a specific project — count & project any order
sauron workspace 8 .              # the current folder, explicitly
sauron workspace 5 --orcs 2       # 5 hobbits + 2 orcs (see below)
sauron workspace 5 --mordor       # run the swarm on a local model (see 🌋 below)
sauron workspace 5 --clipboard-handoff  # strict read-at-start/write-at-end passes
sauron workspace 8 <project> -y   # skip the prompt (also skipped when scripted)
```

A bare **`sauron workspace`** opens the **repository you're in** (the git repo
containing your cwd) and asks a quick question first:

```
  sauron workspace  →  /Users/you/code/worldsmith   (claude)
  panes [5]: 
  orcs  [0]: 
  launch 5 pane(s), 0 orc(s)? [Y/n]
```

Press Enter to accept each, type a number to change it, `q` to bail. Pass `-y`
(or pipe/redirect stdin) to skip the dialogue entirely.

### Strict clipboard handoffs

`--clipboard-handoff` wraps every launched hobbit and orc in an opt-in continuity
gate:

1. Before the agent starts, Sauron opens the clipboard, loads the exact prior
   lane handoff plus the highest-signal entries for this repository, and injects
   that context into the first prompt.
2. Before the agent exits, its instructions require a structured JSON handoff:
   completed work, repo/test state, invariants and decisions, blockers, and the
   precise next action.
3. After exit, Sauron reopens the database and verifies that the lane key has a
   new version containing the pass nonce. A missing or unreadable handoff marks
   the pass incomplete with exit code `3`.

Keys and namespaces are deterministic from the canonical repository path and
workspace lane, keeping parallel panes separate while allowing every later pass
to retrieve the repository's recent handoffs. Without the flag, workspace launch
commands and behavior are unchanged.

### 👹 orcs — the maintenance swarm

`--orcs N` stages **N single-shot maintenance agents** in the right column, aimed
at the *cold* corners of the repo — source files **no active session is touching**
and nothing has dirtied in git, ranked by **lines of code plus recent churn**
(a file that gets reopened over and over is structurally wrong more reliably than
a file that is merely long).

Each orc is handed **one file and a ranked charge**, not a list of equal wishes:

1. **Decompose** — split it into a nested module tree, one concern per file, each
   with a header saying what it is for and the grep targets to reach its parts.
   This **outranks the other two combined**; a correct split is a complete result.
2. **Shrink** — cut the line count of what remains without losing behaviour or
   documentation, and report the before/after per file.
3. **Speed** — only where the code makes the win evident (hoist work out of a
   loop, drop a large clone, replace a repeated scan with a lookup), each with a
   one-line reason. Speculative micro-tuning is explicitly forbidden.

Above all three sits a **hard constraint: the tree must compile and the program
must still boot at every moment the orc is working**, not merely when it
finishes — because a hobbit is building this repo the whole time. The charge
spells out the discipline that makes that true: run the build *before* the first
edit, work additive-first (create and wire in the new module, compile, *then*
delete from the original), re-run the build after every step, revert any step
that goes red rather than pressing on, and never touch the build manifest or
entrypoint to make a change compile. sauron fills in the actual commands from the
repo's manifest — including the subdirectory to run them from — and reads the
boot check from `$SAURON_ORC_SMOKE`.

> There is no ecosystem default for the boot check on purpose. Whether a program
> *boots* is a property of the program, not the toolchain: `cargo build` passing
> says nothing about whether `main` reaches its event loop. A guessed command
> would have the orc certify a boot it never performed.
>
> ```bash
> export SAURON_ORC_SMOKE='cargo run --quiet -- --once'   # for sauron itself
> ```

- **They don't start on their own.** The command is *typed into each orc pane but
  not run* — you review the target and press **Enter** to loose it. What gets
  typed is short enough to actually read: `sauron orc src/big.rs`.
- **They're marked distinct.** An orc session wears a green **`orc`** badge in the
  TUI, so you can tell sauron's own maintenance work apart from the **hobbits**
  doing your directed quests.
- **They only take what's safe**, so they can never collide with a hobbit
  mid-edit — and the cold check is **re-run at dispatch**, not trusted from when
  the list was built, so a file a hobbit grabbed in the meantime is refused.

### Loosing an orc from the TUI

`--orcs N` only fires at launch, which is the wrong moment: the hot/cold snapshot
is taken before any hobbit has started, and the pane budget is fixed forever.
Press **`O`** in the running TUI instead and you get a live picker.

```
┌ loose an orc — decompose first ──────────────────────┐
│ ▸ sauron/src/clip/store.rs            1178 loc · 4 commits │
│   sauron/src/clip/mod.rs               633 loc · 2 commits │
│   12 cold · 3 hot · 9 dirty held back                │
│   ⏎ stage orc   j/k move   esc close                 │
└──────────────────────────────────────────────────────┘
```

It shows the evidence behind each rank rather than asking you to trust the
ordering, and it says out loud how many candidates were **held back** — silent
filtering reads as "there was nothing else", which is a different and false
claim. **Enter** splits a new pane in sauron's own column and *stages* the orc
there, same as at launch: typed, awaiting your Enter. Outside a workspace window
there's no pane to split, so the command goes to the clipboard instead.

`<project>` is a directory (path, `~`, `.`) **or** a short alias you've saved
into workspace memory:

```bash
sauron workspace alias api ~/code/api-service   # then:  sauron workspace 6 api
sauron workspace alias worldsmith ~/code/worldsmith
sauron workspace alias                          # list saved aliases
sauron workspace unalias api                    # forget one
```

It spins up a new iTerm2 window, throws it into native fullscreen (which gives
it its own macOS Space), then splits it into a left column of `claude` panes and
a right column with `sauron` on top. The panes reopen each in-flight session
(`claude --resume`) pulled straight from the scanner — the same set the TUI
shows — and run `sauron` itself for the watcher, by the very path you invoked, so
a restored window keeps working. Registry lives at `~/.claude/sauron/workspaces`.

### Growing the column without leaving the TUI

Closing an agent you're done with was always one keystroke. Opening one back up
was a split, a `cd`, and a typed command — so the swarm only ever shrank. Two
keys in the watcher close that loop:

| Key | Opens | Focus |
|:---:|:--|:--|
| <kbd>n</kbd> | a fresh agent at the repo root | **stays in sauron** — press it three times for three panes |
| <kbd>⏎</kbd> | the *selected* session, resumed | **follows the new pane** — you opened it to talk to it |
| <kbd>O</kbd> | the cold-file picker → a **staged orc** in sauron's own column | follows the orc pane, which is waiting on your Enter |

<kbd>n</kbd> and <kbd>⏎</kbd> land in the **left column** and split whichever pane there is currently
tallest, so the column stays even instead of one pane shrinking by half each
time. The column is found live rather than remembered: iTerm2 enumerates a tab's
panes in split-tree order and the layout carves the right column off as the
second child, so *everything ahead of the sauron pane is the agent column* — a
fact that stays true however many panes you close. This means it also works in
workspace windows opened before this feature existed.

<kbd>⏎</kbd> is the counterpart to closing a pane: the session outlives its
terminal, so a session you shut the window on — or one that went blocked
overnight — comes back with its history intact. It does not check whether that
session is already open in another pane; two live panes on one session is not a
state Claude Code guards against, so don't.

If a spawn can't happen — sauron isn't running in an iTerm2 pane, there is no
agent column left of it, or iTerm2 refuses the split because the column has hit
its minimum pane height — the footer says which, in amber, for a few seconds.
Panes opened this way run against the hosted API even in a `--mordor` /
`--nostromo` workspace; the local-model env is set at launch and the watcher does
not carry it.

<details>
<summary><b>Requirements</b></summary>

<br>

- iTerm2 with the AppleScript API enabled
- Accessibility permission granted to iTerm2
  (**System Settings → Privacy & Security → Accessibility**) — needed for the
  fullscreen toggle.

</details>

---

## 🌋 Mordor mode — a local swarm

> **Off the grid. Your servants toil on a model of your own, on your own iron.**

`--mordor` runs the **hobbits and orcs** against a **local** model instead of the
hosted API — the whole swarm, no tokens billed, no network. The **Eye stays on the
hosted API** (it only reads logs; it never calls a model), so nothing about the
watch changes.

It rides on [Ollama's Anthropic-compatible API][ollama-cc] — Claude Code speaks to
a local model with **no proxy and no translation layer**, just a few env vars that
sauron prepends to each servant pane:

```bash
sauron workspace 5 --mordor                    # 5 hobbits on the local model
sauron workspace 5 --mordor --orcs 2           # …and 2 local orcs
sauron workspace 5 --mordor=qwen2.5-coder:7b   # pick the model (see below)
sauron workspace 5 --nostromo                  # …on a remote box over Tailscale (see below)
```

So a session fires against **cloud** (plain `sauron workspace`), the **local Mac**
(`--mordor`), or a **remote box over Tailscale** (`--nostromo`) — your call, per launch.

### Prerequisites

```bash
# 1 · install Ollama, then pull the coder the swarm will run on
ollama pull qwen3-coder      # the default — Qwen3-Coder, the strongest local coder
ollama serve                 # (usually already running on :11434)
```

### What it does

For every hobbit and orc pane, sauron prepends the local endpoint before the
`claude` word — so `cd repo && claude …` becomes:

```bash
cd repo && ANTHROPIC_BASE_URL=http://localhost:11434 \
           ANTHROPIC_AUTH_TOKEN=ollama ANTHROPIC_API_KEY= \
           ANTHROPIC_MODEL=qwen3-coder \
           ANTHROPIC_SMALL_FAST_MODEL=qwen3-coder \
           ANTHROPIC_DEFAULT_HAIKU_MODEL=qwen3-coder \
           claude --resume <id>
```

The background/small-fast model is pinned to the **same** local tag, so a
single-model box never has a background call reach for a `haiku`-class model you
never pulled. The confirmation dialogue names the realm so a local launch is never
mistaken for an ordinary one:

```
  sauron workspace  →  /Users/you/code/worldsmith   (claude · 🌋 Mordor: qwen3-coder @ http://localhost:11434)
```

### The model

The default is **`qwen3-coder`** — Qwen3-Coder, a 30B MoE agentic coder (256K
context) and the strongest local coding model that fits a 32GB Mac or a 24–32GB
GPU. Override it inline, and point at a non-default Ollama with `$SAURON_MORDOR_URL`:

| Want | Do |
|:--|:--|
| The default coder | `--mordor` |
| A specific tag | `--mordor=qwen2.5-coder:7b` (the right call on an 8GB box) |
| A remote / non-standard Ollama | `SAURON_MORDOR_URL=http://box:11434 sauron workspace 4 --mordor` |
| A remote box over Tailscale | `--nostromo` (endpoint from local config — see below) |

### Over Tailscale — `--nostromo`

`--nostromo` is `--mordor` pointed at another box's Ollama over your tailnet — fire
a local swarm on the beefy machine from your laptop. It takes a model tag the same
way (`--nostromo=qwen2.5-coder:7b`).

The endpoint is a **private tailnet hostname, so it is never stored in this repo.**
sauron reads it from your machine only — `$SAURON_NOSTROMO_URL`, or the first line of
`~/.claude/sauron/nostromo-url`:

```bash
# set it once (choose either); the URL is the Ollama root over Tailscale
echo 'https://<your-box>.<tailnet>.ts.net' > ~/.claude/sauron/nostromo-url
# or: export SAURON_NOSTROMO_URL=https://<your-box>.<tailnet>.ts.net
```

On the **serving box**, `tailscale serve` must terminate HTTPS on :443 and proxy to
Ollama at the root, and Ollama must listen beyond loopback so the tunnel can reach it:

```bash
OLLAMA_HOST=0.0.0.0:11434 ollama serve          # bind past 127.0.0.1, or the tunnel gets refused
tailscale serve --bg --https=443 http://127.0.0.1:11434
```

Run `--nostromo` with neither the env var nor the file set and sauron tells you how to
set them rather than guessing an endpoint.

> **Claude Code only, for now.** Mordor rides Ollama's *Anthropic*-compatible API,
> which is Claude Code's wire format. Codex reaches local models a different way
> (`codex --oss`), so `--mordor` warns and no-ops under `--codex` rather than
> silently running against the hosted API.

[ollama-cc]: https://docs.ollama.com/integrations/claude-code

---

## 🤖 Agents — Claude Code, Codex, and beyond

sauron isn't tied to one agent. Everything downstream — the status model, the
cards, workspace, orcs — is agent-agnostic; only *where the logs live* and *how
one record folds into a session* differ, behind a small `Agent` seam
(`src/agent.rs`). **Claude Code** is the default and fully supported; **OpenAI
Codex** is supported too; adding a third is a localized change.

### Choosing an agent

Pick with a flag, the `$SAURON_AGENT` env var, or let it auto-detect — first match wins:

| Precedence | Source | Example |
|:--|:--|:--|
| 1 | flag | `sauron --codex`, `sauron --claude` |
| 2 | env | `SAURON_AGENT=codex sauron` |
| 3 | auto-detect | whichever agent has logs for this repo |
| 4 | default | Claude Code |

The choice flows everywhere:

```bash
sauron                        # watch — auto-detect the agent
sauron --codex                # watch Codex sessions
sauron workspace 5 --codex    # a Codex cockpit: hobbit panes run `codex`,
                              #   the watcher runs `sauron --codex`
sauron workspace 5 --codex --orcs 2   # …and orcs run `codex exec`
```

### Codex specifics

sauron reads Codex rollouts from `~/.codex/sessions/**/*.jsonl`, matching them to
the repo by the `cwd` recorded in each rollout, and folds messages into
prompt/turn state and `apply_patch` envelopes into the write-set.

> ⚠️ **Codex support is best-effort and unverified.** It was written against the
> documented rollout format on a machine with no Codex install. It's defensive —
> it degrades rather than crashes — but if edits or prompts look off, **one real
> `~/.codex/sessions/**/rollout-*.jsonl`** pins the exact field names. The fix is
> isolated to `src/codex.rs`; reports very welcome.

### Adding another agent

Aider, Gemini CLI, Cursor, your own — see **[docs/AGENTS.md](docs/AGENTS.md)** for
the seam and a step-by-step. In short: add an `Agent` variant, give it a spawn
command and a log reader (session discovery + a `fold` that maps records onto the
shared `Session`), and the entire UI, workspace, and orc machinery come for free.

---

## 🔍 How it reads sessions

Each agent stores sessions its own way. **Claude Code** encodes a project path by
swapping separators for dashes:

```
/Users/you/code/my-repo   →   ~/.claude/projects/-Users-you-code-my-repo/
```

**Codex** writes dated rollouts under `~/.codex/sessions/`, tagged with the `cwd`
they ran in. Either way, sauron folds each session's records into a per-session
edit set, subtracts what you've acknowledged, and shows the remainder.
Acknowledgements live in a small state file, so restarting never loses your
place. It re-tails every two seconds, **writes nothing to your repo**, and
**never talks to a running agent**.

---

## 🗂️ Layout

```
sauron/src/
  agent.rs      ·  the agent seam — selection + spawn/log hooks
  scan.rs       ·  incremental log tailer + the Claude Code reader
  codex.rs      ·  the Codex rollout reader
  model.rs      ·  session model, status classification (agent-agnostic)
  ui.rs         ·  the TUI
  scene.rs      ·  the animated Eye
  clip/         ·  SQLite-compatible Agent Clipboard store + CLI
  handoff.rs    ·  strict opt-in clipboard pass lifecycle
  workspace.rs  ·  the `sauron workspace` launcher + iTerm pane splitting
  orc.rs        ·  the orc charge, cold-file ranking, `sauron orc <file>`
docs/AGENTS.md  ·  using Codex, and adding another agent
```

<div align="center">
<br>
<sub>Reads your logs. Touches nothing.</sub>
</div>
