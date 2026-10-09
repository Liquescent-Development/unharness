# Changelog

What changed in each release of unharness. The section for a version is also the notes of its GitHub release.

## 0.7.0 - 2026-10-09

### Added

- Skills are `/name` commands on every harness: Codex's skills (on its `app-server` transport) and agy's (agy 1.1.11 or later) are in the `/` list and run from a slash, as Claude Code's and pi's already did.
- A harness's commands are in the `/` list before the first prompt: its session now starts as soon as a policy is chosen, not with the first prompt. agy still starts with the first prompt.
- A resumed conversation keeps the model and effort chosen for it.

### Fixed

- `--resume` uses the policy chosen for the conversation, not the configured default. A policy, model or effort named on the command line still wins. ([#3](https://github.com/Liquescent-Development/unharness/issues/3))
- A workspace settings file without `auto_sync` keeps the global value instead of turning sync back on. ([#1](https://github.com/Liquescent-Development/unharness/issues/1))
- A workspace's `[sandbox] readable` can reopen a path the global `deny_read` hides, by naming the same path; other workspaces keep it denied. See [Sandbox](https://github.com/Liquescent-Development/unharness/blob/main/docs/sandbox.md). ([#2](https://github.com/Liquescent-Development/unharness/issues/2))
- The status line always shows the harness and its policy, also in a very narrow terminal.

## 0.6.0 - 2026-10-08

### Added

- agy's subagents are shown as they run and report, and the `invoke_subagent` call ends with them (agy 1.3.1).
- On Linux each agent CLI runs below a reaper of its own, so what it starts in a session of its own or detaches ends with it instead of outliving unharness.

### Fixed

- Ending a session: the CLI's process group is killed before it is reaped, its pipes are closed once drained, and a session is resumed or forked only once the CLI that ended it is gone.
- Quitting says when it waits for an agent CLI, and Ctrl+C or Ctrl+D cuts the wait short. A quit signal while the prompt is in `$EDITOR` ends the editor first.
- A second interrupt ends a turn the CLI does not stop by itself, once you have been told it would.
- An interrupt sent before Codex's turn has an id is no longer lost.
- pi: an interrupt closes the permission dialog it has open instead of leaving the turn running.
- A permission request that arrives while a picker is open is shown when the picker closes.
- Ctrl+Z in `--print` stops the agent CLI too, and `fg` continues it.
- Code blocks in replies are drawn complete when a list, a quote or the end of the message closes them, following CommonMark.
- Question text and option descriptions are rendered as markdown.
- Claude Code: multi-select answers are joined the way Claude joins them, so labels with commas or quotes arrive intact.

## 0.5.0 - 2026-10-07

### Added

- `--print` runs one prompt through a session on every harness and writes text, a JSON result or `stream-json` lines in one schema; `--native` keeps the harness's own print mode. See [Headless](https://github.com/Liquescent-Development/unharness/blob/main/docs/headless.md).
- Claude Code on Bedrock, Vertex and Foundry, and Codex's built-in and configured model providers. Claude's model list now comes from Claude.
- Switching harness when the conversation will not fit: the harness being left writes a handoff summary, the bridge is measured by the context window of the harness it goes to, and older turns are told briefly before any is left out.
- The question dialog shows option previews and walks several questions with a submit page.
- Markdown tables are drawn as tables.
- Hook activity is shown in the transcript (Claude Code, Codex).
- Commands and subagents under `.agents/` are shared with Claude Code, pi and Codex. See [Skills](https://github.com/Liquescent-Development/unharness/blob/main/docs/skills.md).
- A harness's own slash commands are passed through, and the `/` list offers the ones the session reports.

### Fixed

- Ending a session stops what its CLI started, and a CLI that cannot be interrupted is killed when its session ends mid-turn.
- Tool calls are summarised without their raw JSON, and a long tool name keeps the call's status in view.
- A Claude Code subagent no longer stays at running after it has ended.
- Each key wakes the interface at once.

## 0.4.0 - 2026-10-06

### Added

- `unharness update` updates unharness when it was installed with the shell installer, and says what to run for Homebrew or cargo.

## 0.3.0 - 2026-10-06

### Added

- `!command` at the prompt runs a shell command in the session's sandbox; its output goes to the agent with the next prompt.
- In a [herdr](https://herdr.dev) pane, unharness reports whether the session is working, waiting for you or idle.

### Fixed

- In a herdr pane, copying goes through the terminal (OSC 52), and agent CLIs no longer get herdr's variables.

## 0.2.0 - 2026-10-06

First release: one interface for Claude Code, Codex, pi, Antigravity and any Agent Client Protocol agent, with switching mid-conversation, one sandbox for every agent, an `ask` policy that holds, rewind with file restore, and MCP servers, skills and `AGENTS.md` configured once.
