# Rules and skills

## Rules files

`AGENTS.md` is the one instructions file. `CLAUDE.md` and `GEMINI.md` are
symlinks to it, created by `unharness init` and refreshed by `unharness
sync` (and before each run, unless `auto_sync = false` or `--no-sync`).

A `CLAUDE.md` whose content differs from `AGENTS.md` is never overwritten:
unharness replaces a file with a symlink only when the contents are
identical, and warns otherwise.

## Skills

Skills live in `.agents/skills/`. `unharness skills <args…>` passes its
arguments to the [`skills`](https://github.com/vercel-labs/skills) CLI
(`npx skills`, which needs Node.js), which installs skills and projects
them into every agent's directory:

```bash
unharness skills add vercel-labs/agent-skills
unharness skills list
```

### Importing the skills you already have

`import` is unharness's own subcommand:

```bash
unharness skills import                    # pick from skills your harnesses already have
unharness skills import --from plugins -g  # only Claude plugin-bundled skills, globally
unharness skills import --all --dry-run    # show what would be imported
```

It scans:

- `~/.claude/skills` (and synced buckets) and Claude plugin caches;
- `~/.codex/skills` (`--include-system` adds the Codex built-ins);
- the Antigravity and pi skill directories;
- project-level `.claude`, `.codex` and `.pi` skill directories.

The chosen skills are installed with `skills add <path>`, so they land in
`.agents/skills` and are projected everywhere.

Plugins and pi extensions are specific to one harness, so they are not
imported; `unharness doctor` lists them instead.
