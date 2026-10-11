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

## Presets

Gemini CLI, OpenCode, Goose, GitHub Copilot, Cursor Agent, Qwen Code and
Kiro are added automatically when their binary is on `PATH`, using the
launch commands from the ACP registry. A table with the same name
overrides a preset.

> [!NOTE]
> The presets have not been run with unharness yet, and `unharness doctor`
> says so. Verified agents: `@agentclientprotocol/claude-agent-acp` 0.85.1
> and `@agentclientprotocol/codex-acp` 2.1.1.

## Permissions

unharness applies the permission policy to the requests an agent makes:

| Policy | What unharness does |
|---|---|
| `ask` | shows every request |
| `accept-edits` | answers file edits itself and shows the rest |
| `auto` | falls back to `accept-edits` |
| `bypass` | answers everything |

What a request is for (an edit, a command, …) is the tool call's kind as
the agent announced it, also when the request itself names the call only
by its id (as Mistral Vibe's `vibe-acp` does).

But an agent asks only about what it chooses to ask about, and unharness
cannot make it ask. codex-acp 2.1.1 ran a command, an edit and a network
call without asking. So `ask` is offered only for an agent whose table has
`asks_permission = true`. Set it for an agent you have seen ask before it
writes, runs a command or reaches out (claude-agent-acp 0.85.1 does). The
presets do not have it until you add a table for them.

## Modes

Many agents have session modes of their own, and some of them approve
calls without sending a request: Mistral Vibe's `vibe-acp` starts every
session in `accept-edits`, which writes files without asking, and a session
loaded again comes back in the mode it was left in. unharness cannot see
what such a mode lets through, so `modes` says which of the agent's modes
backs each policy:

```toml
[harnesses.vibe]
protocol        = "acp"
command         = ["vibe-acp"]
asks_permission = true
modes           = { ask = "ask", bypass = "auto-approve" }
```

The keys are policies, the values the agent's mode ids (the agent lists
them when a session starts, and the message that refuses `ask` for want of
a map names them). A policy without an entry takes the entry of the
nearest less permissive policy that has one, never a more permissive one's:
above, `accept-edits` keeps vibe in its `ask` mode and unharness answers the
edits itself. Keeping the agent in the mode that asks and letting unharness
decide is the safe form; map a policy to the agent's own `accept-edits`
only if you know it approves nothing but edits.

unharness sets the mapped mode when a session starts, after it is resumed
or loaded, and whenever the policy changes, and reads it back before the
first prompt goes: through the session's `mode` option where the agent
offers one (its answer says which mode is in effect), else with
`session/set_mode`. If the agent does not offer the mode, refuses it, or
answers with another one, the policy is not available for that session:
the TUI asks for another one (Ctrl+P) and keeps the prompt, and `--print`
fails without sending it. If the agent goes into another mode by itself, the
mapped one is set again, and a turn that is running is stopped when the new
mode could run what the policy asks about.

An agent that has session modes and no `modes` map gets no policy below
`bypass`: not `ask` (even with `asks_permission = true`), not
`accept-edits`. unharness cannot tell what the mode the agent is in lets
through, and a session can be in any of them: a vibe session loaded again
in `auto-approve` would run commands unasked under `accept-edits`. Only
`bypass` leaves the agent in whatever mode it is in. With a map, every
policy needs an entry at or below it.

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

## Limits

- Models and effort levels come from the running session, so the pickers
  fill in once it has started (with the TUI, before the first prompt).
  Choices the agent groups under headers are listed as one list.
- Reasoning effort is the session option in the protocol's `thought_level`
  category. An agent that offers its level under another category has no
  effort in unharness, and `/effort` and `default_effort` are not sent to
  it: Mistral Vibe 2.26.1 calls its option `thinking`, and setting that
  rewrites its own `~/.vibe/config.toml`.
- ACP agents run [headless](headless.md) with `-p` like any harness, but
  have no `--native` or `--no-tui` mode.
- Sign in with the agent's own CLI.
