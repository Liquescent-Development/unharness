# Rewind and fork: status notes

Rewind and fork, all checked live: Claude `rewind_conversation` (targets
the uuid we put on each user message; refuses the session's first message
and uuids from the session a fork came from with "stale target", reported
as `RewindFailed`), `--resume X --fork-session`; Codex `thread/revert`
and `thread/fork` (turn ids survive a fork); pi `fork` (a new session,
only in place once it answers) and `--fork <id>` (entry ids survive);
ACP `session/fork` where the agent advertises it (vibe-acp 2.26.1,
codex-acp 2.1.1, claude-agent-acp 0.85.1; see the ACP note), of the
whole session only, with no rewind in the spec, so no anchors to carry
(vibe's own `_rewind/to` is in the vibe note).
Claude's `rewind_files` answers "File rewinding is not enabled" in this
mode, which is why file restore is unharness's own.
