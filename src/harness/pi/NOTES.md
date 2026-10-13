# pi: status notes

pi has no note of its own: what was recorded and checked on it is in the
notes that cross harnesses.

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
