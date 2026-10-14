//! The conversation: starting a new one, resuming a saved one, and the
//! picker. What it holds, saving it, and putting another in its place are
//! the controller's (`core::controller`, `conversation.rs`).

use super::App;
use crate::tui::modal::{ListPicker, Modal};

impl App {
    /// Start a new conversation in this workspace, as a fresh launch would
    /// (`Controller::new_conversation`).
    pub fn new_conversation(&mut self) {
        self.with(|c, screen| c.new_conversation(screen));
    }

    /// Replace the current state with a saved conversation.
    pub fn resume_conversation(&mut self, id_or_prefix: String) {
        self.with(|c, screen| c.resume_conversation(id_or_prefix, screen));
    }

    pub fn open_resume_picker(&mut self) {
        let rows = self.controller.store.list();
        if rows.is_empty() {
            self.controller
                .conversation
                .transcript
                .push_notice("no saved conversations in this workspace");
            return;
        }
        let idx = rows
            .iter()
            .position(|r| r.id == self.controller.conversation.record.id);
        self.modal = Some(Modal::Resume(ListPicker::new(rows).with_selected(idx)));
    }
}
