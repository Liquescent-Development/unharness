# Usage

## Commands

```bash
unharness                                   # TUI with the default harness
unharness -H codex "Refactor the parser"    # pick a harness, start with a prompt
unharness -p "Summarise src/"               # headless: print the answer
unharness --no-tui                          # run the vendor's own TUI instead
```

| Command | What it does |
|---|---|
| `unharness doctor` | Harnesses, sign-in, capabilities, sandbox, MCP servers, skills CLI and allow rules |
| `unharness models [-H <harness>] [--provider <p>]` | Providers and models per harness |
| `unharness sessions [--clear]` | Saved conversations in this workspace; `--clear` removes them, the prompt history and the checkpoints |
| `unharness init` | Sets up `AGENTS.md`, `.agents/skills`, the rules symlinks and the workspace config file |
| `unharness sync` | Refreshes the `CLAUDE.md` / `GEMINI.md` symlinks to `AGENTS.md` |
| `unharness switch <harness> [-g]` | Sets the default harness for this workspace, or globally with `-g` |
| `unharness skills <args…>` | Manages skills (see [Rules and skills](skills.md)) |
| `unharness update [<version>] [--check] [--prerelease]` | Updates unharness itself when the shell installer put it there; otherwise says what to run (see [Updating](../README.md#updating)) |

## Flags

| Flag | Meaning |
|---|---|
| `-H, --harness <id>` | Harness to use: `claude`, `codex`, `pi`, `agy`, or a configured [ACP agent](acp.md). Also `UNHARNESS_HARNESS` |
| `-m, --model <model>` | Model for the session |
| `--provider <provider>` | Provider within the harness (pi has many) |
| `-e, --effort <level>` | Reasoning effort; the levels come from the harness |
| `--policy <policy>` | `ask` (default), `accept-edits`, `auto` or `bypass`. Also `UNHARNESS_POLICY`. See [Permissions](permissions.md) |
| `-y, --yes` | Short for `--policy bypass` |
| `--sandbox <level>` | `workspace-write` (default), `read-only` or `off`. Also `UNHARNESS_SANDBOX`. See [Sandbox](sandbox.md) |
| `--resume [id]` | Resume the latest conversation, or the one whose id starts with `id` |
| `-p, --print` | Run one prompt without the TUI and print the answer; the prompt may come on stdin. See [Headless runs](headless.md) |
| `--format <format>` | With `-p`: `text` (default), `json` or `stream-json` |
| `--native` | With `-p`: run the harness's own print mode and pass its output through |
| `--no-tui` | Hand the terminal to the harness's own interface |
| `--no-sync` | Skip refreshing the rules symlinks before the run |

## The default harness

Without `-H` and without `default_harness` in the config, unharness picks
the first installed harness that is signed in, in this order: Antigravity,
Claude Code, Codex, pi, then ACP agents.

Antigravity, Claude Code and Codex can report their sign-in quickly. pi and
ACP agents cannot: pi needs a model query and an ACP agent needs a session.
So pi and ACP agents are chosen only when none of the first three is signed
in, and ahead of any harness that is known to be signed out. `unharness
doctor` shows the choice as "Active Default".

## Conversations

A conversation is what you resume. unharness saves each one under
`<workspace>/.unharness/conversations/`, which `unharness init` adds to
`.gitignore`. A saved conversation holds:

- the merged transcript;
- the vendor session id of every harness that took part;
- the bridging bookmarks (what each harness has already seen);
- which harness was active.

`unharness --resume`, `/resume` and `Ctrl+R` put the transcript back in the
pane. Each harness reattaches to its own vendor session when you
`/harness` to it, and is bridged only what it has not seen yet. With
`--resume`, `-H` overrides which harness continues.

## Switching harnesses

`/harness` (or `/switch`, or `Ctrl+H`) shuts the current session down. The
next harness starts on your next prompt and is seeded with the
conversation so far. The seed is capped by `bridge_max_chars` (24,000
characters by default) and keeps the most recent part. Returning to a
harness resumes its own session and bridges only what happened since.

Policy, model and effort changes apply from the next turn on every harness:

| Harness | How the change is applied |
|---|---|
| Claude Code | its control channel |
| Codex | each `turn/start` |
| pi | an RPC command |
| Antigravity | its process is restarted on the same conversation |

Antigravity has no separate effort setting. The effort is the last part
of its model ids, and `--effort` is never passed to it.

A sandbox change (`/sandbox`) applies to the process, so the harness is
restarted on its session with the next prompt.
