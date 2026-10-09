//! Modal state: list pickers and the permission/question/dialog prompts.

use serde_json::{Value, json};

use crate::core::conversations::ConversationSummary;
use crate::core::rules::{Rule, Scope};
use crate::core::sandbox::SandboxLevel;
use crate::core::{
    HarnessId, PermissionDecision, PermissionKind, PermissionPolicy, PermissionRequest, Question,
};
use crate::harness::ModelInfo;

#[derive(Debug, Clone)]
pub struct ListPicker<T> {
    pub items: Vec<T>,
    pub selected: usize,
    /// Why each row cannot be chosen, `None` for one that can. The cursor
    /// never rests on such a row.
    pub disabled: Vec<Option<String>>,
}

impl<T> ListPicker<T> {
    pub fn new(items: Vec<T>) -> Self {
        let disabled = items.iter().map(|_| None).collect();
        ListPicker {
            items,
            selected: 0,
            disabled,
        }
    }

    /// Mark the rows that cannot be chosen, each with its reason.
    pub fn with_disabled(mut self, why: impl Fn(&T) -> Option<String>) -> Self {
        self.disabled = self.items.iter().map(why).collect();
        self.settle();
        self
    }

    /// Start on row `idx`, or on the first that can be chosen if it cannot.
    pub fn with_selected(mut self, idx: Option<usize>) -> Self {
        if let Some(i) = idx
            && i < self.items.len()
        {
            self.selected = i;
        }
        self.settle();
        self
    }

    pub fn is_enabled(&self, idx: usize) -> bool {
        idx < self.items.len() && self.disabled.get(idx).is_none_or(Option::is_none)
    }

    pub fn disabled_reason(&self, idx: usize) -> Option<&str> {
        self.disabled.get(idx).and_then(Option::as_deref)
    }

    fn settle(&mut self) {
        if !self.is_enabled(self.selected)
            && let Some(i) = (0..self.items.len()).find(|&i| self.is_enabled(i))
        {
            self.selected = i;
        }
    }

    /// Move one row, wrapping, past the ones that cannot be chosen.
    fn step(&mut self, back: bool) {
        let n = self.items.len();
        for k in 1..=n {
            let i = if back {
                (self.selected + n * k - k) % n
            } else {
                (self.selected + k) % n
            };
            if self.is_enabled(i) {
                self.selected = i;
                return;
            }
        }
    }

    pub fn up(&mut self) {
        self.step(true);
    }

    pub fn down(&mut self) {
        self.step(false);
    }

    /// The row under the cursor, if it can be chosen.
    pub fn current(&self) -> Option<&T> {
        if !self.is_enabled(self.selected) {
            return None;
        }
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

/// "Allow always" before it is confirmed: the rules it would write.
#[derive(Debug, Clone)]
pub struct AlwaysDraft {
    /// As proposed; empty when no rule can cover the request.
    proposed: Vec<Rule>,
    pub scope: Scope,
    /// There is a workspace, so the scope is a choice.
    pub has_workspace: bool,
    /// A lone rule's pattern, open for editing.
    pub pattern: Option<String>,
    /// Why Enter was refused.
    pub problem: Option<String>,
}

impl AlwaysDraft {
    pub fn new(proposed: Vec<Rule>, has_workspace: bool) -> Self {
        let pattern = match proposed.as_slice() {
            [rule] => rule.pattern().map(str::to_string),
            _ => None,
        };
        AlwaysDraft {
            proposed,
            scope: if has_workspace {
                Scope::Workspace
            } else {
                Scope::Global
            },
            has_workspace,
            pattern,
            problem: None,
        }
    }

    /// The rules as they would be written now.
    pub fn rules(&self) -> Vec<Rule> {
        match (&self.pattern, self.proposed.as_slice()) {
            (Some(pattern), [rule]) => vec![rule.with_pattern(pattern.trim())],
            _ => self.proposed.clone(),
        }
    }

    pub fn other_scope(&self) -> Scope {
        match self.scope {
            Scope::Workspace => Scope::Global,
            Scope::Global => Scope::Workspace,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PermissionModal {
    pub request: PermissionRequest,
    /// Allow-always flow: the user is looking at what would be allowed.
    pub always: Option<AlwaysDraft>,
    /// Deny flow: the user is typing a reason.
    pub denying: bool,
    pub reason: String,
    pub show_input: bool,
}

#[derive(Debug, Clone)]
pub struct QuestionModal {
    pub request_id: String,
    pub questions: Vec<Question>,
    /// Current page: a question, or `questions.len()` for the review page.
    pub idx: usize,
    /// Highlighted option (or `options.len()` = the free-text "Other" row).
    pub cursor: usize,
    /// Per question, per option: chosen.
    pub chosen: Vec<Vec<bool>>,
    pub other: Vec<String>,
    pub editing_other: bool,
    /// The free text as it was when typing started, for Esc.
    other_before: String,
    /// Lines the preview pane is scrolled down.
    pub preview_scroll: u16,
    /// The furthest the preview can scroll, as last drawn.
    pub preview_max: u16,
}

/// The question has a row for a typed answer: it allows one, or it has
/// no options to choose from.
pub fn takes_text(q: &Question) -> bool {
    q.allow_other || q.options.is_empty()
}

/// What a key on the question modal leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionStep {
    Stay,
    Submit,
    Dismiss,
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
            other_before: String::new(),
            preview_scroll: 0,
            preview_max: 0,
        }
    }

    /// One single-select question is answered by choosing; anything more
    /// ends on a page that shows the answers and sends them.
    pub fn has_review(&self) -> bool {
        self.questions.len() > 1 || self.questions.iter().any(|q| q.multi)
    }

    pub fn page_count(&self) -> usize {
        self.questions.len() + usize::from(self.has_review())
    }

    pub fn on_review(&self) -> bool {
        self.idx >= self.questions.len()
    }

    /// The question on this page; none on the review page.
    pub fn current(&self) -> Option<&Question> {
        self.questions.get(self.idx)
    }

    pub fn row_count(&self) -> usize {
        self.current()
            .map_or(0, |q| q.options.len() + usize::from(takes_text(q)))
    }

    pub fn is_other_row(&self) -> bool {
        self.current()
            .is_some_and(|q| takes_text(q) && self.cursor >= q.options.len())
    }

    /// Any question has an option with a preview.
    pub fn has_previews(&self) -> bool {
        self.questions
            .iter()
            .any(|q| q.options.iter().any(|o| o.preview.is_some()))
    }

    /// The highlighted option's preview.
    pub fn preview(&self) -> Option<&str> {
        self.current()?.options.get(self.cursor)?.preview.as_deref()
    }

    pub fn up(&mut self) {
        let n = self.row_count();
        if n > 0 {
            self.cursor = if self.cursor == 0 {
                n - 1
            } else {
                self.cursor - 1
            };
            self.preview_scroll = 0;
        }
    }

    pub fn down(&mut self) {
        let n = self.row_count();
        if n > 0 {
            self.cursor = (self.cursor + 1) % n;
            self.preview_scroll = 0;
        }
    }

    pub fn scroll_preview(&mut self, lines: i32) {
        let max = i32::from(self.preview_max);
        self.preview_scroll = (i32::from(self.preview_scroll) + lines).clamp(0, max) as u16;
    }

    /// Space: pick a single-select option, toggle a multi-select one, or
    /// start typing on the "Other" row.
    pub fn choose(&mut self) {
        let Some(multi) = self.current().map(|q| q.multi) else {
            return;
        };
        if self.is_other_row() {
            self.start_other();
            return;
        }
        let row = &mut self.chosen[self.idx];
        if multi {
            row[self.cursor] = !row[self.cursor];
        } else {
            for (i, c) in row.iter_mut().enumerate() {
                *c = i == self.cursor;
            }
            self.other[self.idx].clear();
        }
    }

    /// Enter: answer this page and move on; on the review page, send.
    pub fn enter(&mut self) -> QuestionStep {
        let Some(multi) = self.current().map(|q| q.multi) else {
            return self.submit_or_show_missing();
        };
        if self.is_other_row() {
            self.start_other();
            return QuestionStep::Stay;
        }
        if !multi || !self.has_answer(self.idx) {
            self.choose();
        }
        self.advance()
    }

    fn start_other(&mut self) {
        self.editing_other = true;
        self.other_before = self.other[self.idx].clone();
    }

    /// Esc while typing on the "Other" row: put the text back as it was.
    pub fn cancel_other(&mut self) {
        self.editing_other = false;
        self.other[self.idx] = std::mem::take(&mut self.other_before);
    }

    /// Enter while typing on the "Other" row: free text answers the
    /// question, and replaces a single-select choice.
    pub fn finish_other(&mut self) -> QuestionStep {
        self.editing_other = false;
        if self.other[self.idx].trim().is_empty() {
            return QuestionStep::Stay;
        }
        if self.current().is_some_and(|q| !q.multi) {
            self.chosen[self.idx].fill(false);
        }
        self.advance()
    }

    fn advance(&mut self) -> QuestionStep {
        if self.idx + 1 < self.page_count() {
            self.go_to(self.idx + 1);
            QuestionStep::Stay
        } else {
            self.submit_or_show_missing()
        }
    }

    /// Send when every question has an answer, else go to the first one
    /// without.
    fn submit_or_show_missing(&mut self) -> QuestionStep {
        match self.unanswered().first() {
            None => QuestionStep::Submit,
            Some(&q) => {
                self.go_to(q);
                QuestionStep::Stay
            }
        }
    }

    pub fn has_answer(&self, q: usize) -> bool {
        self.chosen[q].iter().any(|c| *c) || !self.other[q].trim().is_empty()
    }

    pub fn unanswered(&self) -> Vec<usize> {
        (0..self.questions.len())
            .filter(|q| !self.has_answer(*q))
            .collect()
    }

    /// Open a page with the cursor on its answer (free text, which wins
    /// over options, before a chosen option).
    pub fn go_to(&mut self, page: usize) {
        if page >= self.page_count() {
            return;
        }
        self.idx = page;
        self.editing_other = false;
        self.preview_scroll = 0;
        self.cursor = match self.questions.get(page) {
            Some(q) if !self.other[page].trim().is_empty() => q.options.len(),
            Some(_) => self.chosen[page].iter().position(|c| *c).unwrap_or(0),
            None => 0,
        };
    }

    pub fn next_page(&mut self) -> bool {
        if self.idx + 1 < self.page_count() {
            self.go_to(self.idx + 1);
            true
        } else {
            false
        }
    }

    pub fn prev_page(&mut self) {
        if self.idx > 0 {
            self.go_to(self.idx - 1);
        }
    }

    /// A question's answer as the review page shows it.
    pub fn answer_text(&self, q: usize) -> Option<String> {
        let other = self.other[q].trim();
        if !other.is_empty() {
            return Some(other.to_string());
        }
        let labels = self.chosen_labels(q);
        (!labels.is_empty()).then(|| labels.join(", "))
    }

    fn chosen_labels(&self, q: usize) -> Vec<String> {
        self.questions[q]
            .options
            .iter()
            .zip(&self.chosen[q])
            .filter(|(_, c)| **c)
            .map(|(o, _)| o.label.clone())
            .collect()
    }

    /// `{question_id: label | [labels]}`; free text wins over options.
    pub fn answers(&self) -> Value {
        let mut map = serde_json::Map::new();
        for (qi, q) in self.questions.iter().enumerate() {
            let other = self.other[qi].trim();
            let labels = self.chosen_labels(qi);
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
    Sandbox(ListPicker<SandboxLevel>),
    /// The conversation's subagents; the chosen one's transcript is opened.
    Subagents(ListPicker<super::app::SubagentOption>),
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
                always: None,
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
            Modal::Permission(PermissionModal {
                always:
                    Some(AlwaysDraft {
                        pattern: Some(pattern),
                        problem,
                        ..
                    }),
                ..
            }) => {
                *problem = None;
                Some((pattern, false))
            }
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

    /// What a prompt modal asks of the user, in a few words.
    pub fn waiting_on(&self) -> Option<String> {
        match self {
            Modal::Permission(m) => Some(super::herdr::request_message(&m.request.kind)),
            Modal::Question(m) => Some(
                m.current()
                    .map_or_else(|| "submit answers".into(), super::herdr::question_label),
            ),
            Modal::Confirm(m) => Some(m.title.clone()),
            Modal::Select(m) => Some(m.title.clone()),
            Modal::Input(m) => Some(m.title.clone()),
            _ => None,
        }
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
    use crate::core::QuestionOption;

    fn q(id: &str, multi: bool) -> Question {
        Question {
            id: id.into(),
            header: "H".into(),
            text: id.into(),
            options: vec![QuestionOption::new("A", "a"), QuestionOption::new("B", "b")],
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
    fn list_picker_skips_rows_that_cannot_be_chosen() {
        let why = |n: &i32| (n % 2 == 0).then(|| format!("{n} is even"));
        // Asked to start on a row that cannot be chosen: the first that can.
        let mut p = ListPicker::new(vec![1, 2, 3, 4, 5])
            .with_disabled(why)
            .with_selected(Some(1));
        assert_eq!(p.current(), Some(&1));
        p.down();
        assert_eq!(p.current(), Some(&3));
        p.down();
        p.down();
        assert_eq!(p.current(), Some(&1));
        p.up();
        assert_eq!(p.current(), Some(&5));
        assert_eq!(p.disabled_reason(1), Some("2 is even"));
        assert_eq!(p.disabled_reason(0), None);

        // The order of the builders does not matter.
        let p = ListPicker::new(vec![2, 3])
            .with_selected(Some(0))
            .with_disabled(why);
        assert_eq!(p.current(), Some(&3));

        // Nothing can be chosen: nothing is current, and moving is harmless.
        let mut p = ListPicker::new(vec![2, 4]).with_disabled(why);
        p.down();
        p.up();
        assert!(p.current().is_none());
    }

    #[test]
    fn question_single_multi_and_other() {
        let mut m = QuestionModal::new("r".into(), vec![q("one", false), q("two", true)]);
        assert_eq!(m.row_count(), 3);
        assert_eq!(m.page_count(), 3); // two questions and the review page
        m.down();
        // Enter on a single-select answers it and moves to the next question.
        assert_eq!(m.enter(), QuestionStep::Stay);
        assert!(m.has_answer(0));
        assert_eq!(m.idx, 1);
        assert_eq!(m.cursor, 0);
        m.choose();
        m.down();
        m.choose(); // multi: Space toggles and stays
        assert_eq!(m.idx, 1);
        m.down();
        assert!(m.is_other_row());
        assert_eq!(m.enter(), QuestionStep::Stay);
        assert!(m.editing_other);
        m.other[1] = "custom".into();
        // Enter in the text field answers and moves to the review page.
        assert_eq!(m.finish_other(), QuestionStep::Stay);
        assert!(m.on_review() && m.current().is_none());
        assert_eq!(m.answer_text(1).as_deref(), Some("custom"));
        let a = m.answers();
        assert_eq!(a["one"], "B");
        assert_eq!(a["two"], "custom"); // free text wins
        m.other[1].clear();
        assert_eq!(m.answers()["two"], json!(["A", "B"]));
        assert_eq!(m.answer_text(1).as_deref(), Some("A, B"));
        // Only the review page sends.
        assert_eq!(m.enter(), QuestionStep::Submit);
        assert!(!m.next_page());
    }

    #[test]
    fn question_pages_move_freely_and_land_on_the_answer() {
        let qs = vec![q("one", false), q("two", false), q("three", false)];
        let mut m = QuestionModal::new("r".into(), qs);
        // Skip ahead without answering.
        assert!(m.next_page());
        m.down();
        assert_eq!(m.enter(), QuestionStep::Stay); // "two" = B, now on "three"
        assert_eq!(m.idx, 2);
        m.prev_page();
        assert_eq!((m.idx, m.cursor), (1, 1)); // back on the chosen option
        m.prev_page();
        assert_eq!((m.idx, m.cursor), (0, 0));
        m.prev_page();
        assert_eq!(m.idx, 0);
        // The review page names what is missing and Enter goes there first.
        m.go_to(3);
        assert!(m.on_review());
        assert_eq!(m.unanswered(), vec![0, 2]);
        assert_eq!(m.enter(), QuestionStep::Stay);
        assert_eq!(m.idx, 0);
        m.enter(); // "one" = A; Enter moves on page by page
        assert_eq!((m.idx, m.cursor), (1, 1));
        m.enter();
        m.enter();
        assert!(m.on_review());
        assert!(m.unanswered().is_empty());
        assert_eq!(m.enter(), QuestionStep::Submit);
        assert_eq!(m.answers(), json!({"one": "A", "two": "B", "three": "A"}));
        // Free text replaces a single-select choice, and the cursor lands on it.
        m.go_to(1);
        m.cursor = 2;
        m.choose();
        m.other[1] = "mine".into();
        m.finish_other();
        assert_eq!(m.answers()["two"], "mine");
        m.go_to(1);
        assert_eq!(m.cursor, 2);
        // Picking an option again drops the text.
        m.cursor = 0;
        m.choose();
        assert_eq!(m.answers()["two"], "A");
    }

    #[test]
    fn one_single_select_question_sends_on_enter() {
        let mut m = QuestionModal::new("r".into(), vec![q("one", false)]);
        assert!(!m.has_review());
        assert_eq!(m.page_count(), 1);
        m.down();
        assert_eq!(m.enter(), QuestionStep::Submit);
        assert_eq!(m.answers(), json!({"one": "B"}));
        // A lone multi-select one still ends on the review page.
        let mut m = QuestionModal::new("r".into(), vec![q("many", true)]);
        assert!(m.has_review());
        assert_eq!(m.enter(), QuestionStep::Stay);
        assert!(m.on_review());
        assert_eq!(m.answers(), json!({"many": ["A"]}));
        // Enter on a multi-select page with something chosen does not toggle.
        m.go_to(0);
        assert_eq!(m.enter(), QuestionStep::Stay);
        assert_eq!(m.answers(), json!({"many": ["A"]}));
    }

    #[test]
    fn esc_while_typing_puts_the_text_back() {
        let mut m = QuestionModal::new("r".into(), vec![q("one", false), q("two", false)]);
        m.enter(); // "one" = A
        m.prev_page();
        m.cursor = 2;
        m.enter();
        assert!(m.editing_other);
        m.other[0] = "foo".into();
        m.cancel_other();
        assert!(!m.editing_other);
        assert_eq!(m.answers()["one"], "A");
        // Esc after editing text that was already there restores it.
        m.cursor = 2;
        m.enter();
        m.other[0] = "kept".into();
        assert_eq!(m.finish_other(), QuestionStep::Stay);
        m.go_to(0);
        assert_eq!(m.cursor, 2); // the free text is the answer, so it is highlighted
        m.choose();
        m.other[0].push_str(" and more");
        m.cancel_other();
        assert_eq!(m.answers()["one"], "kept");
    }

    #[test]
    fn free_text_on_a_multi_select_replaces_the_ticks() {
        let mut m = QuestionModal::new("r".into(), vec![q("many", true)]);
        m.choose();
        m.down();
        m.choose();
        m.down();
        m.enter();
        m.other[0] = "neither".into();
        assert_eq!(m.finish_other(), QuestionStep::Stay);
        assert!(m.on_review());
        assert_eq!(m.answer_text(0).as_deref(), Some("neither"));
        assert_eq!(m.answers(), json!({"many": "neither"}));
        m.go_to(0);
        assert!(m.is_other_row());
    }

    #[test]
    fn a_text_row_only_where_one_is_allowed_or_needed() {
        let mut closed = q("closed", false);
        closed.allow_other = false;
        let mut open = q("open", false);
        open.options.clear();
        open.allow_other = false;
        let mut m = QuestionModal::new("r".into(), vec![closed, open]);
        assert_eq!(m.row_count(), 2);
        m.down();
        m.down();
        assert_eq!(m.cursor, 0);
        assert!(!m.is_other_row());
        m.next_page();
        assert_eq!(m.row_count(), 1);
        assert!(m.is_other_row());
        assert_eq!(m.enter(), QuestionStep::Stay);
        assert!(m.editing_other);
    }

    #[test]
    fn question_preview_follows_the_cursor() {
        let mut one = q("one", false);
        one.options[0].preview = Some("# Draft".into());
        let mut m = QuestionModal::new("r".into(), vec![one, q("two", false)]);
        assert!(m.has_previews());
        assert_eq!(m.preview(), Some("# Draft"));
        m.preview_max = 5;
        m.scroll_preview(3);
        m.scroll_preview(10);
        assert_eq!(m.preview_scroll, 5);
        m.scroll_preview(-2);
        assert_eq!(m.preview_scroll, 3);
        m.down();
        assert_eq!((m.preview(), m.preview_scroll), (None, 0));
        m.down();
        assert_eq!(m.preview(), None); // the "Other" row
        m.go_to(2);
        assert_eq!(m.preview(), None); // the review page
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
