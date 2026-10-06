<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img alt="unharness" src="assets/logo-light.svg" width="312">
  </picture>
</p>

<p align="center">
  <strong>One terminal UI for every coding agent you already have an account for.</strong>
</p>

<p align="center">
  <a href="https://github.com/Liquescent-Development/unharness/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/Liquescent-Development/unharness/ci.yml?branch=main&style=flat-square&label=CI"></a>
  <a href="https://github.com/Liquescent-Development/unharness/releases/latest"><img alt="Release" src="https://img.shields.io/github/v/release/Liquescent-Development/unharness?style=flat-square"></a>
  <a href="LICENSE"><img alt="License: AGPL-3.0" src="https://img.shields.io/badge/license-AGPL--3.0-blue?style=flat-square"></a>
  <img alt="Platforms: Linux, macOS" src="https://img.shields.io/badge/platform-linux%20%7C%20macOS-lightgrey?style=flat-square">
</p>

---

unharness runs Claude Code, Codex, pi, Antigravity and any
[Agent Client Protocol](https://agentclientprotocol.com) agent behind one
interface. The transcript, keys, permission prompts, model pickers and
resume work the same whichever agent is running, and you can switch agents
mid-conversation without losing context.

- **One interface.** Every agent gets the same transcript, keybindings,
  model and effort pickers, and session resume.
- **Switch mid-conversation.** `/harness codex` hands the conversation to
  another agent. Switching back resumes the first agent's own session.
- **One sandbox for every agent.** Landlock on Linux and `sandbox-exec` on
  macOS confine the whole agent process: it can write only to your
  project, and cannot read your cloud and signing credentials.
- **`ask` means `ask`.** Nothing writes, runs or reaches out without your
  answer, on every harness that offers the policy. "Always allow" rules
  belong to unharness and apply to every agent.
- **Rewind with your files.** Go back to any earlier prompt, and restore
  the working tree to how it was then, on any harness. Checkpoints never
  touch your `.git`.
- **Configure once.** MCP servers, skills and `AGENTS.md` are set up once
  and handed to each agent in the form it understands.

## Install

```bash
# Shell installer (Linux, macOS)
curl -fsSL https://github.com/Liquescent-Development/unharness/releases/latest/download/unharness-installer.sh | sh

# Homebrew
brew install Liquescent-Development/tap/unharness

# From source (Rust 1.89+)
cargo install --locked --git https://github.com/Liquescent-Development/unharness
```

You also need at least one agent CLI, installed and signed in:
[Claude Code](https://docs.anthropic.com/en/docs/claude-code),
[Codex](https://github.com/openai/codex), [pi](https://pi.dev),
Antigravity (`agy`), or an [ACP agent](docs/acp.md). The sandbox needs
Linux 6.2 or newer; the macOS backend has not been run on a Mac yet.
Managing skills needs Node.js.

Then check what unharness found:

```bash
unharness doctor
```

## Quick start

```bash
unharness                                   # TUI with the default harness
unharness -H codex "Refactor the parser"    # pick a harness, start with a prompt
unharness --policy accept-edits             # ask | accept-edits | auto | bypass
unharness --sandbox read-only               # read-only | workspace-write (default) | off
unharness --resume                          # resume the latest conversation
unharness -p "Summarise src/"               # headless print mode
unharness models                            # providers and models per harness
```

See [Usage](docs/usage.md) for every command and flag.

## Supported agents

| Harness | Transport | Interactive permissions | Resume | Live models |
|---|---|---|---|---|
| Claude Code | `claude -p --input-format stream-json` (long-lived) | yes, incl. AskUserQuestion | `--resume` | static list |
| Codex | `codex app-server` JSON-RPC (long-lived), `exec --json` fallback | yes (app-server) | thread id | `model/list` |
| pi | `pi --mode rpc` (long-lived) | yes, through an extension unharness loads | `--session-id` | `get_available_models`, many providers |
| Antigravity | `agy --print= --input-format stream-json` (long-lived) | no | `--conversation` | `agy models` |
| ACP agents | Agent Client Protocol v1 over stdio (long-lived) | yes, when the agent asks | `session/resume` or `session/load` | from the session |

unharness drives the vendor CLIs, which change more slowly than the agent
SDKs, and normalises their streaming protocols into one event model. Each
harness declares what it can do, and anything it lacks shows up as a
caveat in the TUI instead of failing silently:

| | Claude Code | Codex (app-server) | pi | ACP agents | Codex exec, Antigravity |
|---|---|---|---|---|---|
| Image attachments | yes | yes | per model | per agent | exec only |
| PDF and text file attachments | yes | no | no | yes (as a link the agent reads) | no |
| Plan / todo list | yes | yes | no | yes | exec only |
| Subagents: start, end, tool calls, report | yes | yes | no | no | no |
| Subagents: task description and progress in words | yes | no (a name and its tool calls) | no | no | no |
| Subagents: stop one | yes | yes | no | no | no |
| Steer a running turn | yes | yes | yes | no (queued) | no (queued) |
| Compact on request | yes | yes | yes | no | no |
| Context-window gauge | yes | yes | yes | yes | no |
| Rate-limit windows | yes | yes | no | no | no |
| Native rewind | yes | yes | yes | no (fresh session) | no (fresh session) |
| Native fork | yes | yes | yes | no (fresh session) | no (fresh session) |
| MCP servers from unharness's config | yes | yes | no | yes (http ones per agent) | exec only |

## Permissions and sandbox

Four policies mean the same on every harness. If a harness lacks the one
you asked for, unharness falls back to the nearest **less** permissive
policy, never a more permissive one. If there is none, it asks you which
to use.

| Policy | Claude Code | Codex (app-server) | Codex (exec) | pi | Antigravity |
|---|---|---|---|---|---|
| `ask` | default mode, prompts in the TUI | `untrusted`, prompts in the TUI | not available | every tool call but a read prompts in the TUI | not available |
| `accept-edits` | `acceptEdits` | `on-request` | no prompts | falls back to ask* | `--mode accept-edits`: edits run, a shell command is refused and ends the turn |
| `auto` | `auto` (classifier) | `on-request` | `--approve-for-me` | falls back to ask* | falls back to accept-edits* |
| `bypass` | `bypassPermissions` | `never` + full access | `--dangerously-bypass-approvals-and-sandbox` | nothing is asked, your extensions' dialogs are auto-accepted* | `--dangerously-skip-permissions` |

`*` shown as a warning in the header.

The sandbox is a separate setting. No policy changes it, not even
`bypass`:

| Level | Writes |
|---|---|
| `workspace-write` (default) | workspace, harness state, temp |
| `read-only` | harness state and temp only |
| `off` | unconfined |

Read more in [Permissions](docs/permissions.md) and [Sandbox](docs/sandbox.md).

## Essential keys

| Key | Action |
|---|---|
| `Enter` / `Alt+Enter` | Send (queued during a turn) / steer the running turn |
| `Esc` | Interrupt the running turn |
| `@` | Reference a file, with fuzzy search |
| `Ctrl+H` · `Ctrl+M` · `Ctrl+E` · `Ctrl+P` | Harness · model · effort · policy |
| `Ctrl+R` | Resume a saved conversation |
| `Ctrl+V` | Attach the clipboard image |
| `Ctrl+S` | Open a subagent's transcript |
| `/rewind`, `/fork` | Go back to an earlier prompt; branch the conversation |

All keys and slash commands are listed in [The TUI](docs/tui.md).

## Documentation

- [Usage](docs/usage.md): commands, flags, resume, switching harnesses
- [The TUI](docs/tui.md): keys, slash commands, attachments, rewind, subagents
- [Permissions](docs/permissions.md): policies and allow rules
- [Sandbox](docs/sandbox.md): what is confined and how
- [Configuration](docs/configuration.md): `config.toml` reference
- [MCP servers](docs/mcp.md) · [ACP agents](docs/acp.md) · [Rules and skills](docs/skills.md)

## Contributing

Bug reports and pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md)
covers the development setup and architecture, and [AGENTS.md](AGENTS.md)
lists the invariants every change keeps.

## License

unharness is free software under the [GNU Affero General Public License,
version 3 or later](LICENSE).
