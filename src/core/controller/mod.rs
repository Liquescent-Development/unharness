//! The session controller: what decides how each harness runs, apart from
//! what the TUI shows of it. Per harness: the permission policy asked for
//! and in effect ([`PolicyState`]), the provider, model and effort chosen
//! and where each came from ([`ChoiceState`]) with the lists they are
//! chosen from, and what its live sessions reported on top of its declared
//! capabilities ([`Controller::caps_of`]). For the active harness's
//! session: when a choice may be made, how it reaches the session
//! ([`Route`]), and a choice sent that the session has not said how it took
//! ([`PendingChoice`]), which holds back what follows it.
//!
//! The TUI calls into it and reads from it; what it wants shown comes back
//! as values (notices, refusals) for the TUI to push to the transcript.
//! `headless` resolves its policy through the same [`PolicyState`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::config::Config;
use crate::core::conversations::Conversation;
use crate::core::registry::Registry;
use crate::core::{
    Capabilities, CapsUpdate, HarnessCommand, HarnessId, ModelInfo, ModelRef, PermissionPolicy,
    PolicyResolution, PolicySupport, PolicyUnavailable, ProviderId, SessionCommand,
};

mod choices;
mod policy;

pub use choices::{Choice, ChoiceState, PendingChoice, Route, SessionNow};
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
}

impl Controller {
    /// Every harness starts with its configured provider, else its guess at
    /// its own, and its configured model and effort; the run's harness with
    /// what `launch` names. A configured model of a harness whose provider
    /// is not known is given to the run's harness only.
    pub fn new(registry: Arc<Registry>, config: &Config, launch: Launch) -> Self {
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
        }
    }

    /// What `harness` runs with besides its policy.
    pub fn choices(&self, harness: HarnessId) -> Option<&ChoiceState> {
        self.choices.get(&harness)
    }

    pub fn provider(&self, harness: HarnessId) -> Option<&ProviderId> {
        self.choices(harness)?.provider()
    }

    /// The provider to tell `harness`, if the user chose one.
    pub fn chosen_provider(&self, harness: HarnessId) -> Option<&ProviderId> {
        self.choices(harness)?.chosen_provider()
    }

    /// The provider `harness`'s session runs on when it is not the chosen
    /// one.
    pub fn runs_on(&self, harness: HarnessId) -> Option<&ProviderId> {
        self.choices(harness)?.runs_on.as_ref()
    }

    pub fn model(&self, harness: HarnessId) -> Option<&ModelRef> {
        self.choices(harness)?.model()
    }

    pub fn effort(&self, harness: HarnessId) -> Option<&str> {
        self.choices(harness)?.effort()
    }

    /// The model the config names for `harness`, on its provider.
    fn configured_model(&self, config: &Config, harness: HarnessId) -> Option<ModelRef> {
        let model = config.harness(harness.as_str())?.default_model.clone()?;
        let provider = self
            .provider(harness)
            .cloned()
            .unwrap_or_else(|| ProviderId::new("default"));
        Some(ModelRef::new(harness, provider, model))
    }

    /// Choose `provider` for `harness`. Another than it had takes the
    /// model chosen on the one before with it, here and in `conversation`,
    /// and what its session ran on. Returns whether it is another.
    pub fn choose_provider(
        &mut self,
        harness: HarnessId,
        provider: ProviderId,
        conversation: &mut Conversation,
    ) -> bool {
        let state = self.choices.entry(harness).or_default();
        let changed = state.provider() != Some(&provider);
        if changed {
            state.runs_on = None;
            state.model = None;
            conversation.models.remove(&harness);
        }
        state.provider = Some(Pick::new(provider, Origin::User));
        changed
    }

    /// Choose `model` for `harness`, on its provider.
    pub fn choose_model(&mut self, harness: HarnessId, model: String) -> ModelRef {
        let provider = self
            .provider(harness)
            .cloned()
            .unwrap_or_else(|| ProviderId::new("default"));
        let m = ModelRef::new(harness, provider, model);
        self.choices.entry(harness).or_default().model = Some(Pick::new(m.clone(), Origin::User));
        m
    }

    /// Choose `effort` for `harness`.
    pub fn choose_effort(&mut self, harness: HarnessId, effort: String) {
        self.choices.entry(harness).or_default().effort = Some(Pick::new(effort, Origin::User));
    }

    /// Whether `harness` takes `choice` only between turns, with no
    /// subagent whose end will be told at work: the one reader of
    /// `Capabilities::between_turns`.
    pub fn between_turns(&self, harness: HarnessId, choice: Choice) -> bool {
        let Some(between) = self.caps_of(harness).map(|c| c.between_turns) else {
            return false;
        };
        match choice {
            Choice::Model => between.model,
            Choice::Effort => between.effort,
            Choice::Provider => between.provider,
        }
    }

    /// Why `harness`'s stored session cannot take a new model, effort or
    /// provider, if the conversation has one (`stored`): the one reader of
    /// `Capabilities::stored_session_keeps`.
    pub fn stored_session_keeps(&self, harness: HarnessId, stored: bool) -> Option<&'static str> {
        self.caps_of(harness)?
            .stored_session_keeps
            .filter(|_| stored)
    }

    /// Why a choice leaves the stored session for a new one now, with no
    /// session to say so: none is live, or the live one was sent nothing
    /// (it would be started again on the stored session, as it ran). A
    /// session that was prompted says it itself (`SessionRenewed`).
    fn renewal_without_session(
        &self,
        harness: HarnessId,
        now: &SessionNow,
    ) -> Option<&'static str> {
        if now.prompted {
            return None;
        }
        self.stored_session_keeps(harness, now.stored)
    }

    /// Why nothing that would start, end or change `harness`'s session is
    /// made now, if so: its start is held until the CLI before it is gone
    /// (`start_held`), or it was sent a choice it takes only between turns
    /// and has not said how it took it (which may leave it for a new
    /// session, or start its CLI again).
    pub fn settling(&self, harness: HarnessId, start_held: bool) -> Option<String> {
        let name = self.short_name(harness);
        if start_held {
            return Some(format!(
                "the previous {name} is still exiting; try again in a moment"
            ));
        }
        self.pending.as_ref().map(|pending| {
            format!(
                "{name} is still taking the {} just chosen; try again in a moment",
                pending.what
            )
        })
    }

    /// Why `choice`, which `harness` takes only between turns, is refused
    /// now, if it is. Not during a turn, nor while subagents whose end will
    /// be told are at work (they would end with the session or its
    /// process), nor, for a stored session that keeps what it ran, while
    /// the CLI before is still exiting (the start that waits for it resumes
    /// that session). (No choice is made while the session has not said how
    /// it took the one before, `pending`: it may be leaving for a new
    /// session, or starting its CLI again, which would take an effort sent
    /// meanwhile as a turn.)
    fn refused_between_turns(
        &self,
        harness: HarnessId,
        choice: Choice,
        now: &SessionNow,
    ) -> Option<String> {
        let what = choice.what();
        if now.start_held && self.stored_session_keeps(harness, now.stored).is_some() {
            return self.settling(harness, true);
        }
        if !now.alive {
            return None;
        }
        if now.turn {
            return Some(format!(
                "finish or interrupt the current turn before changing the {what}"
            ));
        }
        // They would end with the process.
        let stop = self.caps_of(harness).is_some_and(|c| c.subagents.stop);
        (now.subagents > 0)
            .then(|| subagents_refusal(now.subagents, stop, &format!("changing the {what}")))
    }

    /// Why no choice is made now while one is pending, if one is.
    fn refused_while_pending(&self, harness: HarnessId, now: &SessionNow) -> Option<String> {
        self.pending
            .as_ref()
            .and_then(|_| self.settling(harness, now.start_held))
    }

    /// Choose `provider` for `harness`, if it may be chosen now (else why
    /// not), and say how it reaches the session. A session that takes it
    /// only between turns is told, and says how it took it (`Route::Hold`);
    /// where it is the process's, a session nothing was sent to is started
    /// again on it; a stored session that keeps what it ran is left for a
    /// new one.
    pub fn set_provider(
        &mut self,
        harness: HarnessId,
        provider: ProviderId,
        now: &SessionNow,
        conversation: &mut Conversation,
    ) -> Result<Route, String> {
        if let Some(why) = self.refused_while_pending(harness, now) {
            return Err(why);
        }
        let changed = self.provider(harness) != Some(&provider);
        let between = self.between_turns(harness, Choice::Provider);
        if changed
            && between
            && let Some(why) = self.refused_between_turns(harness, Choice::Provider, now)
        {
            return Err(why);
        }
        self.choose_provider(harness, provider.clone(), conversation);
        let multi = self.caps_of(harness).is_some_and(|c| c.multi_provider);
        Ok(if !changed {
            Route::None
        } else if let Some(why) = self.renewal_without_session(harness, now) {
            Route::Renew(why)
        } else if now.alive && !now.prompted && between && multi {
            // Nothing was sent to it: it starts again on the provider. (A
            // harness of one provider is not, for a name it does not know.)
            Route::Restart
        } else if now.prompted && between {
            self.hold(Choice::Provider, Some(provider.clone()), now);
            Route::Hold(SessionCommand::SetProvider(provider))
        } else if now.prompted {
            Route::Send(SessionCommand::SetProvider(provider))
        } else {
            Route::None
        })
    }

    /// Choose `model` for `harness`, if it may be chosen now (else why
    /// not); returns it and how it reaches the session (`Controller::route`).
    pub fn set_model(
        &mut self,
        harness: HarnessId,
        model: String,
        now: &SessionNow,
    ) -> Result<(ModelRef, Route), String> {
        if let Some(why) = self.refused_while_pending(harness, now) {
            return Err(why);
        }
        let between = self.between_turns(harness, Choice::Model);
        if between && let Some(why) = self.refused_between_turns(harness, Choice::Model, now) {
            return Err(why);
        }
        let m = self.choose_model(harness, model);
        let route = self.route(
            harness,
            between,
            SessionCommand::SetModel(m.clone()),
            Choice::Model,
            now,
        );
        Ok((m, route))
    }

    /// Choose `effort` for `harness`, one of the levels it has, if it may be
    /// chosen now (else why not); returns it and how it reaches the session
    /// (`Controller::route`).
    pub fn set_effort(
        &mut self,
        harness: HarnessId,
        effort: String,
        now: &SessionNow,
    ) -> Result<(String, Route), String> {
        let caps = self.caps_of(harness).unwrap_or_default();
        if caps.effort_levels.is_empty() {
            return Err(format!(
                "{} does not expose reasoning effort",
                self.display_name(harness)
            ));
        }
        let effort = effort.to_lowercase();
        if !caps.supports_effort(&effort) {
            return Err(format!(
                "unknown effort '{}'; {} supports: {}",
                effort,
                self.short_name(harness),
                caps.effort_levels.join(", ")
            ));
        }
        if let Some(why) = self.refused_while_pending(harness, now) {
            return Err(why);
        }
        let between = self.between_turns(harness, Choice::Effort);
        if between && let Some(why) = self.refused_between_turns(harness, Choice::Effort, now) {
            return Err(why);
        }
        self.choose_effort(harness, effort.clone());
        let route = self.route(
            harness,
            between,
            SessionCommand::SetEffort(Some(effort.clone())),
            Choice::Effort,
            now,
        );
        Ok((effort, route))
    }

    /// How a model or effort just chosen (`cmd`) reaches the session, which
    /// takes it only `between` turns or any time: a stored session that
    /// keeps what it ran is left for a new one; one nothing was sent to is
    /// started again with it (Claude would take an effort as a turn); one
    /// prompted is told, and says how it took it.
    fn route(
        &mut self,
        harness: HarnessId,
        between: bool,
        cmd: SessionCommand,
        choice: Choice,
        now: &SessionNow,
    ) -> Route {
        if let Some(why) = self.renewal_without_session(harness, now) {
            Route::Renew(why)
        } else if !now.alive {
            // The next session starts with it.
            Route::None
        } else if !now.prompted {
            Route::Restart
        } else if between {
            self.hold(choice, None, now);
            Route::Hold(cmd)
        } else {
            Route::Send(cmd)
        }
    }

    /// `choice` goes to the session, which says how it took it before what
    /// is typed after it goes.
    fn hold(&mut self, choice: Choice, provider: Option<ProviderId>, now: &SessionNow) {
        self.pending = Some(PendingChoice {
            queued: now.queued,
            what: choice.what(),
            provider,
        });
    }

    /// A choice was sent that the session has not said how it took.
    pub fn choice_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The choice the session was sent has been answered, or will not be
    /// (its process is gone): what waited for the answer.
    pub fn choice_answered(&mut self) -> Option<PendingChoice> {
        self.pending.take()
    }

    /// The models `harness` offers on `provider`, if they were listed.
    pub fn model_list(&self, harness: HarnessId, provider: &ProviderId) -> Option<&Vec<ModelInfo>> {
        self.model_lists.get(&(harness, provider.0.clone()))
    }

    pub fn keep_models(&mut self, harness: HarnessId, provider: String, models: Vec<ModelInfo>) {
        self.model_lists.insert((harness, provider), models);
    }

    /// The providers `harness` offers, if they were listed.
    pub fn provider_list(&self, harness: HarnessId) -> Option<&Vec<(ProviderId, String)>> {
        self.provider_lists.get(&harness)
    }

    pub fn keep_providers(&mut self, harness: HarnessId, providers: Vec<(ProviderId, String)>) {
        self.provider_lists.insert(harness, providers);
    }

    /// `harness`'s session process is ended: what was sent to it will not
    /// be reported back, and the next one reports where it runs.
    pub fn process_ended(&mut self, harness: HarnessId) {
        self.policy.process_ended();
        if let Some(state) = self.choices.get_mut(&harness) {
            state.runs_on = None;
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
    pub fn sync_conversation(
        &self,
        config: &Config,
        active: HarnessId,
        prompted: bool,
        conversation: &mut Conversation,
    ) {
        self.policy.sync_conversation(conversation);
        for (h, state) in &self.choices {
            if let Some(m) = state.chosen_model() {
                conversation.models.insert(*h, m.clone());
            }
            if let Some(e) = state.chosen_effort() {
                conversation.efforts.insert(*h, e.clone());
            }
        }
        if prompted && let Some(p) = self.effective_policy(config, active) {
            conversation.last_policies.insert(active, p);
        }
    }

    /// As if `harness`'s own default were `provider`, or a provider chosen
    /// for it now `provider`.
    #[cfg(test)]
    pub fn put_provider(&mut self, harness: HarnessId, provider: ProviderId) {
        let state = self.choices.entry(harness).or_default();
        let origin = state
            .provider
            .as_ref()
            .map_or(Origin::Harness, |p| p.origin);
        state.provider = Some(Pick::new(provider, origin));
    }

    /// Forget the model lists, as if none had been listed.
    #[cfg(test)]
    pub fn forget_model_lists(&mut self) {
        self.model_lists.clear();
    }

    /// Take `update` as if a session of `harness` had reported it, without
    /// what a report sets off.
    #[cfg(test)]
    pub fn overlay(&mut self, harness: HarnessId, update: CapsUpdate) {
        self.live.entry(harness).or_default().merge(update);
    }
}

/// Why what would end a session with `n` subagents at work, whose end will
/// be told, is refused for `doing`: they would end with it. Stopping them
/// is named only where the harness can (`can_stop`).
pub fn subagents_refusal(n: usize, can_stop: bool, doing: &str) -> String {
    let stop = if can_stop {
        " or stop them (Ctrl+S)"
    } else {
        ""
    };
    format!("{n} subagent(s) still at work would be stopped; wait for them{stop} before {doing}")
}

#[cfg(test)]
mod tests;
