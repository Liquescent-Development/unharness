//! The provider, model and effort each harness runs with, where each came
//! from, and when one may be chosen.

use super::{Action, Controller, Front, Origin, Pick};
use crate::core::{HarnessId, ModelInfo, ModelRef, ProviderId, SessionCommand};

/// What a harness runs with besides its policy.
#[derive(Debug, Clone, Default)]
pub struct ChoiceState {
    /// The provider: the user's choice (`Origin::User`) or the configured
    /// one (`Origin::Configured`), which are what the harness is told;
    /// otherwise a guess at the harness's own default, corrected by what
    /// its session reports (`Origin::Harness`).
    pub(super) provider: Option<Pick<ProviderId>>,
    /// The provider a session runs on when it is not the chosen one.
    pub(super) runs_on: Option<ProviderId>,
    /// The configured model (`Origin::Configured`), or one chosen in this
    /// run or for the conversation; only a chosen one is kept with the
    /// conversation.
    pub(super) model: Option<Pick<ModelRef>>,
    /// As `model`.
    pub(super) effort: Option<Pick<String>>,
}

impl ChoiceState {
    pub fn provider(&self) -> Option<&ProviderId> {
        self.provider.as_ref().map(|p| &p.value)
    }

    /// The provider the harness is told: one chosen or configured.
    pub fn chosen_provider(&self) -> Option<&ProviderId> {
        self.provider
            .as_ref()
            .filter(|p| p.origin != Origin::Harness)
            .map(|p| &p.value)
    }

    pub fn model(&self) -> Option<&ModelRef> {
        self.model.as_ref().map(|m| &m.value)
    }

    pub fn effort(&self) -> Option<&str> {
        self.effort.as_ref().map(|e| e.value.as_str())
    }

    /// The model, when it was chosen rather than configured: what the
    /// conversation keeps.
    pub(super) fn chosen_model(&self) -> Option<&ModelRef> {
        self.model
            .as_ref()
            .filter(|m| m.origin != Origin::Configured)
            .map(|m| &m.value)
    }

    /// As `chosen_model`.
    pub(super) fn chosen_effort(&self) -> Option<&String> {
        self.effort
            .as_ref()
            .filter(|e| e.origin != Origin::Configured)
            .map(|e| &e.value)
    }
}

/// A choice of a harness's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Model,
    Effort,
    Provider,
}

impl Choice {
    pub fn what(self) -> &'static str {
        match self {
            Choice::Model => "model",
            Choice::Effort => "effort",
            Choice::Provider => "provider",
        }
    }
}

/// A choice sent to a session that takes it only between turns, until the
/// session has said how it took it: prompts typed meanwhile are queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChoice {
    /// The prompts queued when it was sent: any after them were typed
    /// after it, and go when it has been taken.
    pub queued: usize,
    /// What was chosen: `model`, `effort`, `provider`.
    pub what: &'static str,
    /// A provider's line, which says how the session took it, waits for
    /// the answer.
    pub provider: Option<ProviderId>,
}

/// What is known of the active harness's session that decides whether
/// a choice may be made now, and how it reaches the session.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionNow {
    /// Its process is live.
    pub alive: bool,
    /// It was sent something: one started early has seen nothing of the
    /// conversation.
    pub prompted: bool,
    /// Its start waits for the CLI before it to be gone (`Ending`).
    pub start_held: bool,
    /// A turn runs.
    pub turn: bool,
    /// Subagents at work whose end will be told.
    pub subagents: usize,
    /// The conversation has a stored session of the harness.
    pub stored: bool,
    /// Prompts waiting to be sent.
    pub queued: usize,
}

/// How a choice just made reaches the session.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// It does not: no session is live (the next one starts with it), the
    /// live one was sent nothing and a provider is no reason to start it
    /// again (one the harness takes any time, or of a harness with one
    /// provider), or it is the provider it had.
    None,
    /// The stored session keeps what it ran, and no session prompted is
    /// there to say so: the conversation goes on in a new session, for
    /// this reason.
    Renew(&'static str),
    /// The live session was sent nothing yet: it is started again with
    /// the choice.
    Restart,
    /// The session takes it only between turns: it is sent, and what is
    /// typed after it waits until the session has said how it took it
    /// (`Controller::pending_answered`).
    Hold(SessionCommand),
    /// It is sent.
    Send(SessionCommand),
}

/// Each harness's provider, model and effort, the lists they are chosen
/// from, and when one may be chosen for the active harness's session.
impl Controller {
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
    pub(super) fn configured_model(&self, harness: HarnessId) -> Option<ModelRef> {
        let model = self
            .config
            .harness(harness.as_str())?
            .default_model
            .clone()?;
        let provider = self
            .provider(harness)
            .cloned()
            .unwrap_or_else(|| ProviderId::new("default"));
        Some(ModelRef::new(harness, provider, model))
    }

    /// Choose `provider` for `harness`. Another than it had takes the
    /// model chosen on the one before with it, here and in the
    /// conversation, and what its session ran on. Returns whether it is
    /// another.
    pub fn choose_provider(&mut self, harness: HarnessId, provider: ProviderId) -> bool {
        let state = self.choices.entry(harness).or_default();
        let changed = state.provider() != Some(&provider);
        if changed {
            state.runs_on = None;
            state.model = None;
            self.conversation.record.models.remove(&harness);
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
        let name = self.short_name_of(harness);
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
    pub fn route_provider(
        &mut self,
        harness: HarnessId,
        provider: ProviderId,
        now: &SessionNow,
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
        self.choose_provider(harness, provider.clone());
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
    pub fn route_model(
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
    pub fn route_effort(
        &mut self,
        harness: HarnessId,
        effort: String,
        now: &SessionNow,
    ) -> Result<(String, Route), String> {
        let caps = self.caps_of(harness).unwrap_or_default();
        if caps.effort_levels.is_empty() {
            return Err(format!(
                "{} does not expose reasoning effort",
                self.display_name_of(harness)
            ));
        }
        let effort = effort.to_lowercase();
        if !caps.supports_effort(&effort) {
            return Err(format!(
                "unknown effort '{}'; {} supports: {}",
                effort,
                self.short_name_of(harness),
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
    pub fn pending_answered(&mut self) -> Option<PendingChoice> {
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

/// Choosing the active harness's provider, model and effort, and what a
/// choice does to its session.
impl Controller {
    pub fn model_label(&self) -> String {
        match self.model(self.active) {
            Some(m) => m.label(),
            None => match self.provider(self.active) {
                Some(p) => format!("{p}/default"),
                None => "default".to_string(),
            },
        }
    }

    /// What decides whether a choice may be made now, and how it reaches
    /// the session (`Controller::route_provider` and the others).
    fn session_now(&self) -> SessionNow {
        SessionNow {
            alive: self.session_alive,
            prompted: self.session_seen(),
            start_held: self.start_held,
            turn: self.is_generating,
            subagents: self.awaited_subagents(),
            stored: self.conversation.session_ids.contains_key(&self.active),
            queued: self.queued.len(),
        }
    }

    /// Choose the active harness's provider (`Controller::route_provider`).
    pub fn set_provider(&mut self, provider: ProviderId, front: &mut dyn Front) {
        let now = self.session_now();
        let before = self.model_key(self.active);
        let route = match self.route_provider(self.active, provider.clone(), &now) {
            Ok(route) => route,
            Err(why) => {
                self.conversation.transcript.push_error(why);
                return;
            }
        };
        // Another provider takes the model chosen on the one before.
        if self.model_key(self.active) != before {
            self.fresh_context();
        }
        self.idle_start_paused = false;
        let line = provider_line(&provider);
        match route {
            Route::Renew(why) => {
                self.conversation.transcript.push_system(line);
                self.session_renewed(&format!("provider {provider}: {why}"), front);
            }
            Route::Restart => {
                self.conversation
                    .transcript
                    .push_system(restart_line(&provider, self.display_name()));
                self.shutdown_session(front);
            }
            // Its line says how the session took it, with its answer.
            Route::Hold(cmd) => self.choice_sent(cmd),
            Route::Send(cmd) => {
                self.conversation.transcript.push_system(line);
                self.actions.push_back(Action::Command(cmd));
            }
            Route::None => self.conversation.transcript.push_system(line),
        }
    }

    pub fn set_model(&mut self, model: String, front: &mut dyn Front) {
        let now = self.session_now();
        let before = self.model_key(self.active);
        let (m, route) = match self.route_model(self.active, model, &now) {
            Ok(set) => set,
            Err(why) => {
                self.conversation.transcript.push_error(why);
                return;
            }
        };
        if self.model_key(self.active) != before {
            self.fresh_context();
        }
        self.idle_start_paused = false;
        self.conversation
            .transcript
            .push_system(format!("Model: {}", m.label()));
        self.apply_choice(route, &format!("model {}", m.model), front);
    }

    pub fn set_effort(&mut self, effort: String, front: &mut dyn Front) {
        let now = self.session_now();
        let (effort, route) = match self.route_effort(self.active, effort, &now) {
            Ok(set) => set,
            Err(why) => {
                self.conversation.transcript.push_error(why);
                return;
            }
        };
        self.idle_start_paused = false;
        self.conversation
            .transcript
            .push_system(format!("Effort: {effort}"));
        self.apply_choice(route, &format!("effort {effort}"), front);
    }

    /// Carry out a model or effort just chosen (worded `choice`) as the
    /// controller routes it (`Controller::route`).
    fn apply_choice(&mut self, route: Route, choice: &str, front: &mut dyn Front) {
        match route {
            Route::Renew(why) => self.session_renewed(&format!("{choice}: {why}"), front),
            // The next session starts with it.
            Route::None => {}
            Route::Restart => self.shutdown_session(front),
            Route::Hold(cmd) => self.choice_sent(cmd),
            Route::Send(cmd) => self.actions.push_back(Action::Command(cmd)),
        }
    }

    /// Send `cmd`, a choice the session takes only between turns, whose
    /// answer the controller waits for (`Route::Hold`): what is typed after
    /// it is held until the session has said how it took it (`Restarted`,
    /// `SessionRenewed`). Its process may be started again by its driver:
    /// as before the TUI starts one, what the session changed since its
    /// last turn is told now, its harness is prepared, and the next process
    /// is watched from its start.
    fn choice_sent(&mut self, cmd: SessionCommand) {
        self.check_guard();
        match self.harness().prepare() {
            Ok(Some(note)) => self.conversation.transcript.push_notice(note),
            Ok(None) => {}
            Err(e) => self
                .conversation
                .transcript
                .push_error(format!("{}: {e:#}", self.short_name())),
        }
        self.arm_guard();
        self.actions.push_back(Action::Command(cmd));
    }

    /// The choice the session was sent has been answered, or will not be
    /// (its process is gone): what waited for the answer. A provider's line
    /// is said here, as `taken` words it.
    pub(super) fn choice_answered(
        &mut self,
        taken: impl FnOnce(&ProviderId) -> String,
    ) -> Option<PendingChoice> {
        let pending = self.pending_answered()?;
        if let Some(provider) = &pending.provider {
            self.conversation.transcript.push_system(taken(provider));
        }
        Some(pending)
    }

    /// The choice the session was sent will not be answered: it is ending,
    /// or gone. A stored session that keeps what it ran is forgotten, as
    /// its answer would have had it, so that it is not resumed as it ran;
    /// what was typed after the choice waits for Enter.
    pub(super) fn choice_unanswered(&mut self) {
        let Some(pending) = self.choice_answered(provider_line) else {
            return;
        };
        let stored = self.conversation.session_ids.contains_key(&self.active);
        if self.stored_session_keeps(self.active, stored).is_some() {
            self.forget_ids(self.active);
        }
        if self.queued.len() > pending.queued {
            self.conversation.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    /// The choice the session was sent is taken: the first prompt typed
    /// after it goes now, as it would have had it been typed then; those
    /// held before it still wait.
    pub(super) fn send_typed_after(&mut self, pending: &PendingChoice, front: &mut dyn Front) {
        if self.queued.len() > pending.queued {
            self.send_queued(pending.queued, front);
        }
    }
}

/// What is said of a provider chosen.
pub(super) fn provider_line(provider: &ProviderId) -> String {
    format!("Provider: {provider} (pick a model with Ctrl+M)")
}

/// What is said of a provider whose harness's process starts again on it.
pub(super) fn restart_line(provider: &ProviderId, name: &str) -> String {
    format!("Provider: {provider} ({name} restarts on it; pick a model with Ctrl+M)")
}
