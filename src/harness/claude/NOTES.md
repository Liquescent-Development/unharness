# Claude Code: status notes

## Subagents

Subagents in Claude Code (2.1.289) run in the background unless the model
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

## Remote Control

Remote Control (#113, `/remote-control`, `/rc`), Claude only
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

## Models

Claude's models (2.1.292) are the `models` of its answer to
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

With no model chosen (#329), `system/init` names the one that runs
(`model`, the full id: `claude-haiku-4-5-20251001` in the fixtures,
2.1.289 to 2.1.296), but only once a prompt is sent, since Claude writes
its `init` with a turn, not at its start. Until then the status line
says `anthropic (CLI default)`, then `anthropic/<model> (default)`.
The `initialize` answer's `default` entry also says what it resolves to
(`resolvedModel`), which is not used; the picker marks no entry, since
that one's own name says it ("Default (recommended)").

## Providers

Claude providers (2.1.292, read in the binary and run): Claude picks its
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
The switch is in Claude's environment, so a provider chosen after a
prompt has the driver start Claude again on it (#275; before, the TUI
ended the session and started it): `interrupt`, its input closed, and
once it is gone `--resume` of the session with no model, greeted with
`initialize` and put back on Remote Control if it was on, the lines the
TUI's restart sent (`claude_starts_again_on_a_new_provider_resumed_and_back_on_remote_control`,
e2e, not run live); the TUI is told `Restarted`. It is offered only
between turns (`Capabilities::between_turns`). Since the TUI no longer
ends the session for it, the "Waiting for the previous Claude to exit"
flash is gone: the driver keeps what is sent meanwhile for the new
process, and a `Shutdown` meanwhile ends the session instead. Until
Claude has been started again (`Restarted`) the TUI holds prompts, and
still refuses a switch, resume, fork, rewind or `/clear`, now worded
"Claude is still taking the provider just chosen; try again in a
moment" (`Controller::settling`) where it was "the previous Claude is
still exiting", and refuses another provider, a model and an effort
the same way. A provider Claude does not have (`/provider nowhere`)
ends the session as a shutdown does, with the error, as the failed
start did before.
Unverified: a turn on any of them with credentials, `_MANTLE` beside
`_BEDROCK` (the binary treats Mantle as a Bedrock variant),
`CLAUDE_CODE_USE_GATEWAY` (left alone).

## Failed turns

An API or sign-in error ends a turn with a `result` of `subtype:
"success"` and `is_error: true` whose `result` is the error's text,
after an `assistant` message of Claude's own (model `<synthetic>`,
`is_api_error_message: true`, `error` the kind) with the same text
(recorded on 2.1.292, `fixtures/provider_vertex.jsonl`: Vertex without
credentials; an expired sign-in reported in #325). Read in the 2.1.300
binary: the result's other subtypes are `error_during_execution`,
`error_max_turns`, `error_max_budget_usd` and
`error_max_structured_output_retries`, which the SDK schema gives an
`errors` list and no `result` (not read yet), and `is_api_error_message`
marks "an assistant message [that] wraps an API error". So a subtype
leads the turn's error only when it starts with `error`; otherwise
`is_error` says it failed and the error is the result's text alone,
or "the turn failed" when it has none (before, the user read
`success: Failed to authenticate: …`). The synthetic message's text is
held for the result and dropped when the result repeats it, so the
error is shown once, as the error; it is shown as text when the result
says something else, when another line says something first, or when
Claude exits without a result (`on_exit`). A subagent's is shown as
text. Unverified: an `error_…` result live (none recorded), and a
synthetic message a result does not follow.

## Compaction

A compaction ends with a `system` `compact_boundary` whose
`compact_metadata` has `trigger` (`manual` or `auto`), `pre_tokens` and
`post_tokens`; the parser makes it the "context compacted" notice and,
from `post_tokens`, the context's new size (#310), which otherwise kept
the size from before the compaction until the next request's
`message_start`. Recorded for `/compact` only (2.1.288,
`fixtures/steer_and_compact.jsonl`: `pre_tokens` 22975, `post_tokens`
1988; the next turn was not recorded). Read in the 2.1.300 binary: the
SDK schema declares `post_tokens` optional (`pre_tokens` is not); one
function writes the line for every `compact_boundary`, whatever its
trigger, and leaves `post_tokens` out when it is unset; the compaction
both `/compact` and auto-compaction run (`isAutoCompact`) sets it, as do
reactive compaction (auto), a compaction a hook replaced and partial
compaction (manual). It is Claude Code's own estimate of what the kept
messages hold (counted from their content, not an API's usage), and its
own context size after a boundary with no assistant message since is
`postTokens`, else that estimate. It leaves out the system prompt and
the tools, so the next request's `message_start`, the real figure, is
expected to be well above it: a jump in ctx % on the first request
after `/compact` is that, not a fault. A boundary without it, or with
0, reports no size, so the last one stands. A subagent's compaction
writes no line: Claude forwards only a subagent's `assistant` and
`user` messages (read in 2.1.300, not run live), and the parser ignores
a subagent's `system` lines anyway (`feed_sub`), so a `compact_boundary`
is always the main agent's. Unverified live: an
auto-compaction's line (no recording), and how far the estimate is from
the next request's usage.

## Credentials and the relocated config

Claude keeps its login in a store named after
`CLAUDE_SECURESTORAGE_CONFIG_DIR` when that is set, else after
`CLAUDE_CONFIG_DIR` (read in the 2.1.300 binary, and in 2.1.296): on
macOS the Keychain entry is `Claude Code-credentials`, with
`-<first 8 hex of sha256(dir)>` appended once either names a directory;
the plaintext store is `<dir>/.credentials.json`, `~/.claude` when the
variable is empty. The relocation sets `CLAUDE_CONFIG_DIR=~/.claude`, so
on a Mac its Claude looked for an entry a plain `claude` never signs in
and signed in from a stale `~/.claude/.credentials.json`: "OAuth session
expired and could not be refreshed" on every turn while `claude` and
`doctor` (which checks without the relocation) were signed in (#324).
Checked on that Mac (2.1.300): `claude -p` worked, with
`CLAUDE_CONFIG_DIR=$HOME/.claude` it failed so, and with
`CLAUDE_SECURESTORAGE_CONFIG_DIR=` beside it it worked. So the
relocation sets that variable empty too, unless the user set it. On
Linux it changes nothing (`~/.claude` either way).

A user who set `CLAUDE_CONFIG_DIR` gets no relocation, and their
environment is passed on as it is: Claude then keeps its login where a
plain `claude` with the same environment does, on macOS the hashed entry
unless they also set `CLAUDE_SECURESTORAGE_CONFIG_DIR=`. Setting it empty
for them would split the two the other way when the variable is
exported in their shell, so `docs/sandbox.md` says so instead.

Both variables reach what Claude starts (the Bash tool, hooks, MCP
servers). Read in the 2.1.300 binary, not run: it removes them from a
subprocess's environment only when it scrubs that environment
(`CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`, on by default in GitHub Actions),
where they are in its list of variables naming credential and config
files; it passes both on to a Claude it starts itself (a teammate),
`CLAUDE_SECURESTORAGE_CONFIG_DIR` even when empty, the only variable it
passes on empty; and no settings file can set either. So a `claude` run from
a session uses the session's config and login, which is what it would
use in a shell that exports both; nothing to undo.

Unverified: a session in unharness on macOS with the fix, and a token
refresh there inside Seatbelt (the refreshed token written back to the
default Keychain entry from a confined Claude).
