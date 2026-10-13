# Plan mode: status notes

Plan mode is `PermissionPolicy::Plan`, the least permissive policy,
driven on Claude only (#133). Claude Code 2.1.296
(`fixtures/plan_*.jsonl`):
- Started with `--permission-mode plan` (or `set_permission_mode
  plan`), it writes its plan to `~/.claude/plans/<slug>.md` without a
  request (writable in the sandbox as part of `~/.claude`). It runs
  read-only commands unasked.
- It asks before a write (its Write check returns `ask`, "Cannot write
  to … while in plan mode", before its own allow rules), which is why
  only a read rule answers anything under plan. A session launched with
  `--dangerously-skip-permissions` and then set to plan still asked
  (`plan_after_bypass`): its `plan` acts like bypass only in an
  interactive session (read in the binary, `lq`).
- `ExitPlanMode` comes as `can_use_tool` with `plan` and `planFilePath`
  (`PermissionKind::PlanApproval`, a modal of its own; no rule answers
  it).
- On approval Claude restores the mode it planned from
  (`prePlanMode ?? "default"`), but only if the mode is still `plan`.
  So the driver sends `set_permission_mode` with the chosen policy
  before the allow.
- `bypassPermissions` is refused in a session not launched with it
  (Claude's `error_code` `bypass_not_launched`, quoted in #142; no
  committed fixture has it), so the modal offers bypass only when the
  session was started under it (#142 for Ctrl+P).
- A deny reaches the model verbatim as the tool's error. A bare
  "Use Howdy" was followed by an edit to the plan and then the old
  change, so the transport words it as Claude's own interface does
  ("…To tell you how to proceed, the user said:\n…", or "STOP what you
  are doing…" with none), and haiku followed that.
- Every mode change, ours or the model's own `EnterPlanMode` (which
  asks nothing), comes as `system` `status` with `permissionMode`
  (`AgentEvent::PolicyChanged`). The TUI takes one it did not set as
  the harness's policy.
- Checked in the TUI (tmux, Landlock, haiku): reject with feedback and
  the plan was revised; approve under accept-edits and the edit ran
  unasked; a resume came back in plan.

Unverified: plan under `auto` before plan (the auto-mode gate falls back
to `default` on exit, read in the binary), `dontAsk` (no policy; a
notice), plan approval by a subagent (`awaitingLeaderApproval`).
`--print` and `--no-tui` do not offer plan.

Still declared, not driven (`Capabilities::plan_mode`):
- Codex 0.157.0 `collaborationMode` on `turn/start` (app-server schema;
  `exec` has none), read from the schema, not run;
- an ACP session's `modes` with id `plan` (claude-agent-acp 0.85.1 lists
  one, codex-acp 2.1.1 does not).

pi plans only through an extension, and agy's `--mode plan` does not
wait (see above).
