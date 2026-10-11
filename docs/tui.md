# The TUI

The transcript fills the window. The current state sits under the prompt:

- a status rule, showing what the agent is doing and for how long;
- the prompt box, which grows with what you type up to eight rows, then
  scrolls;
- a status line with the working directory and git branch, the harness,
  provider and model, effort, policy and sandbox level, token usage and
  cost, the session id, any capability caveat, and key hints. On a
  narrow terminal the directory is shortened first, then effort, model
  and sandbox give way; the harness and its policy always stay.

Above the prompt, when there is something to show: the agent's plan as a
checklist (`/plan` hides it; plan mode is the `plan`
[policy](permissions.md#plan)), prompts waiting in the queue, and files
attached to the next prompt.

## Keys

### Prompt

| Key | Action |
|---|---|
| `Enter` | Send the prompt. During a turn, queue it for when the turn finishes |
| `Alt+Enter` | Steer: send the prompt into the running turn (queued where the harness cannot steer) |
| `Ctrl+J`, `Shift+Enter` | New line. `Ctrl+J` works everywhere; `Shift+Enter` needs the kitty keyboard protocol ([below](#terminal-notes)) |
| `Up` / `Down` | Move between the prompt's lines. On the first / last line, step through prompts sent from this workspace |
| `Home` / `End`, `Ctrl+A` | Start / end of the line (`Ctrl+A` is start only) |
| `Ctrl+U` | Clear the prompt |
| `Ctrl+G` | Edit the prompt in `$VISUAL` / `$EDITOR`. What the editor saves comes back into the prompt, unsent |
| `@` | Pick a file to reference ([below](#file-references)) |
| `!command` | Run a shell command yourself ([below](#shell-commands)). `\!` at the start sends a prompt that begins with `!` |
| `/command` | One of unharness's [slash commands](#slash-commands), else the agent's own ([Agent commands](#agent-commands)). `\/` at the start sends it to the agent as typed |
| `Ctrl+V`, `/paste` | Attach the image on the clipboard |
| `Alt+Up` | Pull the last queued prompt back into the prompt box |
| `Esc` | Close the autocomplete list, else interrupt the running turn, else stop the running `!` command, else clear the prompt. Never quits |
| `Ctrl+D`, `/quit` | Quit. `Ctrl+C` also quits when idle (and stops a `!` command first). The agent's CLI is given a few seconds to exit; while the status line says so, `Ctrl+C`, `Ctrl+D`, SIGTERM, SIGHUP or SIGQUIT stop waiting and kill it |

### Pickers

| Key | Action |
|---|---|
| `Ctrl+H` | Harness |
| `Ctrl+M` | Model (provider first on multi-provider harnesses) |
| `Ctrl+E` | Reasoning effort (the levels come from the harness) |
| `Ctrl+P` | Permission policy for this harness (`d` saves it as the harness's default) |
| `Ctrl+R` | Resume a saved conversation |
| `Ctrl+S`, `/subagents` | Subagents of this conversation |

A choice that cannot be taken is greyed out with the reason beside it,
and the cursor skips it: a harness not found on `PATH` (which `/switch`
refuses too), a policy the
harness does not have, a sandbox level with no sandbox to enforce it.

### Transcript

| Key | Action |
|---|---|
| `PageUp` / `PageDown` | Scroll ten lines |
| `Shift+Up` / `Shift+Down` | Scroll two lines |
| Mouse wheel, scrollbar | Scroll. Drag the thumb or click the track |
| `↓ Jump to bottom` | Shown while scrolled up. Click it to follow new output again |
| Drag, double click, triple click | Select text, a word (paths stay whole) or a line, copied on release without the drawing around it |
| Right click on a block | Copy all of it, also what is not shown: a call's command and whole output, a file it wrote, a response's markdown. On a code block in a response, that code alone |
| `Ctrl+O` | Expand or collapse the last tool call's output |
| `Ctrl+T` | Expand or collapse every tool call |
| Click on a tool call | Expand or collapse it. On a call that spawned a subagent, open the subagent's transcript |

### Subagents

| Key | Action |
|---|---|
| `Down` past the prompt's last row | Move into the subagent list. `Up`/`Down` choose, `Enter` opens, `Delete` removes a finished one, `Esc` or typing returns to the prompt |
| In a subagent's transcript | `Esc` back · `s` stop it · `Tab` / `Shift+Tab` next / previous · arrows, `PageUp`/`PageDown`, `End` scroll · `Ctrl+O`, `Ctrl+T` expand tool calls |

### Permission prompts

When a harness asks for permission, a dialog opens:

| Key | Action |
|---|---|
| `y` | Allow once |
| `a` | Allow always: propose a rule ([Allow rules](permissions.md#allow-rules)) |
| `n` | Deny, with a reason |
| `i` | Show the full tool input |

Agent questions (Claude's AskUserQuestion, Codex's requestUserInput, pi's
extension dialogs) open the matching dialog. When there are several
questions, or a multi-select one, they are shown as a strip across the top
(`✓` marks the answered ones) and the dialog ends on a Submit page that
lists the answers; nothing is sent until you press `Enter` there. A single
single-select question is sent as soon as you choose. When an option comes
with a preview (the draft or snippet being chosen between), it is shown
beside the options, or below them on a narrow terminal.

| Key | Action |
|---|---|
| `↑`/`↓` | Highlight an option |
| `Enter` | Choose it and go to the next question. On a multi-select question, go on with what is ticked (the highlighted option, if nothing is). On the Submit page, send, or open the first unanswered question |
| `Space` | Choose, or tick/untick on a multi-select question |
| `←`/`→`, `Tab`/`Shift+Tab` | Move between the questions and the Submit page; the cursor lands on your answer |
| `PageUp`/`PageDown`, mouse wheel | Scroll the preview |
| `Esc` | Dismiss the questions unanswered |

The `[other]` row takes a typed answer instead of the options (on a
multi-select question it replaces the ticks); a question with no options
has an `[answer]` row. `Enter` on the row starts typing, `Enter` again
keeps the answer and moves on, and `Esc` puts the text back as it was.

Under the `plan` [policy](permissions.md#plan), the agent's plan opens in
a dialog of its own: the plan, then one row per policy to carry it out
under (the one you planned from is highlighted) and a Keep planning row.

| Key | Action |
|---|---|
| `↑`/`↓` | Highlight a row |
| `Enter` | Approve the plan under the highlighted policy; on Keep planning, start typing what to change, and `Enter` again sends it |
| `PageUp`/`PageDown`, mouse wheel | Scroll the plan |
| `Esc` | Keep planning without saying what to change (while typing, stop typing) |

The wheel scrolls a plan or a preview only while unharness has the mouse
(see [Mouse, selection and clipboard](#mouse-selection-and-clipboard)).
With `mouse = false` many terminals send the wheel as `↑`/`↓`, which move
the highlighted row instead.

## Slash commands

Type `/` to open autocomplete; `Enter` on a partial command completes it.

| Command | Action |
|---|---|
| `/harness`, `/switch` | Switch harness ([Switching harnesses](usage.md#switching-harnesses)) |
| `/provider`, `/model`, `/effort`, `/policy`, `/sandbox` | Open the matching picker |
| `/resume` | Resume a saved conversation (every harness in it) |
| `/sessions`, `/conversations` | List saved conversations in this workspace |
| `/usage` | Token usage and cost, plus the account's rate-limit windows |
| `/plan` | Show or hide the agent's plan checklist (plan mode is `/policy plan`) |
| `/subagents` | Open a subagent's own transcript (and stop it from there) |
| `/attach <path>` | Attach an image, PDF or text file to the next prompt |
| `/paste`, `/detach` | Attach the clipboard image; drop pending attachments |
| `/steer <text>` | Send a message into the running turn |
| `/compact [instructions]` | Summarise the context now |
| `/rewind` | Go back to before an earlier prompt and edit it ([below](#rewind-and-checkpoints)) |
| `/undo-restore` | Reverse the last file restore made by `/rewind` |
| `/fork` | Continue in a copy of the conversation |
| `/skills` | List skills found in `.agents/skills` |
| `/allow` | List what is allowed without asking |
| `/remote-control`, `/rc` | Answer the session from claude.ai/code or the Claude app (Claude Code; [below](#remote-control)) |
| `/clear` | Start a new conversation (the previous one stays in `/resume`) |
| `/help`, `/quit` | Commands and shortcuts; quit |

### Agent commands

A `/command` unharness does not have goes to the agent as typed, where the
agent runs commands of its own from a prompt: Claude Code (its built-ins,
custom commands and skills), Codex (its skills), pi (extension commands,
prompt templates and `/skill:<name>`), Antigravity (its skills) and an ACP
agent once it has listed some. The transcript says it was passed on.
Skills are `/name` on every harness: Codex's own interface names one
`$name`, and unharness sends `/name` for a Codex skill that way. Where the
agent has listed none (Codex's `exec` transport, an ACP agent that lists
nothing) such a command is an error.

The autocomplete list offers the commands the agent has, after
unharness's own and marked with the harness's name. The harness is
started as soon as unharness is (and after a switch or a resume), before
anything is sent to it, so its commands are there before the first
prompt; a list opened while it starts takes them in when they come.
Antigravity is the exception: it keeps a conversation for every process,
so it is started with the first prompt, and unharness asks
`agy --print=/skills` for its skills instead.

unharness's own commands come first: `/clear` starts a new
conversation ([below](#clear)). `\/` sends the rest as typed: `\/clear` is Claude Code's
`/clear`, and with `\/` the list offers the agent's commands of the same
names. Where the agent lists no commands, `\/` sends a prompt that starts
with `/`.
An agent's command that does what one of unharness's does (`\/clear`,
Claude's `/new`, `\/model`) changes its session without unharness
knowing, which the transcript warns about: the model shown, the
transcript and what the next switch tells the agent may not match it.

A `/` prompt queued during a turn is sent only to the harness it was
typed for. After a switch it comes back to the prompt box, since there it
could be a command where it was text, or the other way round.

A command has to start the message the agent gets, so it goes alone: the
context of a harness switch and the output of `!` commands it has not
seen go with the next prompt instead.

### Remote Control

`/remote-control` (or `/rc`) puts the Claude Code session on Claude's
Remote Control: the transcript gives a `claude.ai/code/session_…` link,
which opens in a browser or the Claude app signed in to the same account,
and the status bar says `remote` (`remote: starting` until the link is
there, the connection's state while it is not connected, `remote:
paused` while Claude is not running). `/rc <name>`
names the session there, `/rc status` shows the link again, `/rc off`
takes it off. Claude's own `/rc` cannot run in the headless mode unharness
drives it in; this is the same thing through Claude's SDK interface.

A prompt typed there runs as a turn of the session, shown in the
transcript as yours under "from Remote Control". A permission request
goes to both sides, and whichever answers first decides: an answer there
closes the request here. An allow rule still answers what it covers at
once (the remote side may show the request until then).

A new Claude process (a sandbox or provider change, the next prompt after
Claude exited) is put on Remote Control again, with the same link where
Claude resumes the session; meanwhile the status bar says `remote:
paused`. Turned on before the session has started, it goes on once it has.
A switch to another harness, a fork, `/resume`, `/clear` and quitting take
it off. Elsewhere `/rc` is the agent's own command, if it has one.
It is not offered in `--print` or `--no-tui` (where Claude's own `/rc`
works).

## File references

An `@` at the start of a word lists the files under the working
directory, hidden ones included, leaving out what `.gitignore`, `.ignore`
and git's exclude files leave out. Typing after the `@` narrows the list
fuzzily: `@tuiapp` finds `src/tui/app.rs`. The list is rebuilt at each
`@`, up to 50,000 files.

`Tab` or `Enter` replaces the word with the path in the form the active
harness reads:

- **Claude Code** gets `@src/tui/app.rs` and puts the file's content into
  the turn itself.
- **Every other harness** gets the plain path, and its agent opens the
  file with its own tools.

`Esc` closes the list and leaves the word as typed. After that the path is
ordinary text: it is not rewritten if the prompt is later sent to another
harness.

## Shell commands

A prompt that starts with `!` is not sent to the agent: unharness runs
the rest as a shell command and shows its output in the transcript, the
way Claude Code's `!` does.

```
! cargo test -p parser
```

- It runs with `$SHELL -c` (`/bin/sh` when `SHELL` is unset), so
  only what your shell reads for a non-interactive command applies: for
  zsh that is `.zshenv`, not `.zshrc` and its aliases.
- It runs in the session's working directory, under the sandbox the
  active harness's process gets ([Sandbox](sandbox.md)): a command you
  type cannot write where the agent could not. Like the agent, it
  reaches herdr through unharness ([herdr](#herdr)).
- It has no input and no terminal: stdin is empty, so a command that asks
  for a password or opens an editor fails instead of waiting.
- Output (stdout and stderr, as they arrive) streams into the transcript
  with the exit status. The last 64 KiB are kept; earlier lines are
  counted, not kept.
- `Esc` or `Ctrl+C` stops it, along with anything it started. When it
  ends, what it left running in the background (`cmd &`) is stopped too.
- It asks nothing and adds no [allow rule](permissions.md#allow-rules):
  you typed it.
- One runs at a time, and not during a turn: a `!` typed then is handed
  back in the prompt box. A prompt sent while a command runs waits for it
  to end.

The agent sees the command too. The command, the tail of its output (up
to 8,000 characters) and how it ended go in front of your next prompt,
whichever harness that goes to, once. After a harness switch, the
commands are part of the conversation bridged to the next harness. The
block is saved with the conversation and comes back with `/resume`.

To send the agent a prompt that starts with `!`, type `\!`. `!` is a
TUI feature: in `--print` and `--no-tui` a prompt starting with `!` is
the agent's, as is an initial prompt given on the command line.

## Prompt history

Sent prompts and commands are kept per workspace: the newest 500, in
`.unharness/prompt_history.jsonl`. `unharness sessions --clear` removes
them. `Up` on the prompt's first line recalls them, and what you had typed
comes back when you step `Down` past the newest. While the `/` or `@` list
is open, the arrows move in the list instead.

## Mouse, selection and clipboard

unharness handles the mouse itself and replaces the terminal's selection
with its own. Drag in the transcript to select; the text is copied when
you let go, and the status rule confirms it. Dragging past the top or
bottom edge scrolls.

- The copy goes through `wl-copy`, `xclip`, `xsel` or `pbcopy` when one is
  installed. Otherwise, and over ssh, it goes through the terminal (OSC 52),
  which some terminals ignore and tmux passes on only with
  `set-clipboard on`. In a [herdr](https://herdr.dev) pane it always goes
  through the terminal: herdr hands it to the client you are attached
  with, which knows better than the pane whether you are at the desktop or
  over ssh.
- A copy takes the text and leaves out what is drawn around it: block
  headers, a tool call's label and status, the count of a command's
  further lines, gutters, wrap marks, a thought's box, code block
  frames, table rules and the row counting hidden lines. A line wrapped
  over several rows comes out as one line. Only what is copied is
  highlighted. A list keeps its `-` and numbers
  and a table its cells, a tab between them; an edit keeps its `-` and
  `+`, a written file does not. A drag over nothing but drawing (a label)
  copies that as drawn. A table whose cells wrap is copied a row of the
  screen at a time, and one too narrow for its columns with its `|`; a
  right click on it copies the markdown.
- A right click on a block copies all of it as it was written, also what
  the transcript does not show: a tool call's whole command and output
  (collapsed or not), the content of a file it wrote, an edit as `-` and
  `+` lines, a response as markdown, a subagent's report. On a code block
  in a response it copies that code alone, with its tabs. A `!` command's
  output is what was kept of it.
- Holding `Shift` while dragging gives you the terminal's own selection in
  most terminals. `mouse = false` in the config turns all of this off.

## Paste, drop and images

**Pasted text** goes into the prompt as is, newlines included, and is
never sent until you press `Enter` (this relies on bracketed paste, which
every current terminal supports). A paste while a dialog is open goes
into the dialog's text field if it has one, and is otherwise ignored.

**Dropped files** are attached, as `/attach` would. A paste that is only
paths of existing files is read as a drop: absolute or `~/` paths, bare,
quoted or backslash-escaped as terminals write them, or `file://` URIs. A
file the active harness cannot take stays a path in the prompt, with a
notice saying why.

**Clipboard images** need `Ctrl+V` or `/paste`, because a terminal's own
paste carries text only. The image is read with `wl-paste` (Wayland),
`xclip` (X11) or `osascript` (macOS), saved under
`<state dir>/unharness/pasted/` (images older than a week are removed),
and attached like any other image. Files copied in a file manager are
attached the same way.

> [!NOTE]
> Over ssh those tools would read the remote machine's clipboard, so
> unharness reads nothing and says so; copy the file over and `/attach`
> it. Where the terminal takes `Ctrl+V` for its own paste, use `/paste`.
> In a herdr pane, unharness sees the display and ssh session the herdr
> server was started from, not the one you are attached from now.

## Context and rate limits

The usage line shows how full the model's context is (`ctx 34%`).
`/usage` adds the account's rate-limit windows, and a notice appears when
one passes 80%.

## Rendering

Blocks are one blank line apart, except tool calls (and hooks) in a row,
which stay together: each call's gutter ends with `└`. A call's command
is in the terminal's own colour and its output dimmed below it.

Tool calls render as blocks:

- shell output in a gutter; a command of several lines (a heredoc
  script) shows its first line and how many follow, and the expanded call
  shows the rest of it above the output;
- file edits as syntax-coloured red/green replacements;
- reads highlighted by file type;
- unified diffs in red/green.

Fenced code in answers is syntax highlighted and wrapped, never cut off.

## Rewind and checkpoints

`/rewind` lists your earlier prompts. Picking one removes it and
everything after it from the conversation, and puts its text back in the
prompt box.

- **Where the harness can rewind**, its own session is rewound, so it
  really no longer knows the removed turns. This works whether or not
  the harness is running at the moment.
- **Otherwise** the harness starts a fresh session with the remaining
  conversation as context. The picker tells you which will happen, and if
  a harness reports that its rewind failed, unharness falls back to a
  fresh session by itself.

Press `f` instead of `Enter` in the picker to also restore the files to
how they were before that prompt. In a git repository, unharness
checkpoints the working tree before every prompt (tracked and untracked
files, not ignored ones):

- Checkpoints go into a shadow repository of unharness's own under your
  state directory (`~/.local/state/unharness/checkpoints/<project>-<hash>.git`
  on Linux), which uses the project as its work tree. **Nothing is written
  to the project's own `.git`.**
- The restore works for any harness, since it does not depend on the
  agent, and `/undo-restore` reverses it.
- Changes outside the project are not covered.
- The first checkpoint copies the project's files once; later ones store
  only what changed.
- `file_checkpoints = false` turns checkpoints off, and `unharness sessions
  --clear` deletes the project's shadow repository.

## Fork

`/fork` continues in a copy of the conversation and leaves the original
unchanged. Harnesses that can branch a session do so, and the copy knows
exactly what the original knew. The others start fresh with the
transcript as context.

## Clear

`/clear` starts a new conversation in the same workspace, as a fresh
launch would, with the same harness, policy, model, effort, provider and
sandbox. The conversation left is saved as it was and stays in
`/resume`; when it cannot be saved, `/clear` keeps it and says why. The
agent's session ends and a new one starts that knows nothing of it
(one started fresh and sent nothing yet carries on), so usage and
the context meter start from zero; a harness used before the clear
starts afresh too when you switch to it. Prompts held after a turn that
did not finish stay queued. `/clear` waits until the turn, the subagents
at work and a running `!` command are done.

## Hooks

A hook the harness runs (Claude Code, and Codex over `app-server`) is a
line in the transcript, `⚙ hook PreToolUse:Bash`, with the first line of
what it said: grey when it succeeded, yellow when it failed, red when it
blocked what it ran before (a tool call, a prompt, the end of the turn),
with the reason it gave. One whose end never came (the harness exited, the
turn it ran in was interrupted) says so instead, until the end arrives
after all.

## Subagents

Subagents are shown for as long as they run, which can outlast the turn
that started them. Claude Code launches them in the background by
default, and a Codex sub-agent keeps going when the main agent does not
wait for it.

They are listed under the prompt, each with its task, how long it has
run and what it is doing: running ones first, then finished ones, latest
first. A finished one stays listed so you can still read what it did,
until you remove it with `Delete`. Its transcript is kept, and `Ctrl+S`
still offers it. While any subagent is running, the status rule stays
busy ("2 subagents running") instead of "Ready".

The main transcript belongs to the main agent. A subagent appears there
as one line, the call that spawned it, marked running, done, failed or
stopped. Everything it does (tool calls, prose, final report) is in a
transcript of its own:

- `Down` from the prompt, then `Enter`, opens the chosen subagent;
- `Ctrl+S` or `/subagents` offers every subagent of the conversation;
- clicking its line in the transcript works too.

A subagent's own subagents appear in its transcript the same way. A
subagent is stopped only from its own transcript, with `s`; `Esc` there
just goes back. A prompt sent while subagents run goes to the main
agent. All of it is saved with the conversation.

Claude Code starts a turn of its own to report when a background subagent
ends or is stopped. Codex does not, so a Codex sub-agent's report is only
in its own transcript. Codex also names a sub-agent without describing
its task, and shows what it is doing only through its tool calls.
Antigravity names a subagent by its role and shows neither its tool
calls nor what it is doing: its transcript holds its report, or the
error that stopped it. Antigravity's turn waits for its subagents, and
none can be stopped.

## herdr

Run inside a [herdr](https://herdr.dev) pane (`HERDR_ENV=1`), unharness
tells herdr what the session is doing, so the pane shows up in `herdr
agent list` as `unharness`:

- **blocked** while a permission request, question or confirmation waits
  for your answer, also when it is queued behind another picker, and
  while the policy picker holds the session because the policy you asked
  for is not available;
- **working** while a turn runs or subagents are still at work;
- **idle** otherwise. herdr shows it as **done** when the turn ended in a
  tab you were not looking at.

When unharness quits it hands the pane back. Nothing waits on herdr: if
its socket cannot be reached, unharness carries on and writes what went
wrong to `herdr.log` in its state directory (`~/.local/state/unharness/`
on Linux). `herdr = false` in the config turns this off.

The agent CLIs unharness starts get the pane's `HERDR_*` variables, so
an agent can drive herdr from its own pane (`herdr pane split --current`,
the herdr skill). Their `HERDR_SOCKET_PATH` names a socket of
unharness's that passes everything on to herdr except an agent CLI's
own state reports (`herdr integration install claude`, pi's herdr
extension): unharness answers those itself, so the pane shows
unharness's state. They are noted once each in `herdr.log`.

`--print` reports nothing, but its CLI reaches herdr through that
socket too: herdr 0.8.2 keeps the first report a CLI's integration makes
for a pane after the CLI has gone, and a TUI in that pane later could
not report. `--no-tui`, where the CLI's own interface is what runs in
the pane, and `herdr = false` pass the variables on unchanged, and there
a CLI's own integration reports for the pane.

## Terminal notes

Most terminals send the same byte for `Enter`, `Shift+Enter` and `Ctrl+M`.
In those, `Shift+Enter` sends the prompt and `Ctrl+M` cannot open the
model picker (use `/model`). Terminals that implement the kitty keyboard
protocol (kitty, foot, Ghostty, WezTerm, Alacritty and recent iTerm2,
among others) are asked to tell the keys apart, and in those both keys
work as listed.
