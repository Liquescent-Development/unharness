//! The session controller: what decides how each harness runs, apart from
//! what the TUI shows of it. Per harness: the permission policy asked for
//! and in effect ([`PolicyState`]), the provider, model and effort chosen
//! and where each came from ([`ChoiceState`]) with the lists they are
//! chosen from, and what its live sessions reported on top of its declared
//! capabilities ([`Controller::caps_of`]). For the active harness's
//! session: when a choice may be made, how it reaches the session
//! ([`Route`]), and a choice sent that the session has not said how it took
//! ([`PendingChoice`]), which holds back what follows it. And the
//! conversation it runs ([`ConversationState`]): installed as one, saved,
//! and its file checkpoints.
//!
//! The TUI calls into it and reads from it. What it says of the
//! conversation it holds (a save that failed, a checkpoint that could not
//! be taken, a resumed conversation's choices, a file restore undone) it
//! puts in that conversation's transcript itself; what it says of a choice
//! or a policy comes back as values (notices, refusals) for the TUI to
//! push there. `headless` resolves its policy through the same
//! [`PolicyState`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::config::Config;
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{Conversation, ConversationStore};
use crate::core::registry::Registry;
use crate::core::{
    Capabilities, CapsUpdate, HarnessCommand, HarnessId, ModelInfo, ModelRef, PermissionPolicy,
    PolicySupport, ProviderId,
};

mod choices;
mod conversation;
mod policy;

pub use choices::{Choice, ChoiceState, PendingChoice, Route, SessionNow, subagents_refusal};
pub use conversation::{ConversationState, Keeping};
pub use policy::{PolicySet, PolicySource, PolicyState, Refused, configured_policy};

/// Where a choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The harness's own: a guess at its default, or what its session
    /// reported.
    Harness,
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
    /// What the user is told of it, in order.
    pub notices: Vec<String>,
    /// It listed commands of its own, or said whether it runs them.
    pub commands: bool,
    /// It refused policies, and that moved the one in effect.
    pub refused: Option<Refused>,
}

/// What a run named on its command line, for the harness it starts on.
#[derive(Debug, Clone)]
pub struct Launch {
    pub harness: HarnessId,
    /// The policy named for every harness.
    pub policy: Option<PermissionPolicy>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Each harness's guess at its own provider (`Harness::default_provider`),
    /// which reads the user's vendor configuration.
    pub default_providers: HashMap<HarnessId, ProviderId>,
}

/// The state that decides how each harness runs. See the module.
pub struct Controller {
    registry: Arc<Registry>,
    pub policy: PolicyState,
    choices: HashMap<HarnessId, ChoiceState>,
    /// The models each harness offers on a provider, as last listed.
    model_lists: HashMap<(HarnessId, String), Vec<ModelInfo>>,
    /// The providers each harness offers, as last listed.
    provider_lists: HashMap<HarnessId, Vec<(ProviderId, String)>>,
    /// What live sessions reported on top of the declared capabilities.
    live: HashMap<HarnessId, CapsUpdate>,
    /// A choice sent to the active harness's session that it takes only
    /// between turns, until it has said how it took it: prompts typed
    /// meanwhile wait, and another choice and what would end the session
    /// are refused.
    pending: Option<PendingChoice>,
    /// The conversation it runs, installed as one (`Controller::install`).
    pub conversation: ConversationState,
    /// Where the workspace's conversations are kept.
    pub store: ConversationStore,
    /// Saving it failed, which is said once per run.
    persist_failed: bool,
    /// File checkpoints of the workspace; `None` outside a git repository
    /// or when switched off.
    checkpoints: Option<Checkpoints>,
    checkpoint_failed: bool,
    restore_seq: usize,
}

impl Controller {
    /// Every harness starts with its configured provider, else its guess at
    /// its own, and its configured model and effort; the run's harness with
    /// what `launch` names. A configured model of a harness whose provider
    /// is not known is given to the run's harness only. It runs a new
    /// conversation, kept and checkpointed as `keeping` says.
    pub fn new(registry: Arc<Registry>, config: &Config, launch: Launch, keeping: Keeping) -> Self {
        let mut choices: HashMap<HarnessId, ChoiceState> = HashMap::new();
        for h in registry.all() {
            let id = h.descriptor().id;
            let settings = config.harness(id.as_str());
            let provider = match settings.and_then(|s| s.default_provider.clone()) {
                Some(p) => Some(Pick::new(ProviderId::new(p), Origin::Configured)),
                None => launch
                    .default_providers
                    .get(&id)
                    .map(|p| Pick::new(p.clone(), Origin::Harness)),
            };
            let state = choices.entry(id).or_default();
            if let (Some(p), Some(m)) = (&provider, settings.and_then(|s| s.default_model.clone()))
            {
                state.model = Some(Pick::new(
                    ModelRef::new(id, p.value.clone(), m),
                    Origin::Configured,
                ));
            }
            if let Some(e) = settings.and_then(|s| s.default_effort.clone()) {
                state.effort = Some(Pick::new(e, Origin::Configured));
            }
            state.provider = provider;
        }
        let state = choices.entry(launch.harness).or_default();
        if let Some(p) = launch.provider {
            state.provider = Some(Pick::new(ProviderId::new(p), Origin::User));
        }
        // `launch.model` is only a flag: the configured model of a harness
        // whose provider is not known yet is still given to it.
        let configured_model = config
            .harness(launch.harness.as_str())
            .and_then(|s| s.default_model.clone());
        if let Some(m) = launch
            .model
            .clone()
            .or_else(|| configured_model.filter(|_| state.model.is_none()))
        {
            let origin = if launch.model.is_some() {
                Origin::User
            } else {
                Origin::Configured
            };
            let p = state
                .provider()
                .cloned()
                .unwrap_or_else(|| ProviderId::new("default"));
            state.model = Some(Pick::new(ModelRef::new(launch.harness, p, m), origin));
        }
        if let Some(e) = launch.effort {
            state.effort = Some(Pick::new(e, Origin::User));
        }
        Controller {
            registry,
            policy: PolicyState::new(launch.policy),
            choices,
            model_lists: HashMap::new(),
            provider_lists: HashMap::new(),
            live: HashMap::new(),
            pending: None,
            conversation: ConversationState::new(Conversation::new(launch.harness), launch.harness),
            store: keeping.store,
            persist_failed: false,
            checkpoints: keeping.checkpoints,
            checkpoint_failed: false,
            restore_seq: 0,
        }
    }

    /// `harness`'s session process is ended: what was sent to it will not
    /// be reported back, and the next one reports where it runs.
    pub fn process_ended(&mut self, harness: HarnessId) {
        self.policy.process_ended();
        if let Some(state) = self.choices.get_mut(&harness) {
            state.runs_on = None;
        }
    }

    /// The harnesses whose capabilities it reads.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
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
        let mut notices = Vec::new();
        if let Some(levels) = &update.effort_levels
            && self
                .caps_of(harness)
                .is_some_and(|c| c.effort_levels != *levels)
        {
            notices.push(format!("effort levels now: {}", levels.join(", ")));
        }
        // A guess at the harness's own default gives way to what it
        // reports. A choice stays the user's, and is what the next session
        // is told; the driver reported the mismatch and the status line
        // keeps showing it.
        if let Some(reported) = &update.provider {
            let name = self.short_name(harness);
            let state = self.choices.entry(harness).or_default();
            if state.chosen_provider().is_some() {
                state.runs_on = (state.provider() != Some(reported)).then(|| reported.clone());
            } else if state.provider() != Some(reported) {
                notices.push(format!(
                    "{name} runs on {reported}, chosen by its own configuration"
                ));
                state.provider = Some(Pick::new(reported.clone(), Origin::Harness));
                if let Some(m) = state.model.as_mut() {
                    m.value.provider = reported.clone();
                }
            }
        }
        if let Some(models) = &update.models {
            let provider = models
                .first()
                .map(|m| m.model_ref.provider.0.clone())
                .or_else(|| self.provider(harness).map(|p| p.0.clone()))
                .unwrap_or_else(|| "default".into());
            self.model_lists.insert((harness, provider), models.clone());
        }
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
        CapsReport {
            notices,
            commands,
            refused,
        }
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

    /// Take what a resumed `conversation` was run with, its policy, models
    /// and efforts, except where this run named its own. What the last
    /// conversation chose goes back to the configured default. Returns
    /// what the user is told, in order.
    pub fn take_conversation(
        &mut self,
        config: &Config,
        conversation: &Conversation,
    ) -> Vec<String> {
        self.policy.take_conversation(conversation);
        let harnesses: Vec<HarnessId> = self.choices.keys().copied().collect();
        for h in harnesses {
            let model = self.configured_model(config, h);
            let effort = config
                .harness(h.as_str())
                .and_then(|s| s.default_effort.clone());
            let Some(state) = self.choices.get_mut(&h) else {
                continue;
            };
            if state
                .model
                .as_ref()
                .is_some_and(|m| m.origin == Origin::Conversation)
            {
                state.model = model.map(|m| Pick::new(m, Origin::Configured));
            }
            if state
                .effort
                .as_ref()
                .is_some_and(|e| e.origin == Origin::Conversation)
            {
                state.effort = effort.map(|e| Pick::new(e, Origin::Configured));
            }
        }

        let mut notices = Vec::new();
        let mut models: Vec<(HarnessId, ModelRef)> = conversation
            .models
            .iter()
            .map(|(h, m)| (*h, m.clone()))
            .collect();
        models.sort_by_key(|(h, _)| h.as_str());
        // An effort is for the model it was chosen with.
        let mut left = HashSet::new();
        for (h, m) in models {
            let Some(harness) = self.registry.get(h) else {
                continue;
            };
            let state = self.choices.entry(h).or_default();
            if state
                .model
                .as_ref()
                .is_some_and(|m| m.origin == Origin::User)
            {
                continue;
            }
            match state.provider() {
                Some(p) if *p != m.provider => {
                    let name = harness.descriptor().short_name;
                    notices.push(format!(
                        "the model chosen for {name} in this conversation ({}) is not used: {name} is on {p} now",
                        m.label(),
                    ));
                    left.insert(h);
                }
                _ => state.model = Some(Pick::new(m, Origin::Conversation)),
            }
        }
        for (h, e) in &conversation.efforts {
            if left.contains(h) || self.registry.get(*h).is_none() {
                continue;
            }
            let state = self.choices.entry(*h).or_default();
            if state
                .effort
                .as_ref()
                .is_some_and(|e| e.origin == Origin::User)
            {
                continue;
            }
            state.effort = Some(Pick::new(e.clone(), Origin::Conversation));
        }
        notices
    }

    /// What the conversation keeps of the choices. `prompted`: `active`'s
    /// live session was sent something, so the policy in effect is the
    /// one it last ran under.
    fn sync_choices(&mut self, config: &Config, active: HarnessId, prompted: bool) {
        let last = if prompted {
            self.effective_policy(config, active)
        } else {
            None
        };
        let conversation = &mut self.conversation.record;
        self.policy.sync_conversation(conversation);
        for (h, state) in &self.choices {
            if let Some(m) = state.chosen_model() {
                conversation.models.insert(*h, m.clone());
            }
            if let Some(e) = state.chosen_effort() {
                conversation.efforts.insert(*h, e.clone());
            }
        }
        if let Some(p) = last {
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
