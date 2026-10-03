# unharness

One terminal UI for every AI coding agent you have an account for.

unharness wraps the vendor CLIs (`claude`, `codex`, `pi`, `agy`) behind a
single ratatui interface with the same transcript, keybindings, permission
prompts, model/effort pickers and session resume regardless of which agent is
running. Switch agents mid-conversation and the context follows you.

## Why

Agent SDKs churn; the vendor CLIs are the stable surface. Each one already
speaks a streaming JSON protocol with a permission channel:

| Harness | Transport | Interactive permissions | Resume | Live models |
|---|---|---|---|---|
| Claude Code | `claude -p --input-format stream-json` (long-lived) | yes, incl. AskUserQuestion | `--resume` | static list |
| Codex | `codex app-server` JSON-RPC (long-lived), `exec --json` fallback | yes (app-server) | thread id | `model/list` |
| pi | `pi --mode rpc` (long-lived) | extension dialogs | `--session-id` | `get_available_models`, many providers |
| Antigravity | per-turn `agy -p` (unverified, no account) | no | no | `agy models` |

unharness normalises those into one event model and one capability set, so
the TUI never assumes what a harness can do. Anything a harness lacks is shown
as a degraded capability rather than failing silently.

## Install

```bash
cargo install --path .
unharness doctor          # harnesses, auth, capabilities, skills CLI, rules
```

Skills are managed by the [`skills`](https://github.com/vercel-labs/skills)
CLI (`npx skills`), which needs Node.js.

## Use

```bash
unharness                                   # TUI with the default harness
unharness -H codex "Refactor the parser"    # pick a harness, start with a prompt
unharness --policy accept-edits             # ask | accept-edits | auto | bypass
unharness -y                                # alias for --policy bypass
unharness --resume                          # resume the last session of the default harness
unharness -p "Summarise src/"               # headless print mode
unharness --no-tui                          # drop into the vendor's own TUI
unharness models                            # providers and models per harness
unharness sessions                          # recorded sessions in this workspace
unharness skills add vercel-labs/agent-skills
```

### In the TUI

| Key | Action |
|---|---|
| `Enter` | Send the prompt |
| `Ctrl+H` | Harness picker |
| `Ctrl+M` | Model picker (provider picker first on multi-provider harnesses) |
| `Ctrl+E` | Reasoning effort picker (levels come from the harness) |
| `Ctrl+P` | Permission policy picker |
| `Ctrl+R` | Resume a recorded session |
| `Ctrl+O` | Expand or collapse the last tool call's output |
| `Esc` / `Ctrl+C` | Interrupt the running turn, else quit |

Slash commands: `/switch`, `/provider`, `/model`, `/effort`, `/policy`,
`/resume`, `/sessions`, `/usage`, `/skills`, `/clear`, `/help`, `/quit`.
Typing `/` opens autocomplete.

When a harness asks for permission, a modal opens: `y` allow once, `a` allow
always (when the harness offers a rule), `n` deny with a reason, `i` show the
full tool input. Agent questions (Claude's AskUserQuestion, Codex's
requestUserInput, pi's extension dialogs) open the matching modal.

### Permission policy

| Policy | Claude Code | Codex (app-server) | Codex (exec) | pi |
|---|---|---|---|---|
| `ask` | default mode, prompts in the TUI | `untrusted` + read-only sandbox, prompts in the TUI | read-only sandbox, cannot prompt* | only extension dialogs prompt* |
| `accept-edits` | `acceptEdits` | `on-request` + workspace-write | workspace-write | falls back to ask* |
| `auto` | `auto` (classifier) | `on-request` + workspace-write | `--approve-for-me` | falls back to ask* |
| `bypass` | `bypassPermissions` | `never` + full access | `--dangerously-bypass-approvals-and-sandbox` | dialogs auto-accepted* |

`*` shown as a warning in the header. A requested policy a harness cannot
honour falls back to the nearest *less* permissive one it supports.

### Switching harnesses

`/switch` shuts the current session down and starts the next harness lazily on
your next prompt, seeding it with the conversation so far (capped by
`bridge_max_chars`, default 24k characters, keeping the tail). Returning to a
harness resumes its own session and bridges only what happened since.

## Configuration

Workspace `unharness.toml` is merged over `~/.config/unharness/config.toml`.

```toml
default_harness  = "claude"      # agy | claude | codex | pi
default_policy   = "ask"
auto_sync        = true          # refresh CLAUDE.md/GEMINI.md symlinks before each run
bridge_max_chars = 24000

[harnesses.claude]
# binary = "/path/to/claude"
default_model  = "opus"
default_effort = "high"
default_policy = "accept-edits"
extra_args     = []

[harnesses.codex]
transport = "auto"               # auto | app-server | exec

[harnesses.pi]
default_provider = "openai-codex"
default_model    = "gpt-5.5"
```

Session ids are recorded in `<workspace>/.unharness/sessions.json` (added to
`.gitignore` by `unharness init`).

## Rules and skills

`AGENTS.md` is the canonical instructions file; `CLAUDE.md` and `GEMINI.md`
are symlinks to it (`unharness init` / `unharness sync`). A `CLAUDE.md` whose
content differs from `AGENTS.md` is never overwritten.

Skills live in `.agents/skills/` and are installed and projected into every
agent's directory by `unharness skills <args...>`, a passthrough to `npx
skills`.

## Architecture

```
src/core/      HarnessId/ProviderId/ModelRef, Capabilities + PermissionPolicy,
               AgentEvent, SessionHandle/SessionCommand, LineProcess,
               per-turn driver, JSON-RPC framing, Registry, SessionsStore
src/harness/   one module per harness: descriptor, capabilities, probe,
               list_models, start_session, build_print_command
  claude/      stream-json transport + parser + fixtures/
  codex/       app-server + exec transports + parsers + fixtures/
  pi/          rpc transport + parser + fixtures/
  legacy/      v1 Antigravity adapter behind the per-turn shim
src/tui/       App state (pure), transcript blocks, modals, rendering, event loop
scripts/       record-*.py capture real vendor sessions; fake-harness.py replays
               them for tests/session_e2e.rs
```

Adding a harness: a `HarnessId` variant, a module implementing `Harness`, a
recorded fixture under `fixtures/` with its expected `.events`, one line in
`Registry::from_config`.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                                   # unit + fixture + e2e (needs python3)
UNHARNESS_UPDATE_FIXTURES=1 cargo test      # regenerate .events after a parser change
scripts/record-claude.py out.jsonl "prompt"  # record a new fixture (run from a scratch dir)
```
