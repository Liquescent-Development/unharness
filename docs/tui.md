# The TUI

The transcript fills the window. The current state sits under the prompt:

- a status rule, showing what the agent is doing and for how long;
- the prompt box, which grows with what you type up to eight rows, then
  scrolls;
- a status line with the working directory and git branch, the harness,
  provider and model, effort, policy and sandbox level, token usage and
  cost, the session id, any capability caveat, and key hints.

Above the prompt, when there is something to show: the agent's plan as a
checklist (`/plan` hides it), prompts waiting in the queue, and files
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
| `Ctrl+V`, `/paste` | Attach the image on the clipboard |
| `Alt+Up` | Pull the last queued prompt back into the prompt box |
| `Esc` | Close the autocomplete list, else interrupt the running turn, else clear the prompt. Never quits |
| `Ctrl+D`, `/quit` | Quit. `Ctrl+C` also quits when idle |

### Pickers

| Key | Action |
|---|---|
| `Ctrl+H` | Harness |
| `Ctrl+M` | Model (provider first on multi-provider harnesses) |
| `Ctrl+E` | Reasoning effort (the levels come from the harness) |
| `Ctrl+P` | Permission policy |
| `Ctrl+R` | Resume a saved conversation |
| `Ctrl+S`, `/subagents` | Subagents of this conversation |

### Transcript

| Key | Action |
|---|---|
| `PageUp` / `PageDown` | Scroll ten lines |
| `Shift+Up` / `Shift+Down` | Scroll two lines |
| Mouse wheel, scrollbar | Scroll. Drag the thumb or click the track |
| `↓ Jump to bottom` | Shown while scrolled up. Click it to follow new output again |
| Drag, double click, triple click | Select text, a word (paths stay whole) or a row, copied on release |
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
extension dialogs) open the matching dialog.

## Slash commands

Type `/` to open autocomplete; `Enter` on a partial command completes it.

| Command | Action |
|---|---|
| `/harness`, `/switch` | Switch harness ([Switching harnesses](usage.md#switching-harnesses)) |
| `/provider`, `/model`, `/effort`, `/policy`, `/sandbox` | Open the matching picker |
| `/resume` | Resume a saved conversation (every harness in it) |
| `/sessions`, `/conversations` | List saved conversations in this workspace |
| `/usage` | Token usage and cost, plus the account's rate-limit windows |
| `/plan` | Show or hide the agent's plan |
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
| `/clear` | Clear the transcript |
| `/help`, `/quit` | Commands and shortcuts; quit |

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
- The rows are copied as drawn, so a wrapped paragraph keeps its line
  breaks and indent.
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

Tool calls render as blocks:

- shell output in a gutter;
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

The agent CLIs unharness starts do not get herdr's `HERDR_*` variables,
so they cannot reach herdr's socket (which drives every pane), and a
vendor's own herdr integration does not report over unharness's. pi's
herdr extensions are silent under unharness as a result.

## Terminal notes

Most terminals send the same byte for `Enter`, `Shift+Enter` and `Ctrl+M`.
In those, `Shift+Enter` sends the prompt and `Ctrl+M` cannot open the
model picker (use `/model`). Terminals that implement the kitty keyboard
protocol (kitty, foot, Ghostty, WezTerm, Alacritty and recent iTerm2,
among others) are asked to tell the keys apart, and in those both keys
work as listed.
