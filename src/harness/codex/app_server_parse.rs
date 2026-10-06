//! Parser for `codex app-server` notifications and server requests.
//!
//! Responses to our own requests are handled by the transport driver (it
//! owns the request ids); this parser ignores them.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::core::jsonrpc::RpcMessage;
use crate::core::rules::unwrap_shell;
use crate::core::{
    AgentEvent, ContextUsage, PermissionKind, PermissionRequest, PlanEntry, PlanStatus, Question,
    QuestionOption, RateLimitInfo, RateLimitWindow, StopReason, SubagentStatus, ToolAction, Usage,
};

/// Title prefix marking an MCP elicitation prompt (answered with `{action, content}`).
pub const ELICITATION_PREFIX: &str = "MCP ";

#[derive(Debug, Default)]
pub struct CodexAppServerParser {
    turn_started: bool,
    /// agentMessage / reasoning item ids that streamed deltas.
    streamed_items: HashSet<String>,
    /// Rate-limit updates are sparse; a null window keeps its last value.
    rate_primary: Option<RateLimitWindow>,
    rate_secondary: Option<RateLimitWindow>,
    /// Sub-agent thread id → the item that spawned it. Sub-agent threads
    /// report on the same stream as the main thread.
    children: HashMap<String, String>,
    /// Sub-agent thread id → its name (the last part of its agent path).
    child_names: HashMap<String, String>,
    /// Sub-agent threads with a turn in progress.
    running_children: HashSet<String>,
    /// The MCP tool call in progress on each thread: thread id → (item id,
    /// name). The request to approve it follows and names only the thread.
    mcp_calls: HashMap<String, (String, String)>,
    /// How many MCP tool calls are in progress on each thread. With more
    /// than one, which of them a request is about cannot be told.
    mcp_running: HashMap<String, usize>,
    /// The changes of each `fileChange` item in progress. The request to
    /// approve one names only the item.
    file_changes: HashMap<String, Value>,
    /// MCP servers already reported as failed; Codex retries and says so again.
    failed_mcp_servers: HashSet<String>,
}

/// What a request to approve a command says, as recorded. One that says
/// more (a grant beyond the command, a retry outside the sandbox) is asking
/// for more than a rule about the command allows.
const COMMAND_APPROVAL: &[&str] = &[
    "kind",
    "threadId",
    "turnId",
    "itemId",
    "startedAtMs",
    "environmentId",
    "command",
    "cwd",
    "commandActions",
    "proposedExecpolicyAmendment",
    "availableDecisions",
];

/// The same for a file change. `reason` and `grantRoot` were always null
/// when recorded; `grantRoot` asks for a whole directory.
const FILE_APPROVAL: &[&str] = &["threadId", "turnId", "itemId", "startedAtMs"];

/// Whether a request has nothing set beyond the fields it is known by.
fn only_known(params: &Value, known: &[&str]) -> bool {
    params.as_object().is_some_and(|fields| {
        fields
            .iter()
            .all(|(key, value)| value.is_null() || known.contains(&key.as_str()))
    })
}

/// A `fileChange` item's (id, changes).
fn file_change(item: &Value) -> Option<(String, Value)> {
    (s(item.get("type")?) == "fileChange").then(|| {
        (
            s(item.get("id").unwrap_or(&Value::Null)).to_string(),
            item.get("changes").cloned().unwrap_or(Value::Null),
        )
    })
}

/// The files a `fileChange` item's changes write: each one's path, and
/// where it moves to.
fn changed_paths(changes: &Value) -> ToolAction {
    let paths: Vec<_> = changes
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| [c.get("path"), c.pointer("/kind/move_path")])
        .filter_map(|p| p.and_then(Value::as_str))
        .map(Into::into)
        .collect();
    if paths.is_empty() {
        ToolAction::Opaque
    } else {
        ToolAction::Edit { paths }
    }
}

/// An `mcpToolCall` item's (id, `server/tool`).
fn mcp_call(item: &Value) -> Option<(String, String)> {
    (s(item.get("type")?) == "mcpToolCall").then(|| {
        (
            s(item.get("id").unwrap_or(&Value::Null)).to_string(),
            format!(
                "{}/{}",
                s(item.get("server").unwrap_or(&Value::Null)),
                s(item.get("tool").unwrap_or(&Value::Null))
            ),
        )
    })
}

impl CodexAppServerParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        match RpcMessage::parse(line) {
            Some(RpcMessage::Notification { method, params }) => match self.child_parent(&params) {
                // A sub-agent's turns are its life, not the main agent's.
                Some(parent) if method == "turn/started" => {
                    self.on_child_turn_started(parent, &params)
                }
                Some(parent) if method == "turn/completed" => {
                    self.on_child_turn_completed(parent, &params)
                }
                // Its token accounting is its own.
                Some(_) if method == "thread/tokenUsage/updated" => vec![],
                Some(parent) => wrap_sub(&parent, self.on_notification(&method, &params)),
                None => self.on_notification(&method, &params),
            },
            Some(RpcMessage::Request { id, method, params }) => {
                let events = self.on_server_request(&id, &method, &params);
                match self.child_parent(&params) {
                    Some(parent) => wrap_sub(&parent, events),
                    None => events,
                }
            }
            Some(RpcMessage::Response { error: Some(e), .. }) => vec![AgentEvent::Error(format!(
                "codex: {}",
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("request failed")
            ))],
            Some(RpcMessage::Response { .. }) => vec![],
            None => vec![AgentEvent::Notice(line.to_string())],
        }
    }

    /// The thread of the sub-agent that the item `id` spawned.
    pub fn child_thread(&self, id: &str) -> Option<&str> {
        self.children
            .iter()
            .find(|(_, spawn)| *spawn == id)
            .map(|(thread, _)| thread.as_str())
    }

    /// Codex logs tracing lines (`2026-… ERROR module: msg`) to stderr. Only
    /// WARN/ERROR are surfaced, without the timestamp and module path.
    pub fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        let t = line.trim();
        if t.is_empty() {
            return vec![];
        }
        let mut parts = t.splitn(3, ' ');
        let first = parts.next().unwrap_or("");
        let level = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        if first.len() > 20 && first.ends_with('Z') && first.contains('T') {
            return match level {
                "ERROR" => {
                    let msg = rest.split_once(": ").map(|(_, m)| m).unwrap_or(rest);
                    vec![AgentEvent::Notice(format!("codex: {msg}"))]
                }
                "WARN" => {
                    let msg = rest.split_once(": ").map(|(_, m)| m).unwrap_or(rest);
                    vec![AgentEvent::Notice(format!("codex warning: {msg}"))]
                }
                _ => vec![],
            };
        }
        vec![AgentEvent::Notice(format!("codex: {t}"))]
    }

    /// The spawning item's id when `params` belong to a sub-agent thread.
    fn child_parent(&self, params: &Value) -> Option<String> {
        self.children
            .get(params.get("threadId")?.as_str()?)
            .cloned()
    }

    /// A sub-agent's first turn starts right after it is announced; a later
    /// one means it was sent back to work.
    fn on_child_turn_started(&mut self, id: String, p: &Value) -> Vec<AgentEvent> {
        let thread = s(p.get("threadId").unwrap_or(&Value::Null));
        if !self.running_children.insert(thread.to_string()) {
            return vec![];
        }
        vec![AgentEvent::SubagentStarted {
            id,
            description: self.child_names.get(thread).cloned().unwrap_or_default(),
            kind: None,
        }]
    }

    /// The end of a sub-agent's turn is the end of its work. The main thread
    /// also gets a `subAgentActivity` item, but not when the turn was
    /// interrupted from outside, and never with the report.
    fn on_child_turn_completed(&mut self, id: String, p: &Value) -> Vec<AgentEvent> {
        let thread = s(p.get("threadId").unwrap_or(&Value::Null));
        if !self.running_children.remove(thread) {
            return vec![];
        }
        let status = match s(p.pointer("/turn/status").unwrap_or(&Value::Null)) {
            "completed" => SubagentStatus::Completed,
            "interrupted" | "cancelled" => SubagentStatus::Cancelled,
            _ => SubagentStatus::Failed,
        };
        let result = match status {
            // The turn's summary ends with the sub-agent's answer.
            SubagentStatus::Completed => p
                .pointer("/turn/items")
                .and_then(Value::as_array)
                .and_then(|items| {
                    items.iter().rev().find_map(|i| {
                        (s(i.get("type").unwrap_or(&Value::Null)) == "agentMessage")
                            .then(|| s(i.get("text").unwrap_or(&Value::Null)).to_string())
                    })
                })
                .filter(|t| !t.is_empty()),
            SubagentStatus::Failed => p
                .pointer("/turn/error/message")
                .and_then(Value::as_str)
                .map(str::to_string),
            SubagentStatus::Cancelled => None,
        };
        vec![AgentEvent::SubagentEnded { id, status, result }]
    }

    fn on_notification(&mut self, method: &str, p: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match method {
            "thread/started" => out.push(AgentEvent::SessionStarted {
                session_id: s(p.pointer("/thread/id").unwrap_or(&Value::Null)).to_string(),
                model: p
                    .pointer("/thread/model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
            "turn/started" => {
                if !self.turn_started {
                    self.turn_started = true;
                    out.push(AgentEvent::TurnStarted);
                }
            }
            "item/started" => {
                if let Some(item) = p.get("item") {
                    if let Some(call) = mcp_call(item) {
                        let thread = s(p.get("threadId").unwrap_or(&Value::Null));
                        self.mcp_calls.insert(thread.to_string(), call);
                        *self.mcp_running.entry(thread.to_string()).or_default() += 1;
                    }
                    if let Some((id, changes)) = file_change(item) {
                        self.file_changes.insert(id, changes);
                    }
                    self.on_item_started(item, &mut out);
                }
            }
            "item/completed" => {
                if let Some(item) = p.get("item") {
                    if mcp_call(item).is_some() {
                        let thread = s(p.get("threadId").unwrap_or(&Value::Null));
                        self.mcp_calls.remove(thread);
                        if let Some(running) = self.mcp_running.get_mut(thread) {
                            *running = running.saturating_sub(1);
                        }
                    }
                    if let Some((id, _)) = file_change(item) {
                        self.file_changes.remove(&id);
                    }
                    self.on_item_completed(item, &mut out);
                }
            }
            "item/agentMessage/delta" => {
                self.streamed_items
                    .insert(s(p.get("itemId").unwrap_or(&Value::Null)).to_string());
                let d = s(p.get("delta").unwrap_or(&Value::Null));
                if !d.is_empty() {
                    out.push(AgentEvent::TextDelta(d.to_string()));
                }
            }
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                self.streamed_items
                    .insert(s(p.get("itemId").unwrap_or(&Value::Null)).to_string());
                let d = s(p.get("delta").unwrap_or(&Value::Null));
                if !d.is_empty() {
                    out.push(AgentEvent::ThinkingDelta(d.to_string()));
                }
            }
            "item/commandExecution/outputDelta" => {
                let d = s(p.get("delta").unwrap_or(&Value::Null));
                if !d.is_empty() {
                    out.push(AgentEvent::ToolCallDelta {
                        id: s(p.get("itemId").unwrap_or(&Value::Null)).to_string(),
                        name: "shell".into(),
                        delta: d.to_string(),
                    });
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(last) = p.pointer("/tokenUsage/last") {
                    out.push(AgentEvent::Usage(Usage {
                        input: u(last, "inputTokens"),
                        output: u(last, "outputTokens"),
                        cache_read: u(last, "cachedInputTokens"),
                        cache_write: u(last, "cacheWriteInputTokens"),
                        cost_usd: None,
                        cumulative: false,
                    }));
                }
                // The last request's total is what currently sits in the context.
                let used = p
                    .pointer("/tokenUsage/last/totalTokens")
                    .and_then(Value::as_u64);
                let window = p
                    .pointer("/tokenUsage/modelContextWindow")
                    .and_then(Value::as_u64);
                if used.is_some() || window.is_some() {
                    out.push(AgentEvent::Context(ContextUsage { used, window }));
                }
            }
            "turn/plan/updated" => {
                let entries = p
                    .get("plan")
                    .and_then(Value::as_array)
                    .map(|steps| {
                        steps
                            .iter()
                            .map(|st| PlanEntry {
                                text: s(st.get("step").unwrap_or(&Value::Null)).to_string(),
                                status: PlanStatus::parse(s(st
                                    .get("status")
                                    .unwrap_or(&Value::Null))),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(AgentEvent::PlanUpdated {
                    entries,
                    explanation: p
                        .get("explanation")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            "account/rateLimits/updated" => {
                let limits = p.get("rateLimits").unwrap_or(&Value::Null);
                if let Some(w) = rate_window(limits.get("primary")) {
                    self.rate_primary = Some(w);
                }
                if let Some(w) = rate_window(limits.get("secondary")) {
                    self.rate_secondary = Some(w);
                }
                out.push(AgentEvent::RateLimit(RateLimitInfo {
                    status: limits
                        .get("rateLimitReachedType")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    windows: self
                        .rate_primary
                        .iter()
                        .chain(self.rate_secondary.iter())
                        .cloned()
                        .collect(),
                }));
            }
            "turn/completed" => {
                self.turn_started = false;
                let status = s(p.pointer("/turn/status").unwrap_or(&Value::Null));
                let stop_reason = match status {
                    "completed" => StopReason::Done,
                    "interrupted" | "cancelled" => StopReason::Interrupted,
                    other => StopReason::Error(
                        p.pointer("/turn/error/message")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("turn {other}")),
                    ),
                };
                out.push(AgentEvent::TurnCompleted { stop_reason });
            }
            "error" => {
                let msg = p
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex error");
                if p.get("willRetry").and_then(Value::as_bool).unwrap_or(false) {
                    out.push(AgentEvent::Notice(format!("codex retrying: {msg}")));
                } else {
                    out.push(AgentEvent::Error(msg.to_string()));
                }
            }
            "thread/compacted" => out.push(AgentEvent::Notice("context compacted".into())),
            "warning" | "deprecationNotice" | "configWarning" => {
                // `deprecationNotice` carries `summary` instead of `message`.
                let msg = p
                    .get("message")
                    .or_else(|| p.get("summary"))
                    .and_then(Value::as_str)
                    .unwrap_or("codex warning");
                out.push(AgentEvent::Notice(msg.to_string()));
            }
            "autoApprovalReview/strictReviewRequired" => out.push(AgentEvent::Notice(
                "codex auto-review requires strict review".into(),
            )),
            "mcpServer/startupStatus/updated"
                if s(p.get("status").unwrap_or(&Value::Null)) == "failed" =>
            {
                let name = s(p.get("name").unwrap_or(&Value::Null));
                if self.failed_mcp_servers.insert(name.to_string()) {
                    let error = p
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("MCP server '{name}' failed to start"));
                    out.push(AgentEvent::Notice(error));
                }
            }
            // thread/status/changed, account/updated, remoteControl/*,
            // serverRequest/resolved, item/updated…
            _ => {}
        }
        out
    }

    fn on_item_started(&mut self, item: &Value, out: &mut Vec<AgentEvent>) {
        let id = s(item.get("id").unwrap_or(&Value::Null)).to_string();
        match s(item.get("type").unwrap_or(&Value::Null)) {
            "commandExecution" => out.push(AgentEvent::ToolCallStarted {
                id,
                name: "shell".into(),
                input: json!({
                    "command": item.get("command").cloned().unwrap_or(Value::Null),
                    "cwd": item.get("cwd").cloned().unwrap_or(Value::Null),
                }),
            }),
            "fileChange" => out.push(AgentEvent::ToolCallStarted {
                id,
                name: "apply_patch".into(),
                input: json!({"changes": item.get("changes").cloned().unwrap_or(Value::Null)}),
            }),
            "mcpToolCall" => out.push(AgentEvent::ToolCallStarted {
                id,
                name: mcp_call(item).map(|(_, name)| name).unwrap_or_default(),
                input: item.get("arguments").cloned().unwrap_or(Value::Null),
            }),
            "dynamicToolCall" => out.push(AgentEvent::ToolCallStarted {
                id,
                name: s(item.get("tool").unwrap_or(&Value::Null)).to_string(),
                input: item.get("arguments").cloned().unwrap_or(Value::Null),
            }),
            "contextCompaction" => out.push(AgentEvent::Notice("compacting context…".into())),
            // One item announces the sub-agent and names its thread. Later
            // ones (`interacted`, `interrupted`, `completed`, each with an id
            // of its own) only repeat what the sub-agent's turns say.
            "subAgentActivity" => {
                if s(item.get("kind").unwrap_or(&Value::Null)) == "started" {
                    let thread = s(item.get("agentThreadId").unwrap_or(&Value::Null)).to_string();
                    let path = s(item.get("agentPath").unwrap_or(&Value::Null));
                    // All there is to describe it: `/root/read_notes`.
                    let name = path.rsplit('/').next().unwrap_or(path).to_string();
                    self.children.insert(thread.clone(), id.clone());
                    self.child_names.insert(thread.clone(), name.clone());
                    self.running_children.insert(thread);
                    out.push(AgentEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "agent".into(),
                        input: json!({"path": path}),
                    });
                    out.push(AgentEvent::SubagentStarted {
                        id,
                        description: name,
                        kind: None,
                    });
                }
            }
            "collabAgentToolCall" => {
                // `wait` has neither a prompt nor receivers; say nothing then.
                let mut input = serde_json::Map::new();
                if let Some(prompt) = item.get("prompt").filter(|p| !p.is_null()) {
                    input.insert("prompt".into(), prompt.clone());
                }
                if let Some(agents) = item
                    .get("receiverThreadIds")
                    .filter(|a| a.as_array().is_some_and(|a| !a.is_empty()))
                {
                    input.insert("agents".into(), agents.clone());
                }
                out.push(AgentEvent::ToolCallStarted {
                    id,
                    name: format!("agent:{}", s(item.get("tool").unwrap_or(&Value::Null))),
                    input: if input.is_empty() {
                        Value::Null
                    } else {
                        Value::Object(input)
                    },
                });
            }
            "webSearch" => out.push(AgentEvent::ToolCallStarted {
                id,
                name: "web_search".into(),
                input: json!({"query": item.get("query").cloned().unwrap_or(Value::Null)}),
            }),
            _ => {} // agentMessage, reasoning, userMessage, plan…
        }
    }

    fn on_item_completed(&mut self, item: &Value, out: &mut Vec<AgentEvent>) {
        let id = s(item.get("id").unwrap_or(&Value::Null)).to_string();
        let streamed = self.streamed_items.remove(&id);
        match s(item.get("type").unwrap_or(&Value::Null)) {
            "agentMessage" if !streamed => {
                let t = s(item.get("text").unwrap_or(&Value::Null));
                if !t.is_empty() {
                    out.push(AgentEvent::TextDelta(t.to_string()));
                }
            }
            "reasoning" if !streamed => {
                let t = item
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("summary").and_then(Value::as_str))
                    .unwrap_or("");
                if !t.is_empty() {
                    out.push(AgentEvent::ThinkingDelta(t.to_string()));
                }
            }
            "commandExecution" => {
                let exit = item.get("exitCode").and_then(Value::as_i64);
                out.push(AgentEvent::ToolCallResult {
                    id,
                    output: item
                        .get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    is_error: exit.is_some_and(|c| c != 0)
                        || s(item.get("status").unwrap_or(&Value::Null)) == "failed",
                });
            }
            "fileChange" => {
                let changes = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .map(|c| {
                        c.iter()
                            .map(|ch| {
                                format!(
                                    "{} {}",
                                    s(ch.get("kind").unwrap_or(&Value::Null)),
                                    s(ch.get("path").unwrap_or(&Value::Null))
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                out.push(AgentEvent::ToolCallResult {
                    id,
                    output: changes,
                    is_error: s(item.get("status").unwrap_or(&Value::Null)) == "failed",
                });
            }
            "mcpToolCall" | "dynamicToolCall" => out.push(AgentEvent::ToolCallResult {
                id,
                output: item
                    .get("result")
                    .map(|r| match r {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default(),
                is_error: item.get("error").is_some_and(|e| !e.is_null())
                    || s(item.get("status").unwrap_or(&Value::Null)) == "failed",
            }),
            "webSearch" => out.push(AgentEvent::ToolCallResult {
                id,
                output: String::new(),
                is_error: false,
            }),
            "contextCompaction" => out.push(AgentEvent::Notice("context compacted".into())),
            // Spawning returns at once; the sub-agent works on.
            "subAgentActivity" if s(item.get("kind").unwrap_or(&Value::Null)) == "started" => out
                .push(AgentEvent::ToolCallResult {
                    id,
                    output: String::new(),
                    is_error: false,
                }),
            "collabAgentToolCall" => out.push(AgentEvent::ToolCallResult {
                id,
                output: String::new(),
                is_error: s(item.get("status").unwrap_or(&Value::Null)) == "failed",
            }),
            // A proposed plan (plan mode) is prose; the step list arrives as
            // `turn/plan/updated`.
            "plan" => {
                let text = s(item.get("text").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    out.push(AgentEvent::Notice(format!("plan:\n{text}")));
                }
            }
            _ => {}
        }
    }

    fn on_server_request(&mut self, id: &Value, method: &str, p: &Value) -> Vec<AgentEvent> {
        let rid = match id {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut item_id = p.get("itemId").and_then(Value::as_str).map(str::to_string);
        let kind = match method {
            "item/commandExecution/requestApproval" => PermissionKind::ToolUse {
                tool: "shell".into(),
                action: match p.get("command").and_then(Value::as_str) {
                    Some(command) if only_known(p, COMMAND_APPROVAL) => ToolAction::Shell {
                        command: unwrap_shell(command),
                        cwd: p.get("cwd").and_then(Value::as_str).map(Into::into),
                    },
                    _ => ToolAction::Opaque,
                },
                input: json!({
                    "command": p.get("command").cloned().unwrap_or(Value::Null),
                    "cwd": p.get("cwd").cloned().unwrap_or(Value::Null),
                    "reason": p.get("reason").cloned().unwrap_or(Value::Null),
                }),
                description: p.get("reason").and_then(Value::as_str).map(str::to_string),
            },
            "item/fileChange/requestApproval" => {
                // The request has no changes of its own; the item it names does.
                let changes = item_id
                    .as_ref()
                    .and_then(|id| self.file_changes.get(id))
                    .cloned()
                    .unwrap_or(Value::Null);
                PermissionKind::ToolUse {
                    tool: "apply_patch".into(),
                    action: if only_known(p, FILE_APPROVAL) {
                        changed_paths(&changes)
                    } else {
                        ToolAction::Opaque
                    },
                    input: json!({
                        "reason": p.get("reason").cloned().unwrap_or(Value::Null),
                        "grantRoot": p.get("grantRoot").cloned().unwrap_or(Value::Null),
                        "changes": changes,
                    }),
                    description: p.get("reason").and_then(Value::as_str).map(str::to_string),
                }
            }
            "item/permissions/requestApproval" => PermissionKind::ToolUse {
                tool: "permissions".into(),
                action: ToolAction::Opaque,
                input: p.clone(),
                description: p.get("reason").and_then(Value::as_str).map(str::to_string),
            },
            "item/tool/requestUserInput" => PermissionKind::Question {
                questions: p
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|qs| {
                        qs.iter()
                            .map(|q| Question {
                                id: s(q.get("id").unwrap_or(&Value::Null)).to_string(),
                                header: s(q.get("header").unwrap_or(&Value::Null)).to_string(),
                                text: s(q.get("question").unwrap_or(&Value::Null)).to_string(),
                                options: q
                                    .get("options")
                                    .and_then(Value::as_array)
                                    .map(|os| {
                                        os.iter()
                                            .map(|o| {
                                                QuestionOption::new(
                                                    s(o.get("label").unwrap_or(&Value::Null)),
                                                    s(o.get("description").unwrap_or(&Value::Null)),
                                                )
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                allow_other: q
                                    .get("isOther")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(true),
                                multi: false,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            // Approval of an MCP tool call, asked as an elicitation with
            // nothing to fill in.
            "mcpServer/elicitation/request"
                if p.pointer("/_meta/codex_approval_kind")
                    .and_then(Value::as_str)
                    == Some("mcp_tool_call") =>
            {
                let thread = s(p.get("threadId").unwrap_or(&Value::Null));
                let alone = self.mcp_running.get(thread) == Some(&1);
                let message = s(p.get("message").unwrap_or(&Value::Null));
                let (call, tool) = match self.mcp_calls.get(thread) {
                    Some((id, name)) => (Some(id.clone()), name.clone()),
                    None => (
                        None,
                        s(p.get("serverName").unwrap_or(&Value::Null)).to_string(),
                    ),
                };
                item_id = call;
                PermissionKind::ToolUse {
                    // The request names no call: it is taken to be about the
                    // one in progress on its thread. For a rule to answer it
                    // that has to be the only one, and the question has to
                    // name the same server and tool.
                    action: match tool.split_once('/') {
                        Some((server, tool))
                            if alone
                                && message.contains(&format!("the {server} MCP server"))
                                && message.contains(&format!("tool \"{tool}\"")) =>
                        {
                            ToolAction::Mcp {
                                server: server.to_string(),
                                tool: tool.to_string(),
                            }
                        }
                        _ => ToolAction::Opaque,
                    },
                    tool,
                    input: p
                        .pointer("/_meta/tool_params")
                        .cloned()
                        .unwrap_or(json!({})),
                    description: p.get("message").and_then(Value::as_str).map(str::to_string),
                }
            }
            "mcpServer/elicitation/request" => {
                let server = s(p.get("serverName").unwrap_or(&Value::Null));
                let message = p
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| p.pointer("/params/message").and_then(Value::as_str))
                    .unwrap_or("needs your input");
                PermissionKind::Input {
                    title: format!("{ELICITATION_PREFIX}{server}: {message}"),
                    placeholder: None,
                    prefill: None,
                    multiline: false,
                }
            }
            other => {
                return vec![AgentEvent::Notice(format!(
                    "codex request '{other}' is not supported; declined"
                ))];
            }
        };
        vec![AgentEvent::PermissionRequest(PermissionRequest {
            id: rid,
            kind,
            tool_call_id: item_id,
        })]
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn wrap_sub(parent: &str, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
    events
        .into_iter()
        .map(|event| AgentEvent::Sub {
            parent: parent.to_string(),
            event: Box::new(event),
        })
        .collect()
}

/// A `primary` / `secondary` rate-limit window, labelled by its length (`5h`, `7d`).
fn rate_window(w: Option<&Value>) -> Option<RateLimitWindow> {
    let w = w.filter(|w| w.is_object())?;
    let label = match w.get("windowDurationMins").and_then(Value::as_u64) {
        Some(m) if m >= 1440 => format!("{}d", m / 1440),
        Some(m) if m >= 60 => format!("{}h", m / 60),
        Some(m) => format!("{m}m"),
        None => "limit".to_string(),
    };
    Some(RateLimitWindow {
        label,
        used_percent: w
            .get("usedPercent")
            .and_then(Value::as_f64)
            .map(|p| p as f32),
        resets_at: w.get("resetsAt").and_then(Value::as_i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testing::{LineParser, assert_fixture, fixtures_dir};

    impl LineParser for CodexAppServerParser {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            CodexAppServerParser::feed(self, line)
        }
        fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
            CodexAppServerParser::feed_stderr(self, line)
        }
    }

    #[test]
    fn fixture_app_server_mcp_server() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_mcp_server",
        );
    }

    #[test]
    fn fixture_app_server_file_change() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_file_change",
        );
    }

    #[test]
    fn fixture_app_server_two_turns() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_two_turns",
        );
    }

    #[test]
    fn fixture_app_server_fork() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_fork",
        );
    }

    #[test]
    fn fixture_app_server_rewind() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_rewind",
        );
    }

    #[test]
    fn fixture_app_server_subagent() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_subagent",
        );
    }

    #[test]
    fn fixture_app_server_subagent_parallel() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_subagent_parallel",
        );
    }

    #[test]
    fn fixture_app_server_subagent_stopped() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_subagent_stopped",
        );
    }

    #[test]
    fn fixture_app_server_subagent_background() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_subagent_background",
        );
    }

    #[test]
    fn fixture_app_server_subagent_interrupted() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_subagent_interrupted",
        );
    }

    #[test]
    fn fixture_app_server_steer_and_compact() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_steer_and_compact",
        );
    }

    #[test]
    fn fixture_app_server_image_turn() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_image_turn",
        );
    }

    #[test]
    fn plan_update_replaces_the_plan() {
        // Shape from the 0.157.0 app-server schema (TurnPlanUpdatedNotification).
        let mut p = CodexAppServerParser::new();
        let ev = p.feed(r#"{"method":"turn/plan/updated","params":{"threadId":"t","turnId":"u","explanation":"why","plan":[{"step":"say hello","status":"completed"},{"step":"say bye","status":"inProgress"}]}}"#);
        assert_eq!(ev[0].summary(), "PlanUpdated [x] say hello; [~] say bye");
        assert!(
            matches!(&ev[0], AgentEvent::PlanUpdated { explanation: Some(e), .. } if e == "why")
        );
    }

    #[test]
    fn user_input_request_maps_to_question() {
        let mut p = CodexAppServerParser::new();
        let ev = p.feed(r#"{"method":"item/tool/requestUserInput","id":3,"params":{"itemId":"i1","questions":[{"id":"q1","header":"Env","question":"Which env?","options":[{"label":"dev","description":"d"},{"label":"prod","description":"p"}],"isOther":false}]}}"#);
        match &ev[0] {
            AgentEvent::PermissionRequest(r) => {
                assert_eq!(r.id, "3");
                assert_eq!(r.tool_call_id.as_deref(), Some("i1"));
                match &r.kind {
                    PermissionKind::Question { questions } => {
                        assert_eq!(questions[0].id, "q1");
                        assert_eq!(questions[0].options.len(), 2);
                        assert!(!questions[0].allow_other);
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    /// The action of the permission request a line turns into.
    fn requested(parser: &mut CodexAppServerParser, line: Value) -> ToolAction {
        match parser.feed(&line.to_string()).last() {
            Some(AgentEvent::PermissionRequest(PermissionRequest {
                kind: PermissionKind::ToolUse { action, .. },
                ..
            })) => action.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_request_that_asks_for_more_than_its_command_or_files_is_not_matched() {
        let mut p = CodexAppServerParser::new();
        let command = |extra: Value| {
            let mut params = json!({"kind": "command", "threadId": "t", "turnId": "u",
                "itemId": "i", "startedAtMs": 1, "environmentId": "local",
                "command": "/usr/bin/zsh -lc 'cargo test'", "cwd": "/w",
                "commandActions": [], "proposedExecpolicyAmendment": ["cargo", "test"],
                "availableDecisions": ["accept", "cancel"], "reason": null});
            params
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"method": "item/commandExecution/requestApproval", "id": 1, "params": params})
        };
        assert_eq!(
            requested(&mut p, command(json!({}))),
            ToolAction::Shell {
                command: "cargo test".into(),
                cwd: Some("/w".into())
            }
        );
        for extra in [
            json!({"reason": "retry without the sandbox?"}),
            json!({"additionalPermissions": {"network": true}}),
            json!({"networkApprovalContext": {"host": "example.com"}}),
        ] {
            assert_eq!(
                requested(&mut p, command(extra.clone())),
                ToolAction::Opaque,
                "{extra}"
            );
        }

        p.feed(
            &json!({"method": "item/started", "params": {"threadId": "t", "item": {
            "type": "fileChange", "id": "f", "changes": [
                {"path": "/w/a", "kind": {"type": "update", "move_path": "/w/b"}},
                {"path": "/w/c", "kind": {"type": "add"}}]}}})
            .to_string(),
        );
        let change = |extra: Value| {
            let mut params = json!({"threadId": "t", "turnId": "u", "itemId": "f",
                "startedAtMs": 1, "reason": null, "grantRoot": null});
            params
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"method": "item/fileChange/requestApproval", "id": 2, "params": params})
        };
        assert_eq!(
            requested(&mut p, change(json!({}))),
            ToolAction::Edit {
                paths: vec!["/w/a".into(), "/w/b".into(), "/w/c".into()]
            }
        );
        // Write access to a whole directory, asked for with one file's change.
        assert_eq!(
            requested(&mut p, change(json!({"grantRoot": "/w"}))),
            ToolAction::Opaque
        );
        assert_eq!(
            requested(&mut p, change(json!({"itemId": "unknown"}))),
            ToolAction::Opaque
        );
    }

    #[test]
    fn an_mcp_approval_is_matched_only_when_it_is_clear_which_call_it_is_for() {
        let started = |id: &str, tool: &str| {
            json!({"method": "item/started", "params": {"threadId": "t", "item": {
                "type": "mcpToolCall", "id": id, "server": "docs", "tool": tool,
                "arguments": {}}}})
            .to_string()
        };
        let ask = |tool: &str| {
            json!({"method": "mcpServer/elicitation/request", "id": 1, "params": {
                "threadId": "t", "serverName": "docs", "mode": "form",
                "_meta": {"codex_approval_kind": "mcp_tool_call", "tool_params": {}},
                "message": format!("Allow the docs MCP server to run tool \"{tool}\"?")}})
        };
        let search = ToolAction::Mcp {
            server: "docs".into(),
            tool: "search".into(),
        };
        let mut p = CodexAppServerParser::new();
        p.feed(&started("a", "search"));
        assert_eq!(requested(&mut p, ask("search")), search);
        // The question names another tool than the call it is tied to.
        assert_eq!(requested(&mut p, ask("delete")), ToolAction::Opaque);
        // Two calls in progress: either could be meant.
        p.feed(&started("b", "search"));
        assert_eq!(requested(&mut p, ask("search")), ToolAction::Opaque);
        p.feed(
            &json!({"method": "item/completed", "params": {"threadId": "t", "item": {
            "type": "mcpToolCall", "id": "a", "server": "docs", "tool": "search"}}})
            .to_string(),
        );
        // No call at all.
        let mut p = CodexAppServerParser::new();
        assert_eq!(requested(&mut p, ask("search")), ToolAction::Opaque);
    }

    #[test]
    fn failed_turn_and_unknown_request() {
        let mut p = CodexAppServerParser::new();
        let ev = p.feed(r#"{"method":"turn/completed","params":{"turn":{"status":"failed","error":{"message":"nope"}}}}"#);
        assert!(
            matches!(&ev[0], AgentEvent::TurnCompleted { stop_reason: StopReason::Error(e) } if e == "nope")
        );
        let ev = p.feed(r#"{"method":"item/tool/call","id":9,"params":{}}"#);
        assert!(matches!(&ev[0], AgentEvent::Notice(n) if n.contains("not supported")));
        let ev = p.feed(r#"{"method":"mcpServer/elicitation/request","id":10,"params":{"serverName":"cq","threadId":"t","message":"Which domain?"}}"#);
        match &ev[0] {
            AgentEvent::PermissionRequest(r) => {
                assert_eq!(r.id, "10");
                assert!(
                    matches!(&r.kind, PermissionKind::Input { title, .. } if title.starts_with(ELICITATION_PREFIX) && title.contains("Which domain?"))
                );
            }
            other => panic!("{other:?}"),
        }
        // stderr: tracing lines are filtered to WARN/ERROR without noise
        assert!(
            p.feed_stderr("2026-10-03T14:51:23.774097Z INFO codex_core::x: hello")
                .is_empty()
        );
        assert_eq!(
            p.feed_stderr("2026-10-03T14:51:23.774097Z ERROR codex_app_server::bespoke: request failed with code -32601"),
            vec![AgentEvent::Notice("codex: request failed with code -32601".into())]
        );
        assert!(p.feed(r#"{"id":1,"result":{}}"#).is_empty());
    }
}
