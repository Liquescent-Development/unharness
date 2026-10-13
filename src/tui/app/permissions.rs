//! Permission requests: allow rules, the modal and the ones waiting behind it.

use serde_json::Value;

use super::{Action, App};
use crate::core::rules::{Scope, rule_may_answer};
use crate::core::{
    PermissionDecision, PermissionKind, PermissionPolicy, PermissionRequest, Rule, SessionCommand,
};
use crate::tui::modal::Modal;

impl App {
    /// Requests of a session that is gone: nobody can answer them now.
    pub(super) fn drop_prompts(&mut self) {
        if self.modal.as_ref().is_some_and(Modal::is_prompt) {
            self.modal = None;
        }
        self.pending_prompts.clear();
    }

    /// The allow rules that answer a request, if the user has them for it
    /// and it may be answered by one (`rule_may_answer`).
    fn allowing_rules(&self, req: &PermissionRequest, planning: bool) -> Option<Vec<&Rule>> {
        match &req.kind {
            PermissionKind::ToolUse { tool, action, .. } if rule_may_answer(planning, action) => {
                self.rules.allows(tool, action, &self.cwd)
            }
            _ => None,
        }
    }

    pub(super) fn on_permission_request(&mut self, req: PermissionRequest) {
        let planning = self.asked_planning.remove(&req.id)
            || self.effective_policy() == Some(PermissionPolicy::Plan);
        if let Some(handoff) = self.handoff {
            let moving = if handoff.renew {
                "The user is moving this conversation to a new session."
            } else {
                "The user is switching to another agent."
            };
            let decision = match &req.kind {
                PermissionKind::ToolUse { tool, .. } => {
                    self.transcript
                        .push_notice(format!("denied {tool}: a handoff summary uses no tools"));
                    PermissionDecision::Deny {
                        reason: format!(
                            "{moving} Write the handoff summary from what you already know, \
                             without tools."
                        ),
                    }
                }
                // Never approved unseen.
                PermissionKind::PlanApproval { .. } => PermissionDecision::Deny {
                    reason: format!(
                        "{moving} Do not carry out the plan; write the handoff summary."
                    ),
                },
                _ => PermissionDecision::Answer(Value::Null),
            };
            self.actions
                .push_back(Action::Command(SessionCommand::RespondPermission {
                    id: req.id,
                    decision,
                }));
            return;
        }
        if let Some(rules) = self.allowing_rules(&req, planning) {
            // Said every time: what runs unasked should not also run unseen.
            let rules: Vec<String> = rules.into_iter().map(Rule::describe).collect();
            let notice = match rules.as_slice() {
                [rule] => format!("allowed by your rule for {rule}"),
                rules => format!("allowed by your rules for {}", rules.join(", and for ")),
            };
            self.transcript.push_notice(notice);
            self.actions
                .push_back(Action::Command(SessionCommand::RespondPermission {
                    id: req.id,
                    decision: PermissionDecision::Allow {
                        updated_input: None,
                    },
                }));
            return;
        }
        if self.modal.is_none() {
            let mut modal = Modal::for_request(req);
            if let Modal::Plan(m) = &mut modal {
                let preferred = self.pre_plan.get(&self.active).copied();
                m.set_policies(
                    self.plan_policies(),
                    preferred.unwrap_or(PermissionPolicy::Ask),
                );
            }
            self.modal = Some(modal);
        } else {
            if planning {
                self.asked_planning.insert(req.id.clone());
            }
            self.pending_prompts.push_back(req);
        }
    }

    pub(super) fn answer_prompt(&mut self, decision: PermissionDecision) {
        if let Some(id) = self.modal.as_ref().and_then(Modal::request_id) {
            self.actions
                .push_back(Action::Command(SessionCommand::RespondPermission {
                    id: id.to_string(),
                    decision,
                }));
        }
        self.modal = None;
        self.show_next_prompt();
    }

    /// Open the next request waiting behind another modal, if none is
    /// open now: a rule added in the meantime may answer some of them.
    pub(super) fn show_next_prompt(&mut self) {
        while self.modal.is_none()
            && let Some(req) = self.pending_prompts.pop_front()
        {
            self.on_permission_request(req);
        }
    }

    /// The harness stopped waiting for request `id`: close it unanswered.
    pub(super) fn withdraw_prompt(&mut self, id: &str) {
        if self.modal.as_ref().and_then(Modal::request_id) == Some(id) {
            self.modal = None;
            self.transcript.push_notice(format!(
                "{} stopped waiting for your answer",
                self.short_name()
            ));
            self.show_next_prompt();
        }
        self.pending_prompts.retain(|req| req.id != id);
        self.asked_planning.remove(id);
    }

    /// Whether a request waits for the user, open or behind another modal.
    pub fn awaiting_answer(&self) -> bool {
        self.modal.as_ref().is_some_and(Modal::is_prompt) || !self.pending_prompts.is_empty()
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
        match self.rules.append(scope, rules) {
            Ok(path) => self.transcript.push_notice(format!(
                "from now on allowing {} {place} (see /allow; kept in {})",
                what.join(", "),
                path.display()
            )),
            Err(e) => self.transcript.push_error(format!(
                "allowed this once, but the rule could not be saved: {e:#}"
            )),
        }
        self.answer_prompt(PermissionDecision::Allow {
            updated_input: None,
        });
    }
}
