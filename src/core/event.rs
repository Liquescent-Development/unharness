//! The harness-agnostic event model. Every protocol parser produces these and
//! the TUI consumes only these.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    SessionStarted {
        session_id: String,
        model: Option<String>,
    },
    TurnStarted,
    TextDelta(String),
    ThinkingDelta(String),
    /// Emitted once the tool call's input is complete.
    ToolCallStarted {
        id: String,
        name: String,
        input: Value,
    },
    /// Streaming argument bytes (before `ToolCallStarted`) or live output (after).
    ToolCallDelta {
        id: String,
        name: String,
        delta: String,
    },
    ToolCallResult {
        id: String,
        output: String,
        is_error: bool,
    },
    PermissionRequest(PermissionRequest),
    Usage(Usage),
    /// Effort levels changed (e.g. after a model switch in pi or codex).
    CapabilitiesChanged {
        effort_levels: Vec<String>,
    },
    TurnCompleted {
        stop_reason: StopReason,
    },
    /// Informational line (retries, compaction, degraded policy...).
    Notice(String),
    Error(String),
    ProcessExited {
        code: Option<i32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Done,
    Interrupted,
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    /// Protocol-level request id; echoed back in the decision.
    pub id: String,
    pub kind: PermissionKind,
    /// The tool call this request gates, when known.
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionKind {
    ToolUse {
        tool: String,
        input: Value,
        /// Harness-provided "allow always" rule suggestions, passed back verbatim.
        suggestions: Option<Value>,
        description: Option<String>,
    },
    Question {
        questions: Vec<Question>,
    },
    Confirm {
        title: String,
        message: Option<String>,
    },
    Select {
        title: String,
        options: Vec<String>,
    },
    Input {
        title: String,
        placeholder: Option<String>,
        prefill: Option<String>,
        multiline: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub text: String,
    /// (label, description)
    pub options: Vec<(String, String)>,
    pub allow_other: bool,
    pub multi: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Allow {
        updated_input: Option<Value>,
    },
    AllowAlways,
    Deny {
        reason: String,
    },
    /// Question: `{question_id: [answers]}`; Confirm: bool; Select/Input: string; cancelled: null.
    Answer(Value),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: Option<f64>,
    /// True when the numbers are session totals rather than one turn's.
    pub cumulative: bool,
}

impl Usage {
    /// Accumulate a per-turn usage into a running total.
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        if let Some(c) = other.cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + c);
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

impl AgentEvent {
    /// One-line summary used by fixture tests (`fixtures/<case>.events`).
    pub fn summary(&self) -> String {
        fn short(s: &str) -> String {
            let one_line = s.replace('\n', "\\n");
            if one_line.chars().count() > 60 {
                let cut: String = one_line.chars().take(57).collect();
                format!("{}...", cut)
            } else {
                one_line
            }
        }
        match self {
            AgentEvent::SessionStarted { session_id, model } => format!(
                "SessionStarted id={} model={}",
                session_id,
                model.as_deref().unwrap_or("-")
            ),
            AgentEvent::TurnStarted => "TurnStarted".to_string(),
            AgentEvent::TextDelta(t) => format!("TextDelta {:?}", short(t)),
            AgentEvent::ThinkingDelta(t) => format!("ThinkingDelta {:?}", short(t)),
            AgentEvent::ToolCallStarted { id, name, input } => {
                format!(
                    "ToolCallStarted id={} name={} input={}",
                    id,
                    name,
                    short(&input.to_string())
                )
            }
            AgentEvent::ToolCallDelta { id, name, delta } => {
                format!("ToolCallDelta id={} name={} {:?}", id, name, short(delta))
            }
            AgentEvent::ToolCallResult {
                id,
                output,
                is_error,
            } => format!(
                "ToolCallResult id={} error={} {:?}",
                id,
                is_error,
                short(output)
            ),
            AgentEvent::PermissionRequest(req) => {
                let kind = match &req.kind {
                    PermissionKind::ToolUse { tool, .. } => format!("ToolUse {}", tool),
                    PermissionKind::Question { questions } => {
                        format!("Question n={}", questions.len())
                    }
                    PermissionKind::Confirm { title, .. } => format!("Confirm {:?}", short(title)),
                    PermissionKind::Select { title, options } => {
                        format!("Select {:?} n={}", short(title), options.len())
                    }
                    PermissionKind::Input { title, .. } => format!("Input {:?}", short(title)),
                };
                format!("PermissionRequest id={} {}", req.id, kind)
            }
            AgentEvent::Usage(u) => format!(
                "Usage in={} out={} cache_read={} cache_write={} cost={} cumulative={}",
                u.input,
                u.output,
                u.cache_read,
                u.cache_write,
                u.cost_usd
                    .map(|c| format!("{:.4}", c))
                    .unwrap_or_else(|| "-".into()),
                u.cumulative
            ),
            AgentEvent::CapabilitiesChanged { effort_levels } => {
                format!("CapabilitiesChanged efforts={}", effort_levels.join(","))
            }
            AgentEvent::TurnCompleted { stop_reason } => match stop_reason {
                StopReason::Done => "TurnCompleted Done".to_string(),
                StopReason::Interrupted => "TurnCompleted Interrupted".to_string(),
                StopReason::Error(e) => format!("TurnCompleted Error {:?}", short(e)),
            },
            AgentEvent::Notice(n) => format!("Notice {:?}", short(n)),
            AgentEvent::Error(e) => format!("Error {:?}", short(e)),
            AgentEvent::ProcessExited { code } => {
                format!(
                    "ProcessExited code={}",
                    code.map(|c| c.to_string()).unwrap_or("-".into())
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_single_line_and_bounded() {
        let long = "x".repeat(200);
        let s = AgentEvent::TextDelta(format!("a\nb{}", long)).summary();
        assert!(!s.contains('\n'));
        assert!(s.len() < 90);
        assert!(s.starts_with("TextDelta \"a\\\\nb"));
    }

    #[test]
    fn usage_total() {
        let u = Usage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            ..Default::default()
        };
        assert_eq!(u.total_tokens(), 10);
    }
}
