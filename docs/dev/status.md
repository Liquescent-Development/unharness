# Account and rate-limit status

`/status` (`src/tui/app/status.rs`) shows what each harness's CLI said
of the account it runs on, beside the session's own figures
([user documentation](../tui.md#status)). This note is what each CLI
sends and where it is read.

## What each harness reports

| Harness | Account (`AgentEvent::Account`) | Rate limits (`AgentEvent::RateLimit`) |
|---|---|---|
| Claude Code | `initialize`'s answer, `account`: `subscriptionType`, `apiProvider`, `email`, `organization` | `rate_limit_event`'s `rate_limit_info`: `unifiedWindows` (utilization 0 to 1, `resetsAt` Unix seconds), `status`, `overageStatus`, `overageDisabledReason`, `isUsingOverage` (`overageResetsAt` named in 2.1.305, not recorded) |
| Codex app-server | `account/updated` (`authMode`, `planType`) at start; `planType` and `credits` of `account/rateLimits/updated` when they change | `account/rateLimits/updated`: `primary`/`secondary` (`usedPercent` 0 to 100, `windowDurationMins`, `resetsAt`), `limitId`, `rateLimitReachedType` |
| claude-agent-acp | none | `usage_update._meta["_claude/rateLimit"]`, Claude's `rate_limit_info` (read by the same `rate_limit_info`) |
| Codex `exec`, pi, agy, codex-acp, vibe-acp | none | none |

Where nothing is reported, the Account section falls back on what the
harness's probe found when unharness started (`HarnessOption::auth`,
the same check `doctor` prints), and says so.

The parsers emit what they read; the controller keeps it per harness in
`core/controller/account.rs`, merging each report into the last
(`AccountInfo::merge` keeps known fields, `RateLimitInfo::merge`
replaces windows by label, so a report that names only the window in
force keeps the others). The 80% notice is given there, once per
harness each time a window crosses it.

## Not saved

The account and its limits are not part of the conversation file: they
are the account's, out of date by the next run, and the email and
organization have no business in a file kept per workspace. Who the
account is also stays out of the `.events` summaries (`email=set`) and
of `--print`'s `account` event. `record-claude.py` redacts
`organization`, which the email pattern only caught when it was
`<email>'s Organization`.

## Not done

- Asking for the figures: they arrive with a turn, so before the first
  one `/status` says none have yet. Codex app-server answers
  `account/rateLimits/read`, `account/read` and `account/usage/read`
  (named in 0.162.1), and Claude 2.1.305 a `get_usage` control request
  (marked experimental, with `utilization` 0 to 100 and an ISO-8601
  `resets_at`; another branch answers "not supported in this context").
  Each needs a recording before unharness sends it.
- agy has usage, credit and quota reports in print mode
  (`--print=/usage`, `/credits`), not wired.
- pi's `get_session_stats` carries the session's token and cost totals;
  not read, since unharness sums the same per-response usage, and the
  totals would count a fork's parent turns.
- #344: Claude's per-event `rate limit: <status>` notice still floods
  the transcript; the per-harness store is where its warning belongs.
