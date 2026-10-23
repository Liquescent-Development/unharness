//! pi `--mode rpc` / `--mode json` event parser.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::gate;

use crate::core::{
    AgentEvent, CapsUpdate, ContextUsage, HarnessCommand, Parser, PermissionKind,
    PermissionRequest, SessionStats, StopReason, Usage, UsageScope,
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
    /// User messages already seen in `get_fork_messages`; `None` until the
    /// first listing of a session, which only sets the baseline.
    fork_messages_seen: Option<usize>,
    /// The session was to be resumed, and has not been listed yet.
    resumed_unlisted: bool,
    /// What the context fill told here is of. pi answers
    /// `get_session_stats` with `tokens: null` after a compaction until a
    /// response it can count follows; then the figure told stands only
    /// if it is of what pi holds now (#355).
    fill: Fill,
}

/// What the context fill a [`PiParser`] told is of (#319, #355).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// Nothing told in this process is of what pi holds now: nothing was
    /// told yet (a resume's figure is the controller's, saved by an
    /// earlier run), or the context changed under it (a fork, a
    /// compaction without an estimate, a response after the estimate
    /// that pi could not count).
    #[default]
    Stale,
    /// A compaction's estimate, and no response since.
    Estimate,
    /// A response's figure, or pi's own.
    Counted,
}

impl PiParser {
    pub fn new(session_id: Option<String>) -> Self {
        PiParser {
            session_id,
            // A new session has no user messages yet.
            fork_messages_seen: Some(0),
            ..Default::default()
        }
    }

    /// The session already has history (it was resumed): the next
    /// `get_fork_messages` listing is a baseline, not a finished turn.
    /// An empty one is a session pi did not have: given a `--session-id`
    /// it has no session for, pi 1.1.0 creates an empty one under that id
    /// (with a warning on stderr), so it is reported as a new session in
    /// place of the one to resume (`SessionReplaced`).
    pub fn expect_fork_baseline(&mut self) {
        self.fork_messages_seen = None;
        self.resumed_unlisted = true;
    }
}

impl Parser for PiParser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return vec![AgentEvent::Notice(line.to_string())];
        };
        self.feed_value(&val)
    }

    fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        let t = line.trim();
        if t.is_empty() {
            return vec![];
        }
        // pi before 0.76.0 refuses `--session-id` this way and exits
        // (`fixtures/too_old.jsonl`), as one of `Unknown options: --a, --b`
        // when there are more (0.73.1's source); another refused flag says
        // nothing about the version.
        if t.contains("Unknown option") && t.split([' ', ',']).any(|word| word == "--session-id") {
            let min = super::MIN_VERSION;
            return vec![AgentEvent::Notice(format!(
                "pi: {t} (unharness needs pi {min} or later: run `{}`)",
                min.upgrade
            ))];
        }
        vec![AgentEvent::Notice(format!("pi: {t}"))]
    }
}

impl PiParser {
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
                        scope: UsageScope::Request,
                    }));
                }
            }
            // What the context holds once a response is in, the way pi
            // counts it (`calculateContextTokens` in 1.1.0's `getContextUsage`),
            // so that the figure moves during a turn; the
            // `get_session_stats` after the turn adds what came after the
            // last response (#355).
            "message_end" => {
                let Some(message) = v.get("message").filter(|m| is_response(m)) else {
                    return out;
                };
                match context_tokens(message) {
                    Some(used) => {
                        self.fill = Fill::Counted;
                        out.push(AgentEvent::Context(ContextUsage {
                            used: Some(used),
                            window: None,
                        }));
                    }
                    // A response that reported no usage (a provider that
                    // streams none): the context grew past the estimate,
                    // and pi, which cannot count it either, answers `null`
                    // from here on.
                    None if self.fill == Fill::Estimate => self.fill = Fill::Stale,
                    None => {}
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
                    out.push(AgentEvent::Error(msg.to_string()));
                } else if v.get("aborted").and_then(Value::as_bool) == Some(true) {
                    // Cancelled: no `result`, and no message to say why.
                    out.push(AgentEvent::Notice("compaction cancelled".into()));
                } else {
                    out.push(AgentEvent::Notice("context compacted".into()));
                    // pi's estimate of the context it rebuilt (1.1.0),
                    // told until a response gives the real figure, as
                    // Claude's `post_tokens` is (#310); `get_session_stats`
                    // has none meanwhile (#355). Without one (a pi that
                    // gives none) what was told before is of the context
                    // as it was.
                    let after = v
                        .pointer("/result/estimatedTokensAfter")
                        .and_then(Value::as_u64)
                        .filter(|&n| n > 0);
                    self.fill = match after {
                        Some(_) => Fill::Estimate,
                        None => Fill::Stale,
                    };
                    out.push(match after {
                        Some(after) => AgentEvent::Context(ContextUsage {
                            used: Some(after),
                            window: None,
                        }),
                        None => AgentEvent::ContextUnknown,
                    });
                }
                // A requested compaction is its own unit of work; automatic
                // ones happen inside a turn that ends by itself.
                if s(v, "reason") == "manual" {
                    out.push(AgentEvent::TurnCompleted {
                        stop_reason: StopReason::Done,
                    });
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
            // message_start, agent_end, queue_update, session_info_changed, bash_execution_update…
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
        let options: Vec<String> = v
            .get("options")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|o| {
                        o.as_str()
                            .map(str::to_string)
                            .or_else(|| o.get("label").and_then(Value::as_str).map(str::to_string))
                            .unwrap_or_else(|| o.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The gate extension asking about a tool call.
        if s(v, "method") == "select"
            && let Some(call) = gate::parse_request(&title, &options)
        {
            out.push(AgentEvent::PermissionRequest(PermissionRequest {
                id,
                kind: PermissionKind::ToolUse {
                    action: gate::tool_action(&call.tool, &call.input),
                    tool: call.tool,
                    input: call.input,
                    description: None,
                },
                tool_call_id: Some(call.tool_call_id),
            }));
            return;
        }
        let kind = match s(v, "method") {
            "confirm" => PermissionKind::Confirm {
                title,
                message: v
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|_m| v.get("title").is_some())
                    .map(str::to_string),
            },
            "select" => PermissionKind::Select { title, options },
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
            if command == "fork" {
                out.pop();
                out.push(AgentEvent::RewindFailed {
                    reason: s(v, "error").to_string(),
                });
            }
            // compaction_end already reported the failure and closed the unit.
            if command == "compact" {
                out.pop();
            }
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
            // The first user message that is new since the last listing is the
            // turn that just ran (steering messages come after it).
            "get_fork_messages" => {
                let messages = v
                    .pointer("/data/messages")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if let Some(seen) = self.fork_messages_seen
                    && let Some(id) = messages
                        .get(seen)
                        .and_then(|m| m.get("entryId"))
                        .and_then(Value::as_str)
                {
                    out.push(AgentEvent::TurnAnchor { id: id.to_string() });
                }
                if std::mem::take(&mut self.resumed_unlisted) && messages.is_empty() {
                    out.push(AgentEvent::SessionReplaced {
                        reason: "could not resume the session (pi has none under its id, and started an empty one)".into(),
                    });
                }
                self.fork_messages_seen = Some(messages.len());
            }
            // The fork is a new session: the listing that follows is its baseline.
            "fork" => {
                // An extension can veto a fork.
                if v.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
                    out.push(AgentEvent::RewindFailed {
                        reason: "the fork was cancelled by a pi extension".into(),
                    });
                } else {
                    self.fork_messages_seen = None;
                    // The fork holds less than the session it left.
                    self.fill = Fill::Stale;
                }
            }
            "get_session_stats" => {
                if let Some(c) = v.pointer("/data/contextUsage") {
                    let used = c.get("tokens").and_then(Value::as_u64);
                    out.push(AgentEvent::Context(ContextUsage {
                        used,
                        window: c.get("contextWindow").and_then(Value::as_u64),
                    }));
                    // `null`: no response pi could count since the last
                    // compaction, so pi does not know. A fill told since
                    // then (the compaction's estimate) stands; one from
                    // before it (a session resumed after a compaction) or
                    // one the context has grown past is no longer of what
                    // pi holds, and is not kept (#319).
                    if used.is_some() {
                        self.fill = Fill::Counted;
                    } else if c.get("tokens").is_some_and(Value::is_null)
                        && self.fill == Fill::Stale
                    {
                        out.push(AgentEvent::ContextUnknown);
                    }
                }
                // Its own count, apart from the per-response usage
                // unharness sums: it covers the whole session file.
                if let Some(d) = v
                    .get("data")
                    .filter(|d| d.get("tokens").is_some_and(Value::is_object))
                {
                    let n = |p: &str| d.pointer(p).and_then(Value::as_u64);
                    out.push(AgentEvent::SessionStats(SessionStats {
                        session_id: d
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        user_messages: n("/userMessages"),
                        assistant_messages: n("/assistantMessages"),
                        tool_calls: n("/toolCalls"),
                        tokens: n("/tokens/total"),
                        cost_usd: d.get("cost").and_then(Value::as_f64),
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
            // Extension commands, prompt templates and skills (`skill:`
            // names); pi's own interface's commands are not among them.
            "get_commands" => {
                if let Some(commands) = v.pointer("/data/commands").and_then(Value::as_array) {
                    out.push(AgentEvent::CapabilitiesChanged(CapsUpdate {
                        commands: Some(
                            commands
                                .iter()
                                .filter_map(|c| {
                                    HarnessCommand::new(
                                        c.get("name")?.as_str()?,
                                        c.get("description").and_then(Value::as_str),
                                        None,
                                    )
                                })
                                .collect(),
                        ),
                        ..Default::default()
                    }));
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

/// An assistant `message` that pi counts the context by: not one that
/// was aborted or failed (1.1.0's `getContextUsage`). Other messages
/// carry no usage.
fn is_response(message: &Value) -> bool {
    s(message, "role") == "assistant" && !matches!(s(message, "stopReason"), "aborted" | "error")
}

/// The tokens a response says the context holds, as pi 1.1.0 reckons
/// them (`calculateContextTokens`): `totalTokens`, or the sum of the four
/// counts where that is missing or zero. One that counted nothing (its
/// provider reported no usage) says nothing, as in pi.
fn context_tokens(message: &Value) -> Option<u64> {
    let usage = message.get("usage")?;
    let tokens = match u(usage, "totalTokens") {
        0 => ["input", "output", "cacheRead", "cacheWrite"]
            .iter()
            .map(|k| u(usage, k))
            .sum(),
        total => total,
    };
    (tokens > 0).then_some(tokens)
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

    /// A count without its tokens is no count; one with some fields
    /// missing keeps the rest, and the session it names.
    #[test]
    fn session_stats_take_what_pi_gave() {
        let stats = |data: &str| {
            PiParser::new(None)
                .feed(&format!(
                    r#"{{"id":"req-4","type":"response","command":"get_session_stats","success":true,"data":{data}}}"#
                ))
                .into_iter()
                .find_map(|e| match e {
                    AgentEvent::SessionStats(s) => Some(s),
                    _ => None,
                })
        };
        assert_eq!(stats(r#"{"sessionId":"s1","userMessages":1}"#), None);
        assert_eq!(stats(r#"{"userMessages":1,"tokens":null}"#), None);
        let s = stats(r#"{"sessionId":"s1","userMessages":1,"tokens":{"input":5},"cost":0.5}"#)
            .unwrap();
        assert_eq!(s.session_id.as_deref(), Some("s1"));
        assert_eq!(
            (s.user_messages, s.tokens, s.cost_usd),
            (Some(1), None, Some(0.5))
        );
        assert!(
            PiParser::new(None)
                .feed(r#"{"id":"req-4","type":"response","command":"get_session_stats","success":false,"error":"no session"}"#)
                .iter()
                .all(|e| !matches!(e, AgentEvent::SessionStats(_)))
        );
    }
    use crate::core::testing::{assert_fixture, fixtures_dir, replay};

    fn told_fill(events: &[AgentEvent]) -> Vec<Option<u64>> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Context(c) => Some(c.used),
                _ => None,
            })
            .collect()
    }

    /// Each response says how full the context is, as pi counts it,
    /// so the figure moves during a turn (#355); one that was aborted,
    /// failed or counted nothing says nothing, as in pi.
    #[test]
    fn each_response_tells_the_fill() {
        let end = |stop: &str, usage: &str| {
            told_fill(&PiParser::new(None).feed(&format!(
                r#"{{"type":"message_end","message":{{"role":"assistant","content":[],"usage":{usage},"stopReason":"{stop}"}}}}"#
            )))
        };
        let usage = r#"{"input":17,"output":2,"cacheRead":1614,"cacheWrite":0,"totalTokens":1633}"#;
        assert_eq!(end("toolUse", usage), [Some(1633)]);
        assert_eq!(end("stop", usage), [Some(1633)]);
        assert_eq!(end("length", usage), [Some(1633)]);
        // Without `totalTokens` (or with it 0), the four counts.
        assert_eq!(
            end(
                "stop",
                r#"{"input":17,"output":2,"cacheRead":1614,"cacheWrite":3,"totalTokens":0}"#
            ),
            [Some(1636)]
        );
        assert_eq!(
            end("stop", r#"{"input":17,"output":2,"cacheRead":1614}"#),
            [Some(1633)]
        );
        assert!(end("aborted", usage).is_empty());
        assert!(end("error", usage).is_empty());
        assert!(end("stop", r#"{"input":0,"output":0,"totalTokens":0}"#).is_empty());
        // A user message or a tool result is no response.
        assert!(
            told_fill(&PiParser::new(None).feed(
                r#"{"type":"message_end","message":{"role":"toolResult","content":[],"usage":{"totalTokens":5}}}"#
            ))
            .is_empty()
        );

        let jsonl =
            std::fs::read_to_string(fixtures_dir(file!()).join("compaction.jsonl")).unwrap();
        let events = replay(&mut PiParser::new(None), &jsonl);
        // The tool call's response, then the answer, before the turn ends.
        let first_end = events
            .iter()
            .position(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
            .unwrap();
        assert_eq!(
            told_fill(&events[..first_end]),
            [None, Some(1613), Some(1633)]
        );
    }

    /// A compaction that succeeds tells pi's estimate of what is left
    /// (whichever started it); one that failed or was cancelled leaves
    /// the figure as it was (#355).
    #[test]
    fn a_compaction_tells_its_estimate() {
        let end = |line: &str| PiParser::new(None).feed(line);
        for reason in ["manual", "threshold", "overflow"] {
            let ev = end(&format!(
                r#"{{"type":"compaction_end","reason":"{reason}","result":{{"summary":"s","firstKeptEntryId":"e1","tokensBefore":252199,"estimatedTokensAfter":32000}},"aborted":false,"willRetry":false}}"#
            ));
            assert_eq!(told_fill(&ev), [Some(32000)], "{reason}");
        }
        let failed = end(
            r#"{"type":"compaction_end","reason":"threshold","aborted":false,"willRetry":false,"errorMessage":"Auto-compaction failed: boom"}"#,
        );
        assert!(told_fill(&failed).is_empty());
        // An estimate of 0 is none (as Claude's `post_tokens`).
        let zero = end(
            r#"{"type":"compaction_end","reason":"manual","result":{"summary":"s","tokensBefore":9000,"estimatedTokensAfter":0},"aborted":false,"willRetry":false}"#,
        );
        assert!(told_fill(&zero).is_empty());
        assert!(zero.contains(&AgentEvent::ContextUnknown));
        let cancelled =
            end(r#"{"type":"compaction_end","reason":"manual","aborted":true,"willRetry":false}"#);
        assert!(told_fill(&cancelled).is_empty());
        assert_eq!(
            cancelled[0],
            AgentEvent::Notice("compaction cancelled".into())
        );
    }

    /// pi answers `tokens: null` after a compaction until a response
    /// follows. The compaction's estimate, told since, stands; a figure
    /// from before it is dropped (#319): a session resumed after a
    /// compaction, a fork, a compaction without an estimate. A count
    /// that is only missing drops nothing.
    #[test]
    fn a_null_fill_drops_only_what_is_from_before_the_compaction() {
        let stats = |tokens: &str| {
            format!(
                r#"{{"id":"req-4","type":"response","command":"get_session_stats","success":true,"data":{{"contextUsage":{{{tokens}"contextWindow":262144,"percent":null}}}}}}"#
            )
        };
        let null = stats(r#""tokens":null,"#);
        let unknown = |events: Vec<AgentEvent>| events.contains(&AgentEvent::ContextUnknown);

        // The recording: the stats asked once the `/compact` ended.
        let jsonl =
            std::fs::read_to_string(fixtures_dir(file!()).join("compaction.jsonl")).unwrap();
        let events = replay(&mut PiParser::new(None), &jsonl);
        assert!(!events.contains(&AgentEvent::ContextUnknown));
        let compacted = events
            .iter()
            .position(|e| *e == AgentEvent::Notice("context compacted".into()))
            .unwrap();
        assert_eq!(
            told_fill(&events[compacted..]),
            [Some(1671), None, Some(1862), Some(1862)]
        );

        // Resumed: nothing told yet in this process.
        let mut resumed = PiParser::new(Some("s".into()));
        resumed.expect_fork_baseline();
        assert!(unknown(resumed.feed(&null)));
        // A count that is not there says nothing of what it would be.
        assert!(!unknown(PiParser::new(None).feed(&stats(""))));

        // A response, then a compaction pi gave no estimate for.
        let mut p = PiParser::new(None);
        p.feed(r#"{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"totalTokens":9000},"stopReason":"stop"}}"#);
        assert!(!unknown(p.feed(&null)));
        let ended = p.feed(r#"{"type":"compaction_end","reason":"threshold","result":{"summary":"s","tokensBefore":9000},"aborted":false,"willRetry":false}"#);
        assert!(unknown(ended));
        assert!(unknown(p.feed(&null)));
        // A response after it is of the context pi holds now.
        p.feed(r#"{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"totalTokens":2000},"stopReason":"stop"}}"#);
        assert!(!unknown(p.feed(&null)));

        // A fork holds less than what was told of the session it left.
        p.feed(r#"{"type":"response","command":"fork","success":true,"data":{}}"#);
        assert!(unknown(p.feed(&null)));
        // pi's own figure is of what it holds.
        p.feed(&stats(r#""tokens":1500,"#));
        assert!(!unknown(p.feed(&null)));

        // A provider that reports no usage: pi counts no response after
        // the compaction, and answers `null` from then on. The estimate
        // stands until a response grows the context past it; an aborted
        // or failed one is not counted by pi either way.
        let estimate = r#"{"type":"compaction_end","reason":"threshold","result":{"summary":"s","tokensBefore":9000,"estimatedTokensAfter":3000},"aborted":false,"willRetry":false}"#;
        let uncounted = |stop: &str| {
            format!(
                r#"{{"type":"message_end","message":{{"role":"assistant","content":[],"usage":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0}},"stopReason":"{stop}"}}}}"#
            )
        };
        let mut p = PiParser::new(None);
        p.feed(estimate);
        assert!(!unknown(p.feed(&null)));
        for stop in ["aborted", "error"] {
            assert!(told_fill(&p.feed(&uncounted(stop))).is_empty());
            assert!(!unknown(p.feed(&null)), "{stop}");
        }
        assert!(told_fill(&p.feed(&uncounted("stop"))).is_empty());
        assert!(unknown(p.feed(&null)));
        assert!(unknown(p.feed(&null)));
        // Without a compaction pi estimates the fill itself.
        let mut p = PiParser::new(None);
        p.feed(&uncounted("stop"));
        assert!(!unknown(p.feed(&stats(r#""tokens":2500,"#))));
    }

    /// pi reports usage at each `turn_end`, once per model call: a turn
    /// that runs a tool reports two (#312).
    #[test]
    fn usage_is_reported_per_request() {
        let jsonl =
            std::fs::read_to_string(fixtures_dir(file!()).join("basic_and_bash.jsonl")).unwrap();
        let scopes: Vec<UsageScope> = replay(&mut PiParser::new(None), &jsonl)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Usage(u) => Some(u.scope),
                _ => None,
            })
            .collect();
        assert_eq!(scopes, [UsageScope::Request; 3]);
    }

    /// Writes the sandbox refused (1.1.0, under a Landlock ruleset like
    /// unharness's).
    #[test]
    fn fixture_sandbox_denied() {
        assert_fixture(
            &mut PiParser::new(None),
            &fixtures_dir(file!()),
            "sandbox_denied",
        );
    }

    #[test]
    fn fixture_gate() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "gate",
        );
    }

    #[test]
    fn fixture_gate_abort() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "gate_abort",
        );
    }

    #[test]
    fn fixture_gate_parallel() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "gate_parallel",
        );
    }

    #[test]
    fn fixture_rewind() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "rewind",
        );
    }

    #[test]
    fn fixture_steer_and_compact() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "steer_and_compact",
        );
    }

    /// A `/compact` that succeeds (pi 1.1.0), between two turns and
    /// before a third.
    #[test]
    fn fixture_compaction() {
        assert_fixture(
            &mut PiParser::new(Some("local-session".into())),
            &fixtures_dir(file!()),
            "compaction",
        );
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

    /// `get_commands` lists a prompt template and a skill, which the next
    /// two prompts ran (`/hello world`, `/skill:greet ami`).
    #[test]
    fn fixture_commands() {
        let dir = fixtures_dir(file!());
        assert_fixture(&mut PiParser::new(None), &dir, "commands");
        let text = std::fs::read_to_string(dir.join("commands.jsonl")).unwrap();
        let names: Vec<String> = crate::core::testing::replay(&mut PiParser::new(None), &text)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::CapabilitiesChanged(u) => u.commands,
                _ => None,
            })
            .flatten()
            .map(|c| format!("{}: {}", c.name, c.description))
            .collect();
        assert!(names.contains(&"hello: Say hello".to_string()), "{names:?}");
        assert!(
            names.contains(
                &"skill:greet: Greets the user in French. Use when asked to greet.".to_string()
            ),
            "{names:?}"
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
    fn only_the_gates_dialog_is_a_tool_request() {
        let mut p = PiParser::new(None);
        let kind = |p: &mut PiParser, line: &str| match p.feed(line).remove(0) {
            AgentEvent::PermissionRequest(r) => (r.kind, r.tool_call_id),
            other => panic!("{other:?}"),
        };
        let title = r#"unharness-gate:{\"toolCallId\":\"c1\",\"toolName\":\"bash\",\"input\":{\"command\":\"ls\"}}"#;
        let (k, id) = kind(
            &mut p,
            &format!(
                r#"{{"type":"extension_ui_request","id":"g1","method":"select","title":"{title}","options":["Allow","Deny"]}}"#
            ),
        );
        assert!(matches!(k, PermissionKind::ToolUse { tool, .. } if tool == "bash"));
        assert_eq!(id.as_deref(), Some("c1"));
        // The same title on another dialog, or with other answers, is a
        // dialog like any other: it is shown, not matched against rules.
        let (k, id) = kind(
            &mut p,
            &format!(
                r#"{{"type":"extension_ui_request","id":"g2","method":"select","title":"{title}","options":["Deny","Allow"]}}"#
            ),
        );
        assert!(matches!(k, PermissionKind::Select { .. }) && id.is_none());
        let (k, _) = kind(
            &mut p,
            &format!(
                r#"{{"type":"extension_ui_request","id":"g3","method":"confirm","title":"{title}"}}"#
            ),
        );
        assert!(matches!(k, PermissionKind::Confirm { .. }));
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

    /// pi 1.1.0 given a `--session-id` it has no session for creates an
    /// empty one under that id: the listing of the session to resume has
    /// no user messages, which is a new session in its place. A listing
    /// with messages, or one that is not the first, is not.
    #[test]
    fn fixture_resume_missing() {
        let dir = fixtures_dir(file!());
        let mut p = PiParser::new(Some("3c0c2f6e-5b1a-4d2e-9f7a-1b2c3d4e5f62".into()));
        p.expect_fork_baseline();
        assert_fixture(&mut p, &dir, "resume_missing");

        let listing = |n: usize| {
            let messages: Vec<Value> = (0..n)
                .map(|i| serde_json::json!({"entryId": format!("e{i}"), "text": "hi"}))
                .collect();
            serde_json::json!({"type": "response", "command": "get_fork_messages", "success": true,
                   "data": {"messages": messages}})
            .to_string()
        };
        let replaced = |evs: &[AgentEvent]| {
            evs.iter()
                .any(|e| matches!(e, AgentEvent::SessionReplaced { .. }))
        };
        let mut resumed = PiParser::new(Some("s".into()));
        resumed.expect_fork_baseline();
        assert!(!replaced(&resumed.feed(&listing(2))));
        let mut new = PiParser::new(Some("s".into()));
        assert!(!replaced(&new.feed(&listing(0))));
        // A rewind's fork of the first prompt lists nothing either.
        let mut rewound = PiParser::new(Some("s".into()));
        rewound.expect_fork_baseline();
        rewound.feed(&listing(1));
        rewound.feed(r#"{"type":"response","command":"fork","success":true,"data":{}}"#);
        assert!(!replaced(&rewound.feed(&listing(0))));
    }

    /// A session saved after a `/compact`, resumed (pi 1.1.0): asked at
    /// once, pi answers `null`, and what was saved before is dropped
    /// until the response (#355).
    #[test]
    fn fixture_resume_compacted() {
        let dir = fixtures_dir(file!());
        let mut p = PiParser::new(Some("401bf0eb-8893-4cae-89d6-77f7c3f2efef".into()));
        p.expect_fork_baseline();
        assert_fixture(&mut p, &dir, "resume_compacted");
    }

    #[test]
    fn fixture_too_old() {
        // pi 0.73.1 refusing `--session-id`.
        assert_fixture(&mut PiParser::new(None), &fixtures_dir(file!()), "too_old");
    }

    #[test]
    fn only_a_refused_session_id_names_the_version() {
        let mut p = PiParser::new(None);
        let hinted = |events: Vec<AgentEvent>| matches!(&events[..], [AgentEvent::Notice(n)] if n.contains("pi update --self"));
        assert!(hinted(p.feed_stderr(
            "Error: Unknown options: --frobnicate, --session-id"
        )));
        assert!(!hinted(
            p.feed_stderr("Error: Unknown option: --frobnicate")
        ));
        assert!(!hinted(
            p.feed_stderr("Error: Unknown option: --session-idx")
        ));
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
