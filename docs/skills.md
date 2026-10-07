# Rules, skills, commands and subagents

## Rules files

`AGENTS.md` is the one instructions file. `CLAUDE.md` and `GEMINI.md` are
symlinks to it, created by `unharness init` and refreshed by `unharness
sync` (and before each run, unless `auto_sync = false` or `--no-sync`).

A `CLAUDE.md` whose content differs from `AGENTS.md` is never overwritten:
unharness replaces a file with a symlink only when the contents are
identical, and warns otherwise. A regular `CLAUDE.md` with no `AGENTS.md`
becomes `AGENTS.md`, with `CLAUDE.md` linked to it. An `AGENTS.md` that is
itself a link is left as it is (one that leads nowhere is never written
through), and so is every link and file on its way: with `AGENTS.md ->
CLAUDE.md -> docs/rules.md`, `CLAUDE.md` keeps pointing at `docs/rules.md`.

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

## Commands and subagents

Custom slash commands and subagent definitions are written once under
`.agents/` and projected into each harness's project directory by
`unharness sync` (and before each run, like the rules files):

| Source | Claude Code | Codex | pi | Antigravity |
|---|---|---|---|---|
| `.agents/commands/<name>.md` | `.claude/commands/<name>.md` (link) | — (skills) | `.pi/prompts/<name>.md` (link) | — (skills) |
| `.agents/agents/<name>.md` | `.claude/agents/<name>.md` (link) | `.codex/agents/<name>.toml` (generated) | — | — |

A command is a prompt with optional `description` and `argument-hint`
frontmatter. Write `$ARGUMENTS` for what follows the command: it is the only
placeholder both harnesses read the same way (Claude Code's `$0` is the
first word, pi's `$1`). Codex and Antigravity have no command files of their
own; a skill in `.agents/skills` is a slash command there.

A subagent is a file in Claude Code's format:

```markdown
---
name: reviewer
description: Reviews a diff for bugs. Use it after a change.
tools: Read, Grep
---

You review code. …
```

Codex gets a role file with its `name`, `description` and the body as
`developer_instructions`; `tools` and `model` are left out, since their
values are Claude's. A definition without a `description` or a body is not
written for Codex, with a warning.

What is projected is a link to the source, or for Codex a file whose first
line says it was generated. Nothing else is replaced: a file of your own
in the way is left alone with a warning (one with the same content as its
source becomes a link). A link or generated file whose source is gone is
removed.

Sync never writes through a symbolic link out of the workspace: a vendor
directory that is one (`.codex` linked to `~/.codex`, say) gets nothing,
with a warning when there is something to project, since sync runs outside
the sandbox and would otherwise write wherever the link leads. One linked
to a directory inside the workspace (`.claude` to `config/claude`) is
followed. A source that is a link into the harness's own directory (sharing
`.claude/agents` "backwards" as `.agents/agents`) is not projected there,
since it is already the same file; the other harnesses still get it. A
file whose name holds a control character or `\` is not projected.

pi reads `.pi/prompts` only in a project it trusts (`pi --approve`, or once
trusted from pi's own interface); unharness does not trust it for you. In
the TUI a command the harness defines is not yet passed through
([#17](https://git.ldllc.dev/Liquescent/unharness/issues/17)); it works with
`--print`, `--no-tui` and the harness's own interface.

### Hooks

Hooks are not shared: each harness sends its hooks a payload of its own
(tool names, input fields), so a hook written for one would misread another's
calls, and one that guards something would let it through. Define them in
each harness's own settings. unharness shows the hooks Claude Code and Codex
(`app-server`) run, and what a hook that blocked or failed said.
