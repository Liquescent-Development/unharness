//! The view the controller works on (`core::controller::Front`): the
//! parts of the App its logic reads and changes as it goes, borrowed
//! beside the controller for one call (`App::split`).

use std::time::Instant;

use super::{App, ListRequest, TranscriptView};
use crate::core::controller::{Controller, Front, GrantOffer, PlanAsk, PolicyAsk};
use crate::core::{Attachment, PermissionPolicy, PermissionRequest};
use crate::tui::modal::{GrantDraft, ListPicker, Modal, PolicyPicker};
use crate::tui::selection::{Point, Selection};

/// The App's view, borrowed beside its controller.
pub struct Screen<'a> {
    modal: &'a mut Option<Modal>,
    input: &'a mut String,
    cursor: &'a mut usize,
    attachments: &'a mut Vec<Attachment>,
    flash: &'a mut Option<(String, Instant)>,
    auto_scroll: &'a mut bool,
    scroll: &'a mut u16,
    transcript_view: &'a mut TranscriptView,
    selection: &'a mut Option<Selection>,
    last_click: &'a mut Option<(Instant, Point, u8)>,
    subagent_focus: &'a mut Option<String>,
    viewing: &'a mut Option<String>,
    main_scroll: &'a mut (u16, bool),
    open_when_listed: &'a mut Option<ListRequest>,
    commands_pending: &'a mut bool,
}

impl App {
    /// The controller, and the view it reads and changes as it works.
    pub(crate) fn split(&mut self) -> (&mut Controller, Screen<'_>) {
        let App {
            controller,
            modal,
            input,
            cursor,
            attachments,
            flash,
            auto_scroll,
            scroll,
            transcript_view,
            selection,
            last_click,
            subagent_focus,
            viewing,
            main_scroll,
            open_when_listed,
            commands_pending,
            ..
        } = self;
        (
            controller,
            Screen {
                modal,
                input,
                cursor,
                attachments,
                flash,
                auto_scroll,
                scroll,
                transcript_view,
                selection,
                last_click,
                subagent_focus,
                viewing,
                main_scroll,
                open_when_listed,
                commands_pending,
            },
        )
    }

    /// Run `f` on the controller and the view it works on, then bring the
    /// view up to date with what it was told meanwhile.
    pub(super) fn with<R>(&mut self, f: impl FnOnce(&mut Controller, &mut Screen<'_>) -> R) -> R {
        let (controller, mut screen) = self.split();
        let r = f(controller, &mut screen);
        self.settle();
        r
    }

    /// Bring the view up to date with what the controller told it while
    /// it worked (`App::split`): the suggestions, if the harness's
    /// commands arrived.
    pub(crate) fn settle(&mut self) {
        if std::mem::take(&mut self.commands_pending) {
            self.commands_arrived();
        }
    }
}

impl Screen<'_> {
    /// Another transcript is to be drawn: nothing kept from the last one applies.
    pub(super) fn show_from_the_end(&mut self) {
        *self.transcript_view = TranscriptView::default();
        *self.selection = None;
        *self.last_click = None;
        *self.scroll = 0;
        *self.auto_scroll = true;
    }

    /// Back to the main transcript, where it was.
    pub(super) fn close_subagent_view(&mut self) {
        if self.viewing.take().is_some() {
            self.show_from_the_end();
            (*self.scroll, *self.auto_scroll) = *self.main_scroll;
        }
    }
}

/// The policy picker for `ask`.
pub(super) fn policy_picker(ask: PolicyAsk) -> Modal {
    let PolicyAsk {
        harness,
        offered,
        start,
        has_workspace,
    } = ask;
    let idx = start.and_then(|s| PermissionPolicy::ALL.iter().position(|p| *p == s));
    Modal::Policy(PolicyPicker {
        list: ListPicker::new(PermissionPolicy::ALL.to_vec())
            .with_disabled(|p| (!offered.contains(p)).then(|| format!("not offered by {harness}")))
            .with_selected(idx),
        save: None,
        has_workspace,
    })
}

impl Front for Screen<'_> {
    fn offer_grant(&mut self, offer: GrantOffer) {
        *self.modal = Some(Modal::SandboxGrant(GrantDraft::offered(offer)));
    }

    fn dialog_open(&self) -> bool {
        self.modal.as_ref().is_some_and(|m| !m.yields())
    }

    fn ask(&mut self, req: PermissionRequest, plan: Option<PlanAsk>) {
        let mut modal = Modal::for_request(req);
        if let (Modal::Plan(m), Some(plan)) = (&mut modal, plan) {
            m.set_policies(plan.policies, plan.preferred);
        }
        *self.modal = Some(modal);
    }

    fn withdraw(&mut self, id: &str) -> bool {
        if self.modal.as_ref().and_then(Modal::request_id) == Some(id) {
            *self.modal = None;
            return true;
        }
        false
    }

    fn drop_request(&mut self) {
        if self.modal.as_ref().is_some_and(Modal::is_prompt) {
            *self.modal = None;
        }
    }

    fn choosing_policy(&self) -> bool {
        matches!(&self.modal, Some(Modal::Policy(p)) if p.save.is_none())
    }

    fn close_policy_picker(&mut self) {
        *self.modal = None;
    }

    fn ask_policy(&mut self, ask: PolicyAsk) {
        *self.modal = Some(policy_picker(ask));
    }

    fn attachments(&mut self) -> &mut Vec<Attachment> {
        self.attachments
    }

    fn put_back(&mut self, text: String, attachments: Vec<Attachment>) {
        *self.cursor = text.chars().count();
        *self.input = if self.input.is_empty() {
            text
        } else {
            format!("{text}\n{}", self.input)
        };
        self.attachments.extend(attachments);
    }

    fn edit(&mut self, text: String) {
        *self.cursor = text.chars().count();
        *self.input = text;
    }

    fn follow(&mut self) {
        *self.auto_scroll = true;
    }

    fn flash(&mut self, message: String) {
        *self.flash = Some((message, Instant::now()));
    }

    fn prompt_submitted(&mut self) {
        // A picker the user asked for and moved on from stays shut.
        *self.open_when_listed = None;
    }

    fn commands_arrived(&mut self) {
        // Brought up to date once the controller is done (`App::with`):
        // the list is the App's, made from its input and its harness's
        // commands.
        *self.commands_pending = true;
    }

    fn conversation_installed(&mut self) {
        *self.modal = None;
        *self.subagent_focus = None;
        self.close_subagent_view();
        self.show_from_the_end();
    }
}
