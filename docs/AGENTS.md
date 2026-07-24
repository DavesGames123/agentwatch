# Agents

sauron watches a coding agent's session logs and shows what changed. It is not
tied to one agent: reading the logs is separated from everything else, so Claude
Code, Codex, and anything you add all share the same status model, TUI,
`workspace` launcher, and orcs.

- [The seam](#the-seam)
- [Choosing an agent](#choosing-an-agent)
- [Codex](#codex)
- [Adding a new agent](#adding-a-new-agent)

---

## The seam

Everything agent-specific lives behind one enum, `Agent` (`src/agent.rs`). Given
a repo, an `Agent` answers a handful of questions:

| Method | Answers |
|:--|:--|
| `log_root(repo)` | where this agent's logs live (for the "no sessions" message) |
| `session_files(repo)` | the log files that belong to this repo |
| `session_id(path)` | a stable id from a log file's name |
| `fold(session, record, repo)` | apply one parsed log record to a `Session` |
| `label()` | the CLI name, and the bare "open a session" pane command |
| `resume_cmd(id)` | how a `workspace` pane resumes a session |
| `oneshot_argv(prompt)` | how an orc runs a single-shot task, as `(program, args)` |

`fold` populates a plain `Session` (`src/model.rs`). **The status classification
is shared** — once the `Session` fields are filled, `Session::status` decides
`ERRORED / WAITING ON YOU / WORKING / DELEGATED / NEEDS TEST / CLEAR` the same way
for every agent. So a new agent is really just: *find its log files* and *map its
records onto `Session`*.

The two worked examples to copy from:

- **Claude Code** — `fold_record` in `src/scan.rs`
- **Codex** — `fold` in `src/codex.rs`

---

## Choosing an agent

First match wins:

1. **Flag** — `--claude` / `--codex`
2. **Env** — `SAURON_AGENT=claude|codex`
3. **Auto-detect** — whichever agent has logs for this repo
4. **Default** — Claude Code

```bash
sauron                        # auto-detect
sauron --codex                # watch Codex
SAURON_AGENT=codex sauron
sauron workspace 5 --codex            # Codex cockpit: panes run `codex`
sauron workspace 5 --codex --orcs 2   # …and orcs run `codex exec`
```

The choice threads through the TUI, `--list-working`, the hot/cold detection the
orcs rely on, and every pane `workspace` opens (the watcher pane is launched as
`sauron --<agent>` so a restored window keeps watching the right thing).

---

## Local models (Mordor)

`sauron workspace --mordor` runs the **servants** — hobbits and orcs — against a
**local** model instead of the hosted API. The Eye itself calls no model, so this
touches exactly one thing: the env prepended to each servant pane command. It is
produced by `Agent::local_env(Option<&Mordor>)` in `src/agent.rs` and inserted
right before the `claude` word by the `workspace` launcher:

```
cd repo && ANTHROPIC_BASE_URL=… ANTHROPIC_AUTH_TOKEN=ollama ANTHROPIC_API_KEY= \
           ANTHROPIC_MODEL=<tag> ANTHROPIC_SMALL_FAST_MODEL=<tag> \
           ANTHROPIC_DEFAULT_HAIKU_MODEL=<tag> claude --resume <id>
```

This rides [Ollama's Anthropic-compatible API][ollama-cc] — Claude Code's own wire
format — so **no proxy** is involved. The `Mordor` struct carries the model tag
(default `qwen3-coder`) and the base url (default local Ollama, overridable with
`$SAURON_MORDOR_URL`); the main and background models are pinned to the same tag so
a single-model box never reaches for a `haiku`-class model it never pulled.

**Per-agent.** `local_env` is agent-dispatched. Claude Code emits the `ANTHROPIC_*`
block; **Codex** returns empty, because its local path is `codex --oss`, a
different wiring — so the launcher warns and drops `--mordor` under `--codex`
rather than silently running against the hosted API. A new agent with an
Anthropic-compatible local endpoint slots in by returning the same block; one with
its own scheme returns whatever it needs (a flag baked into `run_cmd`/`resume_cmd`,
its own env), all behind the one seam.

[ollama-cc]: https://docs.ollama.com/integrations/claude-code

---

## Codex

sauron reads Codex rollouts from `~/.codex/sessions/**/*.jsonl`. It matches a
rollout to a repo by the `cwd` recorded in its session-meta header, folds
`message` items into the last prompt and turn state, and pulls the write-set out
of `apply_patch` envelopes (`*** Update File: <path>` / `*** Add File:` /
`*** Delete File:`).

> ⚠️ **Best-effort and unverified.** The Codex reader was written against the
> documented rollout format on a machine with no Codex install. It is defensive —
> it unwraps a `{type, payload}` envelope if present, reads fields at either
> level, and degrades to "session exists, fewer signals" rather than crashing —
> but the exact field names are not certified against real files.

**To certify it**, run `SAURON_AGENT=codex sauron` in a repo you've used Codex in.
If sessions show up with sensible edits and prompts, it works. If not, send along
one `~/.codex/sessions/**/rollout-*.jsonl` — the fix is contained to
`src/codex.rs`.

---

## Adding a new agent

Say you want Aider, Gemini CLI, Cursor, or your own. sauron assumes logs are
**append-only JSONL, one JSON record per line** (`fold` receives each line as a
parsed `serde_json::Value`). If your agent writes that, this is a small,
localized change.

### 1. Add the variant and its hooks

In `src/agent.rs`, add a variant to `Agent` and fill in each `match`:

```rust
pub enum Agent { Claude, Codex, Aider }   // <- new
```

- `label()` → `"aider"`
- `resume_cmd(id)` / `run_cmd(prompt)` → however that CLI resumes / runs one-shot
- `log_root(repo)` / `session_files(repo)` / `session_id(path)` / `fold(...)` →
  delegate to a new reader module (below)

Then teach selection about it: a case in `from_env` (`"aider" => …`) and, if you
want auto-detect, a probe in `select` (e.g. "does `~/.aider` exist?").

### 2. Write the reader module

Create `src/aider.rs` (mirror `src/codex.rs`) with two jobs:

**Discovery** — `session_files(repo) -> Vec<PathBuf>`: return the JSONL logs for
this repo. Claude has one directory per repo; Codex scans a date tree and filters
by recorded `cwd`. Do whichever your agent needs.

**Fold** — `fold(session: &mut Session, v: &Value, repo: &Path)`: apply one record.
Populate the fields the shared classifier reads:

| `Session` field | Fill it when… | Drives |
|:--|:--|:--|
| `last_activity` | any record has a newer timestamp | recency / dormancy |
| `last_prompt` | the user sends a message | the card's "you asked" line |
| `turn_complete` | the agent hands back / finishes; **clear it** on a new user turn or an in-flight tool call | `WORKING` vs settled |
| `edits: path → ts` | a file is written (parse the patch/diff/tool result) | `NEEDS TEST` |
| `previews: path → (ts, lines)` | you can grab the recent lines written to a file | the selected card's per-file preview |
| `open_questions` | the agent asks a question and waits | `WAITING ON YOU` |
| `pending_tools` | a tool call has no result yet (approval prompts) | `WAITING ON YOU` (after a quiet spell) |
| `error: Option<ErrorKind>` | a turn dies (API error / truncation / refusal) | `ERRORED` |
| `agent_launched_ms` | it spawns a background agent it now waits on | `DELEGATED` |

You do **not** write any status logic — `Session::status` (`src/model.rs`) already
turns these fields into the right state, ranking, colour, and glyph.

Reuse the helpers in `src/scan.rs`: `repo_relative(path, repo)` normalizes and
filters a write path to a repo-relative one (dropping scratchpad/anything outside
the repo), and `parse_rfc3339_ms(s)` turns a timestamp into epoch millis.

### 3. Test the parsing

The pure parsing is unit-testable without the agent installed — see the tests at
the bottom of `src/codex.rs` (patch extraction, id-from-filename, a full `fold`).
Feed `fold` a hand-written record and assert on the resulting `Session`.

### 4. That's it

Selection, the TUI, `workspace`, the hobbit/orc panes, hot/cold detection, and
`--list-working` all become available for the new agent with no further work —
they only ever talk to the `Agent` seam and the shared `Session` model.
