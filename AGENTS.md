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
  init`, and never its MCP servers (commands to run). The config and state
  directories are never writable in the sandbox. A config file that does
  not parse is an error, never a silent fall back to defaults.
- **"Allow always" is an unharness rule.** A harness is only ever told
  "allow" or "deny": what it offers to remember (Claude's
  `updatedPermissions`, Codex's `acceptForSession`, an ACP `allow_always`
  option) is not sent, because it lands in vendor settings or dies with the
  session. Rules live in `core/rules.rs` and under the config directory
  (`allow.toml`, `workspaces/<key>.allow.toml`), and match on the
  `ToolAction` each parser derives in `src/harness/<name>/`, which is part
  of the `.events` summary. When a command or path cannot be read off a
  request with certainty the action is `Opaque` and the user is asked; a
  field that only describes a call (ACP `locations`) is not certainty, and
  neither is a request carrying fields no recording had. The shell splitter
  (`shell_segments`) refuses what it does not model instead of guessing;
  a new construct it accepts needs a test showing a shell reads it the
  same way. No rule reaches unharness's own config and state directories.
- **A change to a harness's own configuration is never silent.** The files
  that decide a CLI's next run are declared in `src/harness/<name>/`
  (`guarded`) and compared after every turn (`core/guard.rs`). What a CLI
  rewrites by itself is projected away there (Claude's counters in
  `.claude.json`, Codex's trust entry for the workspace), found by running
  it, so that a warning means something.
- **No silent edits to vendor settings.** unharness does not write to
  `~/.claude`, `~/.codex`, `~/.gemini` or similar. The one exception:
  unless `[harnesses.claude] relocate_config = false`, `~/.claude.json` is
  copied to `~/.claude/.claude.json` once, with a notice, and the original
  is never touched.
- **Processes are reaped.** Child processes go through `core::process::
  LineProcess` (kill-on-drop, bounded drain) or the per-turn driver; never a
  bare `tokio::process::Command::spawn` in a transport.
- **Harness processes are spawned sandboxed.** `LineProcess::spawn` takes the
  session's `Sandbox` and `runner.rs` wraps the print command with it; probes
  (`--version`, auth, model lists) and unharness's own `git` stay outside;
  `doctor`'s MCP handshake does not, since what a server command runs may
  be a file in the workspace.
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
- File attachments (PDF and text): Claude Code 2.1.289 takes `document`
  content blocks on stream-json input (base64 `application/pdf`, and a
  `text` source for `text/plain`), both answered from. ACP files go as a
  `resource_link`, the one block besides text every agent must accept:
  claude-agent-acp 0.85.1 and codex-acp 2.1.1 both read the linked file
  with their own tools (so it must be readable inside the sandbox).
  claude-agent-acp drops embedded `resource` blobs silently, which is why
  files are not embedded. Codex's `UserInput` (0.157.0 schema: text, image,
  localImage, audio, localAudio, skill, mention), `codex exec` and pi's
  `prompt` (`images` only) have no document input.
- File references (`@` completion, `tui/files.rs`, `Harness::
  file_reference`): Claude Code 2.1.289 reads the file of an `@path` in a
  stream-json user message into the turn (answered from with no tool call),
  also after other text (the bridge prefix) and as `@"path with spaces"`,
  and under the sandbox; a missing path is passed on as text. Codex 0.157.0
  `app-server` and pi 0.87.1 `rpc` pass `@path` on as text and do not read
  it, so they get the bare path, as does everything unverified: `codex
  exec`, agy, the ACP agents (claude-agent-acp included), a Claude path
  with a `"` in it. The list is of the session's `cwd`, not the workspace
  root, and is walked again at each `@`; never tried on a tree past the
  50,000 file cap.
- Clipboard images (`tui/clipboard.rs`, `Ctrl+V` and `/paste`): the reader
  logic is tested against a stand-in tool only. No Wayland or X11 display
  and no Mac were available, so the real `wl-paste --list-types` /
  `--no-newline --type`, `xclip -selection clipboard -o -t TARGETS` and
  `osascript -e 'the clipboard as «class PNGf»'` calls are unverified, as
  is which terminals pass `Ctrl+V` through. Dropped files (`tui/drop.rs`)
  are read from a bracketed paste; the quoting forms come from terminal
  documentation, and only the bare and single-quoted ones were sent
  through a real pty.
- MCP servers (`core/mcp.rs`, `[mcp_servers.<name>]`), each run with a stdio
  and an http server defined only in unharness's config, which the model
  then called: Claude Code 2.1.289 `--mcp-config <file>` (added to its own
  servers; the JSON is in a 0600 file under unharness's state directory
  because it holds `env` and `headers`; the flag takes every word up to
  the next flag, hence its position and the `--` before a print prompt),
  Codex 0.157.0 `-c mcp_servers.<name>.command|args|env|url` on
  `app-server` and `exec`, with header values in its environment and
  named by `env_http_headers` (a stdio server's `env` stays on the command
  line: `env_vars` only forwards a variable under its own name, and
  setting it on Codex would hand it to everything Codex starts),
  claude-agent-acp 0.85.1 `mcpServers` on `session/new` (stdio only).
  Codex merges an override key by key into a server of the same name in
  its `config.toml`, even when the override is a whole table (`codex mcp
  get` with a scratch `CODEX_HOME`): a `command` over a `url` stops it
  from starting, which is why such names are left to Codex
  (`own_mcp_servers`). Codex asks to approve an MCP tool call with
  `mcpServer/elicitation/request` carrying `_meta.codex_approval_kind:
  "mcp_tool_call"` and a thread id but no item id (the parser ties it to
  the `mcpToolCall` item in progress on that thread); under `never` with a
  read-only sandbox it refuses the call instead, under `never` with full
  access it runs it. Failed servers: Claude's `init` lists `status:
  "failed"`, Codex sends `mcpServer/startupStatus/updated` (twice, it
  retries). Neither CLI stops what an MCP launcher (`sh -c`, `npx`)
  started when it exits. Unverified: http servers over ACP,
  `session/resume` and `session/load` with servers, codex-acp, a same-name
  server in Claude's own config, how to answer the elicitation's `persist`
  offer (so "always allow" is not offered), agy (`agy mcp add` writes its
  own config; no session flag on 1.2.16) and pi (no MCP client).
- Plan mode (`Capabilities::plan_mode`) is declared, not driven: nothing in
  unharness enters it yet. What the flag stands on: Claude Code 2.1.289
  `--permission-mode plan`, Codex 0.157.0 `collaborationMode` on
  `turn/start` (app-server schema; `exec` has none), agy 1.2.16 `--mode
  plan`, all read from `--help` or the schema and none run; an ACP session
  reports it when its `modes` list one with id `plan` (claude-agent-acp
  0.85.1 does, codex-acp 2.1.1 does not). pi plans only through an
  extension.
- Allow rules, checked live on Claude Code 2.1.289 and Codex 0.157.0
  (app-server): a shell rule written from a Claude `Bash` request answered
  Codex's request for the same command in the same workspace, a command
  joined to another with `&&` was still asked about, and an edit rule
  answered Claude's next `Write`. Recorded: Codex's
  `item/fileChange/requestApproval` has no changes of its own, only the
  `itemId` of the `fileChange` item that does
  (`fixtures/app_server_file_change.jsonl`). Unverified: codex-acp 2.1.1
  asked for no permission in its default mode in three recordings (a shell
  command, an edit, a network command), so the action of a non-Claude ACP
  request is built from its tool calls' shapes (`rawInput.command` and
  `cwd`, `diff` content paths) and the spec's `kind`, and a read, move or
  delete is `Opaque`; Codex's `move_path` on a rename, and a command or
  file approval with `reason`, `grantRoot` or any field the recordings
  lack (`Opaque`); the wording of Codex's MCP approval question, which the
  parser checks for the server and tool (one recording); Claude's
  `NotebookEdit` and `MultiEdit` inputs (field names from the tool schemas,
  no request recorded). An ACP agent that offers `allow_always` and no
  `allow_once` gets a cancel and the user a notice. Not done: a lock
  around appending to a rules file (two unharness saving at once can lose
  one rule), re-reading rules while running, closing the modal when Claude
  cancels a request.
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
  Landlock cannot protect a file in a directory where the CLI creates
  files, and every CLI does so beside its config (Codex's SQLite files in
  `~/.codex`, pi's lock files in `~/.pi/agent`, Claude's relocated config),
  which is why those files are watched and not write-protected.
  Unverified: the macOS backend (`seatbelt.rs`; only the profile text is
  tested), a user-enabled Claude Code sandbox inside ours, agy's and the ACP
  presets' state directories, `--no-tui` passthrough under the sandbox.

## Commit style

Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`), one logical
change per commit, tree green (fmt, clippy, tests) at every commit.
