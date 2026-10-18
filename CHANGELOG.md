# Changelog

What changed in each release of unharness. The section for a version is also the notes of its GitHub release.

## 0.11.1 - 2026-10-18

### Fixed

- On macOS, Claude Code run in the sandbox can save a refreshed sign-in to the login Keychain. It could not, so it kept the refreshed token in `~/.claude/.credentials.json` while a plain `claude` used the old one from the Keychain, and Claude's server then revoked the login ("OAuth token revoked"), asking you to sign in again. The sandbox now lets Claude write only `login.keychain-db` and the files the Keychain writes beside it, and `unharness doctor` warns when your default keychain is another file. See [Sandbox](https://github.com/Liquescent-Development/unharness/blob/main/docs/sandbox.md#things-to-know).

## 0.11.0 - 2026-10-16

### Added

- Send a prompt now: `/now [text]`, or `Ctrl+Enter` where the terminal tells it from `Enter`, stops the running turn and sends the prompt as the next one; from an empty prompt box it sends the oldest queued prompt, and `Esc` while it waits takes it back. It works on every harness, and Claude Code is sent the prompt together with the interrupt. See [Sending a prompt now](https://github.com/Liquescent-Development/unharness/blob/main/docs/tui.md#sending-a-prompt-now).
- While prompts are queued during a turn, a line under them says how to send the first one now (`Ctrl+Enter` where the terminal can send it, `/now` elsewhere).
- `capabilities_changed` in `--print` and `--no-tui` can carry `take_back_sent_now`. See [Headless](https://github.com/Liquescent-Development/unharness/blob/main/docs/headless.md).

### Changed

- With no model chosen, the status line and the "Switched to" notice name the model the CLI says it runs, as `<provider>/<model> (default)`, or `<provider> (CLI default)` until it says; they used to show `anthropic/default`. An effort not chosen reads `CLI default`.
- The model picker marks the model a harness's list calls its default (Codex).

### Fixed

- An interrupted Claude Code turn is shown as "Turn interrupted." instead of an error.
- Choosing Claude's "Default (recommended)" model no longer shows it as `anthropic/default`.
- A harness without effort levels (agy) shows no effort in the status line, the switch notice, the footer, the model picker or `unharness models`.

## 0.10.1 - 2026-10-15

### Changed

- With `--print` or `--no-tui`, a Claude turn that fails with an API or sign-in error gives its message only as the error (stderr, or `error` in JSON), no longer also as the turn's text.

### Fixed

- On macOS, Claude Code run by unharness signs in with the same login as a plain `claude`. It looked for its login in a Keychain entry of its own and could fail every turn with "OAuth session expired and could not be refreshed" while `claude` worked. See [Sandbox](https://github.com/Liquescent-Development/unharness/blob/main/docs/sandbox.md#things-to-know).
- A Claude turn that fails with an API or sign-in error shows the error once, without a `success:` prefix.

## 0.10.0 - 2026-10-15

### Added

- `/usage` shows the billed tokens of the last turn and of the session, split into uncached input, output, cache reads and cache writes, with the cost, and says whether the harness never reports how full its context is or has not said yet. See [Tokens and cost](https://github.com/Liquescent-Development/unharness/blob/main/docs/tui.md#tokens-and-cost).
- The status line shows `ctx –` for a harness that never reports how full its context is (agy, Codex on `exec`), and leaves `ctx` out only while a harness has not said yet.
- Each harness's context fill and last turn's usage are kept with the conversation and come back on a resume or a switch back to it; until a session reports its model's window, the window that model last reported is used.
- Usage events of `--print` and `--no-tui` say what they cover in `scope` (`turn`, `request` or `session`). See [Headless](https://github.com/Liquescent-Development/unharness/blob/main/docs/headless.md).

### Changed

- A key or harness table that nothing reads is a config error naming it, where it used to be ignored: a misspelt key, another harness's key, `persist_sessions`, `command` or `display_name` on a harness that does not take them, a `protocol` other than `acp`, a table no harness reads, and an unknown Codex `transport`. `unharness doctor` lists them.
- The usage line keeps the last turn's numbers until the next turn reports, instead of going back to 0 when a turn starts; on Codex app-server and pi it counts up during the turn.
- A Codex app-server session's total is Codex's own, so it follows compaction, `/rewind` and `/fork` as Codex counts them.

### Fixed

- The usage line no longer reads `↑0 ↓0` after a turn that failed or reported no tokens, or after Claude's `/compact`.
- Codex's cached input is no longer counted twice in `↑`, the session total and `/usage`.
- `ctx %` drops after Claude's `/compact` instead of keeping the size from before, and starts again after a model change or a new session instead of showing the old figure.
- Codex on `transport = "auto"` that falls back to `exec` offers only what `exec` can do, and `/fork` there starts a new session.
- A `/compact` or `/rewind` the harness refuses no longer leaves the TUI waiting.
- On macOS, a CLI whose error output stayed open after it exited was never seen to end.
- A harness's CLI started again (agy on a new model, Codex `exec`'s next turn) starts only once the one before it has ended.

## 0.9.0 - 2026-10-13

### Added

- Mistral Vibe is a built-in harness: `unharness -H vibe` drives `vibe-acp` over the Agent Client Protocol, with its model and thinking level chosen as the session starts (never written into `~/.vibe/config.toml`), `--print --native` and `--no-tui` through vibe's own CLI, providers from `[harnesses.vibe.providers.<name>]`, and `doctor` showing its version, sign-in and whether vibe trusts the folder. See [Mistral Vibe](https://github.com/Liquescent-Development/unharness/blob/main/docs/vibe.md).
- `ask` and `accept-edits` on vibe are agent profiles of unharness's own (`unharness-ask`, `unharness-accept-edits`), so a tool that vibe's configuration lets through is still asked about; `auto` is vibe's smart-approve classifier and `bypass` its auto-approve.
- Vibe's subagents are listed as they are spawned and end with their report when vibe waits for one, and `/rewind` on vibe rewinds its session in place through its rewind extension.
- ACP agents are held to the session mode their policy maps to (`[harnesses.<name>] modes`); a mode the agent moves into by itself is set back, and an agent with modes and no map offers only `bypass`. See [Modes](https://github.com/Liquescent-Development/unharness/blob/main/docs/acp.md#modes).
- ACP agents: questions arrive as dialogs through form elicitation (vibe's ask_user_question, claude-agent-acp's AskUserQuestion), `/fork` branches the agent's session where it advertises `session/fork`, a session is closed with `session/close` before its process ends, and a request the agent withdraws closes its dialog.
- An ACP agent that is not signed in says which sign-in methods it offers and what to run (`[harnesses.<name>] sign_in`), and starts no session in place of the one it could not open.
- An ACP agent's effort can come from an option the harness declares, and select options sent in groups are read.
- Copying from the transcript takes the text without the drawing around it: a drag copies what was selected, and a right click copies a whole block, or a code block's code.

### Changed

- `doctor`, `switch` and `models` stop on a wrong harness table as a run does, instead of showing the built-in harness in its place; `doctor` names the table and exits 1.
- `--print --resume` of a session the agent no longer has (pi, ACP agents) fails with exit 1 instead of answering from a new session.
- Under `accept-edits` an ACP agent's edit is answered without asking only when it stays inside the workspace; an edit outside is asked about.
- A policy an ACP session refuses is offered again once that session ends.
- A session an agent starts in place of one it could not resume or fork is told the conversation so far.
- An interrupt answers the open permission requests before `session/cancel`, which keeps vibe's session usable afterwards.

### Fixed

- A policy changed while an ACP agent was still setting its mode could leave the agent in the earlier, wider mode; mode requests now go one at a time and a late answer sets the mode again.
- A `VIBE_TOOLS` setting in `$VIBE_HOME/.env` is covered by unharness's profiles, and a `.env` unharness cannot read refuses `ask` and `accept-edits` rather than trusting it.
- An ACP `execute` request carrying fields no recording had (such as `args`) is asked about instead of being matched by an allow rule.
- A permission request that names its tool call by id alone is read with that call's kind and input, and a call known only from its request ends when the request is answered.
- A vibe subagent whose end is never reported no longer holds back `/clear`, `/model`, `/effort` or a provider change.
- A right click after a bare diff hunk with tab-indented lines copies the code block it was on.
- A shell command of several lines is shown whole.

## 0.8.0 - 2026-10-10

### Added

- Plan is a fifth permission policy, the least permissive, for Claude Code: the agent plans read-only, and approving its plan picks the policy to carry it out under, or you keep planning with feedback. See [Permissions](https://github.com/Liquescent-Development/unharness/blob/main/docs/permissions.md#plan).
- `/remote-control` (`/rc`) puts a Claude Code session on Claude's Remote Control: the link is shown in the transcript and the state in the status bar, and a prompt typed remotely is shown as yours. ([#6](https://github.com/Liquescent-Development/unharness/issues/6))
- In a herdr pane, agents get herdr's pane context (`--current` and the herdr skill work), and an agent CLI's own herdr integration no longer reports over unharness. ([#4](https://github.com/Liquescent-Development/unharness/issues/4))
- `d` on a policy in the picker saves it as the harness's default, for this workspace or every workspace. A policy chosen in the TUI holds for the harness it was chosen on.
- `doctor` shows each harness's default policy and where it comes from, and warns about a harness version that is too old or that it cannot read, with what to run. ([#5](https://github.com/Liquescent-Development/unharness/issues/5))
- Running tool, subagent and `!` command rows show a spinner and the time so far.
- The mouse wheel scrolls the plan and a question's preview.

### Changed

- `/clear` saves the conversation and starts a new one with the same harness and choices, instead of emptying the transcript and keeping the vendor session.
- Pickers grey out, with the reason, a policy the harness lacks, a harness that is not installed and a sandbox level there is no backend for; `/policy`, `/switch` and `/sandbox` refuse them too. `--policy`, `UNHARNESS_POLICY` and `default_policy` still fall back.

### Fixed

- Claude Code's sync of organisation skills no longer ends a turn with "configuration changed". ([#7](https://github.com/Liquescent-Development/unharness/issues/7))
- `--resume` with `--policy` holds over every policy the conversation had, also when it names the one the conversation was started with.
- A permission dialog closes when Claude cancels its request (a hook answered it, or it was answered remotely).
- `doctor` reads the version of a pi that prints it on stderr, and pi's unknown-option error names the version unharness needs.
- The suggestion list grows to the prompt's width and shows a cut description in full below it.
- A drag a dialog cut short no longer keeps scrolling behind it.

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
