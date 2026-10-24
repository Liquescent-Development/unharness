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

## Interrupting and sending the next prompt

`turn/interrupt` during a command (0.162.1,
`fixtures/app_server_interrupt_then_prompt.jsonl`,
`record-codex-app-server.py --interrupt-after 3`): answered `{}`, then
`turn/completed` with `status: interrupted`, then the next prompt's
`turn/start` ran as a turn of its own. The driver tells the turn's end on
`turn/completed` only (a `turn/start` while the turn is active steers
it), so `/now` sends the prompt then (e2e
`codex_app_server_ends_an_interrupted_turn_on_its_turn_completed`). A
`turn/steer` sent while the command ran and interrupted 4 s later
(twice, recorded, not committed) was each time taken up first (a
`userMessage` item for it before the interrupt), so a steer dropped by
an interrupt was not reproduced. `exec`'s interrupt kills the turn's
process; the next turn resumes the thread.

## Compaction

`thread/compact/start` on 0.157.0 (`fixtures/app_server_steer_and_compact.jsonl`)
runs as a turn whose `contextCompaction` item starts and completes with
nothing but its id: no summary, no tokens, no trigger, and nothing of
what was kept. The parser tells its completion as a compaction with none
of those (`AgentEvent::Compacted`, #359), and `thread/compacted`, the
other notification app-server has for it (in the 0.162.1 binary, not
recorded), as the same, once per turn: the one it names, or else its
thread's turn in progress. The TUI shows a divider and folds nothing.

## Token usage

Codex counts cached input in its input: app-server's `inputTokens`
includes `cachedInputTokens` (`totalTokens` is `inputTokens +
outputTokens`), and so does exec's `input_tokens`. Both parsers take the
cached count out of `Usage::input` (#308).

`thread/tokenUsage/updated` carries `last` (the request just made) and
`total` (the thread's). The parser reports `last` as a request's usage,
for the turn's numbers and the context fill, and then `total` as the
session's (`cumulative`), which replaces the session's figure (#311).
Adding up `last` does not give `total`, in the recordings:

- compaction reports `last` with every count 0 but `totalTokens` (the
  compacted context, 5570) and leaves `total` as it was
  (`app_server_steer_and_compact.jsonl`);
- after `thread/revert`, `total` is the one at the turn reverted to plus
  the new turn's (32497, then 16238 + 16266 = 32504;
  `app_server_rewind.jsonl`): the dropped turn is no longer counted;
- a thread forked (`thread/fork`, `excludeTurns: true`) starts from the
  total of the thread it was forked from: its first `total` is 52729
  with `last` 19509 (`app_server_fork.jsonl`), and the source thread's
  last rollout ends at `total_token_usage` 33220 = 52729 − 19509. A
  second fork of the same thread also started at 52729.

A resumed thread carries on from its total too. Checked on the rollout
files Codex wrote for the exec recordings (0.157.0, thread `01a10023`):
the second turn, a `codex exec resume` in a process of its own, recorded
`total_token_usage` 33299 = 16234 (the first turn's) + 17065. That a
resume on app-server's `thread/resume` does the same is inferred, not
recorded: the server and exec share Codex's core, which rebuilds a
resumed or forked thread from its rollout (the 0.162.1 binary names
`core/src/session/rollout_reconstruction.rs`; its source was not read),
and a fork on app-server does carry its total. Also unrecorded: whether
`thread/resume` sends a `thread/tokenUsage/updated` before the first
turn. A saved conversation's figure is replaced by the first `total`
after a resume either way. A new thread's first `total` is its first
`last` (every recording), so a thread that replaces one that could not
be resumed takes the conversation's figure for Codex back to its own.

Exec's `turn.completed` `usage` is the thread's `total_token_usage` too,
not the turn's: in the same rollout, the resumed turn's `usage`
(`input_tokens` 50329, `cached_input_tokens` 41088, `output_tokens` 102,
`exec_resume_command.jsonl`) is the thread's total after it, while that
turn's two requests came to 34197. The exec parser still reports it as a
turn's (unharness then adds it to the session's), unfixed.

Unverified: whether `cacheWriteInputTokens` (always 0 in the
recordings) is also counted in `inputTokens`.

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

## The default model

With no model chosen (#329), app-server's `thread/started` names the one
that runs (`thread.model`, `gpt-6-astra` in `fixtures/app_server_*.jsonl`,
0.157.0), shown as `openai/<model> (default)`; `exec`'s `thread.started`
names none (`fixtures/exec_*.jsonl`), so an `exec` session stays
`openai (CLI default)`. `model/list` marks one entry `isDefault`
(`gpt-6-astra` in `fixtures/app_server_two_turns.jsonl`), which the
picker marks `(default)`; the fallback table, which `exec` lists, marks
none. That flag is the catalog's: a `model` in Codex's `config.toml`
is not reflected in it (unverified), so the label waits for the thread.
Unverified: whether `thread/resume` and `thread/fork` are followed by a
`thread/started` that names the model; no recording has either on
app-server, so a resumed or forked thread may say `openai (CLI default)`.

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

## Account and rate limits

App-server 0.157.0 sends `account/updated` (`authMode`: `chatgpt`,
`planType`: `prolite`) at start, and `account/rateLimits/updated` after
requests: `limitId` (`codex`), `primary` and `secondary` windows
(`usedPercent` 0 to 100, `windowDurationMins`, `resetsAt` in Unix
seconds; a null window keeps its last value), `credits`
(`hasCredits`, `unlimited`, `balance` as a decimal string, `62500` or
`62500.0000000000`), `planType` and `rateLimitReachedType`. The first
is read as `AgentEvent::Account`; the second as `RateLimit`, and as
`Account` (plan and credits) when those change. `exec` reports none of
it.

Asked on `/status` (0.162.1, `app_server_account_read.jsonl`, recorded
with `record-codex-app-server.py --read-account`):
`account/rateLimits/read` (no params) answers `rateLimits` in the
shape of the notification, plus `rateLimitsByLimitId`,
`rateLimitResetCredits` and `accountId` (redacted by the recorder);
`account/read` refuses a missing `params` (`-32600`) and with `{}`
answers `account` (`type`: `chatgpt`, `email`, `planType`) and
`workspaceRouting` (its `chatgptAccountId` redacted). They go under the
names `rate-limits` and `account`, which the parser reads; an error to
either turns `status_query` off for the session, quietly. Unverified: the
answers under an API key, and `account/usage/read` (lifetime and daily
token counts), not asked.

Sandbox denials on app-server (`fixtures/app_server_sandbox_denied.jsonl`,
0.162.1, under a Landlock ruleset like unharness's, the thread's sandbox
`danger-full-access`):
- **apply_patch:** it ends `failed` with no words, only its path in
  `changes[].path`. "Failed to create parent directories" goes to stderr
  only.
- **Commands:** `/usr/bin/zsh -lc '…'` carries zsh's output in
  `aggregatedOutput` (`zsh:1: permission denied: <p>`, `mkdir:
  Permission denied`).

Not recorded: `exec`.
