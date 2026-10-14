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
    /// The agent reads and plans; it acts only once the user approves its
    /// plan, and then under the policy the user picks with the approval.
    /// Everything `Ask` guarantees holds here too.
    Plan,
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
    pub const ALL: [PermissionPolicy; 5] = [
        PermissionPolicy::Plan,
        PermissionPolicy::Ask,
        PermissionPolicy::AcceptEdits,
        PermissionPolicy::Auto,
        PermissionPolicy::Bypass,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "plan" => Some(PermissionPolicy::Plan),
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
            PermissionPolicy::Plan => "plan",
            PermissionPolicy::Ask => "ask",
            PermissionPolicy::AcceptEdits => "accept-edits",
            PermissionPolicy::Auto => "auto",
            PermissionPolicy::Bypass => "bypass",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            PermissionPolicy::Plan => "Read and plan; act only once the plan is approved",
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
                "Unknown policy '{}'. Supported: plan, ask, accept-edits, auto, bypass",
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
    /// The choices the user is offered only between turns, with no subagent
    /// whose end will be told still at work: Claude's and Codex's provider,
    /// Mistral Vibe's model, effort and provider. Read by
    /// `Controller::between_turns` alone. How the session takes one is its
    /// driver's to say, and nothing is sent after it until it has.
    pub between_turns: Choices,
    /// A stored session of the harness keeps the model and effort it last
    /// ran, for this reason (Mistral Vibe pins them on the session): a
    /// choice made while none of its sessions is live, or while the live
    /// one is a stored session nothing was sent to, leaves it for a new
    /// session told the conversation, and none is made while an earlier
    /// CLI of the harness is still exiting (the start that waits for it
    /// resumes the stored session). Read by
    /// `Controller::stored_session_keeps` alone. It stays a flag (#275): it
    /// decides before a driver could, with none running, or with one the
    /// TUI would otherwise start again on the stored session, as it ran.
    pub stored_session_keeps: Option<&'static str>,
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
    ///
    /// Read by `Controller::start_when_idle` alone. It stays a flag (#275): it
    /// decides whether a process is started at all before the first
    /// prompt, when there is no driver yet to decide anything (agy 1.3.2
    /// writes a conversation for every process it starts,
    /// `docs/dev/slash-commands.md`).
    pub start_unprompted: bool,
    /// The session can be put on the vendor's own remote control (Claude's
    /// Remote Control: claude.ai/code and the Claude app), and taken off it
    /// (`SessionCommand::RemoteControl`).
    pub remote_control: bool,
}

/// What a harness declares and a live session of it can lack, of what no
/// other field of a [`CapsUpdate`] says (`CapsUpdate::lacks`): each is
/// something the TUI offers, or a command the driver would refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ability {
    /// [`Capabilities::steer`].
    Steer,
    /// [`Capabilities::compaction`].
    Compaction,
    /// [`RewindSupport::conversation`].
    Rewind,
    /// [`SubagentSupport::stop`].
    StopSubagent,
    /// [`Capabilities::interactive_permissions`].
    InteractivePermissions,
}

impl Ability {
    pub const ALL: [Ability; 5] = [
        Ability::Steer,
        Ability::Compaction,
        Ability::Rewind,
        Ability::StopSubagent,
        Ability::InteractivePermissions,
    ];

    /// Its name in `--print`'s `capabilities_changed` and in a fixture's
    /// `.events`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Ability::Steer => "steer",
            Ability::Compaction => "compaction",
            Ability::Rewind => "rewind",
            Ability::StopSubagent => "stop_subagent",
            Ability::InteractivePermissions => "interactive_permissions",
        }
    }
}

/// Which of the choices of a model, an effort and a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Choices {
    pub model: bool,
    pub effort: bool,
    pub provider: bool,
}

impl Choices {
    pub const ALL: Choices = Choices {
        model: true,
        effort: true,
        provider: true,
    };
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
    ///
    /// Read by `--print` alone, whose `Tracking` (`report_turns`) counts
    /// that turn's end before the run is over (`docs/dev/headless.md`);
    /// the TUI shows such a turn as it comes. It stays a flag (#275): that
    /// the turn will come is known of the harness (Claude Code), not said
    /// on its stream before the turn starts.
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
        if let Some(f) = update.fork {
            self.fork = f;
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
        if let Some(q) = update.ask_user_question {
            self.ask_user_question = q;
        }
        for refused in [&update.refused_policies, &update.lacks_policies]
            .into_iter()
            .flatten()
        {
            self.permission_policies
                .retain(|p| !refused.contains(&p.policy));
        }
        if let Some(lacks) = &update.lacks {
            for ability in lacks {
                self.withhold(*ability);
            }
        }
    }

    /// Whether it has `ability`.
    pub fn has(&self, ability: Ability) -> bool {
        match ability {
            Ability::Steer => self.steer,
            Ability::Compaction => self.compaction,
            Ability::Rewind => self.rewind.conversation,
            Ability::StopSubagent => self.subagents.stop,
            Ability::InteractivePermissions => self.interactive_permissions,
        }
    }

    fn withhold(&mut self, ability: Ability) {
        match ability {
            Ability::Steer => self.steer = false,
            Ability::Compaction => self.compaction = false,
            Ability::Rewind => self.rewind.conversation = false,
            Ability::StopSubagent => self.subagents.stop = false,
            Ability::InteractivePermissions => self.interactive_permissions = false,
        }
    }

    /// The report, on top of these declared capabilities, of a live
    /// session that can do only what `session` declares, for every session
    /// of the run: the abilities and policies it lacks (`CapsUpdate::lacks`
    /// and `lacks_policies`, lists also when it lacks none), and the flags
    /// of a report that it does not have. Never what it has beyond them:
    /// such a report only takes away. The effort levels and models a
    /// session offers are its own to report.
    pub fn lacking(&self, session: &Capabilities) -> CapsUpdate {
        let lost = |declared: bool, has: bool| (declared && !has).then_some(false);
        let lacked: Vec<PermissionPolicy> = self
            .permission_policies
            .iter()
            .map(|p| p.policy)
            .filter(|p| session.supports_policy(*p).is_none())
            .collect();
        CapsUpdate {
            lacks: Some(
                Ability::ALL
                    .into_iter()
                    .filter(|a| self.has(*a) && !session.has(*a))
                    .collect(),
            ),
            image_input: lost(self.image_input, session.image_input),
            resume_by_id: lost(self.resume_by_id, session.resume_by_id),
            fork: lost(self.fork, session.fork),
            mcp_http: lost(self.mcp.http, session.mcp.http),
            plan_mode: lost(self.plan_mode, session.plan_mode),
            slash_commands: lost(self.slash_commands, session.slash_commands),
            ask_user_question: lost(self.ask_user_question, session.ask_user_question),
            lacks_policies: Some(lacked),
            ..Default::default()
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
        assert_eq!(PermissionPolicy::parse("Plan"), Some(Plan));
        assert!(Plan < Ask);
        // As saved in a conversation.
        assert_eq!(serde_json::to_string(&Plan).unwrap(), "\"plan\"");
    }

    /// Plan is the least permissive policy: nothing stands in for it, and
    /// it stands in for nothing a harness lacks unless nothing else is left.
    #[test]
    fn plan_is_never_more_permissive_than_asked() {
        let without = [PolicySupport::full(Ask), PolicySupport::full(Bypass)];
        let e = resolve_policy(&without, Plan).unwrap_err();
        assert_eq!(e.requested, Plan);
        let with: Vec<PolicySupport> = PermissionPolicy::ALL
            .into_iter()
            .map(PolicySupport::full)
            .collect();
        assert_eq!(resolve_policy(&with, Ask).unwrap().effective, Ask);
        let only_plan = [PolicySupport::full(Plan), PolicySupport::full(Bypass)];
        let r = resolve_policy(&only_plan, AcceptEdits).unwrap();
        assert_eq!(r.effective, Plan);
        assert!(r.warning.is_some());
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

    /// What a session reports of what it lacks takes that away, and only
    /// that: what it has beyond the declaration is not said, and applied
    /// to the declaration the report leaves what both have (#288).
    #[test]
    fn a_session_lacking_what_is_declared_only_takes_away() {
        let mut declared = Capabilities {
            steer: true,
            compaction: true,
            interactive_permissions: true,
            fork: true,
            image_input: true,
            permission_policies: vec![PolicySupport::full(Ask), PolicySupport::full(Bypass)],
            ..Default::default()
        };
        declared.rewind.conversation = true;
        declared.subagents.stop = true;
        let mut session = Capabilities {
            compaction: true,
            image_input: true,
            // More than declared: not said.
            plan_mode: true,
            remote_control: true,
            permission_policies: vec![
                PolicySupport::full(AcceptEdits),
                PolicySupport::full(Bypass),
            ],
            ..Default::default()
        };
        session.subagents.stop = true;

        let report = declared.lacking(&session);
        assert_eq!(
            report,
            CapsUpdate {
                lacks: Some(vec![
                    Ability::Steer,
                    Ability::Rewind,
                    Ability::InteractivePermissions
                ]),
                fork: Some(false),
                lacks_policies: Some(vec![Ask]),
                ..Default::default()
            }
        );
        let mut now = declared.clone();
        now.apply(&report);
        for ability in Ability::ALL {
            assert_eq!(
                now.has(ability),
                declared.has(ability) && session.has(ability),
                "{ability:?}"
            );
        }
        assert!(!now.fork && now.image_input && !now.plan_mode && !now.remote_control);
        assert_eq!(now.permission_policies, [PolicySupport::full(Bypass)]);

        // A session with all that is declared lacks nothing, and says so.
        assert_eq!(
            declared.lacking(&declared),
            CapsUpdate {
                lacks: Some(Vec::new()),
                lacks_policies: Some(Vec::new()),
                ..Default::default()
            }
        );
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
