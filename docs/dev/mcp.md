# MCP servers: status notes

MCP servers (`core/mcp.rs`, `[mcp_servers.<name>]`), each run with a stdio
and an http server defined only in unharness's config, which the model
then called: Claude Code 2.1.289 `--mcp-config <file>` (added to its own
servers; the JSON is in a 0600 file under unharness's state directory
because it holds `env` and `headers`; the flag takes every word up to
the next flag, hence its position and the `--` before a print prompt),
Codex 0.157.0 `-c mcp_servers.<name>.command|args|env|url` on
`app-server` and `exec`, with header values in its environment and
named by `env_http_headers` (a stdio server's `env` stays on the command
line: `env_vars` only forwards a variable under its own name, and
setting it on Codex would hand it to everything Codex starts),
claude-agent-acp 0.85.1 `mcpServers` on `session/new` (stdio only).
Codex merges an override key by key into a server of the same name in
its `config.toml`, even when the override is a whole table (`codex mcp
get` with a scratch `CODEX_HOME`): a `command` over a `url` stops it
from starting, which is why such names are left to Codex
(`own_mcp_servers`). Codex asks to approve an MCP tool call with
`mcpServer/elicitation/request` carrying `_meta.codex_approval_kind:
"mcp_tool_call"` and a thread id but no item id (the parser ties it to
the `mcpToolCall` item in progress on that thread); under `never` with a
read-only sandbox it refuses the call instead, under `never` with full
access it runs it. Failed servers: Claude's `init` lists `status:
"failed"`, Codex sends `mcpServer/startupStatus/updated` (twice, it
retries). Neither CLI stops what an MCP launcher (`sh -c`, `npx`)
started when it exits. Unverified: http servers over ACP,
`session/resume` and `session/load` with servers, codex-acp, a same-name
server in Claude's own config, how to answer the elicitation's `persist`
offer (so "always allow" is not offered), agy (`agy mcp add` writes its
own config; no session flag on 1.2.17) and pi (no MCP client).
