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
  confirm the flag exists on the installed version. What was recorded and
  checked on that harness, and on which version, is in its
  `src/harness/<name>/NOTES.md`.
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
  itself: allowed by a rule, denied otherwise (`src/headless.rs` through
  `core/controller/tracking.rs`; who answers a request is decided in
  `core/permissions.rs`). Declaring it needs a recording or a live run
  that shows the asking. The policy never chooses a sandbox level.
- **Parsers are pure and fixture-tested.** `Parser::feed(line) -> Vec<AgentEvent>`
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
  not parse is an error, never a silent fall back to defaults, and so is
  a harness table that is wrong (an unknown policy, Codex transport or
  protocol, a mode map or a provider that is not whole, an ACP agent without a
  command, a key its harness does not take, a table no harness takes, a
  key unharness no longer takes, `registry::config_errors`):
  a run, `switch` and `models` stop on it in `Registry::from_config`, and
  `doctor` names it, leaves out the harness it could not build (no
  built-in or preset takes its name) and exits 1 (#217).
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
  LineProcess` (kill-on-drop, bounded drain) or a driver in
  `core::driver` (the long-lived loop, or `per_turn`), which ends, kills
  and drains them; never a bare `tokio::process::Command::spawn` in a
  transport. Each leads a
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
  user interrupts again, once told that would (`Controller::end_stuck_turn`);
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
  (`controller::Ending`), so that two never write one session: one still there
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
harness → vendor session map, bridging bookmarks, per-harness usage and
the last turn's usage, and the context fill (told again only for the
session and model it was measured on), the active harness, and the policy, models and efforts chosen for it (never a
configured default; one named in the run resuming it wins), one JSON
file per conversation plus an index. `Controller::persist`
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
   implementing `Harness` and declaring honest `Capabilities`. The
   transport implements `core::driver::StreamProtocol` (one process for
   the session: its handshake, what a command becomes, what it makes of a
   line, how its CLI is ended) and starts with `driver::start`, or
   `core::driver::per_turn::PerTurnProtocol` (a process per turn); the
   driver owns the process, ending and reaping it, and the answer to a
   command the `Capabilities` do not offer. Override
   `quick_auth` only if sign-in can be checked in well under a second; it
   runs at startup to choose the default harness. Declare the CLI's own
   state directories in `sandbox_paths`, and run a turn with the sandbox on.
   Declare `min_version` when an older release is known to lack a flag or
   API unharness uses; `doctor` warns below it.
3. Record a fixture with a `scripts/record-<name>.py` and generate its
   `.events` with `UNHARNESS_UPDATE_FIXTURES=1`. Write
   `src/harness/<name>/NOTES.md`: what was recorded and checked live, on
   which version, and what is unverified. Name it in the module's doc
   comment and add the harness to the table under Status notes.
4. Give it a `build` that parses what its `[harnesses.<name>]` table takes
   besides the keys every harness takes (`HarnessSettings::own`), and add
   that to `BUILTIN` in `core/registry.rs`.
5. Add an e2e test if the transport has a handshake or permission channel.

## Status notes

What was recorded and checked live, on which vendor version, and what is
still unverified lives beside the code it is about: each harness's note is
its `src/harness/<name>/NOTES.md`, and what crosses harnesses is in
`docs/dev/`. A change adds what it found to that file, not to this one.

| Harness | Note | Checked on | Parked or not declared |
|---|---|---|---|
| Claude Code | [`src/harness/claude/NOTES.md`](src/harness/claude/NOTES.md) | 2.1.289 to 2.1.302 | |
| Codex | [`src/harness/codex/NOTES.md`](src/harness/codex/NOTES.md) | 0.157.0 | `ask` on `exec`, `--print` and `--no-tui` ([ask](docs/dev/ask.md)) |
| pi | [`src/harness/pi/NOTES.md`](src/harness/pi/NOTES.md), which lists the notes that cover it | 0.87.1 to 1.1.0 | plan mode: pi plans only through an extension ([plan mode](docs/dev/plan-mode.md)) |
| Antigravity (`agy`) | [`src/harness/agy/NOTES.md`](src/harness/agy/NOTES.md) | 1.2.17; subagents on 1.3.1 | plan mode (`--mode plan` acts in the same turn) |
| ACP | [`src/harness/acp/NOTES.md`](src/harness/acp/NOTES.md) | claude-agent-acp 0.85.1, codex-acp 2.1.1, vibe-acp 2.26.1 | |
| ACP presets (gemini, opencode, goose, copilot, cursor, qwen, kiro) | [`src/harness/acp/NOTES.md`](src/harness/acp/NOTES.md) | not installed when added | unverified until recorded |
| Mistral Vibe | [`src/harness/vibe/NOTES.md`](src/harness/vibe/NOTES.md) | 2.26.1 | plan (#166), macOS (#175) |

Across harnesses:

- [Sandbox](docs/dev/sandbox.md): Landlock, what each CLI writes, Codex's
  own sandbox where ours does not run.
- [`ask` per harness](docs/dev/ask.md): Codex app-server's `untrusted`,
  pi's gate, agy's own interface.
- [Allow rules](docs/dev/allow-rules.md): one rule answering several
  harnesses, and the action read off each request.
- [Plan mode](docs/dev/plan-mode.md): driven on Claude only.
- [Questions](docs/dev/questions.md): Claude's AskUserQuestion and the
  modal.
- [Hooks](docs/dev/hooks.md): Claude Code's and Codex's hook events.
- [Headless `--print`](docs/dev/headless.md).
- [Bridging](docs/dev/bridging.md): handoff summaries, and sessions in
  place of one not resumed or forked.
- [Rewind and fork](docs/dev/rewind-and-fork.md) on each harness.
- [A harness's own slash commands](docs/dev/slash-commands.md), and
  sessions started before the first prompt.
- [Commands and subagents under `.agents/`](docs/dev/projections.md).
- [MCP servers](docs/dev/mcp.md).
- [Prompt input](docs/dev/prompt-input.md): file attachments, `@`
  references, `!command`.
- [Clipboard and copying](docs/dev/clipboard-and-copying.md): images,
  dropped files, herdr panes, copying from the transcript.
- [herdr](docs/dev/herdr.md).
- [`unharness update`](docs/dev/update.md).
- [Harness versions](docs/dev/versions.md): `min_version` and `doctor`.
- [Cost](docs/dev/cost.md): running totals reported per turn.

## Commit style

Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`), one logical
change per commit, tree green (fmt, clippy, tests) at every commit. A
release bumps the version and adds its notes to `CHANGELOG.md` (the steps
are under Releases in `CONTRIBUTING.md`).
