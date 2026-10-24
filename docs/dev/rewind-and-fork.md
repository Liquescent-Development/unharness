# Rewind and fork: status notes

Rewind and fork, all checked live: Claude `rewind_conversation` (targets
the uuid we put on each user message; refuses the session's first message
and uuids from the session a fork came from with "stale target", reported
as `RewindFailed`), `--resume X --fork-session`; Codex `thread/revert`
and `thread/fork` (turn ids survive a fork); pi `fork` (a new session,
only in place once it answers) and `--fork <id>` (entry ids survive);
ACP `session/fork` where the agent advertises it (vibe-acp 2.26.1,
codex-acp 2.1.1, claude-agent-acp 0.85.1; see [the ACP note](../../src/harness/acp/NOTES.md#forks)), of the
whole session only, with no rewind in the spec, so no anchors to carry
(vibe's own `_rewind/to` is in [the vibe note](../../src/harness/vibe/NOTES.md#rewind)).
Claude's `rewind_files` answers "File rewinding is not enabled" in this
mode, which is why file restore is unharness's own.

## A compaction's fold

A compaction's fold (#359) is the view's: the blocks it hides stay in
the transcript, so `/rewind` lists the prompts in it and `/fork` copies
them. A rewind to a folded prompt truncates the transcript there, and
the compaction, which comes after it, goes with the rest; the fold with
it (`rewinding_to_a_folded_turn_drops_the_compaction`). The vendor's own
rewind to a turn from before its compaction (Claude's
`rewind_conversation` to a uuid before a `compact_boundary`, pi's `fork`
from an entry before a compaction) was not run live.

What a compaction kept as it was is put on the transcript by the turn's
anchor (`Controller::kept_at`): the user turn the harness names by the id
its `TurnAnchor` gave, or the compaction itself when nothing was kept.
Since #359 an anchor also records the oldest block the turn's message
told the harness of (`TurnAnchorRecord::told_from`: a bridge, `!`
commands), which a compaction that keeps the turn keeps too, so the
fold ends there. An anchor saved before #359 does not say, and a
compaction keeping from its turn folds nothing. A turn whose anchor is
still owed (pi lists one after its turn) is waited for: the fold is put
on once it comes (`KeptAt::Pending`). A harness that says which turn a
message inside a turn is in (pi, from its entries: `Kept::InTurnOf`)
names the user messages up to it, latest first: the first that is one
of its anchors holds it (a steer is none). Anything else, a message
inside a turn otherwise or a turn unharness did not send, folds
nothing. Nothing kept
during a turn folds all before the compaction but what was sent into
that turn (steers, `Block::User::steer`), which the harness may not have
taken yet; the turns a fold counts are not steers either.

A compaction is of its harness (`HarnessId`), and folds only while that
harness is the one in use: one switched to was told the folded turns
by the bridge. It is retired (`Transcript::retire_compactions`) when
that harness's vendor session is left: forgotten (`forget_ids`: a
rewind or a renewal into a fresh session, a choice a stored session
cannot take), replaced (`SessionReplaced`), or started afresh by a fork.
A retired one folds nothing and its summary is what the harness knew
then; saved so (`CompactionRecord::retired`). It is saved on the notice
that tells of it (`BlockRecord::Notice::compaction`), so an unharness
before #359 reads that notice in its place and every index after it
(anchors, checkpoints, bridge bookmarks) still holds.

