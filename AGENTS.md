# AGENTS.md

Project guidelines for AI coding agents working on unharness. `CLAUDE.md` and
`GEMINI.md` are symlinks to this file; edit this one.

## What this is

A Rust TUI/CLI that fronts vendor coding-agent CLIs (Claude Code, Codex, pi,
Antigravity) and any Agent Client Protocol agent through one event model. See
`README.md` for the architecture map.

## Commands

```bash
cargo build
cargo test                                  # unit, fixture replay, e2e (python3)
cargo clippy --all-targets -- -D warnings   # must be clean before a task is done
cargo fmt --check
UNHARNESS_UPDATE_FIXTURES=1 cargo test      # accept new parser output into .events files
```

## Invariants

- **Flag mapping lives only in `src/harness/<name>/`.** The TUI never builds a
  vendor command line. Before changing a flag, run the vendor's `--help` and
  confirm the flag exists on the installed version.
- **Capabilities, not assumptions.** The TUI reads `App::caps()` (the
  harness's declared `Capabilities` plus what its live session reported via
  `CapabilitiesChanged`) to decide what to offer. A harness that cannot do something declares it;
  `resolve_policy` picks the nearest *less* permissive supported policy and
  the warning is shown to the user.
- **Parsers are pure and fixture-tested.** `parse::feed(line) -> Vec<AgentEvent>`
  has no I/O. Every protocol change needs a recording made with the matching
  `scripts/record-*.py` and a committed `fixtures/<case>.jsonl` + `.events`.
  Fixtures are redacted (`/WORKSPACE`, `/HOME`, no emails or account ids);
  grep a new one for your user and org names before committing, because
  paths split across streaming deltas escape the recorders' redaction. A
  fixture used by an e2e test must have the same `>>` lines the driver sends.
- **Sync never destroys user files.** `sync/rules.rs` replaces a file with a
  symlink only when its content is identical; otherwise it warns.
- **No silent edits to vendor settings.** unharness does not write to
  `~/.claude`, `~/.codex`, `~/.gemini` or similar.
- **Processes are reaped.** Child processes go through `core::process::
  LineProcess` (kill-on-drop, bounded drain) or the per-turn driver; never a
  bare `tokio::process::Command::spawn` in a transport.

## Conversations

`core/conversations.rs` persists the merged transcript (`BlockRecord`), the
harness → vendor session map, bridging bookmarks, per-harness usage, and the
active harness, one JSON file per conversation plus an index. `App::persist`
runs on session start, turn end, harness switch, `/clear`, and quit. Resume
restores all of it; vendor sessions themselves live with the vendor.

## Testing conventions

- Unit tests sit in-module under `#[cfg(test)]`.
- Filesystem behaviour is tested against `tempfile::tempdir()`, never the real
  home directory.
- Transport drivers are tested end to end in `tests/session_e2e.rs` against
  `scripts/fake-harness.py`, which replays a fixture and blocks on every `>>`
  line until the driver sends something.

## Adding a harness

An agent that speaks ACP needs no code: a `[harnesses.<name>]` table with
`protocol = "acp"` and `command = [...]`, or an entry in
`harness::acp::PRESETS`. Record it with `scripts/record-acp.py` and add the
fixture to `src/harness/acp/fixtures/` if it behaves differently from the
recorded ones. For anything else:

1. Add a `HarnessId` constant in `src/core/ids.rs` (ids are interned names;
   the constant is only for built-ins).
2. Create `src/harness/<name>/{mod.rs, transport.rs, parse.rs, fixtures/}`
   implementing `Harness` and declaring honest `Capabilities`.
3. Record a fixture with a `scripts/record-<name>.py` and generate its
   `.events` with `UNHARNESS_UPDATE_FIXTURES=1`.
4. Register it in `Registry::from_config`.
5. Add an e2e test if the transport has a handshake or permission channel.

## Status notes

- Antigravity (`agy`, `src/harness/agy/`) is best effort: no account was
  available. Verified on agy 1.2.15 without credentials: `--print=` (empty)
  with `--input-format stream-json --output-format stream-json` parses, the
  `result` event shape (`fixtures/auth_required.jsonl`), and the `AGY_ERROR:`
  stderr marker. Unverified: the stdin message shape (Claude-compatible by
  default, `transport = "stream-prompt"` sends `{"prompt": …}`), the `init`
  and `step_update` field names (`fixtures/synthetic_turn.jsonl` is
  hand-written and says so), and `agy --output-format json models`. First
  thing to do with an account: `scripts/record-agy.py` from a scratch dir,
  replace the synthetic fixture, regenerate `.events`, fix the parser.
- Codex `app-server` is marked experimental by OpenAI; `transport = "exec"`
  in `[harnesses.codex]` forces the fallback.
- ACP (`src/harness/acp/`) is verified against
  `@agentclientprotocol/claude-agent-acp` 0.85.1 and
  `@agentclientprotocol/codex-acp` 2.1.1. The presets in `PRESETS` (gemini,
  opencode, goose, copilot, cursor, qwen, kiro) use launch commands from the
  ACP registry and were not installed when they were added: unverified until
  recorded. unharness declares no `fs`/`terminal` client capabilities, and
  uses only stable v1 methods; `session/load` (history replay) is implemented
  from the spec but only `session/resume` has been exercised.
- Codex `turn/plan/updated` is parsed from the 0.157.0 app-server schema; no
  recorded session offered the plan tool.
- Subagent threads in Codex report on the main stream under their own
  `threadId`; the parser and driver ignore their `turn/started` /
  `turn/completed` so they cannot end or re-target the main turn.
- Claude's `total_cost_usd` is a running total per process, and an ACP
  `usage_update.cost` is a running total per session; both parsers report the
  per-turn difference.

## Commit style

Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`), one logical
change per commit, tree green (fmt, clippy, tests) at every commit.
