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
> sandbox and policy settings. A harness table that is wrong (an unknown
> policy, Codex `transport` or `protocol`, a `modes` map or a provider
> that is not whole, an ACP agent without a command, a key its harness
> does not take, a value of the wrong type in a key only its harness
> takes, a key unharness no longer takes, a table no harness takes: a
> misspelt name, or another name for a harness such as `claude-code`,
> whose table is `[harnesses.claude]`) stops a run, `unharness switch`
> and `unharness models` too, and is never replaced by the built-in or
> preset of its name. Such an error names the table, and the key when a
> key is wrong, not the file; `unharness doctor` shows it, leaves out a
> harness it cannot build, shows the rest, and exits 1.

## Example

```toml
default_harness  = "claude"      # agy | claude | codex | pi | vibe | an ACP agent's table name
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
readable = ["~/.config/gh"]      # credential paths to allow reading; also reopens paths inside deny_read; in a workspace file, lifts a global deny_read path it names
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

[harnesses.vibe.providers.lan]   # a provider for Mistral Vibe (see Providers)
api_base = "http://127.0.0.1:8080/v1"
models   = ["qwen3-coder"]

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
| `herdr` | `true` | Inside a [herdr](tui.md#herdr) pane, report the session's state to herdr, and keep the agent CLIs' own herdr integrations from reporting over it |
| `auto_sync` | `true` | Refresh the `CLAUDE.md` / `GEMINI.md` symlinks before each run |
| `bridge_max_chars` | unset | How much of the conversation seeds a newly switched-to harness, in characters. Unset, it is a quarter of the context window the harness's model last reported (at four characters a token, as many characters as the window has tokens; a window is taken as between 8,000 and 2,000,000 tokens), or 24,000 before one has |
| `bridge_summary` | `"auto"` | Ask the harness being left for a handoff summary when the conversation is over `bridge_max_chars` (`"never"` to skip it); see [Switching harnesses](usage.md#switching-harnesses) |
| `file_checkpoints` | `true` | [Checkpoint](tui.md#rewind-and-checkpoints) the working tree before each prompt |
| `[sandbox]` | | See [Sandbox](sandbox.md) |
| `[harnesses.<id>]` | | Per-harness settings. Every harness takes `binary`, `default_model`, `default_effort`, `default_provider`, `default_policy`, `extra_args` and `sandbox_writable`; `protocol`, `command` and `display_name` are an ACP agent's (below). Any other key is one only its harness takes (below), and one it does not take is an error naming it. `<id>` is the harness's own name: a table no harness takes is an error, and so is one under another name for a harness (`claude-code`, `antigravity`, `mistral-vibe`), whose table is the one of its own name |
| `[harnesses.claude] relocate_config` | `true` | See [Sandbox](sandbox.md#things-to-know) |
| `[harnesses.codex] transport` | `auto` | `app-server`, or `exec` to force the fallback (Codex marks `app-server` experimental); any other value is an error. Under `exec` every turn is a process of its own, and what it left running ends with the turn (on Linux; elsewhere what it left in its process group) |
| `[harnesses.<name>] protocol = "acp"` | | With `command`, adds an [ACP agent](acp.md) under the table's name, or replaces the preset of that name. A built-in harness's table (`agy`, `claude`, `codex`, `pi`, `vibe`) stays that harness's with it. `acp` is the only protocol: any other value is an error, in any table |
| `[harnesses.<name>] command`, `display_name` | none | The agent's command line, e.g. `["gemini", "--acp"]`, and the name shown for it. Only a table with `protocol = "acp"` takes them, and `vibe`'s (in place of `vibe-acp` and its own name). `agy`, `claude`, `codex`, `pi` and a preset's table without `protocol = "acp"` run their own command under their own name, so there they are an error; another binary of the harness is set with `binary` |
| `[harnesses.<name>] modes` | none | ACP only: which of the agent's own session modes backs each policy, e.g. `{ ask = "ask", bypass = "auto-approve" }`; see [Modes](acp.md#modes). An agent with modes and no map gets only `bypass`. A workspace's table replaces the global one whole |
| `[harnesses.<name>] sign_in` | none (`vibe --setup` for `vibe`) | ACP only: the command that signs the agent in, shown when a session fails for want of sign-in; unharness never runs it. See [Sign-in](acp.md#sign-in) |
| `[harnesses.vibe.providers.<name>]` | none | A provider for Mistral Vibe: `api_base`, `models`, and optionally `api_style`, `backend`, `api_key_env_var`; see [Providers](vibe.md#providers). A workspace's provider of the same name replaces the global one. A table that is wrong stops unharness (`doctor` names it), and only `vibe` takes providers |
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
| Mistral Vibe | `agent` (vibe's own configuration) and each `[harnesses.vibe.providers.<name>]` of unharness's config | `VIBE_PROVIDERS`, `VIBE_MODELS` and `VIBE_ACTIVE_MODEL` in vibe's environment; see [Providers](vibe.md#providers) |

Credentials, region and project stay Claude Code's own (`AWS_REGION`,
`AWS_PROFILE`, `CLOUD_ML_REGION`, `ANTHROPIC_VERTEX_PROJECT_ID`,
`ANTHROPIC_FOUNDRY_RESOURCE`, ...), from the environment or the `env` of
Claude's settings. Two things to know:

- When the `env` of Claude's settings sets a `CLAUDE_CODE_USE_*` variable,
  that decides: Claude ignores the choice (it falls back to Anthropic's
  API when two are set), and unharness reports the provider the session
  runs on as an error.
- The provider is fixed when Claude or Codex starts, so changing it in
  the TUI restarts the session (resumed). It is
  refused while a turn or a subagent is at work. Mistral Vibe's is fixed
  when it starts too, and a session that was prompted keeps the model it
  ran, so there a change starts a new session told the conversation, as a
  model change does; a vibe subagent its agent has not waited for does
  not hold it back.

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
