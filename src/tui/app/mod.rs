//! TUI state. Pure with respect to I/O: key handling mutates state and queues
//! `Action`s that the event loop in `tui/mod.rs` executes against the session.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::text::Line;

use super::history::PromptHistory;
use super::modal::{HarnessOption, Modal};
use super::selection::{Point, RowCopy, Selection};
// The files below, and their tests, reach these through this module
// (`super::transcript`, `super::shell`, ...), as when they were one file.
use super::transcript::{self, Transcript, tool_summary};
use super::{bridge, herdr, markdown, shell};
use crate::config::{BridgeSummary, Config};
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{
    CheckpointRecord, ContextWindows, Conversation, ConversationStore, TurnAnchorRecord,
    default_context_windows_path, load_context_windows,
};
use crate::core::guard::Watch;
use crate::core::registry::Registry;
use crate::core::sandbox::{Sandbox, SandboxLevel, SandboxSetup};
use crate::core::{
    AgentEvent, Attachment, CapsUpdate, ContextUsage, HarnessCommand, HarnessId, ModelRef,
    PermissionPolicy, PermissionRequest, PlanEntry, ProviderId, RateLimitInfo, Rules,
    SessionCommand, StopReason, Usage,
};
use crate::harness::ModelInfo;

mod accessors;
mod choices;
mod commands;
mod conversation;
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
mod subagents;
mod suggestions;
mod switch;
mod turn;

/// Side effects the event loop performs on the App's behalf.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Start a session for the active harness (if none is alive).
    StartSession {
        resume: Option<String>,
    },
    SendTurn {
        text: String,
        attachments: Vec<Attachment>,
    },
    Command(SessionCommand),
    /// Shut the active session down (harness switch, resume, quit).
    Shutdown,
    /// Run a command the user typed after `!`.
    RunShell {
        command: String,
    },
    /// Kill the running `!` command.
    StopShell,
}

/// A list the TUI wants from a harness. Asking may start the CLI and
/// take seconds (Claude on Bedrock gives up after 15), so it is done off
/// the UI thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ListRequest {
    Providers(HarnessId),
    Models(HarnessId, ProviderId),
    /// Its commands, where no session says before the first prompt.
    Commands(HarnessId),
}

#[derive(Debug, Clone)]
pub enum ListResult {
    Providers(Result<Vec<(ProviderId, String)>, String>),
    Models(Result<Vec<ModelInfo>, String>),
    Commands(Result<Option<Vec<HarnessCommand>>, String>),
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
            ListRequest::Providers(h) | ListRequest::Models(h, _) | ListRequest::Commands(h) => *h,
        }
    }
}

/// The `!` command at work; its output goes to the last running
/// `Block::Shell`.
#[derive(Debug, Clone)]
pub struct ShellRun {
    pub started: Instant,
    /// The user asked for it to be stopped.
    pub stopping: bool,
}

/// The harness the user is switching away from writing a summary for the
/// next one, or for a new session of its own (`renew`): the switch, or the
/// new session, happens when its turn ends.
#[derive(Debug, Clone, Copy)]
struct Handoff {
    to: HarnessId,
    /// Where its turn began in the transcript.
    start: usize,
    /// Prompts were held when it was asked for (after an interrupt): the
    /// user sends those, also after the switch.
    held: bool,
    /// The next session is a new one of the same harness, for another
    /// model or effort (`App::session_renewed`).
    renew: bool,
    /// The session asked for it could not be resumed and runs new
    /// (`AgentEvent::SessionReplaced`): what it writes summarizes nothing.
    blind: bool,
}

/// A subagent that is at work.
#[derive(Debug, Clone)]
pub struct RunningSubagent {
    /// The tool call that spawned it.
    pub id: String,
    pub description: String,
    pub kind: Option<String>,
    /// What it is doing now.
    pub activity: Option<String>,
    /// The harness words its activity itself, so its tool calls need not.
    described: bool,
    /// Tool calls it has made.
    pub tools: usize,
    pub started: Instant,
    /// Its end may never be told (`AgentEvent::SubagentStarted`): it holds
    /// nothing back (`App::awaited_subagents`).
    unreported_end: bool,
}

/// A prompt waiting for the running turn to finish.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedPrompt {
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// The harness active when it was typed: whether a `/` prompt is a
    /// command was decided for that one.
    pub harness: HarnessId,
}

impl Action {
    /// A text-only turn.
    pub fn turn(text: impl Into<String>) -> Self {
        Action::SendTurn {
            text: text.into(),
            attachments: Vec::new(),
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

/// The active harness's session on the vendor's remote control.
#[derive(Debug, Clone, Default)]
struct RemoteSession {
    /// What it is called there, if the user named it.
    name: Option<String>,
    /// Where it can be opened, once the harness's process said.
    url: Option<String>,
    /// How the connection stands, in the harness's words.
    state: Option<String>,
}

pub struct App {
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub registry: Arc<Registry>,
    pub config: Config,

    pub active: HarnessId,
    pub harness_options: Vec<HarnessOption>,
    /// The policy named for every harness (`--policy`, or the one a
    /// resumed conversation was named). Without one each harness has its
    /// configured default (`policy_requested`).
    pub policy_explicit: Option<PermissionPolicy>,
    /// What the user chose in the TUI, per harness. It holds for that
    /// harness only, over the policy named for every harness.
    pub policy_choice: HashMap<HarnessId, PermissionPolicy>,
    /// The policy named in this run (`--policy`): it holds over the one a
    /// resumed conversation was run with.
    run_policy: Option<PermissionPolicy>,
    /// What was chosen in this run: it holds over a resumed conversation's
    /// choice for the same harness.
    run_choices: HashMap<HarnessId, PermissionPolicy>,
    /// The policy each harness ran under before it went into plan mode:
    /// the one its plan approval offers first.
    pre_plan: HashMap<HarnessId, PermissionPolicy>,
    /// Policies sent to the live session whose report has not come back
    /// (`PolicyChanged`), oldest first.
    policies_sent: VecDeque<PermissionPolicy>,
    /// The sandbox level asked for (flag, config, or `/sandbox`) and the
    /// platform's backend. A change reaches the harness's next process.
    pub sandbox: SandboxSetup,
    /// The level the live session's process was started under.
    pub session_sandbox_level: Option<SandboxLevel>,
    /// The policy the live session's process was started under.
    pub session_policy: Option<PermissionPolicy>,
    /// The active harness's own configuration files as they were when its
    /// session started, to notice a change (`core::guard`).
    pub guard: Option<Watch>,
    /// The provider of each harness: the user's choice, else a guess at
    /// the harness's own default, corrected by what its session reports.
    pub providers: HashMap<HarnessId, ProviderId>,
    /// Harnesses whose provider the user chose; only those are told it.
    pub chosen_providers: HashSet<HarnessId>,
    /// The provider a session runs on when it is not the chosen one.
    running_providers: HashMap<HarnessId, ProviderId>,
    pub models: HashMap<HarnessId, ModelRef>,
    pub efforts: HashMap<HarnessId, String>,
    /// Harnesses whose model or effort was chosen, in this run or for the
    /// conversation, rather than configured; only those are kept with the
    /// conversation.
    chosen_models: HashSet<HarnessId>,
    chosen_efforts: HashSet<HarnessId>,
    /// The ones chosen in this run (`--model`, `/model`, ...): they hold
    /// over a resumed conversation's.
    run_models: HashSet<HarnessId>,
    run_efforts: HashSet<HarnessId>,
    model_cache: HashMap<(HarnessId, String), Vec<ModelInfo>>,
    provider_cache: HashMap<HarnessId, Vec<(ProviderId, String)>>,
    /// Lists being fetched, and the ones still to hand to a thread.
    lists_pending: HashSet<ListRequest>,
    /// Lists that failed since they were last asked for by the user.
    lists_failed: HashSet<ListRequest>,
    list_jobs: Vec<ListJob>,
    /// The picker the user asked for, opened when its list arrives.
    open_when_listed: Option<ListRequest>,

    pub transcript: Transcript,
    /// The bridge's budget, when the user set one.
    pub bridge_max_chars: Option<usize>,
    bridge_summary: BridgeSummary,
    /// What the bridge to a harness is measured against otherwise.
    context_windows: ContextWindows,
    context_windows_path: PathBuf,
    handoff: Option<Handoff>,
    /// Transcript length when each harness was last active (bridge start).
    last_active_index: HashMap<HarnessId, usize>,
    /// Session ids seen this run (or chosen via /resume), per harness.
    pub session_ids: HashMap<HarnessId, String>,
    pub session_alive: bool,
    /// The vendor's remote control the user put the active harness's
    /// session on (`/remote-control`). It outlives the harness's process:
    /// each new one is put on it again (`remote_control_resend`); a
    /// switch, a fork, a resume, `/clear` and quit end it.
    remote: Option<RemoteSession>,
    /// The live or starting session was started before anything was sent
    /// to it (so that its commands are listed): until the first prompt it
    /// is not part of the conversation.
    session_blank: bool,
    /// The id (and model) such a session reported, committed with the
    /// first prompt.
    blank_session: Option<(String, Option<String>)>,
    /// A choice sent to the session that it takes only between turns,
    /// until it has said how it took it: prompts typed meanwhile wait.
    choice_pending: Option<choices::PendingChoice>,
    /// The last session ended by itself or could not start: the next
    /// one waits for the user (a prompt, a switch, a resume).
    idle_start_paused: bool,
    /// The session's start waits for the CLI that ended the one before it
    /// to be gone (`tui::Ending`): what would start another is refused
    /// meanwhile, as during a turn.
    pub start_held: bool,
    pub store: ConversationStore,
    pub conversation: Conversation,
    persist_failed: bool,
    first_prompt: Option<String>,

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
    pub scroll: u16,
    pub auto_scroll: bool,

    pub is_generating: bool,
    pub generation_start: Option<Instant>,
    pub generation_duration: Option<Duration>,
    /// When the running turn was asked to stop, and whether the user has
    /// been told it has not yet.
    interrupted: Option<(Instant, bool)>,
    pub spinner_frame: usize,
    pub turn_usage: Usage,
    pub session_usage: Usage,
    /// Context-window fill of the active harness's session.
    pub context: ContextUsage,
    pub rate_limit: Option<RateLimitInfo>,
    pub plan: Vec<PlanEntry>,
    pub show_plan: bool,
    /// Subagents at work, oldest first. They can outlive the turn that
    /// started them.
    pub subagents: Vec<RunningSubagent>,
    /// The subagent the keyboard is on in the list under the prompt; `None`
    /// while it is in the prompt.
    pub subagent_focus: Option<String>,
    /// The subagent whose transcript is shown in place of the main one.
    pub viewing: Option<String>,
    /// Where the main transcript was scrolled to when a subagent's was opened.
    main_scroll: (u16, bool),
    /// Images that go out with the next prompt.
    pub attachments: Vec<Attachment>,
    /// Where each harness can rewind its session to (see `TurnAnchor`).
    anchors: Vec<TurnAnchorRecord>,
    /// Harnesses whose stored session id must be branched, not reattached
    /// (set by `/fork` until the branch exists).
    fork_pending: HashSet<HarnessId>,
    /// File checkpoints of the workspace; `None` outside a git repository
    /// or when switched off.
    checkpoints: Option<Checkpoints>,
    file_checkpoints: Vec<CheckpointRecord>,
    checkpoint_failed: bool,
    /// The tree as it was before the last file restore, for `/undo-restore`.
    restore_undo: Option<String>,
    restore_seq: usize,
    /// User blocks whose anchor has not arrived yet, oldest first. A harness
    /// that can rewind reports one anchor per turn, in order, but not
    /// always before the next turn starts (pi lists it after the turn).
    anchor_pending: VecDeque<usize>,
    /// Prompts entered during a turn; one is sent each time a turn completes.
    pub queued: VecDeque<QueuedPrompt>,
    /// The `!` command running now.
    pub shell: Option<ShellRun>,
    compacting: bool,
    /// What live sessions reported on top of the declared capabilities.
    live_caps: HashMap<HarnessId, CapsUpdate>,
    /// The harnesses already told which MCP servers they do not get.
    mcp_warned: HashSet<HarnessId>,
    /// The harnesses whose [`Harness::session_notice`] was shown.
    ///
    /// [`Harness::session_notice`]: crate::harness::Harness::session_notice
    session_noticed: HashSet<HarnessId>,

    pub modal: Option<Modal>,
    pending_prompts: VecDeque<PermissionRequest>,
    /// Requests in `pending_prompts` the agent made while it planned: they
    /// are judged so also when they come out after the plan's approval.
    asked_planning: HashSet<String>,
    /// Requests these allow are answered without asking.
    rules: Rules,
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

    pub should_quit: bool,
    actions: VecDeque<Action>,
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
    pub max_scroll: u16,
}

impl Scrollbar {
    /// The bar for `total` lines seen through `height` rows from line
    /// `scroll`; `None` when everything fits.
    pub fn new(x: u16, y: u16, height: u16, total: u16, scroll: u16) -> Option<Self> {
        if height == 0 || total <= height {
            return None;
        }
        let max_scroll = total - height;
        let thumb_len = ((height as u32 * height as u32) / total as u32).max(1) as u16;
        let travel = (height - thumb_len) as u32;
        let scroll = scroll.min(max_scroll) as u32;
        // Rounded, but only at the very ends when the view is at the ends.
        let mut thumb_top = ((scroll * travel + max_scroll as u32 / 2) / max_scroll as u32) as u16;
        if travel > 1 {
            if scroll > 0 {
                thumb_top = thumb_top.max(1);
            }
            if scroll < max_scroll as u32 {
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
    pub fn scroll_for(&self, top: i32) -> u16 {
        let travel = (self.height - self.thumb_len) as i32;
        if travel == 0 {
            return 0;
        }
        let top = top.clamp(0, travel);
        ((top * self.max_scroll as i32 + travel / 2) / travel) as u16
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
            });
        }

        let mut providers = HashMap::new();
        let mut chosen_providers = HashSet::new();
        let mut models = HashMap::new();
        let mut efforts = HashMap::new();
        for h in registry.all() {
            let id = h.descriptor().id;
            let settings = config.harness(id.as_str());
            let chosen = settings.and_then(|s| s.default_provider.clone());
            if chosen.is_some() {
                chosen_providers.insert(id);
            }
            let provider = chosen.or_else(|| init.default_providers.get(&id).map(|p| p.0.clone()));
            if let Some(p) = provider.clone() {
                providers.insert(id, ProviderId::new(p));
            }
            if let (Some(p), Some(m)) = (
                provider.clone(),
                settings.and_then(|s| s.default_model.clone()),
            ) {
                models.insert(id, ModelRef::new(id, p.as_str(), m));
            }
            if let Some(e) = settings.and_then(|s| s.default_effort.clone()) {
                efforts.insert(id, e);
            }
        }
        if let Some(p) = init.provider {
            providers.insert(init.harness, ProviderId::new(p));
            chosen_providers.insert(init.harness);
        }
        let mut chosen_models = HashSet::new();
        let mut chosen_efforts = HashSet::new();
        // `init.model` is only a flag: the configured model of a harness
        // whose provider is not known yet is still given to it.
        let configured_model = config
            .harness(init.harness.as_str())
            .and_then(|s| s.default_model.clone());
        if let Some(m) = init
            .model
            .clone()
            .or_else(|| configured_model.filter(|_| !models.contains_key(&init.harness)))
        {
            if init.model.is_some() {
                chosen_models.insert(init.harness);
            }
            let p = providers
                .get(&init.harness)
                .cloned()
                .unwrap_or_else(|| ProviderId::new("default"));
            models.insert(init.harness, ModelRef::new(init.harness, p, m));
        }
        if let Some(e) = init.effort {
            chosen_efforts.insert(init.harness);
            efforts.insert(init.harness, e);
        }

        let store = ConversationStore::open(init.workspace_root.as_deref(), &init.cwd);
        let history = PromptHistory::load(store.history_path());
        let mut resume_error: Option<String> = None;
        let mut loaded: Option<Conversation> = None;
        if let Some(r) = init.resume.as_deref() {
            let target = if r.is_empty() {
                store.last().map(|c| c.id)
            } else {
                Some(r.to_string())
            };
            match target {
                None => {
                    resume_error = Some("no saved conversation to resume in this workspace".into())
                }
                Some(id) => match store.load(&id) {
                    Ok(c) => loaded = Some(c),
                    Err(e) => resume_error = Some(format!("could not resume: {e}")),
                },
            }
        }
        let resumed = loaded.is_some();
        let conversation = loaded.unwrap_or_else(|| Conversation::new(init.harness));
        let active = if resumed
            && !init.harness_explicit
            && registry.get(conversation.active_harness).is_some()
        {
            conversation.active_harness
        } else {
            init.harness
        };
        let session_ids = conversation.sessions.clone();
        let last_active_index = conversation.bookmarks.clone();
        let transcript = Transcript::from_records(&conversation.blocks);
        let session_usage = conversation.usage.get(&active).cloned().unwrap_or_default();
        let first_prompt = (!conversation.title.is_empty()).then(|| conversation.title.clone());
        let plan = conversation.plan.clone();
        let anchors = conversation.anchors.clone();
        let file_checkpoints = conversation.checkpoints.clone();
        let fork_pending: HashSet<HarnessId> = conversation.fork_pending.iter().copied().collect();
        let checkpoints = if config.file_checkpoints.unwrap_or(true) {
            let store = init
                .checkpoint_store
                .unwrap_or_else(Checkpoints::default_store);
            init.workspace_root
                .as_deref()
                .and_then(|root| Checkpoints::open_in(root, &store))
        } else {
            None
        };

        let git_branch = init.workspace_root.as_deref().and_then(git_branch);
        let context_windows_path = init
            .context_windows
            .unwrap_or_else(default_context_windows_path);
        let mut app = App {
            cwd: init.cwd,
            workspace_root: init.workspace_root,
            git_branch,
            registry,
            bridge_max_chars: config.bridge_max_chars,
            context_windows: load_context_windows(&context_windows_path),
            context_windows_path,
            bridge_summary: config.bridge_summary.unwrap_or_default(),
            handoff: None,
            config,
            active,
            harness_options,
            policy_explicit: init.policy,
            policy_choice: HashMap::new(),
            run_policy: init.policy,
            run_choices: HashMap::new(),
            pre_plan: HashMap::new(),
            policies_sent: VecDeque::new(),
            sandbox: init.sandbox,
            session_sandbox_level: None,
            session_policy: None,
            guard: None,
            providers,
            chosen_providers,
            running_providers: HashMap::new(),
            models,
            efforts,
            run_models: chosen_models.clone(),
            run_efforts: chosen_efforts.clone(),
            chosen_models,
            chosen_efforts,
            model_cache: HashMap::new(),
            provider_cache: HashMap::new(),
            lists_pending: HashSet::new(),
            lists_failed: HashSet::new(),
            list_jobs: Vec::new(),
            open_when_listed: None,
            transcript,
            last_active_index,
            session_ids,
            session_alive: false,
            remote: None,
            session_blank: false,
            blank_session: None,
            choice_pending: None,
            idle_start_paused: false,
            start_held: false,
            store,
            conversation,
            persist_failed: false,
            first_prompt,
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
            is_generating: false,
            generation_start: None,
            generation_duration: None,
            interrupted: None,
            spinner_frame: 0,
            turn_usage: Usage::default(),
            session_usage,
            context: ContextUsage::default(),
            rate_limit: None,
            plan,
            show_plan: true,
            subagents: Vec::new(),
            subagent_focus: None,
            viewing: None,
            main_scroll: (0, true),
            attachments: Vec::new(),
            anchors,
            fork_pending,
            checkpoints,
            file_checkpoints,
            checkpoint_failed: false,
            restore_undo: None,
            restore_seq: 0,
            anchor_pending: VecDeque::new(),
            queued: VecDeque::new(),
            shell: None,
            compacting: false,
            live_caps: HashMap::new(),
            mcp_warned: HashSet::new(),
            session_noticed: HashSet::new(),
            modal: None,
            pending_prompts: VecDeque::new(),
            asked_planning: HashSet::new(),
            rules: init.rules,
            suggestions: Vec::new(),
            selected_suggestion: 0,
            suggestions_closed: false,
            completing_file: None,
            file_index: Vec::new(),
            // Listed once at the start, so that the first `@` has files.
            file_index_requested: true,
            file_walk_pending: false,
            should_quit: false,
            actions: VecDeque::new(),
        };

        if resumed {
            app.take_conversation_choices();
        }
        app.transcript.push_system(format!(
            "Welcome to unharness. Harness: {}  Policy: {}  Sandbox: {}. Ctrl+H harness, Ctrl+M model, Ctrl+E effort, Ctrl+P policy, /sandbox, /help.",
            app.display_name(),
            app.policy_label(),
            app.sandbox_level().0
        ));
        if let Some(w) = app.policy_warning() {
            app.transcript.push_notice(w);
        }
        app.require_policy();
        if let Some(w) = app.sandbox_level().1 {
            app.transcript.push_notice(w);
        }
        for problem in app.config.mcp_servers().1 {
            app.transcript.push_notice(problem);
        }
        if let Some(e) = resume_error {
            app.transcript.push_error(e);
        }
        if resumed {
            app.announce_resume(true);
        }
        app
    }

    pub fn on_event(&mut self, ev: AgentEvent) {
        let sender = self.short_name().to_string();
        match ev {
            AgentEvent::SessionStarted { session_id, model } => {
                // A session nobody has prompted is not saved, nor is the
                // conversation for it: not a new one, not a fork made at its
                // start, not one that replaced a session it could not resume.
                if self.session_blank {
                    self.blank_session = Some((session_id, model));
                } else {
                    self.commit_session(session_id, model);
                }
            }
            AgentEvent::SessionReplaced { reason } => self.session_replaced(&reason),
            AgentEvent::SessionRenewed { reason } => self.session_renewed(&reason),
            AgentEvent::Restarted => self.process_restarted(),
            AgentEvent::TurnStarted => {
                if !self.is_generating {
                    self.start_generation();
                }
            }
            AgentEvent::TextDelta(t) => self.transcript.append_assistant(&sender, &t),
            AgentEvent::ThinkingDelta(t) => self.transcript.append_thought(&t),
            AgentEvent::ToolCallStarted { id, name, input } => {
                self.transcript.tool_started(&id, &name, input)
            }
            // Live output. Streaming args arrive before the block exists and
            // are dropped there; ToolCallStarted carries the final input.
            AgentEvent::ToolCallDelta { id, delta, .. } => self.transcript.tool_delta(&id, &delta),
            AgentEvent::ToolCallResult {
                id,
                output,
                is_error,
            } => self.transcript.tool_result(&id, &output, is_error),
            AgentEvent::PermissionRequest(req) => self.on_permission_request(req),
            AgentEvent::PermissionWithdrawn { id } => self.withdraw_prompt(&id),
            AgentEvent::Usage(u) => {
                if u.cumulative {
                    self.session_usage = u;
                } else {
                    self.session_usage.add(&u);
                    self.turn_usage = u;
                }
            }
            AgentEvent::PlanUpdated { entries, .. } => self.plan = entries,
            // What a subagent does goes into its own transcript, not this
            // one: several at work would otherwise drown the main agent.
            AgentEvent::Sub { parent, event } => match *event {
                // These find their own place (ids are unique), or are the user's to answer.
                ev @ (AgentEvent::ToolCallDelta { .. }
                | AgentEvent::ToolCallResult { .. }
                | AgentEvent::PermissionRequest(_)
                | AgentEvent::Sub { .. }
                | AgentEvent::SubagentStarted { .. }
                | AgentEvent::SubagentProgress { .. }
                | AgentEvent::SubagentEnded { .. }) => self.on_event(ev),
                ev => {
                    if let AgentEvent::ToolCallStarted { name, input, .. } = &ev
                        && let Some(a) = self.subagents.iter_mut().find(|a| a.id == parent)
                    {
                        a.tools += 1;
                        if !a.described {
                            a.activity = Some(
                                format!("{name} {}", tool_summary(name, input))
                                    .trim_end()
                                    .to_string(),
                            );
                        }
                    }
                    let sender = self
                        .transcript
                        .agent(&parent)
                        .map_or("subagent", |run| run.sender())
                        .to_string();
                    // Nothing known of the call that spawned it: nowhere to put it.
                    let Some(log) = self.transcript.agent_log(&parent) else {
                        return;
                    };
                    match ev {
                        AgentEvent::ToolCallStarted { id, name, input } => {
                            log.tool_started(&id, &name, input)
                        }
                        AgentEvent::TextDelta(t) => log.append_assistant(&sender, &t),
                        AgentEvent::ThinkingDelta(t) => log.append_thought(&t),
                        AgentEvent::Notice(n) => log.push_notice(n),
                        AgentEvent::Error(e) => log.push_error(e),
                        AgentEvent::HookStarted { id, name } => log.hook_started(&id, &name),
                        AgentEvent::HookEnded {
                            id,
                            name,
                            outcome,
                            output,
                        } => log.hook_ended(&id, &name, outcome, &output),
                        _ => {}
                    }
                }
            },
            AgentEvent::SubagentStarted {
                id,
                description,
                kind,
                unreported_end,
            } => {
                self.transcript
                    .agent_started(&id, &description, kind.as_deref());
                self.subagents.retain(|a| a.id != id);
                self.subagents.push(RunningSubagent {
                    id,
                    description,
                    kind,
                    activity: None,
                    described: false,
                    tools: 0,
                    started: Instant::now(),
                    unreported_end,
                });
            }
            AgentEvent::SubagentProgress { id, activity } => {
                if let Some(a) = self.subagents.iter_mut().find(|a| a.id == id) {
                    a.activity = Some(activity);
                    a.described = true;
                }
            }
            AgentEvent::SubagentEnded { id, status, result } => {
                self.transcript.agent_ended(&id, status, result.as_deref());
                self.subagents.retain(|a| a.id != id);
                // Between turns nothing else would save its report.
                if !self.is_generating {
                    self.persist();
                }
            }
            AgentEvent::Context(c) => {
                if let Some(window) = c.window {
                    self.learn_context_window(window);
                }
                self.context.merge(c);
            }
            AgentEvent::RateLimit(r) => {
                // Warn once each time a window crosses the threshold.
                let was_high = self.rate_limit.as_ref().is_some_and(rate_limit_high);
                if !was_high && rate_limit_high(&r) {
                    self.transcript
                        .push_notice(format!("rate limit: {}", rate_limit_summary(&r)));
                }
                self.rate_limit = Some(r);
            }
            AgentEvent::RewindFailed { reason } => {
                let name = self.short_name();
                if self.is_generating {
                    // The edited prompt is already running in the old session.
                    self.transcript.push_error(format!(
                        "{name} could not rewind its session ({reason}): it still remembers the turns you removed"
                    ));
                } else {
                    self.transcript.push_notice(format!(
                        "{name} could not rewind its session ({reason}); it will start a fresh one with the conversation so far"
                    ));
                    self.forget_session(self.active);
                    self.persist();
                }
            }
            // An empty id means the turn never reached the harness.
            AgentEvent::TurnAnchor { id } => {
                if let Some(block) = self.anchor_pending.pop_front()
                    && !id.is_empty()
                {
                    let harness = self.active;
                    self.anchors
                        .retain(|a| !(a.block == block && a.harness == harness));
                    self.anchors.push(TurnAnchorRecord { block, harness, id });
                }
            }
            AgentEvent::PolicyChanged(p) => self.on_policy_changed(p),
            AgentEvent::CapabilitiesChanged(update) => {
                if let Some(levels) = &update.effort_levels
                    && self.caps().effort_levels != *levels
                {
                    self.transcript
                        .push_notice(format!("effort levels now: {}", levels.join(", ")));
                }
                // A guess at the harness's own default gives way to what
                // it reports. A choice stays the user's, and is what the
                // next session is told; the driver reported the mismatch
                // and the status line keeps showing it.
                if let Some(reported) = &update.provider {
                    if self.chosen_providers.contains(&self.active) {
                        if self.current_provider() == Some(reported) {
                            self.running_providers.remove(&self.active);
                        } else {
                            self.running_providers.insert(self.active, reported.clone());
                        }
                    } else if self.current_provider() != Some(reported) {
                        self.transcript.push_notice(format!(
                            "{} runs on {reported}, chosen by its own configuration",
                            self.short_name()
                        ));
                        self.providers.insert(self.active, reported.clone());
                        if let Some(m) = self.models.get_mut(&self.active) {
                            m.provider = reported.clone();
                        }
                    }
                }
                if let Some(models) = &update.models {
                    let provider = models
                        .first()
                        .map(|m| m.model_ref.provider.0.clone())
                        .or_else(|| self.current_provider().map(|p| p.0.clone()))
                        .unwrap_or_else(|| "default".into());
                    self.model_cache
                        .insert((self.active, provider), models.clone());
                }
                let commands = update.commands.is_some() || update.slash_commands.is_some();
                let refusal = update.refused_policies.is_some();
                let before = self.effective_policy();
                self.live_caps.entry(self.active).or_default().merge(update);
                if commands {
                    self.commands_arrived();
                }
                if refusal {
                    self.policy_refused(before);
                }
            }
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::Error(e),
            } if self.session_blank && !self.is_generating => {
                // A session started early failed before anything was sent
                // to it (Codex's thread, an ACP agent's session/new): a
                // prompt would wait on it for good.
                self.transcript.push_error(e);
                self.shutdown_session();
                self.idle_start_paused = true;
            }
            AgentEvent::TurnCompleted { stop_reason } => {
                let done = stop_reason == StopReason::Done;
                // A hook or a call of a turn cut short is not reported on.
                if !done {
                    self.transcript.end_turn_hooks();
                    self.transcript.end_turn_calls();
                }
                match stop_reason {
                    StopReason::Done => {}
                    StopReason::Interrupted => self.transcript.push_system("Turn interrupted."),
                    StopReason::Error(e) => self.transcript.push_error(e),
                }
                self.check_guard();
                self.finish_generation();
                let held = self.handoff.is_some_and(|h| h.held);
                self.finish_handoff(done);
                self.persist();
                // A clean finish moves on to the next queued prompt; after an
                // interrupt or error the user decides (Enter sends it), also
                // when a handoff summary came in between.
                if done && !held {
                    self.send_next_queued();
                } else if !self.queued.is_empty() {
                    self.transcript.push_notice(format!(
                        "{} queued prompt(s) held; press Enter to send the next",
                        self.queued.len()
                    ));
                }
            }
            AgentEvent::HookStarted { id, name } => self.transcript.hook_started(&id, &name),
            AgentEvent::HookEnded {
                id,
                name,
                outcome,
                output,
            } => self.transcript.hook_ended(&id, &name, outcome, &output),
            AgentEvent::RemoteControl(r) => self.on_remote_control(r),
            AgentEvent::RemotePrompt { text } => self.remote_prompt(text),
            AgentEvent::Notice(n) => self.transcript.push_notice(n),
            AgentEvent::Error(e) => self.transcript.push_error(e),
            AgentEvent::ProcessExited { code } => {
                self.session_alive = false;
                // A choice it was sent will not be answered.
                self.choice_answered(choices::provider_line);
                self.forget_refusals();
                if self.session_blank {
                    self.transcript.push_notice(format!(
                        "{} exited{} before any prompt; your next prompt starts it again",
                        self.short_name(),
                        code.map(|c| format!(" with code {c}")).unwrap_or_default()
                    ));
                }
                self.session_blank = false;
                self.blank_session = None;
                // It is not started again until the user does something.
                self.idle_start_paused = true;
                if self.remote.is_some() {
                    self.transcript.push_notice(format!(
                        "Remote Control is paused until {} runs again (your next prompt)",
                        self.short_name()
                    ));
                }
                self.drop_subagents();
                self.check_guard();
                self.process_gone();
                self.transcript.end_running_tools();
                if self.is_generating {
                    self.finish_generation();
                    self.transcript.push_error(format!(
                        "{} exited{} before the turn completed",
                        self.short_name(),
                        code.map(|c| format!(" with code {c}")).unwrap_or_default()
                    ));
                }
                self.finish_handoff(false);
            }
        }
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

/// Percentage of a rate-limit window at which the user is warned.
const RATE_LIMIT_WARN_PERCENT: f32 = 80.0;

fn rate_limit_high(r: &RateLimitInfo) -> bool {
    r.windows
        .iter()
        .any(|w| w.used_percent.is_some_and(|p| p >= RATE_LIMIT_WARN_PERCENT))
}

fn rate_limit_summary(r: &RateLimitInfo) -> String {
    let windows: Vec<String> = r
        .windows
        .iter()
        .map(|w| match w.used_percent {
            Some(p) => format!("{} {p:.0}% used", w.label),
            None => w.label.clone(),
        })
        .collect();
    match (&r.status, windows.is_empty()) {
        (Some(s), true) => s.clone(),
        (Some(s), false) if s != "allowed" => format!("{s} ({})", windows.join(", ")),
        _ => windows.join(", "),
    }
}

#[cfg(test)]
pub(crate) mod tests;
