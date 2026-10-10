//! The harness-agnostic event model. Every protocol parser produces these and
//! the TUI consumes only these.

use serde_json::Value;

use super::caps::PermissionPolicy;
use super::ids::{ModelInfo, ProviderId};
use super::rules::ToolAction;

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
    /// The harness no longer waits for the answer to request `id` (its turn
    /// was stopped): the question is closed unanswered.
    PermissionWithdrawn {
        id: String,
    },
    Usage(Usage),
    /// The agent's plan / todo list. Each event replaces the previous plan.
    PlanUpdated {
        entries: Vec<PlanEntry>,
        explanation: Option<String>,
    },
    /// An event produced inside a subagent; `parent` is the tool call that
    /// spawned it, which is also the subagent's id in the `Subagent*` events.
    Sub {
        parent: String,
        event: Box<AgentEvent>,
    },
    /// A subagent began work. Its life is its own: the tool call that
    /// spawned it may return long before it ends, and the turn may end too.
    /// Sent again for the same `id` when an ended subagent is put back to work.
    SubagentStarted {
        /// The tool call that spawned it (the `parent` of its `Sub` events).
        id: String,
        /// The task as the harness words it; a bare name where that is all there is.
        description: String,
        /// The kind of agent (`Explore`, `general-purpose`...), where the harness has kinds.
        kind: Option<String>,
    },
    /// What a running subagent is doing now, in the harness's words.
    SubagentProgress {
        id: String,
        activity: String,
    },
    SubagentEnded {
        id: String,
        status: SubagentStatus,
        /// Its final report, when the harness hands one over. One that
        /// comes after the end is sent in a second `SubagentEnded`.
        result: Option<String>,
    },
    /// A hook the harness runs on its own events (before a tool call, when
    /// the turn would stop...) began. `name` is the harness's, e.g.
    /// `PreToolUse:Bash`.
    HookStarted {
        id: String,
        name: String,
    },
    HookEnded {
        id: String,
        name: String,
        outcome: HookOutcome,
        /// What it said: the reason it gave for a block, its output otherwise.
        output: String,
    },
    Context(ContextUsage),
    RateLimit(RateLimitInfo),
    /// Vendor id of the user turn the harness just accepted (rewind / fork target).
    TurnAnchor {
        id: String,
    },
    /// A requested rewind did not happen: the session still holds the turns
    /// the user removed.
    RewindFailed {
        reason: String,
    },
    /// What the harness can do changed (model switch, handshake result...).
    CapabilitiesChanged(CapsUpdate),
    /// The session now runs under this policy: one the user set, or one the
    /// agent moved to by itself (Claude's `EnterPlanMode`).
    PolicyChanged(PermissionPolicy),
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

/// How a subagent ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Completed,
    Failed,
    /// Stopped before it finished, by the user or by the agent that spawned it.
    Cancelled,
}

impl SubagentStatus {
    pub fn label(&self) -> &'static str {
        match self {
            SubagentStatus::Completed => "completed",
            SubagentStatus::Failed => "failed",
            SubagentStatus::Cancelled => "stopped",
        }
    }
}

/// How a hook ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOutcome {
    Succeeded,
    /// It failed without stopping anything.
    Failed,
    /// It stopped what it ran before (a tool call, a prompt, the turn's end).
    Blocked,
}

impl HookOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            HookOutcome::Succeeded => "ok",
            HookOutcome::Failed => "failed",
            HookOutcome::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    /// Vendors spell these `inProgress`, `in_progress`, `in-progress`, `done`...
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().replace(['_', '-'], "").as_str() {
            "inprogress" | "active" | "running" => PlanStatus::InProgress,
            "completed" | "complete" | "done" => PlanStatus::Completed,
            _ => PlanStatus::Pending,
        }
    }

    pub fn marker(&self) -> &'static str {
        match self {
            PlanStatus::Pending => "[ ]",
            PlanStatus::InProgress => "[~]",
            PlanStatus::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanEntry {
    pub text: String,
    pub status: PlanStatus,
}

/// How full the model's context window is. Either side may be unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextUsage {
    pub used: Option<u64>,
    pub window: Option<u64>,
}

impl ContextUsage {
    /// Keep the known halves of `self`, take the rest from `newer`.
    pub fn merge(&mut self, newer: ContextUsage) {
        if newer.used.is_some() {
            self.used = newer.used;
        }
        if newer.window.is_some() {
            self.window = newer.window;
        }
    }

    pub fn percent(&self) -> Option<u8> {
        match (self.used, self.window) {
            (Some(u), Some(w)) if w > 0 => Some((u.saturating_mul(100) / w).min(100) as u8),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitWindow {
    /// e.g. `five_hour`, `seven_day`, `primary`.
    pub label: String,
    pub used_percent: Option<f32>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RateLimitInfo {
    /// Vendor status string when one is reported (`allowed`, `rejected`...).
    pub status: Option<String>,
    pub windows: Vec<RateLimitWindow>,
}

/// A partial capability update; `None` leaves the declared value alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapsUpdate {
    pub effort_levels: Option<Vec<String>>,
    pub image_input: Option<bool>,
    pub resume_by_id: Option<bool>,
    /// The agent takes MCP servers reached over HTTP (`McpSupport::http`).
    pub mcp_http: Option<bool>,
    pub plan_mode: Option<bool>,
    /// Models the live session offers (harnesses that only know after a handshake).
    pub models: Option<Vec<ModelInfo>>,
    /// The provider the live session runs on, as the harness reports it.
    pub provider: Option<ProviderId>,
    /// The commands of its own the live session takes as `/name` at the
    /// start of a prompt; each list replaces the one before.
    pub commands: Option<Vec<HarnessCommand>>,
    /// Whether a prompt that starts with `/name` runs one
    /// (`Capabilities::slash_commands`).
    pub slash_commands: Option<bool>,
}

/// A command a harness runs when a prompt starts with `/name`: one of its
/// own, a custom one or a skill, as its session lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessCommand {
    /// Without the `/`.
    pub name: String,
    pub description: String,
    /// What it takes after its name, when the harness says.
    pub hint: Option<String>,
    /// Other names it runs under, without the `/`.
    pub aliases: Vec<String>,
}

impl HarnessCommand {
    /// `None` for a name that is empty or has a space in it: no prompt
    /// could name it. An empty hint is none.
    pub fn new(name: &str, description: Option<&str>, hint: Option<&str>) -> Option<Self> {
        let name = name.trim().trim_start_matches('/');
        if name.is_empty() || name.contains(char::is_whitespace) {
            return None;
        }
        let hint = hint.map(str::trim).filter(|h| !h.is_empty());
        Some(HarnessCommand {
            name: name.to_string(),
            description: description.unwrap_or_default().trim().to_string(),
            hint: hint.map(str::to_string),
            aliases: Vec::new(),
        })
    }

    /// Whether `/name` runs this command.
    pub fn answers_to(&self, name: &str) -> bool {
        name.strip_prefix('/')
            .is_some_and(|n| n == self.name || self.aliases.iter().any(|a| a == n))
    }
}

impl CapsUpdate {
    pub fn efforts(levels: Vec<String>) -> Self {
        CapsUpdate {
            effort_levels: Some(levels),
            ..Default::default()
        }
    }

    /// Layer `newer` over `self`.
    pub fn merge(&mut self, newer: CapsUpdate) {
        if newer.effort_levels.is_some() {
            self.effort_levels = newer.effort_levels;
        }
        if newer.image_input.is_some() {
            self.image_input = newer.image_input;
        }
        if newer.resume_by_id.is_some() {
            self.resume_by_id = newer.resume_by_id;
        }
        if newer.mcp_http.is_some() {
            self.mcp_http = newer.mcp_http;
        }
        if newer.plan_mode.is_some() {
            self.plan_mode = newer.plan_mode;
        }
        if newer.models.is_some() {
            self.models = newer.models;
        }
        if newer.provider.is_some() {
            self.provider = newer.provider;
        }
        if newer.commands.is_some() {
            self.commands = newer.commands;
        }
        if newer.slash_commands.is_some() {
            self.slash_commands = newer.slash_commands;
        }
    }
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
        /// What the call does, in terms rules can match on every harness.
        action: ToolAction,
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
    /// The agent asks to leave plan mode and carry out `plan` (markdown).
    /// Approved with `Allow`, after the policy to act under is set;
    /// anything else keeps it planning. No rule answers it.
    PlanApproval {
        plan: String,
        /// Where the agent keeps the plan, when it says.
        plan_file: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub text: String,
    pub options: Vec<QuestionOption>,
    pub allow_other: bool,
    pub multi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuestionOption {
    pub label: String,
    /// What choosing it means.
    pub description: String,
    /// The text being chosen between (a draft, a snippet), as markdown.
    pub preview: Option<String>,
}

impl QuestionOption {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        QuestionOption {
            label: label.into(),
            description: description.into(),
            preview: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Allow {
        updated_input: Option<Value>,
    },
    Deny {
        reason: String,
    },
    /// Question: `{question_id: [answers]}`; Confirm: bool; Select/Input: string; cancelled: null.
    Answer(Value),
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
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
                    PermissionKind::ToolUse { tool, action, .. } => {
                        format!("ToolUse {} [{}]", tool, short(&action.summary()))
                    }
                    PermissionKind::Question { questions } => {
                        // Per question: header, `multi`, labels (`+preview`
                        // marks an option that carries one).
                        let each: Vec<String> = questions
                            .iter()
                            .map(|q| {
                                let labels: Vec<String> = q
                                    .options
                                    .iter()
                                    .map(|o| {
                                        let mark =
                                            if o.preview.is_some() { "+preview" } else { "" };
                                        format!("{}{mark}", o.label)
                                    })
                                    .collect();
                                format!(
                                    " [{}{}: {}]",
                                    q.header,
                                    if q.multi { " multi" } else { "" },
                                    short(&labels.join("|"))
                                )
                            })
                            .collect();
                        format!("Question n={}{}", questions.len(), each.concat())
                    }
                    PermissionKind::Confirm { title, .. } => format!("Confirm {:?}", short(title)),
                    PermissionKind::Select { title, options } => {
                        format!("Select {:?} n={}", short(title), options.len())
                    }
                    PermissionKind::Input { title, .. } => format!("Input {:?}", short(title)),
                    PermissionKind::PlanApproval { plan, plan_file } => format!(
                        "PlanApproval {:?} file={}",
                        short(plan),
                        plan_file.as_deref().unwrap_or("-")
                    ),
                };
                format!("PermissionRequest id={} {}", req.id, kind)
            }
            AgentEvent::PermissionWithdrawn { id } => format!("PermissionWithdrawn id={id}"),
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
            AgentEvent::PlanUpdated { entries, .. } => format!(
                "PlanUpdated {}",
                short(
                    &entries
                        .iter()
                        .map(|e| format!("{} {}", e.status.marker(), e.text))
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            ),
            AgentEvent::Sub { parent, event } => {
                format!("Sub parent={} {}", parent, event.summary())
            }
            AgentEvent::SubagentStarted {
                id,
                description,
                kind,
            } => format!(
                "SubagentStarted id={} kind={} {:?}",
                id,
                kind.as_deref().unwrap_or("-"),
                short(description)
            ),
            AgentEvent::SubagentProgress { id, activity } => {
                format!("SubagentProgress id={} {:?}", id, short(activity))
            }
            AgentEvent::SubagentEnded { id, status, result } => format!(
                "SubagentEnded id={} {} {}",
                id,
                status.label(),
                result
                    .as_deref()
                    .map(|r| format!("{:?}", short(r)))
                    .unwrap_or_else(|| "-".into())
            ),
            AgentEvent::HookStarted { id, name } => format!("HookStarted id={id} {name}"),
            AgentEvent::HookEnded {
                id,
                name,
                outcome,
                output,
            } => format!(
                "HookEnded id={id} {name} {} {:?}",
                outcome.label(),
                short(output)
            ),
            AgentEvent::Context(c) => format!(
                "Context used={} window={}",
                c.used.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                c.window
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into())
            ),
            AgentEvent::RateLimit(r) => format!(
                "RateLimit status={} {}",
                r.status.as_deref().unwrap_or("-"),
                r.windows
                    .iter()
                    .map(|w| format!(
                        "{}={}",
                        w.label,
                        w.used_percent
                            .map(|p| format!("{p:.0}%"))
                            .unwrap_or_else(|| "?".into())
                    ))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            AgentEvent::TurnAnchor { id } => format!("TurnAnchor id={id}"),
            AgentEvent::RewindFailed { reason } => format!("RewindFailed {:?}", short(reason)),
            AgentEvent::CapabilitiesChanged(u) => {
                let mut parts = Vec::new();
                if let Some(e) = &u.effort_levels {
                    parts.push(format!("efforts={}", e.join(",")));
                }
                if let Some(i) = u.image_input {
                    parts.push(format!("image_input={i}"));
                }
                if let Some(r) = u.resume_by_id {
                    parts.push(format!("resume_by_id={r}"));
                }
                if let Some(h) = u.mcp_http {
                    parts.push(format!("mcp_http={h}"));
                }
                if let Some(p) = u.plan_mode {
                    parts.push(format!("plan_mode={p}"));
                }
                if let Some(m) = &u.models {
                    parts.push(format!("models={}", m.len()));
                }
                if let Some(p) = &u.provider {
                    parts.push(format!("provider={p}"));
                }
                if let Some(c) = &u.commands {
                    parts.push(format!("commands={}", c.len()));
                }
                if let Some(s) = u.slash_commands {
                    parts.push(format!("slash_commands={s}"));
                }
                format!("CapabilitiesChanged {}", parts.join(" "))
            }
            AgentEvent::TurnCompleted { stop_reason } => match stop_reason {
                StopReason::Done => "TurnCompleted Done".to_string(),
                StopReason::Interrupted => "TurnCompleted Interrupted".to_string(),
                StopReason::Error(e) => format!("TurnCompleted Error {:?}", short(e)),
            },
            AgentEvent::PolicyChanged(p) => format!("PolicyChanged {p}"),
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
    fn plan_status_spellings() {
        assert_eq!(PlanStatus::parse("inProgress"), PlanStatus::InProgress);
        assert_eq!(PlanStatus::parse("in_progress"), PlanStatus::InProgress);
        assert_eq!(PlanStatus::parse("completed"), PlanStatus::Completed);
        assert_eq!(PlanStatus::parse("whatever"), PlanStatus::Pending);
    }

    #[test]
    fn context_merge_and_percent() {
        let mut c = ContextUsage::default();
        assert_eq!(c.percent(), None);
        c.merge(ContextUsage {
            used: Some(50_000),
            window: None,
        });
        c.merge(ContextUsage {
            used: None,
            window: Some(200_000),
        });
        assert_eq!(c.percent(), Some(25));
    }

    #[test]
    fn new_event_summaries() {
        let sub = AgentEvent::Sub {
            parent: "t1".into(),
            event: Box::new(AgentEvent::TextDelta("hi".into())),
        };
        assert_eq!(sub.summary(), "Sub parent=t1 TextDelta \"hi\"");
        assert_eq!(
            AgentEvent::SubagentStarted {
                id: "t1".into(),
                description: "read a file".into(),
                kind: Some("Explore".into()),
            }
            .summary(),
            "SubagentStarted id=t1 kind=Explore \"read a file\""
        );
        assert_eq!(
            AgentEvent::SubagentEnded {
                id: "t1".into(),
                status: SubagentStatus::Cancelled,
                result: None,
            }
            .summary(),
            "SubagentEnded id=t1 stopped -"
        );
        let plan = AgentEvent::PlanUpdated {
            entries: vec![PlanEntry {
                text: "a".into(),
                status: PlanStatus::Completed,
            }],
            explanation: None,
        };
        assert_eq!(plan.summary(), "PlanUpdated [x] a");
        assert_eq!(
            AgentEvent::CapabilitiesChanged(CapsUpdate::efforts(vec!["low".into()])).summary(),
            "CapabilitiesChanged efforts=low"
        );
    }

    #[test]
    fn a_command_needs_a_name_a_prompt_can_carry() {
        let c = HarnessCommand::new("/review ", None, Some("  ")).unwrap();
        assert_eq!(c.name, "review");
        assert_eq!(c.description, "");
        assert_eq!(c.hint, None);
        assert!(HarnessCommand::new("", Some("d"), None).is_none());
        assert!(HarnessCommand::new("two words", Some("d"), None).is_none());
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
