//! What a harness is told of the conversation it did not see: the blocks
//! since it was last active, as text within a budget.
//!
//! The conversation is told turn by turn. Over budget, the older turns are
//! told more briefly before any is left out: first without their working
//! (the files edited, the calls that failed and the final answer stay),
//! then in a few lines. Turns are left out last, from the middle, and the
//! first prompt is always kept: it is usually the task.
//!
//! When the conversation does not fit, a handoff summary in it (the one a
//! harness wrote when the user switched away from it) stands in for
//! everything before it but the first prompt, cut to a share of the
//! budget, and only what came after is told turn by turn.
//! When it fits, summaries are left out: they say again what is there.

use serde_json::Value;

use super::shell::{CONTEXT_MAX_CHARS, context_entry};
use super::transcript::{Block, tool_summary, truncate_chars};
use crate::core::conversations::ShellStatus;

/// How much of a turn is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Detail {
    /// Everything said, every call with the part of its output that matters.
    Full,
    /// The prompt, what the calls changed or where they failed, the answer.
    Compact,
    /// A few lines.
    Brief,
    Omitted,
}

/// A prompt kept in full at most this long, once its turn is told briefly
/// (the first prompt excepted).
const COMPACT_PROMPT: usize = 2_000;
const BRIEF_PROMPT: usize = 300;
const COMPACT_ANSWER: usize = 2_000;
const BRIEF_ANSWER: usize = 300;
/// The end of a failed call's output, where the error usually is.
const FAILURE_LINES: usize = 20;
const FAILURE_CHARS: usize = 1_500;
/// A subagent's report, which is what the call was for.
const REPORT_CHARS: usize = 1_500;
/// Edited files listed by name in a brief turn.
const BRIEF_FILES: usize = 10;
/// Beside a handoff summary, the first prompt takes at most this fraction
/// (1/n) of the budget: the summary is asked for half of it.
const HANDOFF_PROMPT_SHARE: usize = 4;

/// The text of `blocks` for another harness, at most `max_chars` long, or
/// `None` when there is nothing in them to tell.
pub fn render(blocks: &[Block], max_chars: usize) -> Option<String> {
    let whole = tell(blocks, usize::MAX);
    if whole
        .as_ref()
        .is_none_or(|w| w.chars().count() <= max_chars)
    {
        return whole;
    }
    let Some(at) = last_handoff(blocks) else {
        return tell(blocks, max_chars);
    };
    let head = handoff_head(blocks, at, max_chars);
    let room = max_chars.saturating_sub(head.chars().count() + 2);
    let text = match tell(&blocks[at + 1..], room).filter(|_| room > 0) {
        Some(rest) => format!("{head}\n\n{rest}"),
        None => head,
    };
    Some(cut_middle(&text, max_chars))
}

/// Whether [`render`] would have to shorten what `blocks` say, with no
/// handoff summary in them to stand in for what it shortens.
pub fn needs_summary(blocks: &[Block], max_chars: usize) -> bool {
    let len = |text: Option<String>| text.map_or(0, |t| t.chars().count());
    if len(tell(blocks, usize::MAX)) <= max_chars {
        return false;
    }
    match last_handoff(blocks) {
        None => true,
        Some(at) => {
            let rest = len(tell(&blocks[at + 1..], usize::MAX));
            handoff_head(blocks, at, max_chars).chars().count() + 2 + rest > max_chars
        }
    }
}

fn last_handoff(blocks: &[Block]) -> Option<usize> {
    blocks
        .iter()
        .rposition(|b| matches!(b, Block::Handoff { .. }))
}

/// The first prompt before the summary at `at`, cut to its share of
/// `max_chars`, and the summary.
fn handoff_head(blocks: &[Block], at: usize, max_chars: usize) -> String {
    let mut head = String::new();
    if let Some(prompt) = blocks[..at].iter().find_map(|b| match b {
        Block::User { text } => Some(text),
        _ => None,
    }) {
        let prompt = truncate_chars(prompt, max_chars / HANDOFF_PROMPT_SHARE);
        head.push_str(&format!("User: {prompt}\n\n"));
    }
    if let Block::Handoff { text, sender, .. } = &blocks[at] {
        head.push_str(&format!(
            "[Handoff summary {sender} wrote of the conversation up to here]\n{text}"
        ));
    }
    head
}

/// [`render`] turn by turn, handoff summaries left out.
fn tell(blocks: &[Block], max_chars: usize) -> Option<String> {
    let mut turns = split(blocks);
    turns.retain(|t| t.has_content());
    if turns.is_empty() {
        return None;
    }
    let first = turns.iter().position(|t| t.prompt.is_some());
    let last = turns.len() - 1;
    let mut details = vec![Detail::Full; turns.len()];
    let mut texts: Vec<String> = turns
        .iter()
        .enumerate()
        .map(|(i, t)| t.render(Detail::Full, Some(i) == first))
        .collect();

    // Oldest first, the newest turn last of all.
    let older = 0..last;
    let steps = older
        .clone()
        .map(|i| (i, Detail::Compact))
        .chain(older.clone().map(|i| (i, Detail::Brief)))
        .chain([(last, Detail::Compact)])
        .chain(
            older
                .filter(|i| Some(*i) != first)
                .map(|i| (i, Detail::Omitted)),
        )
        .chain([(last, Detail::Brief)]);
    for (i, detail) in steps {
        if assemble(&texts, &details).chars().count() <= max_chars {
            break;
        }
        details[i] = detail;
        texts[i] = turns[i].render(detail, Some(i) == first);
    }
    let text = assemble(&texts, &details);
    Some(cut_middle(&text, max_chars))
}

/// The turns told, with a line for each run of turns left out.
fn assemble(texts: &[String], details: &[Detail]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut omitted = 0usize;
    for (text, detail) in texts.iter().zip(details) {
        if *detail == Detail::Omitted {
            omitted += 1;
            continue;
        }
        if omitted > 0 {
            parts.push(omission(omitted));
            omitted = 0;
        }
        parts.push(text.clone());
    }
    if omitted > 0 {
        parts.push(omission(omitted));
    }
    parts.join("\n\n")
}

fn omission(n: usize) -> String {
    match n {
        1 => "[… 1 turn omitted]".to_string(),
        n => format!("[… {n} turns omitted]"),
    }
}

/// At most `max` characters: the start and the end, the middle left out.
fn cut_middle(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    const MARK: &str = "\n[…]\n";
    let room = max.saturating_sub(MARK.chars().count());
    let head = room / 3;
    let tail = room - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(len - tail).collect();
    format!("{start}{MARK}{end}")
}

/// A prompt and what followed it, up to the next prompt. The blocks before
/// the first prompt are a turn without one.
struct Turn<'a> {
    prompt: Option<&'a str>,
    blocks: &'a [Block],
}

fn split(blocks: &[Block]) -> Vec<Turn<'_>> {
    let mut turns = Vec::new();
    let mut start = 0;
    for (i, b) in blocks.iter().enumerate() {
        if matches!(b, Block::User { .. }) && i > start {
            turns.push(Turn::of(&blocks[start..i]));
            start = i;
        }
    }
    if start < blocks.len() {
        turns.push(Turn::of(&blocks[start..]));
    }
    turns
}

impl<'a> Turn<'a> {
    fn of(blocks: &'a [Block]) -> Self {
        match blocks.first() {
            Some(Block::User { text }) => Turn {
                prompt: Some(text),
                blocks: &blocks[1..],
            },
            _ => Turn {
                prompt: None,
                blocks,
            },
        }
    }

    fn has_content(&self) -> bool {
        self.prompt.is_some() || self.blocks.iter().any(|b| told(b).is_some())
    }

    fn render(&self, detail: Detail, first: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(prompt) = self.prompt {
            let cap = match detail {
                _ if first => usize::MAX,
                Detail::Full => usize::MAX,
                Detail::Compact => COMPACT_PROMPT,
                Detail::Brief | Detail::Omitted => BRIEF_PROMPT,
            };
            parts.push(format!("User: {}", truncate_chars(prompt, cap)));
        }
        match detail {
            Detail::Full => parts.extend(self.blocks.iter().filter_map(full)),
            Detail::Compact => parts.extend(self.compact()),
            Detail::Brief => parts.extend(self.brief()),
            Detail::Omitted => {}
        }
        parts.join("\n\n")
    }

    /// What the calls changed and where they failed, the subagents'
    /// reports, the commands the user ran, and the last answer.
    fn compact(&self) -> Vec<String> {
        let mut parts = Vec::new();
        let mut others = 0usize;
        for b in self.blocks {
            match told(b) {
                Some(Told::Tool(call)) => match call.outcome() {
                    Outcome::Failed => parts.push(call.failure()),
                    Outcome::Agent(_) => parts.push(call.line(&call.report(3))),
                    Outcome::Edited(_) => {}
                    Outcome::Ok => others += 1,
                },
                Some(Told::Shell { command, status }) => {
                    parts.push(format!(
                        "User ran a shell command: $ {command} [{}]",
                        status.label()
                    ));
                }
                _ => {}
            }
        }
        let edited = self.edited();
        if !edited.is_empty() {
            parts.insert(0, format!("[files edited: {}]", edited.join(", ")));
        }
        if others > 0 {
            parts.push(format!("[{others} other tool call(s)]"));
        }
        parts.extend(self.answer(COMPACT_ANSWER));
        parts
    }

    /// The files edited, how many calls there were and failed, the start of
    /// the last answer.
    fn brief(&self) -> Vec<String> {
        let mut parts = Vec::new();
        let edited = self.edited();
        if !edited.is_empty() {
            let mut list = edited
                .iter()
                .take(BRIEF_FILES)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if edited.len() > BRIEF_FILES {
                list.push_str(&format!(" and {} more", edited.len() - BRIEF_FILES));
            }
            parts.push(format!("[files edited: {list}]"));
        }
        let (mut calls, mut failed) = (0usize, 0usize);
        for b in self.blocks {
            match told(b) {
                Some(Told::Tool(call)) => {
                    calls += 1;
                    if call.outcome() == Outcome::Failed {
                        failed += 1;
                    }
                }
                Some(Told::Shell { .. }) => calls += 1,
                _ => {}
            }
        }
        if calls > 0 {
            parts.push(match failed {
                0 => format!("[{calls} tool call(s)]"),
                f => format!("[{calls} tool call(s), {f} failed]"),
            });
        }
        parts.extend(self.answer(BRIEF_ANSWER));
        parts
    }

    /// The last thing the agent said in the turn.
    fn answer(&self, cap: usize) -> Option<String> {
        self.blocks.iter().rev().find_map(|b| match b {
            Block::Assistant { text, sender, .. } if !text.trim().is_empty() => {
                Some(format!("{sender}: {}", truncate_chars(text.trim(), cap)))
            }
            _ => None,
        })
    }

    /// Every file a call that did not fail edited, once, in order.
    fn edited(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for b in self.blocks {
            if let Some(Told::Tool(call)) = told(b)
                && let Outcome::Edited(paths) = call.outcome()
            {
                for p in paths {
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        }
        out
    }
}

/// A block another harness is told about.
enum Told<'a> {
    Said,
    Tool(Call<'a>),
    /// A `!` command an agent has been told about already (until then it
    /// goes in front of the next prompt, whichever agent that is for).
    Shell {
        command: &'a str,
        status: &'a ShellStatus,
    },
}

fn told(b: &Block) -> Option<Told<'_>> {
    match b {
        Block::Assistant { text, .. } if !text.trim().is_empty() => Some(Told::Said),
        Block::Tool {
            name,
            input,
            output,
            is_error,
            agent,
            ..
        } => Some(Told::Tool(Call {
            name,
            input,
            output,
            is_error: *is_error,
            agent: agent.as_deref(),
        })),
        Block::Shell {
            command,
            status,
            sent: true,
            ..
        } if !matches!(status, ShellStatus::Running | ShellStatus::Failed { .. }) => {
            Some(Told::Shell { command, status })
        }
        _ => None,
    }
}

/// A block in full.
fn full(b: &Block) -> Option<String> {
    match b {
        Block::Assistant { text, sender, .. } if !text.trim().is_empty() => {
            Some(format!("{sender}: {text}"))
        }
        Block::Tool { .. } => match told(b) {
            Some(Told::Tool(call)) => Some(call.full()),
            _ => None,
        },
        Block::Shell {
            command,
            output,
            dropped,
            status,
            ..
        } if told(b).is_some() => Some(format!(
            "User ran a shell command:\n{}",
            context_entry(command, output, *dropped, status, CONTEXT_MAX_CHARS)
        )),
        _ => None,
    }
}

struct Call<'a> {
    name: &'a str,
    input: &'a Value,
    output: &'a str,
    is_error: bool,
    agent: Option<&'a super::transcript::AgentRun>,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Failed,
    /// It spawned a subagent: its status.
    Agent(&'static str),
    /// It changed these files.
    Edited(Vec<String>),
    Ok,
}

impl Call<'_> {
    fn outcome(&self) -> Outcome {
        if let Some(run) = self.agent {
            return Outcome::Agent(match run.status {
                None => "running",
                Some(s) => s.label(),
            });
        }
        if self.is_error {
            return Outcome::Failed;
        }
        let paths = edited_paths(self.name, self.input);
        if paths.is_empty() {
            Outcome::Ok
        } else {
            Outcome::Edited(paths)
        }
    }

    fn line(&self, out: &str) -> String {
        let status = match self.outcome() {
            Outcome::Agent(s) => s,
            Outcome::Failed => "error",
            _ => "ok",
        };
        let head = format!(
            "[tool {} {} → {status}]",
            self.name,
            tool_summary(self.name, self.input)
        );
        if out.is_empty() {
            head
        } else {
            format!("{head} {out}")
        }
    }

    /// The call with the part of its output that matters: none for an edit
    /// or a read (the file says it better), the end for a failure or a
    /// command, a subagent's report rather than the call's launch receipt.
    fn full(&self) -> String {
        match self.outcome() {
            Outcome::Failed => self.failure(),
            Outcome::Agent(_) => self.line(&self.report(usize::MAX)),
            Outcome::Edited(_) => self.line(""),
            Outcome::Ok if reads(self.name) => self.line(""),
            Outcome::Ok if runs(self.name) => self.line(&last_lines(self.output, 3, 200)),
            Outcome::Ok => self.line(&first_lines(self.output, 3)),
        }
    }

    /// The call and the end of its output, line by line.
    fn failure(&self) -> String {
        let head = self.line("");
        let tail = tail(self.output, FAILURE_LINES, FAILURE_CHARS);
        if tail.is_empty() {
            head
        } else {
            format!("{head}\n{tail}")
        }
    }

    /// The end of the subagent's report, on one line when `lines` is small.
    fn report(&self, lines: usize) -> String {
        let report = self.agent.map(|r| r.report()).unwrap_or("");
        if lines == usize::MAX {
            tail(report, usize::MAX, REPORT_CHARS)
        } else {
            last_lines(report, lines, 200)
        }
    }
}

/// The files a call edits, where its input names them.
fn edited_paths(name: &str, input: &Value) -> Vec<String> {
    let field = |keys: &[&str]| -> Vec<String> {
        keys.iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
            .filter(|s| !s.trim().is_empty())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
    };
    match name {
        // Claude Code, pi and the ACP kind.
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "write" | "edit" => {
            field(&["file_path", "notebook_path", "path", "filename"])
        }
        // agy.
        "write_to_file" | "replace_file_content" | "multi_replace_file_content" => {
            field(&["TargetFile"])
        }
        // Codex: {"changes": [{"path", "kind", "diff"}]}.
        "apply_patch" | "file_change" => input
            .get("changes")
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .filter_map(|c| c.get("path").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A call whose output is a file's content or a list of names.
fn reads(name: &str) -> bool {
    matches!(name, "Read" | "read" | "view_file" | "Glob" | "glob")
}

/// A call that runs a command, whose output ends with what it came to.
fn runs(name: &str) -> bool {
    matches!(
        name,
        "Bash"
            | "bash"
            | "shell"
            | "command_execution"
            | "exec_command"
            | "execute"
            | "run_command"
    )
}

/// The last non-blank lines of `s`, at most `max_chars`, as they were.
fn tail(s: &str, lines: usize, max_chars: usize) -> String {
    let all: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let skipped = all.len().saturating_sub(lines);
    let mut out = all[skipped..].join("\n");
    let len = out.chars().count();
    let mut cut = skipped > 0;
    if len > max_chars {
        out = out.chars().skip(len - max_chars).collect();
        cut = true;
    }
    if cut {
        out.insert_str(0, "… ");
    }
    out
}

/// The last `n` non-blank lines of `s` on one line.
fn last_lines(s: &str, n: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out = lines[lines.len().saturating_sub(n)..].join(" / ");
    if lines.len() > n {
        out.insert_str(0, "… ");
    }
    truncate_chars(&out, max_chars)
}

/// The first `n` lines of `s` on one line.
fn first_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().take(n).collect();
    let mut out = lines.join(" / ");
    if s.lines().count() > n {
        out.push_str(" …");
    }
    truncate_chars(&out, 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::transcript::Transcript;
    use serde_json::json;
    use std::time::Duration;

    fn turn(t: &mut Transcript, prompt: &str, answer: &str) {
        t.push_user(prompt);
        t.append_assistant("Claude", answer);
        t.finish_turn(Duration::ZERO);
    }

    fn call(t: &mut Transcript, id: &str, name: &str, input: Value, out: &str, err: bool) {
        t.tool_started(id, name, input);
        t.tool_result(id, out, err);
    }

    #[test]
    fn everything_fits_in_full() {
        let mut t = Transcript::default();
        t.push_user("fix the build");
        t.append_assistant("Claude", "Looking.");
        call(
            &mut t,
            "1",
            "Read",
            json!({"file_path": "/w/a.rs"}),
            "fn main() {}\n",
            false,
        );
        call(
            &mut t,
            "2",
            "Edit",
            json!({"file_path": "/w/a.rs"}),
            "updated",
            false,
        );
        call(
            &mut t,
            "3",
            "Bash",
            json!({"command": "cargo test"}),
            "compiling\nrunning 3 tests\ntest result: ok",
            false,
        );
        let fail = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        call(&mut t, "4", "Bash", json!({"command": "make"}), &fail, true);
        t.append_assistant("Claude", "Done.");
        let text = render(&t.blocks, 100_000).unwrap();
        assert_eq!(
            text,
            "User: fix the build\n\n\
             Claude: Looking.\n\n\
             [tool Read /w/a.rs → ok]\n\n\
             [tool Edit /w/a.rs → ok]\n\n\
             [tool Bash cargo test → ok] compiling / running 3 tests / test result: ok\n\n\
             [tool Bash make → error]\n… "
                .to_string()
                + &(11..=30)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n")
                + "\n\nClaude: Done."
        );
    }

    #[test]
    fn older_turns_shrink_before_any_is_left_out_and_the_first_prompt_stays() {
        let mut t = Transcript::default();
        t.push_user(format!("the task: {}", "t".repeat(500)));
        call(
            &mut t,
            "e",
            "Write",
            json!({"file_path": "/w/new.rs"}),
            "ok",
            false,
        );
        call(
            &mut t,
            "f",
            "Bash",
            json!({"command": "cargo build"}),
            "error[E0425]",
            true,
        );
        call(
            &mut t,
            "r",
            "Read",
            json!({"file_path": "/w/x"}),
            "x",
            false,
        );
        t.append_assistant("Claude", &format!("first answer {}", "a".repeat(3000)));
        t.finish_turn(Duration::ZERO);
        for i in 1..10 {
            turn(
                &mut t,
                &format!("question {i} {}", "q".repeat(400)),
                &format!("answer {i} {}", "b".repeat(400)),
            );
        }
        let full = render(&t.blocks, usize::MAX).unwrap();

        // A little short: the oldest turn loses its working first.
        let text = render(&t.blocks, full.chars().count() - 100).unwrap();
        assert!(text.contains(&"t".repeat(500)));
        assert!(text.contains("[files edited: /w/new.rs]"));
        assert!(text.contains("[tool Bash cargo build → error]\nerror[E0425]"));
        assert!(text.contains("[1 other tool call(s)]"));
        assert!(!text.contains("[tool Read"));
        assert!(text.contains(&format!("answer 9 {}", "b".repeat(400))));

        // Far short: turns go from the middle, the first prompt and the
        // newest turn stay.
        let text = render(&t.blocks, 2_000).unwrap();
        assert!(text.chars().count() <= 2_000, "{}", text.chars().count());
        assert!(text.starts_with(&format!("User: the task: {}", "t".repeat(500))));
        assert!(text.contains("[files edited: /w/new.rs]"));
        assert!(text.contains("turns omitted]"));
        assert!(text.contains("question 9"));
        assert!(!text.contains("question 1 "));
    }

    #[test]
    fn a_single_turn_too_long_keeps_its_start_and_end() {
        let mut t = Transcript::default();
        // Told briefly, the answer is cut; a first prompt never is, but
        // for the budget itself.
        turn(
            &mut t,
            &format!("go {}", "p".repeat(10_000)),
            &"z".repeat(10_000),
        );
        let text = render(&t.blocks, 1_000).unwrap();
        assert_eq!(text.chars().count(), 1_000);
        assert!(text.starts_with("User: go"));
        assert!(text.contains("\n[…]\n"));
        assert!(text.ends_with("z…"));
    }

    #[test]
    fn codex_and_agy_edits_are_named() {
        assert_eq!(
            edited_paths(
                "apply_patch",
                &json!({"changes": [{"path": "/w/a"}, {"path": "/w/b"}]})
            ),
            ["/w/a", "/w/b"]
        );
        assert_eq!(
            edited_paths("write_to_file", &json!({"TargetFile": "/w/x.txt"})),
            ["/w/x.txt"]
        );
        assert!(edited_paths("Read", &json!({"file_path": "/w/a"})).is_empty());
    }

    #[test]
    fn a_handoff_summary_stands_in_for_what_does_not_fit() {
        let mut t = Transcript::default();
        turn(&mut t, "the task", &"a".repeat(3_000));
        for i in 1..5 {
            turn(&mut t, &format!("question {i}"), &"b".repeat(3_000));
        }
        t.append_assistant("Claude", "Goal: the task. Done: a, b. Next: c.");
        assert!(t.mark_handoff(t.blocks.len() - 1, "Codex"));
        turn(&mut t, "after the switch", "fine");

        // It fits: the turns themselves, the summary left out.
        let whole = render(&t.blocks, usize::MAX).unwrap();
        assert!(!whole.contains("Handoff summary"));
        assert!(whole.contains("question 4"));

        assert!(!needs_summary(&t.blocks, usize::MAX));
        assert!(!needs_summary(&t.blocks, 2_000));
        assert!(needs_summary(&t.blocks, 100));
        assert!(needs_summary(&t.blocks[..t.blocks.len() - 3], 2_000));

        // It does not: the first prompt, the summary, what came after.
        let text = render(&t.blocks, 2_000).unwrap();
        assert_eq!(
            text,
            "User: the task\n\n\
             [Handoff summary Claude wrote of the conversation up to here]\n\
             Goal: the task. Done: a, b. Next: c.\n\n\
             User: after the switch\n\nClaude: fine"
        );
    }

    #[test]
    fn a_first_prompt_too_long_for_the_bridge_is_cut_beside_a_summary() {
        let mut t = Transcript::default();
        turn(&mut t, &format!("the task {}", "p".repeat(5_000)), "ok");
        t.append_assistant("Claude", "Goal: the task. Next: c.");
        assert!(t.mark_handoff(t.blocks.len() - 1, "Codex"));
        turn(&mut t, "after the switch", "fine");

        // A summary is worth asking for, and once there it makes room.
        assert!(needs_summary(&t.blocks[..2], 2_000));
        assert!(!needs_summary(&t.blocks, 2_000));
        let text = render(&t.blocks, 2_000).unwrap();
        assert!(text.chars().count() <= 2_000, "{}", text.chars().count());
        assert!(text.starts_with("User: the task ppp"));
        assert!(text.contains("p…\n\n[Handoff summary Claude wrote"));
        assert!(text.contains("Goal: the task. Next: c."));
        assert!(text.ends_with("User: after the switch\n\nClaude: fine"));
        assert!(!text.contains("\n[…]\n"));
    }

    #[test]
    fn nothing_to_tell() {
        let mut t = Transcript::default();
        t.push_notice("n");
        t.append_thought("hm");
        assert!(render(&t.blocks, 1000).is_none());
    }
}
