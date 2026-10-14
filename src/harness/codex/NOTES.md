# Codex: status notes

## `app-server` and `exec`

Codex `app-server` is marked experimental by OpenAI; `transport = "exec"`
in `[harnesses.codex]` forces the fallback. Each `exec` turn is a
process of its own, and what it leaves behind is killed when it exits;
an `app-server` process keeps what it started until the session ends.
For a command's background job on 0.157.0 (pty runs, `sleep 123`) the
difference did not show with a `cmd &` one, which did not outlive its
command on either transport; a `setsid cmd &` one, which outlived
everything before the reaper, now ends with the turn on `exec` and
with the session on `app-server`. What else Codex keeps between turns
(MCP servers) is unverified.

`transport = "auto"` is resolved once per run (`app-server --help`), and
declares app-server's capabilities. A Codex without `app-server` runs
`exec`, whose session reports first what `exec` lacks of them, kept for
the run (`CapsUpdate::lacks` and `lacks_policies`: steering, compaction,
rewind, stopping a subagent, interactive permissions and `ask`; fork,
plan mode and questions as flags), so that the TUI stops offering them.
A `/compact` or rewind sent to `exec` all the same reaches no CLI: the
driver ends the compaction's turn with "codex cannot compact its context
on request" as its error, and answers the rewind with `RewindFailed`
("not supported"). `exec` branches no thread: a session asked to fork
starts new and says so (`SessionReplaced`) rather than resuming the
original. All of this was checked against `scripts/fake-harness.py`
alone, which fails `app-server --help`; no Codex without `app-server`
was run (#288).

## `turn/plan/updated`

Codex `turn/plan/updated` is parsed from the 0.157.0 app-server schema; no
recorded session offered the plan tool.

## Subagent threads

Subagent threads in Codex report on the main stream under their own
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

## Providers

Codex providers (0.157.0): the built-in `openai`, `ollama`, `lmstudio`
and `amazon-bedrock` cannot be redefined ("reserved built-in provider
IDs"; Bedrock takes only a few fields), an unknown `model_provider`
stops Codex at startup, and `config/read` lists the configured
`model_providers` (with `name`) and the `model_provider` in effect.
`-c model_provider="<id>"` chooses one on app-server and exec, and
`modelProvider` goes on `thread/resume` and `thread/fork` (schema; a
thread keeps its own provider otherwise);
`thread/start` and `thread/resume` answer with `modelProvider`, which
the driver reports and checks against the choice. A provider chosen
after a prompt has the app-server driver start the server again on it
(#275; before, the TUI did), its input closed and, once it is gone,
`thread/resume` of the thread with `modelProvider` (e2e, not run live);
on `exec` it goes on the next turn's command line. Either tells the TUI
`Restarted`. It is offered only between turns
(`Capabilities::between_turns`). `model/list` gave
OpenAI's catalog under every provider tried, so only `openai` lists
models; with no model chosen Codex took `gpt-6-astra` on `ollama` too.
Checked live: a turn through a custom `[model_providers]` entry
(`wire_api = "responses"`, a llama.cpp server) on app-server and exec.
Codex retried an unreachable Ollama without end. Unverified: what a
resumed thread does with a `modelProvider` other than its own,
`amazon-bedrock`, `lmstudio`, a provider defined only in a project's
`.codex/config.toml` (`config/read` is asked without a `cwd`).
