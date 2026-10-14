//! The conversation: saving it, starting a new one, resuming a saved one.

use super::App;
use crate::core::conversations::{Conversation, now_rfc3339, truncate_title};
use crate::core::transcript::Transcript;
use crate::core::{ContextUsage, Usage};
use crate::tui::modal::{ListPicker, Modal};

impl App {
    /// Printed after the TUI closes, when a conversation was saved.
    pub fn exit_summary(&self) -> Option<String> {
        if !self.conversation.has_content() {
            return None;
        }
        let id = &self.conversation.id;
        let short = &id[..8.min(id.len())];
        let mut harnesses: Vec<&str> = self.session_ids.keys().map(|h| h.as_str()).collect();
        harnesses.sort_unstable();
        Some(format!(
            "Conversation {short} saved ({}). Resume it with:\n  unharness --resume {short}",
            if harnesses.is_empty() {
                "no sessions yet".to_string()
            } else {
                harnesses.join(", ")
            }
        ))
    }

    pub(super) fn sync_conversation(&mut self) {
        let c = &mut self.conversation;
        c.active_harness = self.active;
        c.sessions = self.session_ids.clone();
        c.bookmarks = self.last_active_index.clone();
        c.blocks = self.transcript.to_records();
        c.usage.insert(self.active, self.session_usage.clone());
        c.plan = self.plan.clone();
        c.anchors = self.anchors.clone();
        c.checkpoints = self.file_checkpoints.clone();
        c.fork_pending = self.fork_pending.iter().copied().collect();
        c.fork_pending.sort_by_key(|h| h.as_str());
        self.controller.sync_conversation(
            &self.config,
            self.active,
            self.session_seen(),
            &mut self.conversation,
        );
        let c = &mut self.conversation;
        if c.title.is_empty()
            && let Some(p) = &self.first_prompt
        {
            c.title = truncate_title(p);
        }
        c.updated_at = now_rfc3339();
    }

    /// Write the conversation to disk. Failures are reported once per run.
    pub fn persist(&mut self) {
        if let Err(e) = self.save_conversation()
            && !self.persist_failed
        {
            self.persist_failed = true;
            self.transcript
                .push_error(format!("could not save the conversation: {e:#}"));
        }
    }

    /// Write the conversation to disk, if it has anything to keep.
    fn save_conversation(&mut self) -> anyhow::Result<()> {
        self.sync_conversation();
        if !self.conversation.has_content() {
            return Ok(());
        }
        self.store.save(&self.conversation)
    }

    /// Take what the resumed conversation was run with, its policy, models
    /// and efforts, except where this run named its own
    /// (`Controller::take_conversation`).
    pub(super) fn take_conversation_choices(&mut self) {
        for notice in self
            .controller
            .take_conversation(&self.config, &self.conversation)
        {
            self.transcript.push_notice(notice);
        }
    }

    /// Say which conversation was resumed, and whether its harness runs
    /// under another policy than it last did. `at_start`: by `--resume`.
    pub(super) fn announce_resume(&mut self, at_start: bool) {
        let summary = self.conversation.summary();
        self.transcript.push_notice(format!(
            "resumed conversation {} ({}); continuing on {}",
            &summary.id[..8.min(summary.id.len())],
            summary
                .harnesses
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.short_name()
        ));
        let Some(last) = self.conversation.last_policies.get(&self.active).copied() else {
            return;
        };
        if let Some(notice) =
            self.controller
                .resumed_under(&self.config, self.active, last, at_start)
        {
            self.transcript.push_notice(notice);
        }
    }

    /// Start a new conversation in this workspace, as a fresh launch would,
    /// keeping the harness and what the user chose to run it with. The one
    /// left is saved first and stays in `/resume`; its sessions stay with
    /// it, so every harness starts afresh in the new one.
    pub fn new_conversation(&mut self) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before clearing");
            return;
        }
        if self.refused_while_settling() {
            return;
        }
        // They would end with the session, and report into the new one.
        if self.refused_for_subagents("clearing") {
            return;
        }
        if self.shell.is_some() {
            self.transcript.push_error(
                "stop the ! command first (Esc); its output belongs to this conversation",
            );
            return;
        }
        // Nothing of it is dropped: one that cannot be saved stays.
        if let Err(e) = self.save_conversation() {
            self.transcript.push_error(format!(
                "could not save the conversation, so /clear keeps it: {e:#}"
            ));
            return;
        }
        let had_content = self.conversation.has_content();
        // A session started with nothing to resume and sent nothing knows
        // nothing of this conversation, and serves the next. Any other ends,
        // a blank one resumed on a session of this conversation too, and
        // one that refused a policy, so that the next is asked for it again
        // (its process may have been started with what refused it).
        let keep = self.session_alive
            && self.session_blank
            && !self.session_ids.contains_key(&self.active)
            && !self.session_refused();
        self.end_remote_control();
        let said = self.transcript.blocks.len();
        if self.session_alive && !keep {
            self.shutdown_session();
        }
        // What ending it told (a change to the harness's configuration) is
        // told in the new conversation.
        let told = self.transcript.blocks.split_off(said);
        self.idle_start_paused = false;
        self.conversation = self.successor();
        self.controller.policy.forget_plans();
        self.transcript = Transcript::default();
        self.session_ids.clear();
        self.last_active_index.clear();
        self.fork_pending.clear();
        self.plan.clear();
        self.anchors.clear();
        self.file_checkpoints.clear();
        self.restore_undo = None;
        self.first_prompt = None;
        self.generation_duration = None;
        self.reset_usage();
        self.subagent_focus = None;
        self.close_subagent_view();
        self.show_from_the_end();
        self.transcript.push_system(if had_content {
            "New conversation; the previous one is in /resume."
        } else {
            "New conversation."
        });
        self.transcript.blocks.extend(told);
        // Prompts held after a turn that did not finish are the user's, not
        // the conversation's.
        if !self.queued.is_empty() {
            self.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    /// A new conversation that keeps the models and efforts this one chose.
    pub(super) fn successor(&self) -> Conversation {
        let mut next = Conversation::new(self.active);
        next.models = self.conversation.models.clone();
        next.efforts = self.conversation.efforts.clone();
        next
    }

    /// Usage and context start again with a session that knows nothing.
    pub(super) fn reset_usage(&mut self) {
        self.anchor_pending.clear();
        self.session_usage = Usage::default();
        self.turn_usage = Usage::default();
        self.context = ContextUsage::default();
    }

    /// Replace the current state with a saved conversation.
    pub fn resume_conversation(&mut self, id_or_prefix: String) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before resuming");
            return;
        }
        if self.refused_while_settling() {
            return;
        }
        let conv = match self.store.load(&id_or_prefix) {
            Ok(c) => c,
            Err(e) => {
                self.transcript.push_error(format!("could not resume: {e}"));
                return;
            }
        };
        // Another conversation, perhaps on another harness.
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session();
        }
        self.idle_start_paused = false;
        self.persist();
        let active = if self.registry().get(conv.active_harness).is_some() {
            conv.active_harness
        } else {
            self.active
        };
        self.session_ids = conv.sessions.clone();
        self.last_active_index = conv.bookmarks.clone();
        self.transcript = Transcript::from_records(&conv.blocks);
        self.session_usage = conv.usage.get(&active).cloned().unwrap_or_default();
        self.turn_usage = Usage::default();
        self.context = ContextUsage::default();
        self.plan = conv.plan.clone();
        self.anchors = conv.anchors.clone();
        self.file_checkpoints = conv.checkpoints.clone();
        self.fork_pending = conv.fork_pending.iter().copied().collect();
        self.restore_undo = None;
        self.anchor_pending.clear();
        self.first_prompt = (!conv.title.is_empty()).then(|| conv.title.clone());
        self.generation_duration = None;
        self.modal = None;
        self.pending_prompts.clear();
        self.active = active;
        self.conversation = conv;
        self.controller.policy.forget_plans();
        self.take_conversation_choices();
        self.announce_resume(false);
        if let Some(w) = self.policy_warning() {
            self.transcript.push_notice(w);
        }
        self.require_policy();
        self.auto_scroll = true;
    }

    pub fn open_resume_picker(&mut self) {
        let rows = self.store.list();
        if rows.is_empty() {
            self.transcript
                .push_notice("no saved conversations in this workspace");
            return;
        }
        let idx = rows.iter().position(|r| r.id == self.conversation.id);
        self.modal = Some(Modal::Resume(ListPicker::new(rows).with_selected(idx)));
    }
}
