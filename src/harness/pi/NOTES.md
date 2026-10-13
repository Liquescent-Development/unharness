# pi: status notes

## `ask` per harness

`ask` per harness. Codex 0.157.0 app-server: `approvalPolicy:
"untrusted"` with `sandbox: "workspace-write"` (unharness's sandbox off)
asked before a file change and before every command, `cat` of a file
included (two recordings and a TUI run; not committed as a fixture
because Codex read the user's skill files into them despite the prompt).
The Codex CLI itself rejects `untrusted`: `-a` takes only `on-request`
and `never`, and `-c approval_policy="untrusted"` fails with "no longer
supported", which is why `exec`, `--print` and `--no-tui` have no `ask`
on Codex; only the app-server protocol still takes it. pi 0.87.1: the
gate (`src/harness/pi/gate.ts`, `gate.rs`) is a `tool_call` handler that
asks with `ctx.ui.select`, which arrives over RPC as an
`extension_ui_request` whose title carries the call as JSON
(`fixtures/gate.jsonl`); `-e` loads it beside the user's extensions and
under `--no-extensions`, from the state directory, under the sandbox.
Checked in the TUI: a command allowed always, a write denied (pi reports
the tool as failed with "Denied by the user"), the same command then
answered by the rule; a call from one of the user's own skills was asked
about too. In `--print` `ctx.hasUI` is false and the gate blocks. pi
passes a path as the model wrote it, often relative, and a rule does not
match a relative path. Read in pi 0.87.1's source, not run: before using
a path pi turns Unicode spaces into ASCII ones, strips a leading `@`,
expands `~` and reads `file://` URLs, so such a path is `Opaque`
(`gate::path_is_literal`); every extension's `tool_call` handler runs in
turn and may change the input after the gate saw it, and the gate knows
a read by its tool name alone. pi 1.0.4: two calls in one message are
asked about one after the other, each after its `tool_execution_start`
(`tool_call` handlers run in turn, `fixtures/gate_parallel.jsonl`); the gate passes the turn's
`ctx.signal` to its dialog, because without it `abort` while the
dialog is open went unanswered and the turn never ended (#89). With it
the call fails with "Operation aborted" and the turn settles as any
other (`fixtures/gate_abort.jsonl`), and pi sends nothing about the
dialog, so the transport withdraws it (`PermissionWithdrawn`) and
reports the turn interrupted. A request that arrives while a picker is
open waits until the picker closes. Unverified: the gate's human-readable dialog in
pi's own interface (`--no-tui`, `ctx.mode` other than `rpc`), tool
calls made by a pi sub-agent or an extension that runs tools without
`tool_call`, agy's own interface under `ask`.
