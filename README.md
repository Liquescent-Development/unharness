# unharness

A vendor-neutral TUI and CLI runner for AI coding agents.

## Why unharness?

Coding agent SDK contracts change constantly. The vendor CLIs are the stable
surface: `claude` (Claude Code), `codex` (OpenAI Codex), `agy` (Antigravity),
`pi`. unharness wraps those CLIs so you get:

1. **One TUI for every harness.** Same keybindings, same transcript, same
   model/effort/permission controls regardless of which agent is running.
2. **Switch harness mid-conversation.** Context is bridged into the next
   agent's session.
3. **One source of truth for project rules.** `AGENTS.md` is canonical;
   `CLAUDE.md` and `GEMINI.md` are symlinks to it.
4. **Skills managed by the standard tool.** Skills live in `.agents/skills/`
   and are installed and projected into every agent's directory by the
   [`skills`](https://github.com/vercel-labs/skills) CLI. unharness does not
   reimplement that.

## Installation

```bash
cargo install --path .
unharness --version
unharness doctor
```

The `skills` subcommand needs Node.js (`npx skills`) or a global
`npm install -g skills`.

## Quickstart

```bash
unharness init          # AGENTS.md, .agents/skills/, symlinks, unharness.toml
unharness doctor        # installed harnesses, auth, skills CLI, rules status
unharness "Refactor the parser module"      # TUI with the default harness
unharness -H claude "Review recent commits" # force a harness
unharness -p "Explain the runner"           # headless print mode
unharness --no-tui                          # drop into the vendor CLI directly
```

### Skills

```bash
unharness skills add vercel-labs/agent-skills   # install into .agents/skills and project everywhere
unharness skills list
unharness skills update
unharness skills init my-skill                   # scaffold a new SKILL.md
```

Every argument after `skills` is passed straight to the `skills` CLI.

### Rules

```bash
unharness sync          # ensure CLAUDE.md and GEMINI.md -> AGENTS.md
```

If a `CLAUDE.md` exists without `AGENTS.md`, it is promoted to `AGENTS.md`
and replaced with a symlink. A `CLAUDE.md` or `GEMINI.md` whose content
differs from `AGENTS.md` is never overwritten; `doctor` warns instead.

### Switch default harness

```bash
unharness switch claude            # workspace (unharness.toml)
unharness switch claude --global   # ~/.config/unharness/config.toml
```

## TUI

```
┌─ UNHARNESS  [Claude Code (claude)] [Model: opus] [Effort: high] [Auto: ON] ─┐
│  Repository: /home/you/code/project                                        │
└────────────────────────────────────────────────────────────────────────────┘
┌─ Activity ─────────────────────────────────────────────────────────────────┐
│  ❯ You                                                                     │
│    Refactor the parser module                                              │
│  ┌─ 💭 Thinking (3.2s) ───────────────────────────────                      │
│  │ Checking parser error types...                                          │
│  └──────────────────────────────────────────────────                       │
│  ● Claude Code (claude) (3.8s)                                             │
│    Here is the updated parser...                                           │
└────────────────────────────────────────────────────────────────────────────┘
┌─ Prompt (type / for commands) ──────────────────────────────────────────────┐
│                                                                            │
└────────────────────────────────────────────────────────────────────────────┘
```

| Key | Action |
|---|---|
| `Ctrl+H` | Harness picker |
| `Ctrl+M` | Model picker |
| `Ctrl+E` | Reasoning effort picker |
| `Ctrl+P` | Toggle auto-approve |
| `Tab` | Accept suggestion |
| `↑`/`↓`, `PgUp`/`PgDn` | Navigate suggestions or scroll |
| `Ctrl+C` / `Esc` | Cancel the running turn, or quit |

Slash commands: `/switch [harness]`, `/model [name]`, `/effort [level]`,
`/skills`, `/auto`, `/clear`, `/help`, `/quit`.

## Configuration

Workspace `unharness.toml` (merged over `~/.config/unharness/config.toml`):

```toml
default_harness = "agy"       # agy | claude | codex
default_policy  = "ask"       # permission policy (ask | accept-edits | auto | bypass)
auto_sync       = true        # refresh rules symlinks before each run

[harnesses.claude]
# binary = "/path/to/claude"
default_model  = "opus"
default_effort = "high"
extra_args     = []

[harnesses.agy]
default_model = "gemini-3.8-flash-high"
```

Any harness id can appear under `[harnesses.<id>]`; unknown ids are ignored.

## Layout

```
Workspace
├── AGENTS.md                 canonical instructions
├── CLAUDE.md -> AGENTS.md    symlink for Claude Code
├── GEMINI.md -> AGENTS.md    symlink for Antigravity
├── .agents/skills/           canonical skills (managed by `skills`)
├── .claude/skills/           projections created by `skills`
└── unharness.toml
```

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

See `AGENTS.md` for project conventions.
