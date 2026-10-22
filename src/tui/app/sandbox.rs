//! The sandbox level the active harness's processes run at: the picker;
//! and the dialog that grants a path beyond it. The level and the grants
//! are the controller's (`core::controller`, `sandbox.rs`).

use std::path::{Path, PathBuf};

use super::App;
use crate::core::controller::GrantScope;
use crate::core::sandbox::grants::{grant_target, propose};
use crate::core::sandbox::{Access, Sandbox, SandboxLevel, expand_home};
use crate::tui::modal::{GrantDraft, ListPicker, Modal};

impl App {
    /// The sandbox level the active harness runs at, and why it is off if
    /// that was not asked for.
    pub fn sandbox_level(&self) -> (SandboxLevel, Option<String>) {
        self.controller.sandbox_level()
    }

    /// What confines the next process of the active harness.
    pub fn session_sandbox(&self) -> anyhow::Result<Sandbox> {
        self.controller.session_sandbox()
    }

    /// Choose the sandbox level for the run (`Controller::set_sandbox`).
    pub fn set_sandbox(&mut self, level: SandboxLevel) -> bool {
        self.with(|c, screen| c.set_sandbox(level, screen))
    }

    pub fn open_sandbox_picker(&mut self) {
        let wanted = self.sandbox_level().0;
        let idx = SandboxLevel::ALL.iter().position(|l| *l == wanted);
        let unavailable = self.controller.sandbox.backend.is_err();
        self.modal = Some(Modal::Sandbox(
            ListPicker::new(SandboxLevel::ALL.to_vec())
                // Why is said once, above the list.
                .with_disabled(|l| {
                    (unavailable && *l != SandboxLevel::Off).then(|| "no sandbox here".to_string())
                })
                .with_selected(idx),
        ));
    }

    /// Open the dialog that grants the path `text` names beyond the
    /// sandbox: what `grants::propose` would grant for it, or a folder to
    /// make when it ends in `/`.
    pub fn open_sandbox_grant(&mut self, text: &str, access: Access) {
        if text.trim().is_empty() {
            let cmd = match access {
                Access::Write => "/sandbox allow",
                Access::Read => "/sandbox allow-read",
            };
            self.controller
                .conversation
                .transcript
                .push_error(format!("{cmd} takes a path"));
            return;
        }
        let asked = self.grant_path(text);
        if grant_target(&asked).is_none() {
            self.controller
                .conversation
                .transcript
                .push_error(format!("nothing of {} exists", asked.display()));
            return;
        }
        // A folder typed with a `/` at its end is made; otherwise what
        // would be granted for the path (`grants::propose`).
        let (path, make) = if text.trim().ends_with('/') {
            (asked, true)
        } else {
            propose(&asked, access).unwrap_or((asked, false))
        };
        let notes = self.controller.grant_notes(&path, access, make);
        let has_workspace = self.controller.workspace_root.is_some();
        self.modal = Some(Modal::SandboxGrant(GrantDraft::new(
            &path,
            make,
            access,
            has_workspace,
            notes,
        )));
    }

    /// The path `text` names: `~` is the home directory, and a relative
    /// one is under the working directory. What of it exists is canonical
    /// (`..` taken off), the rest as written.
    pub fn grant_path(&self, text: &str) -> PathBuf {
        let home = dirs::home_dir();
        let path = self
            .controller
            .cwd
            .join(expand_home(Path::new(text.trim()), home.as_deref()));
        let Some(base) = path.ancestors().find(|a| a.exists()) else {
            return path;
        };
        match (base.canonicalize(), path.strip_prefix(base)) {
            // Joining nothing would add a `/`.
            (Ok(existing), Ok(rest)) if rest.as_os_str().is_empty() => existing,
            (Ok(existing), Ok(rest)) => existing.join(rest),
            _ => path,
        }
    }

    /// Grant `path` beyond the sandbox, made first as a folder with
    /// `make` (`Controller::grant_sandbox`).
    pub fn grant_sandbox(
        &mut self,
        path: &Path,
        access: Access,
        scope: GrantScope,
        make: bool,
    ) -> bool {
        self.with(|c, screen| c.grant_sandbox(path, access, scope, make, screen))
    }

    /// Grant `path`, and ask the agent to try again what the sandbox kept
    /// it from: the prompt goes to the session that restarts under it.
    pub fn grant_and_retry(&mut self, path: &Path, access: Access, scope: GrantScope, make: bool) {
        if !self.grant_sandbox(path, access, scope, make) {
            return;
        }
        let verb = match access {
            Access::Write => "write",
            Access::Read => "read",
        };
        self.submit_prompt(format!(
            "You can now {verb} in {}. Please try that again.",
            path.display()
        ));
    }
}
