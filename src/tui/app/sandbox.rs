//! The sandbox level the active harness's processes run at: the picker.
//! The level is the controller's (`core::controller`, `sandbox.rs`).

use super::App;
use crate::core::sandbox::{Sandbox, SandboxLevel};
use crate::tui::modal::{ListPicker, Modal};

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
}
