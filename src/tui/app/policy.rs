//! The permission policy as the TUI shows it and asks the user for it.
//! The state is the controller's (`core::controller::PolicyState`), and so
//! is what choosing one does to the session (`core::controller`,
//! `policy.rs`).

use std::path::Path;

use super::App;
use super::front::policy_picker;
use crate::config::Config;
use crate::core::rules::Scope;
use crate::core::{PermissionPolicy, PolicyResolution};

impl App {
    /// The policy the user asked for on the active harness: the one named
    /// for the run, or this harness's configured default.
    pub fn policy_requested(&self) -> PermissionPolicy {
        self.controller
            .policy
            .requested(&self.controller.config, self.controller.active)
    }

    /// The policy asked of the active harness: what the user chose for it,
    /// else the requested one.
    pub fn wanted_policy(&self) -> PermissionPolicy {
        self.controller
            .policy
            .wanted(&self.controller.config, self.controller.active)
    }

    /// `None` until the user chooses, where the requested policy is not
    /// available: a more permissive one is never taken for them.
    pub fn effective_policy(&self) -> Option<PermissionPolicy> {
        self.controller.effective_policy(self.controller.active)
    }

    pub fn policy_warning(&self) -> Option<String> {
        self.controller.policy_warning()
    }

    /// Ask for a policy where the requested one is not available. Returns
    /// whether one is in effect.
    pub fn require_policy(&mut self) -> bool {
        self.with(|c, screen| c.require_policy(screen))
    }

    /// Returns whether `p` could be set on the active harness
    /// (`Controller::set_policy`).
    pub fn set_policy(&mut self, p: PermissionPolicy) -> bool {
        self.with(|c, screen| c.set_policy(p, screen))
    }

    /// Save `p` as the active harness's default policy in the settings of
    /// `scope` (under the config directory, never in the workspace), and
    /// choose it. Returns whether both were done.
    pub fn save_default_policy(&mut self, p: PermissionPolicy, scope: Scope) -> bool {
        if self.offered(p).is_none() {
            return false;
        }
        let name = self.short_name();
        let id = self.controller.active.as_str();
        let root = self.controller.workspace_root.clone();
        let Some(dir) = self
            .controller
            .tracking
            .rules
            .config_dir()
            .map(Path::to_path_buf)
        else {
            self.controller
                .conversation
                .transcript
                .push_error("there is no config directory to save the default policy in");
            return false;
        };
        let Some(path) = Config::scoped_path(&dir, scope, root.as_deref()) else {
            self.controller
                .conversation
                .transcript
                .push_error("there is no workspace to save the default policy for");
            return false;
        };
        if let Err(e) = Config::set_harness_default_policy(&path, id, p) {
            self.controller
                .conversation
                .transcript
                .push_error(format!("could not save the default policy: {e:#}"));
            return false;
        }
        let place = match scope {
            Scope::Workspace => "for this workspace",
            Scope::Global => "for every workspace",
        };
        self.controller.conversation.transcript.push_system(format!(
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
            Some(w) => self.controller.conversation.transcript.push_notice(format!(
                "this workspace's settings make {w} {name}'s default here"
            )),
            None => {
                if let Some(w) = for_all.filter(|w| *w != p.as_str()) {
                    self.controller.conversation.transcript.push_notice(format!(
                        "this workspace's default_policy ({w}) no longer applies to {name}: \
                         a harness's own default comes first (save for this workspace to \
                         keep {w} here)"
                    ));
                }
                self.controller
                    .config
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
        match self.controller.offered(self.controller.active, p) {
            Ok(res) => Some(res),
            Err(e) => {
                self.controller
                    .conversation
                    .transcript
                    .push_error(e.to_string());
                None
            }
        }
    }

    /// The policies the active harness has, least permissive first.
    pub fn offered_policies(&self) -> Vec<PermissionPolicy> {
        self.controller.offered_policies(self.controller.active)
    }

    /// Every policy is listed; one the harness does not have cannot be
    /// chosen (`Controller::policy_ask`).
    pub fn open_policy_picker(&mut self) {
        self.modal = Some(policy_picker(self.controller.policy_ask()));
    }
}
