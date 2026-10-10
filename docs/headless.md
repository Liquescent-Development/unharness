# Headless runs

```bash
unharness -p "Summarise src/"                       # the answer as text
unharness -p --format json "List the TODOs"         # one result object
unharness -p --format stream-json "Fix the test"    # one JSON object per line
git diff | unharness -p "Review this diff"          # the prompt on stdin
unharness -p --native --format json "…"             # the harness's own output
```

`-p` (`--print`) runs one prompt through the same session the TUI would
start: the same harness, policy, sandbox, MCP servers and allow rules.
The output is the same on every harness, ACP agents included. When no
prompt is given as arguments, it is read from stdin.

The run ends when the answer is complete: the turn is over, and every
subagent the agent started has ended. Claude Code reports each
background subagent that finishes in a turn of its own, after the turn
it was started in; the run waits for those turns too. While it waits,
unharness says so on stderr.

## Permissions

There is nobody to ask. A tool call that your [allow
rules](permissions.md#allow-rules) cover is allowed, as in the TUI; every
other call the harness asks about is denied, and the agent is told why.
A question from the agent (Claude's AskUserQuestion, a Codex input
request) is dismissed. Each denial is written to stderr and listed in
the result's `denied`.

What the policy lets a harness do without asking is not asked about, so
`--policy accept-edits` lets edits through and `--policy bypass` lets
everything through. A policy the harness does not have falls back as
in the TUI, and when nothing less permissive is there the run stops
with an error (`ask` on Antigravity, for example).

## Output

| `--format` | stdout |
|---|---|
| `text` (default) | the main agent's text as it streams; text separated by a tool call or a new turn starts a new paragraph |
| `json` | the result object (below), once, at the end |
| `stream-json` | one JSON object per line: `start`, then the session's events and unharness's answers, then the result |

stderr carries everything else: the sandbox level, warnings, denials,
and in `text` and `json` the harness's notices and errors.

Exit status: `0` when the answer is complete, `130` when interrupted,
`1` on an error (the harness failed the turn or exited early).

`Ctrl+C` interrupts the running turn and ends the run. A second one, or
one while only subagents are running, ends it at once. SIGTERM, SIGHUP
and `Ctrl+\` (SIGQUIT) end it too, with the harness and what it
started; under `nohup` a hangup changes nothing. `Ctrl+Z` stops the
harness and what it started along with unharness, and `fg` continues
them.

## Schema

Every line of `stream-json`, and the `json` object, has a `type`. The
`start` and `result` objects carry `schema`, the version of this format,
now `1`. It is raised when a line changes in a way that a reader of the
old one could misread. A new line type or a new field is not such a
change, so ignore what you do not know. Absent values are `null`.

### start

```json
{"type":"start","schema":1,"version":"0.8.0","harness":"claude","policy":"ask","sandbox":"workspace-write","cwd":"/path"}
```

`policy` is the one in effect after any fallback.

### Session events

| `type` | fields |
|---|---|
| `session_started` | `session_id`, `model` |
| `turn_started` | |
| `turn_anchor` | `id`: the harness's id for the user turn |
| `text_delta` | `text` |
| `thinking_delta` | `text` |
| `tool_call_started` | `id`, `name`, `input` (the complete input) |
| `tool_call_delta` | `id`, `name`, `delta` (live output, or input before `tool_call_started`) |
| `tool_call_result` | `id`, `output`, `is_error` |
| `permission_request` | `id`, `tool_call_id`, `kind`, and per kind the fields below |
| `permission_withdrawn` | `id` of a `permission_request` the harness stopped waiting for (its turn was interrupted, or a hook answered it) |
| `usage` | `input`, `output`, `cache_read`, `cache_write`, `cost_usd`, `cumulative` (whether the numbers are session totals rather than this turn's) |
| `context` | `used`, `window` (tokens) |
| `rate_limit` | `status`, `windows`: [{`label`, `used_percent`, `resets_at` (Unix seconds)}] |
| `plan_updated` | `entries`: [{`text`, `status`: `pending`, `in_progress` or `completed`}], `explanation` |
| `subagent_started` | `id`, `description`, `kind` |
| `subagent_progress` | `id`, `activity` |
| `subagent_ended` | `id`, `status`: `completed`, `failed` or `cancelled`, `result` (its report; may come in a second `subagent_ended`) |
| `sub` | `parent`: the subagent's `id`; `event`: an event of that subagent, in this same format |
| `hook_started` | `id`, `name` (the harness's, e.g. `PreToolUse:Bash`) |
| `hook_ended` | `id`, `name`, `outcome`: `succeeded`, `failed` or `blocked`; `output` (the reason for a block, else what it printed) |
| `capabilities_changed` | any of `effort_levels`, `image_input`, `resume_by_id`, `mcp_http`, `plan_mode`, `models`: [{`id`, `provider`, `name`, `description`, `effort_levels`}], `provider` (the one the session runs on), `commands`: [{`name`, `description`, `hint`, `aliases`}] (the harness's own, run by a prompt that starts with `/name`; each list replaces the last), `slash_commands` (whether such a prompt runs one) |
| `rewind_failed` | `reason` |
| `turn_completed` | `status`: `done`, `interrupted` or `error`; `error` |
| `policy_changed` | `policy`: the one the session runs under now (Claude reports each change of its mode, also one the model makes) |
| `notice` | `message` |
| `error` | `message` (the turn goes on) |
| `process_exited` | `code` |

`permission_request` kinds:

| `kind` | fields |
|---|---|
| `tool_use` | `tool`, `input`, `description`, `action` |
| `question` | `questions`: [{`id`, `header`, `text`, `options`: [{`label`, `description`, `preview`}], `allow_other`, `multi`}] |
| `confirm` | `title`, `message` |
| `select` | `title`, `options` |
| `input` | `title`, `placeholder`, `prefill`, `multiline` |
| `plan_approval` | `plan` (markdown), `plan_file`; always denied without the TUI |

`action` is what the call does, as allow rules see it, with a `kind`:
`shell` (`command`, `cwd`), `edit` (`paths`), `read` (`path`), `mcp`
(`server`, `tool`), `other`, or `opaque` (it cannot be told with
certainty).

### permission_answered

Follows each `permission_request`:

```json
{"type":"permission_answered","id":"…","decision":"allow","rules":["shell commands starting with `cargo test`"]}
```

`decision` is `allow` (by the `rules` listed), `deny` or `dismiss` (a
question).

### result

The `json` output, and the last `stream-json` line:

```json
{
  "type": "result",
  "schema": 1,
  "harness": "claude",
  "session_id": "33e5f578-…",
  "model": "claude-haiku-4-5-20251001",
  "status": "done",
  "error": null,
  "text": "…",
  "turns": 1,
  "usage": {"input": 18, "output": 249, "cache_read": 41805, "cache_write": 5199, "cost_usd": 0.0158, "cumulative": false},
  "denied": [{"tool": "Write", "action": {"kind": "edit", "paths": ["/path/x.txt"]}}],
  "warnings": ["…"],
  "duration_ms": 4641
}
```

- `status` is `done`, `interrupted` or `error`, with the reason in
  `error`. An error in any turn of the run is the run's.
- `text` is the main agent's text, as `--format text` prints it.
- `turns` counts the turns that ended, including ones the harness took to
  report a subagent.
- `usage` is summed over the run; `null` when the harness reported none.
- `warnings` holds what unharness wrote to stderr: a policy fallback, a
  missing sandbox, MCP servers left out, denials, and changes to the
  harness's own configuration during the run.

`session_id` can be passed to `--resume` to continue the vendor session
in another `-p` run.

## The harness's own output

`--native` runs the harness's own print command instead (`claude -p`,
`codex exec`, `pi --print`, `agy --print`) and passes its output through;
`--format` is then the harness's. Its policies are the ones that command
can hold to: `ask` is not available on Codex there, and pi blocks every
gated tool call. ACP agents have no such command.
