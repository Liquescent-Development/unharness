//! Providers, models and efforts, and the lists they are chosen from.

use super::{Action, App, ListJob, ListRequest, ListResult};
use crate::core::{HarnessId, ModelRef, ProviderId, SessionCommand};
use crate::harness::ModelInfo;
use crate::tui::modal::{ListPicker, Modal, ProviderOption};

impl App {
    /// The model the config names for `harness`.
    pub(super) fn configured_model(&self, harness: HarnessId) -> Option<ModelRef> {
        let model = self
            .config
            .harness(harness.as_str())?
            .default_model
            .clone()?;
        let provider = self
            .providers
            .get(&harness)
            .cloned()
            .unwrap_or_else(|| ProviderId::new("default"));
        Some(ModelRef::new(harness, provider, model))
    }

    pub(super) fn configured_effort(&self, harness: HarnessId) -> Option<String> {
        self.config
            .harness(harness.as_str())?
            .default_effort
            .clone()
    }

    pub fn current_provider(&self) -> Option<&ProviderId> {
        self.providers.get(&self.active)
    }

    /// The provider to tell the active harness, if the user chose one.
    pub fn chosen_provider(&self) -> Option<&ProviderId> {
        self.chosen_providers
            .contains(&self.active)
            .then(|| self.current_provider())
            .flatten()
    }

    pub fn current_model(&self) -> Option<&ModelRef> {
        self.models.get(&self.active)
    }

    pub fn current_effort(&self) -> Option<&str> {
        self.efforts.get(&self.active).map(String::as_str)
    }

    pub fn model_label(&self) -> String {
        match self.current_model() {
            Some(m) => m.label(),
            None => match self.current_provider() {
                Some(p) => format!("{p}/default"),
                None => "default".to_string(),
            },
        }
    }

    /// Ask the active harness for its commands without a session, once,
    /// where the session would not say before the first prompt.
    pub(super) fn list_commands(&mut self) {
        let request = ListRequest::Commands(self.active);
        let listed = self.controller.commands_listed(self.active);
        if !listed && !self.lists_failed.contains(&request) {
            self.request_list(request);
        }
    }

    /// Choose the active harness's provider. A session that takes it only
    /// between turns is told, and says how it took it ([`App::choice_sent`]);
    /// where it is the process's, a session nothing was sent to is started
    /// again on it; a stored session that keeps what it ran is left for a
    /// new one ([`App::renewal_without_session`]).
    pub fn set_provider(&mut self, provider: ProviderId) {
        if self.choice_pending.is_some() && self.refused_while_settling() {
            return;
        }
        let changed = self.current_provider() != Some(&provider);
        let between = self.between_turns(Choice::Provider);
        if changed && between && self.refused_between_turns(Choice::Provider) {
            return;
        }
        if changed {
            self.running_providers.remove(&self.active);
            self.models.remove(&self.active);
            self.chosen_models.remove(&self.active);
            self.run_models.remove(&self.active);
            self.conversation.models.remove(&self.active);
        }
        self.providers.insert(self.active, provider.clone());
        self.chosen_providers.insert(self.active);
        self.idle_start_paused = false;
        let line = provider_line(&provider);
        if !changed {
            self.transcript.push_system(line);
        } else if let Some(why) = self.renewal_without_session() {
            self.transcript.push_system(line);
            self.session_renewed(&format!("provider {provider}: {why}"));
        } else if self.session_alive && self.session_blank && between && self.caps().multi_provider
        {
            // Nothing was sent to it: it starts again on the provider. (A
            // harness of one provider is not, for a name it does not know.)
            self.transcript
                .push_system(restart_line(&provider, self.display_name()));
            self.shutdown_session();
        } else if self.session_seen() && between {
            // Its line says how the session took it, with its answer.
            self.choice_sent(
                SessionCommand::SetProvider(provider.clone()),
                Choice::Provider,
                Some(provider),
            );
        } else {
            self.transcript.push_system(line);
            if self.session_seen() {
                self.actions
                    .push_back(Action::Command(SessionCommand::SetProvider(provider)));
            }
        }
    }

    pub fn set_model(&mut self, model: String) {
        if self.choice_pending.is_some() && self.refused_while_settling() {
            return;
        }
        let between = self.between_turns(Choice::Model);
        if between && self.refused_between_turns(Choice::Model) {
            return;
        }
        let provider = self
            .current_provider()
            .cloned()
            .unwrap_or_else(|| ProviderId::new("default"));
        let m = ModelRef::new(self.active, provider, model);
        self.models.insert(self.active, m.clone());
        self.chosen_models.insert(self.active);
        self.run_models.insert(self.active);
        self.idle_start_paused = false;
        self.transcript.push_system(format!("Model: {}", m.label()));
        let choice = format!("model {}", m.model);
        self.apply_choice(between, SessionCommand::SetModel(m), Choice::Model, &choice);
    }

    pub fn set_effort(&mut self, effort: String) {
        let caps = self.caps();
        if caps.effort_levels.is_empty() {
            self.transcript.push_error(format!(
                "{} does not expose reasoning effort",
                self.display_name()
            ));
            return;
        }
        let effort = effort.to_lowercase();
        if !caps.supports_effort(&effort) {
            self.transcript.push_error(format!(
                "unknown effort '{}'; {} supports: {}",
                effort,
                self.short_name(),
                caps.effort_levels.join(", ")
            ));
            return;
        }
        if self.choice_pending.is_some() && self.refused_while_settling() {
            return;
        }
        let between = self.between_turns(Choice::Effort);
        if between && self.refused_between_turns(Choice::Effort) {
            return;
        }
        self.efforts.insert(self.active, effort.clone());
        self.chosen_efforts.insert(self.active);
        self.run_efforts.insert(self.active);
        self.idle_start_paused = false;
        self.transcript.push_system(format!("Effort: {effort}"));
        let choice = format!("effort {effort}");
        self.apply_choice(
            between,
            SessionCommand::SetEffort(Some(effort)),
            Choice::Effort,
            &choice,
        );
    }

    /// Carry out a model or effort just chosen (`cmd`, worded `choice`),
    /// which the session takes only `between` turns or any time: a stored
    /// session that keeps what it ran is left for a new one; one nothing
    /// was sent to is started again with it (Claude would take an effort
    /// as a turn); one prompted is told, and says how it took it.
    fn apply_choice(&mut self, between: bool, cmd: SessionCommand, kind: Choice, choice: &str) {
        if let Some(why) = self.renewal_without_session() {
            self.session_renewed(&format!("{choice}: {why}"));
        } else if !self.session_alive {
            // The next session starts with it.
        } else if self.session_blank {
            self.shutdown_session();
        } else if between {
            self.choice_sent(cmd, kind, None);
        } else {
            self.actions.push_back(Action::Command(cmd));
        }
    }

    /// Whether the active harness takes `choice` only between turns, with
    /// no subagent whose end will be told at work: the one reader of
    /// `Capabilities::between_turns`.
    fn between_turns(&self, choice: Choice) -> bool {
        let between = self.caps().between_turns;
        match choice {
            Choice::Model => between.model,
            Choice::Effort => between.effort,
            Choice::Provider => between.provider,
        }
    }

    /// Why the active harness's stored session cannot take a new model,
    /// effort or provider, if the conversation has one: the one reader of
    /// `Capabilities::stored_session_keeps`.
    fn stored_session_keeps(&self) -> Option<&'static str> {
        self.caps()
            .stored_session_keeps
            .filter(|_| self.session_ids.contains_key(&self.active))
    }

    /// Why a choice leaves the stored session for a new one now, with no
    /// session to say so: none is live, or the live one was sent nothing
    /// (it would be started again on the stored session, as it ran). A
    /// session that was prompted says it itself (`SessionRenewed`).
    fn renewal_without_session(&self) -> Option<&'static str> {
        if self.session_seen() {
            return None;
        }
        self.stored_session_keeps()
    }

    /// Whether `choice`, which the active harness takes only between
    /// turns, is refused now; if so the user is told why. Not during a
    /// turn, nor while subagents whose end will be told are at work (they
    /// would end with the session or its process), nor, for a stored
    /// session that keeps what it ran, while the CLI before is still
    /// exiting (the start that waits for it resumes that session). (No
    /// choice is made while the session has not said how it took the one
    /// before, `choice_pending`: it may be leaving for a new session, or
    /// starting its CLI again, which would take an effort sent meanwhile
    /// as a turn.)
    fn refused_between_turns(&mut self, choice: Choice) -> bool {
        let what = choice.what();
        if self.start_held && self.stored_session_keeps().is_some() && self.refused_while_settling()
        {
            return true;
        }
        if !self.session_alive {
            return false;
        }
        if self.is_generating {
            self.transcript.push_error(format!(
                "finish or interrupt the current turn before changing the {what}"
            ));
            return true;
        }
        // They would end with the process.
        self.refused_for_subagents(&format!("changing the {what}"))
    }

    /// Send `cmd`, a choice the session takes only between turns, and hold
    /// what is typed after it until the session has said how it took it
    /// (`Restarted`, `SessionRenewed`). Its process may be started again by
    /// its driver: as before the TUI starts one, what the session changed
    /// since its last turn is told now, its harness is prepared, and the
    /// next process is watched from its start.
    fn choice_sent(&mut self, cmd: SessionCommand, kind: Choice, provider: Option<ProviderId>) {
        self.check_guard();
        match self.harness().prepare() {
            Ok(Some(note)) => self.transcript.push_notice(note),
            Ok(None) => {}
            Err(e) => self
                .transcript
                .push_error(format!("{}: {e:#}", self.short_name())),
        }
        self.arm_guard();
        self.choice_pending = Some(PendingChoice {
            queued: self.queued.len(),
            what: kind.what(),
            provider,
        });
        self.actions.push_back(Action::Command(cmd));
    }

    /// The choice the session was sent has been answered, or will not be
    /// (its process is gone): what waited for the answer. A provider's line
    /// is said here, as `taken` words it.
    pub(super) fn choice_answered(
        &mut self,
        taken: impl FnOnce(&ProviderId) -> String,
    ) -> Option<PendingChoice> {
        let pending = self.choice_pending.take()?;
        if let Some(provider) = &pending.provider {
            self.transcript.push_system(taken(provider));
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
        if self.stored_session_keeps().is_some() {
            self.forget_ids(self.active);
        }
        if self.queued.len() > pending.queued {
            self.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    /// The choice the session was sent is taken: the first prompt typed
    /// after it goes now, as it would have had it been typed then; those
    /// held before it still wait.
    pub(super) fn send_typed_after(&mut self, pending: &PendingChoice) {
        if self.queued.len() > pending.queued {
            self.send_queued(pending.queued);
        }
    }

    pub fn open_provider_picker(&mut self) {
        if !self.caps().multi_provider {
            self.transcript.push_notice(format!(
                "{} has a single provider ({}); use /harness to change agents or /model to change models",
                self.short_name(),
                self.current_provider()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "default".into())
            ));
            return;
        }
        match self.provider_cache.get(&self.active).cloned() {
            Some(providers) => self.show_provider_picker(Ok(providers)),
            None => self.wait_for_list(ListRequest::Providers(self.active)),
        }
    }

    fn show_provider_picker(&mut self, providers: Result<Vec<(ProviderId, String)>, String>) {
        let providers = match providers {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => {
                self.transcript
                    .push_notice("this harness reports no providers");
                return;
            }
            Err(e) => {
                self.transcript.push_error(format!("list providers: {e}"));
                return;
            }
        };
        let items: Vec<ProviderOption> = providers
            .into_iter()
            .map(|(id, name)| ProviderOption { id: id.0, name })
            .collect();
        let idx = self
            .current_provider()
            .and_then(|p| items.iter().position(|i| i.id == p.0));
        self.modal = Some(Modal::Provider(ListPicker::new(items).with_selected(idx)));
    }

    /// The models of `provider` on the active harness if they are known;
    /// otherwise they are asked for, once: after a failure only a picker
    /// the user opens asks again.
    pub(super) fn cached_models(&mut self, provider: &ProviderId) -> Option<Vec<ModelInfo>> {
        let request = ListRequest::Models(self.active, provider.clone());
        let cached = self
            .model_cache
            .get(&(self.active, provider.0.clone()))
            .cloned();
        if cached.is_none() && !self.lists_failed.contains(&request) {
            self.request_list(request);
        }
        cached
    }

    /// Ask for `request` (once at a time) and open its picker when it comes.
    fn wait_for_list(&mut self, request: ListRequest) {
        let what = match &request {
            ListRequest::Providers(_) => "providers".to_string(),
            ListRequest::Models(_, p) => format!("models on {p}"),
            ListRequest::Commands(_) => "commands".to_string(),
        };
        self.lists_failed.remove(&request);
        self.open_when_listed = Some(request.clone());
        if self.request_list(request) {
            self.transcript
                .push_notice(format!("Asking {} for its {what}…", self.short_name()));
        }
    }

    /// Queue `request` for a thread unless it is already under way.
    /// Returns whether it was queued now.
    fn request_list(&mut self, request: ListRequest) -> bool {
        if self.lists_pending.contains(&request) {
            return false;
        }
        let ready = self
            .harness_binary()
            .ok_or_else(|| format!("{} binary not found", self.short_name()))
            .and_then(|binary| {
                let sandbox = self.session_sandbox().map_err(|e| format!("{e:#}"))?;
                Ok((binary, sandbox))
            });
        match ready {
            Ok((binary, sandbox)) => {
                self.lists_pending.insert(request.clone());
                self.list_jobs.push(ListJob {
                    request,
                    registry: self.registry.clone(),
                    binary,
                    cwd: self.cwd.clone(),
                    sandbox,
                });
                true
            }
            Err(why) => {
                self.list_failed(request, why);
                false
            }
        }
    }

    /// A list could not be had: noted, so that completion does not ask
    /// again by itself, and shown if the user is waiting for it.
    fn list_failed(&mut self, request: ListRequest, why: String) {
        self.lists_failed.insert(request.clone());
        // Asked for by itself, and nothing else would say why the list
        // has none of the harness's commands.
        if let ListRequest::Commands(harness) = request {
            let name = self
                .registry
                .get(harness)
                .map_or("the harness", |h| h.descriptor().short_name);
            self.transcript
                .push_notice(format!("{name}'s commands could not be listed: {why}"));
            return;
        }
        if self.open_when_listed.as_ref() == Some(&request) {
            self.open_when_listed = None;
            self.transcript.push_error(match request {
                ListRequest::Providers(_) => format!("list providers: {why}"),
                ListRequest::Models(..) => format!("list models: {why}"),
                ListRequest::Commands(_) => format!("list commands: {why}"),
            });
        }
    }

    /// Lists to fetch on threads of their own.
    pub fn take_list_jobs(&mut self) -> Vec<ListJob> {
        std::mem::take(&mut self.list_jobs)
    }

    /// A list has come back. It is kept unless it failed, and its picker
    /// opens if the user is still waiting for it: on the same harness and
    /// provider, with no other picker open and nothing being typed.
    pub fn on_list(&mut self, request: ListRequest, result: ListResult) {
        self.lists_pending.remove(&request);
        match result {
            ListResult::Providers(Err(why))
            | ListResult::Models(Err(why))
            | ListResult::Commands(Err(why)) => {
                self.list_failed(request, why);
            }
            // It cannot be told this way: not asked again.
            ListResult::Commands(Ok(None)) => {
                self.lists_failed.insert(request);
            }
            ListResult::Commands(Ok(Some(commands))) => {
                let ListRequest::Commands(harness) = request else {
                    return;
                };
                self.controller.commands_arrived(harness, commands);
                if harness == self.active {
                    self.commands_arrived();
                }
            }
            ListResult::Providers(Ok(providers)) => {
                self.lists_failed.remove(&request);
                let ListRequest::Providers(harness) = request else {
                    return;
                };
                self.provider_cache.insert(harness, providers.clone());
                if self.waiting_for(&request) {
                    self.show_provider_picker(Ok(providers));
                }
            }
            ListResult::Models(Ok(models)) => {
                self.lists_failed.remove(&request);
                let ListRequest::Models(harness, provider) = &request else {
                    return;
                };
                self.model_cache
                    .insert((*harness, provider.0.clone()), models.clone());
                if self.completing_file.is_none() && self.input.starts_with("/model ") {
                    self.update_suggestions();
                }
                if self.waiting_for(&request) {
                    let provider = provider.clone();
                    self.show_model_picker(&provider, Ok(models));
                }
            }
        }
    }

    /// Whether the picker for `request`, which has just arrived, should
    /// open now. Either way the user is no longer waiting for it.
    fn waiting_for(&mut self, request: &ListRequest) -> bool {
        if self.open_when_listed.as_ref() != Some(request) {
            return false;
        }
        self.open_when_listed = None;
        let still_there = match request {
            ListRequest::Providers(h) => *h == self.active,
            ListRequest::Models(h, p) => *h == self.active && self.current_provider() == Some(p),
            ListRequest::Commands(_) => false,
        };
        if !still_there || self.modal.is_some() {
            return false;
        }
        if !self.input.is_empty() {
            self.transcript.push_notice(match request {
                ListRequest::Providers(_) => "The providers are here: /provider".to_string(),
                ListRequest::Models(_, p) => format!("The models on {p} are here: Ctrl+M"),
                ListRequest::Commands(_) => return false,
            });
            return false;
        }
        true
    }

    pub fn open_model_picker(&mut self) {
        let Some(provider) = self.current_provider().cloned() else {
            if self.caps().multi_provider {
                self.open_provider_picker();
            } else {
                self.transcript
                    .push_notice("no provider configured for this harness");
            }
            return;
        };
        match self
            .model_cache
            .get(&(self.active, provider.0.clone()))
            .cloned()
        {
            Some(models) => self.show_model_picker(&provider, Ok(models)),
            None => self.wait_for_list(ListRequest::Models(self.active, provider)),
        }
    }

    fn show_model_picker(&mut self, provider: &ProviderId, models: Result<Vec<ModelInfo>, String>) {
        match models {
            Ok(models) if !models.is_empty() => {
                let idx = self
                    .current_model()
                    .and_then(|m| models.iter().position(|i| i.model_ref.model == m.model));
                self.modal = Some(Modal::Model(ListPicker::new(models).with_selected(idx)));
            }
            Ok(_) => self.transcript.push_notice(format!(
                "no models reported for {provider}; use /model <name> to set one directly"
            )),
            Err(e) => self.transcript.push_error(format!("list models: {e}")),
        }
    }

    pub fn open_effort_picker(&mut self) {
        let levels = self.caps().effort_levels;
        if levels.is_empty() {
            self.transcript.push_error(format!(
                "{} does not expose reasoning effort",
                self.display_name()
            ));
            return;
        }
        let idx = self
            .current_effort()
            .and_then(|e| levels.iter().position(|l| l == e));
        self.modal = Some(Modal::Effort(ListPicker::new(levels).with_selected(idx)));
    }
}

/// A choice of the active harness's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Model,
    Effort,
    Provider,
}

impl Choice {
    fn what(self) -> &'static str {
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
pub(super) struct PendingChoice {
    /// The prompts queued when it was sent: any after them were typed
    /// after it, and go when it has been taken.
    pub queued: usize,
    /// What was chosen: `model`, `effort`, `provider`.
    pub what: &'static str,
    /// A provider's line, which says how the session took it, waits for
    /// the answer.
    pub provider: Option<ProviderId>,
}

/// What is said of a provider chosen.
pub(super) fn provider_line(provider: &ProviderId) -> String {
    format!("Provider: {provider} (pick a model with Ctrl+M)")
}

/// What is said of a provider whose harness's process starts again on it.
pub(super) fn restart_line(provider: &ProviderId, name: &str) -> String {
    format!("Provider: {provider} ({name} restarts on it; pick a model with Ctrl+M)")
}
