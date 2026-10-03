//! Capability flags each harness declares, and the unified permission policy.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::event::CapsUpdate;

/// How tool permissions are handled, ordered least → most permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionPolicy {
    /// Every prompt is routed to the TUI for a decision.
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
    /// The agent can ask the user structured questions.
    pub ask_user_question: bool,
    pub interrupt: bool,
    pub usage_reporting: bool,
    /// Turns can carry image attachments.
    pub image_input: bool,
    /// The agent's plan / todo list arrives as `PlanUpdated`.
    pub plan_updates: bool,
    /// Subagent activity is attributed to its parent tool call (`Sub`).
    pub subagents: bool,
    /// A message can be injected into the running turn.
    pub steer: bool,
    /// Context can be compacted on request.
    pub compaction: bool,
    /// Context-window usage is reported (`Context`).
    pub context_usage: bool,
    /// Account rate-limit windows are reported (`RateLimit`).
    pub rate_limits: bool,
    pub rewind: RewindSupport,
    pub fork: bool,
}

/// What a harness can roll back to an earlier turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RewindSupport {
    pub conversation: bool,
    pub files: bool,
}

impl Capabilities {
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

/// Map a requested policy onto what the harness supports.
///
/// Exact match wins. Otherwise the nearest *less* permissive supported policy
/// is chosen (never silently escalate), and only if none exists the nearest
/// more permissive one. A `degraded` note on the chosen policy is surfaced as
/// the warning.
pub fn resolve_policy(caps: &Capabilities, requested: PermissionPolicy) -> PolicyResolution {
    if caps.permission_policies.is_empty() {
        return PolicyResolution {
            requested,
            effective: requested,
            warning: Some("harness declares no permission policies; passing through".to_string()),
        };
    }

    let pick = |cond: &dyn Fn(PermissionPolicy) -> bool, rev: bool| -> Option<PolicySupport> {
        let mut candidates: Vec<PolicySupport> = caps
            .permission_policies
            .iter()
            .copied()
            .filter(|p| cond(p.policy))
            .collect();
        candidates.sort_by_key(|p| p.policy);
        if rev {
            candidates.last().copied()
        } else {
            candidates.first().copied()
        }
    };

    let chosen = caps
        .supports_policy(requested)
        .copied()
        .or_else(|| pick(&|p| p < requested, true))
        .or_else(|| pick(&|p| p > requested, false))
        .expect("non-empty policy list");

    let mut warning = None;
    if chosen.policy != requested {
        let direction = if chosen.policy < requested {
            "less"
        } else {
            "MORE"
        };
        warning = Some(format!(
            "policy '{}' not supported; using {} permissive '{}'",
            requested, direction, chosen.policy
        ));
    }
    if let Some(note) = chosen.degraded {
        warning = Some(match warning {
            Some(w) => format!("{} ({})", w, note),
            None => note.to_string(),
        });
    }

    PolicyResolution {
        requested,
        effective: chosen.policy,
        warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionPolicy::*;

    fn caps(policies: &[PolicySupport]) -> Capabilities {
        Capabilities {
            permission_policies: policies.to_vec(),
            ..Default::default()
        }
    }

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
        let c = caps(&[PolicySupport::full(Ask), PolicySupport::full(Bypass)]);
        let r = resolve_policy(&c, Bypass);
        assert_eq!(r.effective, Bypass);
        assert!(r.warning.is_none());
    }

    #[test]
    fn falls_back_to_less_permissive_first() {
        // Like Antigravity: Auto unsupported, AcceptEdits is.
        let c = caps(&[
            PolicySupport::full(Ask),
            PolicySupport::full(AcceptEdits),
            PolicySupport::full(Bypass),
        ]);
        let r = resolve_policy(&c, Auto);
        assert_eq!(r.effective, AcceptEdits);
        assert!(r.warning.unwrap().contains("less permissive"));
    }

    #[test]
    fn escalates_only_when_nothing_below() {
        let c = caps(&[PolicySupport::full(Auto)]);
        let r = resolve_policy(&c, Ask);
        assert_eq!(r.effective, Auto);
        assert!(r.warning.unwrap().contains("MORE permissive"));
    }

    #[test]
    fn degraded_note_is_surfaced() {
        // Like pi: Ask is supported but with a caveat.
        let c = caps(&[
            PolicySupport::degraded(Ask, "only extension dialogs prompt"),
            PolicySupport::full(Bypass),
        ]);
        let r = resolve_policy(&c, AcceptEdits);
        assert_eq!(r.effective, Ask);
        let w = r.warning.unwrap();
        assert!(w.contains("less permissive") && w.contains("only extension dialogs prompt"));

        let r2 = resolve_policy(&c, Ask);
        assert_eq!(r2.warning.as_deref(), Some("only extension dialogs prompt"));
    }
}
