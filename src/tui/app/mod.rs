//! TUI state: the view, and the controller it shows (`core::controller`).
//! Pure with respect to I/O: key handling changes the view and asks the
//! controller, which queues `Action`s; the event loop in `tui/mod.rs`
//! hands them to `controller::run_actions`, which carries them out
//! against the session.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::text::Line;

use super::history::PromptHistory;
use super::modal::{HarnessOption, Modal};
use super::selection::{Point, RowCopy, Selection};
use super::{herdr, markdown};
use crate::config::Config;
use crate::core::controller::{Controller, Launch, Opening};
use crate::core::registry::Registry;
use crate::core::sandbox::{Sandbox, SandboxSetup};
use crate::core::{
    AgentEvent, Attachment, HarnessCommand, HarnessId, PermissionPolicy, ProviderId, Rules,
};
use crate::harness::ModelInfo;

pub use crate::core::controller::Action;

// The files below, and their tests, reach these through this module
// (`super::transcript`, `super::shell`, ...), as when they were one file.
use crate::core::transcript;

mod accessors;
mod choices;
mod commands;
mod conversation;
mod front;
mod input;
mod modal_keys;
mod mouse;
mod permissions;
mod policy;
mod remote;
mod rewind;
mod sandbox;
mod session;
mod shell_command;
mod status;
mod subagents;
mod suggestions;
mod switch;
mod turn;

pub use front::Screen;
pub use status::{LABEL_WIDTH, StatusReport, Tone, padded};

/// A list the TUI wants from a harness. Asking may start the CLI and
/// take seconds (Claude on Bedrock gives up after 15), so it is done off
/// the UI thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ListRequest {
    Providers(HarnessId),
    Models(HarnessId, ProviderId),
    /// Its commands, where no session says before the first prompt.
    Commands(HarnessId),
    /// Its account's limits, where a run of its CLI apart says them.
    Status(HarnessId),
}

#[derive(Debug, Clone)]
pub enum ListResult {
    Providers(Result<Vec<(ProviderId, String)>, String>),
    Models(Result<Vec<ModelInfo>, String>),
    Commands(Result<Option<Vec<HarnessCommand>>, String>),
    Status(Result<Vec<AgentEvent>, String>),
}

/// Everything a thread needs to answer a `ListRequest`.
pub struct ListJob {
    pub request: ListRequest,
    registry: Arc<Registry>,
    binary: PathBuf,
    cwd: PathBuf,
    sandbox: Sandbox,
}

impl ListJob {
    pub fn run(self) -> (ListRequest, ListResult) {
        let result = match (&self.request, self.registry.get(self.harness())) {
            (ListRequest::Providers(_), Some(h)) => ListResult::Providers(
                h.list_providers(&self.binary, &self.sandbox)
                    .map_err(|e| format!("{e:#}")),
            ),
            (ListRequest::Models(_, p), Some(h)) => ListResult::Models(
                h.list_models(&self.binary, p, &self.sandbox)
                    .map_err(|e| format!("{e:#}")),
            ),
            (ListRequest::Commands(_), Some(h)) => ListResult::Commands(
                h.list_commands(&self.binary, &self.cwd, &self.sandbox)
                    .map_err(|e| format!("{e:#}")),
            ),
            (ListRequest::Status(_), Some(h)) => ListResult::Status(
                h.query_status(&self.binary, &self.cwd, &self.sandbox)
                    .map_err(|e| format!("{e:#}")),
            ),
            (ListRequest::Status(_), None) => {
                ListResult::Status(Err("not a registered harness".into()))
            }
            (ListRequest::Providers(_), None) => {
                ListResult::Providers(Err("not a registered harness".into()))
            }
            (ListRequest::Models(..), None) => {
                ListResult::Models(Err("not a registered harness".into()))
            }
            (ListRequest::Commands(_), None) => {
                ListResult::Commands(Err("not a registered harness".into()))
            }
        };
        (self.request, result)
    }

    fn harness(&self) -> HarnessId {
        match &self.request {
            ListRequest::Providers(h)
            | ListRequest::Models(h, _)
            | ListRequest::Commands(h)
            | ListRequest::Status(h) => *h,
        }
    }
}

/// A subagent as listed under the prompt.
#[derive(Debug, Clone)]
pub struct SubagentRow {
    /// The tool call that spawned it.
    pub id: String,
    pub description: String,
    pub kind: Option<String>,
    /// `None` while it runs.
    pub status: Option<crate::core::SubagentStatus>,
    /// How many subagents deep it was spawned.
    pub depth: usize,
    /// What it is doing now.
    pub activity: Option<String>,
    /// Tool calls it has made.
    pub tools: usize,
    /// How long it has run, or ran.
    pub secs: f32,
}

/// A subagent offered in the picker, running or not.
#[derive(Debug, Clone)]
pub struct SubagentOption {
    /// The tool call that spawned it.
    pub id: String,
    pub description: String,
    pub kind: Option<String>,
    /// `None` while it runs.
    pub status: Option<crate::core::SubagentStatus>,
    /// How many subagents deep it was spawned.
    pub depth: usize,
}

/// The TUI's state: what is on screen, and the controller it shows. The
/// controller works on the parts of the view its logic reads and changes
/// through `Screen` (`App::split`).
pub struct App {
    pub git_branch: Option<String>,
    pub harness_options: Vec<HarnessOption>,
    /// What decides how each harness runs, the conversation, and the
    /// active harness's session as it goes.
    pub controller: Controller,
    /// Lists being fetched, and the ones still to hand to a thread.
    lists_pending: HashSet<ListRequest>,
    /// Lists that failed since they were last asked for by the user.
    lists_failed: HashSet<ListRequest>,
    list_jobs: Vec<ListJob>,
    /// The picker the user asked for, opened when its list arrives.
    open_when_listed: Option<ListRequest>,

    pub input: String,
    /// Char index into `input`.
    pub cursor: usize,
    /// Columns the prompt text has to wrap in; set by the renderer.
    pub prompt_width: usize,
    /// First visible row of the prompt box once it is taller than its cap.
    pub prompt_scroll: usize,
    /// Prompts sent from this workspace, for Up/Down recall.
    pub history: PromptHistory,
    /// The transcript as last drawn, for turning a mouse position into
    /// text; set by the renderer.
    pub transcript_view: TranscriptView,
    /// Text selected in the transcript with the mouse.
    pub selection: Option<Selection>,
    /// The last press, to tell a double or triple click.
    last_click: Option<(Instant, Point, u8)>,
    /// While a drag is held past the transcript's top (-1) or bottom (1)
    /// edge, it keeps scrolling that way.
    drag_edge: i8,
    /// While the scrollbar's thumb is held: rows from its top to the pointer.
    scrollbar_grab: Option<u16>,
    /// Selected text waiting for the event loop to put it on the clipboard.
    copy_request: Option<String>,
    /// A short-lived message in the status rule, and when it appeared.
    flash: Option<(String, Instant)>,
    /// The user asked to edit the prompt in their editor; the event loop
    /// owns the terminal, so it does the work.
    edit_requested: bool,
    /// The user asked for the clipboard's image; reading it runs tools, so
    /// the event loop does it.
    clipboard_requested: bool,
    /// Index of the transcript line on the first row; a transcript can be
    /// longer than a `u16` counts.
    pub scroll: usize,
    pub auto_scroll: bool,

    pub spinner_frame: usize,
    pub show_plan: bool,
    /// The subagent the keyboard is on in the list under the prompt; `None`
    /// while it is in the prompt.
    pub subagent_focus: Option<String>,
    /// The subagent whose transcript is shown in place of the main one.
    pub viewing: Option<String>,
    /// Where the main transcript was scrolled to when a subagent's was opened.
    main_scroll: (usize, bool),
    /// Images that go out with the next prompt.
    pub attachments: Vec<Attachment>,

    pub modal: Option<Modal>,
    pub suggestions: Vec<(String, String)>,
    pub selected_suggestion: usize,
    /// The list was closed (Esc, a recalled prompt, a pick) and not
    /// opened again by typing: commands that arrive do not reopen it.
    suggestions_closed: bool,
    /// The list is of files, for the `@` at this char index of the input,
    /// instead of slash commands.
    pub completing_file: Option<usize>,
    /// The files `@` completes, relative to `cwd`, as last listed.
    file_index: Vec<String>,
    /// The files are to be listed again; walking the tree takes a while,
    /// so the event loop has it done off the UI.
    file_index_requested: bool,
    file_walk_pending: bool,
    /// The active harness's commands arrived while the controller worked:
    /// the suggestions are brought up to date once it is done (`App::with`).
    commands_pending: bool,
}

pub struct AppInit {
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub registry: Arc<Registry>,
    pub config: Config,
    pub harness: HarnessId,
    /// The policy named on the command line, if one was.
    pub policy: Option<PermissionPolicy>,
    pub sandbox: SandboxSetup,
    pub provider: Option<String>,
    /// The model and effort named on the command line, if they were; the
    /// configured ones are taken from `config`.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Conversation id or prefix; empty string = most recent.
    pub resume: Option<String>,
    /// The harness came from the CLI, so it overrides a resumed conversation's.
    pub harness_explicit: bool,
    /// Where file-checkpoint shadow repositories go; `None` = the user's
    /// state directory.
    pub checkpoint_store: Option<PathBuf>,
    /// Where the context windows harnesses reported are kept; `None` = the
    /// user's state directory.
    pub context_windows: Option<PathBuf>,
    /// The user's allow rules, and where "allow always" adds to them.
    pub rules: Rules,
    /// Each harness's guess at its own provider (`Harness::default_provider`),
    /// which reads the user's vendor configuration; tests pass their own.
    pub default_providers: HashMap<HarnessId, ProviderId>,
}

/// What the renderer last drew of the transcript.
#[derive(Debug, Default)]
pub struct TranscriptView {
    /// Where the text is, inside the border.
    pub area: Rect,
    /// Index of the line on the first row.
    pub scroll: usize,
    /// Every rendered line as plain text, scrolled out or not.
    pub lines: Vec<String>,
    /// Every rendered line, kept between frames.
    pub rendered: Vec<Line<'static>>,
    /// Per entry of `lines`, what a copy takes from it.
    pub copy: Vec<RowCopy>,
    /// The code blocks in the responses, where a right click copies the
    /// code alone.
    pub code: Vec<CodeRows>,
    /// Per transcript block, where its lines are in `rendered` and `lines`.
    pub blocks: Vec<KeptBlock>,
    /// The blocks still running, whose status is drawn again on every
    /// frame instead of being laid out again.
    pub live: Vec<LiveRow>,
    /// The width `rendered` was laid out for.
    pub width: u16,
    /// The scrollbar, when there is more transcript than fits.
    pub scrollbar: Option<Scrollbar>,
    /// The jump-to-bottom label, shown while scrolled away from the end.
    pub jump: Option<Rect>,
    /// How long the last frame took to bring `rendered` up to date.
    pub took: Duration,
}

/// One transcript block as it was laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeptBlock {
    /// A fingerprint of the block and of the space above it.
    pub key: u64,
    /// Its first line, below that space.
    pub top: usize,
    /// One past its last line.
    pub end: usize,
}

/// A code block in a response, by the rows it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRows {
    /// Its response's index in [`TranscriptView::blocks`].
    pub block: usize,
    /// Which of the response's code blocks it is
    /// ([`super::markdown::code_blocks`]).
    pub nth: usize,
    /// Its rows in `lines`, frame included.
    pub rows: std::ops::Range<usize>,
}

/// A running block's first line, as laid out before it was fitted to the
/// width, and where its status is in it.
#[derive(Debug, Clone)]
pub struct LiveRow {
    /// Its index in [`TranscriptView::blocks`].
    pub block: usize,
    pub line: Line<'static>,
    /// What a copy takes from `line`.
    pub copy: RowCopy,
    /// The status's span in `line`.
    pub span: usize,
    pub live: Live,
}

/// What a running row's status shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    pub since: Instant,
    /// The time it has run; not while it waits on the user, whose time
    /// that is.
    pub clock: bool,
    /// A `!` command's, which says "running".
    pub shell: bool,
}

/// The transcript's scrollbar: a thumb on the right border whose size and
/// place show how much of the transcript is in view, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrollbar {
    pub x: u16,
    pub y: u16,
    pub height: u16,
    /// Rows from the top of the bar to the top of the thumb.
    pub thumb_top: u16,
    pub thumb_len: u16,
    pub max_scroll: usize,
}

impl Scrollbar {
    /// The bar for `total` lines seen through `height` rows from line
    /// `scroll`; `None` when everything fits.
    pub fn new(x: u16, y: u16, height: u16, total: usize, scroll: usize) -> Option<Self> {
        if height == 0 || total <= height as usize {
            return None;
        }
        let max_scroll = total - height as usize;
        // At most `height`, since `total` is more.
        let thumb_len = ((height as u128 * height as u128) / total as u128).max(1) as u16;
        let travel = (height - thumb_len) as u128;
        let scroll = scroll.min(max_scroll) as u128;
        let max = max_scroll as u128;
        // Rounded, but only at the very ends when the view is at the ends.
        let mut thumb_top = ((scroll * travel + max / 2) / max) as u16;
        if travel > 1 {
            if scroll > 0 {
                thumb_top = thumb_top.max(1);
            }
            if scroll < max {
                thumb_top = thumb_top.min(travel as u16 - 1);
            }
        }
        Some(Scrollbar {
            x,
            y,
            height,
            thumb_top,
            thumb_len,
            max_scroll,
        })
    }

    /// The scroll offset that puts the thumb's top `top` rows down the bar.
    pub fn scroll_for(&self, top: i32) -> usize {
        let travel = (self.height - self.thumb_len) as i32;
        if travel == 0 {
            return 0;
        }
        let (top, travel) = (top.clamp(0, travel) as u128, travel as u128);
        ((top * self.max_scroll as u128 + travel / 2) / travel) as usize
    }

    fn contains(&self, column: u16, row: u16) -> bool {
        column == self.x && row >= self.y && row < self.y + self.height
    }
}

impl App {
    pub fn new(init: AppInit) -> Self {
        let registry = init.registry;
        let config = init.config;

        let mut harness_options = Vec::new();
        for h in registry.all() {
            let d = h.descriptor();
            let probe = h.probe(config.binary_override(d.id.as_str()));
            harness_options.push(HarnessOption {
                id: d.id,
                display_name: d.display_name,
                installed: probe.binary.is_some(),
                version: probe.version,
                interactive_permissions: h.capabilities().interactive_permissions,
                auth: probe.auth,
            });
        }

        let git_branch = init.workspace_root.as_deref().and_then(git_branch);
        let (controller, greeting) = Controller::open(Opening {
            registry,
            config,
            launch: Launch {
                harness: init.harness,
                policy: init.policy,
                provider: init.provider,
                model: init.model,
                effort: init.effort,
                default_providers: init.default_providers,
                cwd: init.cwd,
                workspace_root: init.workspace_root,
                sandbox: init.sandbox,
                rules: init.rules,
            },
            resume: init.resume,
            harness_explicit: init.harness_explicit,
            checkpoint_store: init.checkpoint_store,
            context_windows: init.context_windows,
        });
        let history = PromptHistory::load(controller.store.history_path());

        let mut app = App {
            git_branch,
            harness_options,
            controller,
            lists_pending: HashSet::new(),
            lists_failed: HashSet::new(),
            list_jobs: Vec::new(),
            open_when_listed: None,
            input: String::new(),
            cursor: 0,
            prompt_width: 78,
            prompt_scroll: 0,
            history,
            transcript_view: TranscriptView::default(),
            selection: None,
            last_click: None,
            drag_edge: 0,
            scrollbar_grab: None,
            copy_request: None,
            flash: None,
            edit_requested: false,
            clipboard_requested: false,
            scroll: 0,
            auto_scroll: true,
            spinner_frame: 0,
            show_plan: true,
            subagent_focus: None,
            viewing: None,
            main_scroll: (0, true),
            attachments: Vec::new(),
            modal: None,
            suggestions: Vec::new(),
            selected_suggestion: 0,
            suggestions_closed: false,
            completing_file: None,
            file_index: Vec::new(),
            // Listed once at the start, so that the first `@` has files.
            file_index_requested: true,
            file_walk_pending: false,
            commands_pending: false,
        };

        app.with(|c, screen| c.greet(greeting, screen));
        app
    }

    /// The controller takes the event; the view follows what it did.
    pub fn on_event(&mut self, ev: AgentEvent) {
        self.with(|c, screen| c.on_event(ev, screen));
    }
}

/// Current branch from `.git/HEAD` without spawning git.
fn git_branch(root: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(b) => Some(b.to_string()),
        None => Some(head.chars().take(8).collect()),
    }
}

#[cfg(test)]
pub(crate) mod tests;
