//! Rewind and fork: what a rewind or a fork does to the sessions. The
//! anchors, file checkpoints and their restore are the conversation's
//! (`ConversationState`).

use super::{Action, Controller, Front};
use crate::core::transcript::Block;
use crate::core::{HarnessId, SessionCommand};

impl Controller {
    /// Drop the user turn at `block` and everything after it, and put its
    /// text back in the prompt box. The active harness's session is rewound
    /// natively where it can be; any session that saw the dropped turns and
    /// cannot be rewound is replaced by a fresh one that gets the remaining
    /// conversation as context. With `restore_files`, the working tree is
    /// first put back to how it was before that turn.
    pub fn rewind_to(&mut self, block: usize, restore_files: bool, front: &mut dyn Front) {
        // A rewind may start a session, which needs a policy.
        if self.is_generating || self.refused_while_settling() || !self.require_policy(front) {
            return;
        }
        let Some(Block::User { text, .. }) = self.conversation.transcript.blocks.get(block) else {
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
            match self.restore_files(block) {
                Ok(note) => files_note = note,
                Err(why) => {
                    self.conversation.transcript.push_error(why);
                    return;
                }
            }
        }

        let native = self.native_anchor(self.active, block).map(str::to_string);
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
                // It holds less than it said; it says how much at its next
                // request.
                self.conversation.context.used = None;
            }
            None => self.forget_session(active, front),
        }
        // Other harnesses that were active after the rewind point saw turns
        // that no longer exist.
        let stale: Vec<HarnessId> = self
            .conversation
            .last_active_index
            .iter()
            .filter(|(h, seen)| **h != active && **seen > block)
            .map(|(h, _)| *h)
            .collect();
        for h in stale {
            self.forget_session(h, front);
        }

        self.rewind_conversation(block);
        self.queued.clear();
        front.edit(prompt);
        self.conversation.transcript.push_system(match native {
            Some(_) => format!(
                "Rewound. {} forgot the later turns; {files_note}.",
                self.short_name()
            ),
            None => format!(
                "Rewound. {} will start a fresh session with the conversation so far; {files_note}.",
                self.short_name()
            ),
        });
        front.follow();
        self.persist();
    }

    /// What was held for a session start that will not come (unharness
    /// quits): a rewind among it never reached the vendor session, which
    /// is then not resumed with the turns the transcript dropped.
    pub fn drop_held(&mut self, held: impl IntoIterator<Item = Action>, front: &mut dyn Front) {
        let rewind = held
            .into_iter()
            .any(|a| matches!(a, Action::Command(SessionCommand::Rewind { .. })));
        if rewind {
            self.forget_session(self.active, front);
            self.persist();
        }
    }

    /// Continue in a copy of this conversation. Each harness's session is
    /// branched natively where the harness can (the copy then knows exactly
    /// what the original knew); the others start fresh with the transcript
    /// as context. The original and its sessions are left untouched.
    pub fn fork_conversation(&mut self, front: &mut dyn Front) {
        if self.is_generating {
            self.conversation
                .transcript
                .push_error("finish or interrupt the current turn before forking");
            return;
        }
        if self.refused_while_settling() {
            return;
        }
        if !self.conversation.record.has_content() {
            self.conversation
                .transcript
                .push_notice("nothing to fork yet");
            return;
        }
        self.persist();
        let original = self.conversation.record.id.clone();
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session(front);
        }
        self.idle_start_paused = false;
        let (branched, fresh) = self.fork(self.active);
        self.reset_usage();
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
        self.conversation.transcript.push_system(note);
        self.persist();
    }
}
