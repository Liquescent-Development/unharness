//! Rewind and fork: turn anchors, file checkpoints and their restore.

use super::{Action, App};
use crate::core::conversations::CheckpointRecord;
use crate::core::{HarnessId, SessionCommand};
use crate::tui::modal::{ListPicker, Modal, RewindOption};

impl App {
    /// Record the working tree as it is before the turn at `block` runs.
    pub(super) fn checkpoint_files(&mut self, block: usize) {
        let Some(cp) = self.checkpoints.clone() else {
            return;
        };
        match cp.snapshot(&self.checkpoint_name(&block.to_string())) {
            Ok(commit) => {
                self.file_checkpoints.retain(|c| c.block != block);
                self.file_checkpoints
                    .push(CheckpointRecord { block, commit });
            }
            Err(e) if !self.checkpoint_failed => {
                self.checkpoint_failed = true;
                self.transcript.push_notice(format!(
                    "could not checkpoint files (rewind will not be able to restore them): {e:#}"
                ));
            }
            Err(_) => {}
        }
    }

    /// The harness's own id for the user turn at `block`, when its session
    /// (running or resumable) can be rewound to it.
    fn native_anchor(&self, block: usize) -> Option<&str> {
        if !self.caps().rewind.conversation || !self.session_ids.contains_key(&self.active) {
            return None;
        }
        // Going back to before the session's first turn leaves nothing of
        // it: a fresh session is the same thing, and Claude refuses to
        // rewind to its first message ("stale target").
        let mut mine = self.anchors.iter().filter(|a| a.harness == self.active);
        if !mine.clone().any(|a| a.block < block) {
            return None;
        }
        mine.find(|a| a.block == block).map(|a| a.id.as_str())
    }

    /// The file checkpoint taken just before the user block at `block`.
    fn checkpoint_at(&self, block: usize) -> Option<&str> {
        self.file_checkpoints
            .iter()
            .find(|c| c.block == block)
            .map(|c| c.commit.as_str())
    }

    /// Name of a checkpoint ref for this conversation (see `core::checkpoints`).
    fn checkpoint_name(&self, suffix: &str) -> String {
        let id = &self.conversation.id;
        format!("{}/{suffix}", &id[..8.min(id.len())])
    }

    /// Whether the active harness's next session must branch the stored
    /// session id (it belongs to the conversation this one was forked from).
    pub fn fork_pending(&self) -> bool {
        self.fork_pending.contains(&self.active)
    }

    pub fn open_rewind_picker(&mut self) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before rewinding");
            return;
        }
        let items: Vec<RewindOption> = self
            .transcript
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(block, b)| match b {
                super::transcript::Block::User { text } => Some(RewindOption {
                    block,
                    text: text.clone(),
                    native: self.native_anchor(block).is_some(),
                    files: self.checkpoint_at(block).is_some(),
                }),
                _ => None,
            })
            .collect();
        if items.is_empty() {
            self.transcript.push_notice("nothing to rewind to yet");
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
        let Some(super::transcript::Block::User { text }) = self.transcript.blocks.get(block)
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
            let (Some(cp), Some(commit)) = (
                self.checkpoints.clone(),
                self.checkpoint_at(block).map(str::to_string),
            ) else {
                self.transcript
                    .push_error("there is no file checkpoint for that prompt");
                return;
            };
            self.restore_seq += 1;
            let undo_name = self.checkpoint_name(&format!("undo-{}", self.restore_seq));
            match cp.restore(&commit, &undo_name) {
                Ok(report) => {
                    files_note = format!(
                        "{} file(s) put back to how they were before that prompt (/undo-restore reverses this)",
                        report.changed
                    );
                    self.restore_undo = Some(report.undo);
                }
                Err(e) => {
                    self.transcript.push_error(format!(
                        "could not restore files, nothing was rewound: {e:#}"
                    ));
                    return;
                }
            }
        }

        let native = self.native_anchor(block).map(str::to_string);
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
            .last_active_index
            .iter()
            .filter(|(h, seen)| **h != active && **seen > block)
            .map(|(h, _)| *h)
            .collect();
        for h in stale {
            self.forget_session(h);
        }

        self.transcript.blocks.truncate(block);
        self.anchors.retain(|a| a.block < block);
        self.file_checkpoints.retain(|c| c.block < block);
        self.anchor_pending.clear();
        self.queued.clear();
        self.cursor = prompt.chars().count();
        self.input = prompt;
        self.transcript.push_system(match native {
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

    /// Reverse the last file restore (itself reversible the same way).
    pub fn undo_restore(&mut self) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn first");
            return;
        }
        let (Some(cp), Some(undo)) = (self.checkpoints.clone(), self.restore_undo.clone()) else {
            self.transcript.push_notice("no file restore to undo");
            return;
        };
        self.restore_seq += 1;
        let name = self.checkpoint_name(&format!("undo-{}", self.restore_seq));
        match cp.restore(&undo, &name) {
            Ok(report) => {
                self.restore_undo = Some(report.undo);
                self.transcript.push_system(format!(
                    "{} file(s) put back to how they were before the restore.",
                    report.changed
                ));
            }
            Err(e) => self
                .transcript
                .push_error(format!("could not undo the restore: {e:#}")),
        }
    }

    /// Continue in a copy of this conversation. Each harness's session is
    /// branched natively where the harness can (the copy then knows exactly
    /// what the original knew); the others start fresh with the transcript
    /// as context. The original and its sessions are left untouched.
    pub fn fork_conversation(&mut self) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before forking");
            return;
        }
        if self.refused_while_settling() {
            return;
        }
        if !self.conversation.has_content() {
            self.transcript.push_notice("nothing to fork yet");
            return;
        }
        self.persist();
        let original = self.conversation.id.clone();
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session();
        }
        self.idle_start_paused = false;
        // A copy ran where the original ran.
        let mut fork = self.successor();
        fork.title = self.conversation.title.clone();
        fork.last_policies = self.conversation.last_policies.clone();
        self.conversation = fork;

        let mut branched = Vec::new();
        let mut fresh = Vec::new();
        let harnesses: Vec<HarnessId> = self.session_ids.keys().copied().collect();
        for h in harnesses {
            // What its last session in this run reported counts: an ACP
            // agent declares no fork until it says it can. One not run here
            // since launch starts fresh.
            match self.caps_of(h) {
                Some(c) if c.fork && c.resume_by_id => {
                    self.fork_pending.insert(h);
                    if !c.rewind.anchors_survive_fork {
                        self.anchors.retain(|a| a.harness != h);
                    }
                    branched.push(h.as_str());
                }
                _ => {
                    self.session_ids.remove(&h);
                    self.last_active_index.remove(&h);
                    self.anchors.retain(|a| a.harness != h);
                    fresh.push(h.as_str());
                }
            }
        }
        branched.sort_unstable();
        fresh.sort_unstable();
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
        self.transcript.push_system(note);
        self.persist();
    }
}
