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
below it. Update with `uv tool upgrade mistral-vibe`. To read the
version, doctor runs `vibe-acp --version` with vibe's home pointed at an
empty directory of its own, which it removes afterwards, since vibe sets
up its home even for `--version`: doctor never creates `~/.vibe`.

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

When there is no key and no `~/.vibe` (`$VIBE_HOME`) at all, doctor says
vibe has not been set up.

## Trusted folders

vibe reads a project's own configuration only in folders you trust in
vibe (`~/.vibe/trusted_folders.toml`): elsewhere it ignores the folder's
`AGENTS.md`, its `.vibe/` (`config.toml`, hooks, tools, skills, agents,
prompts) and its `.agents/skills/`, so the project skills unharness puts
there are not loaded. Your own `~/.vibe` and `~/.agents/skills` are read
either way.

`unharness doctor` says whether the folder it runs in is trusted, by
which entry (a folder above it counts), and what vibe ignores there. When
a vibe session starts in a folder vibe does not trust and that has an
`AGENTS.md`, a `.vibe/` or `.agents/skills/`, unharness says so once: in
the TUI as a notice, with `--print` on stderr. To trust a folder, start
`vibe` in it and answer its prompt. unharness only reads the file: it
never trusts a folder for you, never passes `--trust` and never asks vibe
to trust one over its protocol, because a trusted folder's
`.vibe/config.toml` can choose vibe's agent, tools and MCP servers. An
agent that writes `trusted_folders.toml` (it is writable from the
sandbox, under vibe's home) is reported, as for vibe's other
configuration below.

Commands and subagents under `.agents/` ([Rules, skills, commands and
subagents](skills.md#commands-and-subagents)) are not projected for vibe.
vibe reads neither `.agents/commands` nor `.agents/agents`: its slash
commands are skills, its `.vibe/prompts/` are system prompts chosen by
name, and its own agents (`.vibe/agents/<name>.toml`) are configuration
profiles, one of which, named like a built-in, replaces vibe's own mode of
that name, `ask` included. unharness's `ask` and `accept-edits` are
profiles of its own (below), so such a replacement does not change them.

## Policies

| Policy | vibe |
|---|---|
| `ask` | unharness's profile `unharness-ask`: every write and command is asked about, whatever vibe's configuration allows (see [Your vibe configuration](#your-vibe-configuration)) |
| `accept-edits` | unharness's profile `unharness-accept-edits`: edits in the session's directory run, an edit vibe still asks about is answered by unharness when it is in the workspace and asked about otherwise, commands are asked about |
| `auto` | its `smart-approve` mode: a Mistral model decides what runs (see [Auto](#auto)) |
| `bypass` | its `auto-approve` mode |
| `plan` | not offered: vibe's runtime hides its plan mode |

vibe starts every session in a mode of its own that writes files without
asking, so unharness puts each session in the mode above, and reads it back,
before the first prompt (see [Modes](acp.md#modes)).

### Your vibe configuration

vibe's own `ask` mode runs a tool unasked when vibe's configuration says
so, and nothing tells unharness: `[tools.<name>] permission = "always"` or
an `allowlist` entry (what vibe's own "Always allow" writes into
`config.toml`), `bypass_tool_permissions = true`, the same in a trusted
project's `.vibe/config.toml` or in a `VIBE_*` variable, or a profile named
`ask` in `~/.vibe/agents/` that replaces vibe's own. So under `ask` and
`accept-edits` unharness runs vibe in profiles of its own instead, which
vibe applies above all of those:

- unharness writes `unharness-ask.toml` and `unharness-accept-edits.toml`
  under its state directory (`~/.local/state/unharness/vibe-agents/`), which
  nothing in the sandbox can write, and gives `vibe-acp` that directory in
  `VIBE_AGENT_PATHS`. Neither your vibe configuration nor `~/.vibe` is
  written.
- Each turns `bypass_tool_permissions` off and makes `write_file`, `edit`,
  the shells and every other tool your `config.toml`, the project's
  `.vibe/config.toml` or a `VIBE_TOOLS` variable names ask (a tool you set
  to `never` stays off). `unharness-accept-edits` lets `write_file` and
  `edit` run in the session's directory.
- Read-only commands still run without asking: the profile lists vibe
  2.26.1's own read-only commands (`ls`, `cat`, `grep`, `git diff`, …,
  without `git status`, which rewrites `.git/index`), and vibe still asks
  when one has a redirect, `$()`, `sudo`, a path outside the project or an
  option that writes (`sort -o`, `find -delete`).
- Each profile's description carries a code unharness keeps beside them.
  vibe takes a profile of the same name from an `agent_paths` directory
  your `config.toml` or a trusted project's config lists before
  unharness's. When such a directory holds an `unharness-ask.toml` or
  `unharness-accept-edits.toml` as vibe starts, or the session describes
  the mode otherwise, that policy is refused for the session (the TUI asks
  you to choose another, the refusal names the file) instead of running in
  that profile.

What vibe's admin configuration, set by your organisation and fetched
when vibe starts, enforces still wins over any profile: if it allows a
tool, vibe runs it unasked under these policies too.

### Auto

Under `auto` vibe runs in its `smart-approve` mode. Every call that vibe's
own rules would ask about goes first to a Mistral model
(`mistral-small-latest`), which is sent the call's arguments and part of
your latest message, and decides:

- what it judges safe runs without asking. In vibe 2.26.1 that included
  `rm -rf` of a directory in the project, and three of four writes
  outside the project that you had asked for (the fourth was asked
  about);
- what it judges risky and you did not ask for is blocked, and the call
  fails with its reason ("Smart approve blocked this action: …"), which
  the model sees and works around;
- the rest comes to you as a request: a call blocked once and tried
  again, a write to a file that decides what runs later (`.vibe/`,
  `.git/hooks`, shell startup files), or a call the model could not
  judge. unharness shows each one; only "allow once" or "deny" is sent
  back, and "always allow" makes an unharness rule.

unharness does not see the model's verdicts on the calls that ran, and
cannot make vibe ask about something it let through: what keeps a call
inside the project is the sandbox. smart-approve is a remote experiment
in vibe that only some accounts see; unharness starts `vibe-acp` with
`VIBE_SMART_APPROVE_AVAILABLE=true` so that every session offers it, and
under `auto` with `VIBE_DEFAULT_AGENT=smart-approve` so that a new session
starts in it (which, as with vibe's own `--smart-approve`, also overrides
a `disabled_agents` in your vibe config). A session that does not offer
it falls back to `accept-edits`, with a warning.

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

## Providers

vibe's own configuration (`config.toml`) can define providers and models,
and those are plain model choices in unharness (`/model`). A provider can
also be defined in unharness's config, for vibe alone, without touching
vibe's:

```toml
[harnesses.vibe.providers.lan]
api_base = "http://127.0.0.1:8080/v1"   # an OpenAI-compatible server
models   = ["qwen3-coder", "gemma-4"]   # the first runs when none is chosen
# api_style       = "openai"            # openai | reasoning | openai-responses | anthropic
# backend         = "generic"           # generic | mistral
# api_key_env_var = "LAN_API_KEY"       # the variable that holds the key; none is sent without it
```

Then `unharness -H vibe --provider lan` (or `default_provider = "lan"`, or
`/provider` in the TUI) runs vibe on it, and `--model` or `/model` picks
among its `models`; a model it does not list is refused. `agent` is vibe's
own configuration, and `/provider` lists it first to go back to it.

The provider is handed to `vibe-acp` in its environment when it starts
(`VIBE_PROVIDERS`, `VIBE_MODELS`, `VIBE_ACTIVE_MODEL`, as JSON) and is never
written to `config.toml`. vibe sees it as `unharness-<name>`, so that a
provider of the same name in your vibe configuration is left alone; the
models keep their names, and one with the name of a model of yours is
replaced by this one for that process. A chosen effort goes with the model
that runs. A change of provider restarts vibe on it, and once a session
was prompted starts a new session told the conversation so far, as a
model change does.

The key is only ever named: set the variable in the environment you start
unharness from. unharness never reads it, copies it or writes it anywhere.
The providers are read from unharness's own config directory, never from
the project: an agent cannot add one. A provider table that is wrong (a
missing `api_base` or `models`, an unknown key or style, an
`api_key_env_var` that is not a variable's name) stops unharness with the
table's name; `unharness doctor` names it too and shows no vibe until it
is fixed.

Checked with vibe 2.26.1 and an OpenAI-compatible server that needs no
key: the model answered in the TUI under the sandbox, ran a shell command,
and `config.toml` was unchanged. With a Mistral key at hand vibe still
reached Mistral's servers for its experiments, account and connectors in
such a session.

## MCP servers

The stdio servers in unharness's `[mcp_servers]` are handed to each vibe
session. vibe 2.26.1 does not take http ones over ACP. A server's tools
show as `mcp_<server>.<tool>` in vibe and are asked about under `ask`.

## Subagents

vibe's agent can start subagents: a generic one, or one of yours
(`~/.vibe/agents/<name>.toml` with `agent_type = "subagent"`). Each is
listed under the prompt from the moment it starts. vibe tells unharness
that one has ended, and its report, only when the agent waits for it: one
the agent does not wait for stays listed as at work until the session
ends, even once it has finished. Such a subagent does not keep the
status rule busy and does not hold back `/clear`, a change of model,
effort or provider, or a handoff summary: the session they end takes it
along, and its row then says it was stopped. Its own tool calls are not
shown (vibe does not send them), and it cannot be stopped from
unharness. Under `ask`
a subagent's writes and commands are asked about like the agent's own, but
vibe does not say what they are, so no allow rule answers them. Each shows
as a `tool` call that ends with your answer, since vibe does not report
what it did.
`unharness --print` does not wait for vibe's subagents.

## Rewind

`/rewind` rewinds vibe's own session: unharness asks vibe (its `_rewind/to`
extension, which is not part of ACP) to drop the chosen prompt and
everything after it, so vibe really no longer knows those turns, and the
session keeps its id. Files are left to unharness: vibe is told not to
restore any, and `f` in the picker restores them from unharness's own
checkpoints, as for every harness. Going back to before the session's
first prompt starts a fresh session, and so does a rewind after `/fork`
to a prompt from before the fork (a forked vibe session gives the turns it
inherited new ids). If vibe refuses a rewind, unharness starts a fresh
session with the conversation so far.

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

## vibe's own CLI

`unharness -p --native -H vibe` runs `vibe -p` and passes its output
through, and `unharness -H vibe --no-tui` starts `vibe` in its own
interface. Both run under the sandbox, in the session's directory, with
the policy as vibe's agent:

| Policy | `--agent` |
|---|---|
| `ask` | `unharness-ask`: its interface asks before every write and command; `vibe -p` denies them and goes on |
| `accept-edits` | `unharness-accept-edits`: edits in the session's directory run unasked, commands as under `ask` |
| `auto` | not offered; falls back to `accept-edits` |
| `bypass` | `auto-approve` |

The model and effort go to vibe by environment, as for a session. Only a
session says what vibe's own models are, so an effort is passed on only
for a model your `config.toml` defines; otherwise unharness says it is
left out. A provider chosen with `--provider` goes the same way as for a
session (see Providers). unharness's MCP servers are not passed on, and neither is
`--trust`: whether vibe reads the project's own configuration stays
vibe's and your decision. `--format` takes `text`, `json` and
`stream-json` (vibe's `streaming`), and the exit status is vibe's (see
[Headless runs](headless.md#the-harnesss-own-output)). unharness runs the
`vibe` installed beside `vibe-acp`, else the one on `PATH`.

The profiles are unharness's own, found through `VIBE_AGENT_PATHS` as for
a session (see [Your vibe configuration](#your-vibe-configuration)). There
is no session to describe the profile here, so unharness looks itself: a
run whose profile vibe would take from an `agent_paths` directory of your
vibe configuration, or whose configuration unharness cannot read, is
refused before vibe starts. "Always allow" in vibe's own interface still
writes the tool into vibe's `config.toml` (`[tools.write_file] permission =
"always"`); the profiles then ask about it anyway, so it holds only for the
rest of that run. unharness tells you when `config.toml` changes during a
session or a `-p` run, but not after `--no-tui`.

## Not checked yet

- macOS (no Mac was available).
- vibe's plan mode, which vibe 2.26.1 does not offer (see Policies).
- Pasting an image from the clipboard (attaching one with `/attach` was
  checked).
- Under `auto`: the classifier being unreachable (vibe then asks), and a
  provider other than Mistral's (vibe sends the classifier's model name
  to it).
- A subagent given more work, or stopped, by vibe's agent.
- A key in the OS keyring.
- Providers: a key named by `api_key_env_var`, styles other than `openai`,
  the `mistral` backend, and whether vibe calls Mistral in such a session
  without a Mistral key.
- Plugins: none were installed, so whether vibe updates them by itself (a
  change unharness would report) is not known.
