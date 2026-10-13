# The README demo. Needs vhs (with ttyd, ffmpeg and a Chromium it can start),
# unharness on PATH (or built in target/release), and Claude Code, Codex and
# Mistral Vibe signed in. Run from the repository root:  vhs assets/demo.tape
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
# "Allow always" writes an unharness rule, which applies to every agent.
Type "fib(1) returns 0. Find and fix the bug in fib.py. Don't run anything."
Sleep 500ms
Enter
Wait+Screen@180s /allow once/
Sleep 2.5s
Type "a"
Wait+Screen@30s /Enter save and allow/
Sleep 2.5s
Enter
Sleep 3s
Wait+Screen@180s /Ready · last turn/
Sleep 3s

# Hand the conversation to Mistral Vibe, over the Agent Client Protocol.
# It writes a new file, which no rule covers yet, so it asks.
Type "/harness vibe"
Sleep 500ms
Enter
Sleep 3s
Type "Write test_fib.py with one pytest test that checks fib(0) through fib(6). Don't run anything."
Sleep 500ms
Enter
Wait+Screen@180s /allow once/
Sleep 2.5s
Type "y"
Sleep 3s
Wait+Screen@180s /Ready · last turn/
Sleep 3s

# And to Codex, which answers from what the other two did.
Type "/harness codex"
Sleep 500ms
Enter
Sleep 2s
Type "In one sentence each: what did Claude fix, and what did Vibe add? Answer from the conversation, without running anything."
Sleep 500ms
Enter
Sleep 3s
Wait+Screen@180s /Ready · last turn/
Sleep 3s

# The same policies on every agent.
Ctrl+P
Sleep 2.5s
Escape
Sleep 1s

Ctrl+D
Sleep 500ms
