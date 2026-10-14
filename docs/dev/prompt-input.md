# Prompt input: status notes

## File attachments

File attachments (PDF and text): Claude Code 2.1.289 takes `document`
content blocks on stream-json input (base64 `application/pdf`, and a
`text` source for `text/plain`), both answered from. ACP files go as a
`resource_link`, the one block besides text every agent must accept:
claude-agent-acp 0.85.1 and codex-acp 2.1.1 both read the linked file
with their own tools (so it must be readable inside the sandbox).
claude-agent-acp drops embedded `resource` blobs silently, which is why
files are not embedded. Codex's `UserInput` (0.157.0 schema: text, image,
localImage, audio, localAudio, skill, mention), `codex exec` and pi's
`prompt` (`images` only) have no document input.

## File references

File references (`@` completion, `tui/files.rs`, `Harness::
file_reference`): Claude Code 2.1.289 reads the file of an `@path` in a
stream-json user message into the turn (answered from with no tool call),
also after other text (the bridge prefix) and as `@"path with spaces"`,
and under the sandbox; a missing path is passed on as text. Codex 0.157.0
`app-server` and pi 0.87.1 `rpc` pass `@path` on as text and do not read
it, so they get the bare path, as does everything unverified: `codex
exec`, agy, the ACP agents (claude-agent-acp included), a Claude path
with a `"` in it. The list is of the session's `cwd`, not the workspace
root, and is walked again at each `@`; never tried on a tree past the
50,000 file cap.

## `!command` at the prompt

`!command` at the prompt (`core/shell.rs`, `Block::Shell`,
`BlockRecord::Shell`): `$SHELL -c` in the session's `cwd` through
`LineProcess::spawn_no_input` under `Controller::session_sandbox()` (stdin
`/dev/null`, `setsid`, the process tree killed on Esc and on quit, and
on Linux what it left when the shell exits). No permission prompt, no rule. Refused during a
turn; a prompt sent while it runs is queued. Its command, output tail
and status go once in front of the next prompt (unsent blocks, any
harness); a sent one is part of the bridge, and a harness switch
bookmarks before the first unsent one so the harness left behind still
gets it. Checked live (pty, Claude Code 2.1.290, Landlock): output and
exit status shown, Esc stops `sleep`, the next turn answered from the
output, `\!` sent as `!`, a write to the home directory denied, a
background job reaped on quit. Not handled in `--print`, `--no-tui` or
an initial prompt. Unverified: macOS.
