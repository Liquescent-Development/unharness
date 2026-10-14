# Bridging: status notes

Bridging (`core/bridge.rs`): a switch whose bridge is over budget first
asks the harness being left for a handoff summary (`Controller::ask_handoff`,
`Block::Handoff`), with every permission request denied while it
writes; not while its subagents are at work (their requests and reports
would land in that turn), but for one whose end is never told (vibe's,
#215). Checked live (pty, Claude Code 2.1.292 → Codex 0.157.0
app-server, `bridge_max_chars = 600`): Claude wrote the summary within
the word limit it was given, the switch followed the turn, and Codex
answered from the first prompt and the summary it was sent. Windows
reported then: Claude's default model 1,000,000, Codex's 258,400, so an
unset budget is about a quarter of those in characters. The windows are
kept in `<state dir>/unharness/context_windows.json`, not in the
workspace (a window decides how much a harness is sent), and taken as
between 8,000 and 2,000,000 tokens. Unverified: a
summary turn on pi, agy or an ACP agent, and one in which the agent
asks for a tool anyway.

## A new session for a choice

A model, effort or provider the session cannot take (Mistral Vibe pins
them on its sessions) is the driver's to say (#275): it answers
`Encoded::NewSession`, the loop tells `AgentEvent::SessionRenewed`, and
the TUI goes on in a new session of the same harness as after a switch
(`Controller::session_renewed`): over the budget the session left, still live,
writes a handoff summary first (`renew_handoff_prompt`), then it is
forgotten and shut down, and the next prompt bridges the conversation to
the new one. A prompt typed between the choice and that answer waits for
it, and goes once the choice is taken (prompts held before it still
wait for Enter); another such choice, and what would end the session,
is refused until then; a session that ends first is forgotten as the
answer would have had it. With no prompted session to say so (none live, or a stored one
started before the first prompt), the harness's declaration does
(`Capabilities::stored_session_keeps`). A turn that does not stop while
the summary is written is ended with its process, and the notice says
that a new session starts with the change (#226).

## Sessions in place of one not resumed or forked

A session that a harness runs in place of one it could not resume or
fork (`AgentEvent::SessionReplaced`, #187) is bridged as a new one:
before anything went to it the old id is forgotten (the new one is
committed with the first prompt, which carries the bridge); after a
prompt went with its start (resumed by that prompt, so without the
conversation) the next prompt bridges from the start, the last turn
included. Rewind anchors and a pending fork of the harness go. A
handoff summary asked of such a session is not used: the switch goes
on without one, and the session left is forgotten. A `--print
--resume` run fails on it instead (see [Headless](headless.md)). Sent by the ACP
driver (see [the ACP note](../../src/harness/acp/NOTES.md#sessions-in-place-of-one-not-resumed-or-forked)) and by pi's parser: pi 1.1.0 given a `--session-id` it
has no session for warns on stderr and creates an empty one under that
id, so a resumed session whose first `get_fork_messages` lists no user
message is one (`fixtures/resume_missing.jsonl`, `record-pi.py
--resume`; `--fork` of a missing session exits 1). pi saves a prompt
into that session at once (`SessionManager._persist`, read in 1.1.0,
and seen in a scratch run: killed as soon as the empty listing was
read, pi had written the session file with the user message, and a
second pi given the id listed it), after which the id resumes as if it
were the conversation's. So the pi transport holds a turn sent to a
resumed session until that first listing answers; when it shows a
replacement the turn ends with an error unsent, else it goes. In the
TUI a turn is held only when a prompt starts the resume, since it
starts sessions unprompted; sent again, the prompt goes with the
bridge. The others fail instead of starting fresh, so they report
nothing: Claude Code 2.1.296
`--resume` of an unknown id ("No conversation found with session ID",
exit 1) and Codex 0.157.0 `exec resume` ("thread/resume failed: no
rollout found", exit 1; the app-server driver fails the turn on a
`thread/resume` error). Such a resume is tried again with the next
prompt (read in the TUI, not run). Unverified: agy's `--conversation` of a missing conversation
(a per-turn harness reporting another id than the one resumed is not
taken as a replacement).
