# Mistral Vibe: status notes

Mistral Vibe (`src/harness/vibe/`, #164): a built-in harness around
`AcpHarness` (`vibe-acp`; a `[harnesses.vibe]` table sets its command,
name, `asks_permission` and modes; `acp_profile` builds its
`AcpProfile`, with `VibeHooks` as its hooks, #271: unharness's
description of the agent, not one of vibe's agent profiles), with
`ask` → `unharness-ask` and `accept-edits` → `unharness-accept-edits`
(unharness's own profiles, #196, below), `auto` → `smart-approve`
(#174, below) and `bypass` → `auto-approve`, `thinking` its effort
option, and its model and level chosen at spawn
(`AgentHooks::chooses_at_spawn` and `spawn_plan`, `acp/spawn.rs`),
never with `session/set_config_option`: on vibe 2.26.1 that writes the
choice into `~/.vibe/config.toml` (`set_config_option` in
`vibe/acp/agent.py`).
The process gets `VIBE_ACTIVE_MODEL=<alias>` and, for a level, a whole
entry `VIBE_MODELS='[{"alias","name","provider","thinking"}]'`: the
environment layer validates each entry as a full model, so a sparse one
fails the session, and merges the given fields over the model's own
(`vibe/core/config/layers/environment.py`, `exclude_unset`). The name is
the model option's `description`, so a run's first process that has a
level and does not know it yet is started again once its session has
answered (`AgentHooks::learn` and `respawn_plan`), before anything is
sent (the second process is numbered from id 1 again). A fork learns it
from the answer of the source it reattached, before it forks, so that
only the process started again makes one (#222: the first process's
fork had been left in vibe's store, never closed; an e2e test replaying
`vibe_acp_fork.jsonl`, not run live). The provider is
read, never written, from the user's `config.toml` entry for the alias,
else `mistral`. With a level and no model chosen, the model the last
session ran is pinned with it: in a scratch `VIBE_HOME` the first
process ran `mistral-medium-3.5` and the next `glm-5-3`, the
experiment's choice having arrived meanwhile. A level outside
`off, low, medium, high, max` is never put in the entry.
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
with it, and after one the session is forgotten and a new one started,
told the conversation by the bridge with its budget and, over it, a
handoff summary from the old session as for a switch. Since #275 that is
the driver's to say: a model, effort or provider sent to a session that
was prompted is answered `NewSession` (`AgentHooks::chooses_at_spawn`,
the reason `vibe::KEEPS_CHOICES`), which the TUI hears as
`SessionRenewed` and renews from as for a switch (`App::session_renewed`:
the handoff summary asked of the old session, still live, then the
session forgotten and shut down); nothing is sent to vibe, and a prompt
typed before the answer waits for it. The choices are offered only
between turns (`Capabilities::between_turns`: all three for vibe). Where
no prompted session can say it, the static declaration does
(`Capabilities::stored_session_keeps`, one reader,
`App::stored_session_keeps`): a choice made with no vibe session live, or
with the live one a stored session started before the first prompt
(after `--resume`), leaves the stored session for a new one at once, and
none is made while the vibe before it is still exiting. It stays a flag:
it decides before a driver could (none is running), or for a session
the TUI would otherwise start again on the stored session, as it ran.
After every session
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

## The rest of the harness

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
`$VIBE_HOME/.env` (parsed as python-dotenv does, `${VAR}` from the
environment, presence only, a regular file of at most 64 KiB);
otherwise not known, since vibe also takes it
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
directory (`AcpProfile::relative_edits`, vibe only): with
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

## Doctor

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

## Trust

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

## vibe's own CLI

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

## Subagents

Subagents (#170, `vibe/subagents.rs`, an `acp::parse::MetaReader` the
vibe harness gives the ACP parser, `AgentHooks::meta_reader`, since
only vibe's own `_meta` says what such a call is): under vibe 2.26.1's
Unified runtime the model
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
saying that its end shows only when the agent waits for it. Its start
says so (`SubagentStarted::unreported_end`, set by vibe's reader; until
#275 a harness flag, `SubagentSupport::unreported_ends`, which the TUI
and `--print` both read), and such a row (`App::awaited_subagents`) does not keep
the TUI busy (status rule, herdr; its own spinner and time still run,
`has_running_subagents`) and holds back
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
never be sent: a subagent whose start says its end may go unreported
(`unreported_end`) is not waited for, so the run ends with the turn and
says how many subagents it did not wait for, and the session,
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

## Rewind

Rewind (#167, `vibe::REWIND_METHOD`, `AgentHooks::rewind_method`):
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

## Providers

Providers (#173, 2.26.1): a provider is `{name, api_base, api_key_env_var,
api_style, backend, …}` (`vibe/core/config/models.py`), and a model
names its provider, so the provider follows the model. Providers in the
user's `config.toml` are plain model choices (#164). Others come from
`[harnesses.vibe.providers.<name>]` in unharness's config (`api_base`,
`models`, optional `api_style`, `backend`, `api_key_env_var`, checked
by a run, `switch` and `models` with the table named,
`vibe::providers`; `doctor` shows no vibe then): one chosen
(`--provider`, `/provider`) goes to `vibe-acp` as `VIBE_PROVIDERS` (named
`unharness-<name>`, since vibe merges providers by name across layers,
`WithUnionMerge`), each model as a whole entry in `VIBE_MODELS` (alias
and name the model's, the level in the entry of the one that runs) and
`VIBE_ACTIVE_MODEL`, all built as JSON, since a value that does not
parse fails vibe's environment layer. Only the key's variable is named.
The session lists vibe's own models beside the provider's, and the
transport reports only the provider's, as its (`SpawnPlan::restrict`).
`multi_provider` is declared only with a provider defined, and only then
does a provider change start a session nothing was sent to again; a
change after a prompt starts a new bridged session, as a model change
does (`NewSession`, above). Recorded (`fixtures/vibe_acp_provider.jsonl`,
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

## `auto`

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

## Own profiles

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
vibe's home, both with links resolved, as vibe resolves the `VIBE_HOME`
it is given, so a linked `~/.vibe` moves where the walk stops, trusted
or not: it can only ask more) and `VIBE_TOOLS` /
`VIBE_TOOLS__<NAME>__…` in the environment or in `$VIBE_HOME/.env` name
(#212: vibe-acp loads `.env` into its environment before any
configuration layer, `load_dotenv_values`, so unharness parses it as
python-dotenv 1.2.2 does, `${VAR}` expanded in linear time from the
process's environment as unharness sets it, and takes those variables
alone from it, each that is not empty and whose key the environment
does not give a value that is not empty; parser checked against
python-dotenv's own output, not a live vibe; looked at before it is
opened: neither a file nor a FIFO is no `.env`, as for vibe, and a FIFO
is refused unopened, since opening a 1Password local env file has it
serve its content; values that expand past 1 MiB in all, as lines
that each name the one before twice do, refuse both policies and read
as no key for sign-in, where python-dotenv, and so vibe, would run out
of memory), `never` kept where any of them says it; bash gets vibe
2.26.1's default allowlist (`_get_default_allowlist`,
`builtins/bash.py`) without `git status` (which rewrites
`.git/index`), written into the file with that comment.
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
A config file or `.env` that is there and cannot be read (not regular,
over 1 MiB, not UTF-8 or TOML, `tools` not a table, a `VIBE_TOOLS` that
is not a JSON object; a FIFO `.env`, which vibe reads, too) or a state
directory that cannot be written refuses both policies with the reason
(`SpawnPlan::own_modes`), never a degraded profile. Every vibe process
(`vibe-acp` and vibe's CLI) is given `VIBE_HOME`, the home unharness
read: vibe resolves its home from the variable on every access, after
`load_dotenv_values`, so a `VIBE_HOME` written into `.env` (writable in
the sandbox) would otherwise move the next process to a `config.toml`
unharness never read; a `.env` that would still set it (the session's
environment setting it empty) refuses both policies. Profiles are found
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

## Acceptance

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
