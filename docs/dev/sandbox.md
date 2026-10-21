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
On macOS a confined Claude may write the login keychain's files by name
(`~/Library/Keychains/login.keychain-db*` and `.fl<hex>`,
`SandboxPaths::writable_files`, a Seatbelt regex anchored to the
directory), so that a refreshed token is saved (#340); traced on a Mac,
see `src/harness/claude/NOTES.md`. Landlock leaves such grants out.
Landlock cannot protect a file in a directory where the CLI creates
files, and every CLI does so beside its config (Codex's SQLite files in
`~/.codex`, pi's lock files in `~/.pi/agent`, Claude's relocated config),
which is why those files are watched and not write-protected.
Unverified: the macOS backend (`seatbelt.rs`; only the profile text is
tested), a user-enabled Claude Code sandbox inside ours, agy's own
`--sandbox` flag (off unless given), the ACP presets' state directories, `--no-tui` passthrough under the sandbox.

## Grants from the TUI

`/sandbox allow <path>` and `allow-read <path>` widen the profile.
- **Where a grant lives.** One for the run is a `Grant` in
  `SandboxSetup::grants`, read by `runner::session_sandbox` beside
  `[sandbox].writable` and `readable`. A read grant lifts a `deny_read`
  entry of the same path, as a workspace's `readable` does.
- **Saved grants.** One saved is added to the workspace's or the global
  settings file by `Config::add_sandbox_path` (`toml_edit`, written
  atomically, a link stays one). It is added only once the global and
  workspace files, merged as a launch merges them, still make a sandbox,
  so the next run starts.
- **Restart.** A grant restarts the session at once, or at the end of the
  running turn (`Controller::sandbox_restart_due`). Landlock rules are set
  at `pre_exec` and cannot be widened afterwards.
- **What the dialog offers.** It offers what exists (`grants::grant_target`).
  It flags what would act outside the sandbox later (`grants::grant_risk`):
  shell startup files, `$PATH`, login items, git hooks, the harness's
  `guarded` files and credentials.
- **unharness's own directories.** `profile()` refuses a writable path
  inside unharness's own config or state directory, not only one around
  it. Before grants it took `[sandbox].writable =
  ["~/.config/unharness/workspaces"]`.
- **Noticing a block** (`sandbox::denials`). No CLI says "sandbox":
  each says "permission denied" in its own words. The shapes are
  recorded under a Landlock ruleset like unharness's (`setpriv
  --landlock-*` inside the bwrap recording setup) in each harness's
  `fixtures/*sandbox_denied.jsonl`. `candidates` reads paths from three
  places: the error lines (quoted, or absolute words), the input's
  `*path` keys, and a command's words when the error names none.
  `blocked` keeps one only if all three hold: the profile leaves it
  out, `access(2)` says unharness could reach it, and it is not
  unharness's own.
- **Offering.** The controller tells each path once a run, and offers
  it when the turn ends (`Front::offer_grant`). `--print` says it on
  stderr instead.
- **What is proposed.** `grants::propose` offers the path, or the first
  missing directory above it, made when it is granted, or for a new file
  the existing directory it goes in.
