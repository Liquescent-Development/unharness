//! Parser for `codex exec --json` JSONL output (one child per turn).

use serde_json::Value;

use crate::core::{AgentEvent, PlanEntry, PlanStatus, StopReason, Usage};

#[derive(Debug, Default)]
pub struct CodexExecParser {
    turn_started: bool,
}

impl CodexExecParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![AgentEvent::Notice(line.to_string())];
        };
        self.feed_value(&v)
    }

    pub fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        let t = line.trim();
        if t.is_empty() {
            vec![]
        } else {
            vec![AgentEvent::Notice(format!("codex: {t}"))]
        }
    }

    pub fn feed_value(&mut self, v: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match s(v, "type") {
            "thread.started" => out.push(AgentEvent::SessionStarted {
                session_id: s(v, "thread_id").to_string(),
                model: None,
            }),
            "turn.started" => {
                if !self.turn_started {
                    self.turn_started = true;
                    out.push(AgentEvent::TurnStarted);
                }
            }
            "item.started" | "item.updated" | "item.completed" => {
                if let Some(item) = v.get("item") {
                    self.on_item(s(v, "type"), item, &mut out);
                }
            }
            "turn.completed" => {
                if let Some(usage) = v.get("usage") {
                    out.push(AgentEvent::Usage(exec_usage(usage)));
                }
                self.turn_started = false;
                out.push(AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Done,
                });
            }
            "turn.failed" => {
                self.turn_started = false;
                out.push(AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Error(error_message(v)),
                });
            }
            "error" => out.push(AgentEvent::Error(error_message(v))),
            _ => {}
        }
        out
    }

    fn on_item(&mut self, phase: &str, item: &Value, out: &mut Vec<AgentEvent>) {
        let id = s(item, "id").to_string();
        let completed = phase == "item.completed";
        match s(item, "type") {
            "agent_message" if completed => {
                let t = s(item, "text");
                if !t.is_empty() {
                    out.push(AgentEvent::TextDelta(t.to_string()));
                }
            }
            "reasoning" if completed => {
                let t = s(item, "text");
                if !t.is_empty() {
                    out.push(AgentEvent::ThinkingDelta(t.to_string()));
                }
            }
            "command_execution" => {
                if phase == "item.started" {
                    out.push(AgentEvent::ToolCallStarted {
                        id,
                        name: "shell".into(),
                        input: serde_json::json!({"command": s(item, "command")}),
                    });
                } else if completed {
                    let exit = item.get("exit_code").and_then(Value::as_i64);
                    out.push(AgentEvent::ToolCallResult {
                        id,
                        output: s(item, "aggregated_output").to_string(),
                        is_error: exit.is_some_and(|c| c != 0) || s(item, "status") == "failed",
                    });
                }
            }
            "file_change" => {
                if phase == "item.started" {
                    out.push(AgentEvent::ToolCallStarted {
                        id,
                        name: "apply_patch".into(),
                        input: serde_json::json!({"changes": item.get("changes").cloned().unwrap_or(Value::Null)}),
                    });
                } else if completed {
                    let changes = item
                        .get("changes")
                        .and_then(Value::as_array)
                        .map(|c| {
                            c.iter()
                                .map(|ch| format!("{} {}", s(ch, "kind"), s(ch, "path")))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    out.push(AgentEvent::ToolCallResult {
                        id,
                        output: changes,
                        is_error: s(item, "status") == "failed",
                    });
                }
            }
            "mcp_tool_call" => {
                if phase == "item.started" {
                    out.push(AgentEvent::ToolCallStarted {
                        id,
                        name: format!("{}/{}", s(item, "server"), s(item, "tool")),
                        input: item.get("arguments").cloned().unwrap_or(Value::Null),
                    });
                } else if completed {
                    out.push(AgentEvent::ToolCallResult {
                        id,
                        output: item
                            .get("result")
                            .map(|r| r.to_string())
                            .unwrap_or_default(),
                        is_error: item.get("error").is_some_and(|e| !e.is_null())
                            || s(item, "status") == "failed",
                    });
                }
            }
            "web_search" => {
                if phase == "item.started" {
                    out.push(AgentEvent::ToolCallStarted {
                        id,
                        name: "web_search".into(),
                        input: serde_json::json!({"query": s(item, "query")}),
                    });
                } else if completed {
                    out.push(AgentEvent::ToolCallResult {
                        id,
                        output: String::new(),
                        is_error: false,
                    });
                }
            }
            // Sent on start and again on every change, always as the full list.
            "todo_list" | "plan" => {
                if let Some(items) = item.get("items").and_then(Value::as_array) {
                    let entries = items
                        .iter()
                        .map(|i| PlanEntry {
                            text: s(i, "text").to_string(),
                            status: match i.get("completed").and_then(Value::as_bool) {
                                Some(true) => PlanStatus::Completed,
                                Some(false) => PlanStatus::Pending,
                                None => PlanStatus::parse(s(i, "status")),
                            },
                        })
                        .collect();
                    out.push(AgentEvent::PlanUpdated {
                        entries,
                        explanation: None,
                    });
                }
            }
            _ => {}
        }
    }
}

pub fn exec_usage(usage: &Value) -> Usage {
    Usage {
        input: u(usage, "input_tokens"),
        output: u(usage, "output_tokens"),
        cache_read: u(usage, "cached_input_tokens"),
        cache_write: u(usage, "cache_write_input_tokens"),
        cost_usd: None,
        cumulative: false,
    }
}

fn error_message(v: &Value) -> String {
    v.get("error")
        .map(|e| {
            e.as_str()
                .map(str::to_string)
                .or_else(|| e.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| e.to_string())
        })
        .or_else(|| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "codex error".into())
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

    impl LineParser for CodexExecParser {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            CodexExecParser::feed(self, line)
        }
        fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
            CodexExecParser::feed_stderr(self, line)
        }
    }

    #[test]
    fn fixture_exec_basic() {
        assert_fixture(
            &mut CodexExecParser::new(),
            &fixtures_dir(file!()),
            "exec_basic",
        );
    }

    #[test]
    fn fixture_exec_resume_command() {
        assert_fixture(
            &mut CodexExecParser::new(),
            &fixtures_dir(file!()),
            "exec_resume_command",
        );
    }

    #[test]
    fn failed_turn_and_errors() {
        let mut p = CodexExecParser::new();
        assert_eq!(
            p.feed(r#"{"type":"error","message":"rate limited"}"#),
            vec![AgentEvent::Error("rate limited".into())]
        );
        let ev = p.feed(r#"{"type":"turn.failed","error":{"message":"boom"}}"#);
        assert!(
            matches!(&ev[0], AgentEvent::TurnCompleted { stop_reason: StopReason::Error(e) } if e == "boom")
        );
        let ev = p.feed(r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"x","aggregated_output":"bad","exit_code":2,"status":"completed"}}"#);
        assert!(
            matches!(&ev[0], AgentEvent::ToolCallResult { is_error: true, output, .. } if output == "bad")
        );
    }
}
