//! Antigravity (`agy`) `--output-format stream-json` parser.
//!
//! Verified against agy 1.2.15: the `result` event shape (recorded in
//! `fixtures/auth_required.jsonl`), the `AGY_ERROR:` stderr marker, and the
//! event/step vocabulary embedded in the binary (`init`, `step_update`,
//! `result`; step types `agent_response`, `thought`, `tool_use`, `tool_output`,
//! `code_execution`, `plan`, `system`, `mcp_server_tool_result`, …). The exact
//! field names inside `init` and `step_update` are best-effort and tolerant:
//! several spellings are accepted. Re-record `fixtures/synthetic_turn.jsonl`
//! from a real account when one is available.

use serde_json::Value;

use crate::core::{AgentEvent, StopReason, Usage};

#[derive(Debug, Default)]
pub struct AgyParser {
    session_id: Option<String>,
    turn_started: bool,
    tool_seq: usize,
    /// Tool-call ids announced but not yet resolved, oldest first.
    open_tools: Vec<String>,
}

impl AgyParser {
    pub fn new(session_id: Option<String>) -> Self {
        AgyParser {
            session_id,
            ..Default::default()
        }
    }

    pub fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![AgentEvent::Notice(line.to_string())];
        };
        self.feed_value(&v)
    }

    pub fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        let t = line.trim();
        if t.is_empty() || t.starts_with("Debugger") {
            return vec![];
        }
        if let Some(json) = t.strip_prefix("AGY_ERROR:") {
            let msg = serde_json::from_str::<Value>(json.trim())
                .ok()
                .and_then(|v| {
                    ["message", "error", "status"]
                        .iter()
                        .find_map(|k| v.get(*k).and_then(Value::as_str).map(str::to_string))
                })
                .unwrap_or_else(|| json.trim().to_string());
            return vec![AgentEvent::Error(msg)];
        }
        let lower = t.to_lowercase();
        if lower.starts_with("error:") || lower.contains("authentication required") {
            return vec![AgentEvent::Error(t.to_string())];
        }
        vec![AgentEvent::Notice(format!("agy: {t}"))]
    }

    pub fn feed_value(&mut self, v: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let event = v
            .get("event")
            .or_else(|| v.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match event {
            "init" => {
                let p = v.get("init").unwrap_or(v);
                self.turn_started = true;
                out.push(AgentEvent::TurnStarted);
                if let Some(id) = conversation_id(p)
                    && self.session_id.as_deref() != Some(&id)
                {
                    self.session_id = Some(id.clone());
                    out.push(AgentEvent::SessionStarted {
                        session_id: id,
                        model: s_opt(p, &["model"]),
                    });
                }
            }
            "step_update" | "step" => {
                let step = v.get("step_update").or_else(|| v.get("step")).unwrap_or(v);
                if !self.turn_started {
                    self.turn_started = true;
                    out.push(AgentEvent::TurnStarted);
                }
                self.on_step(step, &mut out);
            }
            "result" => {
                let r = v.get("result").unwrap_or(v);
                if let Some(id) = conversation_id(r)
                    && self.session_id.as_deref() != Some(&id)
                {
                    self.session_id = Some(id.clone());
                    out.push(AgentEvent::SessionStarted {
                        session_id: id,
                        model: None,
                    });
                }
                if let Some(usage) = r.get("usage") {
                    out.push(AgentEvent::Usage(Usage {
                        input: u(usage, "input_tokens"),
                        output: u(usage, "output_tokens"),
                        cache_read: u(usage, "cache_read_tokens"),
                        cache_write: 0,
                        cost_usd: None,
                        cumulative: false,
                    }));
                }
                if let Some(denied) = r.get("denied_actions").and_then(Value::as_array)
                    && !denied.is_empty()
                {
                    out.push(AgentEvent::Notice(format!(
                        "{} action(s) denied by permission rules: {}",
                        denied.len(),
                        denied
                            .iter()
                            .map(|d| d
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| d.to_string()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                let error = s(r, &["error"]);
                let status = s(r, &["status"]).to_uppercase();
                let stop_reason = if !error.is_empty() || status == "ERROR" || status == "FAILED" {
                    StopReason::Error(if error.is_empty() {
                        format!("agy status {status}")
                    } else {
                        error.to_string()
                    })
                } else if status == "CANCELLED" || status == "INTERRUPTED" {
                    StopReason::Interrupted
                } else {
                    StopReason::Done
                };
                self.turn_started = false;
                self.open_tools.clear();
                out.push(AgentEvent::TurnCompleted { stop_reason });
            }
            "error" => out.push(AgentEvent::Error(
                s_opt(v, &["message", "error"]).unwrap_or_else(|| "agy error".into()),
            )),
            _ => {}
        }
        out
    }

    fn on_step(&mut self, step: &Value, out: &mut Vec<AgentEvent>) {
        let step_type = s(step, &["step_type", "type"]);
        let delta = s(step, &["text_delta", "delta", "text"]);
        match step_type {
            "agent_response" | "text" | "model_output" => {
                if !delta.is_empty() {
                    out.push(AgentEvent::TextDelta(delta.to_string()));
                }
            }
            "thought" | "thought_summary" | "thinking" => {
                if !delta.is_empty() {
                    out.push(AgentEvent::ThinkingDelta(delta.to_string()));
                }
            }
            "tool_use"
            | "tool_call"
            | "function_call"
            | "code_execution"
            | "code_execution_call"
            | "mcp_server_tool_call" => {
                let id = s_opt(step, &["tool_use_id", "tool_id", "call_id", "id"]).unwrap_or_else(
                    || {
                        self.tool_seq += 1;
                        format!("agy-{}", self.tool_seq)
                    },
                );
                self.open_tools.push(id.clone());
                out.push(AgentEvent::ToolCallStarted {
                    id,
                    name: s_opt(step, &["tool_name", "name", "function"])
                        .unwrap_or_else(|| step_type.to_string()),
                    input: step
                        .get("input")
                        .or_else(|| step.get("arguments"))
                        .or_else(|| step.get("args"))
                        .cloned()
                        .unwrap_or_else(|| {
                            if delta.is_empty() {
                                Value::Null
                            } else {
                                Value::String(delta.to_string())
                            }
                        }),
                });
            }
            "tool_output"
            | "tool_result"
            | "function_result"
            | "code_execution_result"
            | "mcp_server_tool_result" => {
                let id = s_opt(step, &["tool_use_id", "tool_id", "call_id", "id"])
                    .or_else(|| {
                        if self.open_tools.is_empty() {
                            None
                        } else {
                            Some(self.open_tools.remove(0))
                        }
                    })
                    .unwrap_or_else(|| "agy-tool".into());
                self.open_tools.retain(|t| t != &id);
                let output = step
                    .get("output")
                    .or_else(|| step.get("result"))
                    .or_else(|| step.get("content"))
                    .map(|o| match o {
                        Value::String(s) => s.clone(),
                        Value::Array(items) => items
                            .iter()
                            .filter_map(|i| {
                                i.as_str().map(str::to_string).or_else(|| {
                                    i.get("text").and_then(Value::as_str).map(str::to_string)
                                })
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| delta.to_string());
                out.push(AgentEvent::ToolCallResult {
                    id,
                    output,
                    is_error: step
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        || s(step, &["status"]).eq_ignore_ascii_case("error"),
                });
            }
            "plan" => {
                let text = if delta.is_empty() {
                    step.get("plan").map(|p| p.to_string()).unwrap_or_default()
                } else {
                    delta.to_string()
                };
                if !text.is_empty() {
                    out.push(AgentEvent::Notice(format!("plan: {text}")));
                }
            }
            "system" | "status" | "notice" => {
                let text = if delta.is_empty() {
                    s(step, &["message", "status"]).to_string()
                } else {
                    delta.to_string()
                };
                if !text.is_empty() {
                    out.push(AgentEvent::Notice(text));
                }
            }
            "error" => out.push(AgentEvent::Error(
                s_opt(step, &["message", "error", "text_delta"])
                    .unwrap_or_else(|| "agy error".into()),
            )),
            _ => {}
        }
    }
}

fn conversation_id(v: &Value) -> Option<String> {
    s_opt(v, &["conversation_id", "conversationId", "session_id"]).filter(|s| !s.is_empty())
}

fn s<'a>(v: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .unwrap_or("")
}

fn s_opt(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::to_string)
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

    impl LineParser for AgyParser {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            AgyParser::feed(self, line)
        }
        fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
            AgyParser::feed_stderr(self, line)
        }
    }

    #[test]
    fn fixture_auth_required_is_a_real_recording() {
        assert_fixture(
            &mut AgyParser::new(None),
            &fixtures_dir(file!()),
            "auth_required",
        );
    }

    #[test]
    fn fixture_synthetic_turn() {
        assert_fixture(
            &mut AgyParser::new(None),
            &fixtures_dir(file!()),
            "synthetic_turn",
        );
    }

    #[test]
    fn tool_results_pair_with_open_calls_without_ids() {
        let mut p = AgyParser::new(None);
        p.feed(r#"{"event":"step_update","step_update":{"step_type":"tool_use","tool_name":"run_command","input":{"command":"ls"}}}"#);
        let ev = p.feed(
            r#"{"event":"step_update","step_update":{"step_type":"tool_output","output":"a\nb"}}"#,
        );
        assert!(
            matches!(&ev[0], AgentEvent::ToolCallResult { id, output, is_error: false } if id == "agy-1" && output == "a\nb")
        );
    }

    #[test]
    fn stderr_markers() {
        let mut p = AgyParser::new(None);
        assert_eq!(
            p.feed_stderr(r#"AGY_ERROR: {"status":"RESOURCE_EXHAUSTED","message":"quota"}"#),
            vec![AgentEvent::Error("quota".into())]
        );
        assert_eq!(
            p.feed_stderr("Error: authentication required. Run 'agy' to log in, then retry."),
            vec![AgentEvent::Error(
                "Error: authentication required. Run 'agy' to log in, then retry.".into()
            )]
        );
        assert_eq!(
            p.feed_stderr("waiting for background tasks"),
            vec![AgentEvent::Notice(
                "agy: waiting for background tasks".into()
            )]
        );
    }
}
