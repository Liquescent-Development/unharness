# Sandbox: status notes

Sandbox (`src/core/sandbox/`), checked on Linux 7.2 (Landlock ABI 10) with
Claude Code 2.1.289, Codex 0.157.0 (app-server and exec), pi 0.87.1 and
agy 1.2.17 (`~/.gemini/antigravity-cli` writable; the one denial traced
was the updater's write test in `~/.local/bin`): a
workspace write succeeds, a write to the home directory and a read of
a denied credential directory fail, sessions resume. Landlock only allows, so reads are granted
on everything around the denied paths (`linux::read_grants`); directory
listing reaches into them, file contents do not. Codex's bubblewrap fails
inside a Landlock domain ("setting up uid map: Permission denied"), hence
`externalSandbox` on `turn/start`, `danger-full-access` for exec, and
`-c sandbox_mode="danger-full-access"` on the app-server command line
(without it the server warns at startup). Where ours does not run
(`--sandbox off`, or no backend) Codex's own sandbox holds to the level
the user wanted (`codex::OwnSandbox`, `Sandbox::wanted`), under every
policy: `bypass` is `--dangerously-bypass-approvals-and-sandbox` only
at `off`, otherwise `-s <level>` with `approval_policy="never"`.
Checked with `--print` on Codex 0.157.0 under a seccomp filter denying
the Landlock syscalls (`systemd-run -p SystemCallFilter=~landlock_*`):
with the default level and `bypass`, a write to the home directory was
refused ("read-only file system") and one in the workspace succeeded.
`auto` on `exec` is `--approve-for-me` only at workspace-write (the CLI
refuses it beside `-s`); at another level it runs as `-s <level>` with
Codex's default approvals, i.e. as `accept-edits`. Claude Code writes
`~/.claude.json` through a lock directory and a temp file created in the
home directory, both denied: it carries on without updating the file.
With `CLAUDE_CONFIG_DIR` set, the file, its lock and its temp files are
in that directory instead (checked: no denials, signed in from the
copied file), which is what `relocate_config` uses.
Landlock cannot protect a file in a directory where the CLI creates
files, and every CLI does so beside its config (Codex's SQLite files in
`~/.codex`, pi's lock files in `~/.pi/agent`, Claude's relocated config),
which is why those files are watched and not write-protected.
Unverified: the macOS backend (`seatbelt.rs`; only the profile text is
tested), a user-enabled Claude Code sandbox inside ours, agy's own
`--sandbox` flag (off unless given), the ACP presets' state directories, `--no-tui` passthrough under the sandbox.
