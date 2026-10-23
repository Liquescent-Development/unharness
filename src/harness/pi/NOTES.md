# pi: status notes

Besides its context fill (below), what was recorded and checked on pi is
in the notes that cross harnesses.

- [`ask` per harness](../../../docs/dev/ask.md): the gate (`gate.ts`,
  `gate.rs`), on pi 0.87.1 and 1.0.4.
- [Harness versions](../../../docs/dev/versions.md): `min_version` 0.78.1,
  and the deprecated `@mariozechner` package before 0.74.
- [Bridging](../../../docs/dev/bridging.md): a resumed session pi does not
  have (pi 1.1.0).
- [A harness's own slash commands](../../../docs/dev/slash-commands.md):
  `get_commands`, and a session started before the first prompt.
- [Rewind and fork](../../../docs/dev/rewind-and-fork.md): `fork` and
  `--fork <id>`.
- [Plan mode](../../../docs/dev/plan-mode.md): pi plans only through an
  extension.
- [Prompt input](../../../docs/dev/prompt-input.md): no document input,
  and `@path` passed on as text.
- [Commands and subagents under `.agents/`](../../../docs/dev/projections.md):
  `.pi/prompts/` expanded only under `--approve`.
- [Headless `--print`](../../../docs/dev/headless.md): run only against
  `fake-harness.py`.
- [Sandbox](../../../docs/dev/sandbox.md), [MCP servers](../../../docs/dev/mcp.md)
  (pi has no MCP client) and [herdr](../../../docs/dev/herdr.md) (pi's herdr
  and A2A extensions).

## The session's own count

`get_session_stats`' other fields (`userMessages`, `assistantMessages`,
`toolCalls`, `tokens`, `cost`) are read as `AgentEvent::SessionStats`
and shown under `/status`'s Usage, beside unharness's own sum: they
count the whole session file, so a session forked by a rewind counts
the kept turns again (`fixtures/rewind.jsonl`, pi 0.87.1: the
forked session's first stats are the kept turn's and the new one's).
Each names its session (`sessionId`, the id `get_state` gave; the forked
one's after the fork), and is kept with it. `tokens.total` is input,
output, cache read and cache write; `cost` is in dollars, the sum of its
responses' `usage.cost.total`. See [status](../../../docs/dev/status.md).

## Context fill

The window comes with `get_state` at session start. What is used is
told from three places (#355):

- Each assistant `message_end`: its `usage`, counted as pi counts it
  (`calculateContextTokens` in 1.1.0's bundled source: `totalTokens`, or
  input, output, cache read and cache write where that is missing or
  0). pi's own `getContextUsage` leaves out a response that was aborted
  or failed and one that counted nothing, and so does the parser. This
  moves the figure during a turn, once per model call, before the tools
  it asked for run (`fixtures/compaction.jsonl`: 1613 after the tool
  call's response, 1633 after the answer). Only where the provider
  reports usage: one that streams none (a model with pi's
  `compat.supportsUsageInStreaming: false`, or an endpoint that sends
  none) gives responses that count 0, and then the figure moves only
  when a turn ends, with `get_session_stats`, which pi then estimates
  from the messages' text.
- `get_session_stats` (`data.contextUsage.tokens`), which the transport
  asks for once a turn has ended (`TurnCompleted`). pi counts the last
  response's usage plus an estimate of what came after it, so this
  refines the figure the last response gave. A resumed session (also one
  forked at start) is asked as well as it starts, after its listing:
  the figure the conversation saved may be from before a compaction
  made since (by an earlier run, or by pi itself), and asked only once
  a turn ends it would be told only after the turn's first response had
  replaced it anyway (`fixtures/resume_compacted.jsonl`, pi 1.1.0: a
  session saved right after a `/compact`, resumed, answers `null`). A
  conversation resumed with a run that names another model than it ran
  gets back the fill counted on the model before: the controller drops
  the saved fill then (`restore_context`), but pi's answer is of the
  session, counted from its last response. Accepted: pi's own footer
  shows the same figure, it holds until the first response on the new
  model, and before this the same figure came at the end of the first
  turn anyway.
- A compaction that succeeds (`compaction_end` with `result`, whether
  `manual`, `threshold` or `overflow`): `result.estimatedTokensAfter`,
  pi's estimate of the context it rebuilt, told until a response gives
  the real figure, as Claude's `post_tokens` is (#310). One that failed
  (`errorMessage`) or was cancelled (`aborted`, no message) tells
  nothing. Recorded on pi 1.1.0 with a manual `/compact`
  (`fixtures/compaction.jsonl`, a local model, `keepRecentTokens` 1 in
  the workspace's `.pi/settings.json` so that two short turns are enough
  to compact): `tokensBefore` 1655, `estimatedTokensAfter` 1671 (more
  than before: the summary is longer than the one short turn it
  replaced), then 1862 after the next response. Threshold and overflow
  compactions are read from the same event and were not recorded; their
  `result` is built by the same code in 1.1.0's source. pi added
  `estimatedTokensAfter` in 0.79.8 (its CHANGELOG): from 0.78.1
  (`MIN_VERSION`) to 0.79.7 a compaction that succeeds has none, and
  the figure is dropped then (below) until a response counts it. An
  estimate of 0 is taken as none, as Claude's `post_tokens` is.

After a compaction pi answers `get_session_stats` with
`contextUsage.tokens: null` until a response follows (recorded: the
stats asked once the `/compact` ended; pi's `getContextUsage` gives
`null` while no projected response follows its latest compaction). What
the parser makes of it depends on what was told before (#319):

- The compaction's estimate, or a response's figure since, was told in
  this process: the answer is the window alone, and that figure stands
  (`ContextUsage::merge` keeps a known half). This is the case after a
  compaction unharness saw end, which is the one in the recording.
- Nothing was told of what pi holds now: a session resumed after a
  compaction an earlier run (or pi itself) made, a fork, a
  `compaction_end` without `estimatedTokensAfter` (said at once, there),
  or the estimate and then a response (not aborted, not failed) that
  reported no usage. That last one is a provider that streams none: pi
  can count no response after the compaction and answers `null` for the
  rest of the session, while the context grows past the estimate.
  Then the figure is of the context as it was before, and is dropped
  (`AgentEvent::ContextUnknown`): `ctx` leaves the usage line and
  `/status` says it is not known yet, until the next response. A resumed
  conversation whose saved figure was that compaction's estimate loses
  it too, since the parser of a new process cannot tell it from a stale
  one; it comes back with the next response. The resumed case is
  recorded (`fixtures/resume_compacted.jsonl`, and an e2e test drives
  it); the fork, no-estimate and no-usage cases are read from 1.1.0's
  source and pinned by parser tests, not run live.

Asking for `get_session_stats` after every `turn_end` as well was the
other way to move the figure mid-turn. Not done: it changes what
unharness sends to pi, a request and an answer per model call, and pi
walks the session's entries to answer each; the responses' own usage
gives the same figure without either. Read in 1.1.0's source (#313):
the RPC loop answers `get_session_stats` mid-run, so it would work.

## The default model

With no model chosen (#329), `get_state` at session start, before the
first prompt, names the one that runs (`data.model.id`, with its
`provider`: `gpt-5.6-sol` on `openai-codex` in the fixtures, 0.87.1),
shown as `<provider>/<model> (default)`. `get_available_models` marks no
default, so the picker marks none.

## Abort and the next prompt

`abort` during a turn (pi 1.0.4, `fixtures/gate_abort.jsonl`): the turn
ends (`agent_settled`, which the driver tells as the end) before `abort`
is answered; pi answers it once it is idle. A turn sent meanwhile (`/now`
sends the prompt once the turn's end is told) is held until that answer
(`Pi::aborts`; e2e `pi_sends_a_prompt_after_an_abort_only_once_it_is_answered`,
the fixture held at the answer), for 2 s at most (`ABORT_WAIT`): an abort
pi never answers (a dialog it waits in, #89) sends the turn then, with a
notice, and holds nothing after it (e2e
`pi_sends_a_prompt_held_for_an_abort_pi_never_answers`); an
`agent_start` of pi's own also ends the wait. Not checked live: a `prompt` sent before
the answer (whether pi refuses it or queues it).

Sandbox denials (`fixtures/sandbox_denied.jsonl`, 1.1.0, under a Landlock
ruleset like unharness's), each `isError`:
- write: `EACCES: permission denied, mkdir '<dir>'`.
- bash, run with `/bin/bash`:
  - `touch: cannot touch '<p>': Permission denied`;
  - `mkdir: Permission denied`;
  - `/bin/bash: line 2: <p>: Permission denied`.
