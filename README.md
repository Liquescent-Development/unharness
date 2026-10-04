# unharness

One terminal UI for every AI coding agent you have an account for.

unharness wraps the vendor CLIs (`claude`, `codex`, `pi`, `agy`) and any agent
that speaks the [Agent Client Protocol](https://agentclientprotocol.com) behind
a single ratatui interface with the same transcript, keybindings, permission
prompts, model/effort pickers and session resume regardless of which agent is
running. Switch agents mid-conversation and the context follows you.

## Why

Agent SDKs churn; the vendor CLIs are the stable surface. Each one already
speaks a streaming JSON protocol with a permission channel:

| Harness | Transport | Interactive permissions | Resume | Live models |
|---|---|---|---|---|
| Claude Code | `claude -p --input-format stream-json` (long-lived) | yes, incl. AskUserQuestion | `--resume` | static list |
| Codex | `codex app-server` JSON-RPC (long-lived), `exec --json` fallback | yes (app-server) | thread id | `model/list` |
| pi | `pi --mode rpc` (long-lived) | extension dialogs | `--session-id` | `get_available_models`, many providers |
| Antigravity | `agy --print= --input-format stream-json` (long-lived, unverified: no account yet) | no | `--conversation` | `agy models` |
| ACP agents | Agent Client Protocol v1 over stdio (long-lived) | yes, when the agent asks | `session/resume` or `session/load` | from the session |

unharness normalises those into one event model and one capability set, so
the TUI never assumes what a harness can do. Anything a harness lacks is shown
as a degraded capability rather than failing silently.

What each harness supports beyond a plain turn:

| | Claude Code | Codex (app-server) | pi | ACP agents | Codex exec, Antigravity |
|---|---|---|---|---|---|
| Image attachments | yes | yes | per model | per agent | exec only |
| Plan / todo list | yes | yes | no | yes | exec only |
| Subagents: start, end, tool calls, report | yes | yes | no | no | no |
| Subagents: task description and progress in words | yes | no (a name and its tool calls) | no | no | no |
| Subagents: stop one | yes | yes | no | no | no |
| Steer a running turn | yes | yes | yes | no (queued) | no (queued) |
| Compact on request | yes | yes | yes | no | no |
| Context-window gauge | yes | yes | yes | yes | no |
| Rate-limit windows | yes | yes | no | no | no |
| Native rewind | yes | yes | yes | no (fresh session) | no (fresh session) |
| Native fork | yes | yes | yes | no (fresh session) | no (fresh session) |

## Install

```bash
cargo install --path .
unharness doctor          # harnesses, auth, capabilities, skills CLI, rules
```

Skills are managed by the [`skills`](https://github.com/vercel-labs/skills)
CLI (`npx skills`), which needs Node.js.

## Use

```bash
unharness                                   # TUI with the default harness
unharness -H codex "Refactor the parser"    # pick a harness, start with a prompt
unharness --policy accept-edits             # ask | accept-edits | auto | bypass
unharness -y                                # alias for --policy bypass
unharness --resume                          # resume the latest conversation (all harnesses in it)
unharness --resume 01a1                     # by id prefix; -H overrides which harness continues
unharness -p "Summarise src/"               # headless print mode
unharness --no-tui                          # drop into the vendor's own TUI
unharness models                            # providers and models per harness
unharness sessions                          # recorded sessions in this workspace
unharness skills add vercel-labs/agent-skills
```

Without `-H` and without `default_harness` in the config, unharness takes
the first installed harness that is signed in, in the order Antigravity,
Claude Code, Codex, pi, then ACP agents. Antigravity, Claude Code and Codex
can say so quickly; pi and ACP agents cannot (pi needs a model query, an ACP
agent a session), so they are chosen only when none of those three is signed
in, and ahead of one that is known to be signed out. `unharness doctor` shows
the result as "Active Default".

### In the TUI

The transcript fills the window. Everything about the current state sits
under the prompt: a status rule (what the agent is doing and for how long),
the prompt box (it grows with what you type, up to eight rows, then
scrolls), then the working directory and git branch, the harness,
provider/model, effort and policy, token usage and cost, the session id, any
capability caveat, and the key hints.

| Key | Action |
|---|---|
| `Enter` | Send the prompt; during a turn, queue it for when the turn finishes |
| `Alt+Enter` | Steer: send the prompt into the running turn (queued where the harness cannot) |
| `Ctrl+J`, `Shift+Enter` | New line in the prompt. `Ctrl+J` works everywhere; `Shift+Enter` only on terminals with the kitty keyboard protocol (see below) |
| `Up` / `Down` | Move between the prompt's lines; on its first / last line, step back / forward through prompts sent from this workspace |
| `Home` / `End`, `Ctrl+A` | Start / end of the current line (`Ctrl+A` is start only) |
| `Ctrl+U` | Clear the prompt |
| `Ctrl+G` | Edit the prompt in `$VISUAL` / `$EDITOR`; what the editor saves comes back into the prompt, unsent |
| `PageUp` / `PageDown`, `Shift+Up` / `Shift+Down` | Scroll the transcript by ten / two lines |
| Mouse wheel | Scroll the transcript |
| Scrollbar (right edge of the transcript) | Drag the thumb, or click the track, to move through the transcript |
| `↓ Jump to bottom` | Shown while scrolled up; click it to return to the end and follow new output again |
| Drag, double click, triple click | Select transcript text, a word (paths stay whole) or a row, and copy it on release |
| `Alt+Up` | Pull the last queued prompt back into the prompt box |
| `Ctrl+H` | Harness picker |
| `Ctrl+M` | Model picker (provider picker first on multi-provider harnesses) |
| `Ctrl+E` | Reasoning effort picker (levels come from the harness) |
| `Ctrl+P` | Permission policy picker |
| `Ctrl+R` | Resume a saved conversation |
| `Ctrl+O` | Expand or collapse the last tool call's output |
| Click on a tool call | Expand or collapse that call; on a call that spawned a subagent, open the subagent's transcript |
| `Ctrl+T` | Expand or collapse every tool call |
| `Down` past the prompt's last row | Into the list of subagents under the prompt: `Up`/`Down` choose one, `Enter` opens its own transcript, `Delete` takes a finished one off the list, `Esc` (or typing) returns to the prompt |
| `Ctrl+S`, `/subagents` | Choose among all the conversation's subagents and open one's transcript |
| In a subagent's transcript | `Esc` back to the conversation · `s` stop that subagent · `Tab` / `Shift+Tab` next / previous subagent · arrows, `PageUp`/`PageDown`, `End` scroll · `Ctrl+O`, `Ctrl+T` expand its tool calls |
| `Esc` | Interrupt the running turn, else clear the prompt (never quits) |
| `Ctrl+D`, `/quit` | Quit (`Ctrl+C` also quits when idle) |

Slash commands: `/harness` (alias `/switch`), `/provider`, `/model`, `/effort`, `/policy`,
`/resume`, `/sessions`, `/usage`, `/plan`, `/subagents`, `/attach <image>`, `/detach`,
`/steer <text>`, `/compact [instructions]`, `/rewind`, `/undo-restore`,
`/fork`, `/skills`,
`/clear`, `/help`, `/quit`.
Typing `/` opens autocomplete; Enter on a partial command completes it.

Most terminals send the same byte for `Enter`, `Shift+Enter` and `Ctrl+M`,
so on those `Shift+Enter` sends the prompt and `Ctrl+M` cannot open the model
picker (use `/model`). Terminals that implement the kitty keyboard protocol
(kitty, foot, Ghostty, WezTerm, Alacritty, recent iTerm2 among them) are
asked to tell the keys apart, and there both work as listed.

Sent prompts and commands are remembered per workspace (the newest 500, in
`.unharness/prompt_history.jsonl`, removed by `unharness sessions --clear`).
`Up` on the prompt's first line recalls them; whatever you had typed comes
back when you step `Down` past the newest. While the `/` autocomplete list is
open the arrows move in it instead.

unharness takes the mouse, so the terminal's own selection is replaced by
its own: drag in the transcript to select, and the text is copied when you
let go (the status rule says so). Dragging past the top or bottom edge
scrolls. The copy goes through `wl-copy`, `xclip`, `xsel` or `pbcopy` when
one is there, and otherwise, or over ssh, through the terminal (OSC 52),
which some terminals ignore and tmux only passes on with `set-clipboard on`.
What is copied is the rows as drawn, so a wrapped paragraph comes out with
its line breaks and indent. Holding Shift while dragging gives the
terminal's selection back in most terminals, and `mouse = false` in the
config turns all of this off.

Pasted text goes into the prompt as it is, newlines included, and is never
sent until you press Enter (this relies on the terminal's bracketed paste,
which every current terminal has). A paste while a dialog is open goes into
the dialog's text field if it has one and is otherwise ignored.

Above the prompt, when there is something to show: the agent's plan as a
checklist (`/plan` hides it), prompts waiting in the queue, and images
attached to the next prompt. The usage line shows how full the model's
context is (`ctx 34%`); `/usage` adds the account's rate-limit windows, and a
notice appears when one passes 80%.

`/rewind` lists your earlier prompts. Picking one removes it and everything
after it from the conversation and puts its text back in the prompt box.
Where the harness can, its own session is rewound (it then genuinely no
longer knows the removed turns), whether or not it is running at the moment;
otherwise it starts a fresh session with the remaining conversation as
context, and the picker says which will happen. If a harness reports that it
could not rewind, unharness falls back to the fresh session by itself.

Press `f` instead of Enter in the picker to also put the files back to how
they were before that prompt. For a project that is a git repository,
unharness checkpoints the working tree before every prompt: tracked and
untracked files, not ignored ones. The checkpoints go into a shadow
repository of unharness's own, under your state directory
(`~/.local/state/unharness/checkpoints/<project>-<hash>.git` on Linux), which
uses the project as its working tree. Nothing is written to the project's
own `.git`. The restore works for any harness, since it does not depend on
the agent, and `/undo-restore` reverses it. Changes outside the project are
not covered. The first checkpoint copies the project's files once; later
ones store only what changed. `file_checkpoints = false` turns checkpoints
off; `unharness sessions --clear` deletes the project's shadow repository.

`/fork` continues in a copy of the conversation and leaves the original as it
is. Harnesses that can branch a session do, so the copy knows exactly what
the original knew; the others start fresh with the transcript as context.

Subagents are shown as long as they run, which can be longer than the turn
that started them (Claude Code launches them in the background by default,
and a Codex sub-agent keeps going when the agent does not wait for it).
They are listed under the prompt, each with its task, how long it has run
and what it is doing: those at work first, then those that have ended,
latest first. An ended one stays listed, so that what it did can still be
read, until it is taken off with `Delete` (its transcript is kept, and
`Ctrl+S` still offers it). While any is at work the status rule stays busy ("2 subagents
running") instead of "Ready".

The main transcript stays the main agent's: a subagent is one line there,
the call that spawned it, showing whether it is running, done, failed or
stopped. What it does and writes (its tool calls, its prose, its final
report) is a transcript of its own. `Down` from the prompt moves into the
list under it and `Enter` opens the chosen subagent's transcript; `Ctrl+S`
or `/subagents` offers every subagent of the conversation, and a click on
its line in the transcript works too. A subagent's own subagents are in its transcript in the same way.
A subagent is stopped only from its own transcript, with `s`; `Esc` there
just goes back. A prompt sent while subagents run goes straight to the main
agent. All of it is saved with the conversation.

Claude Code starts a turn of its own to report when a background subagent
ends or is stopped; Codex does not, so there its report is only in its own
transcript. Codex names a sub-agent but does not describe its task, and
says what it is doing only through its tool calls.

Policy, model and effort changes apply to the next turn on every harness
(Claude via its control channel, Codex per `turn/start`, pi per RPC command,
Antigravity by restarting its process on the same conversation).

Tool calls render as blocks: shell output wrapped in a gutter, file edits as
syntax-coloured red/green replacements, reads highlighted by file type, and
unified diffs in red/green. Fenced code in answers is syntax highlighted and
wrapped, never cut off.

When a harness asks for permission, a modal opens: `y` allow once, `a` allow
always (when the harness offers a rule), `n` deny with a reason, `i` show the
full tool input. Agent questions (Claude's AskUserQuestion, Codex's
requestUserInput, pi's extension dialogs) open the matching modal.

### Permission policy

| Policy | Claude Code | Codex (app-server) | Codex (exec) | pi | Antigravity |
|---|---|---|---|---|---|
| `ask` | default mode, prompts in the TUI | `untrusted` + read-only sandbox, prompts in the TUI | read-only sandbox, cannot prompt* | only extension dialogs prompt* | soft-denies tools that would prompt* |
| `accept-edits` | `acceptEdits` | `on-request` + workspace-write | workspace-write | falls back to ask* | `--mode accept-edits` |
| `auto` | `auto` (classifier) | `on-request` + workspace-write | `--approve-for-me` | falls back to ask* | falls back to accept-edits* |
| `bypass` | `bypassPermissions` | `never` + full access | `--dangerously-bypass-approvals-and-sandbox` | dialogs auto-accepted* | `--dangerously-skip-permissions` |

`*` shown as a warning in the header. A requested policy a harness cannot
honour falls back to the nearest *less* permissive one it supports.

### Sandbox

unharness confines every harness process itself, whichever agent runs and
whatever it chooses to ask about. The process and everything it starts (shell
commands, MCP servers) can:

- **write** only inside the workspace, the harness's own state directories
  (`~/.claude`, `~/.codex`, `~/.pi`, …; `unharness doctor` lists them) and
  the temp directories (and herdr's runtime directory);
- **read** everything except credential locations: `~/.gnupg`,
  `~/.aws`, `~/.azure`, `~/.kube`, `~/.docker`, `~/.config/gh`,
  `~/.config/gcloud`, `~/.netrc`, `~/.npmrc`, `~/.pypirc`,
  `~/.git-credentials` and shell history;
- use the network freely.

| Level | Writes |
|---|---|
| `workspace-write` (default) | workspace, harness state, temp |
| `read-only` | harness state and temp only |
| `off` | unconfined |

Set it with `--sandbox <level>`, `UNHARNESS_SANDBOX` or `[sandbox] level`.
The default applies under every policy, `bypass` included; Codex `exec` under
`ask` defaults to `read-only` because it cannot prompt. The level is on the
status line and is fixed for a run.

On Linux this is Landlock (kernel 6.2 or newer, no extra binary); on macOS
the process is launched through `sandbox-exec`. Where neither exists the
default degrades to `off` with a warning in the status area, in `doctor` and
on stderr in print mode, and an explicitly requested level is an error.

Things to know:

- The vendors' own sandboxes cannot start inside this one, so while it is
  active Codex is told the sandbox is external (its approvals are
  unchanged). With `--sandbox off` Codex's sandbox follows the policy as in
  the table above.
- `~/.ssh` stays readable, since git over ssh and commit signing need it;
  keep keys in an agent or protect them with a passphrase if that matters.
- An MCP server or extension that keeps data elsewhere needs its directory
  in `writable`.
- Claude Code updates `~/.claude.json` by creating files next to it in the
  home directory, which the sandbox cannot allow without opening the whole
  of it. unharness therefore runs Claude with `CLAUDE_CONFIG_DIR=~/.claude`,
  which puts the file inside its state directory. Your `~/.claude.json` is
  copied there on the first run (a notice says so) and otherwise left
  alone. A plain `claude` outside unharness keeps using the original, so
  the two drift apart unless you export the same variable in your shell.
  `relocate_config = false` under `[harnesses.claude]` turns this off:
  sessions still work, but "always allow" rules, trust and MCP approvals
  given in a confined session are then forgotten after it.
- A confined Claude Code cannot update itself: its installed binaries are
  not writable.
- A harness's own state directory has to stay writable, and its settings
  live there (`~/.claude/settings.json`, `~/.codex/config.toml` and the
  Codex binary, `~/.pi/agent/settings.json`, skills, hooks, MCP servers).
  The sandbox cannot stop an agent editing those, so unharness watches
  them: when one changes during a turn, the transcript (or stderr in print
  mode) says which, and the version from before the session is saved under
  `~/.local/state/unharness/guard/`. It cannot tell the CLI's own change
  (saving an "always allow" rule, say) from the agent's. `--no-tui`
  passthrough is not watched.
- A file that is replaced or created directly in the home directory or in
  `~/.config` after the process started is not readable by it until the
  next session (Linux).
- In a git worktree the repository's data lives outside the workspace;
  add the main repository's `.git` to `writable` to commit from the agent.
- Programs that need to raise privileges (`sudo`) do not work inside
  (Linux).
- unharness's own configuration, the workspace's included, is kept in
  `~/.config/unharness`, which is never writable from inside: an agent
  cannot change the settings its next run starts with.

### Switching harnesses

`/switch` shuts the current session down and starts the next harness lazily on
your next prompt, seeding it with the conversation so far (capped by
`bridge_max_chars`, default 24k characters, keeping the tail). Returning to a
harness resumes its own session and bridges only what happened since.

## Configuration

Settings come from `~/.config/unharness/config.toml`, with the workspace's
overrides merged over it. The overrides are kept outside the workspace, in
`~/.config/unharness/workspaces/<name>-<hash>.toml` (`unharness doctor`
prints the path, `unharness init` creates it), because a file inside the
workspace could be edited by the agent it is meant to configure. An
`unharness.toml` left in a workspace from an earlier version is not read;
`unharness init` imports it once.

```toml
default_harness  = "claude"      # agy | claude | codex | pi
default_policy   = "ask"
mouse            = true          # wheel, scrollbar, jump-to-bottom, drag to select and copy; false leaves the mouse to the terminal
auto_sync        = true          # refresh CLAUDE.md/GEMINI.md symlinks before each run
bridge_max_chars = 24000
file_checkpoints = true          # snapshot the working tree before each prompt (git projects; kept outside the repo)

[sandbox]
level    = "workspace-write"     # read-only | workspace-write | off
writable = ["~/.local/share/my-mcp"]  # extra writable paths (relative ones are under the workspace)
readable = ["~/.config/gh"]      # credential paths to allow reading

[harnesses.claude]
# binary = "/path/to/claude"
default_model  = "opus"
default_effort = "high"
default_policy = "accept-edits"
extra_args     = []
sandbox_writable = []            # extra paths this harness may write inside the sandbox
relocate_config  = true          # keep .claude.json in ~/.claude so a sandboxed Claude can update it (see Sandbox)

[harnesses.codex]
transport = "auto"               # auto | app-server | exec

[harnesses.agy]
transport = "stream"             # stream | stream-prompt | per-turn (see AGENTS.md)

[harnesses.pi]
default_provider = "openai-codex"
default_model    = "gpt-5.5"
```

### Conversations

A conversation is the unit of resume. unharness saves it under
`<workspace>/.unharness/conversations/` (ignored by `unharness init`): the
merged transcript, the vendor session id of every harness that took part, the
bridging bookmarks, and which harness was active. `unharness --resume`,
`/resume`, and Ctrl+R restore the transcript into the pane and reattach each
harness to its own vendor session when you `/harness` to it, bridging only
what that harness has not seen. `unharness sessions` lists saved
conversations; `--clear` removes them.

### ACP agents

Any agent with an [Agent Client Protocol](https://agentclientprotocol.com)
mode can be added from config, without a dedicated adapter:

```toml
[harnesses.claude-acp]
protocol     = "acp"
command      = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
display_name = "Claude (ACP)"
sandbox_writable = ["~/.claude", "~/.npm"]   # the agent's own state, for the sandbox
```

The table name is the harness id (`unharness -H claude-acp`, `/harness
claude-acp`). Gemini CLI, OpenCode, Goose, GitHub Copilot, Cursor Agent, Qwen
Code and Kiro are added automatically when their binary is on PATH, using the
launch commands from the ACP registry; those presets have not been run here
yet, and `unharness doctor` says so. A table of the same name overrides a
preset.

The permission policy is applied by unharness, so it means the same for every
ACP agent: `ask` shows each request, `accept-edits` answers file edits itself,
`bypass` answers everything, `auto` falls back to `accept-edits`. An agent
only asks for what it chooses to ask for, which is shown as a caveat on `ask`.
Models and effort levels come from the running session, so the pickers fill
in after the first prompt. ACP agents have no print or `--no-tui` mode, and
sign-in is done with the agent's own CLI.

## Rules and skills

`AGENTS.md` is the canonical instructions file; `CLAUDE.md` and `GEMINI.md`
are symlinks to it (`unharness init` / `unharness sync`). A `CLAUDE.md` whose
content differs from `AGENTS.md` is never overwritten.

Skills live in `.agents/skills/` and are installed and projected into every
agent's directory by `unharness skills <args...>`, a passthrough to `npx
skills`. The one exception is `import`, which is unharness's own:

```bash
unharness skills import                    # pick from skills your harnesses already have
unharness skills import --from plugins -g  # only Claude plugin-bundled skills, globally
unharness skills import --all --dry-run    # show what would be imported
```

It scans `~/.claude/skills` (and synced buckets), Claude plugin caches,
`~/.codex/skills` (`--include-system` for the Codex built-ins), the
Antigravity and pi skill directories, and project-level `.claude`, `.codex`
and `.pi` skill dirs, then installs the chosen ones through `skills add
<path>` so they land in `.agents/skills` and get projected everywhere.
Plugins and pi extensions are harness-specific; `unharness doctor` lists
them instead.

## Architecture

```
src/core/      HarnessId/ProviderId/ModelRef, Capabilities + PermissionPolicy,
               AgentEvent, SessionHandle/SessionCommand, LineProcess,
               per-turn driver, JSON-RPC framing, Registry, SessionsStore,
               file checkpoints
src/harness/   one module per harness: descriptor, capabilities, probe,
               list_models, start_session, build_print_command
  claude/      stream-json transport + parser + fixtures/
  codex/       app-server + exec transports + parsers + fixtures/
  pi/          rpc transport + parser + fixtures/
  agy/         stream-json transport (best effort, unverified) + per-turn fallback
  acp/         generic Agent Client Protocol client: one instance per configured agent
src/tui/       App state (pure), transcript blocks, modals, rendering, event loop
scripts/       record-*.py capture real vendor sessions; fake-harness.py replays
               them for tests/session_e2e.rs
```

Adding a harness: if the agent speaks ACP, a config table is enough. Otherwise
a module implementing `Harness`, a recorded fixture under `fixtures/` with its
expected `.events`, and one line in `Registry::from_config`.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                                   # unit + fixture + e2e (needs python3)
UNHARNESS_UPDATE_FIXTURES=1 cargo test      # regenerate .events after a parser change
scripts/record-claude.py out.jsonl "prompt"  # record a new fixture (run from a scratch dir)
scripts/record-acp.py out.jsonl "prompt" -- gemini --acp
```
