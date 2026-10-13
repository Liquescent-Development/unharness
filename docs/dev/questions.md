# Questions: status notes

Questions (`PermissionKind::Question`, `tui/modal.rs` `QuestionModal`):
Claude Code 2.1.291 AskUserQuestion options carry `label`,
`description` and an optional `preview` (markdown), with and without a
preview in the same question (`fixtures/ask_previews.jsonl`). In
`-p` stream-json mode the tool is only offered with
`--permission-prompt-tool stdio`; without it the model finds no such
tool (`write_denied.jsonl` on 2.1.288, and a run on 2.1.291), which is
why the recorder passes it. A
multi-select answer sent as a JSON array reached the model as the
labels joined by a bare comma, unquoted (pty runs, 2.1.291 and
2.1.293, whose tool result writes the answer with a template literal),
so the transport sends a string joined the way Claude's own joiner
does (read in 2.1.293): by `", "`, a label containing `", "` or `"`
as a JSON string. The form has to be exactly that: 2.1.293 says the
questions were answered (rather than telling the model to read the
answers carefully) for an array of option labels, or for a string
that splits back into option labels and joins again to the same
string. Checked in the TUI (pty, 2.1.293): labels with `", "`, with
`"` and with both came back quoted, and the result said the questions
were answered. Codex 0.157.0's
`ToolRequestUserInputOption` (app-server schema) has only `label` and
`description`.
The modal sends only from its Submit page, and only once every question
has an answer; a lone single-select question is sent on `Enter`.
