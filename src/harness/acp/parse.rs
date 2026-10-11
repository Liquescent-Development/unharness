//! Agent Client Protocol (ACP, protocol version 1) parser.
//!
//! Pure: one stdout line in, zero or more `AgentEvent`s out. Fixture-tested
//! against recordings made with `scripts/record-acp.py`.
//!
//! Responses to our own requests are recognised by their shape (`sessionId`,
//! `stopReason`, `protocolVersion`), so a recording replays without knowing
//! the request ids. The transport driver owns everything that needs those
//! ids: the handshake, failed requests, and answering the agent's requests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::elicitation::Form;
use crate::core::jsonrpc::RpcMessage;
use crate::core::rules::unwrap_shell;
use crate::core::{
    AgentEvent, CapsUpdate, ContextUsage, HarnessCommand, HarnessId, ModelInfo, ModelRef,
    PermissionKind, PermissionRequest, PlanEntry, PlanStatus, StopReason, ToolAction, Usage,
};
use crate::harness::claude::parse::tool_action as claude_tool_action;

/// Provider id shown for ACP agents (they pick their own backend).
pub const PROVIDER: &str = "agent";

/// What the driver needs from a session's `configOptions`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionOptions {
    /// Id of the select option in category `model`, with its choices.
    pub model_option: Option<String>,
    pub models: Vec<(String, String, Option<String>)>,
    pub current_model: Option<String>,
    /// Id of the select option for reasoning effort, with its values: the
    /// one the harness declared, else the one in category `thought_level`.
    pub effort_option: Option<String>,
    pub efforts: Vec<String>,
    pub current_effort: Option<String>,
    /// Id of the select option in category `mode` (the session's mode, as
    /// an option), with its values and the one in effect.
    pub mode_option: Option<String>,
    pub mode_values: Vec<String>,
    pub current_mode: Option<String>,
}

impl SessionOptions {
    /// Read `configOptions` (from a session result or a `config_option_update`).
    ///
    /// `declared_effort` is the id of the option a harness knows to be its
    /// agent's reasoning effort under a category the spec does not name
    /// (Mistral Vibe's `thinking`). No such id is assumed for an agent that
    /// did not declare one: setting an option can do more than the name
    /// says (vibe's rewrites its own config file).
    pub fn from_config_options(
        options: Option<&Value>,
        declared_effort: Option<&str>,
    ) -> Option<Self> {
        let options = options?.as_array()?;
        let mut out = SessionOptions::default();
        let mut declared = None;
        let mut thought_level = None;
        for opt in options {
            // Boolean options carry no choices to offer.
            let Some(choices) = select_choices(opt) else {
                continue;
            };
            let id = s(opt, "id").to_string();
            let values = || choices.iter().map(|c| s(c, "value").to_string()).collect();
            let current = || {
                opt.get("currentValue")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            if declared_effort.is_some_and(|d| d == id) {
                declared = Some((id, values(), current()));
                continue;
            }
            match s(opt, "category") {
                "model" => {
                    out.model_option = Some(id);
                    out.current_model = opt
                        .get("currentValue")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    out.models = choices
                        .iter()
                        .map(|c| {
                            let value = s(c, "value").to_string();
                            let name = match s(c, "name") {
                                "" => value.clone(),
                                n => n.to_string(),
                            };
                            let description = c
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            (value, name, description)
                        })
                        .collect();
                }
                "thought_level" => thought_level = Some((id, values(), current())),
                "mode" => {
                    out.current_mode = opt
                        .get("currentValue")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    out.mode_values = values();
                    out.mode_option = Some(id);
                }
                _ => {}
            }
        }
        if let Some((id, efforts, current)) = declared.or(thought_level) {
            out.effort_option = Some(id);
            out.efforts = efforts;
            out.current_effort = current;
        }
        Some(out)
    }

    pub fn caps_update(&self, harness: HarnessId) -> CapsUpdate {
        CapsUpdate {
            effort_levels: Some(self.efforts.clone()),
            models: Some(
                self.models
                    .iter()
                    .map(|(value, name, description)| ModelInfo {
                        model_ref: ModelRef::new(harness, PROVIDER, value.clone()),
                        display_name: name.clone(),
                        description: description.clone(),
                        effort_levels: None,
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }
}

/// A tool call as reported so far. `tool_call_update` sends only what changed.
#[derive(Debug, Default)]
struct ToolState {
    name: String,
    /// The agent gave the tool a name (else `name` is the protocol's kind).
    named: bool,
    /// The call is one of Claude Code's tools, passed on with its input.
    claude: bool,
    /// The protocol's category: `execute`, `edit`, `read`, …
    kind: String,
    title: String,
    input: Value,
    content: Value,
    raw_output: Option<Value>,
    locations: Value,
    /// Text streamed by agents that report terminal output out of band.
    terminal: String,
    started: bool,
    finished: bool,
}

#[derive(Debug)]
pub struct AcpParser {
    harness: HarnessId,
    tools: HashMap<String, ToolState>,
    /// `usage_update.cost` is a running total; a turn's cost is the difference.
    last_cost: f64,
    turn_cost: f64,
    /// While a session is being loaded the agent replays its history as
    /// updates; the transcript already has it.
    replaying: bool,
    /// The next cost total includes turns from before this process (a
    /// reattached session): take it as the baseline, not as this turn's cost.
    cost_baseline_pending: bool,
    /// The option id the harness declared as its agent's reasoning effort.
    effort_option: Option<String>,
    /// Calls whose input holds questions (`rawInput.questions`), oldest
    /// first: a form elicitation that names another id is tied to the
    /// newest open one that asks what it asks (vibe-acp 2.26.1).
    question_calls: Vec<String>,
    /// A form elicitation has come (`CapsUpdate::ask_user_question`).
    asked: bool,
    /// The session's directory, for an agent known to write a relative
    /// path in an edit's `diff` there (`resolving_relative_edits`).
    edits_relative_to: Option<PathBuf>,
}

impl AcpParser {
    pub fn new(harness: HarnessId) -> Self {
        AcpParser {
            harness,
            tools: HashMap::new(),
            last_cost: 0.0,
            turn_cost: 0.0,
            replaying: false,
            cost_baseline_pending: false,
            effort_option: None,
            question_calls: Vec::new(),
            asked: false,
            edits_relative_to: None,
        }
    }

    /// Take a relative path in an edit's `diff` content as relative to
    /// `cwd`, the session's directory, which must be absolute. Only for an
    /// agent seen to write such a path there (Mistral Vibe, its harness
    /// says why); a path with anything but plain names in it (`.`, `..`,
    /// a leading `~`) stays unknown, and the call's `locations` must still
    /// name the same files.
    pub fn resolving_relative_edits(self, cwd: Option<PathBuf>) -> Self {
        AcpParser {
            edits_relative_to: cwd.filter(|c| c.is_absolute()),
            ..self
        }
    }

    /// Read reasoning effort from the config option with this id, whatever
    /// its category (see [`SessionOptions::from_config_options`]).
    pub fn declaring_effort(self, option: Option<String>) -> Self {
        AcpParser {
            effort_option: option,
            ..self
        }
    }

    /// Read `configOptions` with the effort option this parser was given.
    pub fn session_options(&self, options: Option<&Value>) -> Option<SessionOptions> {
        SessionOptions::from_config_options(options, self.effort_option.as_deref())
    }

    /// The session was reattached: its cost total did not start at zero.
    pub fn mark_reattached(&mut self) {
        self.cost_baseline_pending = true;
    }

    pub fn set_replaying(&mut self, replaying: bool) {
        self.replaying = replaying;
    }

    /// The protocol's kind (`edit`, `execute`, …) of a tool call, as merged
    /// from every update and request seen for it so far; `None` for a call
    /// never seen or one that named no kind.
    pub fn tool_kind(&self, tool_call_id: &str) -> Option<&str> {
        let tool = self.tools.get(tool_call_id)?;
        (!tool.kind.is_empty()).then_some(tool.kind.as_str())
    }

    pub fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        match RpcMessage::parse(line) {
            Some(RpcMessage::Notification { method, params }) if method == "session/update" => {
                if self.replaying {
                    return vec![];
                }
                self.on_update(params.get("update").unwrap_or(&Value::Null))
            }
            // Extension notifications (`_auth/status_update`, …) are agent-specific.
            Some(RpcMessage::Notification { .. }) => vec![],
            Some(RpcMessage::Request { id, method, params })
                if method == "session/request_permission" =>
            {
                self.on_permission(&id, &params)
            }
            Some(RpcMessage::Request { id, method, params }) if method == "elicitation/create" => {
                self.on_elicitation(line, &id, &params)
            }
            // The driver refuses every other agent → client request.
            Some(RpcMessage::Request { .. }) => vec![],
            Some(RpcMessage::Response { error: Some(e), .. }) => {
                vec![AgentEvent::Error(error_text(&e))]
            }
            Some(RpcMessage::Response {
                result: Some(result),
                ..
            }) => self.on_result(&result),
            Some(RpcMessage::Response { .. }) => vec![],
            // Agents must keep stdout to protocol messages; show what is not.
            None => match line.trim() {
                "" => vec![],
                text => vec![AgentEvent::Notice(text.to_string())],
            },
        }
    }

    fn on_result(&mut self, result: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        if result.get("protocolVersion").is_some() {
            let caps = result.get("agentCapabilities").unwrap_or(&Value::Null);
            out.push(AgentEvent::CapabilitiesChanged(CapsUpdate {
                image_input: Some(
                    caps.pointer("/promptCapabilities/image")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ),
                resume_by_id: Some(can_resume(caps) || can_load(caps)),
                fork: Some(can_fork(caps)),
                mcp_http: Some(mcp_http(caps)),
                ..Default::default()
            }));
        } else if let Some(session_id) = result.get("sessionId").and_then(Value::as_str) {
            let options = self.session_options(result.get("configOptions"));
            out.push(AgentEvent::SessionStarted {
                session_id: session_id.to_string(),
                model: options.as_ref().and_then(|o| o.current_model.clone()),
            });
            let mut update = options
                .map(|o| o.caps_update(self.harness))
                .unwrap_or_default();
            update.plan_mode = plan_mode(result);
            out.push(AgentEvent::CapabilitiesChanged(update));
        } else if let Some(reason) = result.get("stopReason").and_then(Value::as_str) {
            let usage = result.get("usage").unwrap_or(&Value::Null);
            let cost = std::mem::take(&mut self.turn_cost);
            if usage.is_object() || cost > 0.0 {
                out.push(AgentEvent::Usage(Usage {
                    input: u(usage, "inputTokens"),
                    output: u(usage, "outputTokens"),
                    cache_read: u(usage, "cachedReadTokens"),
                    cache_write: u(usage, "cachedWriteTokens"),
                    cost_usd: (cost > 0.0).then_some(cost),
                    cumulative: false,
                }));
            }
            let stop_reason = match reason {
                "end_turn" => StopReason::Done,
                "cancelled" => StopReason::Interrupted,
                "max_tokens" => StopReason::Error("the model hit its output limit".into()),
                "max_turn_requests" => {
                    StopReason::Error("the agent hit its request limit for one turn".into())
                }
                "refusal" => StopReason::Error("the agent refused to continue".into()),
                other => StopReason::Error(format!("turn stopped: {other}")),
            };
            out.push(AgentEvent::TurnCompleted { stop_reason });
        } else if let Some(o) = self.session_options(result.get("configOptions")) {
            // `session/set_config_option` answers with the whole option list:
            // a model switch can change which effort levels exist. A
            // reattached session answers with its options and modes too.
            let mut update = o.caps_update(self.harness);
            update.plan_mode = plan_mode(result);
            out.push(AgentEvent::CapabilitiesChanged(update));
        }
        out
    }

    fn on_update(&mut self, update: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match s(update, "sessionUpdate") {
            "agent_message_chunk" => {
                let text = block_text(update.get("content"));
                if !text.is_empty() {
                    out.push(AgentEvent::TextDelta(text));
                }
            }
            "agent_thought_chunk" => {
                let text = block_text(update.get("content"));
                if !text.is_empty() {
                    out.push(AgentEvent::ThinkingDelta(text));
                }
            }
            "tool_call" | "tool_call_update" => self.on_tool(update, &mut out),
            "plan" => {
                let entries = update
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|e| PlanEntry {
                                text: s(e, "content").to_string(),
                                status: PlanStatus::parse(s(e, "status")),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(AgentEvent::PlanUpdated {
                    entries,
                    explanation: None,
                });
            }
            "usage_update" => {
                let used = update.get("used").and_then(Value::as_u64);
                let window = update.get("size").and_then(Value::as_u64);
                if used.is_some() || window.is_some() {
                    out.push(AgentEvent::Context(ContextUsage { used, window }));
                }
                if let Some(total) = update.pointer("/cost/amount").and_then(Value::as_f64) {
                    if !std::mem::take(&mut self.cost_baseline_pending) {
                        self.turn_cost += (total - self.last_cost).max(0.0);
                    }
                    self.last_cost = total;
                }
            }
            "config_option_update" => {
                if let Some(o) = self.session_options(update.get("configOptions")) {
                    out.push(AgentEvent::CapabilitiesChanged(o.caps_update(self.harness)));
                }
            }
            // Sent again whenever the list changes (claude-agent-acp 0.85.1
            // sent it three times, the first one empty).
            "available_commands_update" => {
                if let Some(commands) = update.get("availableCommands").and_then(Value::as_array) {
                    let commands: Vec<HarnessCommand> =
                        commands.iter().filter_map(command).collect();
                    out.push(AgentEvent::CapabilitiesChanged(CapsUpdate {
                        slash_commands: Some(!commands.is_empty()),
                        commands: Some(commands),
                        ..Default::default()
                    }));
                }
            }
            // The session's title (`title`, `updatedAt`), sent once a turn
            // starts or the agent names it: vibe-acp 2.26.1 sends the first
            // prompt, codex-acp 2.1.1 the prompt and then one it wrote,
            // claude-agent-acp 0.85.1 one it wrote. Not used: by the time it
            // comes the conversation has its title, its first prompt, and an
            // agent's would name only its part of a conversation that can
            // move between harnesses. codex-acp also sends it with no title,
            // its thread's status in `_meta`.
            "session_info_update" => {}
            // The session's mode, and the mode option of a
            // `config_option_update`, are the transport's: it holds the
            // agent to the mode its policy maps to (`modes.rs`).
            // user_message_chunk (our own prompt echoed), and anything newer.
            _ => {}
        }
        out
    }

    /// Merge a `tool_call` / `tool_call_update` into the call's state. The
    /// call is announced once its input can be trusted: when it starts
    /// running, finishes, or asks for permission.
    fn on_tool(&mut self, update: &Value, out: &mut Vec<AgentEvent>) {
        let id = s(update, "toolCallId").to_string();
        if id.is_empty() {
            return;
        }
        let tool = self.tools.entry(id.clone()).or_default();
        merge_tool(tool, update);
        if tool.input.get("questions").is_some_and(Value::is_array)
            && !self.question_calls.contains(&id)
        {
            self.question_calls.push(id.clone());
        }

        let status = s(update, "status");
        let ended = matches!(status, "completed" | "failed");
        if (status == "in_progress" || ended) && !tool.started {
            tool.started = true;
            out.push(started_event(&id, tool));
        }
        if let Some(delta) = update
            .pointer("/_meta/terminal_output_delta/data")
            .and_then(Value::as_str)
            && !delta.is_empty()
        {
            tool.terminal.push_str(delta);
            if !ended {
                out.push(AgentEvent::ToolCallDelta {
                    id: id.clone(),
                    name: tool.name.clone(),
                    delta: delta.to_string(),
                });
            }
        }
        if ended && !tool.finished {
            tool.finished = true;
            let exit_failed = update
                .pointer("/_meta/terminal_exit/exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0);
            out.push(AgentEvent::ToolCallResult {
                id,
                output: tool_output(tool),
                is_error: status == "failed" || exit_failed,
            });
        }
    }

    fn on_permission(&mut self, rpc_id: &Value, params: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let call = params.get("toolCall").unwrap_or(&Value::Null);
        let id = s(call, "toolCallId").to_string();
        // A request without an id stands alone: it must not take what an
        // earlier one without an id said.
        let mut alone = ToolState::default();
        let tool = match id.as_str() {
            "" => &mut alone,
            _ => self.tools.entry(id.clone()).or_default(),
        };
        merge_tool(tool, call);
        if !tool.started && !id.is_empty() {
            tool.started = true;
            out.push(started_event(&id, tool));
        }
        out.push(AgentEvent::PermissionRequest(PermissionRequest {
            id: match rpc_id {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            kind: PermissionKind::ToolUse {
                tool: tool.name.clone(),
                input: tool_input(tool),
                action: tool_action(tool, self.edits_relative_to.as_deref()),
                description: (!tool.title.is_empty()).then(|| tool.title.clone()),
            },
            tool_call_id: (!id.is_empty()).then_some(id),
        }));
        out
    }

    /// A form elicitation: the questions it asks, tied to the tool call
    /// that asks them. One the driver refuses (a URL, a field no question
    /// stands for) gives nothing here.
    fn on_elicitation(&mut self, line: &str, rpc_id: &Value, params: &Value) -> Vec<AgentEvent> {
        let Ok(form) = Form::from_request(line, params) else {
            return vec![];
        };
        let mut out = Vec::new();
        if !std::mem::replace(&mut self.asked, true) {
            out.push(AgentEvent::CapabilitiesChanged(CapsUpdate {
                ask_user_question: Some(true),
                ..Default::default()
            }));
        }
        // The call the form names (claude-agent-acp 0.85.1), else the
        // newest open one that asks these questions: vibe-acp 2.26.1 names
        // an id no tool call has.
        let tools = &self.tools;
        self.question_calls
            .retain(|id| tools.get(id).is_some_and(|t| !t.finished));
        let call = form
            .tool_call_id
            .clone()
            .filter(|id| tools.contains_key(id))
            .or_else(|| {
                self.question_calls
                    .iter()
                    .rev()
                    .find(|id| tools.get(*id).is_some_and(|t| form.asked_by(&t.input)))
                    .cloned()
            });
        let mut input = None;
        if let Some(id) = &call
            && let Some(tool) = self.tools.get_mut(id)
        {
            if !tool.started {
                tool.started = true;
                out.push(started_event(id, tool));
            }
            input = Some(tool.input.clone());
        }
        out.push(AgentEvent::PermissionRequest(PermissionRequest {
            id: match rpc_id {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            kind: form.kind(input.as_ref()),
            tool_call_id: call,
        }));
        out
    }
}

/// `agentCapabilities` allows `session/resume` (reattach without replay).
pub fn can_resume(caps: &Value) -> bool {
    caps.pointer("/sessionCapabilities/resume").is_some()
}

/// `agentCapabilities` allows `session/close`, which ends a session and
/// frees what the agent holds for it.
pub fn can_close(caps: &Value) -> bool {
    caps.pointer("/sessionCapabilities/close")
        .is_some_and(|v| !v.is_null())
}

/// `agentCapabilities` allows `session/fork` (unstable in ACP v1): a new
/// session that starts from what an existing one holds.
pub fn can_fork(caps: &Value) -> bool {
    caps.pointer("/sessionCapabilities/fork")
        .is_some_and(|v| !v.is_null())
}

/// `agentCapabilities` allows `session/load` (reattach, history replayed).
pub fn can_load(caps: &Value) -> bool {
    caps.get("loadSession")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `agentCapabilities` allows MCP servers reached over HTTP.
pub fn mcp_http(caps: &Value) -> bool {
    caps.pointer("/mcpCapabilities/http")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// An `AvailableCommand`: `name`, `description` and `input`, whose
/// `hint` says what goes after the name.
fn command(c: &Value) -> Option<HarnessCommand> {
    HarnessCommand::new(
        c.get("name")?.as_str()?,
        c.get("description").and_then(Value::as_str),
        c.pointer("/input/hint").and_then(Value::as_str),
    )
}

/// Whether a session result's `modes` offer one called `plan`; `None` when
/// the result lists no modes.
fn plan_mode(result: &Value) -> Option<bool> {
    let modes = result.pointer("/modes/availableModes")?.as_array()?;
    Some(modes.iter().any(|m| s(m, "id") == "plan"))
}

/// The values of a select config option, or `None` for one that has none
/// to offer (a boolean). The spec lets `options` be a list of values or a
/// list of groups (`{group, name, options: [values]}`); groups are
/// flattened in order. A value without a `value` cannot be chosen and is
/// left out.
fn select_choices(opt: &Value) -> Option<Vec<&Value>> {
    let options = opt.get("options")?.as_array()?;
    Some(
        options
            .iter()
            .flat_map(|o| match o.get("options").and_then(Value::as_array) {
                Some(group) => group.iter().collect(),
                None => vec![o],
            })
            .filter(|c| !s(c, "value").is_empty())
            .collect(),
    )
}

pub fn error_text(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("request failed");
    match error.pointer("/data/details").and_then(Value::as_str) {
        Some(details) => format!("{message}: {details}"),
        None => message.to_string(),
    }
}

fn merge_tool(tool: &mut ToolState, update: &Value) {
    // `name` is the agent's own tool name; `kind` is the protocol's category.
    let name = update
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| {
            update
                .pointer("/_meta/claudeCode/toolName")
                .and_then(Value::as_str)
        })
        .filter(|n| !n.is_empty());
    if update.pointer("/_meta/claudeCode/toolName").is_some() {
        tool.claude = true;
    }
    if let Some(kind) = update.get("kind").and_then(Value::as_str) {
        tool.kind = kind.to_string();
    }
    if let Some(name) = name {
        tool.name = name.to_string();
        tool.named = true;
    } else if tool.name.is_empty() {
        tool.name = match s(update, "kind") {
            "" => "tool".to_string(),
            kind => kind.to_string(),
        };
    }
    if let Some(title) = update.get("title").and_then(Value::as_str) {
        tool.title = title.to_string();
    }
    for (field, slot) in [
        ("rawInput", &mut tool.input),
        ("content", &mut tool.content),
        ("locations", &mut tool.locations),
    ] {
        if let Some(v) = update.get(field).filter(|v| !v.is_null()) {
            *slot = v.clone();
        }
    }
    if let Some(v) = update.get("rawOutput").filter(|v| !v.is_null()) {
        tool.raw_output = Some(v.clone());
    }
}

fn started_event(id: &str, tool: &ToolState) -> AgentEvent {
    AgentEvent::ToolCallStarted {
        id: id.to_string(),
        name: tool.name.clone(),
        input: tool_input(tool),
    }
}

/// What a tool call does, for allow rules. Claude's tools are known by name.
/// For any other agent there is the protocol's `kind`, and only what says
/// exactly what will happen counts: a command (codex-acp sends it as
/// `rawInput.command`, an MCP call as `server` and `tool`), or the files of
/// an edit's `diff` content. `locations` only describe a call ("where to
/// look"), so a read, a move or a delete is not known well enough. A
/// relative path in a diff is unknown, unless `relative_to` is where the
/// agent is known to write it.
fn tool_action(tool: &ToolState, relative_to: Option<&Path>) -> ToolAction {
    if tool.claude {
        return claude_tool_action(&tool.name, &tool.input);
    }
    let raw = |key: &str| tool.input.get(key).and_then(Value::as_str);
    let entries = |list: &Value| list.as_array().cloned().unwrap_or_default();
    let path = |entry: &Value| entry.get("path").and_then(Value::as_str).map(PathBuf::from);
    match tool.kind.as_str() {
        "execute" => match (raw("command"), raw("server"), raw("tool")) {
            (Some(command), _, _) => ToolAction::Shell {
                command: unwrap_shell(command),
                cwd: raw("cwd").map(Into::into),
            },
            (None, Some(server), Some(name)) => ToolAction::Mcp {
                server: server.to_string(),
                tool: name.to_string(),
            },
            _ => ToolAction::Opaque,
        },
        "edit" => {
            let content = entries(&tool.content);
            let diffs: Option<Vec<PathBuf>> = content
                .iter()
                .map(|c| {
                    (c.get("type").and_then(Value::as_str) == Some("diff"))
                        .then(|| path(c))?
                        .and_then(|p| absolute_edit_path(p, relative_to))
                })
                .collect();
            match diffs {
                // Every piece of content is a diff, and nothing is located
                // anywhere the diffs do not account for.
                Some(paths)
                    if !paths.is_empty()
                        && entries(&tool.locations)
                            .iter()
                            .all(|l| path(l).is_some_and(|p| paths.contains(&p))) =>
                {
                    ToolAction::Edit { paths }
                }
                _ => ToolAction::Opaque,
            }
        }
        "read" | "delete" | "move" => ToolAction::Opaque,
        // A name of the agent's own can be held on to; a kind is too wide.
        _ if tool.named => ToolAction::Other,
        _ => ToolAction::Opaque,
    }
}

/// `path` when it is absolute; a relative one joined to `relative_to`
/// when it has only plain names in it (no `.`, `..` or leading `~`, which
/// the agent might read otherwise); else unknown.
fn absolute_edit_path(path: PathBuf, relative_to: Option<&Path>) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path);
    }
    let base = relative_to?;
    let plain = path.components().all(|c| match c {
        std::path::Component::Normal(name) => !name.to_string_lossy().starts_with('~'),
        _ => false,
    });
    (plain && path.components().next().is_some()).then(|| base.join(path))
}

/// The agent's raw input when it sent one, else what the protocol describes.
fn tool_input(tool: &ToolState) -> Value {
    if tool.input.as_object().is_some_and(|o| !o.is_empty()) {
        return tool.input.clone();
    }
    let mut input = json!({"description": tool.title});
    if let Some(path) = tool.locations.pointer("/0/path") {
        input["path"] = path.clone();
    }
    input
}

fn tool_output(tool: &ToolState) -> String {
    if let Some(Value::String(text)) = &tool.raw_output {
        return text.clone();
    }
    let mut parts = Vec::new();
    for item in tool.content.as_array().into_iter().flatten() {
        match s(item, "type") {
            "content" => {
                let text = block_text(item.get("content"));
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            "diff" => parts.push(diff_text(item)),
            _ => {} // terminal: its output arrives separately
        }
    }
    if !tool.terminal.is_empty() {
        parts.push(tool.terminal.clone());
    }
    if parts.is_empty()
        && let Some(raw) = &tool.raw_output
    {
        return raw.to_string();
    }
    parts.join("\n")
}

/// A `diff` content item as a unified diff the TUI can colour.
fn diff_text(item: &Value) -> String {
    let path = s(item, "path");
    let mut out = format!("--- {path}\n+++ {path}\n@@\n");
    for line in s(item, "oldText").lines() {
        out.push_str(&format!("-{line}\n"));
    }
    for line in s(item, "newText").lines() {
        out.push_str(&format!("+{line}\n"));
    }
    out
}

/// Text of a content block; non-text blocks become `[type]`.
fn block_text(block: Option<&Value>) -> String {
    let Some(block) = block else {
        return String::new();
    };
    match s(block, "type") {
        "text" => s(block, "text").to_string(),
        "resource" => block
            .pointer("/resource/text")
            .and_then(Value::as_str)
            .unwrap_or("[resource]")
            .to_string(),
        "" => String::new(),
        other => format!("[{other}]"),
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testing::{LineParser, assert_fixture, fixtures_dir};

    impl LineParser for AcpParser {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            AcpParser::feed(self, line)
        }
        // Agents log freely to stderr; it is not part of the protocol.
    }

    /// The action of a permission request for this tool call.
    fn requested(tool_call: Value) -> ToolAction {
        let line = json!({"jsonrpc": "2.0", "id": 0, "method": "session/request_permission",
            "params": {"toolCall": tool_call, "options": []}});
        let events = parser().feed(&line.to_string());
        match events.last() {
            Some(AgentEvent::PermissionRequest(PermissionRequest {
                kind: PermissionKind::ToolUse { action, .. },
                ..
            })) => action.clone(),
            other => panic!("{other:?}"),
        }
    }

    /// The shapes are codex-acp 2.1.1's tool calls (`fixtures/codex_acp.jsonl`
    /// and a recording of an edit); it asked for no permission while recorded.
    #[test]
    fn a_tool_without_claudes_names_is_known_by_its_kind() {
        let call = |fields: Value| {
            let mut call = json!({"toolCallId": "t"});
            call.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            requested(call)
        };
        assert_eq!(
            call(json!({"name": "exec_command", "kind": "execute",
                "rawInput": {"command": "/usr/bin/zsh -lc 'cargo test'", "cwd": "/w"}})),
            ToolAction::Shell {
                command: "cargo test".into(),
                cwd: Some("/w".into())
            }
        );
        assert_eq!(
            call(json!({"kind": "execute", "title": "mcp.docs.search",
                "rawInput": {"server": "docs", "tool": "search", "arguments": {}}})),
            ToolAction::Mcp {
                server: "docs".into(),
                tool: "search".into()
            }
        );
        let diff = |path: &str| json!({"type": "diff", "path": path, "newText": "x"});
        assert_eq!(
            call(json!({"kind": "edit", "title": "Editing files",
                "locations": [{"path": "/w/a"}], "content": [diff("/w/a"), diff("/w/b")]})),
            ToolAction::Edit {
                paths: vec!["/w/a".into(), "/w/b".into()]
            }
        );
        // A name can be held on to, a bare kind cannot.
        assert_eq!(
            call(json!({"name": "web_search", "kind": "fetch"})),
            ToolAction::Other
        );
        // What does not say exactly which files or which command: asked about.
        for fields in [
            json!({"kind": "fetch"}),
            json!({"kind": "execute", "title": "something"}),
            json!({"kind": "edit"}),
            json!({"kind": "edit", "locations": [{"path": "/w/a"}]}),
            json!({"kind": "edit", "locations": [{"path": "/w/c"}], "content": [diff("/w/a")]}),
            json!({"kind": "edit", "content": [diff("/w/a"), {"type": "content"}]}),
            json!({"kind": "read", "locations": [{"path": "/w/a"}]}),
            json!({"kind": "move", "locations": [{"path": "/w/a"}]}),
            json!({"kind": "delete", "locations": [{"path": "/w/a"}]}),
            json!({}),
        ] {
            assert_eq!(call(fields.clone()), ToolAction::Opaque, "{fields}");
        }
        // Claude's tools, passed on, are known by name whatever the kind.
        assert_eq!(
            call(
                json!({"name": "Bash", "kind": "execute", "rawInput": {"command": "ls"},
                "_meta": {"claudeCode": {"toolName": "Bash"}}})
            ),
            ToolAction::Shell {
                command: "ls".into(),
                cwd: None
            }
        );
    }

    fn parser() -> AcpParser {
        AcpParser::new(HarnessId::intern("acp-test"))
    }

    #[test]
    fn fixture_claude_agent_acp() {
        assert_fixture(&mut parser(), &fixtures_dir(file!()), "claude_agent_acp");
    }

    #[test]
    fn fixture_claude_agent_acp_resume() {
        assert_fixture(
            &mut parser(),
            &fixtures_dir(file!()),
            "claude_agent_acp_resume",
        );
    }

    #[test]
    fn fixture_claude_agent_acp_mcp() {
        assert_fixture(
            &mut parser(),
            &fixtures_dir(file!()),
            "claude_agent_acp_mcp",
        );
    }

    /// The request a hook answered first, then the agent's `$/cancel_request`
    /// for it (handled by the driver, so not among the parser's events) and
    /// the call failing with the hook's message.
    #[test]
    fn fixture_claude_agent_acp_withdrawn() {
        assert_fixture(
            &mut parser(),
            &fixtures_dir(file!()),
            "claude_agent_acp_withdrawn",
        );
    }

    /// Every `available_commands_update` replaces the list; a project's
    /// command is among them, and a prompt of `/hello world` ran it.
    #[test]
    fn fixture_claude_agent_acp_commands() {
        let dir = fixtures_dir(file!());
        assert_fixture(&mut parser(), &dir, "claude_agent_acp_commands");
        let text = std::fs::read_to_string(dir.join("claude_agent_acp_commands.jsonl")).unwrap();
        let lists: Vec<Vec<HarnessCommand>> = crate::core::testing::replay(&mut parser(), &text)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::CapabilitiesChanged(u) => u.commands,
                _ => None,
            })
            .collect();
        let last = lists.last().unwrap();
        let hello = last.iter().find(|c| c.name == "hello").unwrap();
        assert_eq!(hello.description, "Say hello (project)");
        assert_eq!(hello.hint, None);
        let model = last.iter().find(|c| c.name == "model").unwrap();
        assert_eq!(model.hint.as_deref(), Some("<model>"));
    }

    /// vibe-acp 2.26.1 started in its own `ask` mode
    /// (`VIBE_DEFAULT_AGENT=ask`): each request names its call by
    /// `toolCallId` alone, and what it asks about is the `tool_call` before it.
    #[test]
    fn fixture_vibe_acp_ask() {
        let dir = fixtures_dir(file!());
        assert_fixture(&mut parser(), &dir, "vibe_acp_ask");
        let text = std::fs::read_to_string(dir.join("vibe_acp_ask.jsonl")).unwrap();
        let mut p = parser();
        let mut asked = Vec::new();
        for line in text.lines().filter(|l| !l.starts_with(['>', '!', '#'])) {
            for ev in p.feed(line) {
                if let AgentEvent::PermissionRequest(PermissionRequest {
                    kind: PermissionKind::ToolUse { action, .. },
                    tool_call_id: Some(id),
                    ..
                }) = ev
                {
                    asked.push((p.tool_kind(&id).map(str::to_string), action));
                }
            }
        }
        assert_eq!(
            asked,
            [
                // The diff names `a.txt` as the model wrote it and the
                // location is absolute: not the same file for certain.
                (Some("edit".into()), ToolAction::Opaque),
                (
                    Some("execute".into()),
                    ToolAction::Shell {
                        command: "touch x".into(),
                        cwd: None
                    }
                ),
            ]
        );
    }

    /// What vibe-acp 2.26.1 asked about in `fixture`, replayed by a parser
    /// that takes its relative edit paths as in `cwd`.
    fn vibe_actions(fixture: &str, cwd: Option<&str>) -> Vec<ToolAction> {
        let text = std::fs::read_to_string(fixtures_dir(file!()).join(fixture)).unwrap();
        let mut p = parser().resolving_relative_edits(cwd.map(PathBuf::from));
        let mut asked = Vec::new();
        for line in text.lines().filter(|l| !l.starts_with(['>', '!', '#'])) {
            for ev in p.feed(line) {
                if let AgentEvent::PermissionRequest(PermissionRequest {
                    kind: PermissionKind::ToolUse { action, .. },
                    ..
                }) = ev
                {
                    asked.push(action);
                }
            }
        }
        asked
    }

    /// vibe-acp 2.26.1 writes a relative path where the session is: with
    /// the process started elsewhere, the write landed in the session's
    /// directory while `locations` named the process's (a recording kept
    /// out of the repository). unharness starts it in the session's
    /// directory, so there the two agree, and the vibe harness has a
    /// relative diff path taken as in it.
    #[test]
    fn a_relative_vibe_edit_is_in_the_sessions_directory() {
        assert_eq!(
            vibe_actions("vibe_acp_ask.jsonl", Some("/WORKSPACE"))[0],
            ToolAction::Edit {
                paths: vec!["/WORKSPACE/a.txt".into()]
            }
        );
        // Not where `locations` points elsewhere: a session started in
        // another directory, or one behind a link (vibe resolves links).
        assert_eq!(
            vibe_actions("vibe_acp_ask.jsonl", Some("/elsewhere"))[0],
            ToolAction::Opaque
        );
        // A relative base is no base.
        assert_eq!(
            vibe_actions("vibe_acp_ask.jsonl", Some("WORKSPACE"))[0],
            ToolAction::Opaque
        );
        assert_eq!(
            vibe_actions("vibe_acp_ask.jsonl", None)[0],
            ToolAction::Opaque
        );
    }

    #[test]
    fn only_plain_relative_edit_paths_are_resolved() {
        let base = Some(Path::new("/w"));
        let at = |p: &str| absolute_edit_path(PathBuf::from(p), base);
        assert_eq!(at("a.txt"), Some("/w/a.txt".into()));
        assert_eq!(at("src/lib.rs"), Some("/w/src/lib.rs".into()));
        assert_eq!(at("/etc/hosts"), Some("/etc/hosts".into()));
        for unknown in ["../x", "src/../../x", "./a", "~/x", "~user/x", ""] {
            assert_eq!(at(unknown), None, "{unknown}");
        }
        // Without a base a relative path is unknown, whoever sent it.
        assert_eq!(absolute_edit_path("a.txt".into(), None), None);
        assert_eq!(absolute_edit_path("/w/a".into(), None), Some("/w/a".into()));
    }

    /// A parser as the vibe harness has it: `thinking` its effort, and a
    /// relative edit path in the session's directory.
    fn vibe_parser() -> AcpParser {
        parser()
            .declaring_effort(Some("thinking".into()))
            .resolving_relative_edits(Some("/WORKSPACE".into()))
    }

    /// The tool calls a fixture asks about, as the vibe harness reads them.
    fn vibe_asked(case: &str) -> Vec<(String, ToolAction)> {
        let dir = fixtures_dir(file!());
        assert_fixture(&mut vibe_parser(), &dir, case);
        let text = std::fs::read_to_string(dir.join(format!("{case}.jsonl"))).unwrap();
        let mut p = vibe_parser();
        let mut asked = Vec::new();
        for line in text.lines().filter(|l| !l.starts_with(['>', '!', '#'])) {
            for ev in p.feed(line) {
                if let AgentEvent::PermissionRequest(PermissionRequest {
                    kind: PermissionKind::ToolUse { tool, action, .. },
                    ..
                }) = ev
                {
                    asked.push((tool, action));
                }
            }
        }
        asked
    }

    /// vibe-acp 2.26.1 in its `ask` mode (set through the mode option, as
    /// the driver does): a write allowed, then a command denied, which
    /// vibe reports as a failed call ("Tool execution denied by approval
    /// callback"). The write's relative path is the session's.
    #[test]
    fn fixture_vibe_acp_allow_deny() {
        assert_eq!(
            vibe_asked("vibe_acp_allow_deny"),
            [
                (
                    "edit".to_string(),
                    ToolAction::Edit {
                        paths: vec!["/WORKSPACE/a.txt".into()]
                    }
                ),
                (
                    "execute".to_string(),
                    ToolAction::Shell {
                        command: "touch /WORKSPACE/b.txt".into(),
                        cwd: None
                    }
                ),
            ]
        );
    }

    /// A stdio MCP server's tool (`mcp_<server>.<tool>` in vibe's own
    /// `_meta.tool_name`, which unharness does not read) comes as kind
    /// `other` with only its arguments: asked about, and no rule can know
    /// it. vibe first searches for it (`search_tool_functions`, asked
    /// about by nobody).
    #[test]
    fn fixture_vibe_acp_mcp() {
        assert_eq!(
            vibe_asked("vibe_acp_mcp"),
            [("other".to_string(), ToolAction::Opaque)]
        );
    }

    /// `session/cancel` while a command runs: the command stops and the
    /// prompt answers `cancelled`.
    #[test]
    fn fixture_vibe_acp_cancel() {
        assert_eq!(
            vibe_asked("vibe_acp_cancel"),
            [(
                "execute".to_string(),
                ToolAction::Shell {
                    command: "sleep 45".into(),
                    cwd: None
                }
            )]
        );
    }

    /// `session/load` of the allow-and-deny session in a new process: the
    /// history is replayed (dropped by the driver, not here) and the next
    /// turn knows it.
    #[test]
    fn fixture_vibe_acp_load() {
        assert!(vibe_asked("vibe_acp_load").is_empty());
    }

    /// vibe-acp 2.26.1 starts in accept-edits; the mode option, set to
    /// `ask` before the first prompt, answers with the option list, whose
    /// mode option reads it back. The write is then asked about.
    #[test]
    fn fixture_vibe_acp_mode() {
        let dir = fixtures_dir(file!());
        assert_fixture(&mut parser(), &dir, "vibe_acp_mode");
        let text = std::fs::read_to_string(dir.join("vibe_acp_mode.jsonl")).unwrap();
        let modes: Vec<Option<String>> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| {
                parser()
                    .session_options(v.pointer("/result/configOptions"))
                    .map(|o| o.current_mode)
            })
            .collect();
        assert_eq!(
            modes,
            [
                Some("accept-edits".into()),
                Some("ask".into()),
                Some("auto-approve".into())
            ]
        );
        let o = parser().session_options(Some(&vibe_options())).unwrap();
        assert_eq!(o.mode_option.as_deref(), Some("mode"));
        assert_eq!(o.mode_values, ["ask", "accept-edits", "auto-approve"]);
    }

    /// A loaded vibe session comes back in the mode it was left in. (The
    /// history it replays is dropped by the driver, not here.)
    #[test]
    fn fixture_vibe_acp_mode_load() {
        assert_fixture(&mut parser(), &fixtures_dir(file!()), "vibe_acp_mode_load");
    }

    /// vibe-acp 2.26.1 interrupted with a write's request open (#183): the
    /// request is answered `cancelled` (by the driver, not among the
    /// parser's events), the call is reported failed, and the prompt then
    /// answers `cancelled`.
    #[test]
    fn fixture_vibe_acp_interrupt() {
        assert_fixture(&mut parser(), &fixtures_dir(file!()), "vibe_acp_interrupt");
    }

    /// vibe-acp 2.26.1 says it closes sessions, titles the session after
    /// its first prompt, and answers `session/close` with `{}`; neither
    /// the title nor the answer is an event.
    #[test]
    fn fixture_vibe_acp_close() {
        let dir = fixtures_dir(file!());
        assert_fixture(&mut parser(), &dir, "vibe_acp_close");
        let text = std::fs::read_to_string(dir.join("vibe_acp_close.jsonl")).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let caps = lines
            .iter()
            .find_map(|v| v.pointer("/result/agentCapabilities"))
            .unwrap();
        assert!(can_close(caps));
        for v in &lines {
            let kind = v.pointer("/params/update/sessionUpdate");
            if kind == Some(&json!("session_info_update")) || v.get("result") == Some(&json!({})) {
                assert!(parser().feed(&v.to_string()).is_empty(), "{v}");
            }
        }
        assert!(!can_close(&json!({"sessionCapabilities": {"close": null}})));
        assert!(!can_close(&json!({"sessionCapabilities": {}})));
    }

    /// vibe-acp 2.26.1 answers `session/load` and `session/fork` of a
    /// session it does not have with -32602 "Session not found" (an error
    /// here; the driver takes the answer before the parser sees it and
    /// reports `SessionReplaced`); the new session the driver starts in
    /// its place reports as any other.
    #[test]
    fn fixture_vibe_acp_missing_session() {
        let dir = fixtures_dir(file!());
        for case in ["vibe_acp_load_missing", "vibe_acp_fork_missing"] {
            assert_fixture(&mut parser(), &dir, case);
        }
    }

    /// The questions of every request in a fixture, with the tool call
    /// each is tied to.
    fn asked_questions(case: &str) -> Vec<(Option<String>, Vec<crate::core::Question>)> {
        let text =
            std::fs::read_to_string(fixtures_dir(file!()).join(format!("{case}.jsonl"))).unwrap();
        crate::core::testing::replay(&mut parser(), &text)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::PermissionRequest(PermissionRequest {
                    kind: PermissionKind::Question { questions },
                    tool_call_id,
                    ..
                }) => Some((tool_call_id, questions)),
                _ => None,
            })
            .collect()
    }

    /// vibe-acp 2.26.1 with form elicitation declared: `ask_user_question`
    /// is a tool call, then `elicitation/create` names an id no call has,
    /// so the question is tied to the open call that asks it. The call
    /// says `hideOther: false`: a typed answer is offered.
    #[test]
    fn fixture_vibe_acp_question() {
        assert_fixture(&mut parser(), &fixtures_dir(file!()), "vibe_acp_question");
        let asked = asked_questions("vibe_acp_question");
        let [(call, questions)] = asked.as_slice() else {
            panic!("{asked:?}")
        };
        assert_eq!(
            call.as_deref(),
            Some("effect-70979aa9-c4d8-54a8-a3d8-de5d4837ca50")
        );
        let q = &questions[0];
        assert_eq!((q.id.as_str(), q.header.as_str()), ("q0", "Colour"));
        assert_eq!(q.text, "What is your favourite colour?");
        assert!(q.allow_other && !q.multi);
    }

    /// A multiple choice, then a single one declined; the second call's
    /// option descriptions are only in its `rawInput`.
    #[test]
    fn fixture_vibe_acp_question_multi() {
        assert_fixture(
            &mut parser(),
            &fixtures_dir(file!()),
            "vibe_acp_question_multi",
        );
        let asked = asked_questions("vibe_acp_question_multi");
        assert_eq!(asked.len(), 2);
        assert!(asked[0].1[0].multi);
        assert_eq!(
            asked[1].0.as_deref(),
            Some("effect-0bb7f61e-6fb0-531e-b1ee-e74266c98de2")
        );
        assert_eq!(
            asked[1].1[0].options[0],
            crate::core::QuestionOption::new("Tea", "Brewed leaves, hot or iced")
        );
    }

    /// claude-agent-acp 0.85.1: the elicitation names the AskUserQuestion
    /// call, still pending (announced with it), and the free answer is a
    /// property of its own beside each question (its "Other").
    #[test]
    fn fixture_claude_agent_acp_question() {
        assert_fixture(
            &mut parser(),
            &fixtures_dir(file!()),
            "claude_agent_acp_question",
        );
        let asked = asked_questions("claude_agent_acp_question");
        assert_eq!(asked.len(), 2);
        for (call, questions) in &asked {
            assert!(call.as_deref().is_some_and(|c| c.starts_with("toolu_")));
            assert_eq!(questions.len(), 1);
            assert!(questions[0].allow_other);
        }
        assert_eq!(asked[0].1[0].text, "What's your favourite colour?");
        assert_eq!(
            asked[0].1[0].options[1],
            crate::core::QuestionOption::new("Blue", "Cool, calm, serene")
        );
        assert!(asked[1].1[0].multi);
    }

    #[test]
    fn fixture_codex_acp() {
        assert_fixture(&mut parser(), &fixtures_dir(file!()), "codex_acp");
    }

    /// `session/fork` of a stored session in a new process, as the driver
    /// sends it, from three agents that all advertise it: vibe-acp 2.26.1
    /// (refused until the source was loaded, answered with the source's
    /// mode), codex-acp 2.1.1 and claude-agent-acp 0.85.1 (forked from the
    /// stored session, the fork then resumed). Each fork answered from the
    /// first session's turn.
    #[test]
    fn fixture_fork() {
        let dir = fixtures_dir(file!());
        for case in ["vibe_acp_fork", "codex_acp_fork", "claude_agent_acp_fork"] {
            assert_fixture(&mut parser(), &dir, case);
            let text = std::fs::read_to_string(dir.join(format!("{case}.jsonl"))).unwrap();
            let caps = text
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find_map(|v| v.pointer("/result/agentCapabilities").cloned())
                .unwrap();
            assert!(can_fork(&caps), "{case}");
            let events = crate::core::testing::replay(&mut parser(), &text);
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    AgentEvent::CapabilitiesChanged(CapsUpdate {
                        fork: Some(true),
                        ..
                    })
                )),
                "{case}"
            );
            let said: String = events
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::TextDelta(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            assert!(said.contains("pineapple"), "{case}: {said}");
        }
        assert!(!can_fork(&json!({"sessionCapabilities": {"fork": null}})));
        assert!(!can_fork(&json!({"sessionCapabilities": {}})));
    }

    #[test]
    fn fixture_file_links() {
        assert_fixture(
            &mut parser(),
            &fixtures_dir(file!()),
            "claude_agent_acp_file",
        );
        assert_fixture(&mut parser(), &fixtures_dir(file!()), "codex_acp_file");
    }

    /// The option list Mistral Vibe 2.26.1 answered `session/new` with
    /// (`fixtures/vibe_acp.jsonl`): its reasoning level is the option
    /// `thinking` in category `thinking`, which the spec does not name.
    fn vibe_options() -> Value {
        let text = std::fs::read_to_string(fixtures_dir(file!()).join("vibe_acp.jsonl")).unwrap();
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find_map(|v| v.pointer("/result/configOptions").cloned())
            .unwrap()
    }

    /// Undeclared, vibe's `thinking` gives no effort levels.
    #[test]
    fn fixture_vibe_acp() {
        assert_fixture(&mut parser(), &fixtures_dir(file!()), "vibe_acp");
    }

    /// vibe-acp 2.26.1 started with `VIBE_ACTIVE_MODEL` and a model entry
    /// in `VIBE_MODELS` at thinking `off` (#164), parsed as the vibe
    /// harness does, with `thinking` declared its effort.
    #[test]
    fn fixture_vibe_acp_spawn_env() {
        let mut p = parser().declaring_effort(Some("thinking".into()));
        assert_fixture(&mut p, &fixtures_dir(file!()), "vibe_acp_spawn_env");
    }

    #[test]
    fn effort_is_read_from_thought_level() {
        let options = json!([
            {"id": "reasoning", "name": "Reasoning", "category": "thought_level", "type": "select",
             "currentValue": "low",
             "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
            {"id": "verbose", "name": "Verbose", "type": "boolean", "currentValue": true},
        ]);
        let o = SessionOptions::from_config_options(Some(&options), None).unwrap();
        assert_eq!(o.effort_option.as_deref(), Some("reasoning"));
        assert_eq!(o.efforts, ["low", "high"]);
        assert_eq!(o.current_effort.as_deref(), Some("low"));
        // A declaration that names no option leaves `thought_level` to it.
        let o = SessionOptions::from_config_options(Some(&options), Some("thinking")).unwrap();
        assert_eq!(o.effort_option.as_deref(), Some("reasoning"));
    }

    /// vibe's `thinking` is effort only for a harness that declares it:
    /// setting it rewrites vibe's own config file.
    #[test]
    fn effort_under_another_category_needs_a_declaration() {
        let options = vibe_options();
        let o = SessionOptions::from_config_options(Some(&options), None).unwrap();
        assert_eq!(o.effort_option, None);
        assert!(o.efforts.is_empty());
        assert_eq!(o.model_option.as_deref(), Some("model"));
        assert_eq!(o.current_model.as_deref(), Some("mistral-medium-3.5"));

        let o = SessionOptions::from_config_options(Some(&options), Some("thinking")).unwrap();
        assert_eq!(o.effort_option.as_deref(), Some("thinking"));
        assert_eq!(o.efforts, ["off", "high"]);
        assert_eq!(o.current_effort.as_deref(), Some("high"));
        assert_eq!(o.model_option.as_deref(), Some("model"));

        // The declared option wins over one in `thought_level`.
        let mut both = options.as_array().unwrap().clone();
        both.push(
            json!({"id": "reasoning", "name": "Reasoning", "category": "thought_level",
            "type": "select", "currentValue": "a", "options": [{"value": "a", "name": "A"}]}),
        );
        let both = Value::Array(both);
        let o = SessionOptions::from_config_options(Some(&both), Some("thinking")).unwrap();
        assert_eq!(o.effort_option.as_deref(), Some("thinking"));
        let o = SessionOptions::from_config_options(Some(&both), None).unwrap();
        assert_eq!(o.effort_option.as_deref(), Some("reasoning"));
    }

    /// The parser reports what the transport sends to: an undeclared
    /// `thinking` gives no effort levels (`/effort` then says the agent
    /// exposes none), a declared one gives its values.
    #[test]
    fn parser_reports_a_declared_effort_option() {
        let line = json!({"jsonrpc": "2.0", "id": 2,
            "result": {"sessionId": "s", "configOptions": vibe_options()}})
        .to_string();
        let efforts = |p: &mut AcpParser| {
            p.feed(&line).into_iter().find_map(|e| match e {
                AgentEvent::CapabilitiesChanged(u) => u.effort_levels,
                _ => None,
            })
        };
        assert_eq!(efforts(&mut parser()), Some(vec![]));
        let mut p = parser().declaring_effort(Some("thinking".into()));
        assert_eq!(efforts(&mut p), Some(vec!["off".into(), "high".into()]));
    }

    /// The spec's grouped form: `options` is a list of `{group, name,
    /// options}`, flattened in order for the model and the effort lists.
    #[test]
    fn grouped_select_options_are_flattened() {
        let options = json!([
            {"id": "model", "name": "Model", "category": "model", "type": "select",
             "currentValue": "b",
             "options": [
                {"group": "fast", "name": "Fast", "options": [
                    {"value": "a", "name": "Model A"}]},
                {"group": "smart", "name": "Smart", "options": [
                    {"value": "b", "name": "Model B", "description": "big"},
                    {"value": "c", "name": ""}]},
                {"group": "empty", "name": "Empty", "options": []},
             ]},
            {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select",
             "currentValue": "low",
             "options": [
                {"group": "g1", "name": "G1", "options": [{"value": "low", "name": "Low"}]},
                {"group": "g2", "name": "G2", "options": [{"value": "high", "name": "High"}]},
             ]},
        ]);
        let o = SessionOptions::from_config_options(Some(&options), None).unwrap();
        assert_eq!(
            o.models,
            [
                ("a".into(), "Model A".into(), None),
                ("b".into(), "Model B".into(), Some("big".into())),
                ("c".into(), "c".into(), None),
            ]
        );
        assert_eq!(o.current_model.as_deref(), Some("b"));
        assert_eq!(o.efforts, ["low", "high"]);

        // Groups of a declared option too.
        let thinking = json!([{"id": "thinking", "name": "Thinking", "category": "thinking",
            "type": "select", "currentValue": "off",
            "options": [{"group": "g", "name": "G", "options": [
                {"value": "off", "name": "Off"}, {"value": "max", "name": "Max"}]}]}]);
        let o = SessionOptions::from_config_options(Some(&thinking), Some("thinking")).unwrap();
        assert_eq!(o.efforts, ["off", "max"]);
    }

    #[test]
    fn plan_and_thought_updates() {
        let mut p = parser();
        let ev = p.feed(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"plan","entries":[{"content":"check syntax","priority":"high","status":"completed"},{"content":"fix types","priority":"medium","status":"in_progress"}]}}}"#);
        assert_eq!(
            ev[0].summary(),
            "PlanUpdated [x] check syntax; [~] fix types"
        );
        let ev = p.feed(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}}}"#);
        assert_eq!(ev, vec![AgentEvent::ThinkingDelta("hmm".into())]);
        // Unknown update kinds and extension notifications are ignored.
        assert!(p.feed(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"something_new"}}}"#).is_empty());
        assert!(
            p.feed(r#"{"jsonrpc":"2.0","method":"_vendor/thing","params":{}}"#)
                .is_empty()
        );
    }

    #[test]
    fn tool_without_raw_input_is_described_and_diffed() {
        let mut p = parser();
        let ev = p.feed(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"Editing config.json","kind":"edit","status":"pending","locations":[{"path":"/w/config.json"}]}}}"#);
        assert!(ev.is_empty(), "a pending call is not announced yet");
        let ev = p.feed(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","content":[{"type":"diff","path":"/w/config.json","oldText":"a","newText":"b"}]}}}"#);
        assert_eq!(
            ev[0],
            AgentEvent::ToolCallStarted {
                id: "c1".into(),
                name: "edit".into(),
                input: json!({"description":"Editing config.json","path":"/w/config.json"}),
            }
        );
        assert_eq!(
            ev[1],
            AgentEvent::ToolCallResult {
                id: "c1".into(),
                output: "--- /w/config.json\n+++ /w/config.json\n@@\n-a\n+b\n".into(),
                is_error: false,
            }
        );
    }

    #[test]
    fn stop_reasons_and_errors() {
        let mut p = parser();
        let ev = p.feed(r#"{"jsonrpc":"2.0","id":7,"result":{"stopReason":"cancelled"}}"#);
        assert_eq!(
            ev,
            vec![AgentEvent::TurnCompleted {
                stop_reason: StopReason::Interrupted
            }]
        );
        let ev = p.feed(r#"{"jsonrpc":"2.0","id":8,"result":{"stopReason":"refusal"}}"#);
        assert!(matches!(
            &ev[0],
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::Error(_)
            }
        ));
        let ev = p.feed(r#"{"jsonrpc":"2.0","id":9,"error":{"code":-32000,"message":"Authentication required"}}"#);
        assert_eq!(
            ev,
            vec![AgentEvent::Error("Authentication required".into())]
        );
    }

    #[test]
    fn cost_is_per_turn_and_a_reattached_total_is_only_a_baseline() {
        let cost = |amount: f64| {
            format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"usage_update","used":1,"size":10,"cost":{{"amount":{amount},"currency":"USD"}}}}}}}}"#
            )
        };
        let done = r#"{"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}"#;
        let turn_cost = |p: &mut AcpParser| match p.feed(done).first() {
            Some(AgentEvent::Usage(u)) => u.cost_usd,
            _ => None,
        };
        let mut p = parser();
        p.feed(&cost(0.25));
        assert_eq!(turn_cost(&mut p), Some(0.25));
        p.feed(&cost(0.75));
        assert_eq!(turn_cost(&mut p), Some(0.5));

        let mut p = parser();
        p.mark_reattached();
        p.feed(&cost(3.0));
        assert_eq!(turn_cost(&mut p), None);
        p.feed(&cost(3.5));
        assert_eq!(turn_cost(&mut p), Some(0.5));
    }

    #[test]
    fn replayed_history_is_dropped() {
        let mut p = parser();
        p.set_replaying(true);
        assert!(p.feed(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old"}}}}"#).is_empty());
        p.set_replaying(false);
        assert_eq!(p.feed(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"new"}}}}"#).len(), 1);
    }
}
