# ACP agents

Any agent that speaks the [Agent Client Protocol](https://agentclientprotocol.com)
can be added from the config, without a dedicated adapter:

```toml
[harnesses.claude-acp]
protocol     = "acp"
command      = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
display_name = "Claude (ACP)"
sandbox_writable = ["~/.claude", "~/.npm"]   # the agent's own state, for the sandbox
asks_permission  = true                      # this agent asks before it acts: offer `ask`
modes            = { ask = "default" }       # its session mode that asks (see Modes)
```

The table name is the harness id: `unharness -H claude-acp`, or
`/harness claude-acp` in the TUI.

## Mistral Vibe

Mistral Vibe's `vibe-acp` is built in: `unharness -H vibe` with nothing in
the config, `ask` and `accept-edits` mapped to profiles of unharness's own
([Your vibe configuration](vibe.md#your-vibe-configuration)), `auto` to
`smart-approve` and `bypass` to `auto-approve` (see Modes below, and
[Mistral Vibe](vibe.md#auto) for what `auto` lets through). A `[harnesses.vibe]` table sets its
`command`, `display_name`, `asks_permission`, `modes` and `sign_in` in
place of the built-in ones; the rest stays. When vibe is not signed in,
unharness says to run `vibe --setup` ([Sign-in](#sign-in)). Allow rules,
MCP servers and what vibe may write are in [Mistral Vibe](vibe.md).

Inside, vibe is the same ACP client as any agent on this page, with a
profile: what unharness knows about `vibe-acp` that the protocol does not
say. A table sets three parts of an agent's profile, `asks_permission`,
`modes` and `sign_in`, and the rest is the protocol's way. vibe's
built-in profile also names its reasoning option (`thinking`), reads a
relative path in an edit as one in the session's directory, and has hooks
the client calls where vibe does what the protocol does not say: its
model and level chosen when it starts (below), its subagents and its
rewind ([Mistral Vibe](vibe.md#rewind)).

Its model and reasoning level (`/model`, `/effort`, `default_model`,
`default_effort`) are never set over ACP, because vibe 2.26.1 writes a
choice made that way into your `~/.vibe/config.toml`. unharness starts
`vibe-acp` with them in its environment instead (`VIBE_ACTIVE_MODEL`, and
the level in a `VIBE_MODELS` entry for the model), which vibe reads for that
process only:

- before the first prompt a change starts vibe again with it;
- after it, vibe starts a new session: a vibe session keeps the model and
  level it last ran, also when it is loaded again. Your next prompt tells
  the new session the conversation so far, the way a harness you switch to
  is told, and when the conversation is too long for that the old session
  writes a handoff summary first.
- With a level and no model chosen, the model the session ran is kept.
- If vibe runs another model or level than the one chosen (one it does not
  have, or a level the model does not offer), unharness says so.

The level's entry needs the model's provider, which unharness reads from
your `~/.vibe/config.toml` (or `$VIBE_HOME/config.toml`) and never writes;
a model not defined there is `mistral`'s.

## Presets

Gemini CLI, OpenCode, Goose, GitHub Copilot, Cursor Agent, Qwen Code and
Kiro are added automatically when their binary is on `PATH`, using the
launch commands from the ACP registry. A table with the same name and
`protocol = "acp"` replaces a preset; without it, the table sets only the
keys every harness takes (`binary`, `default_model`, …) for the preset.

> [!NOTE]
> The presets have not been run with unharness yet, and `unharness doctor`
> says so. Verified agents: `@agentclientprotocol/claude-agent-acp` 0.85.1
> and `@agentclientprotocol/codex-acp` 2.1.1.

## Permissions

unharness applies the permission policy to the requests an agent makes:

| Policy | What unharness does |
|---|---|
| `ask` | shows every request |
| `accept-edits` | answers edits of files inside the workspace itself and shows the rest |
| `auto` | shows every request, where the agent's own mode backs it (Mistral Vibe's `smart-approve`); otherwise not offered and falls back to `accept-edits` |
| `bypass` | answers everything |

What a request is for (an edit, a command, …) is the tool call's kind as
the agent announced it, also when the request itself names the call only
by its id (as Mistral Vibe's `vibe-acp` does).

Under `accept-edits` an edit is answered only when every file it writes
is known and inside the workspace: the nearest directory at or above the
one you started in that has `.git`, `.agents` or `AGENTS.md`, else the
one you started in, the same workspace allow rules and the sandbox use.
Files are compared as allow rules compare them, after following symbolic
links. An edit of a file outside it (in `/tmp`, your home directory,
another project), of a relative path the agent might resolve its own
way, of a file in unharness's own config or state directory, or one
whose files the request does not name exactly, is shown to you.

But an agent asks only about what it chooses to ask about, and unharness
cannot make it ask. codex-acp 2.1.1 ran a command, an edit and a network
call without asking. So `ask` is offered only for an agent whose table has
`asks_permission = true`. Set it for an agent you have seen ask before it
writes, runs a command or reaches out (claude-agent-acp 0.85.1 does). The
presets do not have it until you add a table for them.

A turn stopped while a request waits (an interrupt, or a policy the agent
can no longer be held to) answers it as cancelled and closes its prompt;
the agent is told to stop once the call the request was for has ended, or
after 2 s. Mistral Vibe 2.26.1 lost the turn when told to stop first.

## Modes

Many agents have session modes of their own, and some of them approve
calls without sending a request: Mistral Vibe's `vibe-acp` starts every
session in `accept-edits`, which writes files without asking, and a session
loaded again comes back in the mode it was left in. unharness cannot see
what such a mode lets through, so `modes` says which of the agent's modes
backs each policy:

```toml
[harnesses.my-agent]
protocol        = "acp"
command         = ["my-agent", "--acp"]
asks_permission = true
modes           = { ask = "ask", bypass = "auto-approve" }
```

(Mistral Vibe has this map built in.)

The keys are policies, the values the agent's mode ids (the agent lists
them when a session starts, and the message that refuses `ask` for want of
a map names them). A policy without an entry takes the entry of the
nearest less permissive policy that has one, never a more permissive one's:
above, `accept-edits` keeps the agent in its `ask` mode and unharness answers the
edits itself. Keeping the agent in the mode that asks and letting unharness
decide is the safe form; map a policy to the agent's own `accept-edits`
only if you know it approves nothing but edits.

unharness sets the mapped mode when a session starts, after it is resumed
or loaded, and whenever the policy changes, and reads it back before the
first prompt goes: through the session's `mode` option where the agent
offers one (its answer says which mode is in effect), else with
`session/set_mode`. If the agent does not offer the mode, refuses it, or
answers with another one, the policy is not available for that session:
the TUI falls back to the nearest less permissive policy or asks for
another one (Ctrl+P), a prompt that was waiting for the session fails
without being sent (send it again), and `--print` fails without sending
it. The next session (after `/clear`, a model or sandbox change, a switch
away and back, or the next prompt after it exits) is asked for the policy
again. A policy changed while a mode is still being set has its own mode
set once the agent has answered, and the prompt waits for that. If the
agent goes into another mode by itself, the mapped one is set again, and a
turn that is running is stopped when the new mode could run what the
policy asks about.

An agent that has session modes and no `modes` map gets no policy below
`bypass`: not `ask` (even with `asks_permission = true`), not
`accept-edits`. unharness cannot tell what the mode the agent is in lets
through, and a session can be in any of them: a vibe session loaded again
in `auto-approve` would run commands unasked under `accept-edits`. Only
`bypass` leaves the agent in whatever mode it is in. With a map, every
policy needs an entry at or below it.

`auto` is offered only when the map names a mode for it, since the
agent's own judgement is what `auto` means: what the agent then asks about
is what that mode left to you, and unharness shows it, edits included. A
session that does not offer the mode refuses `auto`, and the TUI takes
`accept-edits` with a warning.

Checked with Mistral Vibe 2.26.1: `ask = "ask"` held (a write was asked
about and denied, and not made). `plan` is hidden by vibe's runtime and
cannot be mapped.

## Questions

unharness tells every agent it can show forms (form elicitation), and an
agent's form is shown as a question in the TUI: one page per field, a
choice with its options (one, or several for a list), a text, number or
Yes/No field as a typed answer. Agents ask with it only when the client
says it can: Mistral Vibe offers its `ask_user_question` tool, and
claude-agent-acp its AskUserQuestion, only then. Whether an agent asks is
known once it has: the session then reports it (`ask_user_question` in
`--print`'s `capabilities_changed`).

A choice takes an answer of your own where the agent says it does: vibe
when its call does not hide the "Other" answer, claude-agent-acp always
(its form has an "Other" field beside each question, which the answer
goes to). Every field must be answered before the form is sent. Esc
declines the form, and so does `--print`, which has nobody to ask; the
agent is told and the turn goes on. A form still open when you interrupt
the turn is cancelled. Questions are asked under every policy, `bypass`
too.

An agent may also ask to open a web page (URL elicitation); unharness does
not offer that, and refuses one with a notice.

Checked with Mistral Vibe 2.26.1 (a question answered in the TUI, one
declined by `--print`, a multiple choice recorded) and claude-agent-acp
0.85.1 (recorded).

## Forks

`/fork` branches the agent's session where the agent advertises
`session/fork` (an unstable part of ACP v1; Mistral Vibe 2.26.1,
codex-acp 2.1.1 and claude-agent-acp 0.85.1 all do). The copy of the
conversation then continues in a new session of the agent that holds
everything the original session held, and the original is left as it
was: nothing is sent to it but what the fork needs. Mistral Vibe forks
only a session it has open, so unharness loads the original first; an
agent that resumes sessions has the new one resumed before the first
prompt, since codex-acp and claude-agent-acp leave it closed. The
agent's mode is then set for the policy, as for any session: a Vibe fork
comes back in the mode the original was in.

`/fork` copies the whole conversation, and `session/fork` copies the
whole session, so the two agree. There is no fork at an earlier point:
to go on from an earlier prompt, `/rewind` first, which starts the agent
afresh with the transcript up to there (ACP has no rewind), and a fork
after that is a fresh session too. Mistral Vibe is the exception: its own
rewind is used ([Mistral Vibe](vibe.md#rewind)), so its session is still
the conversation's after `/rewind` and `/fork` then forks it.

Whether an agent forks is known once a session of it has started: an
agent not run since unharness started, like one that does not advertise
it, starts fresh in the copy with the transcript as context. If the fork
itself fails, or a session to resume is not there any more (Mistral Vibe
answers "Session not found"), unharness starts a new session, says so,
and your next prompt tells it the conversation so far, as it tells a
harness you switch to; the conversation then keeps the new session.
A `--print --resume` run fails there instead ([headless](headless.md#result)).

## Sign-in

unharness does not sign agents in. An agent that wants you signed in
answers with the protocol's auth-required error, at the start of a session
(Mistral Vibe 2.26.1 without an API key answers so before the first
prompt) or at a prompt. unharness then shows the sign-in methods the agent
offered and, where it knows it, the command to run in a terminal: for
Mistral Vibe `vibe --setup`, built in, and for another agent what its
table's `sign_in` names:

```toml
[harnesses.my-agent]
protocol = "acp"
command  = ["my-agent", "--acp"]
sign_in  = "my-agent login"   # shown when it is not signed in; never run
```

```text
vibe is not signed in (Missing API key for mistral provider). It offers:
"Sign in through Mistral AI Studio" (Sign into Mistral Vibe through your
Mistral AI Studio account). unharness does not sign agents in: run
`vibe --setup` in a terminal, then try again
```

Without `sign_in` it says to sign in with the agent's own command line.
The agent is then ended, and nothing is tried again by itself: once you
have signed in, your next prompt starts it afresh, and it picks up the new
sign-in. A session that was to be resumed or forked is not replaced by a
new one meanwhile. With `--print` the run fails with this message and
exit status 1. `unharness doctor` shows the command beside the agent.

Neither the protocol's `authenticate` request nor an agent's terminal
sign-in is used. A terminal sign-in is a command the agent names, which
unharness would have to run outside the sandbox and in your terminal,
and the agent's own login is left to you for every other harness too;
so unharness does not declare that it can run one, and an agent that
offers one (Mistral Vibe offers `vibe-acp --setup` when asked) is shown
by its name only.

## Limits

- Models and effort levels come from the running session, so the pickers
  fill in once it has started (with the TUI, before the first prompt).
  Choices the agent groups under headers are listed as one list.
- Reasoning effort is the session option in the protocol's `thought_level`
  category. An agent that offers its level under another category has no
  effort in unharness, and `/effort` and `default_effort` are not sent to
  it. (Mistral Vibe calls its option `thinking`; unharness knows it, see
  above.)
- ACP agents run [headless](headless.md) with `-p` like any harness, but
  have no `--native` or `--no-tui` mode. Mistral Vibe has both, through
  its own `vibe` ([Mistral Vibe](vibe.md#vibes-own-cli)).
- Sign in with the agent's own CLI ([Sign-in](#sign-in)).
