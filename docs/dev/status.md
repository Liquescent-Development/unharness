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
| agy | none | asked apart: `agy --print=/usage --output-format stream-json`'s `command_result` (below) |
| Codex `exec`, pi, codex-acp, vibe-acp | none | none |

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

## Telling the user

Parsers only report limits; what the user is told of them is
`core/rate_limits.rs`'s `LimitWatch`, one per harness in the
controller's store and one in a `--print` run (#344). Claude sends a
`rate_limit_event` with every API call of a turn, and from its warning
threshold on each says `allowed_warning`: a notice per event was a
flood. `LimitWatch` says a window once at 80%, again at 90% and 95%,
and after the window started over (its reset moved later by more than
ten minutes, or its use fell below 70%, so that use hovering at a step
is not said again); Claude's `allowed_warning` naming a window below
80% (`rateLimitType`, kept as `RateLimitInfo::window` when the status
is not `allowed`) is said once, and again only once the window started
over, since an event may name another window between two that name it.
A window whose reset is past is left out (`current`) until the CLI
reports it again, as is a status that named it. Starting over is read
off a reset that moved; a CLI whose reset rolled later with use would
be said again each time (not seen: Claude's and Codex's resets stayed
put within a window in the recordings).
A limit reached (`RateLimitInfo::reached`: a status other than
`allowed` and `allowed_warning`, or a window at 100%) is said once, as
an error unless extra usage covers it (`overage_covers`: Claude's
`isUsingOverage` or `overageStatus` `allowed`); in `--print` that is
a warning. Codex sends `"rateLimitReachedType": null` while no limit is
reached, read as `allowed`, so that a limit lifted is not still taken
for reached. Its
`rateLimitReachedType` values (0.162.1's schema: `rate_limit_reached`,
`workspace_{owner,member}_{credits_depleted,usage_limit_reached}`) name
no window: the one used most is taken, and the `workspace_` ones are
said in their words. Reset times are on the local clock
(`localtime_r`). The usage line shows `marker`, the window nearest its
limit, while there is one, in its colour also where the row is cut to
fit. No recording has `allowed_warning`, `rejected` or a reached Codex
limit; the shapes are 2.1.302's, seen live (#344), and the schema's,
and `claude/fixtures/rate_limit_warning.jsonl` is hand-written in
them.

## Asking

Rate limits otherwise come with a turn. Where a CLI can be asked
(`Capabilities::status_query`), `/status` asks when it opens and on
`r` (`SessionCommand::QueryStatus`, `Controller::query_status`): at
most once in ten seconds per harness, only while its session runs, and
says `refreshing…` until the answer comes or ten seconds pass.

| Harness | Request | Answer read |
|---|---|---|
| Claude Code (2.1.305) | control request `get_usage`, `skip_behaviors: true` | `rate_limits`' windows, `utilization` 0 to 100, `resets_at` ISO 8601 (`iso8601_to_unix`), `model_scoped` |
| Codex app-server (0.162.1) | `account/rateLimits/read`, `account/read` (`params: {}`), under the names `rate-limits` and `account` | as the notification; `account`'s `type`, `email`, `planType` |
| agy (1.3.6) | a run of `agy --print=/usage --output-format stream-json` apart (`Capabilities::status_probe`, `Harness::query_status`), in the session's sandbox, with or without a session: its print-mode reports are refused on stream-json input ("unavailable with --input-format stream-json; run it as its own --print /usage invocation") | `command_result`'s `command.data.groups[].buckets[]`: `remaining_fraction` 0 to 1 (read as used), `reset_time` ISO 8601, `window` (`weekly`); each bucket a window named after its group (`Gemini weekly`, `Claude and GPT weekly`) |

The probe is a list job of the TUI's (`ListRequest::Status`), on a
thread; `Controller::status_probed` takes its answer for the harness it
was asked of, also after a switch. A failed one is shown under Rate
limits ("could not be asked"), and `r` asks again. agy's `/credits`
answered `no credits info found` on the account checked (an individual
tier), so nothing of the account is read.

An error answer turns `status_query` off for that session, without an
error shown. Nothing is asked at session start, only when someone
looks: Codex sends its account and limits by itself then, and Claude
its account (`initialize`).

## Not done

- Codex's `account/usage/read` (lifetime and daily tokens), Claude's
  `get_status` (marked internal), and `get_usage`'s `extra_usage` and
  `spend`.
- agy's `/credits` (not answered on the account checked).
- pi's `get_session_stats` carries the session's token and cost totals;
  not read, since unharness sums the same per-response usage, and the
  totals would count a fork's parent turns.
