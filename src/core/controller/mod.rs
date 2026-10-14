//! The session controller: what decides how each harness runs, apart from
//! what the TUI shows of it. Per harness: the permission policy asked for
//! and in effect ([`PolicyState`]), and what its live sessions reported on
//! top of its declared capabilities ([`Controller::caps_of`]).
//!
//! The TUI calls into it and reads from it; what it wants shown comes back
//! as values (notices, refusals) for the TUI to push to the transcript.
//! `headless` resolves its policy through the same [`PolicyState`].

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::core::conversations::Conversation;
use crate::core::registry::Registry;
use crate::core::{
    Capabilities, CapsUpdate, HarnessCommand, HarnessId, PermissionPolicy, PolicyResolution,
    PolicySupport, PolicyUnavailable,
};

mod policy;

pub use policy::{PolicySet, PolicySource, PolicyState, Refused, configured_policy};

/// Where a choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The configuration's default for the harness.
    Configured,
    /// Chosen for the conversation resumed.
    Conversation,
    /// Chosen in this run: on the command line, or in the TUI.
    User,
}

/// A value and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick<T> {
    pub value: T,
    pub origin: Origin,
}

impl<T> Pick<T> {
    pub fn new(value: T, origin: Origin) -> Self {
        Pick { value, origin }
    }
}

/// What a live session reported, as it changed what the TUI offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapsReport {
    /// It listed commands of its own, or said whether it runs them.
    pub commands: bool,
    /// It refused policies, and that moved the one in effect.
    pub refused: Option<Refused>,
}

/// The state that decides how each harness runs. See the module.
pub struct Controller {
    registry: Arc<Registry>,
    pub policy: PolicyState,
    /// What live sessions reported on top of the declared capabilities.
    live: HashMap<HarnessId, CapsUpdate>,
}

impl Controller {
    /// `policy`: the one named for every harness in this run, if any.
    pub fn new(registry: Arc<Registry>, policy: Option<PermissionPolicy>) -> Self {
        Controller {
            registry,
            policy: PolicyState::new(policy),
            live: HashMap::new(),
        }
    }

    /// Declared capabilities of `harness` plus what a session of it
    /// reported in this run (an ACP agent says at `initialize` whether it
    /// can fork); `None` for one not in the registry.
    pub fn caps_of(&self, harness: HarnessId) -> Option<Capabilities> {
        let mut caps = self.registry.get(harness)?.capabilities();
        if let Some(update) = self.live.get(&harness) {
            caps.apply(update);
        }
        Some(caps)
    }

    /// What `harness` offers of the policies, as its sessions left it.
    fn policies(&self, harness: HarnessId) -> Vec<PolicySupport> {
        self.caps_of(harness)
            .map(|c| c.permission_policies)
            .unwrap_or_default()
    }

    /// The display name of `harness`, for what is said of it.
    fn display_name(&self, harness: HarnessId) -> &'static str {
        self.registry
            .get(harness)
            .map_or("the harness", |h| h.descriptor().display_name)
    }

    fn short_name(&self, harness: HarnessId) -> &'static str {
        self.registry
            .get(harness)
            .map_or("the harness", |h| h.descriptor().short_name)
    }

    /// How the policy wanted of `harness` maps onto what it offers.
    pub fn policy_resolution(
        &self,
        config: &Config,
        harness: HarnessId,
    ) -> Result<PolicyResolution, PolicyUnavailable> {
        self.policy
            .resolve(config, harness, &self.policies(harness))
    }

    /// The policy `harness` runs under; `None` until the user chooses,
    /// where the wanted one is not available.
    pub fn effective_policy(
        &self,
        config: &Config,
        harness: HarnessId,
    ) -> Option<PermissionPolicy> {
        self.policy
            .effective(config, harness, &self.policies(harness))
    }

    /// The policies `harness` has, least permissive first.
    pub fn offered_policies(&self, harness: HarnessId) -> Vec<PermissionPolicy> {
        PolicyState::listed(&self.policies(harness))
    }

    /// How `p` resolves on `harness`, if it offers it.
    pub fn offered(
        &self,
        harness: HarnessId,
        p: PermissionPolicy,
    ) -> Result<PolicyResolution, PolicyUnavailable> {
        PolicyState::offered(&self.policies(harness), p)
    }

    /// Choose `p` for `harness` (`PolicyState::set`).
    pub fn set_policy(
        &mut self,
        config: &Config,
        harness: HarnessId,
        p: PermissionPolicy,
        prompted: bool,
    ) -> Result<PolicySet, PolicyUnavailable> {
        let policies = self.policies(harness);
        self.policy.set(config, harness, p, &policies, prompted)
    }

    /// `harness`'s session runs under `p` now (`PolicyState::on_changed`).
    /// Returns what the user is told when it moved there by itself.
    pub fn on_policy_changed(
        &mut self,
        config: &Config,
        harness: HarnessId,
        p: PermissionPolicy,
    ) -> Option<String> {
        let policies = self.policies(harness);
        let p = self.policy.on_changed(config, harness, p, &policies)?;
        let name = self.display_name(harness);
        Some(match p {
            PermissionPolicy::Plan => {
                format!(
                    "{name} went into plan mode: it asks you to approve its plan before it acts"
                )
            }
            _ => format!("{name} now runs under policy {p}"),
        })
    }

    /// The policies a plan of `harness` can be carried out under.
    pub fn plan_policies(&self, harness: HarnessId) -> Vec<PermissionPolicy> {
        self.policy.plan_policies(self.offered_policies(harness))
    }

    /// What a resumed conversation says of `harness`'s policy, when it runs
    /// under another than `last`, the one it last ran under. `at_start`:
    /// resumed by `--resume`.
    pub fn resumed_under(
        &self,
        config: &Config,
        harness: HarnessId,
        last: PermissionPolicy,
        at_start: bool,
    ) -> Option<String> {
        let now = self.effective_policy(config, harness);
        if now == Some(last) {
            return None;
        }
        let name = self.short_name(harness);
        let wanted = self.policy.wanted(config, harness);
        let why = if now != Some(wanted) {
            format!("{wanted} is not available on {name}")
        } else {
            match self.policy.source(harness) {
                PolicySource::ChosenInRun => "chosen in this run".to_string(),
                PolicySource::ChosenForConversation => {
                    format!("chosen for {name} in this conversation")
                }
                PolicySource::NamedInRun if at_start => "named with --policy".to_string(),
                PolicySource::NamedInRun => "named in this run".to_string(),
                PolicySource::NamedForConversation => {
                    "the policy named for this conversation".to_string()
                }
                PolicySource::Configured => "the configured default".to_string(),
            }
        };
        Some(match now {
            Some(now) => format!(
                "{name} last ran this conversation under {last}; it continues under {now} ({why})"
            ),
            None => format!(
                "{name} last ran this conversation under {last}; it waits for a policy ({why})"
            ),
        })
    }

    /// `harness`'s live session reported `update`. A policy it refused
    /// moves the one in effect as an unsupported policy would; a live
    /// session (`alive`) is told where it went.
    pub fn report(
        &mut self,
        config: &Config,
        harness: HarnessId,
        update: CapsUpdate,
        alive: bool,
    ) -> CapsReport {
        let commands = update.commands.is_some() || update.slash_commands.is_some();
        let refusal = update.refused_policies.is_some();
        let before = self.effective_policy(config, harness);
        self.live.entry(harness).or_default().merge(update);
        let refused = if refusal {
            let policies = self.policies(harness);
            self.policy
                .on_refused(config, harness, before, &policies, alive)
        } else {
            None
        };
        CapsReport { commands, refused }
    }

    /// `harness`'s session is gone. What it refused was for it
    /// (`CapsUpdate::refused_policies`: a mode that did not take, a
    /// profile shadowed): the next session is asked for the wanted policy
    /// again, and refuses it again if it still cannot hold it. The rest of
    /// what it reported is the agent's, kept until the next one says.
    /// Returns whether it had refused any.
    pub fn forget_refusals(&mut self, harness: HarnessId) -> bool {
        self.live
            .get_mut(&harness)
            .is_some_and(|u| u.refused_policies.take().is_some())
    }

    /// `harness`'s session refused a policy.
    pub fn refused_policies(&self, harness: HarnessId) -> bool {
        self.live
            .get(&harness)
            .and_then(|u| u.refused_policies.as_ref())
            .is_some_and(|r| !r.is_empty())
    }

    /// The commands of its own `harness`'s session listed.
    pub fn harness_commands(&self, harness: HarnessId) -> &[HarnessCommand] {
        self.live
            .get(&harness)
            .and_then(|u| u.commands.as_deref())
            .unwrap_or_default()
    }

    /// Whether `harness` has listed its commands, by a session or asked.
    pub fn commands_listed(&self, harness: HarnessId) -> bool {
        self.live
            .get(&harness)
            .is_some_and(|c| c.commands.is_some())
    }

    /// `harness` listed `commands` without a session.
    pub fn commands_arrived(&mut self, harness: HarnessId, commands: Vec<HarnessCommand>) {
        self.live.entry(harness).or_default().merge(CapsUpdate {
            slash_commands: Some(!commands.is_empty()),
            commands: Some(commands),
            ..Default::default()
        });
    }

    /// Take what a resumed `conversation` was run with, except where this
    /// run named its own.
    pub fn take_conversation(&mut self, conversation: &Conversation) {
        self.policy.take_conversation(conversation);
    }

    /// What the conversation keeps of the choices. `prompted`: `active`'s
    /// live session was sent something, so the policy in effect is the
    /// one it last ran under.
    pub fn sync_conversation(
        &self,
        config: &Config,
        active: HarnessId,
        prompted: bool,
        conversation: &mut Conversation,
    ) {
        self.policy.sync_conversation(conversation);
        if prompted && let Some(p) = self.effective_policy(config, active) {
            conversation.last_policies.insert(active, p);
        }
    }

    /// Take `update` as if a session of `harness` had reported it, without
    /// what a report sets off.
    #[cfg(test)]
    pub fn overlay(&mut self, harness: HarnessId, update: CapsUpdate) {
        self.live.entry(harness).or_default().merge(update);
    }
}

#[cfg(test)]
mod tests;
