//! pi `--mode rpc` / `--mode json` event parser.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::core::{
    AgentEvent, CapsUpdate, ContextUsage, PermissionKind, PermissionRequest, StopReason, Usage,
};

#[derive(Debug, Default)]
pub struct PiParser {
    /// Session id we started pi with; reported on `get_state`.
    session_id: Option<String>,
    turn_started: bool,
    /// Tool-call names by id, from `toolcall_start`.
    tool_names: HashMap<String, String>,
    announced_tools: HashSet<String>,
    /// Latest `partialResult` text per running tool call.
    partial_output: HashMap<String, String>,
}

impl PiParser {
    pub fn new(session_id: Option<String>) -> Self {
        PiParser {
            session_id,
            ..Default::default()
        }
    }

    pub fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return vec![AgentEvent::Notice(line.to_string())];
        };
        self.feed_value(&val)
    }

    pub fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        let t = line.trim();
        if t.is_empty() {
            return vec![];
        }
        vec![AgentEvent::Notice(format!("pi: {t}"))]
    }

    pub fn feed_value(&mut self, v: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match s(v, "type") {
            "agent_start" | "turn_start" => {
                if !self.turn_started {
                    self.turn_started = true;
                    out.push(AgentEvent::TurnStarted);
                }
            }
            "message_update" => self.on_message_update(v, &mut out),
            "turn_end" => {
                if let Some(usage) = v.pointer("/message/usage") {
                    out.push(AgentEvent::Usage(Usage {
                        input: u(usage, "input"),
                        output: u(usage, "output"),
                        cache_read: u(usage, "cacheRead"),
                        cache_write: u(usage, "cacheWrite"),
                        cost_usd: usage.pointer("/cost/total").and_then(Value::as_f64),
                        cumulative: false,
                    }));
                }
            }
            "tool_execution_start" => {
                let id = s(v, "toolCallId").to_string();
                if self.announced_tools.insert(id.clone()) {
                    out.push(AgentEvent::ToolCallStarted {
                        id,
                        name: s(v, "toolName").to_string(),
                        input: v.get("args").cloned().unwrap_or(Value::Null),
                    });
                }
            }
            "tool_execution_update" => {
                // `partialResult` is a snapshot; forward only what extends the
                // previous one (the final result replaces it anyway).
                let id = s(v, "toolCallId").to_string();
                let text = flatten_result(v.get("partialResult"));
                let seen = self.partial_output.entry(id.clone()).or_default();
                if text.len() > seen.len() && text.starts_with(seen.as_str()) {
                    let delta = text[seen.len()..].to_string();
                    *seen = text;
                    out.push(AgentEvent::ToolCallDelta {
                        id,
                        name: s(v, "toolName").to_string(),
                        delta,
                    });
                }
            }
            "tool_execution_end" => {
                self.partial_output.remove(s(v, "toolCallId"));
                out.push(AgentEvent::ToolCallResult {
                    id: s(v, "toolCallId").to_string(),
                    output: flatten_result(v.get("result")),
                    is_error: v.get("isError").and_then(Value::as_bool).unwrap_or(false),
                });
            }
            "agent_settled" => {
                self.turn_started = false;
                out.push(AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Done,
                });
            }
            "extension_ui_request" => self.on_ui_request(v, &mut out),
            "response" => self.on_response(v, &mut out),
            "auto_retry_start" => out.push(AgentEvent::Notice(format!(
                "retry {}/{}: {}",
                u(v, "attempt"),
                u(v, "maxAttempts"),
                s(v, "errorMessage")
            ))),
            "auto_retry_end" => {
                if !v.get("success").and_then(Value::as_bool).unwrap_or(true) {
                    out.push(AgentEvent::Error(format!(
                        "pi gave up after retries: {}",
                        s(v, "finalError")
                    )));
                }
            }
            "compaction_start" => out.push(AgentEvent::Notice("compacting context…".into())),
            "compaction_end" => {
                if let Some(msg) = v.get("errorMessage").and_then(Value::as_str) {
                    out.push(AgentEvent::Error(format!("compaction failed: {msg}")));
                } else {
                    out.push(AgentEvent::Notice("context compacted".into()));
                }
            }
            "thinking_level_changed" => out.push(AgentEvent::Notice(format!(
                "thinking level: {}",
                s(v, "level")
            ))),
            "extension_error" => out.push(AgentEvent::Error(format!(
                "extension {}: {}",
                s(v, "extensionPath"),
                s(v, "error")
            ))),
            // message_start/end, agent_end, queue_update, session_info_changed, bash_execution_update…
            _ => {}
        }
        out
    }

    fn on_message_update(&mut self, v: &Value, out: &mut Vec<AgentEvent>) {
        let Some(ev) = v.get("assistantMessageEvent") else {
            return;
        };
        if !self.turn_started {
            self.turn_started = true;
            out.push(AgentEvent::TurnStarted);
        }
        match s(ev, "type") {
            "text_delta" => {
                let d = s(ev, "delta");
                if !d.is_empty() {
                    out.push(AgentEvent::TextDelta(d.to_string()));
                }
            }
            "thinking_delta" => {
                let d = s(ev, "delta");
                if !d.is_empty() {
                    out.push(AgentEvent::ThinkingDelta(d.to_string()));
                }
            }
            "toolcall_start" => {
                self.tool_names
                    .insert(s(ev, "id").to_string(), s(ev, "toolName").to_string());
            }
            "toolcall_delta" => {
                // Argument bytes stream without the call id; the complete call
                // arrives in `toolcall_end`, so these are not surfaced.
            }
            "toolcall_end" => {
                if let Some(call) = ev.get("toolCall") {
                    let id = s(call, "id").to_string();
                    if self.announced_tools.insert(id.clone()) {
                        out.push(AgentEvent::ToolCallStarted {
                            id,
                            name: s(call, "name").to_string(),
                            input: call.get("arguments").cloned().unwrap_or(Value::Null),
                        });
                    }
                }
            }
            "error" => out.push(AgentEvent::Error(
                ev.get("error")
                    .map(|e| {
                        e.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| e.to_string())
                    })
                    .unwrap_or_else(|| "provider error".into()),
            )),
            _ => {}
        }
    }

    fn on_ui_request(&mut self, v: &Value, out: &mut Vec<AgentEvent>) {
        let id = s(v, "id").to_string();
        let title = v
            .get("title")
            .or_else(|| v.get("message"))
            .or_else(|| v.get("prompt"))
            .and_then(Value::as_str)
            .unwrap_or("The agent needs your input")
            .to_string();
        let kind = match s(v, "method") {
            "confirm" => PermissionKind::Confirm {
                title,
                message: v
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|_m| v.get("title").is_some())
                    .map(str::to_string),
            },
            "select" => PermissionKind::Select {
                title,
                options: v
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|o| {
                                o.as_str()
                                    .map(str::to_string)
                                    .or_else(|| {
                                        o.get("label").and_then(Value::as_str).map(str::to_string)
                                    })
                                    .unwrap_or_else(|| o.to_string())
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            "input" | "editor" => PermissionKind::Input {
                title,
                placeholder: v
                    .get("placeholder")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                prefill: v
                    .get("prefill")
                    .or_else(|| v.get("value"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                multiline: s(v, "method") == "editor",
            },
            "notify" => {
                let msg = s(v, "message").to_string();
                if s(v, "notifyType") == "error" {
                    out.push(AgentEvent::Error(msg));
                } else {
                    out.push(AgentEvent::Notice(msg));
                }
                return;
            }
            // setStatus, setWidget, setTitle, set_editor_text…
            _ => return,
        };
        out.push(AgentEvent::PermissionRequest(PermissionRequest {
            id,
            kind,
            tool_call_id: None,
        }));
    }

    fn on_response(&mut self, v: &Value, out: &mut Vec<AgentEvent>) {
        let command = s(v, "command");
        if !v.get("success").and_then(Value::as_bool).unwrap_or(true) {
            out.push(AgentEvent::Error(format!(
                "pi {}: {}",
                command,
                s(v, "error")
            )));
            if command == "prompt" {
                self.turn_started = false;
                out.push(AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Error(s(v, "error").to_string()),
                });
            }
            return;
        }
        match command {
            "get_state" => {
                let data = v.get("data").cloned().unwrap_or(Value::Null);
                let sid = data
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.session_id.clone());
                if let Some(sid) = sid {
                    self.session_id = Some(sid.clone());
                    out.push(AgentEvent::SessionStarted {
                        session_id: sid,
                        model: data
                            .pointer("/model/id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                }
                if let Some(model) = data.get("model").filter(|m| m.is_object()) {
                    if let Some(window) = model.get("contextWindow").and_then(Value::as_u64) {
                        out.push(AgentEvent::Context(ContextUsage {
                            used: None,
                            window: Some(window),
                        }));
                    }
                    if let Some(inputs) = model.get("input").and_then(Value::as_array) {
                        out.push(AgentEvent::CapabilitiesChanged(CapsUpdate {
                            image_input: Some(inputs.iter().any(|i| i == "image")),
                            ..Default::default()
                        }));
                    }
                }
            }
            "get_session_stats" => {
                if let Some(c) = v.pointer("/data/contextUsage") {
                    out.push(AgentEvent::Context(ContextUsage {
                        used: c.get("tokens").and_then(Value::as_u64),
                        window: c.get("contextWindow").and_then(Value::as_u64),
                    }));
                }
            }
            "get_available_thinking_levels" => {
                if let Some(levels) = v.pointer("/data/levels").and_then(Value::as_array) {
                    out.push(AgentEvent::CapabilitiesChanged(CapsUpdate::efforts(
                        levels
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                    )));
                }
            }
            "set_model" => {
                if let Some(m) = v.pointer("/data/model/id").and_then(Value::as_str) {
                    out.push(AgentEvent::Notice(format!("pi model: {m}")));
                }
            }
            _ => {}
        }
    }
}

/// Tool results are `{content:[{type:"text",text}, ...]}`.
pub fn flatten_result(result: Option<&Value>) -> String {
    match result {
        Some(Value::String(s)) => s.clone(),
        Some(r) => r
            .get("content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| r.to_string()),
        None => String::new(),
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key)
        .and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64)))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testing::{LineParser, assert_fixture, fixtures_dir};

    impl LineParser for PiParser {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            PiParser::feed(self, line)
        }
        fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
            PiParser::feed_stderr(self, line)
        }
    }

    #[test]
    fn fixture_image_turn() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "image_turn",
        );
    }

    #[test]
    fn fixture_session_stats() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "session_stats",
        );
    }

    #[test]
    fn fixture_basic_and_bash() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "basic_and_bash",
        );
    }

    #[test]
    fn ui_requests_map_to_prompts() {
        let mut p = PiParser::new(None);
        let ev = p.feed(r#"{"type":"extension_ui_request","id":"u1","method":"confirm","title":"Delete?","message":"all files"}"#);
        match &ev[0] {
            AgentEvent::PermissionRequest(r) => {
                assert_eq!(r.id, "u1");
                assert!(
                    matches!(&r.kind, PermissionKind::Confirm { title, message: Some(m) } if title == "Delete?" && m == "all files")
                );
            }
            other => panic!("{other:?}"),
        }
        let ev = p.feed(r#"{"type":"extension_ui_request","id":"u2","method":"select","title":"Pick","options":["a",{"label":"b"}]}"#);
        assert!(
            matches!(&ev[0], AgentEvent::PermissionRequest(r) if matches!(&r.kind, PermissionKind::Select { options, .. } if options == &vec!["a".to_string(), "b".to_string()]))
        );
        let ev = p.feed(r#"{"type":"extension_ui_request","id":"u3","method":"editor","title":"Edit","prefill":"x"}"#);
        assert!(
            matches!(&ev[0], AgentEvent::PermissionRequest(r) if matches!(&r.kind, PermissionKind::Input { multiline: true, prefill: Some(p), .. } if p == "x"))
        );
        assert_eq!(
            p.feed(r#"{"type":"extension_ui_request","id":"u4","method":"notify","message":"hi","notifyType":"info"}"#),
            vec![AgentEvent::Notice("hi".into())]
        );
        assert!(
            p.feed(
                r#"{"type":"extension_ui_request","id":"u5","method":"setStatus","statusKey":"k"}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn failed_prompt_completes_turn_with_error() {
        let mut p = PiParser::new(None);
        p.feed(r#"{"type":"agent_start"}"#);
        let ev = p.feed(r#"{"id":"x","type":"response","command":"prompt","success":false,"error":"no credentials"}"#);
        assert_eq!(ev.len(), 2);
        assert!(
            matches!(&ev[1], AgentEvent::TurnCompleted { stop_reason: StopReason::Error(e) } if e == "no credentials")
        );
    }
}
