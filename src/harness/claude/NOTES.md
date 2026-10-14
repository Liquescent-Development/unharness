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
flash and the refusal of a switch, resume, fork, rewind or `/clear`
while the old process exits are gone: the driver keeps what is sent
meanwhile for the new process, and a `Shutdown` meanwhile ends the
session instead. Until Claude has been started again (`Restarted`) the
TUI holds prompts, and refuses another provider and what would end the
session ("is still taking the provider just chosen"). A provider Claude
does not have (`/provider nowhere`) ends the session as a shutdown does,
with the error, as the failed start did before.
Unverified: a turn on any of them with credentials, `_MANTLE` beside
`_BEDROCK` (the binary treats Mantle as a Bedrock variant),
`CLAUDE_CODE_USE_GATEWAY` (left alone).
