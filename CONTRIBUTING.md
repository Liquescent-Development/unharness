# Contributing

Thanks for your interest in unharness. Read [`AGENTS.md`](AGENTS.md) before
changing code: it lists the invariants every change must keep (flag mapping,
`ask`, sandboxing, fixtures, checkpoints) and what has been checked against
which vendor version.

## Development

Requires Rust 1.89 or newer and `python3` for the end-to-end tests.

```bash
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings   # must be clean
cargo test                                  # unit, fixture replay and e2e
UNHARNESS_UPDATE_FIXTURES=1 cargo test      # accept new parser output into .events files
```

`UNHARNESS_FRAME_LOG=<file>` makes the TUI write a line per frame: the input
and harness events taken since the last one, how long the input took, and how
long the frame took to lay out the transcript, to render and to draw.

## Architecture

```
src/core/      HarnessId/ProviderId/ModelRef, Capabilities + PermissionPolicy,
               AgentEvent, SessionHandle/SessionCommand, LineProcess,
               per-turn driver, JSON-RPC framing, Registry, SessionsStore,
               file checkpoints, allow rules (rules.rs), the sandbox
               (sandbox/: levels, profile, Landlock and Seatbelt backends)
               and the watch on vendor config (guard.rs)
src/harness/   one module per harness: descriptor, capabilities, probe,
               list_models, start_session, build_print_command, and what
               the sandbox must leave writable and watch for it
  claude/      stream-json transport + parser + fixtures/
  codex/       app-server + exec transports + parsers + fixtures/
  pi/          rpc transport + parser + fixtures/
  agy/         stream-json transport
  acp/         generic Agent Client Protocol client: one instance per configured agent
src/tui/       App state (pure), transcript blocks, modals, rendering, event loop
src/headless.rs  `--print`: one prompt through a session, written as text,
               json or stream-json (schema in docs/headless.md)
scripts/       record-*.py capture real vendor sessions; fake-harness.py replays
               them for tests/session_e2e.rs
```

Each vendor CLI's streaming protocol is parsed into one event model
(`AgentEvent`) by a pure parser, `parse::feed(line) -> Vec<AgentEvent>`,
that is tested against recorded sessions. The TUI reads only events and
declared `Capabilities`, so it never builds a vendor command line or
assumes what a harness can do.

## Recording fixtures

Every protocol change needs a real recording. Run the recorders from a
scratch directory:

```bash
scripts/record-claude.py out.jsonl "prompt"
scripts/record-acp.py out.jsonl "prompt" -- gemini --acp
```

Commit the recording as `fixtures/<case>.jsonl` next to its generated
`.events` file. Fixtures are redacted (`/WORKSPACE`, `/HOME`, no emails or
account ids). Grep a new one for your user and org names before
committing, because paths split across streaming deltas can escape the
recorders' redaction.

## Adding a harness

If the agent speaks ACP, a config table is enough; see
[ACP agents](docs/acp.md). Otherwise:

1. Add a module under `src/harness/<name>/` implementing `Harness`, with
   honest `Capabilities` and its `sandbox_paths`.
2. Record a fixture with a `scripts/record-<name>.py` and generate its
   `.events`.
3. Register it in `Registry::from_config`.
4. Add an e2e test if the transport has a handshake or a permission
   channel.

[`AGENTS.md`](AGENTS.md#adding-a-harness) has the full checklist.

## Commits and pull requests

Use conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`), one
logical change per commit, with fmt, clippy and tests green at every
commit.

## Releases

Releases are built by [dist](https://opensource.axo.dev/cargo-dist/). Pushing
a `v*` tag builds the Linux and macOS binaries, publishes a GitHub Release
with the shell installer, and updates the formula in
[Liquescent-Development/homebrew-tap](https://github.com/Liquescent-Development/homebrew-tap).
After changing `dist-workspace.toml`, run `dist generate` to refresh
`.github/workflows/release.yml`.

## License

By contributing you agree that your contributions are licensed under the
[AGPL-3.0-or-later](LICENSE).
