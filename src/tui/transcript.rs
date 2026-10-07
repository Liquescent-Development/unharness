//! The conversation as rendered blocks, plus the text bridged into another
//! harness when the user switches mid-conversation.

use std::time::{Duration, Instant};

use serde_json::Value;

use super::code::sanitize;
use crate::core::SubagentStatus;
use crate::core::conversations::{AgentRecord, BlockRecord, ShellStatus};

/// A subagent's work, kept on the tool call that spawned it. The call
/// itself may be long done (a background launch returns at once). What the
/// subagent did and wrote is a transcript of its own, shown on request, so
/// that several at work do not flood the main one.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub description: String,
    pub kind: Option<String>,
    /// `None` while it runs.
    pub status: Option<SubagentStatus>,
    /// Its tool calls, its prose and, last, its report.
    pub log: Transcript,
    /// Taken off the list under the prompt by the user, once it had ended.
    pub dismissed: bool,
    /// Never announced by the harness, only seen at work: it ends when
    /// the call that spawned it returns.
    implicit: bool,
    pub started: Instant,
    pub duration: Option<Duration>,
}

impl AgentRun {
    /// Who its prose is from.
    pub fn sender(&self) -> &str {
        self.kind.as_deref().unwrap_or("subagent")
    }

    /// The last thing it wrote: its report, once it has ended.
    pub fn report(&self) -> &str {
        self.log
            .blocks
            .iter()
            .rev()
            .find_map(|b| match b {
                Block::Assistant { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("")
    }

    fn end(&mut self, status: SubagentStatus) {
        // Ended already, and now its report has come: it took no longer.
        let elapsed = self.duration.unwrap_or_else(|| self.started.elapsed());
        self.status = Some(status);
        self.duration = Some(elapsed);
        self.log.finish_turn(elapsed);
        // What it started unannounced ends with it. An announced one ends
        // on its own (a Claude subagent's subagent outlives it), or with
        // the session.
        for b in &mut self.log.blocks {
            if let Block::Tool {
                agent: Some(run), ..
            } = b
                && run.implicit
                && run.status.is_none()
            {
                run.end(SubagentStatus::Cancelled);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Block {
    User {
        text: String,
    },
    Assistant {
        text: String,
        sender: String,
        duration: Option<Duration>,
    },
    Thought {
        text: String,
        duration: Option<Duration>,
    },
    /// What the harness the user switched away from wrote for the next one
    /// (see [`super::bridge`]).
    Handoff {
        text: String,
        sender: String,
        to: String,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        output: String,
        is_error: bool,
        done: bool,
        collapsed: bool,
        started: Instant,
        duration: Option<Duration>,
        /// The subagent this call spawned.
        agent: Option<Box<AgentRun>>,
    },
    System(String),
    Notice(String),
    Error(String),
    /// A command the user ran from the prompt with `!`.
    Shell {
        command: String,
        /// Stdout and stderr as they came, the tail when long.
        output: String,
        /// Lines dropped from the front of `output`.
        dropped: usize,
        status: ShellStatus,
        /// It already went in front of a prompt to an agent.
        sent: bool,
        started: Instant,
        duration: Option<Duration>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub blocks: Vec<Block>,
    thought_start: Option<Instant>,
}

/// Default cap on bridged transcript text.
pub const DEFAULT_BRIDGE_MAX_CHARS: usize = 24_000;

/// The tool call `id`, in `blocks` or in the transcript of any subagent
/// spawned there (ids are unique across them).
fn find_in<'a>(blocks: &'a mut [Block], id: &str) -> Option<&'a mut Block> {
    let here = blocks
        .iter()
        .rposition(|b| matches!(b, Block::Tool { id: tid, .. } if tid == id));
    if let Some(i) = here {
        return blocks.get_mut(i);
    }
    for b in blocks.iter_mut().rev() {
        if let Block::Tool {
            agent: Some(run), ..
        } = b
            && let Some(found) = find_in(&mut run.log.blocks, id)
        {
            return Some(found);
        }
    }
    None
}

fn records_of(blocks: &[Block]) -> Vec<BlockRecord> {
    blocks
        .iter()
        .map(|b| match b {
            Block::User { text } => BlockRecord::User { text: text.clone() },
            Block::Assistant {
                text,
                sender,
                duration,
            } => BlockRecord::Assistant {
                text: text.clone(),
                sender: sender.clone(),
                secs: duration.map(|d| d.as_secs_f32()),
            },
            Block::Thought { text, duration } => BlockRecord::Thought {
                text: text.clone(),
                secs: duration.map(|d| d.as_secs_f32()),
            },
            Block::Handoff { text, sender, to } => BlockRecord::Handoff {
                text: text.clone(),
                sender: sender.clone(),
                to: to.clone(),
            },
            Block::Tool {
                id,
                name,
                input,
                output,
                is_error,
                agent,
                ..
            } => BlockRecord::Tool {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                output: output.clone(),
                is_error: *is_error,
                parent: None,
                agent: agent.as_ref().map(|a| AgentRecord {
                    description: a.description.clone(),
                    kind: a.kind.clone(),
                    status: a.status,
                    blocks: records_of(&a.log.blocks),
                    dismissed: a.dismissed,
                    secs: a.duration.map(|d| d.as_secs_f32()),
                }),
            },
            Block::System(t) => BlockRecord::System { text: t.clone() },
            Block::Notice(t) => BlockRecord::Notice { text: t.clone() },
            Block::Error(t) => BlockRecord::Error { text: t.clone() },
            Block::Shell {
                command,
                output,
                dropped,
                status,
                sent,
                duration,
                ..
            } => BlockRecord::Shell {
                command: command.clone(),
                output: output.clone(),
                dropped: *dropped,
                status: status.clone(),
                sent: *sent,
                secs: duration.map(|d| d.as_secs_f32()),
            },
        })
        .collect()
}

impl Transcript {
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::User {
            text: sanitize(&text.into()),
        });
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::System(sanitize(&text.into())));
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::Notice(sanitize(&text.into())));
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::Error(sanitize(&text.into())));
    }

    /// A `!` command starts.
    pub fn shell_started(&mut self, command: &str) {
        self.close_thought();
        self.blocks.push(Block::Shell {
            command: sanitize(command),
            output: String::new(),
            dropped: 0,
            status: ShellStatus::Running,
            sent: false,
            started: Instant::now(),
            duration: None,
        });
    }

    fn running_shell(&mut self) -> Option<&mut Block> {
        self.blocks.iter_mut().rev().find(|b| {
            matches!(
                b,
                Block::Shell {
                    status: ShellStatus::Running,
                    ..
                }
            )
        })
    }

    /// A line of the running `!` command's output.
    pub fn shell_output(&mut self, line: &str) {
        if let Some(Block::Shell {
            output, dropped, ..
        }) = self.running_shell()
        {
            super::shell::push_line(output, dropped, &sanitize(line));
        }
    }

    /// The running `!` command ended. One that could not start has nothing
    /// to tell an agent.
    pub fn shell_ended(&mut self, end: ShellStatus) {
        if let Some(Block::Shell {
            status,
            sent,
            started,
            duration,
            ..
        }) = self.running_shell()
        {
            *sent |= matches!(end, ShellStatus::Failed { .. });
            *status = end;
            *duration = Some(started.elapsed());
        }
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.thought_start = None;
    }

    /// Serializable form for conversation persistence.
    pub fn to_records(&self) -> Vec<BlockRecord> {
        records_of(&self.blocks)
    }

    /// Rebuild from persisted records; everything comes back finished and
    /// collapsed.
    pub fn from_records(records: &[BlockRecord]) -> Self {
        let mut t = Transcript::default();
        for r in records {
            let mut parent = None;
            let block = match r {
                BlockRecord::User { text } => Block::User { text: text.clone() },
                BlockRecord::Assistant { text, sender, secs } => Block::Assistant {
                    text: text.clone(),
                    sender: sender.clone(),
                    duration: Some(Duration::from_secs_f32(secs.unwrap_or(0.0))),
                },
                BlockRecord::Thought { text, secs } => Block::Thought {
                    text: text.clone(),
                    duration: Some(Duration::from_secs_f32(secs.unwrap_or(0.0))),
                },
                BlockRecord::Handoff { text, sender, to } => Block::Handoff {
                    text: text.clone(),
                    sender: sender.clone(),
                    to: to.clone(),
                },
                BlockRecord::Tool {
                    id,
                    name,
                    input,
                    output,
                    is_error,
                    parent: p,
                    agent,
                } => {
                    parent = p.as_deref();
                    Block::Tool {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        output: output.clone(),
                        is_error: *is_error,
                        done: true,
                        collapsed: true,
                        started: Instant::now(),
                        duration: Some(Duration::ZERO),
                        agent: agent.as_ref().map(|a| {
                            Box::new(AgentRun {
                                description: a.description.clone(),
                                kind: a.kind.clone(),
                                // Whatever was still running went with its session.
                                status: Some(a.status.unwrap_or(SubagentStatus::Cancelled)),
                                log: Transcript::from_records(&a.blocks),
                                dismissed: a.dismissed,
                                implicit: false,
                                started: Instant::now(),
                                duration: Some(Duration::from_secs_f32(a.secs.unwrap_or(0.0))),
                            })
                        }),
                    }
                }
                BlockRecord::System { text } => Block::System(text.clone()),
                BlockRecord::Notice { text } => Block::Notice(text.clone()),
                BlockRecord::Error { text } => Block::Error(text.clone()),
                BlockRecord::Shell {
                    command,
                    output,
                    dropped,
                    status,
                    sent,
                    secs,
                } => Block::Shell {
                    command: command.clone(),
                    output: output.clone(),
                    dropped: *dropped,
                    // One still running went with the unharness that ran it.
                    status: match status {
                        ShellStatus::Running => ShellStatus::Killed,
                        s => s.clone(),
                    },
                    sent: *sent,
                    started: Instant::now(),
                    duration: Some(Duration::from_secs_f32(secs.unwrap_or(0.0))),
                },
            };
            // Older files kept a subagent's calls in the main list, each
            // pointing at the call that spawned it.
            match parent.and_then(|p| t.agent_log(p)) {
                Some(log) => log.blocks.push(block),
                None => t.blocks.push(block),
            }
        }
        t.end_running_agents();
        t
    }

    pub fn append_assistant(&mut self, sender: &str, delta: &str) {
        let delta = &sanitize(delta);
        self.close_thought();
        if let Some(Block::Assistant {
            text,
            sender: s,
            duration: None,
        }) = self.blocks.last_mut()
            && s == sender
        {
            text.push_str(delta);
            return;
        }
        self.blocks.push(Block::Assistant {
            text: delta.to_string(),
            sender: sender.to_string(),
            duration: None,
        });
    }

    pub fn append_thought(&mut self, delta: &str) {
        let delta = &sanitize(delta);
        if let Some(Block::Thought {
            text,
            duration: None,
        }) = self.blocks.last_mut()
        {
            text.push_str(delta);
            return;
        }
        self.thought_start = Some(Instant::now());
        self.blocks.push(Block::Thought {
            text: delta.to_string(),
            duration: None,
        });
    }

    pub fn is_thinking(&self) -> bool {
        self.thought_start.is_some()
    }

    pub fn thought_elapsed(&self) -> Option<Duration> {
        self.thought_start.map(|s| s.elapsed())
    }

    fn close_thought(&mut self) {
        if let Some(start) = self.thought_start.take()
            && let Some(Block::Thought { duration, .. }) = self.blocks.last_mut()
        {
            *duration = Some(start.elapsed());
        }
    }

    pub fn tool_started(&mut self, id: &str, name: &str, input: Value) {
        self.close_thought();
        self.blocks.push(Block::Tool {
            id: id.to_string(),
            name: name.to_string(),
            input,
            output: String::new(),
            is_error: false,
            done: false,
            collapsed: true,
            started: Instant::now(),
            duration: None,
            agent: None,
        });
    }

    /// The tool call `id` spawned a subagent, or its subagent is back at work.
    pub fn agent_started(&mut self, id: &str, description: &str, kind: Option<&str>) {
        if self.find_tool(id).is_none() {
            // A subagent we never saw spawned still needs a home.
            self.tool_started(id, "agent", serde_json::json!({"description": description}));
            self.tool_result(id, "", false);
        }
        let Some(Block::Tool { agent, .. }) = self.find_tool(id) else {
            return;
        };
        match agent {
            Some(run) => {
                run.status = None;
                // Back at work, it is back on the list.
                run.dismissed = false;
                run.implicit = false;
                run.started = Instant::now();
                run.duration = None;
            }
            None => {
                *agent = Some(Box::new(AgentRun {
                    description: sanitize(description),
                    kind: kind.map(sanitize),
                    status: None,
                    log: Transcript::default(),
                    dismissed: false,
                    implicit: false,
                    started: Instant::now(),
                    duration: None,
                }))
            }
        }
    }

    /// The subagent that the tool call `id` spawned, wherever it is.
    pub fn agent(&self, id: &str) -> Option<&AgentRun> {
        self.agents()
            .into_iter()
            .find_map(|(i, run, _)| (i == id).then_some(run))
    }

    pub fn agent_mut(&mut self, id: &str) -> Option<&mut AgentRun> {
        match self.find_tool(id) {
            Some(Block::Tool { agent, .. }) => agent.as_deref_mut(),
            _ => None,
        }
    }

    /// Every subagent in spawning order, those spawned by subagents
    /// included: (spawning call, run, how many subagents deep).
    pub fn agents(&self) -> Vec<(&str, &AgentRun, usize)> {
        fn walk<'a>(
            blocks: &'a [Block],
            depth: usize,
            out: &mut Vec<(&'a str, &'a AgentRun, usize)>,
        ) {
            for b in blocks {
                if let Block::Tool {
                    id,
                    agent: Some(run),
                    ..
                } = b
                {
                    out.push((id, run, depth));
                    walk(&run.log.blocks, depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.blocks, 0, &mut out);
        out
    }

    /// The subagents listed by the prompt: every one the user has not
    /// taken off the list, and any that is running.
    pub fn listed_agents(&self) -> Vec<(&str, &AgentRun, usize)> {
        self.agents()
            .into_iter()
            .filter(|(_, run, _)| run.status.is_none() || !run.dismissed)
            .collect()
    }

    /// The transcript of the subagent that the tool call `id` spawned. A
    /// harness that reports a subagent's work without announcing it gets
    /// one made here, which lasts as long as the call does.
    pub fn agent_log(&mut self, id: &str) -> Option<&mut Transcript> {
        let Some(Block::Tool {
            name,
            input,
            done,
            agent,
            ..
        }) = self.find_tool(id)
        else {
            return None;
        };
        let run = agent.get_or_insert_with(|| {
            Box::new(AgentRun {
                description: tool_summary(name, input),
                kind: None,
                status: done.then_some(SubagentStatus::Completed),
                log: Transcript::default(),
                dismissed: false,
                implicit: true,
                started: Instant::now(),
                duration: done.then_some(Duration::ZERO),
            })
        });
        Some(&mut run.log)
    }

    pub fn agent_ended(&mut self, id: &str, status: SubagentStatus, result: Option<&str>) {
        let Some(run) = self.agent_mut(id) else {
            return;
        };
        // The report is usually the last thing it wrote.
        if let Some(result) = result.map(sanitize)
            && !result.trim().is_empty()
            && !run.report().trim_end().ends_with(result.trim())
        {
            let sender = run.sender().to_string();
            // A block of its own, not the tail of an unfinished one.
            run.log.finish_turn(run.started.elapsed());
            run.log.append_assistant(&sender, &result);
        }
        run.end(status);
    }

    /// The session is gone and its subagents with it.
    pub fn end_running_agents(&mut self) {
        for b in &mut self.blocks {
            if let Block::Tool {
                agent: Some(run), ..
            } = b
            {
                if run.status.is_none() {
                    run.end(SubagentStatus::Cancelled);
                }
                run.log.end_running_agents();
            }
        }
    }

    fn find_tool(&mut self, id: &str) -> Option<&mut Block> {
        find_in(&mut self.blocks, id)
    }

    /// Live output (or streaming args before the call is announced; those are
    /// ignored because `tool_started` carries the final input).
    pub fn tool_delta(&mut self, id: &str, delta: &str) {
        let delta = &sanitize(delta);
        if let Some(Block::Tool { output, done, .. }) = self.find_tool(id)
            && !*done
        {
            output.push_str(delta);
        }
    }

    pub fn tool_result(&mut self, id: &str, result: &str, is_error: bool) {
        let result = &sanitize(result);
        match self.find_tool(id) {
            Some(Block::Tool {
                output,
                is_error: err,
                done,
                started,
                duration,
                agent,
                ..
            }) => {
                if !result.is_empty() {
                    *output = result.to_string();
                }
                *err = is_error;
                *done = true;
                *duration = Some(started.elapsed());
                // A subagent nobody announced has no end of its own.
                if let Some(run) = agent
                    && run.implicit
                    && run.status.is_none()
                {
                    run.end(if is_error {
                        SubagentStatus::Failed
                    } else {
                        SubagentStatus::Completed
                    });
                }
            }
            _ => {
                // Result for a call we never saw announced (e.g. codex exec).
                self.blocks.push(Block::Tool {
                    id: id.to_string(),
                    name: "tool".to_string(),
                    input: Value::Null,
                    output: result.to_string(),
                    is_error,
                    done: true,
                    collapsed: true,
                    started: Instant::now(),
                    duration: Some(Duration::ZERO),
                    agent: None,
                });
            }
        }
    }

    /// Called when a turn ends: stamps the duration on the last assistant
    /// block and closes any open thought.
    pub fn finish_turn(&mut self, duration: Duration) {
        self.close_thought();
        if let Some(Block::Assistant {
            duration: d @ None, ..
        }) = self.blocks.last_mut()
        {
            *d = Some(duration);
        }
    }

    pub fn toggle_last_tool(&mut self) -> bool {
        if let Some(Block::Tool { collapsed, .. }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, Block::Tool { .. }))
        {
            *collapsed = !*collapsed;
            return true;
        }
        false
    }

    pub fn toggle_all_tools(&mut self) {
        let any_collapsed = self.blocks.iter().any(|b| {
            matches!(
                b,
                Block::Tool {
                    collapsed: true,
                    ..
                }
            )
        });
        for b in &mut self.blocks {
            if let Block::Tool { collapsed, .. } = b {
                *collapsed = !any_collapsed;
            }
        }
    }

    /// Make what the agent last said from block `start` on the handoff
    /// summary for `to`. Returns whether it said anything.
    pub fn mark_handoff(&mut self, start: usize, to: &str) -> bool {
        let Some(i) = self
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::Assistant { text, .. } if !text.trim().is_empty()))
        else {
            return false;
        };
        if i < start {
            return false;
        }
        if let Block::Assistant { text, sender, .. } = &self.blocks[i] {
            self.blocks[i] = Block::Handoff {
                text: text.trim().to_string(),
                sender: sender.clone(),
                to: to.to_string(),
            };
        }
        true
    }

    /// Conversation text from block `from` onward, for seeding another
    /// harness, at most `max_chars` long (see [`super::bridge`]).
    pub fn bridge_text(&self, from: usize, max_chars: usize) -> Option<String> {
        super::bridge::render(self.blocks.get(from..).unwrap_or_default(), max_chars)
    }
}

/// One-line description of a tool call for headers and bridges (truncated).
pub fn tool_summary(name: &str, input: &Value) -> String {
    truncate_chars(&tool_summary_full(name, input), 60)
}

/// Untruncated single-line description of a tool call.
pub fn tool_summary_full(name: &str, input: &Value) -> String {
    let s = known_summary(name, input).unwrap_or_else(|| generic_summary(input));
    sanitize(&s).replace('\n', " ⏎ ")
}

/// Whether the summary line leaves part of a call's input out, so that the
/// expanded call shows the input: a call [`known_summary`] has no case for,
/// whose input is more than one line of text.
pub fn input_beyond_summary(name: &str, input: &Value) -> bool {
    known_summary(name, input).is_none()
        && match input {
            Value::Null => false,
            Value::String(s) => s.contains('\n'),
            Value::Object(m) => {
                m.len() > 1
                    || m.values()
                        .any(|v| v.as_str().is_none_or(|s| s.contains('\n')))
            }
            _ => true,
        }
}

/// Whether a call waits on the user's answer, so that how long it took is
/// how long they took and not the tool.
pub fn waits_on_user(name: &str) -> bool {
    name == "AskUserQuestion"
}

/// The summary of a tool whose input this knows, or `None` when the name
/// or the input is not one of them.
fn known_summary(name: &str, input: &Value) -> Option<String> {
    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str).filter(|s| !blank(s)))
            .map(first_line)
    };
    match name {
        // `exec_command` is codex-acp's, `execute` the ACP kind.
        "Bash" | "bash" | "shell" | "command_execution" | "exec_command" | "execute" => {
            pick(&["command", "cmd"])
        }
        "Read" | "Write" | "Edit" | "MultiEdit" | "read" | "write" | "edit" => {
            pick(&["file_path", "path", "filename"])
        }
        "Glob" | "Grep" | "glob" | "grep" => pick(&["pattern", "query"]),
        "apply_patch" | "file_change" => input
            .get("changes")
            .and_then(Value::as_array)
            .map(|c| format!("{} file(s)", c.len()))
            .or_else(|| pick(&["path"])),
        "WebFetch" | "WebSearch" | "web_search" => pick(&["url", "query"]),
        "Agent" | "Task" => pick(&["description", "prompt"]),
        // Claude Code's task list: {"subject", "description"} and
        // {"taskId", "status"}, as its session logs record them.
        "TaskCreate" => pick(&["subject", "description"]),
        "TaskUpdate" => {
            let id = input.get("taskId").and_then(Value::as_str)?;
            Some(match input.get("status").and_then(Value::as_str) {
                Some(status) => format!("#{id} {status}"),
                None => format!("#{id}"),
            })
        }
        // Claude Code: {"questions": [{"question", "header", "options",
        // "multiSelect"}]} (fixtures/ask_previews.jsonl). The answer
        // follows as the tool's result.
        "AskUserQuestion" => {
            let questions = input.get("questions").and_then(Value::as_array)?;
            let first = questions
                .first()
                .and_then(|q| q.get("question"))
                .and_then(Value::as_str)
                .map(first_line)?;
            Some(match questions.len() {
                1 => first,
                n => format!("{first} (+{} more)", n - 1),
            })
        }
        // Claude Code: {"skill", "args"?}, as its session logs record it.
        "Skill" => {
            let skill = input.get("skill").and_then(Value::as_str)?;
            Some(
                match input
                    .get("args")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                {
                    Some(args) => format!("{skill} {}", first_line(args)),
                    None => skill.to_string(),
                },
            )
        }
        _ => None,
    }
}

/// Something short for a call nothing above describes, never the input as
/// JSON: a field that usually says what the call does, else the first text
/// field, else the field names. The expanded call shows the whole input.
fn generic_summary(input: &Value) -> String {
    match input {
        Value::Null => String::new(),
        Value::String(s) => first_line(s),
        Value::Object(m) => [
            "command",
            "file_path",
            "path",
            "query",
            "description",
            "prompt",
        ]
        .iter()
        .find_map(|k| m.get(*k).and_then(Value::as_str).filter(|s| !blank(s)))
        .or_else(|| m.values().filter_map(Value::as_str).find(|s| !blank(s)))
        .map(first_line)
        .unwrap_or_else(|| match m.len() {
            0 => String::new(),
            _ => format!("{{{}}}", m.keys().cloned().collect::<Vec<_>>().join(", ")),
        }),
        Value::Array(a) => format!("{} item(s)", a.len()),
        v => v.to_string(),
    }
}

/// The first line with something on it.
fn first_line(s: &str) -> String {
    s.lines().find(|l| !blank(l)).unwrap_or("").to_string()
}

fn blank(s: &str) -> bool {
    s.trim().is_empty()
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_deltas_merge_and_turn_stamps_duration() {
        let mut t = Transcript::default();
        t.push_user("hi");
        t.append_thought("hmm");
        assert!(t.is_thinking());
        t.append_assistant("Claude", "he");
        t.append_assistant("Claude", "llo");
        assert!(!t.is_thinking());
        t.finish_turn(Duration::from_secs(1));
        assert_eq!(t.blocks.len(), 3);
        assert!(matches!(
            &t.blocks[1],
            Block::Thought {
                duration: Some(_),
                ..
            }
        ));
        assert!(
            matches!(&t.blocks[2], Block::Assistant { text, duration: Some(_), .. } if text == "hello")
        );
        // A new delta after the turn ended starts a fresh block.
        t.append_assistant("Claude", "again");
        assert_eq!(t.blocks.len(), 4);
    }

    #[test]
    fn tool_lifecycle() {
        let mut t = Transcript::default();
        t.tool_started("t1", "Bash", json!({"command":"ls -la"}));
        t.tool_delta("t1", "a\n");
        t.tool_delta("t1", "b\n");
        t.tool_result("t1", "a\nb\nc\n", false);
        match &t.blocks[0] {
            Block::Tool {
                output,
                done,
                duration,
                ..
            } => {
                assert_eq!(output, "a\nb\nc\n");
                assert!(*done && duration.is_some());
            }
            other => panic!("{other:?}"),
        }
        // Unknown result creates a block.
        t.tool_result("zz", "out", true);
        assert_eq!(t.blocks.len(), 2);
        assert!(t.toggle_last_tool());
        assert!(matches!(
            &t.blocks[1],
            Block::Tool {
                collapsed: false,
                ..
            }
        ));
    }

    #[test]
    fn bridge_text_caps_and_marks_omissions() {
        let mut t = Transcript::default();
        for i in 0..10 {
            t.push_user(format!("question {i} {}", "x".repeat(100)));
            t.append_assistant("A", &format!("answer {i}"));
            t.finish_turn(Duration::ZERO);
        }
        let full = t.bridge_text(0, 1_000_000).unwrap();
        assert!(full.starts_with("User: question 0"));
        // The first prompt and the last turn stay, the middle goes.
        let capped = t.bridge_text(0, 400).unwrap();
        assert!(capped.chars().count() <= 400);
        assert!(capped.starts_with("User: question 0"));
        assert!(capped.contains("turns omitted]"));
        assert!(capped.contains("answer 9"));
        assert!(!capped.contains("question 1 "));
        assert!(t.bridge_text(t.blocks.len(), 1000).is_none());
        let from_mid = t.bridge_text(18, 1000).unwrap();
        assert!(from_mid.starts_with("User: question 9"));
    }

    #[test]
    fn records_roundtrip() {
        let mut t = Transcript::default();
        t.push_user("q");
        t.append_thought("think");
        t.tool_started("t1", "Bash", json!({"command":"ls"}));
        t.tool_result("t1", "a\nb", false);
        t.append_assistant("Claude", "answer");
        t.finish_turn(Duration::from_millis(1500));
        t.push_notice("n");
        let records = t.to_records();
        assert_eq!(records.len(), 5);
        let back = Transcript::from_records(&records);
        assert_eq!(back.blocks.len(), 5);
        assert!(
            matches!(&back.blocks[2], Block::Tool { output, done: true, .. } if output == "a\nb")
        );
        assert!(
            matches!(&back.blocks[3], Block::Assistant { text, duration: Some(d), .. } if text == "answer" && d.as_millis() == 1500)
        );
        assert_eq!(back.to_records(), records);
        assert!(!back.is_thinking());
    }

    #[test]
    fn a_shell_block_streams_ends_and_comes_back_from_disk() {
        let mut t = Transcript::default();
        t.shell_started("make test");
        t.shell_output("\x1b[32mok\x1b[0m");
        t.shell_output("done");
        // Still running when saved: it comes back stopped.
        let back = Transcript::from_records(&t.to_records());
        assert!(matches!(
            &back.blocks[0],
            Block::Shell { status: ShellStatus::Killed, output, duration: Some(_), .. }
                if output == "ok\ndone\n"
        ));
        assert!(t.bridge_text(0, 1000).is_none());

        t.shell_ended(ShellStatus::Exited { code: 2 });
        // Output after the end belongs to nothing.
        t.shell_output("late");
        t.shell_started("missing");
        t.shell_ended(ShellStatus::Failed {
            error: "no such shell".into(),
        });
        let records = t.to_records();
        assert!(matches!(
            &records[0],
            BlockRecord::Shell { command, output, status: ShellStatus::Exited { code: 2 }, sent: false, secs: Some(_), .. }
                if command == "make test" && output == "ok\ndone\n"
        ));
        // A command that never ran has nothing to send.
        assert!(matches!(&records[1], BlockRecord::Shell { sent: true, .. }));
        let back = Transcript::from_records(&records);
        assert_eq!(back.to_records().len(), 2);
        assert!(matches!(
            &back.to_records()[0],
            BlockRecord::Shell {
                status: ShellStatus::Exited { code: 2 },
                sent: false,
                ..
            }
        ));

        // Bridged to another harness like the rest of the conversation,
        // once an agent was told about it (until then it goes in front of
        // the next prompt instead).
        assert!(t.bridge_text(0, 1000).is_none());
        if let Block::Shell { sent, .. } = &mut t.blocks[0] {
            *sent = true;
        }
        assert_eq!(
            t.bridge_text(0, 1000).unwrap(),
            "User ran a shell command:\n$ make test\nok\ndone\n[exit 2]"
        );
    }

    #[test]
    fn tool_summaries() {
        assert_eq!(
            tool_summary("Bash", &json!({"command":"cargo test\nmore"})),
            "cargo test"
        );
        assert_eq!(
            tool_summary("Edit", &json!({"file_path":"/a/b.rs"})),
            "/a/b.rs"
        );
        assert_eq!(
            tool_summary("apply_patch", &json!({"changes":[1,2]})),
            "2 file(s)"
        );
        assert_eq!(tool_summary("Weird", &Value::Null), "");
        assert!(!input_beyond_summary("Weird", &Value::Null));
        assert!(!input_beyond_summary("Bash", &json!({"command":"ls"})));
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
    }

    #[test]
    fn a_question_is_summarised_by_its_text() {
        // As Claude Code sends it (fixtures/ask_previews.jsonl).
        let question = |text: &str| {
            json!({"question": text, "header": "Falsify", "multiSelect": false,
                   "options": [{"label": "Verify all", "description": "All 9 findings"}]})
        };
        let one = json!({"questions": [question("Run the falsification pass?")]});
        assert_eq!(
            tool_summary_full("AskUserQuestion", &one),
            "Run the falsification pass?"
        );
        assert!(!input_beyond_summary("AskUserQuestion", &one));
        let three =
            json!({"questions": [question("First?"), question("Second?"), question("Third?")]});
        assert_eq!(
            tool_summary_full("AskUserQuestion", &three),
            "First? (+2 more)"
        );
        // The call as it starts streaming, before its input arrived.
        assert_eq!(tool_summary_full("AskUserQuestion", &json!({})), "");
        assert!(waits_on_user("AskUserQuestion") && !waits_on_user("Bash"));
    }

    #[test]
    fn a_skill_is_summarised_by_its_name_and_args() {
        // The shapes of Claude Code's Skill calls in its session logs.
        assert_eq!(
            tool_summary_full("Skill", &json!({"skill": "reviewing-merge-requests"})),
            "reviewing-merge-requests"
        );
        assert_eq!(
            tool_summary_full(
                "Skill",
                &json!({"skill": "code-review", "args": "low --no-comment"})
            ),
            "code-review low --no-comment"
        );
        assert_eq!(
            tool_summary_full("Skill", &json!({"skill": "code-review", "args": " "})),
            "code-review"
        );
        assert!(!input_beyond_summary(
            "Skill",
            &json!({"skill": "x", "args": "y"})
        ));
    }

    #[test]
    fn an_unknown_call_is_never_summarised_as_json() {
        // A field that says what the call does comes first.
        let mcp = json!({"limit": 5, "query": "open issues", "repo": "unharness"});
        assert_eq!(tool_summary_full("mcp__forge__search", &mcp), "open issues");
        assert!(input_beyond_summary("mcp__forge__search", &mcp));
        // Else the first text field (fields are in name order).
        let other = json!({"count": 2, "name": "nightly", "title": "Build"});
        assert_eq!(tool_summary_full("Weird", &other), "nightly");
        // Else the field names.
        let numbers = json!({"zzz": 1, "aaa": [1, 2]});
        assert_eq!(tool_summary_full("Weird", &numbers), "{aaa, zzz}");
        assert!(input_beyond_summary("Weird", &numbers));
        assert_eq!(tool_summary_full("Weird", &json!({})), "");
        assert!(!input_beyond_summary("Weird", &json!({})));
        // One line of text says it all; more lines do not.
        assert!(!input_beyond_summary("Weird", &json!({"note": "hi"})));
        assert!(input_beyond_summary("Weird", &json!({"note": "a\nb"})));
        assert_eq!(tool_summary_full("Weird", &json!([1, 2])), "2 item(s)");
        // A known tool missing its field falls back the same way.
        assert_eq!(
            tool_summary_full("Bash", &json!({"timeout": 5, "workdir": "/w"})),
            "/w"
        );
        assert!(input_beyond_summary("Bash", &json!({"timeout": 5})));
        // A blank field or a blank first line is passed over.
        assert_eq!(
            tool_summary_full("Weird", &json!({"command": " ", "path": "/x"})),
            "/x"
        );
        assert_eq!(
            tool_summary_full("Weird", &json!({"note": "\n real"})),
            " real"
        );
    }

    #[test]
    fn calls_that_have_a_case_of_their_own() {
        // codex-acp's command tool and the ACP kind.
        let cmd = json!({"command": "cat a.txt", "cwd": "/w"});
        for name in ["exec_command", "execute"] {
            assert_eq!(tool_summary_full(name, &cmd), "cat a.txt");
            assert!(!input_beyond_summary(name, &cmd));
        }
        // Claude Code's task list, in the shapes of its session logs.
        assert_eq!(
            tool_summary_full(
                "TaskCreate",
                &json!({"subject": "Fix tables", "description": "Long"})
            ),
            "Fix tables"
        );
        assert_eq!(
            tool_summary_full("TaskUpdate", &json!({"taskId": "1", "status": "completed"})),
            "#1 completed"
        );
    }
}
