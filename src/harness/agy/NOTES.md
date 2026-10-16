# Antigravity (`agy`): status notes

Antigravity (`agy`, `src/harness/agy/`), recorded on agy 1.2.17 with an
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
`sleep` gone on Ctrl+C). A model, effort or policy change starts the
next turn's agy again with its flags, resuming the conversation; since
#286 the agy before is ended as a shutdown ends it (its input closed,
killed after `END_GRACE`) and the next starts once it is gone (before,
it was killed and the next started at once). How fast agy exits once
its input closes was not checked live. Sign-in is the token
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

## Sending a prompt now

agy has no interrupt (1.3.5 ignores `interrupt`, `steer` and `cancel` on
stdin, the issue's research): `/now` kills the process as an interrupt
does (`SendNow::AfterExit`), and once it has exited the prompt starts it
again on the conversation (`--conversation <id>`). Checked by controller
tests only, not live.

## Subagents

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

## Context fill

agy reports no context figure unharness could show, so it declares no
`context_usage` and the status row shows `ctx –` (#313). Looked for on
agy 1.3.5 (`strings` of the binary, no model call; first on 1.3.2,
which it updated itself to meanwhile) and in the recordings (1.2.17,
1.3.1): `step_update.usage` and `result.usage` carry
`input_tokens`, `output_tokens`, `thinking_tokens`, `cache_read_tokens`
and `total_tokens`, and nothing else (no window, no fraction);
`agy --output-format json models` lists only `id` and `label` per
model (checked on 1.3.2 and 1.3.5). The binary does know a window: a
`ContextWindowMetadata` from its backend (`EstimatedTokensUsed`,
`MaxContextTokens`), the `/context` command of its own interface
("Visualize current context usage"), and its status-line command's
JSON (`context_window`, `context_window_size`, `used_percentage`); none
of that is in `--output-format stream-json`, and print mode has no
context report among its own commands (`printmode` has reports for
usage, model, effort, config, credits, skills, hooks, agents and the
like, none for context). A step's `input_tokens` would say what one
model call sent, but without the window it is no fill. Unverified:
whether a later agy puts the window in `init` or `result`.

## The default model

agy names no model in `--output-format stream-json` (1.2.17, 1.3.1
recordings: neither `init` nor `result` has one), so with none chosen
the status line says `google (CLI default)` throughout (#329), and
`agy --output-format json models` (only `id` and `label`) marks no
default.
