# Headless `--print`: status notes

Headless `--print` (`src/headless.rs`, `docs/headless.md`) runs one
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
`Ctrl+C`. With `--resume`, a `SessionReplaced` (pi, an ACP agent; see
[Bridging](bridging.md)) fails the run at once (#214): exit 1, the reason on stderr
for `text` and in the result's `error`, a `session_replaced` line in
`stream-json` and nothing of the new session after it, `session_id`
`null`, and the session is shut down before the new one answers, as
Claude and Codex exit 1 on a session they do not have; without
`--resume` it is a notice. pi is never sent the prompt (the hold in
[Bridging](bridging.md)). Run only against `fake-harness.py` (`vibe_acp_load_missing`,
pi's `resume_missing`): there the ACP driver had not sent the prompt
either (it waits for the new session's mode), which is a race the
tests do not hold it to. `--native` and `--no-tui` are not covered.
Not persisted as a conversation.
