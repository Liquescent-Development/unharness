//! Claude Code `--output-format stream-json` parser.
//!
//! Pure: one stdout line in, zero or more `AgentEvent`s out. Fixture-tested
//! against recordings in `fixtures/` made with `scripts/record-claude.py`.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::core::{
    AgentEvent, ContextUsage, PermissionKind, PermissionRequest, PlanEntry, PlanStatus, Question,
    QuestionOption, RateLimitInfo, RateLimitWindow, StopReason, SubagentStatus, ToolAction, Usage,
};

/// What one of Claude Code's tools does, for allow rules. Also used for
/// agents that pass Claude's tool calls on under their own names (ACP).
pub fn tool_action(tool: &str, input: &Value) -> ToolAction {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let edit = |key: &str| match text(key) {
        Some(path) => ToolAction::Edit {
            paths: vec![path.into()],
        },
        None => ToolAction::Opaque,
    };
    match tool {
        // A command that asks to leave Claude's own sandbox is not the
        // command a rule was written for.
        "Bash" if input.get("dangerouslyDisableSandbox") == Some(&Value::Bool(true)) => {
            ToolAction::Opaque
        }
        "Bash" => match text("command") {
            Some(command) => ToolAction::Shell {
                command: command.to_string(),
                cwd: None,
            },
            None => ToolAction::Opaque,
        },
        "Write" | "Edit" | "MultiEdit" => edit("file_path"),
        "NotebookEdit" => edit("notebook_path"),
        "Read" => match text("file_path") {
            Some(path) => ToolAction::Read { path: path.into() },
            None => ToolAction::Opaque,
        },
        _ => match tool.strip_prefix("mcp__").map(|t| (t, t.split_once("__"))) {
            // `mcp__a__b__c`: server `a` or server `a__b`, there is no telling.
            Some((_, Some((_, tool)))) if tool.contains("__") => ToolAction::Opaque,
            Some((_, None)) => ToolAction::Opaque,
            Some((_, Some((server, tool)))) => ToolAction::Mcp {
                server: server.to_string(),
                tool: tool.to_string(),
            },
            None => ToolAction::Other,
        },
    }
}

#[derive(Debug, Default)]
enum BlockAcc {
    #[default]
    Text,
    Thinking,
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
}

#[derive(Debug, Default)]
pub struct ClaudeParser {
    session_id: Option<String>,
    /// MCP servers already reported as failed.
    failed_mcp_servers: HashSet<String>,
    /// Open content blocks of the message currently streaming, by index.
    blocks: HashMap<u64, BlockAcc>,
    /// Tool-use ids already announced via `ToolCallStarted`.
    seen_tool_ids: HashSet<String>,
    /// Message ids for which we saw streamed deltas (so `assistant` is a dup).
    streamed_messages: HashSet<String>,
    turn_started: bool,
    /// `total_cost_usd` of the previous result; the field is a running total
    /// for the process, so a turn's cost is the difference.
    last_total_cost: f64,
    /// Tasks created with `TaskCreate`, in creation order: (id, entry).
    tasks: Vec<(String, PlanEntry)>,
    /// Subagent task id → the tool call that first spawned it. A subagent
    /// sent back to work (`SendMessage`) starts again under the same task
    /// id, and its messages keep naming the original call.
    agent_tasks: HashMap<String, String>,
    /// Subagent tasks that started and have not reported an end.
    running_agents: HashSet<String>,
    /// Subagent tasks that ended without their report, which the
    /// `task_notification` normally brings right after.
    unreported_agents: HashSet<String>,
}

impl ClaudeParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The task id of the running subagent that the tool call `id` spawned.
    pub fn task_of(&self, id: &str) -> Option<&str> {
        self.agent_tasks
            .iter()
            .find(|(task, spawn)| *spawn == id && self.running_agents.contains(*task))
            .map(|(task, _)| task.as_str())
    }

    pub fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            // Claude never prints non-JSON on stdout in stream-json mode; surface it.
            return vec![AgentEvent::Notice(line.to_string())];
        };
        self.feed_value(&val)
    }

    pub fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        let t = line.trim();
        if t.is_empty() {
            return vec![];
        }
        vec![AgentEvent::Error(t.to_string())]
    }

    pub fn feed_value(&mut self, val: &Value) -> Vec<AgentEvent> {
        // Messages produced inside a subagent name the tool call that spawned it.
        if let Some(parent) = val.get("parent_tool_use_id").and_then(Value::as_str) {
            return self.feed_sub(parent, val);
        }
        let mut out = Vec::new();
        match str_at(val, "type") {
            "system" => self.on_system(val, &mut out),
            "stream_event" => self.on_stream_event(val, &mut out),
            "assistant" => self.on_assistant(val, &mut out),
            "user" => self.on_user(val, &mut out),
            "result" => self.on_result(val, &mut out),
            "control_request" => self.on_control_request(val, &mut out),
            "control_cancel_request" => {
                out.push(AgentEvent::Notice(
                    "permission request cancelled by harness".to_string(),
                ));
            }
            // Handled by the driver (pending request bookkeeping).
            "control_response" => {
                if let Some(err) = val.pointer("/response/error").and_then(Value::as_str) {
                    out.push(AgentEvent::Error(format!("control error: {err}")));
                }
                // The answer to `rewind_conversation` is a "success" either
                // way; `rewound` says whether it happened.
                let answer = val.pointer("/response/response");
                if answer
                    .and_then(|a| a.get("rewound"))
                    .and_then(Value::as_bool)
                    == Some(false)
                {
                    out.push(AgentEvent::RewindFailed {
                        reason: answer
                            .and_then(|a| a.get("error"))
                            .and_then(Value::as_str)
                            .unwrap_or("the session did not rewind")
                            .to_string(),
                    });
                }
            }
            "rate_limit_event" => {
                let info = val.get("rate_limit_info").unwrap_or(&Value::Null);
                let status = info
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("allowed");
                if status != "allowed" {
                    out.push(AgentEvent::Notice(format!("rate limit: {status}")));
                }
                out.push(AgentEvent::RateLimit(RateLimitInfo {
                    status: Some(status.to_string()),
                    windows: rate_limit_windows(info),
                }));
            }
            _ => {}
        }
        out
    }

    /// A subagent's message: same shapes as the main agent's, attributed to
    /// `parent`, and never part of the main turn's bookkeeping.
    fn feed_sub(&mut self, parent: &str, val: &Value) -> Vec<AgentEvent> {
        let mut inner = Vec::new();
        let turn_started = std::mem::replace(&mut self.turn_started, true);
        match str_at(val, "type") {
            "assistant" => self.on_assistant(val, &mut inner),
            "user" => self.on_user(val, &mut inner),
            // Partial subagent messages would clobber the main stream's
            // block state; the complete message follows anyway.
            _ => {}
        }
        self.turn_started = turn_started;
        inner
            .into_iter()
            .map(|event| AgentEvent::Sub {
                parent: parent.to_string(),
                event: Box::new(event),
            })
            .collect()
    }

    fn on_system(&mut self, val: &Value, out: &mut Vec<AgentEvent>) {
        match str_at(val, "subtype") {
            "init" => {
                let sid = str_at(val, "session_id").to_string();
                if !sid.is_empty() && self.session_id.as_deref() != Some(&sid) {
                    self.session_id = Some(sid.clone());
                    out.push(AgentEvent::SessionStarted {
                        session_id: sid,
                        model: opt_str(val, "model"),
                    });
                }
                // Every turn starts with an `init`; say it once per server.
                for server in val
                    .get("mcp_servers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let name = str_at(server, "name");
                    if str_at(server, "status") == "failed"
                        && self.failed_mcp_servers.insert(name.to_string())
                    {
                        out.push(AgentEvent::Notice(format!(
                            "MCP server '{name}' failed to start"
                        )));
                    }
                }
            }
            "api_retry" => {
                out.push(AgentEvent::Notice(format!(
                    "API retry {}/{} ({})",
                    val.get("attempt").and_then(Value::as_u64).unwrap_or(0),
                    val.get("max_retries").and_then(Value::as_u64).unwrap_or(0),
                    str_at(val, "error")
                )));
            }
            "permission_denied" => {
                let tool = opt_str(val, "tool_name")
                    .or_else(|| opt_str(val, "tool"))
                    .unwrap_or_else(|| "tool".to_string());
                out.push(AgentEvent::Notice(format!(
                    "permission denied for {tool} (no prompt available in this mode)"
                )));
            }
            "compact_boundary" => out.push(AgentEvent::Notice("context compacted".into())),
            // Shell commands are tasks too (`local_bash`); only agents are subagents.
            "task_started" if str_at(val, "task_type") == "local_agent" => {
                let task = str_at(val, "task_id").to_string();
                let id = self
                    .agent_tasks
                    .entry(task.clone())
                    .or_insert_with(|| str_at(val, "tool_use_id").to_string())
                    .clone();
                self.unreported_agents.remove(&task);
                self.running_agents.insert(task);
                out.push(AgentEvent::SubagentStarted {
                    id,
                    description: str_at(val, "description").to_string(),
                    kind: opt_str(val, "subagent_type"),
                });
            }
            "task_progress" => {
                let activity = str_at(val, "description");
                if let Some(id) = self.agent_tasks.get(str_at(val, "task_id"))
                    && !activity.is_empty()
                {
                    out.push(AgentEvent::SubagentProgress {
                        id: id.clone(),
                        activity: activity.to_string(),
                    });
                }
            }
            // The end of a task: `task_updated` says so first, without the
            // report, and `task_notification` follows with it. Claude sends
            // the notification once per task id until the task starts
            // again, the update on every change of status, so either one
            // ends it; a notification after the update only hands over
            // the report.
            "task_updated" => {
                let task = str_at(val, "task_id");
                let status = match val.pointer("/patch/status").and_then(Value::as_str) {
                    Some("completed") => SubagentStatus::Completed,
                    Some("failed") => SubagentStatus::Failed,
                    Some("killed" | "stopped") => SubagentStatus::Cancelled,
                    // Not an end, or not one seen yet.
                    _ => return,
                };
                if self.running_agents.remove(task)
                    && let Some(id) = self.agent_tasks.get(task)
                {
                    self.unreported_agents.insert(task.to_string());
                    out.push(AgentEvent::SubagentEnded {
                        id: id.clone(),
                        status,
                        result: None,
                    });
                }
            }
            "task_notification" => {
                let task = str_at(val, "task_id");
                let running = self.running_agents.remove(task);
                let unreported = self.unreported_agents.remove(task);
                let status = match str_at(val, "status") {
                    "completed" => SubagentStatus::Completed,
                    "stopped" | "killed" => SubagentStatus::Cancelled,
                    _ => SubagentStatus::Failed,
                };
                // A stopped task's summary is only its description.
                let result = opt_str(val, "summary")
                    .filter(|s| status != SubagentStatus::Cancelled && !s.is_empty());
                if (running || (unreported && result.is_some()))
                    && let Some(id) = self.agent_tasks.get(task)
                {
                    out.push(AgentEvent::SubagentEnded {
                        id: id.clone(),
                        status,
                        result,
                    });
                }
            }
            // status, thinking_tokens, hook_*, plugin_install: not shown.
            _ => {}
        }
    }

    fn on_stream_event(&mut self, val: &Value, out: &mut Vec<AgentEvent>) {
        let Some(event) = val.get("event") else {
            return;
        };
        match str_at(event, "type") {
            "message_start" => {
                if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
                    self.streamed_messages.insert(id.to_string());
                }
                self.blocks.clear();
                if !self.turn_started {
                    self.turn_started = true;
                    out.push(AgentEvent::TurnStarted);
                }
                // The prompt size of the latest request is how full the
                // context is (subagent messages never reach this handler).
                if let Some(usage) = event.pointer("/message/usage") {
                    let used = u64_at(usage, "input_tokens")
                        + u64_at(usage, "cache_read_input_tokens")
                        + u64_at(usage, "cache_creation_input_tokens");
                    if used > 0 {
                        out.push(AgentEvent::Context(ContextUsage {
                            used: Some(used),
                            window: None,
                        }));
                    }
                }
            }
            "content_block_start" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(block) = event.get("content_block") else {
                    return;
                };
                let acc = match str_at(block, "type") {
                    "thinking" => BlockAcc::Thinking,
                    "tool_use" => BlockAcc::ToolUse {
                        id: str_at(block, "id").to_string(),
                        name: str_at(block, "name").to_string(),
                        json: String::new(),
                    },
                    _ => BlockAcc::Text,
                };
                self.blocks.insert(index, acc);
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(delta) = event.get("delta") else {
                    return;
                };
                match str_at(delta, "type") {
                    "text_delta" => {
                        let t = str_at(delta, "text");
                        if !t.is_empty() {
                            out.push(AgentEvent::TextDelta(t.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        let t = str_at(delta, "thinking");
                        if !t.is_empty() {
                            out.push(AgentEvent::ThinkingDelta(t.to_string()));
                        }
                    }
                    "input_json_delta" => {
                        let partial = str_at(delta, "partial_json");
                        if let Some(BlockAcc::ToolUse { id, name, json }) =
                            self.blocks.get_mut(&index)
                            && !partial.is_empty()
                        {
                            json.push_str(partial);
                            out.push(AgentEvent::ToolCallDelta {
                                id: id.clone(),
                                name: name.clone(),
                                delta: partial.to_string(),
                            });
                        }
                    }
                    _ => {} // signature_delta
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(BlockAcc::ToolUse { id, name, json }) = self.blocks.remove(&index)
                    && self.seen_tool_ids.insert(id.clone())
                {
                    let input =
                        serde_json::from_str(&json).unwrap_or(Value::Object(Default::default()));
                    let plan = todo_write_plan(&name, &input);
                    out.push(AgentEvent::ToolCallStarted { id, name, input });
                    out.extend(plan);
                }
            }
            _ => {} // message_delta, message_stop
        }
    }

    fn on_assistant(&mut self, val: &Value, out: &mut Vec<AgentEvent>) {
        let Some(message) = val.get("message") else {
            return;
        };
        let streamed = message
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| self.streamed_messages.contains(id));
        if !self.turn_started {
            self.turn_started = true;
            out.push(AgentEvent::TurnStarted);
        }
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        for block in content {
            match str_at(block, "type") {
                "tool_use" => {
                    let id = str_at(block, "id").to_string();
                    if self.seen_tool_ids.insert(id.clone()) {
                        let name = str_at(block, "name").to_string();
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        let plan = todo_write_plan(&name, &input);
                        out.push(AgentEvent::ToolCallStarted { id, name, input });
                        out.extend(plan);
                    }
                }
                "text" if !streamed => {
                    let t = str_at(block, "text");
                    if !t.is_empty() {
                        out.push(AgentEvent::TextDelta(t.to_string()));
                    }
                }
                "thinking" if !streamed => {
                    let t = str_at(block, "thinking");
                    if !t.is_empty() {
                        out.push(AgentEvent::ThinkingDelta(t.to_string()));
                    }
                }
                _ => {}
            }
        }
    }

    fn on_user(&mut self, val: &Value, out: &mut Vec<AgentEvent>) {
        let Some(content) = val.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for block in content {
            if str_at(block, "type") != "tool_result" {
                continue;
            }
            out.push(AgentEvent::ToolCallResult {
                id: str_at(block, "tool_use_id").to_string(),
                output: flatten_content(block.get("content")),
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
        if let Some(result) = val.get("tool_use_result")
            && self.apply_task_result(result)
        {
            out.push(AgentEvent::PlanUpdated {
                entries: self.tasks.iter().map(|(_, e)| e.clone()).collect(),
                explanation: None,
            });
        }
    }

    /// Track `TaskCreate` / `TaskUpdate` results; true when the task list changed.
    fn apply_task_result(&mut self, result: &Value) -> bool {
        if let Some(task) = result.get("task")
            && let Some(id) = task.get("id").and_then(Value::as_str)
        {
            self.tasks.push((
                id.to_string(),
                PlanEntry {
                    text: str_at(task, "subject").to_string(),
                    status: PlanStatus::Pending,
                },
            ));
            return true;
        }
        let (Some(id), Some(to)) = (
            result.get("taskId").and_then(Value::as_str),
            result.pointer("/statusChange/to").and_then(Value::as_str),
        ) else {
            return false;
        };
        if to == "deleted" {
            let before = self.tasks.len();
            self.tasks.retain(|(tid, _)| tid != id);
            return self.tasks.len() != before;
        }
        match self.tasks.iter_mut().find(|(tid, _)| tid == id) {
            Some((_, entry)) => {
                entry.status = PlanStatus::parse(to);
                true
            }
            None => false,
        }
    }

    fn on_result(&mut self, val: &Value, out: &mut Vec<AgentEvent>) {
        let cost_usd = val.get("total_cost_usd").and_then(Value::as_f64).map(|t| {
            let turn = (t - self.last_total_cost).max(0.0);
            self.last_total_cost = t;
            turn
        });
        if let Some(usage) = val.get("usage") {
            let usage = Usage {
                input: u64_at(usage, "input_tokens"),
                output: u64_at(usage, "output_tokens"),
                cache_read: u64_at(usage, "cache_read_input_tokens"),
                cache_write: u64_at(usage, "cache_creation_input_tokens"),
                cost_usd,
                cumulative: false,
            };
            // A subagent's report that arrives during a turn is taken into
            // that turn; the turn it would have started still gets a result,
            // an empty one that used nothing.
            if usage.total_tokens() > 0 || cost_usd.is_some_and(|c| c > 0.0) {
                out.push(AgentEvent::Usage(usage));
            }
        }
        let window = val
            .get("modelUsage")
            .and_then(Value::as_object)
            .and_then(|m| m.values().map(|u| u64_at(u, "contextWindow")).max())
            .filter(|w| *w > 0);
        if window.is_some() {
            out.push(AgentEvent::Context(ContextUsage { used: None, window }));
        }
        if let Some(denials) = val.get("permission_denials").and_then(Value::as_array)
            && !denials.is_empty()
        {
            let names: Vec<&str> = denials
                .iter()
                .filter_map(|d| d.get("tool_name").and_then(Value::as_str))
                .collect();
            out.push(AgentEvent::Notice(format!(
                "{} permission denial(s): {}",
                denials.len(),
                names.join(", ")
            )));
        }
        let subtype = str_at(val, "subtype");
        let is_error = val
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let stop_reason = if subtype == "success" && !is_error {
            StopReason::Done
        } else {
            let detail = val
                .get("result")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| format!("{subtype}: {s}"))
                .unwrap_or_else(|| subtype.to_string());
            StopReason::Error(detail)
        };
        out.push(AgentEvent::TurnCompleted { stop_reason });
        self.turn_started = false;
        self.blocks.clear();
    }

    fn on_control_request(&mut self, val: &Value, out: &mut Vec<AgentEvent>) {
        let request_id = str_at(val, "request_id").to_string();
        let Some(req) = val.get("request") else {
            return;
        };
        match str_at(req, "subtype") {
            "can_use_tool" => {
                let tool = str_at(req, "tool_name").to_string();
                let input = req.get("input").cloned().unwrap_or(Value::Null);
                let tool_call_id = opt_str(req, "tool_use_id");
                let kind = if tool == "AskUserQuestion" {
                    PermissionKind::Question {
                        questions: parse_questions(&input),
                    }
                } else {
                    PermissionKind::ToolUse {
                        action: tool_action(&tool, &input),
                        tool,
                        input,
                        description: opt_str(req, "description").or_else(|| {
                            opt_str(req, "blocked_path").map(|p| format!("blocked path: {p}"))
                        }),
                    }
                };
                out.push(AgentEvent::PermissionRequest(PermissionRequest {
                    id: request_id,
                    kind,
                    tool_call_id,
                }));
            }
            other => out.push(AgentEvent::Notice(format!(
                "unhandled control request: {other}"
            ))),
        }
    }
}

/// AskUserQuestion input → our `Question`s. The question text doubles as the
/// id because the answer map is keyed by it.
pub fn parse_questions(input: &Value) -> Vec<Question> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .map(|q| {
                    let text = str_at(q, "question").to_string();
                    Question {
                        id: text.clone(),
                        header: str_at(q, "header").to_string(),
                        text,
                        options: q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|os| {
                                os.iter()
                                    .map(|o| QuestionOption {
                                        label: str_at(o, "label").to_string(),
                                        description: str_at(o, "description").to_string(),
                                        preview: opt_str(o, "preview")
                                            .filter(|p| !p.trim().is_empty()),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        allow_other: true,
                        multi: q
                            .get("multiSelect")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Tool result content is either a string or `[{type:"text",text}, ...]`.
pub fn flatten_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => i
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some(format!("[{}]", str_at(i, "type")))),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// `TodoWrite` carries the whole list in its input.
fn todo_write_plan(name: &str, input: &Value) -> Option<AgentEvent> {
    if name != "TodoWrite" {
        return None;
    }
    let entries = input
        .get("todos")?
        .as_array()?
        .iter()
        .map(|t| PlanEntry {
            text: str_at(t, "content").to_string(),
            status: PlanStatus::parse(str_at(t, "status")),
        })
        .collect();
    Some(AgentEvent::PlanUpdated {
        entries,
        explanation: None,
    })
}

/// `unifiedWindows` lists every window; older builds only report the active one.
fn rate_limit_windows(info: &Value) -> Vec<RateLimitWindow> {
    let window = |label: &str, w: &Value| RateLimitWindow {
        label: label.to_string(),
        used_percent: w
            .get("utilization")
            .and_then(Value::as_f64)
            .map(|u| (u * 100.0) as f32),
        resets_at: w.get("resetsAt").and_then(Value::as_i64),
    };
    match info.get("unifiedWindows").and_then(Value::as_object) {
        Some(windows) => windows.iter().map(|(k, w)| window(k, w)).collect(),
        None => info
            .get("rateLimitType")
            .and_then(Value::as_str)
            .map(|label| vec![window(label, info)])
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testing::{LineParser, assert_fixture, fixtures_dir};

    impl LineParser for ClaudeParser {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            ClaudeParser::feed(self, line)
        }
        fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
            ClaudeParser::feed_stderr(self, line)
        }
    }

    fn fixture(case: &str) {
        assert_fixture(&mut ClaudeParser::new(), &fixtures_dir(file!()), case);
    }

    #[test]
    fn fixture_write_denied_auto_mode() {
        fixture("write_denied");
    }

    #[test]
    fn fixture_permission_and_question() {
        fixture("permission_and_question");
    }

    #[test]
    fn fixture_ask_previews() {
        fixture("ask_previews");
    }

    #[test]
    fn fixture_mcp_server() {
        fixture("mcp_server");
    }

    #[test]
    fn fixture_rewind() {
        fixture("rewind");
    }

    #[test]
    fn fixture_subagent() {
        fixture("subagent");
    }

    #[test]
    fn fixture_subagent_parallel() {
        fixture("subagent_parallel");
    }

    #[test]
    fn fixture_subagent_blocking() {
        fixture("subagent_blocking");
    }

    #[test]
    fn fixture_subagent_stopped() {
        fixture("subagent_stopped");
    }

    #[test]
    fn fixture_subagent_resumed() {
        fixture("subagent_resumed");
    }

    #[test]
    fn fixture_subagent_next_prompt() {
        fixture("subagent_next_prompt");
    }

    #[test]
    fn fixture_subagent_stop_task() {
        fixture("subagent_stop_task");
    }

    #[test]
    fn fixture_subagent_interrupted() {
        fixture("subagent_interrupted");
    }

    #[test]
    fn fixture_subagent_nested() {
        fixture("subagent_nested");
    }

    #[test]
    fn a_subagent_ends_without_its_notification() {
        // Claude sends one `task_notification` per task id; `task_updated`
        // still says when a task ends.
        let mut p = ClaudeParser::new();
        p.feed(
            r#"{"type":"system","subtype":"task_started","task_id":"a1","tool_use_id":"t1","description":"Review","subagent_type":"reviewer","task_type":"local_agent"}"#,
        );
        let evs = p.feed(
            r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"status":"completed","end_time":1}}"#,
        );
        assert_eq!(
            evs,
            vec![AgentEvent::SubagentEnded {
                id: "t1".into(),
                status: SubagentStatus::Completed,
                result: None,
            }]
        );
        assert_eq!(p.task_of("t1"), None);
        // A notification that comes after all hands over the report.
        let evs = p.feed(
            r#"{"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"t1","status":"completed","summary":"Looks good."}"#,
        );
        assert_eq!(
            evs,
            vec![AgentEvent::SubagentEnded {
                id: "t1".into(),
                status: SubagentStatus::Completed,
                result: Some("Looks good.".into()),
            }]
        );
        // And only once.
        assert!(p.feed(
            r#"{"type":"system","subtype":"task_notification","task_id":"a1","tool_use_id":"t1","status":"completed","summary":"Looks good."}"#,
        )
        .is_empty());
    }

    #[test]
    fn a_task_update_that_is_not_an_end_ends_nothing() {
        let mut p = ClaudeParser::new();
        p.feed(
            r#"{"type":"system","subtype":"task_started","task_id":"a1","tool_use_id":"t1","description":"Review","task_type":"local_agent"}"#,
        );
        assert!(p.feed(
            r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"is_backgrounded":true}}"#,
        )
        .is_empty());
        assert!(p.feed(
            r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"status":"running"}}"#,
        )
        .is_empty());
        assert_eq!(p.task_of("t1"), Some("a1"));
        // Nor does a shell task's end.
        p.feed(
            r#"{"type":"system","subtype":"task_started","task_id":"b1","tool_use_id":"t2","description":"sleep","task_type":"local_bash"}"#,
        );
        assert!(p.feed(
            r#"{"type":"system","subtype":"task_updated","task_id":"b1","patch":{"status":"completed"}}"#,
        )
        .is_empty());
    }

    #[test]
    fn fixture_steer_and_compact() {
        fixture("steer_and_compact");
    }

    #[test]
    fn fixture_image_turn() {
        fixture("image_turn");
    }

    #[test]
    fn fixture_pdf_turn() {
        fixture("pdf_turn");
    }

    #[test]
    fn fixture_text_file_turn() {
        fixture("text_file_turn");
    }

    #[test]
    fn fixture_task_list() {
        fixture("task_list");
    }

    #[test]
    fn a_rewind_that_did_not_happen_is_reported() {
        // Seen live when the target uuid belongs to the session a fork came from.
        let mut p = ClaudeParser::new();
        let evs = p.feed(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r","response":{"rewound":false,"prefillText":null,"precedingAssistantUuid":null,"error":"stale target","reason":"stale_target"}}}"#,
        );
        assert_eq!(
            evs,
            vec![AgentEvent::RewindFailed {
                reason: "stale target".into()
            }]
        );
        assert!(p.feed(r#"{"type":"control_response","response":{"subtype":"success","request_id":"r","response":{"rewound":true}}}"#).is_empty());
    }

    #[test]
    fn todo_write_replaces_the_plan() {
        let mut p = ClaudeParser::new();
        let evs = p.feed(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"TodoWrite","input":{"todos":[{"content":"a","status":"completed","activeForm":"A"},{"content":"b","status":"in_progress","activeForm":"B"}]}}]}}"#,
        );
        assert_eq!(evs.last().unwrap().summary(), "PlanUpdated [x] a; [~] b");
    }

    #[test]
    fn fixture_basic_turn_bash() {
        fixture("basic_turn");
    }

    #[test]
    fn streamed_text_is_not_duplicated_by_assistant_message() {
        let mut p = ClaudeParser::new();
        let mut ev = Vec::new();
        ev.extend(p.feed(
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}}}"#,
        ));
        ev.extend(p.feed(r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#));
        ev.extend(p.feed(r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#));
        ev.extend(p.feed(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}]}}"#,
        ));
        ev.extend(p.feed(r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":2},"total_cost_usd":0.5}"#));
        assert_eq!(
            ev,
            vec![
                AgentEvent::TurnStarted,
                AgentEvent::TextDelta("hi".into()),
                AgentEvent::Usage(Usage {
                    input: 1,
                    output: 2,
                    cost_usd: Some(0.5),
                    ..Default::default()
                }),
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Done
                },
            ]
        );
    }

    #[test]
    fn assistant_without_stream_emits_text_and_tool() {
        let mut p = ClaudeParser::new();
        let ev = p.feed(r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"x"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#);
        assert_eq!(ev.len(), 3);
        assert!(
            matches!(&ev[2], AgentEvent::ToolCallStarted { id, name, .. } if id == "t1" && name == "Bash")
        );
        // A later stream stop for the same tool id does not re-announce it.
        p.feed(r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Bash"}}}"#);
        let ev2 =
            p.feed(r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#);
        assert!(ev2.is_empty());
    }

    #[test]
    fn tools_are_named_for_what_they_do() {
        use serde_json::json;
        let shell = |command: &str| ToolAction::Shell {
            command: command.into(),
            cwd: None,
        };
        let edit = |path: &str| ToolAction::Edit {
            paths: vec![path.into()],
        };
        assert_eq!(
            tool_action(
                "Bash",
                &json!({"command": "cargo test", "description": "x"})
            ),
            shell("cargo test")
        );
        assert_eq!(
            tool_action(
                "Bash",
                &json!({"command": "cargo test", "dangerouslyDisableSandbox": true})
            ),
            ToolAction::Opaque
        );
        assert_eq!(tool_action("Bash", &json!({})), ToolAction::Opaque);
        for tool in ["Write", "Edit", "MultiEdit"] {
            assert_eq!(
                tool_action(tool, &json!({"file_path": "/w/a"})),
                edit("/w/a")
            );
        }
        assert_eq!(
            tool_action("NotebookEdit", &json!({"notebook_path": "/w/a.ipynb"})),
            edit("/w/a.ipynb")
        );
        assert_eq!(tool_action("Write", &json!({})), ToolAction::Opaque);
        assert_eq!(
            tool_action("Read", &json!({"file_path": "/etc/hosts"})),
            ToolAction::Read {
                path: "/etc/hosts".into()
            }
        );
        assert_eq!(
            tool_action("mcp__probe__magic_word", &json!({})),
            ToolAction::Mcp {
                server: "probe".into(),
                tool: "magic_word".into()
            }
        );
        // Which part is the server's name cannot be told.
        for tool in ["mcp__a__b__c", "mcp__probe", "mcp__"] {
            assert_eq!(tool_action(tool, &json!({})), ToolAction::Opaque, "{tool}");
        }
        assert_eq!(
            tool_action("WebFetch", &json!({"url": "https://example.com"})),
            ToolAction::Other
        );
    }

    #[test]
    fn control_request_maps_to_permission_and_question() {
        let mut p = ClaudeParser::new();
        let ev = p.feed(r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/x"},"permission_suggestions":[{"type":"addRules"}],"tool_use_id":"t9"}}"#);
        match &ev[0] {
            AgentEvent::PermissionRequest(req) => {
                assert_eq!(req.id, "r1");
                assert_eq!(req.tool_call_id.as_deref(), Some("t9"));
                assert!(
                    matches!(&req.kind, PermissionKind::ToolUse { tool, .. } if tool == "Write")
                );
            }
            other => panic!("{other:?}"),
        }
        let ev = p.feed(r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Color?","header":"Color","options":[{"label":"Red","description":"r"},{"label":"Blue","description":"b"}],"multiSelect":false}]}}}"#);
        match &ev[0] {
            AgentEvent::PermissionRequest(req) => match &req.kind {
                PermissionKind::Question { questions } => {
                    assert_eq!(questions.len(), 1);
                    assert_eq!(questions[0].id, "Color?");
                    assert_eq!(questions[0].options[1].label, "Blue");
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn flatten_tool_result_variants() {
        assert_eq!(flatten_content(Some(&serde_json::json!("plain"))), "plain");
        assert_eq!(
            flatten_content(Some(
                &serde_json::json!([{"type":"text","text":"a"},{"type":"image"}])
            )),
            "a\n[image]"
        );
        assert_eq!(flatten_content(None), "");
    }
}
