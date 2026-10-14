//! Permission requests: the notice for what `core::permissions` answers
//! (an allow rule, or a handoff summary being written), the dialog the
//! front end opens for what it leaves to the user, and the requests
//! waiting behind another dialog.

use super::{Action, Controller, Front, PlanAsk};
use crate::core::permissions::{HandoffTo, Route, Router, Why};
use crate::core::{
    PermissionDecision, PermissionKind, PermissionPolicy, PermissionRequest, SessionCommand,
};

impl Controller {
    /// Requests of a session that is gone: nobody can answer them now.
    pub(super) fn drop_prompts(&mut self, front: &mut dyn Front) {
        front.drop_request();
        self.pending_prompts.clear();
    }

    pub(super) fn on_permission_request(&mut self, req: PermissionRequest, front: &mut dyn Front) {
        let planning = self.asked_planning.remove(&req.id)
            || self.effective_policy(self.active) == Some(PermissionPolicy::Plan);
        let router = Router {
            rules: &self.rules,
            cwd: &self.cwd,
            planning,
            handoff: self.handoff.map(|h| {
                if h.renew {
                    HandoffTo::NewSession
                } else {
                    HandoffTo::AnotherAgent
                }
            }),
        };
        if let Route::Answered { decision, why } = router.route(&req) {
            match (why, &req.kind) {
                (Why::Handoff, PermissionKind::ToolUse { tool, .. }) => self
                    .conversation
                    .transcript
                    .push_notice(format!("denied {tool}: a handoff summary uses no tools")),
                (Why::Handoff, _) => {}
                // Said every time: what runs unasked should not also run unseen.
                (Why::Rules(rules), _) => {
                    let notice = match rules.as_slice() {
                        [rule] => format!("allowed by your rule for {rule}"),
                        rules => {
                            format!("allowed by your rules for {}", rules.join(", and for "))
                        }
                    };
                    self.conversation.transcript.push_notice(notice);
                }
            }
            self.actions
                .push_back(Action::Command(SessionCommand::RespondPermission {
                    id: req.id,
                    decision,
                }));
            return;
        }
        if !front.dialog_open() {
            let plan = matches!(req.kind, PermissionKind::PlanApproval { .. }).then(|| {
                let preferred = self.policy.pre_plan(self.active);
                PlanAsk {
                    policies: self.plan_policies(self.active),
                    preferred: preferred.unwrap_or(PermissionPolicy::Ask),
                }
            });
            front.ask(req, plan);
        } else {
            if planning {
                self.asked_planning.insert(req.id.clone());
            }
            self.pending_prompts.push_back(req);
        }
    }

    /// The user answered request `id`.
    pub fn respond(&mut self, id: String, decision: PermissionDecision) {
        self.actions
            .push_back(Action::Command(SessionCommand::RespondPermission {
                id,
                decision,
            }));
    }

    /// Open the next request waiting behind another dialog, if none is
    /// open now: a rule added in the meantime may answer some of them.
    pub fn show_next_prompt(&mut self, front: &mut dyn Front) {
        while !front.dialog_open()
            && let Some(req) = self.pending_prompts.pop_front()
        {
            self.on_permission_request(req, front);
        }
    }

    /// The harness stopped waiting for request `id`: close it unanswered.
    pub(super) fn withdraw_prompt(&mut self, id: &str, front: &mut dyn Front) {
        if front.withdraw(id) {
            self.conversation.transcript.push_notice(format!(
                "{} stopped waiting for your answer",
                self.short_name()
            ));
            self.show_next_prompt(front);
        }
        self.pending_prompts.retain(|req| req.id != id);
        self.asked_planning.remove(id);
    }
}
