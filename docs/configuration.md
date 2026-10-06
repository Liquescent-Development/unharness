# Configuration

Settings come from `~/.config/unharness/config.toml`, with the workspace's
overrides merged over it.

Workspace overrides are kept **outside** the workspace, in
`~/.config/unharness/workspaces/<name>-<hash>.toml`, because a file inside
the workspace could be edited by the agent it configures. `unharness
doctor` prints the path, and `unharness init` creates the file.

> [!IMPORTANT]
> A config file that does not parse stops unharness and names the file and
> line. unharness never falls back to defaults, which would run without your
> sandbox and policy settings.

## Example

```toml
default_harness  = "claude"      # agy | claude | codex | pi
default_policy   = "ask"
mouse            = true          # wheel, scrollbar, jump-to-bottom, drag to select and copy; false leaves the mouse to the terminal
auto_sync        = true          # refresh CLAUDE.md/GEMINI.md symlinks before each run
bridge_max_chars = 24000
file_checkpoints = true          # snapshot the working tree before each prompt (git projects; kept outside the repo)

[sandbox]
level    = "workspace-write"     # read-only | workspace-write | off
writable = ["~/.local/share/my-mcp"]  # extra writable paths (relative ones are under the workspace)
readable = ["~/.config/gh"]      # credential paths to allow reading; also reopens paths inside deny_read
deny_read = ["~/Documents"]      # more paths no harness may read

[harnesses.claude]
# binary = "/path/to/claude"
default_model  = "opus"
default_effort = "high"
default_policy = "accept-edits"
extra_args     = []
sandbox_writable = []            # extra paths this harness may write inside the sandbox
relocate_config  = true          # keep .claude.json in ~/.claude so a sandboxed Claude can update it (see Sandbox)

[harnesses.codex]
transport = "auto"               # auto | app-server | exec

[harnesses.pi]
default_provider = "openai-codex"
default_model    = "gpt-5.5"

[mcp_servers.files]              # an MCP server the harness launches (see MCP servers)
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env     = { LOG_LEVEL = "warn" }

[mcp_servers.docs]               # or one reached over HTTP
url     = "https://example.com/mcp"
headers = { Authorization = "Bearer …" }
# enabled = false                # keep the definition, pass it to no harness
```

## Reference

| Key | Default | Meaning |
|---|---|---|
| `default_harness` | first signed-in ([how](usage.md#the-default-harness)) | Harness to start with |
| `default_policy` | `ask` | [Permission policy](permissions.md) |
| `mouse` | `true` | unharness handles the mouse; `false` leaves it to the terminal |
| `auto_sync` | `true` | Refresh the `CLAUDE.md` / `GEMINI.md` symlinks before each run |
| `bridge_max_chars` | `24000` | How much of the conversation seeds a newly switched-to harness |
| `file_checkpoints` | `true` | [Checkpoint](tui.md#rewind-and-checkpoints) the working tree before each prompt |
| `[sandbox]` | | See [Sandbox](sandbox.md) |
| `[harnesses.<id>]` | | Per-harness `binary`, `default_model`, `default_effort`, `default_provider`, `default_policy`, `extra_args`, `sandbox_writable` |
| `[harnesses.claude] relocate_config` | `true` | See [Sandbox](sandbox.md#things-to-know) |
| `[harnesses.codex] transport` | `auto` | `app-server`, or `exec` to force the fallback (Codex marks `app-server` experimental) |
| `[harnesses.<name>] protocol = "acp"` | | Adds an [ACP agent](acp.md) |
| `[mcp_servers.<name>]` | | See [MCP servers](mcp.md) |

## Files unharness keeps

| Path | What |
|---|---|
| `~/.config/unharness/config.toml` | Global settings |
| `~/.config/unharness/workspaces/<name>-<hash>.toml` | Workspace overrides |
| `~/.config/unharness/allow.toml`, `workspaces/<name>-<hash>.allow.toml` | [Allow rules](permissions.md#allow-rules) |
| `<workspace>/.unharness/conversations/` | [Saved conversations](usage.md#conversations) |
| `<workspace>/.unharness/prompt_history.jsonl` | [Prompt history](tui.md#prompt-history) |
| `~/.local/state/unharness/checkpoints/` | [File checkpoints](tui.md#rewind-and-checkpoints) |
| `~/.local/state/unharness/guard/` | Vendor settings as they were before a session ([Sandbox](sandbox.md#things-to-know)) |
| `<state dir>/unharness/pasted/` | Pasted clipboard images, kept for a week |

Paths are shown for Linux. On macOS, `~/Library/Application Support/unharness`
takes the place of both `~/.config/unharness` and `~/.local/state/unharness`.
