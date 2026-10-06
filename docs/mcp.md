# MCP servers

Define a server once under `[mcp_servers.<name>]` and every harness that can
take one gets it for the session:

```toml
[mcp_servers.files]              # a server the harness launches
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env     = { LOG_LEVEL = "warn" }

[mcp_servers.docs]               # or one reached over HTTP
url     = "https://example.com/mcp"
headers = { Authorization = "Bearer …" }
# enabled = false                # keep the definition, pass it to no harness
```

| Harness | How the server is passed |
|---|---|
| Claude Code | `--mcp-config` |
| Codex | `-c mcp_servers.…` overrides (`app-server` and `exec`) |
| ACP agents | in the session request; http servers only when the agent says it takes them |
| pi, Antigravity | not possible for a single session; unharness says so when a session starts |

Nothing is written to `~/.claude`, `~/.codex` or any other vendor
configuration, and the servers a harness already has there stay
available. A workspace's table with the same name replaces the global
one, so `enabled = false` there switches a server off for that workspace.

## Checking servers

`unharness doctor` lists the servers and shows which installed harnesses
get each one. It starts each `command` server to check that it answers
(name, version, number of tools), the same way a session starts it: in the
workspace and inside the sandbox, so a server that cannot run there fails
here too. A `url` server is listed but not contacted.

## Worth knowing

- **Sandbox.** The harness starts the server inside the
  [sandbox](sandbox.md), so a server that writes outside the workspace
  needs its directory in `[sandbox] writable`.
- **Secrets.** `env` and `headers` often hold tokens. For Claude Code the
  definitions go in a file only you can read, under unharness's state
  directory; for ACP agents they go over the protocol; for Codex, `headers`
  go through its environment.

  > [!WARNING]
  > A Codex command server's `env` is the exception: it is on Codex's
  > command line, where other local users can read it.

- **Name clashes.** If Codex has a server of the same name in its own
  `config.toml`, that one is used and unharness says so, because Codex
  would mix the two definitions.
- **Permissions.** Each harness still applies its own permission rules to
  MCP tools. Under `ask`, Claude Code and Codex prompt for each call (Codex
  has no "always allow" for these yet). `codex exec` cannot prompt and has
  no `ask`.
- **Failures.** A server that fails to start is reported in the transcript
  by Claude Code and Codex. An ACP agent does not report it.
