//! The conversation: starting a new one, resuming a saved one. What it
//! holds, and saving it, are the controller's (`ConversationState`).

use super::App;
use crate::tui::modal::{ListPicker, Modal};

impl App {
    /// Write the conversation to disk (`Controller::persist`).
    pub fn persist(&mut self) {
        let prompted = self.session_seen();
        self.controller.persist(&self.config, self.active, prompted);
    }

    /// Start a new conversation in this workspace, as a fresh launch would,
    /// keeping the harness and what the user chose to run it with. The one
    /// left is saved first and stays in `/resume`; its sessions stay with
    /// it, so every harness starts afresh in the new one.
    pub fn new_conversation(&mut self) {
        if self.is_generating {
            self.controller
                .conversation
                .transcript
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
            self.controller.conversation.transcript.push_error(
                "stop the ! command first (Esc); its output belongs to this conversation",
            );
            return;
        }
        // Nothing of it is dropped: one that cannot be saved stays.
        let prompted = self.session_seen();
        if let Err(e) = self
            .controller
            .save_conversation(&self.config, self.active, prompted)
        {
            self.controller.conversation.transcript.push_error(format!(
                "could not save the conversation, so /clear keeps it: {e:#}"
            ));
            return;
        }
        let had_content = self.controller.conversation.record.has_content();
        // A session started with nothing to resume and sent nothing knows
        // nothing of this conversation, and serves the next. Any other ends,
        // a blank one resumed on a session of this conversation too, and
        // one that refused a policy, so that the next is asked for it again
        // (its process may have been started with what refused it).
        let keep = self.session_alive
            && self.session_blank
            && !self
                .controller
                .conversation
                .session_ids
                .contains_key(&self.active)
            && !self.session_refused();
        self.end_remote_control();
        let said = self.controller.conversation.transcript.blocks.len();
        if self.session_alive && !keep {
            self.shutdown_session();
        }
        // What ending it told (a change to the harness's configuration) is
        // told in the new conversation.
        let told = self
            .controller
            .conversation
            .transcript
            .blocks
            .split_off(said);
        self.idle_start_paused = false;
        let next = self.controller.successor(self.active);
        self.controller.install(next, self.active);
        self.subagent_focus = None;
        self.close_subagent_view();
        self.show_from_the_end();
        self.controller
            .conversation
            .transcript
            .push_system(if had_content {
                "New conversation; the previous one is in /resume."
            } else {
                "New conversation."
            });
        self.controller.conversation.transcript.blocks.extend(told);
        // Prompts held after a turn that did not finish are the user's, not
        // the conversation's.
        if !self.queued.is_empty() {
            self.controller.conversation.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    /// Replace the current state with a saved conversation.
    pub fn resume_conversation(&mut self, id_or_prefix: String) {
        if self.is_generating {
            self.controller
                .conversation
                .transcript
                .push_error("finish or interrupt the current turn before resuming");
            return;
        }
        if self.refused_while_settling() {
            return;
        }
        let conv = match self.controller.store.load(&id_or_prefix) {
            Ok(c) => c,
            Err(e) => {
                self.controller
                    .conversation
                    .transcript
                    .push_error(format!("could not resume: {e}"));
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
        self.controller.install(conv, active);
        self.modal = None;
        self.pending_prompts.clear();
        self.active = active;
        self.controller.take_conversation_choices(&self.config);
        self.controller
            .announce_resume(&self.config, self.active, false);
        if let Some(w) = self.policy_warning() {
            self.controller.conversation.transcript.push_notice(w);
        }
        self.require_policy();
        self.auto_scroll = true;
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
