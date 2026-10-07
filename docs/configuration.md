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
herdr            = true          # report working/blocked/idle to herdr when run in one of its panes
auto_sync        = true          # refresh CLAUDE.md/GEMINI.md symlinks before each run
bridge_max_chars = 24000         # unset: measured by the next harness's context window
bridge_summary   = "auto"        # "never": no handoff summary when the bridge is over budget
file_checkpoints = true          # snapshot the working tree before each prompt (git projects; kept outside the repo)

[sandbox]
level    = "workspace-write"     # read-only | workspace-write | off
writable = ["~/.local/share/my-mcp"]  # extra writable paths (relative ones are under the workspace)
readable = ["~/.config/gh"]      # credential paths to allow reading; also reopens paths inside deny_read
deny_read = ["~/Documents"]      # more paths no harness may read

[harnesses.claude]
# binary = "/path/to/claude"
# default_provider = "bedrock"   # anthropic | bedrock | vertex | foundry (see Providers)
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
| `herdr` | `true` | Inside a [herdr](tui.md#herdr) pane, report the session's state to herdr |
| `auto_sync` | `true` | Refresh the `CLAUDE.md` / `GEMINI.md` symlinks before each run |
| `bridge_max_chars` | unset | How much of the conversation seeds a newly switched-to harness, in characters. Unset, it is a quarter of the context window the harness's model last reported (at four characters a token, as many characters as the window has tokens; a window is taken as between 8,000 and 2,000,000 tokens), or 24,000 before one has |
| `bridge_summary` | `"auto"` | Ask the harness being left for a handoff summary when the conversation is over `bridge_max_chars` (`"never"` to skip it); see [Switching harnesses](usage.md#switching-harnesses) |
| `file_checkpoints` | `true` | [Checkpoint](tui.md#rewind-and-checkpoints) the working tree before each prompt |
| `[sandbox]` | | See [Sandbox](sandbox.md) |
| `[harnesses.<id>]` | | Per-harness `binary`, `default_model`, `default_effort`, `default_provider`, `default_policy`, `extra_args`, `sandbox_writable` |
| `[harnesses.claude] relocate_config` | `true` | See [Sandbox](sandbox.md#things-to-know) |
| `[harnesses.codex] transport` | `auto` | `app-server`, or `exec` to force the fallback (Codex marks `app-server` experimental). Under `exec` every turn is a process of its own, and what it left running in its process group ends with the turn |
| `[harnesses.<name>] protocol = "acp"` | | Adds an [ACP agent](acp.md) |
| `[mcp_servers.<name>]` | | See [MCP servers](mcp.md) |

## Providers

A harness uses the provider its own configuration chooses unless one is
chosen with `--provider`, `default_provider` or `/provider`. Only a chosen
one is passed on, and it overrides the harness's own choice: a
`default_provider = "anthropic"` for Claude takes any `CLAUDE_CODE_USE_*`
out of its environment, so a shell that exports one for Bedrock no longer
decides. Without a choice the TUI shows the provider each session
reports. When a session runs on another provider than the chosen one,
the status line says so until another one is chosen or a session runs
on the chosen one.

| Harness | Providers | How unharness passes one |
|---|---|---|
| Claude Code | `anthropic`, `bedrock` (Amazon Bedrock), `vertex` (Google Vertex AI), `foundry` (Microsoft Foundry) | Sets `CLAUDE_CODE_USE_BEDROCK`, `_VERTEX` or `_FOUNDRY` and removes the others from Claude's environment |
| Codex | `openai`, `ollama`, `lmstudio`, `amazon-bedrock`, and each `[model_providers.<id>]` of Codex's own `config.toml` | `-c model_provider="<id>"`, and `modelProvider` when a thread is resumed or forked |
| pi | whatever `pi --list-models` reports | `--provider` |

Credentials, region and project stay Claude Code's own (`AWS_REGION`,
`AWS_PROFILE`, `CLOUD_ML_REGION`, `ANTHROPIC_VERTEX_PROJECT_ID`,
`ANTHROPIC_FOUNDRY_RESOURCE`, ...), from the environment or the `env` of
Claude's settings. Two things to know:

- When the `env` of Claude's settings sets a `CLAUDE_CODE_USE_*` variable,
  that decides: Claude ignores the choice (it falls back to Anthropic's
  API when two are set), and unharness reports the provider the session
  runs on as an error.
- The provider is fixed when Claude or Codex starts, so changing it in
  the TUI restarts the session (resumed) with the next prompt. It is
  refused while a turn or a subagent is at work.

The model list is Claude's own for that provider, asked of a short-lived
Claude in the sandbox a session would get (its settings can name
commands it runs at startup, such as `apiKeyHelper`). Bedrock without
credentials took a minute to answer; after 15 s unharness gives up and
`/model <name>` still sets one. The TUI asks in the background and opens
the picker when the list arrives, unless you have moved on (another
provider or picker, a prompt being typed or sent); a list that failed is
asked for again only when you open the picker.

Codex lists models for `openai` only: its `model/list` is OpenAI's
catalog whichever provider is in use. On another provider choose the
model by name (`--model`, `/model <name>`); without one Codex picks its
default OpenAI model there too.

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
