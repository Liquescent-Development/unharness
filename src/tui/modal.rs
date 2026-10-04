//! Modal state: list pickers and the permission/question/dialog prompts.

use serde_json::{Value, json};

use crate::core::conversations::ConversationSummary;
use crate::core::{
    HarnessId, PermissionDecision, PermissionKind, PermissionPolicy, PermissionRequest, Question,
};
use crate::harness::ModelInfo;

#[derive(Debug, Clone)]
pub struct ListPicker<T> {
    pub items: Vec<T>,
    pub selected: usize,
}

impl<T> ListPicker<T> {
    pub fn new(items: Vec<T>) -> Self {
        ListPicker { items, selected: 0 }
    }

    pub fn with_selected(mut self, idx: Option<usize>) -> Self {
        if let Some(i) = idx
            && i < self.items.len()
        {
            self.selected = i;
        }
        self
    }

    pub fn up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn current(&self) -> Option<&T> {
        self.items.get(self.selected)
    }
}

#[derive(Debug, Clone)]
pub struct HarnessOption {
    pub id: HarnessId,
    pub display_name: &'static str,
    pub installed: bool,
    pub version: Option<String>,
    pub interactive_permissions: bool,
}

/// A user turn the conversation can be rewound to.
#[derive(Debug, Clone)]
pub struct RewindOption {
    /// Index of the user block in the transcript.
    pub block: usize,
    pub text: String,
    /// The active harness can drop the turn from its own session; otherwise
    /// it starts a fresh session with the remaining conversation as context.
    pub native: bool,
    /// A file checkpoint from just before this prompt exists.
    pub files: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderOption {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct PermissionModal {
    pub request: PermissionRequest,
    /// Deny flow: the user is typing a reason.
    pub denying: bool,
    pub reason: String,
    pub show_input: bool,
}

#[derive(Debug, Clone)]
pub struct QuestionModal {
    pub request_id: String,
    pub questions: Vec<Question>,
    /// Current question page.
    pub idx: usize,
    /// Highlighted option (or `options.len()` = the free-text "Other" row).
    pub cursor: usize,
    /// Per question, per option: chosen.
    pub chosen: Vec<Vec<bool>>,
    pub other: Vec<String>,
    pub editing_other: bool,
}

impl QuestionModal {
    pub fn new(request_id: String, questions: Vec<Question>) -> Self {
        let chosen = questions
            .iter()
            .map(|q| vec![false; q.options.len()])
            .collect();
        let other = questions.iter().map(|_| String::new()).collect();
        QuestionModal {
            request_id,
            questions,
            idx: 0,
            cursor: 0,
            chosen,
            other,
            editing_other: false,
        }
    }

    pub fn current(&self) -> &Question {
        &self.questions[self.idx]
    }

    pub fn row_count(&self) -> usize {
        let q = self.current();
        q.options.len() + usize::from(q.allow_other)
    }

    pub fn is_other_row(&self) -> bool {
        self.cursor >= self.current().options.len()
    }

    pub fn up(&mut self) {
        let n = self.row_count();
        if n > 0 {
            self.cursor = if self.cursor == 0 {
                n - 1
            } else {
                self.cursor - 1
            };
        }
    }

    pub fn down(&mut self) {
        let n = self.row_count();
        if n > 0 {
            self.cursor = (self.cursor + 1) % n;
        }
    }

    /// Space / Enter on an option row. Returns true when the page is complete.
    pub fn choose(&mut self) -> bool {
        if self.is_other_row() {
            self.editing_other = true;
            return false;
        }
        let multi = self.current().multi;
        let row = &mut self.chosen[self.idx];
        if multi {
            row[self.cursor] = !row[self.cursor];
            false
        } else {
            for (i, c) in row.iter_mut().enumerate() {
                *c = i == self.cursor;
            }
            true
        }
    }

    pub fn has_answer(&self, q: usize) -> bool {
        self.chosen[q].iter().any(|c| *c) || !self.other[q].trim().is_empty()
    }

    pub fn next_page(&mut self) -> bool {
        if self.idx + 1 < self.questions.len() {
            self.idx += 1;
            self.cursor = 0;
            true
        } else {
            false
        }
    }

    pub fn prev_page(&mut self) {
        if self.idx > 0 {
            self.idx -= 1;
            self.cursor = 0;
        }
    }

    /// `{question_id: label | [labels]}`; free text wins over options.
    pub fn answers(&self) -> Value {
        let mut map = serde_json::Map::new();
        for (qi, q) in self.questions.iter().enumerate() {
            let other = self.other[qi].trim();
            let labels: Vec<String> = q
                .options
                .iter()
                .enumerate()
                .filter(|(i, _)| self.chosen[qi][*i])
                .map(|(_, (label, _))| label.clone())
                .collect();
            let v = if !other.is_empty() {
                Value::String(other.to_string())
            } else if q.multi {
                json!(labels)
            } else {
                Value::String(labels.first().cloned().unwrap_or_default())
            };
            map.insert(q.id.clone(), v);
        }
        Value::Object(map)
    }
}

#[derive(Debug, Clone)]
pub struct ConfirmModal {
    pub request_id: String,
    pub title: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SelectModal {
    pub request_id: String,
    pub title: String,
    pub picker: ListPicker<String>,
}

#[derive(Debug, Clone)]
pub struct InputModal {
    pub request_id: String,
    pub title: String,
    pub text: String,
    pub multiline: bool,
}

#[derive(Debug, Clone)]
pub enum Modal {
    Harness(ListPicker<HarnessOption>),
    Provider(ListPicker<ProviderOption>),
    Model(ListPicker<ModelInfo>),
    Effort(ListPicker<String>),
    Policy(ListPicker<PermissionPolicy>),
    Resume(ListPicker<ConversationSummary>),
    Rewind(ListPicker<RewindOption>),
    Permission(PermissionModal),
    Question(QuestionModal),
    Confirm(ConfirmModal),
    Select(SelectModal),
    Input(InputModal),
}

impl Modal {
    /// Build the right prompt modal for a harness request.
    pub fn for_request(req: PermissionRequest) -> Modal {
        match &req.kind {
            PermissionKind::ToolUse { .. } => Modal::Permission(PermissionModal {
                request: req,
                denying: false,
                reason: String::new(),
                show_input: false,
            }),
            PermissionKind::Question { questions } => {
                Modal::Question(QuestionModal::new(req.id.clone(), questions.clone()))
            }
            PermissionKind::Confirm { title, message } => Modal::Confirm(ConfirmModal {
                request_id: req.id.clone(),
                title: title.clone(),
                message: message.clone(),
            }),
            PermissionKind::Select { title, options } => Modal::Select(SelectModal {
                request_id: req.id.clone(),
                title: title.clone(),
                picker: ListPicker::new(options.clone()),
            }),
            PermissionKind::Input {
                title,
                prefill,
                multiline,
                ..
            } => Modal::Input(InputModal {
                request_id: req.id.clone(),
                title: title.clone(),
                text: prefill.clone().unwrap_or_default(),
                multiline: *multiline,
            }),
        }
    }

    /// True for modals that answer a harness request (vs. local pickers).
    /// The text field that is taking keystrokes, if any, and whether it
    /// holds newlines.
    pub fn text_field(&mut self) -> Option<(&mut String, bool)> {
        match self {
            Modal::Permission(m) if m.denying => Some((&mut m.reason, false)),
            Modal::Question(m) if m.editing_other => Some((&mut m.other[m.idx], false)),
            Modal::Input(m) => Some((&mut m.text, m.multiline)),
            _ => None,
        }
    }

    pub fn is_prompt(&self) -> bool {
        matches!(
            self,
            Modal::Permission(_)
                | Modal::Question(_)
                | Modal::Confirm(_)
                | Modal::Select(_)
                | Modal::Input(_)
        )
    }

    /// The request id a prompt modal answers.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Modal::Permission(m) => Some(&m.request.id),
            Modal::Question(m) => Some(&m.request_id),
            Modal::Confirm(m) => Some(&m.request_id),
            Modal::Select(m) => Some(&m.request_id),
            Modal::Input(m) => Some(&m.request_id),
            _ => None,
        }
    }

    /// The decision to send when the user dismisses the modal with Esc.
    pub fn dismiss_decision(&self) -> Option<PermissionDecision> {
        match self {
            Modal::Permission(_) => Some(PermissionDecision::Deny {
                reason: "cancelled by user".into(),
            }),
            Modal::Question(_) | Modal::Confirm(_) | Modal::Select(_) | Modal::Input(_) => {
                Some(PermissionDecision::Answer(Value::Null))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: &str, multi: bool) -> Question {
        Question {
            id: id.into(),
            header: "H".into(),
            text: id.into(),
            options: vec![("A".into(), "a".into()), ("B".into(), "b".into())],
            allow_other: true,
            multi,
        }
    }

    #[test]
    fn list_picker_wraps() {
        let mut p = ListPicker::new(vec![1, 2, 3]).with_selected(Some(2));
        assert_eq!(p.current(), Some(&3));
        p.down();
        assert_eq!(p.selected, 0);
        p.up();
        assert_eq!(p.selected, 2);
        let mut e: ListPicker<i32> = ListPicker::new(vec![]);
        e.up();
        e.down();
        assert!(e.current().is_none());
    }

    #[test]
    fn question_single_multi_and_other() {
        let mut m = QuestionModal::new("r".into(), vec![q("one", false), q("two", true)]);
        assert_eq!(m.row_count(), 3);
        m.down();
        assert!(m.choose()); // single-select completes the page
        assert!(m.has_answer(0));
        assert!(m.next_page());
        m.choose();
        m.down();
        m.choose(); // multi: toggles, page not auto-complete
        m.down();
        assert!(m.is_other_row());
        m.choose();
        assert!(m.editing_other);
        m.other[1] = "custom".into();
        assert!(!m.next_page());
        let a = m.answers();
        assert_eq!(a["one"], "B");
        assert_eq!(a["two"], "custom"); // free text wins
        m.other[1].clear();
        assert_eq!(m.answers()["two"], json!(["A", "B"]));
        m.prev_page();
        assert_eq!(m.idx, 0);
    }

    #[test]
    fn modal_for_request_and_dismiss() {
        let req = PermissionRequest {
            id: "x".into(),
            kind: PermissionKind::Confirm {
                title: "Sure?".into(),
                message: None,
            },
            tool_call_id: None,
        };
        let m = Modal::for_request(req);
        assert!(m.is_prompt());
        assert_eq!(m.request_id(), Some("x"));
        assert_eq!(
            m.dismiss_decision(),
            Some(PermissionDecision::Answer(Value::Null))
        );
        let local = Modal::Effort(ListPicker::new(vec!["high".into()]));
        assert!(!local.is_prompt());
        assert!(local.dismiss_decision().is_none());
    }
}
