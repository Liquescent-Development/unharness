//! The permission policy: requested, chosen, in effect, refused and saved.

use std::path::Path;

use super::{Action, App};
use crate::config::Config;
use crate::core::rules::Scope;
use crate::core::{
    PermissionPolicy, PolicyResolution, PolicyUnavailable, SessionCommand, resolve_policy,
};
use crate::tui::modal::{ListPicker, Modal, PolicyPicker};

impl App {
    /// The policy the user asked for on the active harness: the one named
    /// for the run, or this harness's configured default.
    pub fn policy_requested(&self) -> PermissionPolicy {
        self.policy_explicit.unwrap_or_else(|| {
            // The config was checked at startup; `ask` if it changed since.
            crate::runner::configured_policy(&self.config, self.active)
                .unwrap_or(PermissionPolicy::Ask)
        })
    }

    /// The policy asked of the active harness: what the user chose for it,
    /// else the requested one.
    pub fn wanted_policy(&self) -> PermissionPolicy {
        self.policy_choice
            .get(&self.active)
            .copied()
            .unwrap_or_else(|| self.policy_requested())
    }

    fn policy_resolution(&self) -> Result<PolicyResolution, PolicyUnavailable> {
        resolve_policy(&self.caps().permission_policies, self.wanted_policy())
    }

    /// `None` until the user chooses, where the requested policy is not
    /// available: a more permissive one is never taken for them.
    pub fn effective_policy(&self) -> Option<PermissionPolicy> {
        self.policy_resolution().ok().map(|r| r.effective)
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

    /// The active harness's session is gone. What it refused was for it
    /// (`CapsUpdate::refused_policies`: a mode that did not take, a
    /// profile shadowed): the next session is asked for the wanted policy
    /// again, and refuses it again if it still cannot hold it. The rest of
    /// what it reported is the agent's, kept until the next one says.
    pub(super) fn forget_refusals(&mut self) {
        let before = self.effective_policy();
        let Some(update) = self.live_caps.get_mut(&self.active) else {
            return;
        };
        if update.refused_policies.take().is_none() {
            return;
        }
        // A policy picker open on what the session offered is redone, or,
        // where it waited for a policy and one is in effect again, closed:
        // what waited for it is the user's to send.
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
        self.live_caps
            .get(&self.active)
            .and_then(|u| u.refused_policies.as_ref())
            .is_some_and(|r| !r.is_empty())
    }

    /// Returns whether `p` could be set on the active harness. Only a
    /// policy it has can be: what the user picks is what runs.
    pub fn set_policy(&mut self, p: PermissionPolicy) -> bool {
        let Some(res) = self.offered(p) else {
            return false;
        };
        let before = self.effective_policy();
        if res.effective == PermissionPolicy::Plan
            && let Some(was) = before
            && was != PermissionPolicy::Plan
        {
            self.pre_plan.insert(self.active, was);
        }
        // Only a prompt that waited for this choice is sent by it.
        let was_waiting = before.is_none();
        // For this harness only: the others keep theirs.
        self.policy_choice.insert(self.active, p);
        self.run_choices.insert(self.active, p);
        self.transcript
            .push_system(format!("Permission policy: {}", res.effective));
        if let Some(w) = res.warning {
            self.transcript.push_notice(w);
        }
        self.idle_start_paused = false;
        if self.session_alive && self.session_blank {
            // Started with the policy it had: started again with this one.
            self.shutdown_session();
        } else if self.session_alive && before != Some(res.effective) {
            // Sent only as a change, so that each is reported back once.
            self.policies_sent.push_back(res.effective);
            self.actions
                .push_back(Action::Command(SessionCommand::SetPolicy(res.effective)));
        }
        if was_waiting {
            self.send_next_queued();
        }
        true
    }

    /// The session runs under `p` now. One it moved to by itself (the
    /// agent went into plan mode, or left a mode it could not keep) becomes
    /// the harness's policy, so that what is shown, saved and started again
    /// is what runs.
    pub(super) fn on_policy_changed(&mut self, p: PermissionPolicy) {
        // The report of a change we sent, maybe after a later one: the
        // policy shown is already the newest choice.
        if let Some(at) = self.policies_sent.iter().position(|s| *s == p) {
            self.policies_sent.drain(..=at);
            return;
        }
        let was = self.effective_policy();
        if was == Some(p) {
            return;
        }
        if p == PermissionPolicy::Plan
            && let Some(was) = was
        {
            self.pre_plan.insert(self.active, was);
        }
        self.policy_choice.insert(self.active, p);
        self.run_choices.insert(self.active, p);
        let name = self.display_name();
        self.transcript.push_notice(match p {
            PermissionPolicy::Plan => {
                format!(
                    "{name} went into plan mode: it asks you to approve its plan before it acts"
                )
            }
            _ => format!("{name} now runs under policy {p}"),
        });
    }

    /// The session took a policy out of what it offers (an ACP agent whose
    /// mode for it did not take; the driver said why). One in effect gives
    /// way as an unsupported policy does: to the nearest less permissive
    /// one, which the session is told, or, with none, to the user's choice.
    pub(super) fn policy_refused(&mut self, before: Option<PermissionPolicy>) {
        let now = self.effective_policy();
        if now == before {
            return;
        }
        match now {
            Some(p) => {
                if let Some(w) = self.policy_warning() {
                    self.transcript.push_notice(w);
                }
                if self.session_alive {
                    self.policies_sent.push_back(p);
                    self.actions
                        .push_back(Action::Command(SessionCommand::SetPolicy(p)));
                }
            }
            None => {
                self.transcript.push_notice(format!(
                    "{} waits for another policy (Ctrl+P)",
                    self.display_name()
                ));
                self.require_policy();
            }
        }
    }

    /// The policies a plan can be carried out under: those the harness has,
    /// but bypass only in a session started under it, since Claude refuses
    /// to switch to it in one that was not (`bypass_not_launched`, 2.1.296).
    pub(super) fn plan_policies(&self) -> Vec<PermissionPolicy> {
        let bypass = self.session_policy == Some(PermissionPolicy::Bypass);
        self.offered_policies()
            .into_iter()
            .filter(|p| match p {
                PermissionPolicy::Plan => false,
                PermissionPolicy::Bypass => bypass,
                _ => true,
            })
            .collect()
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
        match resolve_policy(&self.caps().permission_policies, p) {
            Ok(res) if res.effective == p => Some(res),
            _ => {
                let e = PolicyUnavailable {
                    requested: p,
                    supported: self.offered_policies(),
                };
                self.transcript.push_error(e.to_string());
                None
            }
        }
    }

    /// The policies the active harness has, least permissive first.
    pub fn offered_policies(&self) -> Vec<PermissionPolicy> {
        let caps = self.caps();
        PermissionPolicy::ALL
            .into_iter()
            .filter(|p| caps.supports_policy(*p).is_some())
            .collect()
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
