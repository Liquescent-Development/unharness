//! Providers, models and efforts as the TUI offers them, and the lists
//! they are chosen from. What a choice does to the session is the
//! controller's (`core::controller`, `choices.rs`).

use super::{App, ListJob, ListRequest, ListResult};
use crate::core::{ModelRef, ProviderId};
use crate::harness::ModelInfo;
use crate::tui::modal::{ListPicker, Modal, ProviderOption};

impl App {
    pub fn current_provider(&self) -> Option<&ProviderId> {
        self.controller.provider(self.controller.active)
    }

    /// The provider to tell the active harness, if the user chose one.
    pub fn chosen_provider(&self) -> Option<&ProviderId> {
        self.controller.chosen_provider(self.controller.active)
    }

    pub fn current_model(&self) -> Option<&ModelRef> {
        self.controller.model(self.controller.active)
    }

    pub fn current_effort(&self) -> Option<&str> {
        self.controller.effort(self.controller.active)
    }

    /// The effort the status line shows (`Controller::effort_label`).
    pub fn effort_label(&self) -> Option<String> {
        self.controller.effort_label(self.controller.active)
    }

    pub fn model_label(&self) -> String {
        self.controller.model_label()
    }

    /// Ask the active harness for its commands without a session, once,
    /// where the session would not say before the first prompt.
    pub(super) fn list_commands(&mut self) {
        let request = ListRequest::Commands(self.controller.active);
        let listed = self.controller.commands_listed(self.controller.active);
        if !listed && !self.lists_failed.contains(&request) {
            self.request_list(request);
        }
    }

    /// Choose the active harness's provider (`Controller::set_provider`).
    pub fn set_provider(&mut self, provider: ProviderId) {
        self.with(|c, screen| c.set_provider(provider, screen));
    }

    pub fn set_model(&mut self, model: String) {
        self.with(|c, screen| c.set_model(model, screen));
    }

    pub fn set_effort(&mut self, effort: String) {
        self.with(|c, screen| c.set_effort(effort, screen));
    }

    pub fn open_provider_picker(&mut self) {
        if !self.caps().multi_provider {
            self.controller.conversation.transcript.push_notice(format!(
                "{} has a single provider ({}); use /harness to change agents or /model to change models",
                self.short_name(),
                self.current_provider()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "default".into())
            ));
            return;
        }
        match self
            .controller
            .provider_list(self.controller.active)
            .cloned()
        {
            Some(providers) => self.show_provider_picker(Ok(providers)),
            None => self.wait_for_list(ListRequest::Providers(self.controller.active)),
        }
    }

    fn show_provider_picker(&mut self, providers: Result<Vec<(ProviderId, String)>, String>) {
        let providers = match providers {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => {
                self.controller
                    .conversation
                    .transcript
                    .push_notice("this harness reports no providers");
                return;
            }
            Err(e) => {
                self.controller
                    .conversation
                    .transcript
                    .push_error(format!("list providers: {e}"));
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
        let request = ListRequest::Models(self.controller.active, provider.clone());
        let cached = self
            .controller
            .model_list(self.controller.active, provider)
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
            self.controller
                .conversation
                .transcript
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
                    registry: self.registry().clone(),
                    binary,
                    cwd: self.controller.cwd.clone(),
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
                .registry()
                .get(harness)
                .map_or("the harness", |h| h.descriptor().short_name);
            self.controller
                .conversation
                .transcript
                .push_notice(format!("{name}'s commands could not be listed: {why}"));
            return;
        }
        if self.open_when_listed.as_ref() == Some(&request) {
            self.open_when_listed = None;
            self.controller
                .conversation
                .transcript
                .push_error(match request {
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
                if harness == self.controller.active {
                    self.commands_arrived();
                }
            }
            ListResult::Providers(Ok(providers)) => {
                self.lists_failed.remove(&request);
                let ListRequest::Providers(harness) = request else {
                    return;
                };
                self.controller.keep_providers(harness, providers.clone());
                if self.waiting_for(&request) {
                    self.show_provider_picker(Ok(providers));
                }
            }
            ListResult::Models(Ok(models)) => {
                self.lists_failed.remove(&request);
                let ListRequest::Models(harness, provider) = &request else {
                    return;
                };
                self.controller
                    .keep_models(*harness, provider.0.clone(), models.clone());
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
            ListRequest::Providers(h) => *h == self.controller.active,
            ListRequest::Models(h, p) => {
                *h == self.controller.active && self.current_provider() == Some(p)
            }
            ListRequest::Commands(_) => false,
        };
        if !still_there || self.modal.is_some() {
            return false;
        }
        if !self.input.is_empty() {
            self.controller
                .conversation
                .transcript
                .push_notice(match request {
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
                self.controller
                    .conversation
                    .transcript
                    .push_notice("no provider configured for this harness");
            }
            return;
        };
        match self
            .controller
            .model_list(self.controller.active, &provider)
            .cloned()
        {
            Some(models) => self.show_model_picker(&provider, Ok(models)),
            None => self.wait_for_list(ListRequest::Models(self.controller.active, provider)),
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
            Ok(_) => self.controller.conversation.transcript.push_notice(format!(
                "no models reported for {provider}; use /model <name> to set one directly"
            )),
            Err(e) => self
                .controller
                .conversation
                .transcript
                .push_error(format!("list models: {e}")),
        }
    }

    pub fn open_effort_picker(&mut self) {
        let levels = self.caps().effort_levels;
        if levels.is_empty() {
            self.controller.conversation.transcript.push_error(format!(
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
