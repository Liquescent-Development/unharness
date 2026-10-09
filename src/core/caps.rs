//! Capability flags each harness declares, and the unified permission policy.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::event::CapsUpdate;
use super::session::Attachment;

/// How tool permissions are handled, ordered least → most permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionPolicy {
    /// Nothing that writes, executes or reaches out runs without the
    /// user's answer or an allow rule; reads may run. A harness that cannot
    /// hold to that does not declare this policy.
    Ask,
    /// File edits are auto-approved; other actions still prompt.
    AcceptEdits,
    /// The harness's own classifier reviews actions; residual prompts reach the TUI.
    Auto,
    /// Skip every permission check (dangerous).
    Bypass,
}

impl PermissionPolicy {
    pub const ALL: [PermissionPolicy; 4] = [
        PermissionPolicy::Ask,
        PermissionPolicy::AcceptEdits,
        PermissionPolicy::Auto,
        PermissionPolicy::Bypass,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "ask" | "manual" => Some(PermissionPolicy::Ask),
            "accept-edits" | "acceptedits" | "edits" => Some(PermissionPolicy::AcceptEdits),
            "auto" => Some(PermissionPolicy::Auto),
            "bypass" | "skip" | "dangerously-skip-permissions" | "yolo" => {
                Some(PermissionPolicy::Bypass)
            }
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionPolicy::Ask => "ask",
            PermissionPolicy::AcceptEdits => "accept-edits",
            PermissionPolicy::Auto => "auto",
            PermissionPolicy::Bypass => "bypass",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            PermissionPolicy::Ask => "Prompt for every tool permission",
            PermissionPolicy::AcceptEdits => "Auto-approve file edits, prompt for the rest",
            PermissionPolicy::Auto => "Harness classifier reviews actions automatically",
            PermissionPolicy::Bypass => "Skip all permission checks (dangerous)",
        }
    }
}

impl fmt::Display for PermissionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PermissionPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PermissionPolicy::parse(s).ok_or_else(|| {
            format!(
                "Unknown policy '{}'. Supported: ask, accept-edits, auto, bypass",
                s
            )
        })
    }
}

/// A policy a harness supports, optionally with a caveat shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicySupport {
    pub policy: PermissionPolicy,
    pub degraded: Option<&'static str>,
}

impl PolicySupport {
    pub const fn full(policy: PermissionPolicy) -> Self {
        PolicySupport {
            policy,
            degraded: None,
        }
    }
    pub const fn degraded(policy: PermissionPolicy, note: &'static str) -> Self {
        PolicySupport {
            policy,
            degraded: Some(note),
        }
    }
}

/// What a harness can do. The TUI reads these instead of assuming.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// A long-lived process accepts turns on stdin.
    pub streaming_input: bool,
    /// Assistant text arrives as deltas rather than whole messages.
    pub text_deltas: bool,
    /// Thinking / reasoning text is exposed.
    pub thinking: bool,
    /// Tool calls and results are exposed as events.
    pub tool_events: bool,
    /// Permission prompts can be answered from the TUI.
    pub interactive_permissions: bool,
    pub permission_policies: Vec<PolicySupport>,
    /// Static effort levels; may be refined live per model.
    pub effort_levels: Vec<String>,
    pub resume_by_id: bool,
    pub live_model_list: bool,
    pub multi_provider: bool,
    /// The provider is chosen when the process starts, so a session that
    /// changes it starts again (resumed); otherwise it goes with the model.
    pub provider_per_process: bool,
    /// The agent can ask the user structured questions.
    pub ask_user_question: bool,
    pub interrupt: bool,
    pub usage_reporting: bool,
    /// Turns can carry image attachments.
    pub image_input: bool,
    /// Turns can carry document attachments (PDF and text files).
    pub file_input: bool,
    /// The agent's plan / todo list arrives as `PlanUpdated`.
    pub plan_updates: bool,
    pub subagents: SubagentSupport,
    /// A message can be injected into the running turn.
    pub steer: bool,
    /// Context can be compacted on request.
    pub compaction: bool,
    /// Context-window usage is reported (`Context`).
    pub context_usage: bool,
    /// Account rate-limit windows are reported (`RateLimit`).
    pub rate_limits: bool,
    pub rewind: RewindSupport,
    /// A session can be branched into a new one (`SessionConfig::fork`).
    pub fork: bool,
    pub mcp: McpSupport,
    /// The vendor has a mode in which the agent proposes a plan and waits
    /// for approval before acting. (`plan_updates` only says that a todo
    /// list is reported.)
    pub plan_mode: bool,
    /// A prompt that starts with `/name` runs the harness's command of that
    /// name (its own, a custom one or a skill), so a command unharness
    /// does not have itself is passed on. Which ones there are comes with
    /// `CapsUpdate::commands`, where the harness lists them.
    pub slash_commands: bool,
    /// A session can be started before anything is sent to it and leave
    /// nothing behind, so the TUI starts it as soon as it is idle without
    /// one, to have what it reports (its commands) before the first prompt.
    pub start_unprompted: bool,
}

/// Whether a harness takes MCP server definitions for one session, without
/// its own configuration being written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct McpSupport {
    /// How the definitions reach it. `None`: it only knows the servers in
    /// its own configuration.
    pub channel: Option<McpChannel>,
    /// Servers reached over HTTP (a `url`) are taken besides the ones it
    /// launches itself (a `command`).
    pub http: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpChannel {
    /// Flags or config overrides on the harness's command line.
    CommandLine,
    /// A field of the protocol's session request.
    Protocol,
}

impl McpSupport {
    pub const NONE: McpSupport = McpSupport {
        channel: None,
        http: false,
    };

    pub const fn via(channel: McpChannel, http: bool) -> Self {
        McpSupport {
            channel: Some(channel),
            http,
        }
    }

    /// For `doctor`: "command line (stdio, http)", "no".
    pub fn describe(&self) -> String {
        let channel = match self.channel {
            None => return "no".to_string(),
            Some(McpChannel::CommandLine) => "command line",
            Some(McpChannel::Protocol) => "protocol",
        };
        format!(
            "{channel} ({})",
            if self.http { "stdio, http" } else { "stdio" }
        )
    }
}

/// What a harness tells about the subagents its agent dispatches, and what
/// can be done about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubagentSupport {
    /// Their start, their end and what they do are reported (`Subagent*`,
    /// `Sub`). Without this a subagent is invisible.
    pub reported: bool,
    /// The harness words each one's task and what it is doing now;
    /// otherwise there is a bare name, and its tool calls stand in.
    pub described: bool,
    /// A running subagent can be stopped (`SessionCommand::StopSubagent`).
    pub stop: bool,
    /// One that ends between turns is reported to the agent in a turn the
    /// harness starts by itself.
    pub report_turn: bool,
}

/// How a harness can go back to an earlier turn of its own session. (Files
/// are restored by unharness itself, from its checkpoints.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RewindSupport {
    /// The session can drop a turn and everything after it (`TurnAnchor`).
    pub conversation: bool,
    /// Anchors of the original session still work in a fork of it.
    pub anchors_survive_fork: bool,
}

impl Capabilities {
    /// Whether a turn can carry this attachment.
    pub fn accepts(&self, attachment: &Attachment) -> bool {
        match attachment {
            Attachment::Image { .. } => self.image_input,
            Attachment::File { .. } => self.file_input,
        }
    }

    /// Overlay what a live session reported on the static declaration.
    pub fn apply(&mut self, update: &CapsUpdate) {
        if let Some(e) = &update.effort_levels {
            self.effort_levels = e.clone();
        }
        if let Some(i) = update.image_input {
            self.image_input = i;
        }
        if let Some(r) = update.resume_by_id {
            self.resume_by_id = r;
        }
        if let Some(h) = update.mcp_http {
            self.mcp.http = h;
        }
        if let Some(p) = update.plan_mode {
            self.plan_mode = p;
        }
        if let Some(s) = update.slash_commands {
            self.slash_commands = s;
        }
    }

    pub fn supports_policy(&self, policy: PermissionPolicy) -> Option<&PolicySupport> {
        self.permission_policies.iter().find(|p| p.policy == policy)
    }

    pub fn supports_effort(&self, level: &str) -> bool {
        self.effort_levels.iter().any(|l| l == level)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyResolution {
    pub requested: PermissionPolicy,
    pub effective: PermissionPolicy,
    pub warning: Option<String>,
}

/// The requested policy is not supported and nothing less permissive is:
/// the user has to choose one of `supported`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyUnavailable {
    pub requested: PermissionPolicy,
    /// Least permissive first.
    pub supported: Vec<PermissionPolicy>,
}

impl fmt::Display for PolicyUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let supported: Vec<&str> = self.supported.iter().map(|p| p.as_str()).collect();
        if supported.is_empty() {
            write!(f, "policy '{}' is not available here", self.requested)
        } else {
            write!(
                f,
                "policy '{}' is not available here; choose one of: {}",
                self.requested,
                supported.join(", ")
            )
        }
    }
}

impl std::error::Error for PolicyUnavailable {}

/// Map a requested policy onto what the harness supports.
///
/// Exact match wins. Otherwise the nearest *less* permissive supported policy
/// is chosen. A more permissive one is never chosen here: when nothing at or
/// below the request is supported the answer is [`PolicyUnavailable`] and the
/// user decides. A `degraded` note on the chosen policy is surfaced as the
/// warning.
pub fn resolve_policy(
    policies: &[PolicySupport],
    requested: PermissionPolicy,
) -> Result<PolicyResolution, PolicyUnavailable> {
    let Some(chosen) = policies
        .iter()
        .filter(|p| p.policy <= requested)
        .max_by_key(|p| p.policy)
    else {
        let mut supported: Vec<PermissionPolicy> = policies.iter().map(|p| p.policy).collect();
        supported.sort();
        return Err(PolicyUnavailable {
            requested,
            supported,
        });
    };

    let mut warning = None;
    if chosen.policy != requested {
        warning = Some(format!(
            "policy '{}' not supported; using less permissive '{}'",
            requested, chosen.policy
        ));
    }
    if let Some(note) = chosen.degraded {
        warning = Some(match warning {
            Some(w) => format!("{} ({})", w, note),
            None => note.to_string(),
        });
    }

    Ok(PolicyResolution {
        requested,
        effective: chosen.policy,
        warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionPolicy::*;

    #[test]
    fn parse_policy() {
        assert_eq!(PermissionPolicy::parse("accept_edits"), Some(AcceptEdits));
        assert_eq!(PermissionPolicy::parse("ACCEPT-EDITS"), Some(AcceptEdits));
        assert_eq!(PermissionPolicy::parse("yolo"), Some(Bypass));
        assert_eq!(PermissionPolicy::parse("nope"), None);
        assert!(Ask < AcceptEdits && AcceptEdits < Auto && Auto < Bypass);
    }

    #[test]
    fn exact_match_no_warning() {
        let c = [PolicySupport::full(Ask), PolicySupport::full(Bypass)];
        let r = resolve_policy(&c, Bypass).unwrap();
        assert_eq!(r.effective, Bypass);
        assert!(r.warning.is_none());
    }

    #[test]
    fn falls_back_to_the_nearest_less_permissive() {
        let c = [
            PolicySupport::full(Ask),
            PolicySupport::full(AcceptEdits),
            PolicySupport::full(Bypass),
        ];
        let r = resolve_policy(&c, Auto).unwrap();
        assert_eq!(r.effective, AcceptEdits);
        assert!(r.warning.unwrap().contains("less permissive"));
    }

    #[test]
    fn never_escalates() {
        // Like a harness that cannot prompt: nothing at or below `ask`.
        let c = [
            PolicySupport::full(Bypass),
            PolicySupport::full(AcceptEdits),
        ];
        let e = resolve_policy(&c, Ask).unwrap_err();
        assert_eq!(e.supported, vec![AcceptEdits, Bypass]);
        assert_eq!(
            e.to_string(),
            "policy 'ask' is not available here; choose one of: accept-edits, bypass"
        );
        assert!(resolve_policy(&[], Bypass).is_err());
    }

    #[test]
    fn degraded_note_is_surfaced() {
        let c = [
            PolicySupport::full(Ask),
            PolicySupport::degraded(AcceptEdits, "edits are not shown"),
        ];
        let r = resolve_policy(&c, Auto).unwrap();
        assert_eq!(r.effective, AcceptEdits);
        let w = r.warning.unwrap();
        assert!(w.contains("less permissive") && w.contains("edits are not shown"));

        let r2 = resolve_policy(&c, AcceptEdits).unwrap();
        assert_eq!(r2.warning.as_deref(), Some("edits are not shown"));
    }

    #[test]
    fn a_session_refines_mcp_and_plan_mode() {
        let mut c = Capabilities {
            mcp: McpSupport::via(McpChannel::Protocol, false),
            ..Default::default()
        };
        assert_eq!(c.mcp.describe(), "protocol (stdio)");
        c.apply(&CapsUpdate {
            mcp_http: Some(true),
            plan_mode: Some(true),
            ..Default::default()
        });
        assert_eq!(c.mcp.describe(), "protocol (stdio, http)");
        assert!(c.plan_mode);
        assert_eq!(McpSupport::NONE.describe(), "no");
        assert_eq!(Capabilities::default().mcp, McpSupport::NONE);
    }
}
