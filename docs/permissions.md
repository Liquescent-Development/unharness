# Permissions

unharness gives every harness the same four permission policies and its
own allow rules. Set the policy with `--policy`, `UNHARNESS_POLICY`,
`default_policy` in the config, `Ctrl+P` or `/policy`.

## Policies

| Policy | Claude Code | Codex (app-server) | Codex (exec) | pi | Antigravity |
|---|---|---|---|---|---|
| `ask` | default mode, prompts in the TUI | `untrusted`, prompts in the TUI | not available | every tool call but a read prompts in the TUI | not available |
| `accept-edits` | `acceptEdits` | `on-request` | no prompts | falls back to ask* | `--mode accept-edits`: edits run, a shell command is refused and ends the turn |
| `auto` | `auto` (classifier) | `on-request` | `--approve-for-me` | falls back to ask* | falls back to accept-edits* |
| `bypass` | `bypassPermissions` | `never` + full access | `--dangerously-bypass-approvals-and-sandbox` | nothing is asked, your extensions' dialogs are auto-accepted* | `--dangerously-skip-permissions` |

`*` shown as a warning in the header. For ACP agents, see
[ACP agents](acp.md#permissions).

### What `ask` guarantees

`ask` means the same on every harness that offers it: **nothing that
writes, runs a command or reaches out happens without your answer or one
of your allow rules.** Reads may run.

A harness that cannot hold to that does not offer `ask`:

- `codex exec` and headless Antigravity cannot prompt;
- `--print --native` on Codex uses `codex exec`, and Codex's own
  interface (`--no-tui`) does not accept the `untrusted` approval policy;
- an ACP agent offers `ask` only when you have marked it as one that asks
  ([ACP agents](acp.md#permissions)).

### Fallback

A requested policy that a harness does not have falls back to the nearest
**less** permissive one it has, never to a more permissive one. When there
is none (for example `ask`, the default, on a harness without it),
unharness does not choose for you:

- the TUI opens the policy picker and starts no session until you choose.
  That choice holds for that harness only, until you set another policy
  for the run;
- `--print` and `--no-tui` stop with an error naming the policies you can
  pass with `--policy`.

The fallback is for a policy named before you knew the harness:
`--policy`, `UNHARNESS_POLICY`, `default_policy`, or one set for the run
on another harness. What you choose in the TUI is what runs: the picker
lists every policy but greys out, and will not select, one the harness
does not have, and `/policy` with such a policy is an error naming the
ones it has.

### Resuming a conversation

A conversation keeps the policy you chose for it (`--policy`,
`UNHARNESS_POLICY`, `/policy`, or the picker where that policy was not
available), and `--resume`, `Ctrl+R` and `/resume` bring it back. A
policy named in the run you resume it in wins: on the command line, or
with `/policy` before `Ctrl+R`. A conversation that never had a policy
named follows `default_policy` as it is when you resume it, whatever the
conversation open before it ran under. When the policy a harness
resumes under is not the one it last ran under (a policy named in the
run, a changed default, a fallback), the transcript says so.

### `--print`

A [headless run](headless.md) has nobody to ask: what an allow rule
covers is allowed, every other request is denied (and the agent told
why), and a question is dismissed. Each denial is written to stderr and
listed in the result.

### pi

pi has no permission prompts of its own. unharness loads a small
extension into it (with `-e`, kept in unharness's state directory; your
own extensions still load) that asks before every tool call except those
named `read`, `grep`, `find` and `ls`, including tools from other
extensions.

- A path that pi would rewrite before using it (a leading `@` or `~`, a
  `file://` URL, a Unicode space) is always asked about, whatever your
  allow rules say.
- In a `--print` run under `ask` there is no one to ask, so those calls are
  denied unless an allow rule covers them (`--print --native`: all of
  them are blocked).

### Policy and sandbox are separate

The policy never chooses a [sandbox](sandbox.md) level. Codex's own sandbox
matters only where unharness's does not run, and then holds to the level
you set under every policy, `bypass` included.

## Allow rules

Pressing `a` in a permission dialog shows the rule that would allow this
kind of request from now on; `Enter` keeps it. The rule belongs to
unharness, so it still applies after `/harness` and in the next session,
whichever harness asks. The harness itself is only told "allow", and
nothing is written to its settings.

- `Tab` switches the rule between this workspace and every workspace.
- You can edit the pattern before keeping it, as long as it still covers
  the request in front of you.
- The proposal is narrow: the first words of each command, the directory
  of a file inside the workspace, or the one file outside it.

Rules are stored in `~/.config/unharness/allow.toml` and, per workspace,
in `~/.config/unharness/workspaces/<name>-<hash>.allow.toml`. `/allow` and
`unharness doctor` list them. To change or remove one, edit the file.
Rules are read at startup, so an edit takes effect from the next start.

```toml
[[allow]]
tool    = "shell"
command = "cargo test"       # the words the command starts with

[[allow]]
tool = "edit"                # or "read"
path = "src/**"              # *, ? and **; relative to the workspace, or /… or ~/…

[[allow]]
tool = "mcp"
name = "docs/search"         # server/tool, or docs/* for every tool of a server

[[allow]]
tool = "WebFetch"            # any other tool, by the harness's own name for it
```

### How rules match

**Tools.** `shell`, `edit`, `read` and `mcp` mean the same on Claude Code,
Codex (app-server) and ACP agents: Claude's `Bash`, Codex's `shell` with its
`/bin/zsh -lc '…'` wrapper, and an ACP `execute` call are all one thing to
a rule. Any other tool is matched by name, and names differ between
harnesses. An `mcp` rule names a server as the harness knows it, so the
same name in two harnesses' own settings may refer to two different
servers.

**Commands.** A command line of several commands (`&&`, `;`, `|`) is
allowed only when every one of them is covered. These are always asked
about:

- command substitution, `${…}`, or any `$` that is more than a variable
  name;
- redirection, a subshell, a comment, or an unusual blank;
- a command the harness says it would run outside the workspace.

**Paths.** A path is compared after following symbolic links, so a link
inside an allowed directory does not extend the rule to where it points.
A relative path in a request, and a link to a file that does not exist
yet, are always asked about.

**Uncertain requests.** When a request does not say exactly what it would
do, it is asked about: a Codex request with a `grantRoot` or a reason
attached, an ACP read, move or delete (which the protocol only describes),
or an MCP approval that cannot be tied to one call.

### Limits

- No rule allows a write or a read in unharness's own config and state
  directories, however wide its pattern.
- Rules only allow. They apply under every policy to every request that
  reaches unharness. What a harness decides without asking (see the policy
  table) never gets here, and neither does anything in `--print --native`
  and `--no-tui` runs.
- Every request a rule answers is noted in the transcript, together with
  the rules that answered it.
- A rules file that does not parse stops unharness, like a config file.
