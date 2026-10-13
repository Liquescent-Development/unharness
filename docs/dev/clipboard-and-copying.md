# Clipboard and copying: status notes

## Clipboard images

Clipboard images (`tui/clipboard.rs`, `Ctrl+V` and `/paste`): the reader
logic is tested against a stand-in tool only. No Wayland or X11 display
and no Mac were available, so the real `wl-paste --list-types` /
`--no-newline --type`, `xclip -selection clipboard -o -t TARGETS` and
`osascript -e 'the clipboard as «class PNGf»'` calls are unverified, as
is which terminals pass `Ctrl+V` through. Dropped files (`tui/drop.rs`)
are read from a bracketed paste; the quoting forms come from terminal
documentation, and only the bare and single-quoted ones were sent
through a real pty. herdr 0.8.2 (`HERDR_ENV=1` in a pane), checked with
an isolated session (`XDG_CONFIG_HOME` in scratch, its client in a pty
standing in for the outer terminal): a pane has the environment of the
server, fixed when the first client started it, not of the client
attached now (also for panes split later from another client), so
`WAYLAND_DISPLAY`, `DISPLAY` and `SSH_*` there can describe a session
the user has left. A pane's OSC 52 `c` write reached the client's
terminal (a 200 KB payload too); `p` and the `?` query did not. The
client re-emits it through its own clipboard writer (from herdr's
source: a tool by its own `WAYLAND_DISPLAY`/`DISPLAY`, OSC 52 over ssh),
which is why copy in herdr is always OSC 52: with a stand-in `wl-copy`
and a server started with `WAYLAND_DISPLAY`, unharness had used the
tool and said "Copied" while nothing reached the client. herdr forwards
mouse reports to a pane that asks for them, and a drag in the
transcript copied through it; bracketed paste (two lines, not sent) and
a dropped single-quoted path (attached) arrived intact. On the machine
of the report the server had been started over ssh on a host with no
display (no `wayland-*` socket, no `/tmp/.X11-unix`), so `Ctrl+V` gave
the over-ssh notice. A `wayland-*` socket is not looked for when
`WAYLAND_DISPLAY` is missing: in a pane that says nothing about where
the user is. Unverified: the real clipboard at the far end
(only the bytes the client wrote were checked), herdr's own
`wl-copy` with a client at the desktop, `herdr --remote`, whose client
takes `Ctrl+V` itself (`keys.remote_image_paste`), stages the image on
the server and pastes its path (read from the source; the file is
removed when that client detaches), and reading an image in a pane
whose server env names a display the user is no longer at.

## Copying from the transcript

Copying from the transcript (#152, `tui/rows.rs`, `tui/selection.rs`,
`transcript::block_text`): every rendered row carries what a copy takes
(`RowCopy`), its text without the drawing around it and how it goes on
from the row above (the space a word wrap dropped, nothing inside a
word), built with the row (`Rows`), so a renderer that adds drawing has
to say so. A drag copies that; one over nothing but drawing copies it as
drawn. A right click copies a block's `block_text` (a call's whole
command and output, a written file with its tabs, a response's
markdown), and on a code block in a response that block's code
(`markdown::code_blocks`, matched by order to the rows `Rows::code`
marks; it parses the sanitized text the rows are drawn from and puts
back each tab whose spaces the code holds whole, #218). Checked live
(tmux, Claude Code haiku, a stand-in `wl-copy`): drags over a wrapped
Bash call and its output, its label alone, a Write, a list and a code
block; right clicks on a collapsed call, a Write (its tab kept), a
code block and prose; a real right click through a tmux with `mouse
on` reached unharness, not tmux's menu.
Unverified: herdr, other terminals' right button, macOS.
