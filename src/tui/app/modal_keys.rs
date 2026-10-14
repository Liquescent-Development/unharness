//! Keys while a dialog or picker is open.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use super::App;
use crate::core::rules::Scope;
use crate::core::sandbox::SandboxLevel;
use crate::core::{
    HarnessId, PermissionDecision, PermissionKind, PermissionPolicy, ProviderId, Rule,
};
use crate::tui::modal::{AlwaysDraft, ListPicker, Modal, QuestionStep};

/// Lines a dialog's plan or preview scrolls on PageUp/PageDown.
const PREVIEW_PAGE: i32 = 5;

impl App {
    pub fn close_modal(&mut self) {
        if let Some(decision) = self.modal.as_ref().and_then(Modal::dismiss_decision) {
            self.answer_prompt(decision);
        } else {
            self.modal = None;
            self.show_next_prompt();
        }
    }

    /// Route a key to the open modal. Local pickers close on Enter/Esc; prompt
    /// modals send their decision through `answer_prompt`.
    pub fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        let page = match key.code {
            KeyCode::PageDown => PREVIEW_PAGE,
            KeyCode::PageUp => -PREVIEW_PAGE,
            _ => 0,
        };
        if page != 0 && modal.scroll(page) {
            self.modal = Some(modal);
            return;
        }
        let mut choice = None;
        let outcome = match &mut modal {
            Modal::Harness(p) => picker_nav(p, key.code)
                .map(|c| c.and_then(|_| p.current().map(|o| ModalChoice::Harness(o.id)))),
            Modal::Provider(p) => picker_nav(p, key.code)
                .map(|c| c.and_then(|_| p.current().map(|o| ModalChoice::Provider(o.id.clone())))),
            Modal::Model(p) => picker_nav(p, key.code).map(|c| {
                c.and_then(|_| {
                    p.current()
                        .map(|m| ModalChoice::Model(m.model_ref.model.clone()))
                })
            }),
            Modal::Effort(p) => picker_nav(p, key.code)
                .map(|c| c.and_then(|_| p.current().map(|e| ModalChoice::Effort(e.clone())))),
            // `d` asks where to save the policy as the harness's default;
            // Enter then saves it there and chooses it.
            Modal::Policy(p) => match (p.save, key.code) {
                (Some(scope), KeyCode::Enter) => p
                    .list
                    .current()
                    .map(|pol| Some(ModalChoice::SavePolicy(*pol, scope))),
                (Some(_), KeyCode::Tab) => {
                    p.toggle_scope();
                    None
                }
                (Some(_), KeyCode::Esc) => {
                    p.save = None;
                    None
                }
                (Some(_), _) => None,
                (None, KeyCode::Char('d')) => {
                    p.start_save();
                    None
                }
                (None, code) => picker_nav(&mut p.list, code)
                    .map(|c| c.and_then(|_| p.list.current().map(|pol| ModalChoice::Policy(*pol)))),
            },
            Modal::Sandbox(p) => picker_nav(p, key.code)
                .map(|c| c.and_then(|_| p.current().map(|l| ModalChoice::Sandbox(*l)))),
            Modal::Subagents(p) => picker_nav(p, key.code)
                .map(|c| c.and_then(|_| p.current().map(|a| ModalChoice::Subagent(a.id.clone())))),
            Modal::Resume(p) => picker_nav(p, key.code)
                .map(|c| c.and_then(|_| p.current().map(|r| ModalChoice::Resume(r.id.clone())))),
            // Enter rewinds the conversation; `f` also restores the files.
            Modal::Rewind(p) => match key.code {
                KeyCode::Char('f') => match p.current() {
                    Some(r) if r.files => Some(Some(ModalChoice::Rewind(r.block, true))),
                    _ => None,
                },
                code => picker_nav(p, code).map(|c| {
                    c.and_then(|_| p.current().map(|r| ModalChoice::Rewind(r.block, false)))
                }),
            },
            Modal::Permission(m) => {
                if let Some(draft) = m.always.as_mut() {
                    match key.code {
                        KeyCode::Esc => m.always = None,
                        KeyCode::Tab if draft.has_workspace => {
                            draft.scope = draft.other_scope();
                        }
                        KeyCode::Backspace => {
                            draft.problem = None;
                            if let Some(pattern) = &mut draft.pattern {
                                pattern.pop();
                            }
                        }
                        // Not Ctrl+C and the like: those are not text.
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            draft.problem = None;
                            if let Some(pattern) = &mut draft.pattern {
                                pattern.push(c);
                            }
                        }
                        KeyCode::Enter => {
                            let rules = draft.rules();
                            let covers = match &m.request.kind {
                                PermissionKind::ToolUse { tool, action, .. } => self
                                    .controller
                                    .tracking
                                    .rules
                                    .would_allow(&rules, tool, action, &self.controller.cwd),
                                _ => false,
                            };
                            // A rule that would not have allowed this very
                            // request is not what was asked for.
                            if let Some(e) = rules.iter().find_map(|r| r.validate().err()) {
                                draft.problem = Some(e.to_string());
                            } else if !rules.is_empty() && !covers {
                                draft.problem = Some("that would not cover this request".into());
                            } else if !rules.is_empty() {
                                choice = Some(ModalChoice::Always(draft.scope, rules));
                            }
                        }
                        _ => {}
                    }
                    choice.map(Some)
                } else if m.denying {
                    match key.code {
                        KeyCode::Enter => {
                            let reason = if m.reason.trim().is_empty() {
                                "denied by user".to_string()
                            } else {
                                m.reason.trim().to_string()
                            };
                            Some(Some(ModalChoice::Decision(PermissionDecision::Deny {
                                reason,
                            })))
                        }
                        KeyCode::Esc => {
                            m.denying = false;
                            None
                        }
                        KeyCode::Backspace => {
                            m.reason.pop();
                            None
                        }
                        KeyCode::Char(c) => {
                            m.reason.push(c);
                            None
                        }
                        _ => None,
                    }
                } else {
                    match key.code {
                        KeyCode::Enter | KeyCode::Char('y') => {
                            Some(Some(ModalChoice::Decision(PermissionDecision::Allow {
                                updated_input: None,
                            })))
                        }
                        KeyCode::Char('a') => {
                            if let PermissionKind::ToolUse { tool, action, .. } = &m.request.kind {
                                m.always = Some(AlwaysDraft::new(
                                    self.controller.tracking.rules.propose(
                                        tool,
                                        action,
                                        &self.controller.cwd,
                                    ),
                                    self.controller.tracking.rules.has_workspace(),
                                ));
                            }
                            None
                        }
                        KeyCode::Char('n') | KeyCode::Char('d') => {
                            m.denying = true;
                            None
                        }
                        KeyCode::Char('i') => {
                            m.show_input = !m.show_input;
                            None
                        }
                        KeyCode::Esc => Some(Some(ModalChoice::Dismiss)),
                        _ => None,
                    }
                }
            }
            Modal::Question(m) => {
                let step = if m.editing_other {
                    match key.code {
                        KeyCode::Enter => m.finish_other(),
                        KeyCode::Esc => {
                            m.cancel_other();
                            QuestionStep::Stay
                        }
                        KeyCode::Backspace => {
                            m.other[m.idx].pop();
                            QuestionStep::Stay
                        }
                        KeyCode::Char(c) => {
                            m.other[m.idx].push(c);
                            QuestionStep::Stay
                        }
                        _ => QuestionStep::Stay,
                    }
                } else {
                    match key.code {
                        KeyCode::Enter => m.enter(),
                        KeyCode::Esc => QuestionStep::Dismiss,
                        KeyCode::Up | KeyCode::Char('k') => {
                            m.up();
                            QuestionStep::Stay
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            m.down();
                            QuestionStep::Stay
                        }
                        KeyCode::Char(' ') => {
                            m.choose();
                            QuestionStep::Stay
                        }
                        KeyCode::Left | KeyCode::BackTab => {
                            m.prev_page();
                            QuestionStep::Stay
                        }
                        KeyCode::Right | KeyCode::Tab => {
                            m.next_page();
                            QuestionStep::Stay
                        }
                        _ => QuestionStep::Stay,
                    }
                };
                match step {
                    QuestionStep::Stay => None,
                    QuestionStep::Submit => Some(Some(ModalChoice::Decision(
                        PermissionDecision::Answer(m.answers()),
                    ))),
                    QuestionStep::Dismiss => Some(Some(ModalChoice::Dismiss)),
                }
            }
            // Enter on a policy approves the plan under it; on the last row
            // it starts the feedback, and Enter again sends it.
            Modal::Plan(m) if m.editing => match key.code {
                KeyCode::Enter => Some(Some(ModalChoice::Decision(m.keep_planning()))),
                KeyCode::Esc => {
                    m.editing = false;
                    None
                }
                KeyCode::Backspace => {
                    m.feedback.pop();
                    None
                }
                KeyCode::Char(c) => {
                    m.feedback.push(c);
                    None
                }
                _ => None,
            },
            Modal::Plan(m) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    m.up();
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    m.down();
                    None
                }
                KeyCode::Enter => match m.current() {
                    Some(p) => Some(Some(ModalChoice::ApprovePlan(p))),
                    None => {
                        m.editing = true;
                        None
                    }
                },
                KeyCode::Esc => Some(Some(ModalChoice::Dismiss)),
                _ => None,
            },
            Modal::Confirm(_) => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => Some(Some(ModalChoice::Decision(
                    PermissionDecision::Answer(Value::Bool(true)),
                ))),
                KeyCode::Char('n') => Some(Some(ModalChoice::Decision(
                    PermissionDecision::Answer(Value::Bool(false)),
                ))),
                KeyCode::Esc => Some(Some(ModalChoice::Dismiss)),
                _ => None,
            },
            Modal::Select(m) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    m.picker.up();
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    m.picker.down();
                    None
                }
                KeyCode::Enter => Some(Some(ModalChoice::Decision(PermissionDecision::Answer(
                    m.picker
                        .current()
                        .cloned()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                )))),
                KeyCode::Esc => Some(Some(ModalChoice::Dismiss)),
                _ => None,
            },
            Modal::Input(m) => match key.code {
                KeyCode::Enter if !m.multiline || key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(Some(ModalChoice::Decision(PermissionDecision::Answer(
                        Value::String(m.text.clone()),
                    ))))
                }
                KeyCode::Enter => {
                    m.text.push('\n');
                    None
                }
                KeyCode::Backspace => {
                    m.text.pop();
                    None
                }
                KeyCode::Char(c) => {
                    m.text.push(c);
                    None
                }
                KeyCode::Esc => Some(Some(ModalChoice::Dismiss)),
                _ => None,
            },
        };

        match outcome {
            // Key handled, modal stays open.
            None => self.modal = Some(modal),
            // Modal closed without a choice (Esc on a local picker).
            Some(None) => {}
            Some(Some(choice)) => {
                self.modal = Some(modal);
                match choice {
                    ModalChoice::Harness(id) => {
                        self.modal = None;
                        self.switch_harness(id);
                    }
                    ModalChoice::Provider(id) => {
                        self.modal = None;
                        self.set_provider(ProviderId::new(id));
                        self.open_model_picker();
                    }
                    ModalChoice::Model(m) => {
                        self.modal = None;
                        self.set_model(m);
                    }
                    ModalChoice::Effort(e) => {
                        self.modal = None;
                        self.set_effort(e);
                    }
                    ModalChoice::Policy(p) => {
                        // An unavailable policy leaves the picker open.
                        if self.set_policy(p) {
                            self.modal = None;
                        }
                    }
                    ModalChoice::SavePolicy(p, scope) => {
                        if self.save_default_policy(p, scope) {
                            self.modal = None;
                        }
                    }
                    ModalChoice::Sandbox(l) => {
                        if self.set_sandbox(l) {
                            self.modal = None;
                        }
                    }
                    ModalChoice::Subagent(id) => {
                        self.modal = None;
                        self.open_subagent(&id);
                    }
                    ModalChoice::Resume(id) => {
                        self.modal = None;
                        self.resume_conversation(id);
                    }
                    ModalChoice::Rewind(block, restore_files) => {
                        self.modal = None;
                        self.rewind_to(block, restore_files);
                    }
                    ModalChoice::Decision(d) => self.answer_prompt(d),
                    // The policy goes first: Claude puts back the mode it
                    // had before plan mode only while it still plans.
                    ModalChoice::ApprovePlan(p) => {
                        if self.set_policy(p) {
                            self.answer_prompt(PermissionDecision::Allow {
                                updated_input: None,
                            });
                        }
                    }
                    ModalChoice::Always(scope, rules) => self.allow_always(scope, &rules),
                    ModalChoice::Dismiss => self.close_modal(),
                }
            }
        }
        // A request that came while a picker was open waited behind it.
        self.show_next_prompt();
    }
}

enum ModalChoice {
    /// One the picker could choose: installed.
    Harness(HarnessId),
    Provider(String),
    Model(String),
    Effort(String),
    Policy(PermissionPolicy),
    /// Choose the policy and save it as the harness's default.
    SavePolicy(PermissionPolicy, Scope),
    Sandbox(SandboxLevel),
    Subagent(String),
    Resume(String),
    /// (user block, also restore files)
    Rewind(usize, bool),
    Decision(PermissionDecision),
    /// Carry out the agent's plan under this policy.
    ApprovePlan(PermissionPolicy),
    /// Allow the request and keep these rules for the next ones like it.
    Always(Scope, Vec<Rule>),
    Dismiss,
}

/// Shared navigation for local list pickers.
/// `None` = key handled, keep open; `Some(None)` = close without choice;
/// `Some(Some(()))` = confirm current item.
fn picker_nav<T>(p: &mut ListPicker<T>, code: KeyCode) -> Option<Option<()>> {
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            p.up();
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            p.down();
            None
        }
        // Nothing that can be chosen is under the cursor of a list that has
        // rows: the picker stays. An empty one closes as before.
        KeyCode::Enter if p.current().is_none() && !p.items.is_empty() => None,
        KeyCode::Enter => Some(Some(())),
        KeyCode::Esc | KeyCode::Char('q') => Some(None),
        _ => None,
    }
}
