# AGENTS.md

Project guidelines for AI coding agents working on unharness. `CLAUDE.md` and
`GEMINI.md` are symlinks to this file; edit this one.

## What this is

A Rust TUI/CLI that fronts vendor coding-agent CLIs (Claude Code, Codex, pi,
Antigravity) and any Agent Client Protocol agent through one event model. See
`CONTRIBUTING.md` for the architecture map and `docs/` for user documentation.

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
  the warning is shown to the user. It never picks a more permissive one:
  when nothing at or below the request is supported the user chooses (the
  TUI holds the session and opens the picker, `--print` and `--no-tui`
  fail).
- **`ask` is a guarantee, not a best effort.** Nothing that writes, executes
  or reaches out runs without the user's answer or an allow rule; reads may
  run. A harness, transport or run mode that cannot hold to that does not
  declare `ask` (`Capabilities::permission_policies`, `Harness::
  print_policies` for `--print --native` and `--no-tui`), and there is no
  degraded `ask`. A `--print` run is a session that answers every request
  itself: allowed by a rule, denied otherwise (`src/headless.rs`). Declaring it needs a
  recording or a live run that shows the asking. The policy never chooses
  a sandbox level.
- **Parsers are pure and fixture-tested.** `parse::feed(line) -> Vec<AgentEvent>`
  has no I/O. Every protocol change needs a recording made with the matching
  `scripts/record-*.py` and a committed `fixtures/<case>.jsonl` + `.events`.
  Fixtures are redacted (`/WORKSPACE`, `/HOME`, no emails or account ids);
  grep a new one for your user and org names before committing, because
  paths split across streaming deltas escape the recorders' redaction. A
  fixture used by an e2e test must have the same `>>` lines the driver sends.
- **Sync never destroys user files.** `sync/rules.rs` replaces a file with a
  symlink only when both read and their content is identical, and never
  touches a link or file a linked `AGENTS.md` passes through; otherwise
  it warns.
  `sync/projections.rs` does the same, rewrites only files whose first
  line is its `MARKER`, and removes only its own links and generated
  files once their source under `.agents/` is gone. Sync runs outside the
  sandbox, so it never writes through a link (`AGENTS.md` is created
  `O_EXCL`, so a dangling link there is left with a warning; a vendor
  directory linked out of the workspace or nowhere is refused, quietly
  when nothing is to be projected, generated files are read `O_NOFOLLOW`
  and replaced by a rename of a new file beside them, so a failed write
  leaves the old one), never projects a source onto the file it resolves to,
  and refuses a file name with a control character (which could add a
  line to a generated role), a backslash, or a character that changes
  how the text around it is shown (zero-width and direction marks, line
  and paragraph separators, bidirectional overrides and isolates, the
  byte order mark). It reads only regular files
  of at most 4 MiB, opened without blocking, so a FIFO or a device an
  agent left in the workspace cannot hang it.
- **Checkpoints never touch the user's repository.** `core/checkpoints.rs`
  commits into a shadow repository under the user's state directory that
  uses the project as its work tree; it must not write anything into the
  project's `.git` (the tests compare refs, status, stash and object count
  before and after), and a restore snapshots the tree first so it can be
  undone. Tests pass their own store directory, never the real one.
- **Nothing an agent can write decides how it is run.** Workspace settings
  are read from `<config dir>/unharness/workspaces/`, never from the
  workspace: an in-tree `unharness.toml` is not read by anything. The config and state
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
  bare `tokio::process::Command::spawn` in a transport. Each leads a
  session of its own (`setsid`), and is killed with its process group and,
  on Linux, every process descended from it (stopped first, read from
  `/proc`): Claude Code runs each command in a session of its own, which a
  group kill misses. What it left in its group goes when it exits by
  itself, killed before it is reaped (a pidfd) so that the group's id
  names no other, and its tree when the runtime drops the watcher. A
  session ends the way its CLI expects (`Shutdown`, `LineProcess::end`: what the
  driver sends first, then stdin closed) and is killed only after
  `END_GRACE`, also when the handle is dropped meanwhile (a switch, a
  fork, a resume, a sandbox or provider change; `/clear` keeps the
  session). Mid-turn a CLI that is sent nothing to stop the turn (all
  but Claude) is killed at once (`end_or_kill`): given its grace it could
  go on with the turn where nobody sees it. Quitting waits for every CLI
  to be gone (`LineProcess::all_ended`), and SIGTERM, SIGHUP or SIGQUIT
  to unharness is a quit, since none reaches a CLI in a session of its
  own; a signal unharness was started with ignored (`nohup`, SIGINT in a
  background job) is not listened for; one while the prompt is in
  `$EDITOR` ends the editor and what it started (SIGTERM, SIGKILL after
  2 s; it shares unharness's process group, for the terminal) and then
  unharness. The TUI says when it waits, and
  Ctrl+C, Ctrl+D or one of those signals stops the wait. The TUI resumes
  or forks a vendor session only once the CLI that ended it is gone
  (`tui::Ending`), so that two never write one session: one still there
  after `END_GRACE + DRAIN_TIMEOUT + 1s` (its driver stuck) is killed
  through the session's `ProcessSlot`; meanwhile it says so, refuses a switch,
  resume, fork or rewind (each would take the held start for its own),
  and an interrupt drops the prompt that waits; a rewind still held at
  quit forgets the vendor session it was for. Not caught: what left the tree
  before the kill (a `setsid cmd &` whose shell exited, a double fork);
  its pipes are closed on it once the drain is up.
- **Harness processes are spawned sandboxed.** `LineProcess::spawn` takes the
  session's `Sandbox` and `runner.rs` wraps the print command with it; probes
  (`--version`, auth) and unharness's own `git` stay outside. A model or
  provider list that starts the CLI with its own configuration goes
  through `core::process::ProbeProcess` in the session's sandbox (Claude,
  Codex), since that configuration is writable from a session and can
  name commands; pi's and agy's lists do not yet. `doctor`'s MCP
  handshake runs inside too, since what a server command runs may be a
  file in the workspace.
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
restores all of it; vendor sessions themselves live with the vendor. A
block of a kind this version does not know is skipped when read; not
handled: an older unharness that rewrites such a conversation drops it.

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

- Antigravity (`agy`, `src/harness/agy/`), recorded on agy 1.2.17 with an
  account. A stdin line is `{"event":"user","message":{"content": …}}`;
  anything without `event` ends the process with an error naming the
  field, and every other event name is ignored with a warning (so there is
  no interrupt, steer or permission answer). `init` comes once per
  process, after the first message. Steps are `user_input`,
  `agent_response`, `tool` and `system_message`; a tool's two updates
  (`ACTIVE`, `DONE`) share only their `step_index`, its `parameters` are
  cut down (a write shows its path, not its content), and neither a failed
  command nor a refused call is marked. Thinking is counted, never sent.
  `result.usage` and `num_turns` are totals over the conversation, also
  after `--conversation` in a new process (the parser sums the steps).
  Headless agy refuses what it would have asked about and ends the turn
  there with an empty response and `SUCCESS`: a write in the default
  mode, a shell command under `--mode accept-edits` (reads and edits run).
  Each refusal is one stderr line naming the permission; the result's
  `denied_actions` is the set of kinds refused so far in the process (one
  entry after three refused commands, `fixtures/denied_twice.jsonl`) and
  is empty again after `--conversation` in a new process. agy's own
  interface (`--no-tui`) asked before a write and before a command and did
  nothing until answered (pty run, 1.2.17), which is what `ask` there
  stands on; its "always allow" offers one that persists to
  `settings.json`, a guarded file. `--model`
  takes the ids `agy --output-format json models` lists
  (`command.data.models[].{id,label}`), which end in their effort, and is
  refused beside `--effort` ("conflicts"); `--effort` alone picks the
  default model's (`low`, `medium`, `high`; `xhigh` and `max` are valid
  words the default model lacks). `--mode plan` writes
  `brain/<conversation>/implementation_plan.md` under the state directory
  and acts in the same turn, so plan mode is not declared. SIGINT gives a
  `result` with `error: "interrupted"` and exit 1, and leaves the command
  agy started running; unharness's kill takes it (a `--print` run's
  `sleep` gone on Ctrl+C). Sign-in is the token
  file beside `settings.json`; whether `GEMINI_API_KEY` signs a headless
  run in is unverified, so it is not looked at.
  Headless runs did not touch `settings.json`, `~/.gemini/config/
  config.json` or `mcp_config.json` (the guarded files). Not committed:
  the plan recording, whose text carried the home path split across
  deltas. Unverified: content blocks on stdin (the binary has a
  `streamInputContentBlock`), `AGY_ERROR:` on stderr (from 1.2.15's
  strings), a model other than the default family's, sub-agents, MCP tool
  steps, what agy's own interface writes to `settings.json` when a
  workspace is trusted there (`trustedWorkspaces`).
- Codex `app-server` is marked experimental by OpenAI; `transport = "exec"`
  in `[harnesses.codex]` forces the fallback. Each `exec` turn is a
  process of its own, and what it leaves in its process group is killed
  when it exits; an `app-server` process keeps its group until the
  session ends. The difference did not show for a command's background
  job on 0.157.0 (pty runs, `sleep 123`): a `cmd &` one did not outlive
  its command on either transport, and a `setsid cmd &` one outlived
  the turn, the session and unharness on both, having left the tree
  before anything was killed. What else Codex keeps in its group
  between turns (MCP servers) is unverified.
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
  ignored): `task_started`, `task_progress`, `task_updated` (whose
  `patch.status` says when it ends) and `task_notification` (the report).
  Either ends it, and a notification after the update only adds the
  report: in 2.1.292's source the notification is sent once per task id
  (a claim released when the task starts again), the update on every
  change of status. A row once stayed at running with no recording to
  show why (#58). `background_tasks_changed` lists the background tasks
  left; it came before the update on 2.1.289 and after the notification
  on 2.1.292, which also adds a `run_id` to each. A subagent's own subagent
  (2.1.292, `fixtures/subagent_nested.jsonl`) is spawned by a call in its
  parent's messages, has `spawn_depth: 2` and `parent_task_id`, and
  outlives the parent's end, reported on its own (so a run's end does not
  end the announced runs inside it). A task
  resumed with `SendMessage` starts again under the same `task_id` with the
  new call's `tool_use_id`, while its messages keep the original call as
  `parent_tool_use_id`; the parser keys on the original. A notification
  that arrives during a turn is folded into it and still yields a `result`
  with `num_turns: 0`, and results of self-started turns can arrive in a
  batch after the last one. `stop_task` with the task id stops one (what
  `StopSubagent` sends) and Claude then reports it in a turn of its own;
  `interrupt` between turns stops all background tasks and starts no
  turn. With only its stdin closed Claude (2.1.292) does not exit while a
  background task runs (a Monitor's `tail -F` kept it for minutes), and
  each command runs in a session of its own; `interrupt` then EOF (what
  `Shutdown` sends), SIGTERM and SIGINT each stopped the Monitor's
  command and ended Claude within half a second. Checked in the TUI
  (pty, Landlock, haiku): with a Monitor's `tail -F` running, `/quit`,
  Ctrl+D, `/switch codex` and SIGTERM to unharness each left no `tail`
  (the build before this left it, in a session of its own), and a
  background `sleep` in `--print` went with the run. Ctrl+C in `--print`
  reaches Claude only as `interrupt` now (exit 130). Never seen: a `failed` task (an agent cut off by `maxTurns`
  reports `completed`), subagent `stream_event`s. Without
  `--forward-subagent-text` a blocking subagent's text was not sent (its
  tool calls were); a background one's final message arrived either way.
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
- `!command` at the prompt (`tui/shell.rs`, `Block::Shell`,
  `BlockRecord::Shell`): `$SHELL -c` in the session's `cwd` through
  `LineProcess::spawn_no_input` under `App::session_sandbox()` (stdin
  `/dev/null`, `setsid`, the process tree killed on Esc and on quit, the
  group when the shell exits). No permission prompt, no rule. Refused during a
  turn; a prompt sent while it runs is queued. Its command, output tail
  and status go once in front of the next prompt (unsent blocks, any
  harness); a sent one is part of the bridge, and a harness switch
  bookmarks before the first unsent one so the harness left behind still
  gets it. Checked live (pty, Claude Code 2.1.290, Landlock): output and
  exit status shown, Esc stops `sleep`, the next turn answered from the
  output, `\!` sent as `!`, a write to the home directory denied, a
  background job reaped on quit. Not handled in `--print`, `--no-tui` or
  an initial prompt. Unverified: macOS.
- Clipboard images (`tui/clipboard.rs`, `Ctrl+V` and `/paste`): the reader
  logic is tested against a stand-in tool only. No Wayland or X11 display
  and no Mac were available, so the real `wl-paste --list-types` /
  `--no-newline --type`, `xclip -selection clipboard -o -t TARGETS` and
  `osascript -e 'the clipboard as «class PNGf»'` calls are unverified, as
  is which terminals pass `Ctrl+V` through. Dropped files (`tui/drop.rs`)
  are read from a bracketed paste; the quoting forms come from terminal
  documentation, and only the bare and single-quoted ones were sent
  through a real pty. herdr 0.8.2 (`HERDR_ENV=1` in a pane), checked with
  an isolated session (`XDG_CONFIG_HOME` in scratch, its client in a pty
  standing in for the outer terminal): a pane has the environment of the
  server, fixed when the first client started it, not of the client
  attached now (also for panes split later from another client), so
  `WAYLAND_DISPLAY`, `DISPLAY` and `SSH_*` there can describe a session
  the user has left. A pane's OSC 52 `c` write reached the client's
  terminal (a 200 KB payload too); `p` and the `?` query did not. The
  client re-emits it through its own clipboard writer (from herdr's
  source: a tool by its own `WAYLAND_DISPLAY`/`DISPLAY`, OSC 52 over ssh),
  which is why copy in herdr is always OSC 52: with a stand-in `wl-copy`
  and a server started with `WAYLAND_DISPLAY`, unharness had used the
  tool and said "Copied" while nothing reached the client. herdr forwards
  mouse reports to a pane that asks for them, and a drag in the
  transcript copied through it; bracketed paste (two lines, not sent) and
  a dropped single-quoted path (attached) arrived intact. On the machine
  of the report the server had been started over ssh on a host with no
  display (no `wayland-*` socket, no `/tmp/.X11-unix`), so `Ctrl+V` gave
  the over-ssh notice. A `wayland-*` socket is not looked for when
  `WAYLAND_DISPLAY` is missing: in a pane that says nothing about where
  the user is. Unverified: the real clipboard at the far end
  (only the bytes the client wrote were checked), herdr's own
  `wl-copy` with a client at the desktop, `herdr --remote`, whose client
  takes `Ctrl+V` itself (`keys.remote_image_paste`), stages the image on
  the server and pastes its path (read from the source; the file is
  removed when that client detaches), and reading an image in a pane
  whose server env names a display the user is no longer at.
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
  own config; no session flag on 1.2.17) and pi (no MCP client).
- Plan mode (`Capabilities::plan_mode`) is declared, not driven: nothing in
  unharness enters it yet. What the flag stands on: Claude Code 2.1.289
  `--permission-mode plan`, Codex 0.157.0 `collaborationMode` on
  `turn/start` (app-server schema; `exec` has none), both read from
  `--help` or the schema and neither run; an ACP session
  reports it when its `modes` list one with id `plan` (claude-agent-acp
  0.85.1 does, codex-acp 2.1.1 does not). pi plans only through an
  extension, and agy's `--mode plan` does not wait (see above).
- `ask` per harness. Codex 0.157.0 app-server: `approvalPolicy:
  "untrusted"` with `sandbox: "workspace-write"` (unharness's sandbox off)
  asked before a file change and before every command, `cat` of a file
  included (two recordings and a TUI run; not committed as a fixture
  because Codex read the user's skill files into them despite the prompt).
  The Codex CLI itself rejects `untrusted`: `-a` takes only `on-request`
  and `never`, and `-c approval_policy="untrusted"` fails with "no longer
  supported", which is why `exec`, `--print` and `--no-tui` have no `ask`
  on Codex; only the app-server protocol still takes it. pi 0.87.1: the
  gate (`src/harness/pi/gate.ts`, `gate.rs`) is a `tool_call` handler that
  asks with `ctx.ui.select`, which arrives over RPC as an
  `extension_ui_request` whose title carries the call as JSON
  (`fixtures/gate.jsonl`); `-e` loads it beside the user's extensions and
  under `--no-extensions`, from the state directory, under the sandbox.
  Checked in the TUI: a command allowed always, a write denied (pi reports
  the tool as failed with "Denied by the user"), the same command then
  answered by the rule; a call from one of the user's own skills was asked
  about too. In `--print` `ctx.hasUI` is false and the gate blocks. pi
  passes a path as the model wrote it, often relative, and a rule does not
  match a relative path. Read in pi 0.87.1's source, not run: before using
  a path pi turns Unicode spaces into ASCII ones, strips a leading `@`,
  expands `~` and reads `file://` URLs, so such a path is `Opaque`
  (`gate::path_is_literal`); every extension's `tool_call` handler runs in
  turn and may change the input after the gate saw it, and the gate knows
  a read by its tool name alone. Unverified: the gate's human-readable dialog in
  pi's own interface (`--no-tui`, `ctx.mode` other than `rpc`), several
  gate dialogs open at once (pi can run a message's tool calls in
  parallel), tool calls made by a pi sub-agent or an extension that runs
  tools without `tool_call`, agy's own interface under `ask`.
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
- herdr (`tui/herdr.rs`), checked on herdr 0.8.2 (socket protocol 20)
  in a named test session with Claude Code 2.1.289 and agy 1.2.17: the
  TUI sends `pane.report_agent` (`source` and `agent` both `unharness`,
  `seq` in microseconds since the epoch) over `HERDR_SOCKET_PATH`, one
  JSON line answered by one line, on a task of its own that sends only
  the newest state; `pane.release_agent` on quit. `herdr agent get`
  showed `working` on a prompt, `blocked` at a Claude permission modal
  and at the policy picker held for agy under `ask`, `idle` after the
  turn and `done` when the turn ended with another tab focused (herdr
  derives `done`; a report is only `idle`, `working`, `blocked` or
  `unknown`), and no agent once unharness quit. herdr's own detection
  took the pane for `claude` (the child process) until the first report
  arrived, about 1.5 s after launch, and the report wins after that.
  Unverified: where herdr shows the `message` of a blocked report (no
  API view returns it), whether the release or the exit cleared the
  pane. `Sandbox::wrap` takes every `HERDR_*` variable away from what
  it starts, sandbox on or off: the socket drives every pane, and a
  vendor integration (`herdr integration install claude`) would
  otherwise report for the same pane under its own `source`. What
  follows was seen before that, with the variables inherited: pi's
  herdr extension reports
  only when `ctx.mode` is `tui`, so it is silent in unharness's `rpc`
  sessions; pi's A2A extension (`herdr-a2a`) fails with "client session
  readiness timed out" because its broker answers `POST /v1/register`
  with 403 `verification_failed` ("Herdr could not verify this pane")
  until its 5 s deadline. The same 403 came back for `herdr-a2a
  client-session` run unsandboxed from a pane herdr knows as `claude`,
  so it is not the sandbox; that the broker wants herdr to know the pane
  as the `pi` of that session is inferred from the binary's strings, not
  read in its source.
- `unharness update` (`src/update.rs`) uses axoupdater 0.10 on the
  receipt dist 0.32.0's shell installer writes to
  `$XDG_CONFIG_HOME/unharness/unharness-receipt.json` (or
  `~/.config/unharness/`); it writes one also with `install-updater =
  false` unless `UNHARNESS_DISABLE_UPDATE=1`. Its `install_prefix` is the
  root above `bin` (`install_layout: "cargo-home"`), which is what
  axoupdater compares the running binary against, so a receipt left by
  another copy is not used. Checked on Linux with a scratch prefix and
  `XDG_CONFIG_HOME`: 0.2.9 → 0.3.0 and a 0.3.0 → 0.2.0 downgrade through
  the release's installer, receipt rewritten, no rc file touched. A
  binary under a `Cellar` directory is Homebrew's; one listed in
  `.crates2.json` beside its `bin` is cargo's, unless the receipt is
  newer. Both are told what to run. It runs before the config is read
  and never on its own. Unverified: macOS, a real Homebrew install.
- Questions (`PermissionKind::Question`, `tui/modal.rs` `QuestionModal`):
  Claude Code 2.1.291 AskUserQuestion options carry `label`,
  `description` and an optional `preview` (markdown), with and without a
  preview in the same question (`fixtures/ask_previews.jsonl`). In
  `-p` stream-json mode the tool is only offered with
  `--permission-prompt-tool stdio`; without it the model finds no such
  tool (`write_denied.jsonl` on 2.1.288, and a run on 2.1.291), which is
  why the recorder passes it. A
  multi-select answer sent as a JSON array was read back by the model as
  the labels joined with commas (pty run, 2.1.291). Codex 0.157.0's
  `ToolRequestUserInputOption` (app-server schema) has only `label` and
  `description`.
  The modal sends only from its Submit page, and only once every question
  has an answer; a lone single-select question is sent on `Enter`.
- Headless `--print` (`src/headless.rs`, `docs/headless.md`) runs one
  prompt through `start_session` and writes text, a result object or
  `stream-json` lines (schema 1, `event_json`); `--native` keeps the vendor
  print command. Checked live on Claude Code 2.1.292 (text, json,
  stream-json with a `Write` denied under `ask`, a prompt on stdin, SIGINT
  giving exit 130: Claude ends that turn with `error_during_execution`)
  and agy 1.2.17 (`accept-edits`). Codex, pi and ACP agents run only
  against `fake-harness.py` (`tests/session_e2e.rs`). Ctrl+C reaches the
  CLI only as `Interrupt`: one before Codex app-server has a thread
  drops the turn, one before its turn has an id is sent once it has one
  (checked live on 0.157.0, the turn ended interrupted at 0.4 s and at
  0.9 s), and an ACP turn waiting for its session is dropped. Ctrl+Z
  stops every harness process tree, then unharness, and continues them
  on `fg` (`process::suspend`; checked live on Claude Code 2.1.292 with
  a Bash `sleep` running: unharness, Claude, its shell and the `sleep`
  all stopped, and the turn ended normally after `fg`). It waits for
  subagents and, where `SubagentSupport::report_turn` is declared, counts
  one more turn end per background run of a subagent that completed or
  failed, wherever its end falls: in every single-prompt Claude recording
  each such end is followed by one `result` (empty ones with `num_turns:
  0` included), a blocking one's report is its call's result instead,
  and one the model stopped (`TaskStop`) brought no turn. A subagent's
  subagent counts only when it ends after its parent (the only case
  recorded). A subagent whose end never comes holds the run until
  `Ctrl+C`. Not persisted as a conversation.
- Hooks (`HookStarted`/`HookEnded`, `Block::Hook`): Claude Code 2.1.292
  sends `system` `hook_started` and `hook_response` for every hook under
  `--include-hook-events` (`SessionStart` also without it), keyed by
  `hook_id`, with `hook_name` (`PreToolUse:Bash`), `hook_event`, `stdout`,
  `stderr`, `exit_code` and `outcome` (`fixtures/hooks.jsonl`). `outcome`
  is `error` for any exit status but 0, also for exit 2, which blocked a
  `PreToolUse`; a hook that blocks by printing a decision
  (`permissionDecision: "deny"`, `decision: "block"` on `Stop`) is
  `success` with exit 0 (`fixtures/hooks_json.jsonl`), so the parser reads
  the printed JSON. Which events can be blocked is from Claude's
  documentation; `PreToolUse`, `Stop` and `PermissionRequest` were
  recorded blocking. A `PermissionRequest` hook runs beside the
  `can_use_tool` request sent to unharness, and the first answer wins:
  when the hook's `decision.behavior: "deny"` came first, Claude sent
  `control_cancel_request` for the request and the call failed with the
  hook's `message`; when unharness had allowed it first, the call ran and
  the hook's deny came after (`fixtures/hooks_permission_{denied,late}`,
  the first recorded with `--answer-delay`). The hook's lines name only
  the tool (`PermissionRequest:Write`), but in all three recordings its
  `hook_started` came right after the `can_use_tool` request it belongs
  to (in the late one after unharness had answered), so the parser ties
  a hook when it starts to the newest request for that tool without a
  hook, decided ones included, and settles its deny by that request: one
  already decided says how (a `control_cancel_request`: blocked; its
  call's `tool_result`: too late), an open one holds the deny until one
  of them comes. A deny whose hook was not seen to start is shown as
  not known. A request is dropped once decided and its hook has
  answered; at most 64 are kept, and a deny held for one dropped then is
  shown as not known. Nothing is dropped at a `result`, since a
  background subagent's request can be open across it. Two `Write` calls
  in one message were asked about one after the other, each request
  after the previous call's result
  (`fixtures/hooks_permission_parallel`), so two requests open at once,
  a deny for a request already decided and a cancel before the hook's
  answer are only in unit tests. Unverified: two requests for one tool
  sent before either hook starts (the hooks would be tied the wrong way
  round), several `PermissionRequest` hooks on one request (the second
  would take an older request for the tool that had no hook, or none,
  and then its deny would show as not known), and whether an interrupt cancels an open request (a held deny
  would then show as blocked).
  Codex 0.157.0 `app-server` sends `hook/started` and `hook/completed`
  with a `run` (`id`, `eventName` such as `preToolUse`, `status`:
  `completed`, `blocked`, `failed`, `stopped`, and `entries` of `{kind,
  text}`), `fixtures/app_server_hooks.jsonl`; a run's id is the same for
  the `Stop` hook of every turn, and a hook's plain stdout is not in it.
  Codex runs only hooks it trusts: `hooks/list` gives each a `key` and
  `currentHash`, and `hooks.state.<key>.trusted_hash` set to that hash
  (also through `-c` as one inline table, since keys hold dots) trusts it.
  Its `Stop` hook must print JSON or it fails. Unverified: hook events
  from `codex exec`, a sub-agent's hooks, `stopped`.
- Commands and subagents under `.agents/` (`sync/projections.rs`,
  `docs/skills.md`), checked live: Claude Code 2.1.292 listed a linked
  `.claude/commands/<name>.md` in `init`'s `slash_commands` and a linked
  `.claude/agents/<name>.md` in `agents`, and expanded the command in `-p`
  (also through `unharness --print`, sandboxed): `$ARGUMENTS` is all of
  it, `$0` the first word. pi 0.87.1 expanded a linked `.pi/prompts/` one
  only under `--approve` (`$1` the first word, `$0` empty); unharness
  does not pass it, since that would let the workspace decide. Codex
  0.157.0 (app-server) spawned a role from a generated
  `.codex/agents/<name>.toml` in a project it called untrusted (it said
  project config was off there), and without the file the same prompt
  got an answer without the role's instructions. Codex has no command
  files (custom prompts are gone; skills are its slash commands), agy's
  `.agents/workflows` are deprecated in favour of skills, and its
  `.agents/agents/<name>/agent.md` came back empty from `agy agent`
  (unverified why), so neither is projected to. Hooks are not projected:
  Claude's live in `settings.json`, Codex's in `.codex/hooks.json` behind
  per-hook trust, agy's in `.agents/hooks.json` in a shape of its own, and
  the payloads differ. Unverified: Claude's subdirectory namespacing
  (`a/b.md` is `/a:b`; only direct children are projected). Not handled:
  a directory on the way swapped for a link between the check and the
  write (`O_NOFOLLOW` covers only the last component); a source under
  `.agents/` that links to a file outside the workspace, which sync reads
  unsandboxed (for a Codex role) and links to; links projected before a
  later refusal of their directory, which stay; links projected before
  their directory moved behind a link (`.claude` into `cfg/claude`, then
  `.claude -> cfg/claude`), whose text is then a `..` short: they are
  neither replaced nor removed, and are warned about on every sync as
  linking elsewhere.
- A harness's own slash commands (`Capabilities::slash_commands`,
  `CapsUpdate::commands`, `App::pass_command`): a `/command` unharness
  does not have is sent as typed where the harness runs commands from a
  prompt, `\/` sends one unharness also has, and the `/` list offers the
  ones the session reported. Claude Code 2.1.292 ran a project command and
  `/context` from a stream-json prompt and passed an unknown `/name` (and
  `/etc/hosts …`) to the model as text; its list is the `commands` of its
  answer to `initialize` (`name`, `description`, `argumentHint`,
  `aliases`: `/new` and `/reset` are its `/clear`), read once per process,
  since `init`'s `slash_commands` names some by an alias
  (`anthropic-skills:pdf` for `pdf`) and nothing else. An ACP agent is
  taken to run commands once its `available_commands_update` lists some:
  claude-agent-acp 0.85.1 sent it three times, the first empty, and ran
  `/hello world` (`fixtures/claude_agent_acp_commands.jsonl`); codex-acp
  2.1.1 listed none. pi 0.87.1 answers `get_commands` (sent after
  `get_state`; spliced into the pi fixtures the e2e tests replay) with
  extension commands, prompt templates and `skill:` ones, without their
  `argument-hint`, and `prompt` ran a template and a skill
  (`fixtures/commands.jsonl`); its interface's own commands do not run
  over RPC (its docs). Codex passes a prompt's text on, and agy is
  unverified: neither is declared. A command has to start the message, so
  it goes alone and the bridge and unsent `!` output wait for the next
  prompt, whose bridge then also tells the command's turn (a switch keeps
  the held start). A queued `/` prompt goes only to the harness it was
  typed for. One of the agent's that does what one of unharness's does
  (`/clear`, `/model`, `/compact` and their aliases) is warned about, not
  followed: unharness's model, transcript and bridge are not updated.
  Checked in the TUI (pty): Claude ran `/hello world` and `\/context`, and
  pi's list came from `get_commands`. Unverified: codex-acp's commands
  (its recordings listed none, so it is not taken to run any), an unknown
  `/name` on an ACP agent, a Claude command added during a session (not
  listed until the next process).
- Claude's models (2.1.292) are the `models` of its answer to
  `initialize` (`value`, `displayName`, `description`,
  `supportedEffortLevels`, the first being `default`, which `--model`
  takes); there is no command that prints them. A session reports them
  as `CapabilitiesChanged`; `list_models` starts Claude with
  `--safe-mode --setting-sources user` in an empty directory, so that no
  hook, plugin, MCP server or workspace setting runs (`--safe-mode` keeps
  the user settings' `env`), and falls back to a table when no answer
  comes within 15 s (Anthropic only). `--safe-mode` still runs the
  settings' `apiKeyHelper` (checked), which a sandboxed session can write
  into `~/.claude/settings.json`, so the probe runs in the session's
  sandbox; a probe does not seed the relocated config (`prepare` does,
  with its notice).
- Claude providers (2.1.292, read in the binary and run): Claude picks its
  API from `CLAUDE_CODE_USE_BEDROCK`, `_FOUNDRY`, `_ANTHROPIC_AWS`,
  `_ANTHROPIC_GOOGLE_CLOUD`, `_MANTLE`, `_VERTEX` (`1`, `true`, `yes`,
  `on` in any case), and with two of them defined, whatever their values
  and whether in the environment or its settings' `env`, it calls
  Anthropic's; so a chosen provider sets its switch and removes the rest.
  unharness offers the documented three. The answer to `initialize`
  names the provider in effect (`account.apiProvider`: `firstParty`,
  `bedrock`, `vertex`, `foundry`), which the driver checks against the
  choice; a switch in the settings' `env` cannot be overridden. Without
  credentials Vertex and Foundry listed their models at once, Bedrock
  after 60 s, and a Vertex turn failed after two `api_retry` with
  `cloud_credential_error` (`fixtures/provider_vertex.jsonl`).
  Unverified: a turn on any of them with credentials, `_MANTLE` beside
  `_BEDROCK` (the binary treats Mantle as a Bedrock variant),
  `CLAUDE_CODE_USE_GATEWAY` (left alone).
- Codex providers (0.157.0): the built-in `openai`, `ollama`, `lmstudio`
  and `amazon-bedrock` cannot be redefined ("reserved built-in provider
  IDs"; Bedrock takes only a few fields), an unknown `model_provider`
  stops Codex at startup, and `config/read` lists the configured
  `model_providers` (with `name`) and the `model_provider` in effect.
  `-c model_provider="<id>"` chooses one on app-server and exec, and
  `modelProvider` goes on `thread/resume` and `thread/fork` (schema; a
  thread keeps its own provider otherwise);
  `thread/start` and `thread/resume` answer with `modelProvider`, which
  the driver reports and checks against the choice. `model/list` gave
  OpenAI's catalog under every provider tried, so only `openai` lists
  models; with no model chosen Codex took `gpt-6-astra` on `ollama` too.
  Checked live: a turn through a custom `[model_providers]` entry
  (`wire_api = "responses"`, a llama.cpp server) on app-server and exec.
  Codex retried an unreachable Ollama without end. Unverified: what a
  resumed thread does with a `modelProvider` other than its own,
  `amazon-bedrock`, `lmstudio`, a provider defined only in a project's
  `.codex/config.toml` (`config/read` is asked without a `cwd`).
- Bridging (`tui/bridge.rs`): a switch whose bridge is over budget first
  asks the harness being left for a handoff summary (`App::ask_handoff`,
  `Block::Handoff`), with every permission request denied while it
  writes; not while its subagents are at work (their requests and reports
  would land in that turn). Checked live (pty, Claude Code 2.1.292 → Codex 0.157.0
  app-server, `bridge_max_chars = 600`): Claude wrote the summary within
  the word limit it was given, the switch followed the turn, and Codex
  answered from the first prompt and the summary it was sent. Windows
  reported then: Claude's default model 1,000,000, Codex's 258,400, so an
  unset budget is about a quarter of those in characters. The windows are
  kept in `<state dir>/unharness/context_windows.json`, not in the
  workspace (a window decides how much a harness is sent), and taken as
  between 8,000 and 2,000,000 tokens. Unverified: a
  summary turn on pi, agy or an ACP agent, and one in which the agent
  asks for a tool anyway.
- Claude's `total_cost_usd` is a running total per process, and an ACP
  `usage_update.cost` is a running total per session; both parsers report the
  per-turn difference.

- Sandbox (`src/core/sandbox/`), checked on Linux 7.2 (Landlock ABI 10) with
  Claude Code 2.1.289, Codex 0.157.0 (app-server and exec), pi 0.87.1 and
  agy 1.2.17 (`~/.gemini/antigravity-cli` writable; the one denial traced
  was the updater's write test in `~/.local/bin`): a
  workspace write succeeds, a write to the home directory and a read of
  a denied credential directory fail, sessions resume. Landlock only allows, so reads are granted
  on everything around the denied paths (`linux::read_grants`); directory
  listing reaches into them, file contents do not. Codex's bubblewrap fails
  inside a Landlock domain ("setting up uid map: Permission denied"), hence
  `externalSandbox` on `turn/start`, `danger-full-access` for exec, and
  `-c sandbox_mode="danger-full-access"` on the app-server command line
  (without it the server warns at startup). Where ours does not run
  (`--sandbox off`, or no backend) Codex's own sandbox holds to the level
  the user wanted (`codex::OwnSandbox`, `Sandbox::wanted`), under every
  policy: `bypass` is `--dangerously-bypass-approvals-and-sandbox` only
  at `off`, otherwise `-s <level>` with `approval_policy="never"`.
  Checked with `--print` on Codex 0.157.0 under a seccomp filter denying
  the Landlock syscalls (`systemd-run -p SystemCallFilter=~landlock_*`):
  with the default level and `bypass`, a write to the home directory was
  refused ("read-only file system") and one in the workspace succeeded.
  `auto` on `exec` is `--approve-for-me` only at workspace-write (the CLI
  refuses it beside `-s`); at another level it runs as `-s <level>` with
  Codex's default approvals, i.e. as `accept-edits`. Claude Code writes
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
  tested), a user-enabled Claude Code sandbox inside ours, agy's own
  `--sandbox` flag (off unless given), the ACP presets' state directories, `--no-tui` passthrough under the sandbox.

## Commit style

Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`), one logical
change per commit, tree green (fmt, clippy, tests) at every commit.
