//! The harness-agnostic event model. Every protocol parser produces these and
//! the TUI consumes only these.

use serde_json::Value;

use super::caps::{Ability, PermissionPolicy};
use super::ids::{ModelInfo, ProviderId};
use super::rules::ToolAction;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    SessionStarted {
        session_id: String,
        model: Option<String>,
    },
    /// The session the harness was to resume or fork could not be, and the
    /// one it runs instead knows nothing of the conversation: what the old
    /// one was told is owed to it. `reason` says what failed. Comes before
    /// or after that session's `SessionStarted`, which may name the id the
    /// old one had (pi creates a missing session under the id it is given).
    SessionReplaced {
        reason: String,
    },
    /// The session cannot take the model, effort or provider just chosen
    /// (`reason` names it and says why: Mistral Vibe pins them on the
    /// session), so the conversation goes on in a new one, told it through
    /// the bridge, as after a switch. Nothing has ended: the session stays,
    /// for a handoff summary when the conversation does not fit the bridge,
    /// until the TUI ends it.
    SessionRenewed {
        reason: String,
    },
    /// The session's CLI was started again for a choice it takes only that
    /// way (Claude's and Codex's provider), and the session goes on, resumed,
    /// in the new process: nothing has been sent to that yet, and what the
    /// one before was running (requests, hooks, subagents) went with it.
    Restarted,
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
    /// was stopped, or the request was answered elsewhere): the question is
    /// closed unanswered.
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
        /// Its end may never be told (Mistral Vibe tells it only when the
        /// agent waits for the subagent): nothing waits for it, neither a
        /// `--print` run nor what would end the session, and it goes with
        /// the session. Not in the `.events` summary.
        unreported_end: bool,
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
    /// What the harness's CLI said of the account it runs on.
    Account(AccountInfo),
    /// What the CLI counts of its own session, over all of it.
    SessionStats(SessionStats),
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
    /// The session's remote control (`SessionCommand::RemoteControl`).
    RemoteControl(RemoteControl),
    /// A prompt the user typed elsewhere (on the remote control), which the
    /// harness takes as a turn of its own.
    RemotePrompt {
        text: String,
    },
    /// Informational line (retries, compaction, degraded policy...).
    Notice(String),
    Error(String),
    ProcessExited {
        code: Option<i32>,
    },
}

/// What became of the session's remote control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteControl {
    /// On; the session can be opened at `url`.
    On { url: String },
    /// How the connection stands, in the harness's words (`ready`,
    /// `connected`...).
    State {
        state: String,
        detail: Option<String>,
    },
    /// Taken off on request.
    Off,
    /// Could not be put on, or was ended by the harness.
    Failed { reason: String },
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

impl RateLimitWindow {
    /// The window in words: `5-hour`, `weekly`, `weekly opus`.
    pub fn name(&self) -> String {
        self.named("-hour", "weekly")
    }

    /// The window in the status line: `5h`, `7d`, `7d opus`.
    pub fn short_name(&self) -> String {
        self.named("h", "7d")
    }

    fn named(&self, hours: &str, week: &str) -> String {
        let label = self.label.as_str();
        if label == "five_hour" {
            return format!("5{hours}");
        }
        if let Some(rest) = label.strip_prefix("seven_day") {
            return format!("{week}{}", rest.replace('_', " "));
        }
        // Codex's, from the window's length: `5h`, `7d`, `90m`.
        let (n, unit) = label.split_at(label.len().saturating_sub(1));
        match (n.parse::<u32>(), unit) {
            (Ok(7), "d") => week.to_string(),
            (Ok(n), "h") => format!("{n}{hours}"),
            (Ok(n), "d") if hours == "h" => format!("{n}d"),
            (Ok(n), "d") => format!("{n}-day"),
            (Ok(n), "m") if hours == "h" => format!("{n}m"),
            (Ok(n), "m") => format!("{n}-minute"),
            _ => label.replace('_', " "),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RateLimitInfo {
    /// Vendor status string when one is reported (`allowed`, `rejected`...).
    pub status: Option<String>,
    /// The window `status` is about, when it is not `allowed` and the
    /// vendor names one (Claude's `rateLimitType`).
    pub window: Option<String>,
    pub windows: Vec<RateLimitWindow>,
    /// Which of the account's limits this is, where it has several
    /// (Codex's `limitId`).
    pub limit: Option<String>,
    /// What may be used past the limits, where the vendor says.
    pub overage: Option<Overage>,
}

impl RateLimitInfo {
    /// Take what `newer` reports: its status, overage and limit when it
    /// has them, and each of its windows in place of the one of that
    /// label. A window it does not mention stays (a report that names only
    /// the window in force).
    pub fn merge(&mut self, newer: RateLimitInfo) {
        if newer.status.is_some() {
            self.status = newer.status;
            self.window = newer.window;
        }
        if newer.limit.is_some() {
            self.limit = newer.limit;
        }
        if newer.overage.is_some() {
            self.overage = newer.overage;
        }
        for w in newer.windows {
            match self.windows.iter_mut().find(|o| o.label == w.label) {
                Some(old) => *old = w,
                None => self.windows.push(w),
            }
        }
    }

    /// Whether a limit stops use: a status other than Claude's `allowed`
    /// and `allowed_warning` (Claude's `rejected`, Codex's
    /// `rateLimitReachedType`), or a window used up.
    pub fn reached(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|s| s != "allowed" && s != "allowed_warning")
            || self
                .windows
                .iter()
                .any(|w| w.used_percent.is_some_and(|p| p >= 100.0))
    }

    /// Whether paid use past the limits goes on where they are reached.
    pub fn overage_covers(&self) -> bool {
        self.overage
            .as_ref()
            .is_some_and(|o| o.in_use || o.status.as_deref() == Some("allowed"))
    }
}

/// The account a harness's CLI runs on, as far as it says. Any field may
/// be unknown.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AccountInfo {
    /// The subscription (`Claude Max`, `prolite`).
    pub plan: Option<String>,
    /// How the CLI signs in (`firstParty`, `vertex`, `chatgpt`, `apikey`).
    pub auth: Option<String>,
    pub email: Option<String>,
    pub organization: Option<String>,
    pub credits: Option<Credits>,
}

impl AccountInfo {
    /// Keep the known fields of `self`, take the rest from `newer`.
    pub fn merge(&mut self, newer: AccountInfo) {
        let AccountInfo {
            plan,
            auth,
            email,
            organization,
            credits,
        } = newer;
        self.plan = plan.or(self.plan.take());
        self.auth = auth.or(self.auth.take());
        self.email = email.or(self.email.take());
        self.organization = organization.or(self.organization.take());
        self.credits = credits.or(self.credits.take());
    }
}

/// A CLI's own count of its session (pi's `get_session_stats`): over all
/// of the session, so also turns unharness did not see (a session resumed
/// from elsewhere) and those of the session it was forked from, which
/// unharness's sum of what each response billed leaves out.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionStats {
    pub user_messages: Option<u64>,
    pub assistant_messages: Option<u64>,
    pub tool_calls: Option<u64>,
    /// Every token, of any kind.
    pub tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// Credits an account can spend past its plan (Codex's).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Credits {
    pub has_credits: bool,
    pub unlimited: bool,
    /// As the vendor gives it (a decimal string, `62500.0000000000`).
    pub balance: Option<String>,
}

/// Use past the rate limits, paid apart (Claude's "extra usage").
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overage {
    /// Vendor status string (`allowed`, `rejected`...).
    pub status: Option<String>,
    pub in_use: bool,
    /// Why it cannot be used, when it cannot.
    pub disabled_reason: Option<String>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

/// A partial capability update; `None` leaves the declared value alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapsUpdate {
    pub effort_levels: Option<Vec<String>>,
    pub image_input: Option<bool>,
    pub resume_by_id: Option<bool>,
    /// A session can be branched into a new one (`Capabilities::fork`): an
    /// ACP agent says so at `initialize`.
    pub fork: Option<bool>,
    /// Context-window usage may be reported (`Capabilities::context_usage`).
    pub context_usage: Option<bool>,
    /// The CLI answers a status query (`Capabilities::status_query`).
    pub status_query: Option<bool>,
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
    /// Policies the live session cannot hold although the harness declares
    /// them (an ACP agent whose mode for the policy did not take, or that
    /// has modes and no map, below `bypass`): taken out of what is offered, as a
    /// policy the harness never had is. Each list replaces the last, and
    /// the TUI drops it when the session ends: a refusal is for the session.
    pub refused_policies: Option<Vec<PermissionPolicy>>,
    /// The agent asks the user questions (`Capabilities::ask_user_question`):
    /// an ACP agent is seen to once it sends a form elicitation.
    pub ask_user_question: Option<bool>,
    /// Whether a prompt sent at once that the CLI already has can be taken
    /// back (`Capabilities::take_back_sent_now`): Claude says at
    /// `system/init` whether its interrupt takes `cancel_queued`.
    pub take_back_sent_now: Option<bool>,
    /// What the live session cannot do although its harness declares it
    /// (Codex under `transport = "auto"` that came to `exec`): taken out of
    /// what is offered, and refused by the driver if asked all the same.
    /// It only ever takes away (`Capabilities::lacking` builds one). Each
    /// list replaces the last, and it stays when the session ends, until
    /// another session of the harness reports one: it holds for every
    /// session of the run.
    pub lacks: Option<Vec<Ability>>,
    /// The policies of the harness's that the live session cannot hold, as
    /// `lacks` is said and kept (Codex under `auto` that came to `exec`
    /// cannot ask): taken out of what is offered for the run, where a
    /// policy in `refused_policies` is for its session alone. One in effect
    /// gives way as a refused one does.
    pub lacks_policies: Option<Vec<PermissionPolicy>>,
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
        if newer.fork.is_some() {
            self.fork = newer.fork;
        }
        if newer.context_usage.is_some() {
            self.context_usage = newer.context_usage;
        }
        if newer.status_query.is_some() {
            self.status_query = newer.status_query;
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
        if newer.refused_policies.is_some() {
            self.refused_policies = newer.refused_policies;
        }
        if newer.ask_user_question.is_some() {
            self.ask_user_question = newer.ask_user_question;
        }
        if newer.take_back_sent_now.is_some() {
            self.take_back_sent_now = newer.take_back_sent_now;
        }
        if newer.lacks.is_some() {
            self.lacks = newer.lacks;
        }
        if newer.lacks_policies.is_some() {
            self.lacks_policies = newer.lacks_policies;
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

/// Tokens a turn, a request or a session used, in four counts that do not
/// overlap: `total_tokens` is their sum.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Input tokens neither read from nor written to the prompt cache.
    /// Codex counts its cache reads in its input; its parsers take them
    /// out (#308).
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: Option<f64>,
    /// True when the numbers are session totals rather than one turn's or
    /// one request's; `scope` is then not read.
    pub cumulative: bool,
    /// What a report that is not `cumulative` covers. Left out of the
    /// fixture summary, and not saved when it is a turn's, as it was before
    /// there was a scope.
    #[serde(default, skip_serializing_if = "UsageScope::is_turn")]
    pub scope: UsageScope,
}

/// What a usage report covers: a whole turn, or one of the model requests
/// a turn makes, of which it may report several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    /// Claude's `result`, agy's sum, Codex `exec`'s `turn.completed`, an
    /// ACP prompt's answer.
    #[default]
    Turn,
    /// Codex app-server's `thread/tokenUsage/updated`, pi's `turn_end`
    /// (one per model call).
    Request,
}

impl UsageScope {
    pub fn is_turn(&self) -> bool {
        *self == UsageScope::Turn
    }
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
            AgentEvent::SessionReplaced { reason } => {
                format!("SessionReplaced {:?}", short(reason))
            }
            AgentEvent::SessionRenewed { reason } => {
                format!("SessionRenewed {:?}", short(reason))
            }
            AgentEvent::Restarted => "Restarted".to_string(),
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
                ..
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
            AgentEvent::RateLimit(r) => {
                let mut parts = vec![format!(
                    "status={}{}",
                    r.status.as_deref().unwrap_or("-"),
                    r.window
                        .as_deref()
                        .map(|w| format!("({w})"))
                        .unwrap_or_default()
                )];
                if let Some(limit) = &r.limit {
                    parts.push(format!("limit={limit}"));
                }
                parts.extend(r.windows.iter().map(|w| {
                    format!(
                        "{}={}@{}",
                        w.label,
                        w.used_percent
                            .map(|p| format!("{p:.0}%"))
                            .unwrap_or_else(|| "?".into()),
                        w.resets_at
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "-".into())
                    )
                }));
                if let Some(o) = &r.overage {
                    parts.push(format!(
                        "overage={}{}{}",
                        o.status.as_deref().unwrap_or("-"),
                        if o.in_use { ",in_use" } else { "" },
                        o.disabled_reason
                            .as_deref()
                            .map(|d| format!("({d})"))
                            .unwrap_or_default()
                    ));
                }
                format!("RateLimit {}", parts.join(" "))
            }
            // Who the account is stays out of fixtures, redacted or not.
            AgentEvent::Account(a) => {
                let or_dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
                let set = |v: &Option<String>| if v.is_some() { "set" } else { "-" };
                format!(
                    "Account plan={} auth={} email={} org={} credits={}",
                    or_dash(&a.plan),
                    or_dash(&a.auth),
                    set(&a.email),
                    set(&a.organization),
                    a.credits
                        .as_ref()
                        .map(|c| if c.unlimited {
                            "unlimited".to_string()
                        } else {
                            format!(
                                "{}{}",
                                c.balance.as_deref().unwrap_or("?"),
                                if c.has_credits { "" } else { ",none" }
                            )
                        })
                        .unwrap_or_else(|| "-".into())
                )
            }
            AgentEvent::SessionStats(st) => {
                let n = |v: Option<u64>| v.map_or("-".to_string(), |n| n.to_string());
                format!(
                    "SessionStats user={} assistant={} tools={} tokens={} cost={}",
                    n(st.user_messages),
                    n(st.assistant_messages),
                    n(st.tool_calls),
                    n(st.tokens),
                    st.cost_usd.map_or("-".to_string(), |c| format!("{c:.4}"))
                )
            }
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
                if let Some(q) = u.status_query {
                    parts.push(format!("status_query={q}"));
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
                if let Some(r) = &u.refused_policies {
                    let r: Vec<&str> = r.iter().map(|p| p.as_str()).collect();
                    parts.push(format!("refused_policies={}", r.join(",")));
                }
                if let Some(q) = u.ask_user_question {
                    parts.push(format!("ask_user_question={q}"));
                }
                if let Some(t) = u.take_back_sent_now {
                    parts.push(format!("take_back_sent_now={t}"));
                }
                if let Some(l) = &u.lacks {
                    let l: Vec<&str> = l.iter().map(|a| a.as_str()).collect();
                    parts.push(format!("lacks={}", l.join(",")));
                }
                if let Some(l) = &u.lacks_policies {
                    let l: Vec<&str> = l.iter().map(|p| p.as_str()).collect();
                    parts.push(format!("lacks_policies={}", l.join(",")));
                }
                format!("CapabilitiesChanged {}", parts.join(" "))
            }
            AgentEvent::TurnCompleted { stop_reason } => match stop_reason {
                StopReason::Done => "TurnCompleted Done".to_string(),
                StopReason::Interrupted => "TurnCompleted Interrupted".to_string(),
                StopReason::Error(e) => format!("TurnCompleted Error {:?}", short(e)),
            },
            AgentEvent::PolicyChanged(p) => format!("PolicyChanged {p}"),
            AgentEvent::RemoteControl(r) => match r {
                RemoteControl::On { url } => format!("RemoteControl On url={url}"),
                RemoteControl::State { state, detail } => match detail {
                    Some(d) => format!("RemoteControl State {state} {:?}", short(d)),
                    None => format!("RemoteControl State {state}"),
                },
                RemoteControl::Off => "RemoteControl Off".to_string(),
                RemoteControl::Failed { reason } => {
                    format!("RemoteControl Failed {:?}", short(reason))
                }
            },
            AgentEvent::RemotePrompt { text } => format!("RemotePrompt {:?}", short(text)),
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
    fn rate_limit_merge_replaces_windows_by_label() {
        let window = |label: &str, pct: f32| RateLimitWindow {
            label: label.into(),
            used_percent: Some(pct),
            resets_at: None,
        };
        let mut r = RateLimitInfo {
            status: Some("allowed".into()),
            windows: vec![window("five_hour", 10.0), window("seven_day", 40.0)],
            ..Default::default()
        };
        r.merge(RateLimitInfo {
            status: Some("allowed_warning".into()),
            windows: vec![window("five_hour", 90.0)],
            ..Default::default()
        });
        assert_eq!(r.status.as_deref(), Some("allowed_warning"));
        assert_eq!(
            r.windows,
            [window("five_hour", 90.0), window("seven_day", 40.0)]
        );
        r.merge(RateLimitInfo {
            windows: vec![window("seven_day_opus", 5.0)],
            limit: Some("codex".into()),
            ..Default::default()
        });
        assert_eq!(r.status.as_deref(), Some("allowed_warning"));
        assert_eq!(r.limit.as_deref(), Some("codex"));
        assert_eq!(r.windows.len(), 3);
    }

    #[test]
    fn rate_limit_windows_are_named_in_words() {
        let names = |label: &str| {
            let w = RateLimitWindow {
                label: label.into(),
                used_percent: None,
                resets_at: None,
            };
            (w.name(), w.short_name())
        };
        assert_eq!(names("five_hour"), ("5-hour".into(), "5h".into()));
        assert_eq!(names("seven_day"), ("weekly".into(), "7d".into()));
        assert_eq!(
            names("seven_day_opus"),
            ("weekly opus".into(), "7d opus".into())
        );
        assert_eq!(names("5h"), ("5-hour".into(), "5h".into()));
        assert_eq!(names("7d"), ("weekly".into(), "7d".into()));
        assert_eq!(names("30d"), ("30-day".into(), "30d".into()));
        assert_eq!(names("limit"), ("limit".into(), "limit".into()));
    }

    #[test]
    fn account_merge_keeps_what_is_known() {
        let mut a = AccountInfo {
            plan: Some("Claude Max".into()),
            email: Some("user@example.com".into()),
            ..Default::default()
        };
        a.merge(AccountInfo {
            plan: Some("Claude Pro".into()),
            auth: Some("firstParty".into()),
            ..Default::default()
        });
        assert_eq!(a.plan.as_deref(), Some("Claude Pro"));
        assert_eq!(a.auth.as_deref(), Some("firstParty"));
        assert_eq!(a.email.as_deref(), Some("user@example.com"));
        // Who the account is never reaches a fixture.
        let summary = AgentEvent::Account(a).summary();
        assert!(!summary.contains("example.com"), "{summary}");
        assert!(summary.contains("email=set"), "{summary}");
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
                unreported_end: false,
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

    /// Usage saved before there was a scope reads as a turn's, and a
    /// turn's is saved as it was then.
    #[test]
    fn usage_saved_without_a_scope_is_a_turns() {
        let old = r#"{"input":1,"output":2,"cache_read":0,"cache_write":0,"cost_usd":null,"cumulative":false}"#;
        let u: Usage = serde_json::from_str(old).unwrap();
        assert_eq!(u.scope, UsageScope::Turn);
        assert_eq!(serde_json::to_string(&u).unwrap(), old);
        let request = Usage {
            scope: UsageScope::Request,
            ..u
        };
        let saved = serde_json::to_string(&request).unwrap();
        assert!(saved.ends_with(r#""scope":"request"}"#), "{saved}");
        assert_eq!(serde_json::from_str::<Usage>(&saved).unwrap(), request);
    }
}
