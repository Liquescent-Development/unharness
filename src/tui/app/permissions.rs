//! Permission requests in the TUI: the modal the controller opens for what
//! `core::permissions` leaves to the user (`core::controller`,
//! `requests.rs`), answering it, and "allow always".

use super::App;
use crate::core::rules::Scope;
use crate::core::{PermissionDecision, Rule};
use crate::tui::modal::Modal;

impl App {
    pub(super) fn answer_prompt(&mut self, decision: PermissionDecision) {
        if let Some(id) = self.modal.as_ref().and_then(Modal::request_id) {
            let id = id.to_string();
            self.controller.respond(id, decision);
        }
        self.modal = None;
        self.show_next_prompt();
    }

    /// Open the next request waiting behind another modal, if none is
    /// open now: a rule added in the meantime may answer some of them.
    pub(super) fn show_next_prompt(&mut self) {
        self.with(|c, screen| c.show_next_prompt(screen));
    }

    /// Whether a request waits for the user, open or behind another modal.
    pub fn awaiting_answer(&self) -> bool {
        self.modal.as_ref().is_some_and(Modal::is_prompt)
            || !self.controller.tracking.pending_prompts.is_empty()
    }

    /// "Allow always": the request is allowed, and so is what the rules
    /// cover from now on, on any harness. The harness itself is only told
    /// "allow": what it would remember, it would remember in its own
    /// settings, or forget with the session.
    pub(super) fn allow_always(&mut self, scope: Scope, rules: &[Rule]) {
        let what: Vec<String> = rules.iter().map(Rule::describe).collect();
        let place = match scope {
            Scope::Workspace => "in this workspace",
            Scope::Global => "in every workspace",
        };
        match self.controller.tracking.rules.append(scope, rules) {
            Ok(path) => self.controller.conversation.transcript.push_notice(format!(
                "from now on allowing {} {place} (see /allow; kept in {})",
                what.join(", "),
                path.display()
            )),
            Err(e) => self.controller.conversation.transcript.push_error(format!(
                "allowed this once, but the rule could not be saved: {e:#}"
            )),
        }
        self.answer_prompt(PermissionDecision::Allow {
            updated_input: None,
        });
    }
}
