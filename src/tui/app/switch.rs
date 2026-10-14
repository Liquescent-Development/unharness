//! Switching harnesses: the picker. The switch, its bridge and its
//! handoff summary are the controller's (`core::controller`, `switch.rs`).

use super::App;
use crate::core::HarnessId;
use crate::tui::modal::{ListPicker, Modal};

impl App {
    pub fn switch_harness(&mut self, next: HarnessId) {
        self.with(|c, screen| c.switch_harness(next, screen));
    }

    pub fn open_harness_picker(&mut self) {
        let idx = self
            .harness_options
            .iter()
            .position(|o| o.id == self.controller.active);
        self.modal = Some(Modal::Harness(
            ListPicker::new(self.harness_options.clone())
                .with_disabled(|o| (!o.installed).then(|| "not found on PATH".to_string()))
                .with_selected(idx),
        ));
    }

    /// Whether `id` was found when the harnesses were probed. One that was
    /// not probed is taken as there.
    pub(super) fn harness_installed(&self, id: HarnessId) -> bool {
        self.harness_options
            .iter()
            .find(|o| o.id == id)
            .is_none_or(|o| o.installed)
    }
}
