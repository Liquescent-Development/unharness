//! Parser for `codex app-server` notifications and server requests.
//!
//! Responses to our own requests are handled by the transport driver (it
//! owns the request ids); this parser ignores them.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::core::jsonrpc::RpcMessage;
use crate::core::{
    AgentEvent, ContextUsage, PermissionKind, PermissionRequest, PlanEntry, PlanStatus, Question,
    RateLimitInfo, RateLimitWindow, StopReason, Usage,
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
}

impl CodexAppServerParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        match RpcMessage::parse(line) {
            Some(RpcMessage::Notification { method, params }) => {
                self.on_notification(&method, &params)
            }
            Some(RpcMessage::Request { id, method, params }) => {
                self.on_server_request(&id, &method, &params)
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
                    self.on_item_started(item, &mut out);
                }
            }
            "item/completed" => {
                if let Some(item) = p.get("item") {
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
                let msg = p
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex warning");
                out.push(AgentEvent::Notice(msg.to_string()));
            }
            "autoApprovalReview/strictReviewRequired" => out.push(AgentEvent::Notice(
                "codex auto-review requires strict review".into(),
            )),
            // thread/status/changed, mcpServer/startupStatus/updated, account/updated,
            // remoteControl/*, serverRequest/resolved, item/updated…
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
                name: format!(
                    "{}/{}",
                    s(item.get("server").unwrap_or(&Value::Null)),
                    s(item.get("tool").unwrap_or(&Value::Null))
                ),
                input: item.get("arguments").cloned().unwrap_or(Value::Null),
            }),
            "dynamicToolCall" => out.push(AgentEvent::ToolCallStarted {
                id,
                name: s(item.get("tool").unwrap_or(&Value::Null)).to_string(),
                input: item.get("arguments").cloned().unwrap_or(Value::Null),
            }),
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
        let item_id = p.get("itemId").and_then(Value::as_str).map(str::to_string);
        let kind = match method {
            "item/commandExecution/requestApproval" => PermissionKind::ToolUse {
                tool: "shell".into(),
                input: json!({
                    "command": p.get("command").cloned().unwrap_or(Value::Null),
                    "cwd": p.get("cwd").cloned().unwrap_or(Value::Null),
                    "reason": p.get("reason").cloned().unwrap_or(Value::Null),
                }),
                suggestions: p.get("availableDecisions").cloned(),
                description: p.get("reason").and_then(Value::as_str).map(str::to_string),
            },
            "item/fileChange/requestApproval" => PermissionKind::ToolUse {
                tool: "apply_patch".into(),
                input: json!({
                    "reason": p.get("reason").cloned().unwrap_or(Value::Null),
                    "grantRoot": p.get("grantRoot").cloned().unwrap_or(Value::Null),
                    "changes": p.get("changes").cloned().unwrap_or(Value::Null),
                }),
                suggestions: p.get("availableDecisions").cloned(),
                description: p.get("reason").and_then(Value::as_str).map(str::to_string),
            },
            "item/permissions/requestApproval" => PermissionKind::ToolUse {
                tool: "permissions".into(),
                input: p.clone(),
                suggestions: p.get("availableDecisions").cloned(),
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
                                                (
                                                    s(o.get("label").unwrap_or(&Value::Null))
                                                        .to_string(),
                                                    s(o.get("description").unwrap_or(&Value::Null))
                                                        .to_string(),
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
    fn fixture_app_server_two_turns() {
        assert_fixture(
            &mut CodexAppServerParser::new(),
            &fixtures_dir(file!()),
            "app_server_two_turns",
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
