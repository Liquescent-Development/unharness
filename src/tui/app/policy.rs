//! The permission policy: requested, chosen, in effect, refused and saved.
//! The state is the controller's (`core::controller::PolicyState`); this
//! is what the TUI shows of it and asks of the user.

use std::path::Path;

use super::{Action, App};
use crate::config::Config;
use crate::core::controller::Refused;
use crate::core::rules::Scope;
use crate::core::{PermissionPolicy, PolicyResolution, PolicyUnavailable, SessionCommand};
use crate::tui::modal::{ListPicker, Modal, PolicyPicker};

impl App {
    /// The policy the user asked for on the active harness: the one named
    /// for the run, or this harness's configured default.
    pub fn policy_requested(&self) -> PermissionPolicy {
        self.controller.policy.requested(&self.config, self.active)
    }

    /// The policy asked of the active harness: what the user chose for it,
    /// else the requested one.
    pub fn wanted_policy(&self) -> PermissionPolicy {
        self.controller.policy.wanted(&self.config, self.active)
    }

    fn policy_resolution(&self) -> Result<PolicyResolution, PolicyUnavailable> {
        self.controller.policy_resolution(&self.config, self.active)
    }

    /// `None` until the user chooses, where the requested policy is not
    /// available: a more permissive one is never taken for them.
    pub fn effective_policy(&self) -> Option<PermissionPolicy> {
        self.controller.effective_policy(&self.config, self.active)
    }

    /// The effective policy for a message.
    pub(super) fn policy_label(&self) -> String {
        match self.effective_policy() {
            Some(p) => p.to_string(),
            None => format!("{} (unavailable)", self.wanted_policy()),
        }
    }

    pub fn policy_warning(&self) -> Option<String> {
        match self.policy_resolution() {
            Ok(res) => res.warning,
            Err(e) => Some(format!("{e} (Ctrl+P)")),
        }
    }

    /// Ask for a policy where the requested one is not available. Returns
    /// whether one is in effect.
    pub fn require_policy(&mut self) -> bool {
        if self.effective_policy().is_some() {
            return true;
        }
        self.open_policy_picker();
        false
    }

    /// The active harness's session is gone, and what it refused with it
    /// (`Controller::forget_refusals`). A policy picker open on what the
    /// session offered is redone, or, where it waited for a policy and one
    /// is in effect again, closed: what waited for it is the user's to
    /// send.
    pub(super) fn forget_refusals(&mut self) {
        let before = self.effective_policy();
        if !self.controller.forget_refusals(self.active) {
            return;
        }
        if matches!(&self.modal, Some(Modal::Policy(p)) if p.save.is_none()) {
            if before.is_none() && self.effective_policy().is_some() {
                self.modal = None;
                if let Some(w) = self.policy_warning() {
                    self.transcript.push_notice(w);
                }
                if !self.queued.is_empty() {
                    self.transcript.push_notice(format!(
                        "{} queued prompt(s) held; press Enter to send the next",
                        self.queued.len()
                    ));
                }
            } else {
                self.open_policy_picker();
            }
        }
    }

    /// The active harness's session refused a policy.
    pub(super) fn session_refused(&self) -> bool {
        self.controller.refused_policies(self.active)
    }

    /// Returns whether `p` could be set on the active harness. Only a
    /// policy it has can be: what the user picks is what runs.
    pub fn set_policy(&mut self, p: PermissionPolicy) -> bool {
        let prompted = self.session_seen();
        let set = match self
            .controller
            .set_policy(&self.config, self.active, p, prompted)
        {
            Ok(set) => set,
            Err(e) => {
                self.transcript.push_error(e.to_string());
                return false;
            }
        };
        self.transcript
            .push_system(format!("Permission policy: {}", set.effective));
        if let Some(w) = set.warning {
            self.transcript.push_notice(w);
        }
        self.idle_start_paused = false;
        if self.session_alive && self.session_blank {
            // Started with the policy it had: started again with this one.
            self.shutdown_session();
        } else if set.send {
            self.actions
                .push_back(Action::Command(SessionCommand::SetPolicy(set.effective)));
        }
        // Only a prompt that waited for this choice is sent by it.
        if set.was_waiting {
            self.send_next_queued();
        }
        true
    }

    /// The session runs under `p` now; one it moved to by itself is said.
    pub(super) fn on_policy_changed(&mut self, p: PermissionPolicy) {
        if let Some(notice) = self
            .controller
            .on_policy_changed(&self.config, self.active, p)
        {
            self.transcript.push_notice(notice);
        }
    }

    /// The session took a policy out of what it offers (an ACP agent whose
    /// mode for it did not take; the driver said why), and the one in
    /// effect gave way (`PolicyState::on_refused`).
    pub(super) fn policy_refused(&mut self, refused: Refused) {
        match refused {
            Refused::GaveWay { now, send } => {
                if let Some(w) = self.policy_warning() {
                    self.transcript.push_notice(w);
                }
                if send {
                    self.actions
                        .push_back(Action::Command(SessionCommand::SetPolicy(now)));
                }
            }
            Refused::Waits => {
                self.transcript.push_notice(format!(
                    "{} waits for another policy (Ctrl+P)",
                    self.display_name()
                ));
                self.require_policy();
            }
        }
    }

    /// The policies a plan can be carried out under (`PolicyState::plan_policies`).
    pub(super) fn plan_policies(&self) -> Vec<PermissionPolicy> {
        self.controller.plan_policies(self.active)
    }

    /// Save `p` as the active harness's default policy in the settings of
    /// `scope` (under the config directory, never in the workspace), and
    /// choose it. Returns whether both were done.
    pub fn save_default_policy(&mut self, p: PermissionPolicy, scope: Scope) -> bool {
        if self.offered(p).is_none() {
            return false;
        }
        let name = self.short_name();
        let id = self.active.as_str();
        let root = self.workspace_root.clone();
        let Some(dir) = self.rules.config_dir().map(Path::to_path_buf) else {
            self.transcript
                .push_error("there is no config directory to save the default policy in");
            return false;
        };
        let Some(path) = Config::scoped_path(&dir, scope, root.as_deref()) else {
            self.transcript
                .push_error("there is no workspace to save the default policy for");
            return false;
        };
        if let Err(e) = Config::set_harness_default_policy(&path, id, p) {
            self.transcript
                .push_error(format!("could not save the default policy: {e:#}"));
            return false;
        }
        let place = match scope {
            Scope::Workspace => "for this workspace",
            Scope::Global => "for every workspace",
        };
        self.transcript.push_system(format!(
            "Saved {p} as {name}'s default policy {place} ({})",
            path.display()
        ));
        // What this run reads as the default from now on, unless the
        // workspace's own settings name another for this harness. One they
        // name for every harness gives way to it, which the user is told.
        let workspace = match scope {
            Scope::Global => Config::scoped_path(&dir, Scope::Workspace, root.as_deref())
                .and_then(|w| Config::load_file(&w).ok().flatten()),
            Scope::Workspace => None,
        };
        let for_harness = workspace
            .as_ref()
            .and_then(|c| c.harness(id)?.default_policy.clone());
        let for_all = workspace.as_ref().and_then(|c| c.default_policy.clone());
        match for_harness {
            Some(w) => self.transcript.push_notice(format!(
                "this workspace's settings make {w} {name}'s default here"
            )),
            None => {
                if let Some(w) = for_all.filter(|w| *w != p.as_str()) {
                    self.transcript.push_notice(format!(
                        "this workspace's default_policy ({w}) no longer applies to {name}: \
                         a harness's own default comes first (save for this workspace to \
                         keep {w} here)"
                    ));
                }
                self.config
                    .harnesses
                    .entry(id.to_string())
                    .or_default()
                    .default_policy = Some(p.to_string());
            }
        }
        self.set_policy(p)
    }

    /// How `p` resolves on the active harness, if it offers it; otherwise
    /// the user is told which it does offer.
    fn offered(&mut self, p: PermissionPolicy) -> Option<PolicyResolution> {
        match self.controller.offered(self.active, p) {
            Ok(res) => Some(res),
            Err(e) => {
                self.transcript.push_error(e.to_string());
                None
            }
        }
    }

    /// The policies the active harness has, least permissive first.
    pub fn offered_policies(&self) -> Vec<PermissionPolicy> {
        self.controller.offered_policies(self.active)
    }

    /// Every policy is listed; one the harness does not have cannot be
    /// chosen. The cursor starts on the wanted policy, else on the one in
    /// effect, else on the least permissive one there is.
    pub fn open_policy_picker(&mut self) {
        let offered = self.offered_policies();
        let name = self.display_name();
        let wanted = self.wanted_policy();
        let start = if offered.contains(&wanted) {
            Some(wanted)
        } else {
            self.effective_policy()
        };
        let idx = start.and_then(|s| PermissionPolicy::ALL.iter().position(|p| *p == s));
        self.modal = Some(Modal::Policy(PolicyPicker {
            list: ListPicker::new(PermissionPolicy::ALL.to_vec())
                .with_disabled(|p| (!offered.contains(p)).then(|| format!("not offered by {name}")))
                .with_selected(idx),
            save: None,
            has_workspace: self.rules.has_workspace(),
        }));
    }
}
