# Hooks: status notes

Hooks (`HookStarted`/`HookEnded`, `Block::Hook`): Claude Code 2.1.292
sends `system` `hook_started` and `hook_response` for every hook under
`--include-hook-events` (`SessionStart` also without it), keyed by
`hook_id`, with `hook_name` (`PreToolUse:Bash`), `hook_event`, `stdout`,
`stderr`, `exit_code` and `outcome` (`fixtures/hooks.jsonl`). `outcome`
is `error` for any exit status but 0, also for exit 2, which blocked a
`PreToolUse`; a hook that blocks by printing a decision
(`permissionDecision: "deny"`, `decision: "block"` on `Stop`) is
`success` with exit 0 (`fixtures/hooks_json.jsonl`), so the parser reads
the printed JSON. Which events can be blocked is from Claude's
documentation; `PreToolUse`, `Stop` and `PermissionRequest` were
recorded blocking. A `PermissionRequest` hook runs beside the
`can_use_tool` request sent to unharness, and the first answer wins:
when the hook's `decision.behavior: "deny"` came first, Claude sent
`control_cancel_request` for the request and the call failed with the
hook's `message`; when unharness had allowed it first, the call ran and
the hook's deny came after (`fixtures/hooks_permission_{denied,late}`,
the first recorded with `--answer-delay`). The hook's lines name only
the tool (`PermissionRequest:Write`), but in all three recordings its
`hook_started` came right after the `can_use_tool` request it belongs
to (in the late one after unharness had answered), so the parser ties
a hook when it starts to the newest request for that tool without a
hook, decided ones included, and settles its deny by that request: one
already decided says how (a `control_cancel_request`: blocked; its
call's `tool_result`: too late), an open one holds the deny until one
of them comes. A deny whose hook was not seen to start is shown as
not known. A request is dropped once decided and its hook has
answered; at most 64 are kept, and a deny held for one dropped then is
shown as not known. Nothing is dropped at a `result`, since a
background subagent's request can be open across it. Two `Write` calls
in one message were asked about one after the other, each request
after the previous call's result
(`fixtures/hooks_permission_parallel`), so two requests open at once,
a deny for a request already decided and a cancel before the hook's
answer are only in unit tests. Unverified: two requests for one tool
sent before either hook starts (the hooks would be tied the wrong way
round), several `PermissionRequest` hooks on one request (the second
would take an older request for the tool that had no hook, or none,
and then its deny would show as not known), and whether an interrupt cancels an open request (a held deny
would then show as blocked).
Codex 0.157.0 `app-server` sends `hook/started` and `hook/completed`
with a `run` (`id`, `eventName` such as `preToolUse`, `status`:
`completed`, `blocked`, `failed`, `stopped`, and `entries` of `{kind,
text}`), `fixtures/app_server_hooks.jsonl`; a run's id is the same for
the `Stop` hook of every turn, and a hook's plain stdout is not in it.
Codex runs only hooks it trusts: `hooks/list` gives each a `key` and
`currentHash`, and `hooks.state.<key>.trusted_hash` set to that hash
(also through `-c` as one inline table, since keys hold dots) trusts it.
Its `Stop` hook must print JSON or it fails. Unverified: hook events
from `codex exec`, a sub-agent's hooks, `stopped`.
