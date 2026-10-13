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
the driver reports and checks against the choice. `model/list` gave
OpenAI's catalog under every provider tried, so only `openai` lists
models; with no model chosen Codex took `gpt-6-astra` on `ollama` too.
Checked live: a turn through a custom `[model_providers]` entry
(`wire_api = "responses"`, a llama.cpp server) on app-server and exec.
Codex retried an unreachable Ollama without end. Unverified: what a
resumed thread does with a `modelProvider` other than its own,
`amazon-bedrock`, `lmstudio`, a provider defined only in a project's
`.codex/config.toml` (`config/read` is asked without a `cwd`).
