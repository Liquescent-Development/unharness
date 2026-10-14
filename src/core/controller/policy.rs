//! The permission policy each harness is asked for and runs under: named,
//! configured, chosen, in effect, sent to the live session, and saved with
//! the conversation (`docs/permissions.md`, "Which policy a harness runs
//! under" and "Resuming a conversation").

use std::collections::{HashMap, VecDeque};

use anyhow::Result;

use super::{Action, Controller, Front, Origin, Pick, PolicyAsk};
use crate::config::Config;
use crate::core::conversations::Conversation;
use crate::core::{
    HarnessId, PermissionPolicy, PolicyResolution, PolicySupport, PolicyUnavailable,
    SessionCommand, resolve_policy,
};

/// The configured policy for `harness`: its own table's, the global one,
/// Ask. A harness's own default is for that harness only.
pub fn configured_policy(config: &Config, harness: HarnessId) -> Result<PermissionPolicy> {
    let configured = config
        .harness(harness.as_str())
        .and_then(|h| h.default_policy.as_deref())
        .or(config.default_policy.as_deref());
    match configured {
        Some(p) => PermissionPolicy::parse(p)
            .ok_or_else(|| anyhow::anyhow!("unknown policy '{p}' in config")),
        None => Ok(PermissionPolicy::Ask),
    }
}

/// The policy of every harness. The first of these that is set, for a
/// harness: what was chosen for it (`chosen`), the policy named for every
/// harness (`named`), its configured default (`configured_policy`).
#[derive(Debug, Clone, Default)]
pub struct PolicyState {
    /// The policy named for every harness: in this run (`--policy`,
    /// `Origin::User`), or for the conversation it resumed
    /// (`Origin::Conversation`). Without one each harness has its
    /// configured default.
    named: Option<Pick<PermissionPolicy>>,
    /// What was chosen for each harness, in this run (`Origin::User`: in
    /// the TUI, or a mode the session went into by itself) or for the
    /// conversation resumed (`Origin::Conversation`). It holds for that
    /// harness only, over the policy named for every harness.
    chosen: HashMap<HarnessId, Pick<PermissionPolicy>>,
    /// The policy each harness ran under before it went into plan mode:
    /// the one its plan approval offers first.
    pre_plan: HashMap<HarnessId, PermissionPolicy>,
    /// Policies sent to the live session whose report has not come back
    /// (`PolicyChanged`), oldest first.
    sent: VecDeque<PermissionPolicy>,
    /// The policy the live session's process was started under.
    in_session: Option<PermissionPolicy>,
}

/// How a policy just chosen goes (`PolicyState::set`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySet {
    /// What runs now: the policy chosen.
    pub effective: PermissionPolicy,
    /// What the harness says of it (a degraded policy).
    pub warning: Option<String>,
    /// No policy was in effect before: what waited for one goes now.
    pub was_waiting: bool,
    /// The live session is told the change.
    pub send: bool,
}

/// What a session's refusal of policies did to the one in effect
/// (`PolicyState::on_refused`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The policy in effect gave way to `now`, the nearest less permissive
    /// one there is, which the live session is told (`send`).
    GaveWay { now: PermissionPolicy, send: bool },
    /// Nothing at or below the wanted policy is left: the user chooses.
    Waits,
}

/// Why a harness runs under its policy, for a resumed conversation's
/// notice (`PolicyState::source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySource {
    ChosenInRun,
    ChosenForConversation,
    NamedInRun,
    NamedForConversation,
    Configured,
}

impl PolicyState {
    /// `named`: the policy named for every harness in this run, if any.
    pub fn new(named: Option<PermissionPolicy>) -> Self {
        PolicyState {
            named: named.map(|p| Pick::new(p, Origin::User)),
            ..Default::default()
        }
    }

    /// The policy named for every harness, if one was.
    pub fn named(&self) -> Option<PermissionPolicy> {
        self.named.as_ref().map(|p| p.value)
    }

    /// The policy asked for `harness` before any choice of its own: the
    /// one named for every harness, or its configured default (`ask` if
    /// the config no longer parses: it was checked at startup).
    pub fn requested(&self, config: &Config, harness: HarnessId) -> PermissionPolicy {
        self.requested_checked(config, harness)
            .unwrap_or(PermissionPolicy::Ask)
    }

    /// As `requested`, with a configured policy that does not parse an
    /// error (a run checks it before anything starts).
    pub fn requested_checked(
        &self,
        config: &Config,
        harness: HarnessId,
    ) -> Result<PermissionPolicy> {
        match self.named() {
            Some(p) => Ok(p),
            None => configured_policy(config, harness),
        }
    }

    /// The policy asked of `harness`: what was chosen for it, else the
    /// requested one.
    pub fn wanted(&self, config: &Config, harness: HarnessId) -> PermissionPolicy {
        self.chosen
            .get(&harness)
            .map(|p| p.value)
            .unwrap_or_else(|| self.requested(config, harness))
    }

    /// How the wanted policy maps onto `policies`, what `harness` offers.
    pub fn resolve(
        &self,
        config: &Config,
        harness: HarnessId,
        policies: &[PolicySupport],
    ) -> Result<PolicyResolution, PolicyUnavailable> {
        resolve_policy(policies, self.wanted(config, harness))
    }

    /// `None` until the user chooses, where the wanted policy is not
    /// available: a more permissive one is never taken for them.
    pub fn effective(
        &self,
        config: &Config,
        harness: HarnessId,
        policies: &[PolicySupport],
    ) -> Option<PermissionPolicy> {
        self.resolve(config, harness, policies)
            .ok()
            .map(|r| r.effective)
    }

    /// How `p` resolves on a harness offering `policies`, if it offers it.
    pub fn offered(
        policies: &[PolicySupport],
        p: PermissionPolicy,
    ) -> Result<PolicyResolution, PolicyUnavailable> {
        match resolve_policy(policies, p) {
            Ok(res) if res.effective == p => Ok(res),
            _ => Err(PolicyUnavailable {
                requested: p,
                supported: Self::listed(policies),
            }),
        }
    }

    /// The policies of `policies`, least permissive first.
    pub fn listed(policies: &[PolicySupport]) -> Vec<PermissionPolicy> {
        PermissionPolicy::ALL
            .into_iter()
            .filter(|p| policies.iter().any(|s| s.policy == *p))
            .collect()
    }

    /// Choose `p` for `harness`, which offers `policies`. Only a policy it
    /// has can be: what the user picks is what runs. For this harness
    /// only: the others keep theirs. A session that was prompted
    /// (`prompted`) is sent it as a change, so that each is reported back
    /// once.
    pub fn set(
        &mut self,
        config: &Config,
        harness: HarnessId,
        p: PermissionPolicy,
        policies: &[PolicySupport],
        prompted: bool,
    ) -> Result<PolicySet, PolicyUnavailable> {
        let res = Self::offered(policies, p)?;
        let before = self.effective(config, harness, policies);
        if res.effective == PermissionPolicy::Plan
            && let Some(was) = before
            && was != PermissionPolicy::Plan
        {
            self.pre_plan.insert(harness, was);
        }
        self.chosen.insert(harness, Pick::new(p, Origin::User));
        let send = prompted && before != Some(res.effective);
        if send {
            self.sent.push_back(res.effective);
        }
        Ok(PolicySet {
            effective: res.effective,
            warning: res.warning,
            was_waiting: before.is_none(),
            send,
        })
    }

    /// The session runs under `p` now. One it moved to by itself (the
    /// agent went into plan mode, or left a mode it could not keep)
    /// becomes the harness's policy, so that what is shown, saved and
    /// started again is what runs. Returns it then; `None` for the report
    /// of a change that was sent, maybe after a later one (the policy
    /// shown is already the newest choice), or of the policy in effect.
    pub fn on_changed(
        &mut self,
        config: &Config,
        harness: HarnessId,
        p: PermissionPolicy,
        policies: &[PolicySupport],
    ) -> Option<PermissionPolicy> {
        if let Some(at) = self.sent.iter().position(|s| *s == p) {
            self.sent.drain(..=at);
            return None;
        }
        let was = self.effective(config, harness, policies);
        if was == Some(p) {
            return None;
        }
        if p == PermissionPolicy::Plan
            && let Some(was) = was
        {
            self.pre_plan.insert(harness, was);
        }
        self.chosen.insert(harness, Pick::new(p, Origin::User));
        Some(p)
    }

    /// The session took policies out of what it offers (an ACP agent whose
    /// mode for one did not take; Codex come to `exec`, for the run):
    /// `before` was in effect, `policies` is what is left. One in effect
    /// gives way as an unsupported policy does: to the nearest less
    /// permissive one, which a live session (`alive`) is told, or, with
    /// none, to the user's choice. `None` when the policy in effect is not
    /// one of them.
    pub fn on_refused(
        &mut self,
        config: &Config,
        harness: HarnessId,
        before: Option<PermissionPolicy>,
        policies: &[PolicySupport],
        alive: bool,
    ) -> Option<Refused> {
        let now = self.effective(config, harness, policies);
        if now == before {
            return None;
        }
        Some(match now {
            Some(p) => {
                if alive {
                    self.sent.push_back(p);
                }
                Refused::GaveWay {
                    now: p,
                    send: alive,
                }
            }
            None => Refused::Waits,
        })
    }

    /// The policy `harness` ran under before it went into plan mode.
    pub fn pre_plan(&self, harness: HarnessId) -> Option<PermissionPolicy> {
        self.pre_plan.get(&harness).copied()
    }

    /// The policies a plan can be carried out under, of `offered`: but
    /// bypass only in a session started under it, since Claude refuses
    /// to switch to it in one that was not (`bypass_not_launched`, 2.1.296).
    pub fn plan_policies(&self, offered: Vec<PermissionPolicy>) -> Vec<PermissionPolicy> {
        let bypass = self.in_session == Some(PermissionPolicy::Bypass);
        offered
            .into_iter()
            .filter(|p| match p {
                PermissionPolicy::Plan => false,
                PermissionPolicy::Bypass => bypass,
                _ => true,
            })
            .collect()
    }

    /// The live session's process was started under `p`.
    pub fn started_under(&mut self, p: Option<PermissionPolicy>) {
        self.in_session = p;
    }

    /// The live session's process is ended: what was sent to it will not
    /// be reported back.
    pub fn process_ended(&mut self) {
        self.sent.clear();
    }

    /// A new conversation, or another one resumed: no harness plans in it.
    pub fn forget_plans(&mut self) {
        self.pre_plan.clear();
    }

    /// Take what the resumed `conversation` was run with, except where
    /// this run named its own: a policy named in this run for every
    /// harness holds over what the conversation chose for each, also when
    /// it is the one the conversation was named; what this run chose holds
    /// over both.
    pub fn take_conversation(&mut self, conversation: &Conversation) {
        let named_in_run = self.named.filter(|p| p.origin == Origin::User);
        let run: Vec<(HarnessId, Pick<PermissionPolicy>)> = self
            .chosen
            .iter()
            .filter(|(_, p)| p.origin == Origin::User)
            .map(|(h, p)| (*h, *p))
            .collect();
        self.named = named_in_run.or_else(|| {
            conversation
                .policy
                .map(|p| Pick::new(p, Origin::Conversation))
        });
        self.chosen = if named_in_run.is_none() {
            conversation
                .policy_choices
                .iter()
                .map(|(h, p)| (*h, Pick::new(*p, Origin::Conversation)))
                .collect()
        } else {
            HashMap::new()
        };
        self.chosen.extend(run);
    }

    /// What the conversation keeps of the policies.
    pub fn sync_conversation(&self, conversation: &mut Conversation) {
        conversation.policy = self.named();
        conversation.policy_choices = self.chosen.iter().map(|(h, p)| (*h, p.value)).collect();
    }

    /// Why `harness` runs under its wanted policy.
    pub fn source(&self, harness: HarnessId) -> PolicySource {
        match (self.chosen.get(&harness), self.named) {
            (Some(p), _) if p.origin == Origin::User => PolicySource::ChosenInRun,
            (Some(_), _) => PolicySource::ChosenForConversation,
            (None, Some(p)) if p.origin == Origin::User => PolicySource::NamedInRun,
            (None, Some(_)) => PolicySource::NamedForConversation,
            (None, None) => PolicySource::Configured,
        }
    }

    /// Whatever the harness had, `p` is chosen for it, as a resumed
    /// conversation's choice would be.
    #[cfg(test)]
    pub fn choose_for_conversation(&mut self, harness: HarnessId, p: PermissionPolicy) {
        self.chosen
            .insert(harness, Pick::new(p, Origin::Conversation));
    }

    /// What was chosen for each harness.
    #[cfg(test)]
    pub fn chosen(&self) -> HashMap<HarnessId, PermissionPolicy> {
        self.chosen.iter().map(|(h, p)| (*h, p.value)).collect()
    }
}

/// The policy of each harness as what it offers stands, with what its
/// sessions refused taken out.
impl Controller {
    /// How the policy wanted of `harness` maps onto what it offers.
    pub fn policy_resolution(
        &self,
        harness: HarnessId,
    ) -> Result<PolicyResolution, PolicyUnavailable> {
        self.policy
            .resolve(&self.config, harness, &self.policies(harness))
    }

    /// The policy `harness` runs under; `None` until the user chooses,
    /// where the wanted one is not available.
    pub fn effective_policy(&self, harness: HarnessId) -> Option<PermissionPolicy> {
        self.policy
            .effective(&self.config, harness, &self.policies(harness))
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
    pub fn route_policy(
        &mut self,
        harness: HarnessId,
        p: PermissionPolicy,
        prompted: bool,
    ) -> Result<PolicySet, PolicyUnavailable> {
        let policies = self.policies(harness);
        self.policy
            .set(&self.config, harness, p, &policies, prompted)
    }

    /// `harness`'s session runs under `p` now (`PolicyState::on_changed`).
    /// Returns what the user is told when it moved there by itself.
    pub fn policy_changed(&mut self, harness: HarnessId, p: PermissionPolicy) -> Option<String> {
        let policies = self.policies(harness);
        let p = self
            .policy
            .on_changed(&self.config, harness, p, &policies)?;
        let name = self.display_name_of(harness);
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
        harness: HarnessId,
        last: PermissionPolicy,
        at_start: bool,
    ) -> Option<String> {
        let now = self.effective_policy(harness);
        if now == Some(last) {
            return None;
        }
        let name = self.short_name_of(harness);
        let wanted = self.policy.wanted(&self.config, harness);
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
}

/// The active harness's policy as the user sees it, chooses it, and its
/// session takes or refuses it.
impl Controller {
    /// The effective policy for a message.
    pub fn policy_label(&self) -> String {
        match self.effective_policy(self.active) {
            Some(p) => p.to_string(),
            None => format!(
                "{} (unavailable)",
                self.policy.wanted(&self.config, self.active)
            ),
        }
    }

    pub fn policy_warning(&self) -> Option<String> {
        match self.policy_resolution(self.active) {
            Ok(res) => res.warning,
            Err(e) => Some(format!("{e} (Ctrl+P)")),
        }
    }

    /// Ask for a policy where the requested one is not available. Returns
    /// whether one is in effect.
    pub fn require_policy(&mut self, front: &mut dyn Front) -> bool {
        if self.effective_policy(self.active).is_some() {
            return true;
        }
        front.ask_policy(self.policy_ask());
        false
    }

    /// The policy picker for the active harness: every policy is listed;
    /// one the harness does not have cannot be chosen. The cursor starts on
    /// the wanted policy, else on the one in effect, else on the least
    /// permissive one there is.
    pub fn policy_ask(&self) -> PolicyAsk {
        let offered = self.offered_policies(self.active);
        let wanted = self.policy.wanted(&self.config, self.active);
        let start = if offered.contains(&wanted) {
            Some(wanted)
        } else {
            self.effective_policy(self.active)
        };
        PolicyAsk {
            harness: self.display_name(),
            offered,
            start,
            has_workspace: self.tracking.rules.has_workspace(),
        }
    }

    /// The active harness's session is gone, and what it refused with it
    /// (`Controller::drop_refusals`). A policy picker open on what the
    /// session offered is redone, or, where it waited for a policy and one
    /// is in effect again, closed: what waited for it is the user's to
    /// send.
    pub(super) fn forget_refusals(&mut self, front: &mut dyn Front) {
        let before = self.effective_policy(self.active);
        if !self.drop_refusals(self.active) {
            return;
        }
        if front.choosing_policy() {
            if before.is_none() && self.effective_policy(self.active).is_some() {
                front.close_policy_picker();
                if let Some(w) = self.policy_warning() {
                    self.conversation.transcript.push_notice(w);
                }
                if !self.queued.is_empty() {
                    self.conversation.transcript.push_notice(format!(
                        "{} queued prompt(s) held; press Enter to send the next",
                        self.queued.len()
                    ));
                }
            } else {
                front.ask_policy(self.policy_ask());
            }
        }
    }

    /// The active harness's session refused a policy.
    pub(super) fn session_refused(&self) -> bool {
        self.refused_policies(self.active)
    }

    /// Returns whether `p` could be set on the active harness. Only a
    /// policy it has can be: what the user picks is what runs.
    pub fn set_policy(&mut self, p: PermissionPolicy, front: &mut dyn Front) -> bool {
        let prompted = self.session_seen();
        let set = match self.route_policy(self.active, p, prompted) {
            Ok(set) => set,
            Err(e) => {
                self.conversation.transcript.push_error(e.to_string());
                return false;
            }
        };
        self.conversation
            .transcript
            .push_system(format!("Permission policy: {}", set.effective));
        if let Some(w) = set.warning {
            self.conversation.transcript.push_notice(w);
        }
        self.idle_start_paused = false;
        if self.session_alive && self.session_blank {
            // Started with the policy it had: started again with this one.
            self.shutdown_session(front);
        } else if set.send {
            self.actions
                .push_back(Action::Command(SessionCommand::SetPolicy(set.effective)));
        }
        // Only a prompt that waited for this choice is sent by it.
        if set.was_waiting {
            self.send_next_queued(front);
        }
        true
    }

    /// The session runs under `p` now; one it moved to by itself is said.
    pub(super) fn on_policy_changed(&mut self, p: PermissionPolicy) {
        if let Some(notice) = self.policy_changed(self.active, p) {
            self.conversation.transcript.push_notice(notice);
        }
    }

    /// The session took a policy out of what it offers (an ACP agent whose
    /// mode for it did not take; the driver said why), and the one in
    /// effect gave way (`PolicyState::on_refused`).
    pub(super) fn policy_refused(&mut self, refused: Refused, front: &mut dyn Front) {
        match refused {
            Refused::GaveWay { now, send } => {
                if let Some(w) = self.policy_warning() {
                    self.conversation.transcript.push_notice(w);
                }
                if send {
                    self.actions
                        .push_back(Action::Command(SessionCommand::SetPolicy(now)));
                }
            }
            Refused::Waits => {
                self.conversation.transcript.push_notice(format!(
                    "{} waits for another policy (Ctrl+P)",
                    self.display_name()
                ));
                self.require_policy(front);
            }
        }
    }
}
