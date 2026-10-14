//! The sandbox level the active harness's processes run at.

use super::App;
use crate::core::sandbox::{Sandbox, SandboxLevel};
use crate::tui::modal::{ListPicker, Modal};

impl App {
    /// The sandbox level the active harness runs at, and why it is off if
    /// that was not asked for.
    pub fn sandbox_level(&self) -> (SandboxLevel, Option<String>) {
        self.sandbox.level(self.harness().default_sandbox())
    }

    /// What confines the next process of the active harness.
    pub fn session_sandbox(&self) -> anyhow::Result<Sandbox> {
        crate::runner::session_sandbox(
            self.harness(),
            &self.sandbox,
            &self.config,
            self.workspace_root.as_deref().unwrap_or(&self.cwd),
        )
    }

    /// Choose the sandbox level for the run. Returns whether it could be
    /// set. The sandbox is applied when a process is spawned, so a live
    /// session is shut down and comes back under the new level, resumed
    /// where the harness can.
    pub fn set_sandbox(&mut self, level: SandboxLevel) -> bool {
        if self.is_generating {
            self.controller
                .conversation
                .transcript
                .push_error("finish or interrupt the current turn before changing the sandbox");
            return false;
        }
        if let Err(why) = &self.sandbox.backend
            && level != SandboxLevel::Off
        {
            self.controller
                .conversation
                .transcript
                .push_error(format!("sandbox '{level}' is unavailable: {why}"));
            return false;
        }
        self.sandbox.explicit = Some(level);
        self.idle_start_paused = false;
        let restart = self.session_alive && self.session_sandbox_level != Some(level);
        self.controller
            .conversation
            .transcript
            .push_system(if restart {
                format!(
                    "Sandbox: {level} ({} restarts under it)",
                    self.display_name()
                )
            } else {
                format!("Sandbox: {level}")
            });
        if restart {
            self.shutdown_session();
        }
        true
    }

    /// The sandbox levels there is a backend for.
    pub(super) fn available_sandbox_levels(&self) -> Vec<SandboxLevel> {
        SandboxLevel::ALL
            .into_iter()
            .filter(|l| self.sandbox.backend.is_ok() || *l == SandboxLevel::Off)
            .collect()
    }

    pub fn open_sandbox_picker(&mut self) {
        let wanted = self.sandbox_level().0;
        let idx = SandboxLevel::ALL.iter().position(|l| *l == wanted);
        let unavailable = self.sandbox.backend.is_err();
        self.modal = Some(Modal::Sandbox(
            ListPicker::new(SandboxLevel::ALL.to_vec())
                // Why is said once, above the list.
                .with_disabled(|l| {
                    (unavailable && *l != SandboxLevel::Off).then(|| "no sandbox here".to_string())
                })
                .with_selected(idx),
        ));
    }
}
