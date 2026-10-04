# AGENTS.md

Project guidelines for AI coding agents working on unharness. `CLAUDE.md` and
`GEMINI.md` are symlinks to this file; edit this one.

## What this is

A Rust TUI/CLI that fronts vendor coding-agent CLIs (Claude Code, Codex, pi,
Antigravity) and any Agent Client Protocol agent through one event model. See
`README.md` for the architecture map.

## Commands

```bash
cargo build
cargo test                                  # unit, fixture replay, e2e (python3)
cargo clippy --all-targets -- -D warnings   # must be clean before a task is done
cargo fmt --check
UNHARNESS_UPDATE_FIXTURES=1 cargo test      # accept new parser output into .events files
```

## Invariants

- **Flag mapping lives only in `src/harness/<name>/`.** The TUI never builds a
  vendor command line. Before changing a flag, run the vendor's `--help` and
  confirm the flag exists on the installed version.
- **Capabilities, not assumptions.** The TUI reads `App::caps()` (the
  harness's declared `Capabilities` plus what its live session reported via
  `CapabilitiesChanged`) to decide what to offer. A harness that cannot do something declares it;
  `resolve_policy` picks the nearest *less* permissive supported policy and
  the warning is shown to the user.
- **Parsers are pure and fixture-tested.** `parse::feed(line) -> Vec<AgentEvent>`
  has no I/O. Every protocol change needs a recording made with the matching
  `scripts/record-*.py` and a committed `fixtures/<case>.jsonl` + `.events`.
  Fixtures are redacted (`/WORKSPACE`, `/HOME`, no emails or account ids);
  grep a new one for your user and org names before committing, because
  paths split across streaming deltas escape the recorders' redaction. A
  fixture used by an e2e test must have the same `>>` lines the driver sends.
- **Sync never destroys user files.** `sync/rules.rs` replaces a file with a
  symlink only when its content is identical; otherwise it warns.
- **Checkpoints never touch the user's repository.** `core/checkpoints.rs`
  commits into a shadow repository under the user's state directory that
  uses the project as its work tree; it must not write anything into the
  project's `.git` (the tests compare refs, status, stash and object count
  before and after), and a restore snapshots the tree first so it can be
  undone. Tests pass their own store directory, never the real one.
- **Nothing an agent can write decides how it is run.** Workspace settings
  are read from `<config dir>/unharness/workspaces/`, never from the
  workspace; an in-tree `unharness.toml` is only imported by `unharness
  init`. The config and state directories are never writable in the sandbox.
- **No silent edits to vendor settings.** unharness does not write to
  `~/.claude`, `~/.codex`, `~/.gemini` or similar. The one exception is
  asked for in config: `[harnesses.claude] relocate_config` copies
  `~/.claude.json` to `~/.claude/.claude.json` once and never touches the
  original.
- **Processes are reaped.** Child processes go through `core::process::
  LineProcess` (kill-on-drop, bounded drain) or the per-turn driver; never a
  bare `tokio::process::Command::spawn` in a transport.
- **Harness processes are spawned sandboxed.** `LineProcess::spawn` takes the
  session's `Sandbox` and `runner.rs` wraps the print command with it; probes
  (`--version`, auth, model lists) and unharness's own `git` stay outside.
  The paths a vendor CLI writes are declared in `src/harness/<name>/`
  (`sandbox_paths`), found by tracing the CLI (`strace -f -e trace=file -e
  status=failed` shows what a confined run was denied), not guessed. Where
  a vendor has its own sandbox it is switched off while ours is active,
  also in `src/harness/<name>/`. The checkpoint store is never writable
  from inside.

## Conversations

`core/conversations.rs` persists the merged transcript (`BlockRecord`), the
harness → vendor session map, bridging bookmarks, per-harness usage, and the
active harness, one JSON file per conversation plus an index. `App::persist`
runs on session start, turn end, harness switch, `/clear`, and quit. Resume
restores all of it; vendor sessions themselves live with the vendor.

## Testing conventions

- Unit tests sit in-module under `#[cfg(test)]`.
- Filesystem behaviour is tested against `tempfile::tempdir()`, never the real
  home directory.
- Transport drivers are tested end to end in `tests/session_e2e.rs` against
  `scripts/fake-harness.py`, which replays a fixture and blocks on every `>>`
  line until the driver sends something.

## Adding a harness

An agent that speaks ACP needs no code: a `[harnesses.<name>]` table with
`protocol = "acp"` and `command = [...]`, or an entry in
`harness::acp::PRESETS`. Record it with `scripts/record-acp.py` and add the
fixture to `src/harness/acp/fixtures/` if it behaves differently from the
recorded ones. For anything else:

1. Add a `HarnessId` constant in `src/core/ids.rs` (ids are interned names;
   the constant is only for built-ins).
2. Create `src/harness/<name>/{mod.rs, transport.rs, parse.rs, fixtures/}`
   implementing `Harness` and declaring honest `Capabilities`. Override
   `quick_auth` only if sign-in can be checked in well under a second; it
   runs at startup to choose the default harness. Declare the CLI's own
   state directories in `sandbox_paths`, and run a turn with the sandbox on.
3. Record a fixture with a `scripts/record-<name>.py` and generate its
   `.events` with `UNHARNESS_UPDATE_FIXTURES=1`.
4. Register it in `Registry::from_config`.
5. Add an e2e test if the transport has a handshake or permission channel.

## Status notes

- Antigravity (`agy`, `src/harness/agy/`) is best effort: no account was
  available. Verified on agy 1.2.15 without credentials: `--print=` (empty)
  with `--input-format stream-json --output-format stream-json` parses, the
  `result` event shape (`fixtures/auth_required.jsonl`), and the `AGY_ERROR:`
  stderr marker. Unverified: the stdin message shape (Claude-compatible by
  default, `transport = "stream-prompt"` sends `{"prompt": …}`), the `init`
  and `step_update` field names (`fixtures/synthetic_turn.jsonl` is
  hand-written and says so), and `agy --output-format json models`. First
  thing to do with an account: `scripts/record-agy.py` from a scratch dir,
  replace the synthetic fixture, regenerate `.events`, fix the parser.
- Codex `app-server` is marked experimental by OpenAI; `transport = "exec"`
  in `[harnesses.codex]` forces the fallback.
- ACP (`src/harness/acp/`) is verified against
  `@agentclientprotocol/claude-agent-acp` 0.85.1 and
  `@agentclientprotocol/codex-acp` 2.1.1. The presets in `PRESETS` (gemini,
  opencode, goose, copilot, cursor, qwen, kiro) use launch commands from the
  ACP registry and were not installed when they were added: unverified until
  recorded. unharness declares no `fs`/`terminal` client capabilities, and
  uses only stable v1 methods; `session/load` (history replay) is implemented
  from the spec but only `session/resume` has been exercised.
- Codex `turn/plan/updated` is parsed from the 0.157.0 app-server schema; no
  recorded session offered the plan tool.
- Subagent threads in Codex report on the main stream under their own
  `threadId`; their `turn/started` / `turn/completed` never end or re-target
  the main turn. They are the subagent's life instead (`SubagentStarted` /
  `SubagentEnded`, the turn's last `agentMessage` being the report): the
  `subAgentActivity` items on the main thread carry no report, and an
  interrupt from outside sends none. Checked on 0.157.0: a sub-agent outlives
  the turn that spawned it, the main thread takes a turn meanwhile, nothing
  follows on the main thread when it ends, `turn/interrupt` on its thread
  and turn stops it (what `StopSubagent` sends). `SubAgentActivityKind` has no `failed`; a sub-agent
  whose command fails completes and says so. Unverified: a failed child turn
  (mapped to `Failed`), `interacted`, nested sub-agents, `multi_agent_v2`
  (stable but off by default).
- Subagents in Claude Code (2.1.289) run in the background unless the model
  passes `run_in_background: false`: the `Agent` call returns a launch
  receipt and the turn can end. Their life is the `system` events of
  `local_agent` tasks (`local_bash` tasks use the same events and are
  ignored): `task_started`, `task_progress`, `task_notification` (which
  ends it; `task_updated` and `background_tasks_changed` repeat it). A task
  resumed with `SendMessage` starts again under the same `task_id` with the
  new call's `tool_use_id`, while its messages keep the original call as
  `parent_tool_use_id`; the parser keys on the original. A notification
  that arrives during a turn is folded into it and still yields a `result`
  with `num_turns: 0`, and results of self-started turns can arrive in a
  batch after the last one. `stop_task` with the task id stops one (what
  `StopSubagent` sends) and Claude then reports it in a turn of its own;
  `interrupt` between turns stops all background tasks and starts no turn. Never seen: a `failed` task (an agent
  cut off by `maxTurns` reports `completed`), nested subagents, subagent
  `stream_event`s. Without `--forward-subagent-text` a blocking subagent's
  text was not sent (its tool calls were); a background one's final message
  arrived either way.
- Rewind and fork, all checked live: Claude `rewind_conversation` (targets
  the uuid we put on each user message; refuses the session's first message
  and uuids from the session a fork came from with "stale target", reported
  as `RewindFailed`), `--resume X --fork-session`; Codex `thread/revert`
  and `thread/fork` (turn ids survive a fork); pi `fork` (a new session,
  only in place once it answers) and `--fork <id>` (entry ids survive).
  Claude's `rewind_files` answers "File rewinding is not enabled" in this
  mode, which is why file restore is unharness's own.
- Claude's `total_cost_usd` is a running total per process, and an ACP
  `usage_update.cost` is a running total per session; both parsers report the
  per-turn difference.

- Sandbox (`src/core/sandbox/`), checked on Linux 7.2 (Landlock ABI 10) with
  Claude Code 2.1.289, Codex 0.157.0 (app-server and exec) and pi 0.87.1: a
  workspace write succeeds, a write to the home directory and a read of
  a denied credential directory fail, sessions resume. Landlock only allows, so reads are granted
  on everything around the denied paths (`linux::read_grants`); directory
  listing reaches into them, file contents do not. Codex's bubblewrap fails
  inside a Landlock domain ("setting up uid map: Permission denied"), hence
  `externalSandbox` on `turn/start`, `danger-full-access` for exec, and
  `-c sandbox_mode="danger-full-access"` on the app-server command line
  (without it the server warns at startup). Claude Code writes
  `~/.claude.json` through a lock directory and a temp file created in the
  home directory, both denied: it carries on without updating the file.
  With `CLAUDE_CONFIG_DIR` set, the file, its lock and its temp files are
  in that directory instead (checked: no denials, signed in from the
  copied file), which is what `relocate_config` uses.
  Unverified: the macOS backend (`seatbelt.rs`; only the profile text is
  tested), a user-enabled Claude Code sandbox inside ours, agy's and the ACP
  presets' state directories, `--no-tui` passthrough under the sandbox.

## Commit style

Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`), one logical
change per commit, tree green (fmt, clippy, tests) at every commit.
