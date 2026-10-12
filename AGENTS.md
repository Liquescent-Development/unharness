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
  (`guarded`) and compared after every turn and when a session ends (`core/guard.rs`). What a CLI
  rewrites by itself is projected away there (Claude's counters in
  `.claude.json`, Codex's trust entry for the workspace), found by running
  it, so that a warning means something. A tree is compared by the mode,
  size and change time of everything in it, so that a change undone before
  the check is still one, except the files a harness declares as its CLI's
  bookkeeping (`Guarded::TreeWithBookkeeping`), compared by content up to
  1 MiB: Claude's sync of organisation skills rewrites
  `skills/synced/<org>/manifest.json` and `.last-complete-round`, and only
  those, with the same bytes on a timer (2.1.296, #114). Watched files
  are opened without blocking (a tree's without following a link), so a
  FIFO put in a file's place cannot hang the check.
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
  fork, a resume, `/clear`, a sandbox or provider change). Mid-turn a CLI that is sent nothing to stop the turn (all
  but Claude) is killed at once (`end_or_kill`): given its grace it could
  go on with the turn where nobody sees it. A turn still running
  `STOP_GRACE` (5 s) after an interrupt is ended with its process when the
  user interrupts again, once told that would (`App::end_stuck_turn`);
  what it changed in the harness's configuration is checked first, its
  requests and owed rewind anchors are dropped, and the next prompt
  resumes the session. A session shut down takes its open requests with
  it. Quitting waits for every CLI
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
  resume, fork, rewind or `/clear` (each would take the held start for
  its own),
  and an interrupt drops the prompt that waits; a rewind still held at
  quit forgets the vendor session it was for. On Linux each CLI runs
  below a reaper of its own (`core::reaper`: unharness's binary as
  `__unharness-reap`, a subreaper, in the sandbox): what the CLI put in a
  session of its own or forked away twice is adopted by it, not by init,
  and is killed when the CLI exits, also by itself. Checked live: a
  `setsid sleep &` from Claude Code 2.1.292, Codex 0.157.0, pi 0.87.1 and
  agy 1.2.17 in `--print` was gone with the run (0.5.0 left Codex's), and
  the TUI left none on `/quit` or a closed terminal. Elsewhere (macOS)
  such a process outlives the CLI; its pipes are closed on it once the
  drain is up.
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
harness → vendor session map, bridging bookmarks, per-harness usage, the
active harness, and the policy, models and efforts chosen for it (never a
configured default; one named in the run resuming it wins), one JSON
file per conversation plus an index. `App::persist`
runs on session start, turn end, harness switch, `/clear` (which then
starts a new conversation with the same harness and run choices and no
sessions, the way a launch does), and quit; a
session nothing has been sent to yet is not saved, nor its id. Resume
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
   Declare `min_version` when an older release is known to lack a flag or
   API unharness uses; `doctor` warns below it.
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
  process, after the first message on 1.2.17 and as the process starts
  on 1.3.2, which then has written a conversation (its database and
  brain directory) with nothing sent. Steps are `user_input`,
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
  strings), a model other than the default family's, MCP tool
  steps, what agy's own interface writes to `settings.json` when a
  workspace is trusted there (`trustedWorkspaces`).
  Subagents (agy 1.3.1, `fixtures/subagent_*.jsonl`, `brain.rs`): an
  `invoke_subagent` call opens as a `tool` or a `subagent` step and closes
  as `subagent`, listing each subagent (`conversation_id`, `role`,
  `type_name`, `initial_prompt`, `log_uri`); one that opened as `tool`
  had stayed running in the TUI (#96). A subagent reports with
  `send_message`, which agy files as `brain/<parent>/.system_generated/
  messages/<id>.json` (`sender`, `recipient`, `timestamp`, `content`)
  and delivers as a `system_message` step that carries none of them, so
  the transport reads the files when the step comes and at `result`. The
  brain directory is the one `log_uri` names
  (`file://<brain>/<id>/.system_generated/logs/transcript.jsonl`), else
  `~/.gemini/antigravity-cli/brain`. It is writable from the sandbox: a
  message is shown only from a subagent started in the process, no
  directory on the way may be a link, and a read is bounded (4096
  entries, 16 MiB, 1 MiB a file). The timestamps are RFC 3339 with
  nanoseconds whose trailing zeros are left out. The turn waited for
  every subagent in four recordings and on the Mac of #96 (also after
  the agent said it had ended its turn), so at `result` one that has not
  reported has ended: its `logs/transcript.jsonl` ends in an `ERROR`
  step when a call of its was refused (default mode, `ls`; it stopped
  without a report, and the parent's `denied_actions` named the command)
  or in its final response. With a `result` that is an interrupt or an
  error, one left is cancelled or failed. Nothing came on stdout for
  240 s after a result. There is nothing on stdin to stop one. Checked
  live on 1.3.1 with `--print` and in the TUI (pty, Landlock): both rows
  ran and ended with their reports. Unverified: a subagent put back to
  work by a later `send_message` from the parent (its second report is
  read at the next delivery or result and shown as a second end),
  `manage_subagents` actions other than `list`, a subagent's own
  subagents, what agy does with its subagents on an interrupt, macOS
  (where the brain directory is taken from `log_uri`).
- Codex `app-server` is marked experimental by OpenAI; `transport = "exec"`
  in `[harnesses.codex]` forces the fallback. Each `exec` turn is a
  process of its own, and what it leaves behind is killed when it exits;
  an `app-server` process keeps what it started until the session ends.
  For a command's background job on 0.157.0 (pty runs, `sleep 123`) the
  difference did not show with a `cmd &` one, which did not outlive its
  command on either transport; a `setsid cmd &` one, which outlived
  everything before the reaper, now ends with the turn on `exec` and
  with the session on `app-server`. What else Codex keeps between turns
  (MCP servers) is unverified.
- ACP (`src/harness/acp/`) is verified against
  `@agentclientprotocol/claude-agent-acp` 0.85.1 and
  `@agentclientprotocol/codex-acp` 2.1.1. The presets in `PRESETS` (gemini,
  opencode, goose, copilot, cursor, qwen, kiro) use launch commands from the
  ACP registry and were not installed when they were added: unverified until
  recorded. unharness declares no `fs`/`terminal` client capabilities
  (form elicitation only, #157), and uses only stable v1 methods;
  `session/load` (history replay) has been
  exercised with vibe-acp 2.26.1 (`fixtures/vibe_acp_mode_load.jsonl`),
  `session/resume` with claude-agent-acp.
  `scripts/record-acp.py` (#169) also sends client capabilities,
  `session/load`, a mode (`set_mode` or the mode option), any config
  option, `session/fork` (after the prompts, or of a stored session in a
  new process as the driver forks, `--fork-from`), `session/close`, a
  timed `session/cancel` and extension requests, and answers permission requests in order and
  `elicitation/create`; `--help` lists the flags. Without them it sends
  what it sent before: the seven committed ACP fixtures replayed through
  it with `fake-harness.py` gave the same `>>` lines, and claude-agent-acp
  0.85.1 and codex-acp 2.1.1 re-recorded `claude_agent_acp` and
  `codex_acp` with the same requests. Each flag was run once against
  vibe-acp 2.26.1 (a scratch `VIBE_HOME`; nothing committed): an
  extension method has its leading `_` on the wire (`_rewind/preview`;
  vibe's router strips it), and vibe echoes each user message, live and
  when `session/load` replays it, with the `messageId` that `_rewind/*`
  takes and `--ext` can name (`$USER_MESSAGE`). The policy reads a request's
  tool kind from the tool call as the parser has merged it
  (`transport::screen_permission`): vibe-acp 2.26.1 names the call in
  `session/request_permission` by `toolCallId` alone, its kind, input and
  locations having come on the `tool_call` before it
  (`fixtures/vibe_acp_ask.jsonl`, vibe started in its own `ask` mode with
  `VIBE_DEFAULT_AGENT=ask`); a kind in the request replaces the earlier one,
  as an update's does, and a request without an id has only its own. Checked
  live (`--print --policy accept-edits`, `command = ["env",
  "VIBE_DEFAULT_AGENT=ask", "vibe-acp"]`, Landlock): vibe asked about a
  write and unharness allowed it with no request shown, and `touch y` was
  asked about and denied. vibe's edit diff names the file as the model wrote
  it (`a.txt`) beside an absolute location, so its action is `Opaque` and no
  edit rule matches it.
  Under `accept-edits` (#204) the transport answers an edit request
  only when its action (the parser's, from the merged call) is an `Edit`
  whose every path is inside the session's workspace
  (`SessionConfig::workspace`: the root `find_workspace_root` found at
  launch, else `cwd`, which is what allow rules and the sandbox take, so
  a session started in a subdirectory covers the whole root), compared
  as rules compare paths (`rules::edits_within`: links followed; a
  relative path, a link to nothing yet or `..` past a missing directory
  is unknown) and never in unharness's own config or state directory.
  Anything else is asked about, `Opaque` included; before, every request
  of kind `edit` was answered wherever it wrote, also where vibe's own
  `ask` mode had asked because the file was outside. vibe-acp 2.26.1 in
  its `ask` mode asked about a write in the workspace and one to
  `/tmp/l204/outside/x.txt` alike, by id alone, both with absolute diff
  paths and locations, the second with options scoped
  `outside_directory` (`fixtures/vibe_acp_accept_edits_outside.jsonl`,
  e2e with the workspace as the recording's root and without it). Checked
  in the TUI (tmux, Landlock, scratch `VIBE_HOME` and XDG directories,
  `--harness vibe --policy accept-edits`, no root above the workspace):
  a write to a workspace file ran with no modal, and a write to
  `/tmp/l204/outside/y.txt` (writable in the sandbox) came as a modal and
  was not made. Unverified: claude-agent-acp and codex-acp edits under
  the policy (a claude-agent-acp `Write` outside the workspace is now
  asked about, by the code), a link out of the workspace in a live run
  (unit tests only), macOS.
  Config options (`SessionOptions::from_config_options`): the schema lets
  a select option's values come in groups (`options: [{group, name,
  options: [...]}]`, read in the schema of `@agentclientprotocol/sdk`
  1.7.0 and of the Python `agent-client-protocol` 0.11.0, schema 1.16.0),
  which are flattened in order for the model and effort lists. No agent
  was seen sending groups (claude-agent-acp 0.85.1, codex-acp 2.1.1 and
  vibe-acp 2.26.1 send flat lists), so that is only unit-tested. Effort is
  the option in category `thought_level`, or the one whose id a harness
  declares (`AcpHarness::declaring_effort_option`), which wins. No id is
  assumed for an agent and none can be declared from config: Mistral Vibe
  2.26.1 offers its level as option `thinking` in category `thinking`
  (`fixtures/vibe_acp.jsonl`; `off` and `high` for its default model), a
  category the spec reserves without naming, and setting it over ACP
  rewrites `~/.vibe/config.toml` (#164). So a vibe config agent has no
  effort and is sent none, also with `default_effort` set (e2e test).
  Nothing declares an id yet.
  Session modes (#155, `modes.rs`): vibe-acp 2.26.1 starts every session
  in `accept-edits`, which wrote a file under `ask` with no request, and a
  session loaded in a new process comes back in the mode it was left in
  (`auto-approve` in `fixtures/vibe_acp_mode_load.jsonl`). So a harness
  holds its agent to a mode per policy (`ModeMap`: declared with
  `AcpHarness::with_modes`, or `[harnesses.<name>] modes`; a policy without
  an entry takes the nearest less permissive one's). After every session
  answer and policy change the driver sets the mode and holds the prompt
  until it is read back: through the option of category `mode` when the
  session offers it (vibe's `session/set_config_option` answers with the
  whole option list, whose `currentValue` is the read-back, and left
  `config.toml` alone, sha256 checked), else `session/set_mode` of a listed
  mode, taken at its word, since vibe answers `{}` also for one it does not
  list (`plan`) and changes nothing. Neither request was followed by a
  `current_mode_update` or `config_option_update` from vibe in three runs.
  A mode not offered, an error, a read-back that differs, or a reattach
  whose answer names no modes refuses the policy for the session
  (`CapsUpdate::refused_policies`): the waiting prompt fails unsent, a
  running one is cancelled, the TUI gives way as to an unsupported policy
  (the picker, or the nearest less permissive one sent as `SetPolicy`), and
  `--print` exits 1. An agent with modes and no map has every policy below
  `bypass` refused the same way (decided on #155), naming the missing
  map, since the mode it is in may run what the policy asks about: a vibe
  session loaded in `auto-approve` would run commands unasked under
  `accept-edits`. Only `bypass` leaves the mode alone. A
  mode the agent reports going into by itself (`current_mode_update`, the
  mode option of `config_option_update`) is set back, and a running turn
  cancelled first when that mode is not one the map gives a policy at or
  below the current one. `scripts/record-acp.py --mode` now waits for the
  mode's answer before the model, options and prompts, as the driver does.
  Checked live (scratch `VIBE_HOME`, Landlock, `modes = { ask = "ask",
  bypass = "auto-approve" }`): `--print --policy ask` had vibe's write
  asked about and denied, and the file was not created; without the map
  `--print` exited 1 naming it, and the TUI opened the picker with `ask`
  not offered. Unverified: an agent that sends `current_mode_update` (the
  e2e test's line is the spec's), `session/set_mode` with a read-back,
  an agent that resumes with
  `{}` but has modes and no map (taken to have none, so nothing is
  refused), and claude-agent-acp
  under a map (its recordings start in `default`, so nothing was sent).
  Withdrawn requests (#158, `Driver::withdraw`): claude-agent-acp 0.85.1
  sends `$/cancel_request` (`{"requestId": 0}`) for a
  `session/request_permission` the client has not answered once the call is
  decided elsewhere: a `PermissionRequest` hook in the workspace's
  `.claude/settings.json` denied the Write first, and the call failed with
  the hook's message (`fixtures/claude_agent_acp_withdrawn.jsonl`,
  `record-acp.py --hold`). The driver drops the request, sends
  `PermissionWithdrawn`, answers it with the error -32800 the protocol's
  cancellation RFD names, and drops an answer the user gave meanwhile; an id
  that waits for nothing (unknown, answered by the user or by the policy) is
  ignored. Checked in the TUI (tmux, haiku, `modes = { ask = "default" }`):
  the modal opened, closed with "stopped waiting for your answer", and the
  file was not written. With `session/cancel` followed at once by the
  request answered `cancelled` (what the driver sent on an interrupt
  before #183, and what `--hold --cancel-after` still sends without
  `--answer-first`), claude-agent-acp sent no `$/cancel_request`, and
  vibe-acp 2.26.1 in its `ask` mode sent none either (its Python `acp`
  package has no sender for one); that order lost vibe's turn (see
  Interrupts). Unverified: codex-acp, a string request id. An elicitation
  is withdrawn through the same map (#157; a synthetic e2e test, no agent
  was seen to withdraw one).
  Interrupts (#183, `Driver::cancel_turn`): every open permission request
  is answered `cancelled` (an elicitation `cancel`) and the TUI told it is
  withdrawn, and `session/cancel` goes once the tool call each was for
  (as the parser tied it) is reported `completed` or `failed`, or after
  `transport::SETTLE_WAIT` (2 s); a request that comes before the prompt
  answers is answered the same way at once and never shown. The other
  order lost vibe-acp 2.26.1's turn (ask mode, the request held,
  `record-acp.py --hold --cancel-after`): `session/cancel` interrupts it,
  which resolves the request's callback in its session journal
  (`_close_open_callbacks` in `mistralai_vibe_local_harness/vibe/
  _session.py`), the `cancelled` answer resolves it again through
  `callback/result` (`respond_to_callback`, which looks for a closed
  callback before an `await` that the interrupt's own `await`s
  straddle), the journal's next read fails ("Invalid Unified session
  store: callback result has no pending registration", `_storage.py`),
  and the prompt was never answered (40 s). Answered `cancelled` just
  before the cancel, it raced the same way and failed too. Answered
  first, with the cancel sent once the call was reported failed
  (milliseconds later), the prompt answered `stopReason: cancelled`, no
  file was written, and the session, loaded in a new process, answered
  the next prompt from its history (`fixtures/vibe_acp_interrupt.jsonl`,
  `record-acp.py --answer-first`). claude-agent-acp 0.85.1 (its call
  failed with "Tool use aborted") and codex-acp 2.1.1 in `read-only` (its
  command failed) ended the same way with `cancelled` (recorded, not
  committed), so no agent declares an order. The spec asks the client to
  answer pending requests `cancelled` when it cancels a turn (the
  schema's `RequestPermissionOutcome` `cancelled`), not in which order. In
  the TUI a permission modal takes every key: Esc there denies
  (`reject_once`; vibe then tried another tool and asked again, checked
  live) and Ctrl+C does nothing, so an interrupt with a request open
  comes only from a policy the agent stops holding, a mode it went into
  by itself, or a request in flight when Esc is pressed. The last was
  checked live (tmux, Landlock, scratch `VIBE_HOME`, `-H vibe` under
  `ask`, the request held 3 s by a wrapper): it came after the
  `session/cancel` and was answered `cancelled` without a modal, vibe
  answered the prompt with -32603 "Callback is closed: approval-…" (shown
  as the turn's error), no file was written, and the next prompt was
  answered in the same session. Unverified: an elicitation open at an
  interrupt on a real agent, an agent that never reports the call ended
  (the cancel then waits 2 s).
  Ending a session (#161): a shutdown between turns sends `session/close`
  where the agent lists `sessionCapabilities.close` (claude-agent-acp
  0.85.1, codex-acp 2.1.1 and vibe-acp 2.26.1 all do) and closes stdin once
  it is answered or after `transport::CLOSE_WAIT` (1 s), whichever comes
  first; `END_GRACE` starts then. With stdin closed right behind it,
  vibe-acp exited without answering; given the time it answered `{}`
  (`fixtures/vibe_acp_close.jsonl`). Mid-turn nothing is sent and the
  agent is killed, as before. Checked in the TUI (tmux, Landlock, scratch
  `VIBE_HOME`, `modes = { ask = "ask", bypass = "auto-approve" }`, the
  wire teed to files): after one turn `/clear` sent `session/close`, vibe
  answered `{}`, stdin closed after it, and the next session started;
  `/quit` did the same for that (unprompted) session and left no
  `vibe-acp`. A switch, resume, fork, sandbox or provider change ends the
  session through the same `Shutdown`, so it is not run separately.
  `session_info_update` (`title`, `updatedAt`) is recognised and not used:
  vibe-acp 2.26.1 sends it as each turn starts with its first prompt as
  the title, codex-acp 2.1.1 the prompt and then a title it wrote (and the
  update with no title, its thread status in `_meta`), claude-agent-acp
  0.85.1 one it wrote. A conversation is saved only once a prompt went,
  and is titled with that prompt, so it always has a title before an
  agent's comes; an agent's would also name only its part of a
  conversation that moves between harnesses, and Claude Code's and
  Codex's own titles are not taken either. Unverified: an agent that
  answers `session/close` with an error (ignored) or serves more than
  one session per process.
  Questions (#157, `elicitation.rs`): `initialize` declares
  `elicitation: {form: {}}` (not `url`); without it vibe-acp 2.26.1 does
  not offer `ask_user_question` and claude-agent-acp 0.85.1 turns
  AskUserQuestion off (both read in their source). vibe's
  `exit_plan_mode`, which the capability also turns on, stays disabled in
  every one of its modes but `plan` (its agent profiles), which its runtime
  hides (#166). With the capability declared, vibe sends a
  `tool_call` (kind `other`, `rawInput.questions[]` with `question`,
  `header`, `options[{label, description}]`, `multiSelect`, `hideOther`)
  and then `elicitation/create` (`mode: "form"`, `message: "User input
  required"`, one property `q<n>` per question: a string with `oneOf`
  `[{const, title}]`, or an array whose `items` have `anyOf`; all
  required, option descriptions only in the call), whose `toolCallId` is
  an id no tool call has (`chatcmpl-tool-…` for the call's `effect-…`), so
  the parser ties it to the newest open call whose `rawInput` asks the same
  questions (`Form::asked_by`). vibe takes a value outside the options
  (its source says so, the tool result marks it `isOther`), so a choice
  takes a typed answer when the call says `hideOther: false`; the model
  set it to `true` in one live run. claude-agent-acp names its pending
  AskUserQuestion call (its `rawInput` arrives after the elicitation, which
  announces the call), puts the question in `message` and option
  descriptions in the schema, requires nothing, and adds a string property
  `question_<n>_custom` ("Other") beside each question: it becomes that
  choice's typed answer and the text goes there (`fixtures/
  claude_agent_acp_question.jsonl`; recorded, a typed answer beside picks
  reached the model as `Red, Blue, Teal`). The answer is `{"action":
  "accept", "content": {...}}` with the schema's `const` values; the
  modal's Esc and `--print` (which answers every question with
  `Answer(Null)`, as it dismisses Claude's) send `decline`: vibe then
  reports the question cancelled to the model, and claude-agent-acp
  answers the tool with no answers (read in its source), where `cancel`
  would abort its tool call. An interrupt answers an open elicitation
  `cancel` after `session/cancel`. A URL (or unknown) mode is answered
  with the error -32602 and a notice; a form with a field no question
  stands for (an object, a list without choices) is declined with a
  notice. The properties keep the order of the line (`serde_json` sorts
  object keys). The first form elicitation reports
  `CapsUpdate::ask_user_question`. Recorded: vibe a single choice
  accepted, a multiple choice accepted and a single one declined
  (`fixtures/vibe_acp_question*.jsonl`). Checked live (tmux, Landlock,
  scratch `VIBE_HOME`, `modes = { ask = "ask", bypass = "auto-approve" }`):
  a vibe question answered in the TUI's modal (descriptions from the call)
  and the model said the answer; `--print --policy ask` declined one and
  the model said it was cancelled, exit 0. Unverified: codex-acp's
  `request_user_input` through elicitation, an agent that withdraws an
  elicitation, number, integer and boolean fields and an empty form (a
  confirmation) from a real agent, optional fields (the modal still wants
  every question answered), and an elicitation scoped to a request
  (`requestId`) rather than a session.
  Forks (#160, `Driver::fork`): `sessionCapabilities.fork` at
  `initialize` reports `CapsUpdate::fork`, and the TUI's `/fork` branches
  an ACP agent's session only when its last session in this run said so
  (`App::caps_of`); one not run since launch, or that does not advertise
  it, starts fresh with the transcript. vibe-acp 2.26.1, codex-acp 2.1.1
  and claude-agent-acp 0.85.1 all advertise it, and need different things
  around it, so the driver, given `fork` and the stored id, sends
  `session/fork` of it; on an error it reattaches the source
  (`session/resume`, else `session/load` with the replay dropped) and
  forks once more; where the agent resumes sessions it resumes the fork;
  then the answer goes through `session_ready` like any other. Recorded
  from a new process as the driver does it (`record-acp.py --fork-from`):
  vibe answered "Session not found" (-32602) for a session it had not
  open, and after `session/load` forked it with `modes`, `configOptions`
  and the source's mode (`auto-approve` in
  `fixtures/vibe_acp_fork.jsonl`, which the driver sets back to `ask`);
  codex-acp forked the stored thread and answered with modes and options
  but unsubscribes the new thread (read in its source), and the fork's
  prompt was never answered (120 s, not committed) until it was resumed
  (`fixtures/codex_acp_fork.jsonl`); claude-agent-acp forked the stored
  files and answered with the id alone, and its `prompt` looks only at
  open sessions (read in its source), so it was resumed too
  (`fixtures/claude_agent_acp_fork.jsonl`). Each fork answered from the
  source's turn, and a vibe fork's cost started at zero. The source is
  sent nothing but the reattach, and is left open in the agent (vibe)
  until the process ends; only the fork is closed. An agent that cannot
  fork, or whose fork fails twice, gets a new session (below).
  The fork is of the whole session, which is what `/fork` copies, so
  there is no fork at an earlier point: `/rewind` on an ACP agent starts
  it fresh with the transcript up to there, and the agents' own fork
  points (vibe's `messageId` parameter, JetBrains AIR's `_meta` in
  claude-agent-acp and codex-acp) are not used. Checked in the TUI (tmux,
  Landlock, scratch `VIBE_HOME`, `modes = { ask = "ask", bypass =
  "auto-approve" }`, the wire teed to files): after one turn `/fork` sent
  `session/fork`, `session/load` and `session/fork` (vibe's answer in
  `ask`, so nothing was set), the fork answered from the first turn, a
  write and then a shell command were asked about and denied and no file
  was made, `/quit` closed the fork and left no `vibe-acp`. A session
  open in the same process (which the TUI never forks: it always forks in
  a new one) vibe-acp 2.26.1 forked at once, in the source's mode and with
  its usage from zero (`record-acp.py --fork`,
  `fixtures/vibe_acp_fork_open.jsonl`). Unverified: codex-acp 2.2.2 and
  claude-agent-acp 0.89.1 (the versions the issue names), an agent that
  forks with neither resume nor load, and whether vibe's `session/load`
  of the source writes to it.
  Sessions in place of one not resumed or forked (#187,
  `Driver::replaced`): where the driver sends `session/new` because the
  session it was given could not be reattached or forked (the agent
  cannot, `session/resume` or `load` answers an error, the fork fails
  twice or its source cannot be reattached), it reports
  `AgentEvent::SessionReplaced` with the reason before the new session's
  `SessionStarted`, and forgets `resume` and `fork`, so that a process
  started again for its spawn plan does not try them again; an
  auth-required answer starts none (#162). vibe-acp 2.26.1 answers
  `session/load` and `session/fork` of a session it does not have with
  -32602 "Session not found" (`record-acp.py --fallback-new`,
  `fixtures/vibe_acp_{load,fork}_missing.jsonl`). The TUI
  (`App::session_replaced`) takes the new session as one started for a
  harness switched to (see Bridging). Checked in the TUI (tmux, Landlock,
  scratch `VIBE_HOME`, built-in vibe, `--policy ask`): a turn given a code
  word, `/quit`, the session's directory removed from
  `logs/session/unified/`, then `--resume`: the notice named "Session not
  found", the next prompt carried the bridge (read back from the new
  session's journal) and was answered with the code word, and the saved
  conversation named the new session. Unverified: what codex-acp and
  claude-agent-acp answer for a session they do not have, and a fork whose
  source loads but whose second fork fails (the same path, not
  recorded).
  Sign-in (#162, `auth.rs`): unharness never sends `authenticate` and runs
  no sign-in; an answer with the spec's auth-required error (-32000) to
  `session/new`, `load`, `resume`, `fork` (also the reattach before a
  second fork) or `session/prompt` fails the turn with the `authMethods`
  the agent offered at `initialize` (names, descriptions, an `env_var`
  method's variables) and the command to run where it is known
  (`[harnesses.<name>] sign_in`, `AcpHarness::signing_in_with`: the
  built-in vibe declares `vibe --setup`, which its table's `sign_in`
  replaces; else the agent's own command line), with no retry and no new session in place
  of a resume or fork, and the driver ends the process, so that the next
  prompt starts one that reads the sign-in afresh. A terminal sign-in is
  a command the agent names, run outside the sandbox in the user's
  terminal, so none is run or declared (neither the spec's `auth.terminal`
  nor vibe's `_meta["terminal-auth"]`), and one an agent offers anyway is
  shown by name. vibe-acp 2.26.1 with no key (scratch `VIBE_HOME`
  without `.env`, `MISTRAL_API_KEY` unset) offered only `browser-auth` and
  answered `session/new` with -32000 "Missing API key for mistral
  provider." (`fixtures/vibe_acp_not_signed_in.jsonl`); with
  `_meta["terminal-auth"]: true` it also offered `vibe-setup`, whose
  command is its own Python running the `vibe-acp` script with
  `--setup` (`fixtures/vibe_acp_terminal_auth.jsonl`), not the agent
  binary with `args` as the spec has it. A key it does not take passes
  `session/new` and fails the prompt with -31002 ("Invalid API key (from
  env var MISTRAL_API_KEY)…", not committed), which is not taken for
  sign-in. Its `session/load` goes through the same check (read in its
  source; a load or fork failing so is only in e2e tests). Checked live
  (Landlock, `sign_in = "vibe --setup"`): `--print` printed the methods and
  the command and exited 1; the TUI showed it once at launch, started
  nothing by itself, and after `.env` was put back the next prompt
  started vibe again and was answered; `doctor` shows the command.
  vibe-acp's `_auth/status` answers without a session (`authenticated`,
  `authState`: `vibe_home_env_file` with the key in `$VIBE_HOME/.env`,
  `signed_out` without), in 0.1 s, but only after 1.6 to 1.9 s of start
  and `initialize`, too slow for `quick_auth`. Unverified: an agent that
  fails `prompt` with -32000, `env_var` and spec terminal methods from a
  real agent, `authenticate` with vibe's `browser-auth`.
- Mistral Vibe (`src/harness/vibe/`, #164): a built-in harness around
  `AcpHarness` (`vibe-acp`; a `[harnesses.vibe]` table sets its command,
  name, `asks_permission` and modes), with `ask` → `unharness-ask` and
  `accept-edits` → `unharness-accept-edits` (unharness's own profiles,
  #196, below), `auto` →
  `smart-approve` (#174, below) and `bypass` → `auto-approve`, `thinking`
  its effort option, and its model and level
  chosen at spawn (`AcpHarness::choosing_at_spawn`, `acp/spawn.rs`), never
  with `session/set_config_option`: on vibe 2.26.1 that writes the choice
  into `~/.vibe/config.toml` (`set_config_option` in `vibe/acp/agent.py`).
  The process gets `VIBE_ACTIVE_MODEL=<alias>` and, for a level, a whole
  entry `VIBE_MODELS='[{"alias","name","provider","thinking"}]'`: the
  environment layer validates each entry as a full model, so a sparse one
  fails the session, and merges the given fields over the model's own
  (`vibe/core/config/layers/environment.py`, `exclude_unset`). The name is
  the model option's `description`, so a run's first process that has a
  level and does not know it yet is started again once its session has
  answered, before anything is sent (the second process is numbered from
  id 1 again); the provider is read, never written, from the user's
  `config.toml` entry for the alias, else `mistral`. With a level and no
  model chosen, the model the last session ran is pinned with it: in a
  scratch `VIBE_HOME` the first process ran `mistral-medium-3.5` and the
  next `glm-5-3`, the experiment's choice having arrived meanwhile. A
  level outside `off, low, medium, high, max` is never put in the entry.
  A session pins its model and level before every turn
  (`_pin_session_model_choice`, `vibe/app_server/
  _unified_harness_backend_adapter.py:6400-6460`) and a loaded or forked
  session gets them back as session overrides (`_restore_session_pins`,
  `:3015-3105`), a layer only an agent profile and admin config outrank
  (`_session_model.py`), so above the environment: live, a prompted
  session restarted with `thinking: high` in `VIBE_MODELS` loaded at `off`,
  and with `VIBE_ACTIVE_MODEL=glm-5-3` on `mistral-medium-3.5`. ACP has no
  write scoped to a session (`ext_method`, `agent.py:1140`). So, as
  Richard decided, a change before the first prompt starts vibe again
  with it, and after one the session is forgotten and a new one started
  (`App::renew_session`, `Capabilities::model_per_process`), told the
  conversation by the bridge with its budget and, over it, a handoff
  summary from the old session as for a switch. After every session
  answer the driver compares `model` and `thinking` with the choice and
  says when they differ (vibe falls back to its default for an alias it
  lacks and resets a level the model does not offer, silently).
  Checked in the TUI (tmux, Landlock, a scratch `VIBE_HOME`): `/model`
  and `/effort off` before a prompt, the turn on `mistral-medium-3.5` at
  `off` (`runtime-state.json`, 3 output tokens, the model's prices);
  after a turn on `glm-5-3`, `/effort off` started a new session
  (`session/new` at `off`, `runtime-state.json` `off`, the old one still
  `high`) that named the code word of the first turn from the bridge;
  `--resume` in a new unharness started vibe with the entry (after one
  restart for the name) and the next turn ran at `off` on the same
  session; `~/.vibe/config.toml` byte-identical throughout. Seen: vibe
  2.26.1 maps `low` to `reasoning_effort: "none"` for every model
  (`_THINKING_TO_REASONING_EFFORT`, `vibe/core/llm/backend/mistral.py`)
  and `glm-5-3` refuses that with a 400, also in vibe's own picker by the
  code; unharness passes the level on as chosen. Fixture:
  `acp/fixtures/vibe_acp_spawn_env.jsonl` (model and `off` by
  environment). Unverified: a provider other than `mistral`, a model
  pinned by an agent profile or admin config, `default_effort` on a first
  run with no session yet (it waits for one, as above), macOS.
  The rest of the harness (#163, 2.26.1, `docs/vibe.md`): `ask`,
  `accept-edits`, `auto` (#174, below) and `bypass`. No plan, parked
  (#166): the Unified runtime, vibe 2.26.1's default, hides its `plan`
  agent from the mode list (`_UNIFIED_HIDDEN_PICKER_AGENTS`,
  `app_server/_projection.py`) and `session/set_mode plan` answers `{}`
  without switching; only `--legacy-harness`, a runtime being retired,
  lists it. Stdio MCP servers only (no `mcpCapabilities`; a server's
  tool comes as kind `other`, named `mcp_<server>.<tool>`, and is asked about,
  `fixtures/vibe_acp_mcp.jsonl`). Sign-in (`quick_auth`, `probe`) is
  `MISTRAL_API_KEY` set in the environment or given a value in
  `$VIBE_HOME/.env` (parsed as python-dotenv does, presence only, a regular
  file of at most 64 KiB); otherwise not known, since vibe also takes it
  from the OS keyring (`vibe/setup/auth/auth_state.py`) and a configured
  provider may use another key. `min_version` is 2.26.1, the release
  checked; `vibe-acp --version` prints `vibe-acp 2.26.1`, and creates the
  home (`logs/`, `trusted_folders.toml`, `vibehistory`) when it is missing.
  `sandbox_paths` is the home alone, found by tracing: an strace of a
  write-and-`ls` turn wrote outside the workspace only under it
  (`logs/vibe.log`; `logs/session/` with each session's journal, chunks,
  generations, locks and `.registry`; the three caches by a rename; and,
  in a new home, `trusted_folders.toml` and `vibehistory`), and the same
  turn through `unharness --print` under Landlock traced with `-e
  status=failed` was denied nothing (vibe looks for `~/.agents/skills`
  and walks up for `AGENTS.md`, reads only). `guarded` is read in vibe's
  source (`harness_files/_harness_manager.py`, `_paths.py`,
  `paths/_vibe_home.py`): `config.toml` (models, providers, MCP servers:
  `vibe mcp add` writes there), `trusted_folders.toml`, `hooks.toml`,
  `AGENTS.md`, `skills.toml`, the trees `agents`, `tools`, `skills`,
  `plugins`, `prompts`, `.env` as a `Guarded::Secret` (compared by digest,
  no copy kept, since it holds the key), and `experiment_eval_cache.json`,
  `whoami_cache.json` and `connector_bootstrap_cache.json` compared
  without their `stored_at_timestamp`/`stored_at`: vibe rewrites the
  experiment and connector caches in every session (seen), and their
  answers choose the default model and system prompt and give the
  `api_base` vibe copies into `config.toml` (`app_server/_account.py`).
  Not watched: `logs/` (journals and the session pins, every turn),
  `builtin-skills/` (rewritten from the binary when it differs), the other
  caches, `vibehistory`, `worktrees/`, `plans/`. Three `--print` sessions
  in a scratch home said nothing, and one under `bypass` whose command
  appended to `trusted_folders.toml` was reported. A relative path in an
  edit's `diff` (the model's `filePath`) is taken as in the session's
  directory (`AcpHarness::resolving_relative_edits`, vibe only): with
  `vibe-acp` started in another directory than the `cwd` of `session/new`
  the write landed in the session's, while `locations` named the
  process's (`Path(...).resolve()` in `acp/session_updates.py`, a
  recording not committed); unharness starts it in the session's, the
  location still has to name the same file (so a link on the way leaves
  it unknown), and only plain names are joined (`..`, `.` and a leading
  `~` stay unknown). Over ACP the session cannot move: `relocate` is
  reached only from the app server's `session/relocate`. Without the
  flag a relative diff path is now unknown for every agent, also with no
  `locations`. `_meta.tool_name` (`file_system.write_file`,
  `search_tool_functions`, `mcp_tiny.shout`) is not read as the name: it
  would replace the kind the TUI, the bridge and the rules read, and
  would make an `other` call `Other` instead of `Opaque` (the subagents
  below read it only to find `subagent.*` calls). Fixtures:
  `vibe_acp_allow_deny` (a write allowed, a command denied, which vibe
  reports as failed, "Tool execution denied by approval callback"; also
  through `VibeHarness` in an e2e test), `vibe_acp_mcp`, `vibe_acp_cancel`
  (`session/cancel` while `sleep` ran: the call failed with "client
  interrupt", the prompt answered `cancelled`), `vibe_acp_load`; the
  handshake is `vibe_acp.jsonl`. Checked in the TUI (tmux, Landlock, a
  scratch `VIBE_HOME`, nothing in the config): a turn, a write under `ask`
  denied and not made, `/quit` leaving no `vibe-acp`, and a `setsid
  sleep 300 &` run by vibe's shell under `bypass` adopted by the reaper
  and gone after `/quit`. Unverified: the keyring and other providers,
  plugins (none installed; whether vibe updates them itself), macOS.
  Doctor (#172, 2.26.1): `vibe-acp --version` sets up the home it is
  given before it reads its arguments (creates a missing one, tightens its
  mode, appends to `logs/vibe.log`, empties a `trusted_folders.toml` it
  cannot parse), and the version probe runs outside the sandbox in
  `doctor` and at every TUI start, so `probe` gives it a new 0700
  directory under the temporary directory as `VIBE_HOME` and removes it
  afterwards (`version_aside`); an strace of `--version` with `VIBE_HOME`
  elsewhere wrote nothing outside it. With no key and no home, doctor says
  vibe has not been set up and names `vibe --setup` (or the table's
  `sign_in`). `Harness::doctor_notes` (empty for the others) gives vibe a
  `Trust` line from `vibe/trust.rs`, which #171 builds on:
  `trusted_folders.toml` read only (a regular file of at most 1 MiB; none
  is no entry, one vibe cannot parse is said so), the folder resolved and
  walked up to the closest entry, `trusted` before `untrusted`, entries
  compared as the strings vibe stored (`TrustedFoldersManager.is_trusted`);
  a folder whose `.vibe` is the home reads no project configuration
  (`cwd_is_user_config_home`). In a folder it does not trust vibe reads
  none of its `AGENTS.md`, `.vibe/` and `.agents/skills/`
  (`_trusted_workdir`, `project_roots` in `harness_files/
  _harness_manager.py`), and the line warns when the folder has one of
  them. The project `.vibe/config.toml` and `.agents/skills/` ignored was
  seen live in #171's discovery, the project `AGENTS.md` ignored in
  #171's live check (below). The policies line now names what a harness
  does not offer (`(not plan)` for vibe since #174): without colour (a pipe)
  the dimmed ones had read as offered. Checked live (`unharness doctor`, scratch `XDG_*` and
  `VIBE_HOME`): a home with no key, one with a copy of `.env`, a path
  that does not exist (still missing afterwards, and no scratch home left
  in the temporary directory), and folders trusted, declined, undecided
  and under a trusted one. Unverified: macOS.
  Trust (#171, 2.26.1): `Harness::session_notice` (empty for the others)
  gives vibe `trust::notice`, said once per harness in a TUI run
  (`App::show_session_notice`, beside `prepare`: a session starts again
  on every model, policy or sandbox change) and as a warning on stderr
  in `--print`, when the folder has project configuration and vibe does
  not trust it (no entry, declined, or a file it cannot read). unharness
  never trusts a folder: no `--trust` (only `vibe` has it; `vibe-acp`
  takes `--setup` alone), no `trust/decision` ext method (`vibe/acp/
  agent.py:1199`), since a trusted folder's `.vibe/config.toml` picks
  vibe's agent, tools and MCP servers. vibe reports trust itself on
  `session/new` (`_meta.workspace_trust`: `status`, and `details` with
  `detected_files`, also the git repo root's, when there is something to
  trust); it is not read: the notice is unharness's own reading, as
  doctor's, of the folder only (vibe's `project_roots` is the trusted
  cwd alone, so a repo root's `.agents/skills` is not read from a
  subfolder either way). Nothing is projected to vibe from `.agents/`:
  it reads neither `commands` nor `agents` (`find_local_config_dirs`,
  `harness_files/_harness_manager.py`); its slash commands are skills,
  `.vibe/prompts/` are system prompts by id (`core/prompts`), and
  `.vibe/agents/<name>.toml` is a profile of configuration overrides
  (`agent_type`, `description`, `instructions`, anything else a config
  key) that, named like a built-in (`ask`, `auto-approve`, …), replaces
  it (`core/agents/registry.py`), so a projected `.agents/agents/ask.md`
  would take the place of the mode behind `ask`. Checked live in the TUI
  (tmux, Landlock, scratch `XDG_*` and `VIBE_HOME`, a folder with an
  `AGENTS.md` holding a code word and a `.agents/skills/pineapple-greet`
  skill): untrusted, the notice came at launch, `/pine` listed nothing,
  and asked without tools for the code word vibe answered `UNKNOWN`
  (its reasoning said no `AGENTS.md` named one); with the folder written
  into the scratch `trusted_folders.toml` outside any session, no notice,
  `/pine` listed `/pineapple-greet` and vibe answered the code word (the
  two sessions ran different default models, `mistral-medium-3.5` and
  `glm-5-3`). Under `bypass` a command of vibe's appending to
  `trusted_folders.toml` was reported by the guard at the turn's end with
  the copy from before. `--print` in an untrusted copy of the folder
  wrote the notice on stderr. A new home's first session also reported
  `trusted_folders.toml` and the three caches (vibe creates them).
  Unverified: macOS, a folder trusted through a repo root vibe offered
  (`trust_repo`), whether vibe 2.26.1 offers a project subagent profile
  to the model.
  vibe's own CLI (#165, 2.26.1, `vibe::cli_command`): `--print --native`
  is `vibe --workdir <cwd> --agent <a> [--output] [--resume <id>]
  --prompt=<text>` (one word, so a prompt starting with `-` is no flag;
  empty, vibe reads stdin), `--no-tui` the same without `--output` and
  with the prompt after `--`. The agent is unharness's `unharness-ask` or
  `unharness-accept-edits` (#196, below) or vibe's built-in
  `auto-approve` (`vibe/core/agents/models.py`), whatever a `modes` table says (that
  is for `vibe-acp`); `auto` and `plan` have none, so `print_policies`
  is the session's policies (`ask` still tied to `asks_permission`).
  `vibe` is the one beside the `vibe-acp` in use, else on `PATH`. The
  model and level go by environment through `spawn_env`; with no
  session in the run only a model the user's `config.toml` defines gets
  a level, and a level left out is said (`Harness::print_warnings`), as
  are unharness's MCP servers, which vibe's CLI is not given. Never
  `--trust`. `--format` `stream-json` is vibe's `streaming`. vibe's exit
  status is passed on unchanged: 0 finished, 1 usage or configuration
  error, 2 infrastructure failure (argparse's own errors are 2 too), 3
  stopped early (a limit, a refusal, SIGINT or SIGTERM), 4 aborted by
  an `--agent-socket` server (`RunOutcome`, `vibe/app_server/
  run_export.py`); none collides with unharness's own 130. Checked live
  under Landlock with a scratch `VIBE_HOME`: under `ask` with `--format
  stream-json` vibe printed an approval `callback` for
  `file_system.write_file`, the write failed "denied by the approval
  callback", the file was not there and vibe exited 0; `accept-edits`
  wrote the file and was denied `touch`; `bypass` with `-m
  mistral-medium-3.5` ran `printf hi > z.txt` in the workspace, on that
  model (`runtime-state.json`; `glm-5-3`, the default, without `-m`).
  The first run in a new home reported the files vibe created there
  (`trusted_folders.toml` and the three caches). In tmux `--no-tui`
  started vibe in `ask`, and with `--policy accept-edits -m
  mistral-medium-3.5` in `accept edits` on that model; under `ask` vibe
  asked before a write (Allow once, for the session, Always allow,
  Deny), and `/exit` ended it with 0. Its "Always allow" wrote
  `[tools.write_file] permission = "always"` into `config.toml` (a
  guarded file, but `--no-tui` is not watched), after which a `--print
  --native --policy ask` run in vibe's built-in `ask` wrote a file
  unasked; since #196 the agent is unharness's profile, which asks
  about such a tool (below). Unverified: a level through `--native` (unit-tested
  only), `--resume` with a `vibe-acp` session's id, a prompt on stdin
  (only that `--prompt=` with nothing there exits 1, "No prompt
  provided"), vibe's trust prompt, macOS.
  Subagents (#170, `vibe/subagents.rs`, an `acp::parse::MetaReader` the
  vibe harness gives the ACP parser, since only vibe's own `_meta` says
  what such a call is): under vibe 2.26.1's Unified runtime the model
  spawns a generic child or a custom one (`agents/<name>.toml` with
  `agent_type = "subagent"`; the built-in `explore` is not offered) with
  its runtime's `subagent.spawn`, which vibe sends as one call (kind
  `think`, title `Starting <name>`, `_meta.tool_name`, `agent`, `task`)
  completed with `_meta.child_session_id`. That completion is
  `SubagentStarted`, its id the spawning call's as for every harness, its
  description the name, its kind the profile, and the task goes in its
  own transcript; a second spawn of a name, completed without a child, is
  only a call. The child runs in the background. Its end comes only as
  the answer of a `subagent.wait` for its `agentName`
  (`rawOutput.structured_content`, `type: "success"`, the report in
  `value`), which is `SubagentEnded`; a wait that failed ("client
  interrupt", recorded; a timeout or a child's failed turn, read in
  `_subagents/_controller.py`) ends nothing. Otherwise vibe puts a
  child's answer into the parent model's input
  (`_subagents/_notifications.py`) and the client hears nothing, so a
  row the agent did not wait for stays at work until the session ends,
  saying that its end shows only when the agent waits for it. Such a
  row (`RunningSubagent::end_reported`, false under `unreported_ends`)
  does not keep the TUI busy (status rule, herdr) and holds back
  neither `/clear`, a model, effort or provider change
  (`App::refused_for_subagents`) nor a handoff summary
  (`handoff_wanted_for`), since it would hold them until the session
  ends (#215); each ends the session, and the row with it, as stopped.
  The refusals name Ctrl+S, and `Delete` on a running row names `s`,
  only where the harness can stop one. A child that answers while the
  summary is written may put its answer in that turn (not seen). A
  child's own tool calls are never sent. Its permission requests are, on the
  parent's session, id-only for a call no update named
  (`vibe/acp/agent.py:879-887`): a bare `tool` asked about as `Opaque`
  under every policy but bypass (`fixtures/vibe_acp_subagent.jsonl`, also
  through `VibeHarness` in an e2e test). Since nothing more comes about
  a call only a request named, the ACP transport ends it with the answer
  (#198, `AcpParser::answered`): allowed, by the user or the policy, it
  is done with a note that what it did is not known; denied, answered
  `cancelled` or withdrawn, it failed. An update that names it after all
  still reports its result. A call an update named and never ended stays
  running after a turn that ended normally, for every harness (the TUI
  ends a turn's open calls only when it was interrupted or failed); no
  ACP recording has one. Nothing stops a child from here:
  ACP has no method for it, `session/cancel` while the parent waited
  failed the wait and the child ran on and answered (a discovery
  recording, not committed), and only the model can (`subagent.stop`,
  `.interrupt`); so no `stop`. `--print` cannot wait for an end that may
  never be sent: `SubagentSupport::unreported_ends` ends the run with the
  turn and says how many subagents it did not wait for, and the session,
  vibe's children with it, goes with the run
  (`fixtures/vibe_acp_subagent_background.jsonl`, an e2e test; live, the
  child answered into the parent's turn and still no end came). Checked
  in the TUI (tmux, Landlock, a scratch `VIBE_HOME` with a `scribe`
  profile): the row came with the spawn, the child's write was asked
  about as `tool` and allowed, the row completed with the report, its
  transcript showed the task and the report, and `/quit` left no
  `vibe-acp`. Unverified: `subagent.send_message` (a new report would
  show as a second end), `.stop` and `.interrupt` by the model (the row
  stays at work), nested subagents, a child at `session/close` or
  process exit, a custom subagent from a trusted project, macOS.
  Rewind (#167, `vibe::REWIND_METHOD`, `AcpHarness::rewinding_with`):
  vibe-acp 2.26.1 has the extension requests `_rewind/to` and
  `_rewind/preview` (`_rewind_extension`, `vibe/acp/agent.py`; the `_`
  is stripped by its router), not advertised at `initialize`, so the
  vibe module declares `RewindSupport::conversation`. Each takes
  `sessionId` and `messageId`, the id vibe gave the user's message and
  echoed as `user_message_chunk.messageId` (live and in a `session/load`
  replay, the same id both ways), which the parser reports as the turn's
  `TurnAnchor` only for an agent that rewinds; the driver lets one
  through per turn sent and reports an empty one for a turn that was
  not sent or whose message was not echoed. `_rewind/to` drops that
  message and everything after it in place: the same session id,
  nothing on stdout but its answer (`messageContent`, the dropped
  prompt, and the restored paths), and the cut is saved (a new process's
  `session/load` replayed only the kept turns). With `restoreFiles` (by
  default true) vibe would put back the files its own snapshots saw; it
  is sent `false` and unharness's checkpoints restore files, so the two
  never both write. It refuses an id it did not give with -32602
  "Rewindable history entry not found" (`RewindFailed`) and, read in the
  source, one while a turn runs (`_require_settled`); it takes the session's first message
  (the history emptied; unharness starts a fresh session there anyway).
  A turn sent meanwhile waits for the answer. A fork's inherited messages
  get ids of their own (`imported-<n>`, `mistralai_vibe_local_harness/
  vibe/_fork.py`), so `anchors_survive_fork` is false (read, not run).
  Fixtures `vibe_acp_rewind` (two turns writing a file each with a code
  word, a rewind to the second, then only the first word known; the
  second file stayed on disk) and `vibe_acp_rewind_first` (an unknown id,
  then the first message), recorded with `--ext`. Checked in the TUI
  (tmux, Landlock, a scratch `VIBE_HOME`, a git workspace): two turns
  each writing a file, `/rewind` to the second with `f`: "session
  rewound", `beta.txt` gone, and vibe then named only the first code word
  in the same session; after `/quit` and `--resume` (reattached with
  `session/load`) a rewind to the third prompt left the first as the last
  one vibe could quote. Unverified: a rewind while a vibe subagent runs,
  and one after `/fork`.
  Providers (#173, 2.26.1): a provider is `{name, api_base, api_key_env_var,
  api_style, backend, …}` (`vibe/core/config/models.py`), and a model
  names its provider, so the provider follows the model. Providers in the
  user's `config.toml` are plain model choices (#164). Others come from
  `[harnesses.vibe.providers.<name>]` in unharness's config (`api_base`,
  `models`, optional `api_style`, `backend`, `api_key_env_var`, checked
  at startup with the table named, `vibe::providers`): one chosen
  (`--provider`, `/provider`) goes to `vibe-acp` as `VIBE_PROVIDERS` (named
  `unharness-<name>`, since vibe merges providers by name across layers,
  `WithUnionMerge`), each model as a whole entry in `VIBE_MODELS` (alias
  and name the model's, the level in the entry of the one that runs) and
  `VIBE_ACTIVE_MODEL`, all built as JSON, since a value that does not
  parse fails vibe's environment layer. Only the key's variable is named.
  The session lists vibe's own models beside the provider's, and the
  transport reports only the provider's, as its (`SpawnPlan::restrict`).
  `multi_provider` and `provider_per_process` are declared only with a
  provider defined; a change after a prompt starts a new bridged session,
  as a model change does. Recorded (`fixtures/vibe_acp_provider.jsonl`,
  `scripts/record-acp.py`, the environment as the driver sets it, the
  server redacted to `https://LOCAL-LLM`): an OpenAI-compatible server
  needing no key, `session/new` listing `mistral-medium-3.5`, `glm-5-3`
  and the provider's model as current, at thinking `off` with all five
  levels, and the model answering. Checked in the TUI (tmux, Landlock,
  scratch XDG and `VIBE_HOME`, `-H vibe --provider lan`): the model
  answered (`runtime-state.json` `active_model` it), `/provider` back to
  `agent` started a new session on vibe's default (`glm-5-3`) told the
  conversation, and `~/.vibe/config.toml` and the scratch one were
  byte-identical. A tool call through the local model ran (`echo`, which
  vibe's `ask` mode runs unasked: its bash allowlist,
  `vibe/core/tools/builtins/bash.py`; recording not committed). With
  `MISTRAL_API_KEY` in `.env`, a session on the local provider still wrote
  `whoami_cache.json`, `experiment_eval_cache.json` and
  `connector_bootstrap_cache.json`, so vibe reached Mistral's servers.
  Unverified: `api_style` other than `openai`, the `mistral` backend, a
  key through `api_key_env_var`, what vibe calls without a Mistral key, a
  local model in the session title or the smart-approve classifier, a
  resumed session whose provider is no longer chosen (its model is
  dropped with a notice, and vibe restores the pinned alias, unknown
  there).
  `auto` (#174, 2.26.1) is vibe's `smart-approve` mode, as Richard
  decided; an ACP agent is offered `auto` only when its map names a mode
  for `auto` itself (`ModeMap::names`). In that mode each builtin or MCP
  call that vibe's own permission rules would ask about goes to Mistral's
  classifier, `mistral-small-latest`, with the call's arguments (up to
  3000 characters), the latest user message (up to 2000) and the writable
  roots (`_classify_gate` in `mistralai_vibe_local_harness/vibe/
  _local_actions.py`, `_smart_approve.py`). `ALLOW` runs the call unasked;
  `PROMPT` on something the user did not ask for fails it with "Smart
  approve blocked this action: <reason>" (deny-and-continue); a repeat of
  a blocked call, a call judged user-authorised, the third denial in a
  row or twentieth in a turn, a write naming a path that decides what
  runs later (`.vibe`, `.git/hooks`, `.git/config`, shell startup files;
  `_protected_paths.py`) and a classifier error escalate to an id-only
  `session/request_permission` that carries no reason. The verdicts are
  invisible to unharness: a blocked call's reason arrives only as that
  failed call's text, and nothing says why a call ran. Destructive
  commands inside the workspace run unasked: `rm -rf` of a scratch tree
  ran twice in the discovery's recordings (not committed), once on "if
  junk looks unimportant, get rid of it". Of four writes outside the
  workspace that the user asked for, one escalated (the discovery's, its
  options scoped `outside_directory`) and three ran unasked
  (`fixtures/vibe_acp_auto.jsonl` after a blocked `mkdir`,
  `fixtures/vibe_acp_auto_outside.jsonl`, a shell `printf >` in the TUI),
  so what keeps a call in the workspace is the sandbox. In
  `vibe_acp_auto.jsonl` a workspace edit was blocked with a reason about
  the other path, and asked about when the model repeated it. Under
  `auto` unharness answers no request itself (`transport::auto_allows`,
  also not an edit, which accept-edits answers) and sends only
  `allow_once` or `reject_once`, never vibe's "for this session" and
  "always" options. smart-approve is listed only for the cohort of the
  `vibe_cli_smart_approve` experiment (`smart_approve_available`,
  `vibe/core/config/vibe_schema.py`); outside it (a fresh `VIBE_HOME`, or
  `VIBE_SMART_APPROVE_AVAILABLE=false`) `session/new` lists `ask`,
  `accept-edits` and `auto-approve`, `set_config_option` of it answers
  -32602, `set_mode` answers `{}` and changes nothing, and a session
  pinned to it loads in `accept-edits`. So every vibe process gets
  `VIBE_SMART_APPROVE_AVAILABLE=true` (the environment layer is above the
  experiments': it was then listed in new and loaded sessions and set and
  read back through the mode option, the default unchanged), and under
  `auto` `VIBE_DEFAULT_AGENT=smart-approve` (`spawn_env`, from
  `SpawnChoice::policy`; also on a provider of unharness's,
  `smart_approve_env`, and neither for vibe's own CLI, which has no
  `auto`): a new session answers `currentModeId:
  "smart-approve"`, so nothing is sent, and vibe looks for its classifier
  model as the session opens, which it does only for a session opened in
  the mode (`vibe/app_server/_runtime.py`). Like vibe's `--smart-approve`
  it forces the mode past a user's `disabled_agents`
  (`vibe/core/agents/manager.py`), which otherwise hides it, variable or
  not. A session that does not offer the mode refuses `auto` and the TUI
  falls back to accept-edits with the warning. A change to `auto` in a
  running session sets the mode through the option, with no restart; the
  driver keeps `cfg.policy` current for a process started again for a
  model or effort. Checked in the TUI (tmux, Landlock, scratch `VIBE_HOME`
  and XDG directories, `glm-5-3`): under `--policy auto` the header said
  `auto` and `vibe-acp` had both variables; an append to a workspace file
  ran unasked; a write to `/tmp/l174/outside/z.txt` (writable in the
  sandbox) ran unasked; a write to `.vibe/notes.txt` in the workspace came
  as a modal and, denied, was not made. Started under `ask`, a `touch` was
  asked about, then Ctrl+P → auto set the mode in the same `vibe-acp`
  process and the next `touch` ran unasked. With
  `VIBE_DISABLED_AGENTS='["smart-approve"]'` and a prompt sent, Ctrl+P →
  auto showed `auto→accept-edits` and the warning (before a prompt the
  TUI starts the session again under `auto`, and the variable forced the
  mode). Unverified: the classifier unreachable (escalates, by the
  source), a provider other than Mistral's (the classifier route keeps the
  active provider and swaps only the model name), MCP tools under it,
  `--print --policy auto` (its escalations are denied unless a rule
  allows them), macOS.
  Own profiles (#196, 2.26.1, `vibe/profiles.rs`, Richard's decision):
  in vibe's built-in `ask` a tool runs with no
  `session/request_permission` when its configuration allows it:
  `[tools.<name>] permission = "always"` or an allowlist entry (what vibe's
  own "Always allow" persists), `bypass_tool_permissions = true`, the same
  in a trusted project's `.vibe/config.toml` or a `VIBE_*` variable, or a
  profile named `ask` in `~/.vibe/agents/` or a trusted project's
  `.vibe/agents/` that replaces the built-in (the discovery's recordings,
  #196). vibe applies an agent profile above every layer but admin config
  (`default_orchestrator.py`: defaults, GrowthBook, user TOML, project
  TOML, environment, runtime overrides, profile, admin), its `tools`
  merged key by key (`WithDeepMerge`, a list inside replaced whole,
  `core/utils/merge.py`), so a key the profile does not name keeps the
  lower layers' value. So unharness writes `unharness-ask.toml` and
  `unharness-accept-edits.toml` under `<state dir>/unharness/vibe-agents/`
  (protected: never writable in the sandbox, readable), in a
  subdirectory named after a digest of their text (two unharness whose
  vibe configurations name other tools never rewrite each other's while
  a vibe reads them; a file that differs is replaced by a rename), and
  every `vibe-acp` process gets `VIBE_AGENT_PATHS='["<dir>", …what the
  variable named before]'`, whatever the policy, so that a later policy
  change sets the mode with no restart. Each profile sets
  `bypass_tool_permissions = false`, `disabled_tools =
  ["exit_plan_mode"]` (the built-in's) and `[tools.<name>] permission =
  "ask"`, `allowlist = []` for `write_file`, `edit`, `powershell`,
  `git_bash` and every key the user's `config.toml`, the project's (found
  as vibe finds it, from the session's directory up to the one holding
  vibe's home, trusted or not: it can only ask more) and `VIBE_TOOLS` /
  `VIBE_TOOLS__<NAME>__…` in the environment name, `never` kept where any
  of them says it; bash gets vibe 2.26.1's default allowlist
  (`_get_default_allowlist`, `builtins/bash.py`) without `git status`
  (which rewrites `.git/index`), written into the file with that comment.
  An entry runs a part unasked only when vibe's guardrails pass: no
  redirect, `$()` or `sudo`, no path outside the workdir, none of the
  writing options `_shell_command_policy.py` gates (`sort -o`, `find
  -delete`/`-exec`, `tree -o`, `uniq in out`, `git … --output`, `date`
  setting the clock). `allowlist = []` on the file tools matters: an
  allowlist glob answers ALWAYS before the outside-workdir check
  (`resolve_file_tool_permission`, `core/tools/utils.py`).
  `unharness-accept-edits` is the same with `write_file` and `edit`
  `always`: vibe runs them in its workdir and still asks for a path
  outside it or a sensitive one, which the transport answers as for any
  ACP agent (an edit inside the workspace allowed, #204; else the user).
  A config file that is there and cannot be read (not regular, over
  1 MiB, not UTF-8 or TOML, `tools` not a table) or a state directory
  that cannot be written refuses both policies with the reason
  (`SpawnPlan::own_modes`), never a degraded profile. Profiles are found
  in `agent_paths` first, then a trusted project's `.vibe/agents/`, then
  `~/.vibe/agents/`, the first of a name kept (`core/agents/
  registry.py`), and `agent_paths` concatenates across layers in order
  (`WithConcatMerge`), so a directory the user's or a trusted project's
  config lists comes before unharness's. Each description carries a nonce
  (32 hex digits in `vibe-agents/nonce`, made once, linked into place);
  the ACP mode step holds a mode of the plan's `own_modes` only when the
  session describes it with it, in `availableModes` and in the mode
  option, and refuses the policy otherwise (`acp::modes::OwnModes`,
  a unit test, and an e2e test with another nonce). The
  nonce is readable from the sandbox (vibe reads it there), so it tells a
  profile that is not unharness's from its own, not a copy of it: an
  agent in a trusted folder that writes a `.vibe/config.toml` naming an
  `agent_paths` directory with a copied `unharness-ask.toml` gets past it
  (such a config can already start MCP servers, #171). So at every spawn,
  a session's and vibe's CLI's alike, `profiles::prepare` also refuses a
  mode whose file (`unharness-ask.toml`, `unharness-accept-edits.toml`)
  exists in an `agent_paths` directory of the user's or the project's
  config (trusted or not; each entry relative to the session's directory,
  literally and with `~` expanded, since 2.26.1 does not expand it), the
  file named in the refusal; a `VIBE_AGENT_PATHS` the user set is not
  looked at, since unharness's directory goes before its entries. What
  remains: a config or profile written between that read and vibe's
  session open (the description check still sees a profile that is not a
  copy), a copied profile in a directory the check cannot see (GrowthBook
  or admin config), and admin config (fetched from the organisation at
  startup, `layers/admin.py`), which wins over the profile. The mode is set through the mode option as before; vibe's
  `set_config_option` and `set_mode` both call `agents.switch`
  (`acp/agent.py`), and the session lists the custom modes in both.
  Recorded with the environment the driver sets (bwrap, a scratch
  `VIBE_HOME` whose `config.toml` had `bypass_tool_permissions = true`,
  `write_file` and `bash` `always`, and an `agents/ask.toml` allowing
  `write_file`): `fixtures/vibe_acp_own_profile.jsonl` under
  `unharness-ask`, a `touch` and a write both asked about, denied, nothing
  written; `fixtures/vibe_acp_own_profile_edits.jsonl` under
  `unharness-accept-edits`, the write in the workspace ran unasked, a
  write to `/tmp/l196/outside.txt` and the `touch` were asked about
  (e2e tests through `VibeHarness` with the recorded nonce, one with
  another nonce refused, and one with an `agent_paths` entry holding an
  `unharness-ask.toml` refused at spawn). Checked live with the same home: `unharness
  --print --native --policy ask` (Landlock) had vibe ask about
  `file_system.write_file` and deny it, nothing written; the TUI (tmux,
  Landlock) under `ask` showed the write's modal. The e2e tests built
  before #196 on recordings in vibe's `ask` run with that map as a
  `modes` table. Unverified: admin config, MCP tool keys (named in the
  profile like any key; no recording), a subagent's calls under the
  profile (its tool modes are its parent's ceiling,
  `_subagents/_configuration.py`, read only), Windows shells, whether
  vibe re-reads profiles for a later session in the same process (it
  discovers them at every session open, read), and `--no-tui` with the
  profiles (unit-tested only). vibe's scratchpad under
  `$VIBE_HOME/logs/session/` is written unasked under every profile
  (`is_scratchpad_path` comes first), as before.
  Acceptance (#176, vibe 2.26.1, Claude Code 2.1.296 haiku for the
  switch): every item of #176 was run in the TUI (tmux, Landlock, scratch
  XDG directories and `VIBE_HOME`, whose `config.toml` set
  `[tools.write_file] permission = "always"`, a git workspace in `/tmp`)
  or headless where #176 says so, and held: under `ask` a write (three
  ways: `write_file`, a shell redirect, vibe's `file_system.write_file`)
  and `touch` came as modals and were denied, and "allow always" made a
  shell rule that answered the same `touch` next turn; `/policy` changes
  set the mode in the same process (`accept-edits`: `write_file` ran
  unasked, `touch` asked; `bypass`: nothing asked); a question answered
  in the modal; Esc during `sleep 45` ended the turn interrupted and the
  `sleep` was gone; `/model` and `/effort off` after a prompt started a
  new session with a handoff summary (`runtime-state.json`
  `mistral-medium-3.5`, `reasoning_effort` `off`; both `config.toml`
  byte-identical; no `set_config_option` but `mode` on the wire); an
  attached PNG named nothing telling was answered from; `\/compact` ran
  vibe's, `/compact` said vibe cannot; a skill in `~/.agents/skills` ran
  as `/name`; a generic subagent ended with its report; the stdio MCP
  server's tool was called (asked about under `ask`); `/fork` (the fork
  answered in `unharness-ask`) and `--resume` in a new unharness (loaded
  in `auto-approve`, set back to `unharness-ask` and read back) carried
  the conversation; a switch to Claude (with a handoff summary over
  `bridge_max_chars = 600`) and back carried it both ways; the guard
  said nothing in six sessions after the new home's first (which
  reported the files vibe creates, as above) and reported a command's append to
  `trusted_folders.toml`; `/quit`, Ctrl+D and `tmux kill-server` each
  left no `vibe-acp` and no `setsid sleep` it started; a write to the
  home directory was refused by the sandbox; `--print` (text, json,
  stream-json, a write denied under `ask` and listed in `denied`),
  `--print --native` and `--no-tui` under `ask` asked about or denied the
  write despite the `always`; `doctor` showed version, sign-in and
  trust. Not checked there: pasting an image from the clipboard (no
  display; `/attach` was used), and the real `~/.agents/skills` (vibe
  looks under `Path.home()`, so `vibe-acp` was given a scratch `HOME`).
  macOS is parked (#175): no Mac was available, so the Seatbelt profile
  with vibe's home writable, vibe's processes on quit there (no reaper),
  `quick_auth` and the install path are unverified, as is everything
  marked macOS above. Also unverified: http MCP servers (vibe 2.26.1
  takes none over ACP) and plan (#166, above).
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
  only in place once it answers) and `--fork <id>` (entry ids survive);
  ACP `session/fork` where the agent advertises it (vibe-acp 2.26.1,
  codex-acp 2.1.1, claude-agent-acp 0.85.1; see the ACP note), of the
  whole session only, with no rewind in the spec, so no anchors to carry
  (vibe's own `_rewind/to` is in the vibe note).
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
  `/dev/null`, `setsid`, the process tree killed on Esc and on quit, and
  on Linux what it left when the shell exits). No permission prompt, no rule. Refused during a
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
- Copying from the transcript (#152, `tui/rows.rs`, `tui/selection.rs`,
  `transcript::block_text`): every rendered row carries what a copy takes
  (`RowCopy`), its text without the drawing around it and how it goes on
  from the row above (the space a word wrap dropped, nothing inside a
  word), built with the row (`Rows`), so a renderer that adds drawing has
  to say so. A drag copies that; one over nothing but drawing copies it as
  drawn. A right click copies a block's `block_text` (a call's whole
  command and output, a written file with its tabs, a response's
  markdown), and on a code block in a response that block's code
  (`markdown::code_blocks`, matched by order to the rows `Rows::code`
  marks). Checked live (tmux, Claude Code haiku, a stand-in `wl-copy`):
  drags over a wrapped Bash call and its output, its label alone, a
  Write, a list and a code block; right clicks on a collapsed call, a
  Write (its tab kept), a code block and prose; a real right click
  through a tmux with `mouse on` reached unharness, not tmux's menu.
  Unverified: herdr, other terminals' right button, macOS.
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
- Plan mode is `PermissionPolicy::Plan`, the least permissive policy,
  driven on Claude only (#133). Claude Code 2.1.296
  (`fixtures/plan_*.jsonl`):
  - Started with `--permission-mode plan` (or `set_permission_mode
    plan`), it writes its plan to `~/.claude/plans/<slug>.md` without a
    request (writable in the sandbox as part of `~/.claude`). It runs
    read-only commands unasked.
  - It asks before a write (its Write check returns `ask`, "Cannot write
    to … while in plan mode", before its own allow rules), which is why
    only a read rule answers anything under plan. A session launched with
    `--dangerously-skip-permissions` and then set to plan still asked
    (`plan_after_bypass`): its `plan` acts like bypass only in an
    interactive session (read in the binary, `lq`).
  - `ExitPlanMode` comes as `can_use_tool` with `plan` and `planFilePath`
    (`PermissionKind::PlanApproval`, a modal of its own; no rule answers
    it).
  - On approval Claude restores the mode it planned from
    (`prePlanMode ?? "default"`), but only if the mode is still `plan`.
    So the driver sends `set_permission_mode` with the chosen policy
    before the allow.
  - `bypassPermissions` is refused in a session not launched with it
    (`bypass_not_launched`), so the modal offers bypass only when the
    session was started under it (#142 for Ctrl+P).
  - A deny reaches the model verbatim as the tool's error. A bare
    "Use Howdy" was followed by an edit to the plan and then the old
    change, so the transport words it as Claude's own interface does
    ("…To tell you how to proceed, the user said:\n…", or "STOP what you
    are doing…" with none), and haiku followed that.
  - Every mode change, ours or the model's own `EnterPlanMode` (which
    asks nothing), comes as `system` `status` with `permissionMode`
    (`AgentEvent::PolicyChanged`). The TUI takes one it did not set as
    the harness's policy.
  - Checked in the TUI (tmux, Landlock, haiku): reject with feedback and
    the plan was revised; approve under accept-edits and the edit ran
    unasked; a resume came back in plan.

  Unverified: plan under `auto` before plan (the auto-mode gate falls back
  to `default` on exit, read in the binary), `dontAsk` (no policy; a
  notice), plan approval by a subagent (`awaitingLeaderApproval`).
  `--print` and `--no-tui` do not offer plan.

  Still declared, not driven (`Capabilities::plan_mode`):
  - Codex 0.157.0 `collaborationMode` on `turn/start` (app-server schema;
    `exec` has none), read from the schema, not run;
  - an ACP session's `modes` with id `plan` (claude-agent-acp 0.85.1 lists
    one, codex-acp 2.1.1 does not).

  pi plans only through an extension, and agy's `--mode plan` does not
  wait (see above).
- Remote Control (#113, `/remote-control`, `/rc`), Claude only
  (`Capabilities::remote_control`). Claude Code 2.1.296 refuses its own
  `/rc` over stream-json ("isn't available in this environment") but takes
  the SDK's control request `remote_control` (`enabled`, `name`; read in
  the binary), answered with `session_url` (the link; `connect_url` and
  `environment_id` came empty) or an error, and `enabled: false` with an
  empty success. `system` `bridge_state` says `ready`, then `connected`
  (`failed` with a `failure_kind`, `terminal` when none is given, and
  `policy_disabled` end it). Recorded with a person on the remote side
  (`fixtures/remote_control_{permission,prompt}.jsonl`): a prompt typed
  there runs as a turn unharness did not send, and is on stdout only under
  `--replay-user-messages`, as a `user` line with `isReplay` and `origin:
  {"kind": "human"}` (ours are echoed with no `origin`), which is why the
  flag is passed; a `can_use_tool` request goes to both sides, and one
  answered there comes to us as `control_cancel_request` (withdrawn). The
  TUI keeps it across a new process of the same harness (sent again once
  it starts) and takes it off on a switch, a fork, a resume, `/clear` and
  quit. A remote prompt is checkpointed and titles a new conversation as
  a typed one does, and leaves what the bridge owes for the next prompt
  typed here; it has no rewind anchor (Claude was not recorded rewinding
  to one), so a rewind to it starts a new session.
  Checked in the TUI (tmux, Landlock, haiku): the link was shown, a prompt
  typed remotely was shown as the user's and made the session the
  conversation's, the modal of its Write closed when it was allowed
  there, a `/sandbox` restart came back with the same link (Claude resumed
  the session), `/rc off` and `/quit` left no Claude process, and the
  guard said nothing. Not committed: the recording with only local turns.
  Unverified: a `failed` or `policy_disabled` state, an org that refuses
  Remote Control, a model or mode changed from the remote side (the mode
  would come as `status`, as the model's own changes do), macOS.
- Harness versions (`Harness::min_version`, `doctor`): only pi declares
  one, 0.78.1, read in its changelog (`--session-id` came in 0.76.0, the
  gate's `ctx.mode` in 0.78.1); recordings start at 0.87.1. pi before
  0.74 is the deprecated `@mariozechner` package: 0.73.1 refused
  `--session-id` ("Error: Unknown option: --session-id", exit 1,
  `fixtures/too_old.jsonl`) and, with stdin not a terminal, printed
  `--version` on stderr and nothing on stdout (run, with its agent
  directory in scratch), which is why `probe_version` takes stderr when
  stdout is empty and stderr is a bare version. Only that refused flag
  gets the upgrade hint; another says nothing about the version. `pi
  update --self` moved it to `@earendil-works` 1.0.4 (reported) and is
  still accepted on 1.1.0. A version doctor cannot read is a warning for
  every harness but an ACP agent (`reports_version`), whose command may
  be a launcher such as `npx`.
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
  a read by its tool name alone. pi 1.0.4: two calls in one message are
  asked about one after the other, each after its `tool_execution_start`
  (`tool_call` handlers run in turn, `fixtures/gate_parallel.jsonl`); the gate passes the turn's
  `ctx.signal` to its dialog, because without it `abort` while the
  dialog is open went unanswered and the turn never ended (#89). With it
  the call fails with "Operation aborted" and the turn settles as any
  other (`fixtures/gate_abort.jsonl`), and pi sends nothing about the
  dialog, so the transport withdraws it (`PermissionWithdrawn`) and
  reports the turn interrupted. A request that arrives while a picker is
  open waits until the picker closes. Unverified: the gate's human-readable dialog in
  pi's own interface (`--no-tui`, `ctx.mode` other than `rpc`), tool
  calls made by a pi sub-agent or an extension that runs tools without
  `tool_call`, agy's own interface under `ask`.
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
  `allow_once` gets a cancel and the user a notice. A request Claude
  cancels (`control_cancel_request`: a hook answered it, or it was
  answered remotely) is withdrawn, which closes its modal. Not done: a
  lock around appending to a rules file (two unharness saving at once can
  lose one rule), re-reading rules while running.
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
  pane. A vendor integration (`herdr integration install claude`)
  reports for the same pane under its own `source`, and one
  `pane.report_agent_session` from `herdr:claude` made herdr ignore
  every later `pane.report_agent` from unharness (stuck on `working`;
  `pane.clear_agent_authority` did not undo it). It outlives the CLI: a
  `--print` run's hook (Claude's integration, version 8, sends only
  `report_agent_session`, `seq` in nanoseconds) held the pane for the
  TUI started there next, and neither a release nor a clear from
  `herdr:claude` with a later `seq`, nor a session report from
  unharness, let go of it. So in the TUI and `--print` what unharness
  starts gets every `HERDR_*` variable with `HERDR_SOCKET_PATH` naming
  a proxy (`tui/herdr_proxy.rs`, a socket in
  a 0700 directory under the runtime directory): `pane.report_agent`,
  `pane.report_agent_session`, `pane.release_agent`,
  `pane.clear_agent_authority` and `pane.report_metadata` are answered
  there with `{"type": "ok"}` (what herdr answers; all 17 built-in
  integrations of 0.8.2 send only the first two, one request per
  connection, and act on no answer), for any pane id, and everything
  else goes to herdr's socket (`Sandbox::wrap`, `HerdrEnv`). herdr
  refuses an array, another spelling of a method and a duplicate key, so
  a report has no other way through; an answer given in the proxy can
  overtake one of herdr's on a connection with several requests. A
  client that has closed its connection ends herdr's side (`POLLHUP`, so
  that an idle subscription does not hold it), and a killed
  unharness's proxy directory is removed by the next. Without the
  pane id an agent's `herdr pane current --current` answered with the
  UI-focused pane, and `pane get --current` with `pane_not_found` (#67);
  hiding the variables was never a boundary, since herdr finds its
  default socket by itself, and that still gets past the proxy (Landlock
  is not asked to confine connecting to a socket: ABI 9's
  `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` is not handled, so herdr's, tmux's
  or Docker's socket leads out of the sandbox). Under `--no-tui` and
  `herdr = false` the variables pass as they are; where the proxy fails
  to start they are removed. Checked in the TUI against an isolated herdr server
  (scratch `XDG_CONFIG_HOME`, tmux, Landlock, Claude Code 2.1.296 with
  its herdr hook installed, focus on another pane): the agent saw all six
  variables, `herdr pane current --current` named its own pane, `herdr
  pane split --current --no-focus` split its own pane, the hook's
  `pane.report_agent_session` was kept from herdr (no `agent_session` on
  the pane) and the pane went `working`, `idle` as unharness reported;
  `/quit` released the pane and removed the proxy's directory. A
  `--print` run went through the proxy, left the pane without an
  `agent_session`, and a TUI started there next went `idle`, `working`,
  `idle`; the directory a SIGKILLed unharness left was removed by the
  next run. `herdr pane get` takes only a pane id (0.8.2): `--current`
  there is `pane_not_found` in any pane. Checked before with a
  prototype: layout, run, wait-output, agent list and wait through it.
  Unverified: `events.subscribe` live (only against a stand-in), a
  client that sends several requests on one connection. On macOS
  `poll` asked for nothing never said `POLLHUP` (the test failed on
  GitHub's runner): it registers a kqueue filter only for what is asked
  for, so `hung_up` asks for `POLLOUT`, whose filter ends once the
  socket cannot send, set by the peer's close and not by its
  `shutdown(SHUT_WR)` (read in XNU's `poll_nocancel`, `filt_sowrite`,
  `unp_disconnect`, `unp_shutdown`), and the test passed there with
  it. Seen with the variables inherited,
  before the proxy: pi's herdr extension reports
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
  multi-select answer sent as a JSON array reached the model as the
  labels joined by a bare comma, unquoted (pty runs, 2.1.291 and
  2.1.293, whose tool result writes the answer with a template literal),
  so the transport sends a string joined the way Claude's own joiner
  does (read in 2.1.293): by `", "`, a label containing `", "` or `"`
  as a JSON string. The form has to be exactly that: 2.1.293 says the
  questions were answered (rather than telling the model to read the
  answers carefully) for an array of option labels, or for a string
  that splits back into option labels and joins again to the same
  string. Checked in the TUI (pty, 2.1.293): labels with `", "`, with
  `"` and with both came back quoted, and the result said the questions
  were answered. Codex 0.157.0's
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
  over RPC (its docs). Codex's commands are its interface's own; on
  app-server (0.157.0) `skills/list` (`cwds`) answers each skill's
  `name` (`plugin:skill` for a plugin's), `description`, `path`,
  `enabled`, and `skills/changed` says to ask again. A turn's input takes
  `{"type": "skill", "name", "path"}`: with a workspace skill whose
  instructions were a fixed answer, `$name args` as text, the text with
  the item and plain text with the item each got that answer with no tool
  call (`fixtures/app_server_skill.jsonl`). The driver asks once the
  thread is there, with a string id so that recorded answers keep theirs,
  holds a `/` prompt until the answer, and sends `/name args` for a listed
  skill as `$name args` with the item, as Codex's interface does, so that
  skills are `/name` on every harness. `exec` lists none. agy 1.3.2 runs
  a skill from a `/name` prompt on stream-json input (no tool call) and
  refuses its own commands there ("answered by the CLI itself"), `init`
  lists no skills, and `/skills` on stream-json input is refused;
  `agy --print=/skills` (since 1.1.11, by its changelog: no turn, no
  quota, no conversation) prints `name<TAB>description` per skill in
  about 3 s. Since agy is not started before the first prompt, the TUI
  asks for that (`Harness::list_commands`, a `ListRequest`) in the
  session's sandbox, only from an agy whose `--version` is three numbers
  at least 1.1.11. In a process group of its own in the TUI's session it
  was stopped for good (agy looks at the terminal), which is why a
  `ProbeProcess` now leads a session of its own. Checked in the TUI (pty,
  Landlock, Codex 0.157.0 app-server and agy 1.3.2): `/pine` before the
  first prompt listed the workspace's `pineapple` skill, and
  `/pineapple hello there` got its fixed answer. Unverified: Codex skills
  on `exec`, agy's plugin skills, a skill with `disable-slash-command`.
  A command has to start the message, so it goes alone and the bridge and
  unsent `!` output wait for the next prompt, whose bridge then also tells
  the command's turn (a switch keeps the held start). A queued `/` prompt goes only to the harness it was
  typed for. One of the agent's that does what one of unharness's does
  (`/clear`, `/model`, `/compact` and their aliases) is warned about, not
  followed: unharness's model, transcript and bridge are not updated.
  Checked in the TUI (pty): Claude ran `/hello world` and `\/context`, and
  pi's list came from `get_commands`. Unverified: codex-acp's commands
  (its recordings listed none, so it is not taken to run any), an unknown
  `/name` on an ACP agent, a Claude command added during a session (not
  listed until the next process).
  The TUI starts the active harness's session whenever it is idle with
  none and a policy is chosen (launch, a switch, a resume, a sandbox or
  provider change), not with the first prompt, so that the list is there
  before it (#118). Until a prompt goes to it the session is not the
  conversation's (`App::session_blank`: nothing saved, its id held back,
  the bridge sent whole); a model, effort or policy chosen meanwhile
  starts it again with the choice, and one that exits, fails to start or
  fails its handshake waits for the user. Checked in the TUI (pty, Claude
  Code 2.1.295): `/cq` typed before the session was up filled in with the
  `cq:` plugin skills, `cq:reflect` (which the model cannot invoke)
  included, and Claude's `SessionStart` hook now runs at launch (again
  on such a restart); Codex 0.157.0 (app-server) and pi 1.1.0 quit
  unprompted left no session file and no saved conversation (pi says it
  creates the session it was given at launch). agy is started with the
  first prompt as before (`Capabilities::start_unprompted`): 1.3.2 writes
  a conversation for every process, and its `init` had opened a turn
  that never ended, behind which the first prompt waited.
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
  would land in that turn), but for one whose end is never told (vibe's,
  #215). Checked live (pty, Claude Code 2.1.292 → Codex 0.157.0
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
  A session that a harness runs in place of one it could not resume or
  fork (`AgentEvent::SessionReplaced`, #187) is bridged as a new one:
  before anything went to it the old id is forgotten (the new one is
  committed with the first prompt, which carries the bridge); after a
  prompt went with its start (resumed by that prompt, so without the
  conversation) the next prompt bridges from the start, the last turn
  included. Rewind anchors and a pending fork of the harness go. A
  handoff summary asked of such a session is not used: the switch goes
  on without one, and the session left is forgotten. Sent by the ACP
  driver (above) and by pi's parser: pi 1.1.0 given a `--session-id` it
  has no session for warns on stderr and creates an empty one under that
  id, so a resumed session whose first `get_fork_messages` lists no user
  message is one (`fixtures/resume_missing.jsonl`, `record-pi.py
  --resume`; `--fork` of a missing session exits 1). The others fail
  instead of starting fresh, so they report nothing: Claude Code 2.1.296
  `--resume` of an unknown id ("No conversation found with session ID",
  exit 1) and Codex 0.157.0 `exec resume` ("thread/resume failed: no
  rollout found", exit 1; the app-server driver fails the turn on a
  `thread/resume` error). Such a resume is tried again with the next
  prompt (read in the TUI, not run). Unverified: agy's `--conversation` of a missing conversation
  (a per-turn harness reporting another id than the one resumed is not
  taken as a replacement).
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
change per commit, tree green (fmt, clippy, tests) at every commit. A
release bumps the version and adds its notes to `CHANGELOG.md` (the steps
are under Releases in `CONTRIBUTING.md`).
