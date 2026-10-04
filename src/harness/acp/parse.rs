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

use serde_json::{Value, json};

use crate::core::jsonrpc::RpcMessage;
use crate::core::{
    AgentEvent, CapsUpdate, ContextUsage, HarnessId, ModelInfo, ModelRef, PermissionKind,
    PermissionRequest, PlanEntry, PlanStatus, StopReason, Usage,
};

/// Provider id shown for ACP agents (they pick their own backend).
pub const PROVIDER: &str = "agent";

/// What the driver needs from a session's `configOptions`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionOptions {
    /// Id of the select option in category `model`, with its choices.
    pub model_option: Option<String>,
    pub models: Vec<(String, String, Option<String>)>,
    pub current_model: Option<String>,
    /// Id of the select option in category `thought_level`, with its values.
    pub effort_option: Option<String>,
    pub efforts: Vec<String>,
}

impl SessionOptions {
    /// Read `configOptions` (from a session result or a `config_option_update`).
    pub fn from_config_options(options: Option<&Value>) -> Option<Self> {
        let options = options?.as_array()?;
        let mut out = SessionOptions::default();
        for opt in options {
            // Boolean options carry no choices to offer.
            let Some(choices) = opt.get("options").and_then(Value::as_array) else {
                continue;
            };
            let id = s(opt, "id").to_string();
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
                "thought_level" => {
                    out.effort_option = Some(id);
                    out.efforts = choices.iter().map(|c| s(c, "value").to_string()).collect();
                }
                _ => {}
            }
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
        }
    }

    /// The session was reattached: its cost total did not start at zero.
    pub fn mark_reattached(&mut self) {
        self.cost_baseline_pending = true;
    }

    pub fn set_replaying(&mut self, replaying: bool) {
        self.replaying = replaying;
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
                ..Default::default()
            }));
        } else if let Some(session_id) = result.get("sessionId").and_then(Value::as_str) {
            let options = SessionOptions::from_config_options(result.get("configOptions"));
            out.push(AgentEvent::SessionStarted {
                session_id: session_id.to_string(),
                model: options.as_ref().and_then(|o| o.current_model.clone()),
            });
            if let Some(o) = options {
                out.push(AgentEvent::CapabilitiesChanged(o.caps_update(self.harness)));
            }
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
        } else if let Some(o) = SessionOptions::from_config_options(result.get("configOptions")) {
            // `session/set_config_option` answers with the whole option list:
            // a model switch can change which effort levels exist.
            out.push(AgentEvent::CapabilitiesChanged(o.caps_update(self.harness)));
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
                if let Some(o) = SessionOptions::from_config_options(update.get("configOptions")) {
                    out.push(AgentEvent::CapabilitiesChanged(o.caps_update(self.harness)));
                }
            }
            // user_message_chunk (our own prompt echoed), available_commands_update,
            // current_mode_update, session_info_update, and anything newer.
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
        let tool = self.tools.entry(id.clone()).or_default();
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
                // The agent's own choices; the driver maps a decision onto them.
                suggestions: params.get("options").cloned(),
                description: (!tool.title.is_empty()).then(|| tool.title.clone()),
            },
            tool_call_id: (!id.is_empty()).then_some(id),
        }));
        out
    }
}

/// `agentCapabilities` allows `session/resume` (reattach without replay).
pub fn can_resume(caps: &Value) -> bool {
    caps.pointer("/sessionCapabilities/resume").is_some()
}

/// `agentCapabilities` allows `session/load` (reattach, history replayed).
pub fn can_load(caps: &Value) -> bool {
    caps.get("loadSession")
        .and_then(Value::as_bool)
        .unwrap_or(false)
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
    if let Some(name) = name {
        tool.name = name.to_string();
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
    fn fixture_codex_acp() {
        assert_fixture(&mut parser(), &fixtures_dir(file!()), "codex_acp");
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
