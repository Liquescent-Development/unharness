//! Rewind and fork: the picker. What a rewind or a fork does to the
//! sessions, and the anchors and file checkpoints, are the controller's
//! (`core::controller`, `rewind.rs`).

use super::{Action, App};
use crate::tui::modal::{ListPicker, Modal, RewindOption};

impl App {
    pub fn open_rewind_picker(&mut self) {
        if self.controller.is_generating {
            self.controller
                .conversation
                .transcript
                .push_error("finish or interrupt the current turn before rewinding");
            return;
        }
        let items: Vec<RewindOption> = self
            .controller
            .conversation
            .transcript
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(block, b)| match b {
                super::transcript::Block::User { text } => Some(RewindOption {
                    block,
                    text: text.clone(),
                    native: self
                        .controller
                        .native_anchor(self.controller.active, block)
                        .is_some(),
                    files: self.controller.checkpoint_at(block).is_some(),
                }),
                _ => None,
            })
            .collect();
        if items.is_empty() {
            self.controller
                .conversation
                .transcript
                .push_notice("nothing to rewind to yet");
            return;
        }
        let last = items.len() - 1;
        self.modal = Some(Modal::Rewind(
            ListPicker::new(items).with_selected(Some(last)),
        ));
    }

    /// Drop the user turn at `block` and everything after it, and put its
    /// text back in the prompt box (`Controller::rewind_to`).
    pub fn rewind_to(&mut self, block: usize, restore_files: bool) {
        self.with(|c, screen| c.rewind_to(block, restore_files, screen));
    }

    /// What was held for a session start that will not come (unharness
    /// quits): a rewind among it never reached the vendor session
    /// (`Controller::drop_held`).
    pub fn drop_held(&mut self, held: impl IntoIterator<Item = Action>) {
        self.with(|c, screen| c.drop_held(held, screen));
    }

    /// Continue in a copy of this conversation (`Controller::fork_conversation`).
    pub fn fork_conversation(&mut self) {
        self.with(|c, screen| c.fork_conversation(screen));
    }
}
