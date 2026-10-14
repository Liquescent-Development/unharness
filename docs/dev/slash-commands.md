# A harness's own slash commands: status notes

A harness's own slash commands (`Capabilities::slash_commands`,
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

## Sessions started before the first prompt

The TUI starts the active harness's session whenever it is idle with
none and a policy is chosen (launch, a switch, a resume, a sandbox
change, a provider changed before anything was sent), not with the first
prompt, so that the list is there
before it (#118). (A provider changed after a prompt is the driver's:
it starts its CLI again itself, #275.) Until a prompt goes to it the session is not the
conversation's (the field `App::session_blank`: nothing saved, its id held back,
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
