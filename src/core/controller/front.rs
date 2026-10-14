//! What the controller asks of, and tells, the front end that shows it,
//! at the points where its logic reads or changes what is on screen: the
//! dialog a permission request is answered in, the policy picker, the
//! prompt box, the status rule. The TUI's `App` is the front end with a
//! screen (`tui::app::Screen`); nothing here builds or draws a dialog.

use crate::core::{Attachment, PermissionPolicy, PermissionRequest};

/// The view a controller works on. Called at the exact points where the
/// TUI's state machine read or changed its view before it moved here.
pub trait Front {
    /// A dialog is open: a request that comes waits behind it.
    fn dialog_open(&self) -> bool;
    /// Open the dialog that asks the user about `req`; for a plan, with the
    /// policies it can be carried out under (`plan`).
    fn ask(&mut self, req: PermissionRequest, plan: Option<PlanAsk>);
    /// The harness stopped waiting for request `id`: its dialog is closed
    /// if it is the one open. Returns whether it was.
    fn withdraw(&mut self, id: &str) -> bool;
    /// Nobody can answer a request any more (its session is gone): a
    /// request's dialog that is open is closed.
    fn drop_request(&mut self);
    /// The policy picker is open on what the session offers (not saving a
    /// default).
    fn choosing_policy(&self) -> bool;
    /// Close the policy picker.
    fn close_policy_picker(&mut self);
    /// Ask the user for a policy: the picker, as `ask` has it.
    fn ask_policy(&mut self, ask: PolicyAsk);
    /// What goes out with the next prompt besides its text.
    fn attachments(&mut self) -> &mut Vec<Attachment>;
    /// A prompt that waited goes back into the prompt box, in front of what
    /// is being typed, with its attachments.
    fn put_back(&mut self, text: String, attachments: Vec<Attachment>);
    /// `text` replaces the prompt box's, to be edited (a rewind).
    fn edit(&mut self, text: String);
    /// The transcript is followed to its end.
    fn follow(&mut self);
    /// A short message in the status rule.
    fn flash(&mut self, message: String);
    /// A prompt is submitted, sent or queued: a picker the user asked for
    /// and moved on from stays shut.
    fn prompt_submitted(&mut self);
    /// The active harness listed its commands: a `/` being typed lists
    /// them.
    fn commands_arrived(&mut self);
    /// Another conversation is in place: nothing shown points into the one
    /// before (a subagent's transcript, a selection, a request's dialog).
    fn conversation_installed(&mut self);
}

/// The policy picker as the controller asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyAsk {
    /// The harness's display name, for the policies it does not offer.
    pub harness: &'static str,
    /// The policies it offers, least permissive first.
    pub offered: Vec<PermissionPolicy>,
    /// Where the cursor starts.
    pub start: Option<PermissionPolicy>,
    /// A default can be saved for the workspace.
    pub has_workspace: bool,
}

/// What a plan's approval offers to carry it out under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanAsk {
    pub policies: Vec<PermissionPolicy>,
    /// The one offered first.
    pub preferred: PermissionPolicy,
}
