//! Rewind and fork: the picker, and what a rewind or a fork does to the
//! sessions. The anchors, file checkpoints and their restore are the
//! controller's (`core::controller::ConversationState`).

use super::{Action, App};
use crate::core::{HarnessId, SessionCommand};
use crate::tui::modal::{ListPicker, Modal, RewindOption};

impl App {
    pub fn open_rewind_picker(&mut self) {
        if self.is_generating {
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
                    native: self.controller.native_anchor(self.active, block).is_some(),
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
    /// text back in the prompt box. The active harness's session is rewound
    /// natively where it can be; any session that saw the dropped turns and
    /// cannot be rewound is replaced by a fresh one that gets the remaining
    /// conversation as context. With `restore_files`, the working tree is
    /// first put back to how it was before that turn.
    pub fn rewind_to(&mut self, block: usize, restore_files: bool) {
        // A rewind may start a session, which needs a policy.
        if self.is_generating || self.refused_while_settling() || !self.require_policy() {
            return;
        }
        let Some(super::transcript::Block::User { text }) =
            self.controller.conversation.transcript.blocks.get(block)
        else {
            return;
        };
        // The attachment markers are display only.
        let prompt: String = text
            .lines()
            .filter(|l| !(l.starts_with("[image: ") && l.ends_with(']')))
            .collect::<Vec<_>>()
            .join("\n");

        // Files first: if they cannot be restored, nothing else changes.
        let mut files_note = "files on disk are unchanged".to_string();
        if restore_files {
            match self.controller.restore_files(block) {
                Ok(note) => files_note = note,
                Err(why) => {
                    self.controller.conversation.transcript.push_error(why);
                    return;
                }
            }
        }

        let native = self
            .controller
            .native_anchor(self.active, block)
            .map(str::to_string);
        let active = self.active;
        match &native {
            Some(anchor) => {
                // A session that is not running is reattached first.
                self.ensure_session();
                self.actions
                    .push_back(Action::Command(SessionCommand::Rewind {
                        anchor: anchor.clone(),
                    }));
                // Rewound, it is the conversation's: a restart now would
                // resume it from before the rewind.
                self.mark_prompted();
            }
            None => self.forget_session(active),
        }
        // Other harnesses that were active after the rewind point saw turns
        // that no longer exist.
        let stale: Vec<HarnessId> = self
            .controller
            .conversation
            .last_active_index
            .iter()
            .filter(|(h, seen)| **h != active && **seen > block)
            .map(|(h, _)| *h)
            .collect();
        for h in stale {
            self.forget_session(h);
        }

        self.controller.rewind_conversation(block);
        self.queued.clear();
        self.cursor = prompt.chars().count();
        self.input = prompt;
        self.controller.conversation.transcript.push_system(match native {
            Some(_) => format!(
                "Rewound. {} forgot the later turns; {files_note}.",
                self.short_name()
            ),
            None => format!(
                "Rewound. {} will start a fresh session with the conversation so far; {files_note}.",
                self.short_name()
            ),
        });
        self.auto_scroll = true;
        self.persist();
    }

    /// What was held for a session start that will not come (unharness
    /// quits): a rewind among it never reached the vendor session, which
    /// is then not resumed with the turns the transcript dropped.
    pub fn drop_held(&mut self, held: impl IntoIterator<Item = Action>) {
        let rewind = held
            .into_iter()
            .any(|a| matches!(a, Action::Command(SessionCommand::Rewind { .. })));
        if rewind {
            self.forget_session(self.active);
            self.persist();
        }
    }

    /// Reverse the last file restore (`Controller::undo_restore`).
    pub fn undo_restore(&mut self) {
        if self.is_generating {
            self.controller
                .conversation
                .transcript
                .push_error("finish or interrupt the current turn first");
            return;
        }
        self.controller.undo_restore();
    }

    /// Continue in a copy of this conversation. Each harness's session is
    /// branched natively where the harness can (the copy then knows exactly
    /// what the original knew); the others start fresh with the transcript
    /// as context. The original and its sessions are left untouched.
    pub fn fork_conversation(&mut self) {
        if self.is_generating {
            self.controller
                .conversation
                .transcript
                .push_error("finish or interrupt the current turn before forking");
            return;
        }
        if self.refused_while_settling() {
            return;
        }
        if !self.controller.conversation.record.has_content() {
            self.controller
                .conversation
                .transcript
                .push_notice("nothing to fork yet");
            return;
        }
        self.persist();
        let original = self.controller.conversation.record.id.clone();
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session();
        }
        self.idle_start_paused = false;
        let (branched, fresh) = self.controller.fork(self.active);
        self.controller.reset_usage();
        let mut note = format!(
            "Forked from conversation {}; the original is unchanged.",
            &original[..8.min(original.len())]
        );
        if !branched.is_empty() {
            note.push_str(&format!(" Sessions branched: {}.", branched.join(", ")));
        }
        if !fresh.is_empty() {
            note.push_str(&format!(
                " Fresh sessions with the transcript as context: {}.",
                fresh.join(", ")
            ));
        }
        self.controller.conversation.transcript.push_system(note);
        self.persist();
    }
}
