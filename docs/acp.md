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

But an agent asks only about what it chooses to ask about, and unharness
cannot make it ask. codex-acp 2.1.1 ran a command, an edit and a network
call without asking. So `ask` is offered only for an agent whose table has
`asks_permission = true`. Set it for an agent you have seen ask before it
writes, runs a command or reaches out (claude-agent-acp 0.85.1 does). The
presets do not have it until you add a table for them.

## Limits

- Models and effort levels come from the running session, so the pickers
  fill in once it has started (with the TUI, before the first prompt).
- ACP agents run [headless](headless.md) with `-p` like any harness, but
  have no `--native` or `--no-tui` mode.
- Sign in with the agent's own CLI.
