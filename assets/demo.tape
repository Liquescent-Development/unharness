# The README demo. Needs vhs (with ttyd, ffmpeg and a Chromium it can start),
# unharness on PATH, and Claude Code and Codex signed in.
# Run from the repository root:  vhs assets/demo.tape
# The agents answer live, so every recording differs a little.

Output assets/demo.gif

Require unharness

Set Shell "bash"
Set FontSize 15
Set Width 1280
Set Height 760
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms
Set Framerate 15

Hide
Type ". ./assets/demo-setup.sh"
Enter
Sleep 1s
Show

Type "unharness -H claude"
Sleep 500ms
Enter
Wait+Screen@30s /Ready/
Sleep 1s

# Claude reads the file freely; the edit waits for an answer under `ask`.
Type "fib(1) returns 0. Find and fix the bug in fib.py. Don't run anything."
Sleep 500ms
Enter
Wait+Screen@180s /allow once/
Sleep 2.5s
Type "y"
Sleep 3s
Wait+Screen@180s /Ready · last turn/
Sleep 3s

# Hand the conversation to Codex; it answers from what Claude did.
Type "/harness codex"
Sleep 500ms
Enter
Sleep 2s
Type "In one sentence, what was the bug Claude just fixed? Answer from the conversation, without running anything."
Sleep 500ms
Enter
Sleep 3s
Wait+Screen@180s /Ready · last turn/
Sleep 4s

Ctrl+D
Sleep 500ms
