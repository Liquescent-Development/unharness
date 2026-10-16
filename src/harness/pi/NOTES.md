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

## Context fill

The window comes with `get_state` at session start; what is used comes
from `get_session_stats` (`data.contextUsage.tokens`), which the
transport asks for once a turn has ended (`TurnCompleted`), so the
figure stays where it was through a long turn. Asking after every
`turn_end` too, which pi sends once per assistant response (one model
call and the tool calls it made, `docs/json.md`), would move it during
the turn. Read in pi 1.1.0's bundled source (#313), not run: the RPC
loop does not wait for a `prompt` to finish (`handleInputLine` is not
awaited and `prompt` returns once the run is accepted), so
`get_session_stats` is answered mid-run; it calls no model, and walks
the session's entries (`getSessionStats`, then `getContextUsage`'s
projection of the branch), which is linear in the session's length.
The assistant message is appended to the session at its `message_end`,
before the `turn_end`, so a request then would see it. The cost would
be a request and a response line per model call, and that walk in pi
each time. Not done: it changes what unharness sends to pi, which needs
a recording made with `scripts/record-pi.py` showing the answers
mid-turn, interleaved with the stream. It is worth doing as a follow-up once
so recorded: a long agentic turn is where the figure matters most, and
a request per model call costs no model call and little beside the
seconds a model call takes. `contextUsage.tokens` is `null`
after a compaction until an assistant response follows; unharness then
keeps showing the figure from before the compaction
(`ContextUsage::merge` keeps a known half), not fixed here.

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
