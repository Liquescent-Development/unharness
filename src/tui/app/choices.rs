//! Providers, models and efforts, and the lists they are chosen from.

use super::{Action, App, ListJob, ListRequest, ListResult};
use crate::core::{CapsUpdate, HarnessId, ModelRef, ProviderId, SessionCommand};
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
        let listed = self
            .live_caps
            .get(&self.active)
            .is_some_and(|c| c.commands.is_some());
        if !listed && !self.lists_failed.contains(&request) {
            self.request_list(request);
        }
    }

    /// Choose the active harness's provider. Where the provider is fixed
    /// when the process starts, a live session is shut down and comes back
    /// on the new one, resumed.
    pub fn set_provider(&mut self, provider: ProviderId) {
        let changed = self.current_provider() != Some(&provider);
        // Where the model is chosen when the process starts as well
        // (Mistral Vibe), the provider goes with it.
        if changed && self.caps().model_per_process && !self.may_restart_for("provider") {
            return;
        }
        let restart = changed && self.session_alive && self.caps().provider_per_process;
        if restart && self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before changing the provider");
            return;
        }
        // They would end with the process.
        if restart && self.refused_for_subagents("changing the provider") {
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
        // A session that was prompted keeps the model it last ran: a new
        // one runs on the provider, told the conversation (`set_model`).
        if changed && self.caps().model_per_process && self.renews_for_choice() {
            self.transcript
                .push_system(format!("Provider: {provider} (pick a model with Ctrl+M)"));
            self.renew_session(&format!("provider {provider}"));
            return;
        }
        self.transcript.push_system(if restart {
            format!(
                "Provider: {provider} ({} restarts on it; pick a model with Ctrl+M)",
                self.display_name()
            )
        } else {
            format!("Provider: {provider} (pick a model with Ctrl+M)")
        });
        if restart {
            self.shutdown_session();
        }
    }

    pub fn set_model(&mut self, model: String) {
        if !self.may_restart_for("model") {
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
        if self.renews_for_choice() {
            self.renew_session(&format!("model {}", m.model));
            return;
        }
        if self.session_alive && self.session_blank {
            // Nothing was sent to it: it starts again with the model.
            self.shutdown_session();
        } else if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::SetModel(m)));
        }
    }

    /// Whether a change of the model or effort starts a new session of the
    /// active harness: its sessions keep the ones they last ran
    /// (`Capabilities::model_per_process`), and the conversation has one.
    /// One nothing was sent to yet starts again with the choice instead.
    fn renews_for_choice(&self) -> bool {
        self.caps().model_per_process && self.session_ids.contains_key(&self.active)
    }

    /// Whether the session may be ended for a change of `what` (its model
    /// or effort) where those are chosen when it starts
    /// (`Capabilities::model_per_process`); if not, the user is told why.
    fn may_restart_for(&mut self, what: &str) -> bool {
        if !self.caps().model_per_process {
            return true;
        }
        if self.refused_while_start_held() {
            return false;
        }
        if !self.session_alive {
            return true;
        }
        if self.is_generating {
            self.transcript.push_error(format!(
                "finish or interrupt the current turn before changing the {what}"
            ));
            return false;
        }
        // They would end with the process.
        if self.refused_for_subagents(&format!("changing the {what}")) {
            return false;
        }
        true
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
        if !self.may_restart_for("effort") {
            return;
        }
        self.efforts.insert(self.active, effort.clone());
        self.chosen_efforts.insert(self.active);
        self.run_efforts.insert(self.active);
        self.idle_start_paused = false;
        self.transcript.push_system(format!("Effort: {effort}"));
        if self.renews_for_choice() {
            self.renew_session(&format!("effort {effort}"));
            return;
        }
        if self.session_alive && self.session_blank {
            // Claude would take the change as a turn: it starts again with
            // the effort instead.
            self.shutdown_session();
        } else if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::SetEffort(Some(effort))));
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
                self.live_caps
                    .entry(harness)
                    .or_default()
                    .merge(CapsUpdate {
                        slash_commands: Some(!commands.is_empty()),
                        commands: Some(commands),
                        ..Default::default()
                    });
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
