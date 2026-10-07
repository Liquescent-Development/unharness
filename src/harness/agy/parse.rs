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
//!
//! Headless agy refuses a call it would have asked about and ends the turn
//! there, with `SUCCESS` and an empty response. Each refusal is one stderr
//! line (`jetski: no output produced — a tool required the "command"
//! permission …`); `result.denied_actions` only names the kinds refused so
//! far in the process, so it is the fallback when the line has not come.
//!
//! Subagents (agy 1.3.1): an `invoke_subagent` call is a step that opens as
//! `tool` or as `subagent` and closes as `subagent`, its `DONE` update
//! listing each subagent (`subagent_info.subagents[]`: `conversation_id`,
//! `role`, `type_name`, `initial_prompt`). Nothing on the stream ends one: a
//! report is a `system_message` step with no content and no sender, and the
//! transport reads it from agy's files (`brain.rs`, `feed_report`). The
//! turn's `result` waited for the subagents in every recording (a turn whose
//! agent said it was done went on when a report came in), so one still
//! open then has ended without a report (`feed_last_step`, then `result`).

use serde_json::Value;

use crate::core::{AgentEvent, StopReason, SubagentStatus, Usage};

#[derive(Debug, Default)]
pub struct AgyParser {
    session_id: Option<String>,
    turn_started: bool,
    /// Summed from the steps of the open turn.
    turn_usage: Option<Usage>,
    /// Indexes of the tool steps announced and not finished.
    open_tools: Vec<u64>,
    /// A refusal was reported for the open turn.
    refusal_noted: bool,
    /// Subagents started and not ended, by conversation id.
    open_subagents: Vec<String>,
    /// Every subagent started in this process.
    subagents: Vec<String>,
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

    /// The conversation agy reported, which owns the subagents' reports.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Subagents started and not ended.
    pub fn open_subagents(&self) -> &[String] {
        &self.open_subagents
    }

    /// A message agy filed for the conversation (`brain::new_messages`).
    /// One from a subagent of this process is its report, and ends it; a
    /// later one is a report after its end.
    pub fn feed_report(&mut self, message: &Value) -> Vec<AgentEvent> {
        let sender = str_of(message, "sender");
        if !self.subagents.iter().any(|s| s == sender) {
            return vec![];
        }
        self.open_subagents.retain(|s| s != sender);
        vec![AgentEvent::SubagentEnded {
            id: sender.to_string(),
            status: SubagentStatus::Completed,
            result: Some(str_of(message, "content").to_string()),
        }]
    }

    /// The last step in the transcript of `id`, a subagent that has not
    /// reported, read when the turn ends: a refused or failed call ends it
    /// as failed, a response with no call as completed with that response.
    pub fn feed_last_step(&mut self, id: &str, step: &Value) -> Vec<AgentEvent> {
        if !self.open_subagents.iter().any(|s| s == id) {
            return vec![];
        }
        let (status, result) = if str_of(step, "status") == "ERROR" {
            (SubagentStatus::Failed, str_of(step, "error"))
        } else if str_of(step, "type") == "PLANNER_RESPONSE"
            && str_of(step, "status") == "DONE"
            && step
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_none_or(|c| c.is_empty())
        {
            (SubagentStatus::Completed, str_of(step, "content"))
        } else {
            return vec![];
        };
        self.open_subagents.retain(|s| s != id);
        vec![AgentEvent::SubagentEnded {
            id: id.to_string(),
            status,
            result: (!result.is_empty()).then(|| result.to_string()),
        }]
    }

    pub fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        let t = line.trim();
        if t.is_empty() || t.starts_with("Debugger") {
            return vec![];
        }
        if let Some(rest) = t.strip_prefix("jetski: no output produced") {
            if self.refusal_noted {
                return vec![];
            }
            self.refusal_noted = true;
            let permission = rest
                .split_once("required the \"")
                .and_then(|(_, r)| r.split_once('"'))
                .map(|(p, _)| p)
                .unwrap_or("a");
            return vec![AgentEvent::Notice(format!(
                "agy refused a call that needs {permission} permission, which it cannot ask for in this mode; the turn ended there"
            ))];
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
                let error = str_of(r, "error");
                let denied: Vec<&str> = r
                    .get("denied_actions")
                    .and_then(Value::as_array)
                    .map(|d| d.iter().map(|d| str_of(d, "display_name")).collect())
                    .unwrap_or_default();
                if !self.refusal_noted
                    && !denied.is_empty()
                    && error.is_empty()
                    && str_of(r, "response").is_empty()
                {
                    self.refusal_noted = true;
                    out.push(AgentEvent::Notice(format!(
                        "agy refused a call it cannot ask about in this mode (so far: {}); the turn ended there",
                        denied.join(", ")
                    )));
                }
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
                for id in std::mem::take(&mut self.open_subagents) {
                    out.push(AgentEvent::SubagentEnded {
                        id,
                        status: SubagentStatus::Completed,
                        result: None,
                    });
                }
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
            self.refusal_noted = false;
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
            step_type @ ("tool" | "subagent") => {
                let Some(index) = step.get("step_index").and_then(Value::as_u64) else {
                    return;
                };
                let id = format!("step-{index}");
                let info = step.get("tool_info");
                let subagents = step
                    .pointer("/subagent_info/subagents")
                    .and_then(Value::as_array);
                if !self.open_tools.contains(&index) {
                    self.open_tools.push(index);
                    let input = match (info.and_then(|i| i.get("parameters")), subagents) {
                        (Some(p), _) => p.clone(),
                        (None, Some(s)) => serde_json::json!({ "Subagents": s }),
                        (None, None) => Value::Null,
                    };
                    out.push(AgentEvent::ToolCallStarted {
                        id: id.clone(),
                        name: str_of(step, "tool_name").to_string(),
                        input,
                    });
                }
                if str_of(step, "state") == "DONE" {
                    self.open_tools.retain(|i| *i != index);
                    let output = if step_type == "subagent" {
                        subagents
                            .into_iter()
                            .flatten()
                            .map(|s| {
                                format!("{}: {}", str_of(s, "role"), str_of(s, "initial_prompt"))
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        info.map(|i| str_of(i, "output")).unwrap_or("").to_string()
                    };
                    // A command that failed and a call agy refused look the
                    // same here: no status, at most an output.
                    out.push(AgentEvent::ToolCallResult {
                        id,
                        output,
                        is_error: false,
                    });
                    for s in subagents.into_iter().flatten() {
                        self.subagent_started(s, out);
                    }
                }
            }
            _ => {}
        }
    }
}

impl AgyParser {
    fn subagent_started(&mut self, s: &Value, out: &mut Vec<AgentEvent>) {
        let id = str_of(s, "conversation_id");
        if id.is_empty() {
            return;
        }
        let role = str_of(s, "role");
        let kind = str_of(s, "type_name");
        if !self.subagents.iter().any(|k| k == id) {
            self.subagents.push(id.to_string());
        }
        if !self.open_subagents.iter().any(|k| k == id) {
            self.open_subagents.push(id.to_string());
        }
        out.push(AgentEvent::SubagentStarted {
            id: id.to_string(),
            description: if role.is_empty() {
                str_of(s, "initial_prompt").to_string()
            } else {
                role.to_string()
            },
            kind: (!kind.is_empty()).then(|| kind.to_string()),
        });
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
    fn fixture_auth_required() {
        assert_fixture(
            &mut AgyParser::new(None),
            &fixtures_dir(file!()),
            "auth_required",
        );
    }

    #[test]
    fn fixtures_from_a_signed_in_account() {
        for case in [
            "turn",
            "bypass",
            "resume",
            "default",
            "denied_twice",
            "bad_model",
            "subagent_refused",
            "subagent_wait",
            "subagent_followup",
        ] {
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
    fn a_refusal_is_reported_once_per_turn() {
        let line = "jetski: no output produced \u{2014} a tool required the \"command\" permission that headless mode cannot prompt for, so it was auto-denied.";
        let result = r#"{"event":"result","result":{"conversation_id":"c","status":"SUCCESS","response":"","num_turns":1,"usage":{},"denied_actions":[{"action":"command","display_name":"RunCommand"}]}}"#;
        // The line first, then the result's fallback stays quiet.
        let mut p = AgyParser::new(None);
        let ev = p.feed_stderr(line);
        assert!(matches!(&ev[0], AgentEvent::Notice(n) if n.contains("needs command permission")));
        assert!(p.feed_stderr(line).is_empty());
        let ev = p.feed(result);
        assert!(!ev.iter().any(|e| matches!(e, AgentEvent::Notice(_))));
        // The result first, then the line that arrives after it.
        let mut p = AgyParser::new(None);
        let ev = p.feed(result);
        assert!(
            ev.iter()
                .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("RunCommand")))
        );
        assert!(p.feed_stderr(line).is_empty());
        // A new turn reports again.
        p.feed(r#"{"event":"step_update","step_update":{"conversation_id":"c","step_index":3,"state":"DONE","step_type":"user_input"}}"#);
        assert_eq!(p.feed_stderr(line).len(), 1);
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
