# Sandbox

unharness confines every harness process itself, whichever agent runs and
whatever the agent chooses to ask about. The confinement covers the
process and everything it starts, including shell commands and MCP
servers. A command you run yourself with `!` at the prompt
([Shell commands](tui.md#shell-commands)) gets the same confinement as
the active harness.

Codex sandboxes the commands its model runs, Claude Code's sandbox is
opt-in, and pi, Antigravity and ACP agents have none. Under `bypass`,
none of the vendors' sandboxes holds. unharness's sandbox puts the same
limits around every harness.

## What is allowed

By default a confined harness can:

- **write** only inside the workspace, the harness's own state directories
  (`~/.claude`, `~/.codex`, `~/.pi`, …; `unharness doctor` lists them), the
  temp directories, and herdr's runtime directory;
- **read** everything except credential locations: `~/.gnupg`, `~/.aws`,
  `~/.azure`, `~/.kube`, `~/.docker`, `~/.config/gh`, `~/.config/gcloud`,
  `~/.netrc`, `~/.npmrc`, `~/.pypirc`, `~/.git-credentials` and shell
  history;
- use the network freely.

| Level | Writes |
|---|---|
| `workspace-write` (default) | workspace, harness state, temp |
| `read-only` | harness state and temp only |
| `off` | unconfined |

Set the level with `--sandbox <level>`, `UNHARNESS_SANDBOX`,
`[sandbox] level` in the config, or `/sandbox` in the TUI. It is a setting
of its own: the default applies under every policy, `bypass` included,
and no policy picks a level. The level is shown on the status line and
holds for the run on every harness. A change in the TUI applies to the
next harness process, so the session restarts with the next prompt
(resumed where the harness can; nothing changes during a turn).

## Hiding more

Everything else your user can read, the agent can read too, other
projects included. `deny_read` under `[sandbox]` adds paths to the denied
list:

```toml
[sandbox]
deny_read = ["~/code"]          # hide the sibling projects of this one
```

The workspace, the harness's own state and `readable` paths stay readable
inside a denied path. So `deny_read = ["~"]` with a `readable` list of your
toolchains (and the agent's own binary, if it is installed under your home
directory) gives a strict mode.

A `deny_read` path inside a readable or writable one, such as a file in
the workspace or anything under `/tmp`, cannot be enforced and is refused
at startup. On Linux the names inside a denied directory can still be
listed, but their contents cannot be read.

## Backends

| Platform | Backend | Status |
|---|---|---|
| Linux | Landlock (kernel 6.2 or newer, no extra binary) | tested |
| macOS | `sandbox-exec` | not yet run on a Mac |

Where neither backend is available, the default level degrades to `off`
with a warning in the status area, in `doctor`, and on stderr in print
mode. An explicitly requested level is an error.

## Things to know

**Vendor sandboxes.** The vendors' own sandboxes cannot start inside this
one, so while it is active Codex is told the sandbox is external (its
approvals are unchanged). Where this one does not run (`--sandbox off`, or
no backend), Codex's own sandbox holds to the level you set under every
policy, and `bypass` switches it off only at `off`. One Codex quirk: on the
`exec` transport, `auto` has its automatic review only at
`workspace-write`, and runs as `accept-edits` at any other level.

**`~/.ssh` stays readable**, because git over ssh and commit signing need
it. Keep keys in an agent, or protect them with a passphrase, if that
matters to you.

**Extra writable paths.** An MCP server or extension that keeps data
elsewhere needs its directory in `writable`. In a git worktree the
repository's data lives outside the workspace, so add the main
repository's `.git` to `writable` to let the agent commit.

**Claude Code's `~/.claude.json`.** Claude Code updates this file by
creating files next to it in your home directory, which the sandbox cannot
allow without opening all of it. So unharness runs Claude with
`CLAUDE_CONFIG_DIR=~/.claude`, which puts the file inside its state
directory.

- Your `~/.claude.json` is copied there on the first run (a notice says
  so) and otherwise left alone.
- A plain `claude` outside unharness keeps using the original, so the two
  drift apart unless you export the same variable in your shell.
- `relocate_config = false` under `[harnesses.claude]` turns this off.
  Sessions still work, but "always allow" rules, trust and MCP approvals
  given in a confined session are forgotten after it.

**Self-updates.** A confined Claude Code cannot update itself, because its
installed binaries are not writable.

**Watched settings.** A harness's own state directory has to stay
writable, and its settings live there (`~/.claude/settings.json`,
`~/.codex/config.toml` and the Codex binary, `~/.pi/agent/settings.json`,
skills, hooks, MCP servers). The sandbox cannot stop an agent from
editing those, so unharness watches them instead:

- when one changes during a turn, the transcript (or stderr in print mode)
  says which;
- the version from before the session is saved under
  `~/.local/state/unharness/guard/`;
- unharness cannot tell a change the CLI made itself (saving an "always
  allow" rule, say) from one the agent made;
- `--no-tui` passthrough is not watched.

**Linux specifics.** A file replaced or created directly in your home
directory or in `~/.config` after the process started is not readable by
it until the next session. Programs that need to raise privileges
(`sudo`) do not work inside.

**unharness's own configuration**, the workspace's included, lives in
`~/.config/unharness`, which is never writable from inside. An agent
cannot change the settings its next run starts with.
