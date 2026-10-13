# Commands and subagents under `.agents/`: status notes

Commands and subagents under `.agents/` (`sync/projections.rs`,
`docs/skills.md`), checked live: Claude Code 2.1.292 listed a linked
`.claude/commands/<name>.md` in `init`'s `slash_commands` and a linked
`.claude/agents/<name>.md` in `agents`, and expanded the command in `-p`
(also through `unharness --print`, sandboxed): `$ARGUMENTS` is all of
it, `$0` the first word. pi 0.87.1 expanded a linked `.pi/prompts/` one
only under `--approve` (`$1` the first word, `$0` empty); unharness
does not pass it, since that would let the workspace decide. Codex
0.157.0 (app-server) spawned a role from a generated
`.codex/agents/<name>.toml` in a project it called untrusted (it said
project config was off there), and without the file the same prompt
got an answer without the role's instructions. Codex has no command
files (custom prompts are gone; skills are its slash commands), agy's
`.agents/workflows` are deprecated in favour of skills, and its
`.agents/agents/<name>/agent.md` came back empty from `agy agent`
(unverified why), so neither is projected to. Hooks are not projected:
Claude's live in `settings.json`, Codex's in `.codex/hooks.json` behind
per-hook trust, agy's in `.agents/hooks.json` in a shape of its own, and
the payloads differ. Unverified: Claude's subdirectory namespacing
(`a/b.md` is `/a:b`; only direct children are projected). Not handled:
a directory on the way swapped for a link between the check and the
write (`O_NOFOLLOW` covers only the last component); a source under
`.agents/` that links to a file outside the workspace, which sync reads
unsandboxed (for a Codex role) and links to; links projected before a
later refusal of their directory, which stay; links projected before
their directory moved behind a link (`.claude` into `cfg/claude`, then
`.claude -> cfg/claude`), whose text is then a `..` short: they are
neither replaced nor removed, and are warned about on every sync as
linking elsewhere.
