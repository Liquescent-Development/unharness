# Mistral Vibe

[Mistral Vibe](https://github.com/mistralai/mistral-vibe) (`vibe`, PyPI
`mistral-vibe`) is built in: unharness drives its `vibe-acp` over the
[Agent Client Protocol](acp.md), with what it knows about vibe on top.

```bash
uv tool install mistral-vibe   # or pip install mistral-vibe
vibe --setup                   # sign in once (or set MISTRAL_API_KEY)
unharness -H vibe
```

Nothing is needed in the config. A `[harnesses.vibe]` table sets its
`command`, `display_name`, `asks_permission`, `modes` and `sign_in` in
place of the built-in ones (see [ACP agents](acp.md#mistral-vibe)); the
rest stays.

unharness was checked against vibe 2.26.1, and `unharness doctor` warns
below it. Update with `uv tool upgrade mistral-vibe`.

## Signing in

vibe's default provider takes its key from `MISTRAL_API_KEY`: in the
environment, in `~/.vibe/.env` (`$VIBE_HOME/.env`), or in the OS keyring.
unharness looks for the first two to tell whether vibe is signed in
(`unharness doctor`, and which harness to start when none is chosen). It
only checks that the key is there: it never reads the value into anything,
shows it or copies it. A key in the keyring, or another provider configured
in vibe, is found once a session starts; a session that fails for want
of one says what vibe offers and to run `vibe --setup` (or the table's
`sign_in`), which unharness never runs itself.

## Policies

| Policy | vibe |
|---|---|
| `ask` | its `ask` mode: every write and command is asked about |
| `accept-edits` | its `ask` mode, and unharness answers the edits |
| `auto` | not offered yet; falls back to `accept-edits` |
| `bypass` | its `auto-approve` mode |
| `plan` | not offered: vibe's runtime hides its plan mode |

vibe starts every session in a mode of its own that writes files without
asking, so unharness puts each session in the mode above, and reads it back,
before the first prompt (see [Modes](acp.md#modes)).

"Always allow" rules work for vibe's edits and commands. vibe names a file
the way the model wrote it, often relative to the project; unharness takes
such a path as one in the session's directory, which is where vibe 2.26.1
writes it, and only when vibe's own description of the call names the same
file. A path with `..` or `~` in it, and a call to an MCP server's tool, is
asked about every time.

## Model and effort

`/model`, `/effort`, `default_model` and `default_effort` are never sent to
vibe over ACP, because vibe writes a choice made that way into your
`config.toml`. They are given to `vibe-acp` when it starts; see
[ACP agents](acp.md#mistral-vibe) for what that means for a running session.

## MCP servers

The stdio servers in unharness's `[mcp_servers]` are handed to each vibe
session. vibe 2.26.1 does not take http ones over ACP. A server's tools
show as `mcp_<server>.<tool>` in vibe and are asked about under `ask`.

## What vibe may write

With the sandbox on, vibe can write to the project and to its home,
`~/.vibe` (or `$VIBE_HOME`): its logs, each session's journal, caches and
locks. A traced turn of vibe-acp 2.26.1 wrote nothing else outside the
project, and a turn under the sandbox was denied nothing.

Its home also holds what decides how vibe runs, so unharness tells you
when one of these changes while vibe runs:

- `config.toml` (models, providers, MCP servers), `trusted_folders.toml`,
  `hooks.toml`, `AGENTS.md`, `skills.toml`;
- `.env`, compared by a digest: unharness keeps no copy of it;
- `agents/`, `tools/`, `skills/`, `plugins/`, `prompts/`;
- the caches vibe refreshes by itself (`experiment_eval_cache.json`,
  `whoami_cache.json`, `connector_bootstrap_cache.json`): vibe rewrites
  them with a new time when it fetches again, so only what the server
  answered is compared. They choose vibe's default model, its system
  prompt and the account's API address.

Session journals under `logs/` are not watched: vibe rewrites them on every
turn.

## Not checked yet

- macOS.
- vibe's plan mode, and its `smart-approve` mode for `auto`.
- A key in the OS keyring, and providers other than Mistral's.
- Plugins: none were installed, so whether vibe updates them by itself (a
  change unharness would report) is not known.
