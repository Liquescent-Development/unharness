//! Antigravity (`agy`) `--output-format stream-json` parser, from recordings
//! of agy 1.2.17 (`fixtures/`).
//!
//! `init` arrives once per process, after the first message. A turn is a run
//! of `step_update`s closed by a `result`. A step has an index, which is all
//! that ties a tool's `ACTIVE` update to its `DONE` one, and a type:
//! `user_input`, `agent_response` (text deltas; only the last one of a turn
//! carries text), `tool`, `system_message`. Thinking is counted in the usage
//! and never sent.
//!
//! `result.usage` and `num_turns` are totals over the conversation, also
//! across processes, so a turn's usage is the sum of its steps'.
//! `denied_actions` repeats what earlier turns of the process were refused.

use serde_json::Value;

use crate::core::{AgentEvent, StopReason, Usage};

#[derive(Debug, Default)]
pub struct AgyParser {
    session_id: Option<String>,
    turn_started: bool,
    /// Summed from the steps of the open turn.
    turn_usage: Option<Usage>,
    /// Indexes of the tool steps announced and not finished.
    open_tools: Vec<u64>,
    /// How many of `denied_actions` earlier results already listed.
    denied_seen: usize,
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
        // A refused tool is reported from the result's `denied_actions`.
        if t.is_empty() || t.starts_with("Debugger") || t.starts_with("jetski: no output produced")
        {
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
        match str_of(v, "event") {
            "init" => {
                self.session(v, &mut out);
                self.open_turn(&mut out);
            }
            "step_update" => {
                let Some(step) = v.get("step_update") else {
                    return out;
                };
                self.session(step, &mut out);
                self.open_turn(&mut out);
                self.on_step(step, &mut out);
            }
            "result" => {
                let Some(r) = v.get("result") else {
                    return out;
                };
                self.session(r, &mut out);
                if let Some(usage) = self.turn_usage.take() {
                    out.push(AgentEvent::Usage(usage));
                }
                let denied = r
                    .get("denied_actions")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if denied.len() > self.denied_seen {
                    let names: Vec<&str> = denied[self.denied_seen..]
                        .iter()
                        .map(|d| str_of(d, "display_name"))
                        .collect();
                    out.push(AgentEvent::Notice(format!(
                        "agy refused what it could not ask about: {}",
                        names.join(", ")
                    )));
                }
                self.denied_seen = denied.len();
                let error = str_of(r, "error");
                let stop_reason = if error == "interrupted" {
                    StopReason::Interrupted
                } else if !error.is_empty() {
                    StopReason::Error(error.to_string())
                } else if str_of(r, "status") != "SUCCESS" {
                    StopReason::Error(format!("agy status {}", str_of(r, "status")))
                } else {
                    StopReason::Done
                };
                self.turn_started = false;
                self.open_tools.clear();
                out.push(AgentEvent::TurnCompleted { stop_reason });
            }
            _ => {}
        }
        out
    }

    /// The id is empty on a result that failed before a conversation existed.
    fn session(&mut self, v: &Value, out: &mut Vec<AgentEvent>) {
        let id = str_of(v, "conversation_id");
        if !id.is_empty() && self.session_id.as_deref() != Some(id) {
            self.session_id = Some(id.to_string());
            out.push(AgentEvent::SessionStarted {
                session_id: id.to_string(),
                model: None,
            });
        }
    }

    fn open_turn(&mut self, out: &mut Vec<AgentEvent>) {
        if !self.turn_started {
            self.turn_started = true;
            out.push(AgentEvent::TurnStarted);
        }
    }

    fn on_step(&mut self, step: &Value, out: &mut Vec<AgentEvent>) {
        if let Some(usage) = step.get("usage") {
            self.turn_usage
                .get_or_insert_with(Usage::default)
                .add(&Usage {
                    input: u(usage, "input_tokens"),
                    output: u(usage, "output_tokens"),
                    cache_read: u(usage, "cache_read_tokens"),
                    cache_write: 0,
                    cost_usd: None,
                    cumulative: false,
                });
        }
        match str_of(step, "step_type") {
            "agent_response" => {
                let delta = str_of(step, "text_delta");
                if !delta.is_empty() {
                    out.push(AgentEvent::TextDelta(delta.to_string()));
                }
            }
            "tool" => {
                let Some(index) = step.get("step_index").and_then(Value::as_u64) else {
                    return;
                };
                let id = format!("step-{index}");
                let info = step.get("tool_info");
                if !self.open_tools.contains(&index) {
                    self.open_tools.push(index);
                    out.push(AgentEvent::ToolCallStarted {
                        id: id.clone(),
                        name: str_of(step, "tool_name").to_string(),
                        input: info
                            .and_then(|i| i.get("parameters"))
                            .cloned()
                            .unwrap_or(Value::Null),
                    });
                }
                if str_of(step, "state") == "DONE" {
                    self.open_tools.retain(|i| *i != index);
                    // A command that failed and a call agy refused look the
                    // same here: no status, at most an output.
                    out.push(AgentEvent::ToolCallResult {
                        id,
                        output: info.map(|i| str_of(i, "output")).unwrap_or("").to_string(),
                        is_error: false,
                    });
                }
            }
            _ => {}
        }
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
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
    fn fixtures_from_a_signed_in_account() {
        for case in ["turn", "bypass", "resume", "default", "bad_model"] {
            assert_fixture(&mut AgyParser::new(None), &fixtures_dir(file!()), case);
        }
    }

    #[test]
    fn a_tool_step_without_its_start_still_pairs() {
        let mut p = AgyParser::new(None);
        let ev = p.feed(
            r#"{"event":"step_update","step_update":{"conversation_id":"c","step_index":4,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"ls"},"output":"a\r\n"}}}"#,
        );
        assert!(
            matches!(&ev[2], AgentEvent::ToolCallStarted { id, name, .. } if id == "step-4" && name == "run_command")
        );
        assert!(
            matches!(&ev[3], AgentEvent::ToolCallResult { id, output, is_error: false } if id == "step-4" && output == "a\r\n")
        );
    }

    #[test]
    fn an_interrupt_is_not_an_error() {
        // What agy printed when sent SIGINT during a command (1.2.17).
        let mut p = AgyParser::new(Some("c".into()));
        let ev = p.feed(
            r#"{"event":"result","result":{"conversation_id":"c","status":"ERROR","response":"","error":"interrupted","duration_seconds":2.4,"num_turns":1,"usage":{"input_tokens":11825,"output_tokens":566,"thinking_tokens":448,"cache_read_tokens":0,"total_tokens":12391}}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::TurnCompleted {
                stop_reason: StopReason::Interrupted
            }]
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
        assert!(p.feed_stderr("jetski: no output produced \u{2014} a tool required the \"command\" permission that headless mode cannot prompt for, so it was auto-denied.").is_empty());
        assert_eq!(
            p.feed_stderr("waiting for background tasks"),
            vec![AgentEvent::Notice(
                "agy: waiting for background tasks".into()
            )]
        );
    }
}
