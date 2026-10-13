# Allow rules: status notes

Allow rules, checked live on Claude Code 2.1.289 and Codex 0.157.0
(app-server): a shell rule written from a Claude `Bash` request answered
Codex's request for the same command in the same workspace, a command
joined to another with `&&` was still asked about, and an edit rule
answered Claude's next `Write`. Recorded: Codex's
`item/fileChange/requestApproval` has no changes of its own, only the
`itemId` of the `fileChange` item that does
(`fixtures/app_server_file_change.jsonl`). Unverified: codex-acp 2.1.1
asked for no permission in its default mode in three recordings (a shell
command, an edit, a network command), so the action of a non-Claude ACP
request is built from its tool calls' shapes (an `execute` call's
`rawInput.command` with at most a string `cwd`, or its `server` and
`tool` with at most `arguments`; any other key, such as `args` or `env`,
makes it `Opaque` (#213), since no recording had one; `diff` content
paths) and the spec's `kind`, and a read, move or delete is `Opaque`.
Read in codex-acp 2.1.1's source (`dist/index.js`, `permission()`), not
recorded: a permission request's input is `{command, cwd, url?,
additionalPermissions?}`, `url` for a network approval and
`additionalPermissions` for a sandbox escalation, which an allow rule
for the command answered before #213 and is now `Opaque`. Also
unverified: Codex's `move_path` on a rename, and a command or file
approval with `reason`, `grantRoot` or any field the recordings lack
(`Opaque`); the wording of Codex's MCP approval question, which the
parser checks for the server and tool (one recording); Claude's
`NotebookEdit` and `MultiEdit` inputs (field names from the tool schemas,
no request recorded). An ACP agent that offers `allow_always` and no
`allow_once` gets a cancel and the user a notice. A request Claude
cancels (`control_cancel_request`: a hook answered it, or it was
answered remotely) is withdrawn, which closes its modal. Not done: a
lock around appending to a rules file (two unharness saving at once can
lose one rule), re-reading rules while running.
