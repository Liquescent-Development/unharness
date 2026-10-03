# unharness

A vendor-neutral CLI runner, skill orchestrator, and rule synchronizer for AI coding agents.

## Why unharness?

Coding agent SDK contracts change constantly, breaking custom supervisors and supervisory channels. However, the underlying harnesses maintain battle-tested CLI tools (`agy` for Antigravity, `claude` for Claude Code, `codex` for Codex).

Different harnesses expect project context and agent skills in different directory structures:
- **Claude Code** looks in `.claude/skills/<name>/SKILL.md` and reads `CLAUDE.md`.
- **Antigravity (`agy`)** discovers workspace skills in `.agents/skills/<name>/SKILL.md` and reads `GEMINI.md` / `AGENTS.md`.
- **Codex** reads `AGENTS.md` and `.codex/skills/`.

`unharness` decouples you from any single vendor harness:
1. **Single Source of Truth:** Skills live once in standard Agent Commons format (`.agents/skills/`) and rules live in `AGENTS.md`.
2. **Transparent Cross-Harness Sync:** Automatically projects and symlinks skills and instructions into `.claude/skills`, `~/.gemini/antigravity-cli/.agents/skills`, `CLAUDE.md`, and `GEMINI.md`.
3. **Unified CLI Dispatcher:** One command (`unharness`) to run any harness interactively or in print/headless mode with normalized flag translations (`-p`, `-y`, `-m`, `-e`).
4. **Seamless Switching:** Easily switch your default provider between `agy`, `claude`, and `codex` (`agy -> claude -> codex` fallback by default).

---

## Installation

Built in Rust:

```bash
cargo install --path .
```

Verify installation:

```bash
unharness --version
unharness doctor
```

---

## Quickstart

### 1. Initialize a Repository

Initialize `unharness` in any project:

```bash
unharness init
```

This:
- Creates `.agents/skills/` for portable skills.
- Creates `AGENTS.md` as the canonical project instructions.
- Symlinks `CLAUDE.md -> AGENTS.md` and `GEMINI.md -> AGENTS.md`.
- Creates `unharness.toml`.

### 2. Inspect Environment & Health

```bash
unharness doctor
```

Checks:
- Installed harnesses (`agy`, `claude`, `codex`, `pi`) and their versions on `$PATH`.
- Authentication status (e.g., active GCP project for `agy`, active enterprise org for `claude`).
- Active default harness resolution.
- Workspace skills and projection health.
- User global skills in `~/.agents/skills` and their projections into `~/.claude/skills` and `~/.gemini/antigravity-cli/.agents/skills`.

### 3. Run Agents

Run with the default harness (Antigravity by default, or Claude if configured):

```bash
## Unified TUI Experience

When you start an interactive session (`unharness` or `unharness "your task"`), `unharness` launches a **single, consistent terminal interface** regardless of whether you're running Antigravity, Claude Code, or Codex.

```
┌─ UNHARNESS  [Claude Code (claude)] [Model: claude-opus-5-5] [Effort: high] [Auto: ON]  Thinking (3.2s) ─┐
│  Repository: /Users/kiener/code/alpha/unharness                                                          │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌─ Activity ──────────────────────────────────────────────────────────────────────────────────────────────┐
│  ❯ You                                                                                                  │
│    Refactor the parser module                                                                           │
│                                                                                                         │
│  ┌─ 💭 Thinking (3.2s) ───────────────────────────────────────────────────                              │
│  │ Checking parser error types and ensuring nom combinators match...                                    │
│  └────────────────────────────────────────────────────────────────────────                              │
│                                                                                                         │
│  ● Claude Code (claude) (3.8s)                                                                          │
│    Here is the updated parser with the unified diff:                                                    │
│                                                                                                         │
│    ┌─ diff ───────────────────────────────────────────────                                              │
│    │ diff --git a/src/parser.rs b/src/parser.rs                                                         │
│    │- fn parse_legacy(input: &str) -> Result<Data> {                                                    │
│    │+ pub fn parse_telemetry(input: &str) -> Result<Data> {                                             │
│    └──────────────────────────────────────────────────────                                              │
└─────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌─ Prompt (type / for commands) ────────────────────────────────────────────────┐
│                                                                               │
└───────────────────────────────────────────────────────────────────────────────┘
Enter Send  Ctrl+H Harness  Ctrl+M Model  Ctrl+P Auto  PgUp/PgDn Scroll  Ctrl+C/Esc Quit
```

### In-TUI Controls & Shortcuts

- **`Ctrl+E`**: Open the **Reasoning Effort / Think Level Picker Modal** (`high`, `xhigh`, `max`, `medium`, `low`)
- **`Ctrl+M`**: Open the **Model Picker Modal** to select models for the active harness
- **`Ctrl+H`**: Open the **Harness Picker Modal** to switch AI harness (`agy` ⇄ `claude` ⇄ `codex`)
- **`Ctrl+P`**: Toggle auto-approvals on/off
- **`Tab`**: Autocomplete selected slash command, model, or effort level
- **`↑` / `↓`**: Navigate suggestions or modal lists, or scroll chat
- **`PgUp` / `PgDn`**: Fast scroll through conversation history
- **`Ctrl+C` / `Esc`**: Cancel running turn or exit

### Slash Commands & Autocomplete

Typing `/` in the prompt automatically pops up an interactive command suggestions dropdown:

- `/effort` or `/think` — Open reasoning effort / think mode picker modal
- `/effort <level>` (or `/think <level>`) — Set effort directly (`high`, `xhigh`, `max`, `medium`, `low`)
- `/model` — Open model picker modal for the active harness
- `/model <name>` — Set the model directly (e.g. `/model opus`, `/model sonnet`, `/model fable`, `/model gemini-3.8-flash-high`)
- `/switch` — Open the modal harness switcher picker
- `/switch <agy|claude|codex>` — Switch directly to a specific harness
- `/skills` — Show skills currently loaded from `.agents/skills`
- `/auto` — Toggle tool auto-approvals
- `/clear` — Clear chat history from the screen
- `/help` — Display in-app help and shortcuts
- `/quit` — Exit the TUI

To bypass the TUI and drop directly into the underlying vendor CLI, pass `--no-tui` (or `--raw`):

```bash
unharness --no-tui "Prompt"
```

# Headless / one-shot print mode
unharness -p "Explain the role of supervisor in this architecture"

# Auto-approve tool permissions (translates to --dangerously-skip-permissions for agy, --permission-mode auto for claude)
unharness -y -p "Run the test suite and report failures"
```

Explicitly target a specific harness:

```bash
# Force Antigravity
unharness -H agy "Generate test suite"

# Force Claude Code
unharness -H claude "Review recent commits"

# Force Codex
unharness -H codex "Implement feature"
```

### 4. Switch Default Harness

```bash
# Switch workspace default
unharness switch claude
unharness switch agy

# Switch global default across all repositories
unharness switch claude --global
```

### 5. Manage & Validate Skills

```bash
# List all workspace and global skills with summaries
unharness skills list

# Create a new portable skill in .agents/skills/<name>/SKILL.md
unharness skills create optimize-queries

# Create a global skill in ~/.agents/skills/<name>/SKILL.md
unharness skills create optimize-queries --global

# Validate YAML frontmatter and directory naming across all skills
unharness skills validate

# Explicitly re-synchronize symlinks
unharness sync
```

---

## Configuration (`unharness.toml`)

```toml
default_harness = "agy" # Fallback: agy -> claude -> codex
auto_sync = true        # Runs quick pre-flight skill/rule sync before execution

[harnesses.agy]
default_model = "gemini-3.8-flash-high"
# default_effort = "high"

[harnesses.claude]
# default_model = "claude-3-7-sonnet-20250219"

[harnesses.codex]
# binary = "/path/to/codex"
```

Global configuration can be placed in `~/.config/unharness/config.toml`.

---

## How Skills and Projections Work

```
Workspace
├── AGENTS.md                  <-- Canonical instructions
├── CLAUDE.md -> AGENTS.md     <-- Symlink for Claude Code
├── GEMINI.md -> AGENTS.md     <-- Symlink for Antigravity
├── .agents/skills/            <-- Canonical skills directory
│   ├── spec-cycle/
│   │   └── SKILL.md
│   └── code-review/
│       └── SKILL.md
└── .claude/skills/            <-- Managed symlink projections
    ├── spec-cycle -> ../../.agents/skills/spec-cycle
    └── code-review -> ../../.agents/skills/code-review

User Home (~/)
├── .agents/skills/            <-- Canonical global skills
│   └── <skill>/SKILL.md
├── .claude/skills/            <-- Projected symlinks for Claude Code
│   └── <skill> -> ../../.agents/skills/<skill>
└── .gemini/antigravity-cli/.agents/skills/
    └── <skill> -> ../../../../.agents/skills/<skill>
```

When you edit a skill in `.agents/skills/`, every harness immediately sees the update. Stale symlinks are automatically cleaned up when skills are removed.
