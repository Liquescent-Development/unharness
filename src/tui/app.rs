//! TUI state. Pure with respect to I/O: key handling mutates state and queues
//! `Action`s that the event loop in `mod.rs` executes against the session.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;
use serde_json::Value;

use super::clipboard::{self, Pasted};
use super::drop;
use super::files;
use super::history::PromptHistory;
use super::modal::{
    AlwaysDraft, HarnessOption, ListPicker, Modal, PolicyPicker, ProviderOption, QuestionStep,
    RewindOption,
};
use super::prompt;
use super::selection::{self, Granularity, Point, RowCopy, Selection};
use super::transcript::{DEFAULT_BRIDGE_MAX_CHARS, Transcript, tool_summary};
use crate::config::{BridgeSummary, Config};
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{
    CONTEXT_WINDOW_MAX, CONTEXT_WINDOW_MIN, CheckpointRecord, ContextWindows, Conversation,
    ConversationStore, ShellStatus, TurnAnchorRecord, default_context_windows_path,
    load_context_windows, now_rfc3339, save_context_windows, truncate_title,
};
use crate::core::guard::{self, Watch};
use crate::core::mcp::{self, McpServer};
use crate::core::registry::Registry;
use crate::core::rules::{Scope, rule_may_answer};
use crate::core::sandbox::{Sandbox, SandboxLevel, SandboxSetup};
use crate::core::{
    AgentEvent, Attachment, Capabilities, CapsUpdate, ContextUsage, HarnessCommand, HarnessId,
    ModelRef, PermissionDecision, PermissionKind, PermissionPolicy, PermissionRequest, PlanEntry,
    PolicyResolution, PolicyUnavailable, ProviderId, RateLimitInfo, RemoteControl, Rule, Rules,
    SessionCommand, StopReason, Usage, resolve_policy,
};
use crate::harness::{Harness, ModelInfo, resolve_binary};

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
/// next one: the switch happens when its turn ends.
#[derive(Debug, Clone, Copy)]
struct Handoff {
    to: HarnessId,
    /// Where its turn began in the transcript.
    start: usize,
    /// Prompts were held when it was asked for (after an interrupt): the
    /// user sends those, also after the switch.
    held: bool,
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

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const BASE_COMMANDS: &[(&str, &str)] = &[
    ("/harness", "Open the harness picker"),
    ("/switch", "Alias for /harness"),
    (
        "/provider",
        "Open the provider picker (multi-provider harnesses)",
    ),
    ("/model", "Open the model picker"),
    ("/effort", "Open the reasoning effort picker"),
    ("/think", "Alias for /effort"),
    ("/policy", "Open the permission policy picker"),
    ("/sandbox", "Open the sandbox level picker"),
    (
        "/resume",
        "Resume a saved conversation (all harnesses in it)",
    ),
    (
        "/conversations",
        "List saved conversations in this workspace",
    ),
    ("/usage", "Show token usage and cost"),
    ("/plan", "Show or hide the agent's plan"),
    (
        "/subagents",
        "Open a subagent's own transcript (and stop it from there)",
    ),
    (
        "/steer",
        "Send a message into the running turn: /steer <text>",
    ),
    ("/rewind", "Go back to before an earlier prompt and edit it"),
    (
        "/undo-restore",
        "Reverse the last file restore made by /rewind",
    ),
    (
        "/fork",
        "Continue in a copy of this conversation, leaving the original as it is",
    ),
    (
        "/compact",
        "Summarise the context now: /compact [instructions]",
    ),
    (
        "/attach",
        "Attach an image, PDF or text file to the next prompt: /attach <path>",
    ),
    (
        "/paste",
        "Attach the image on the clipboard to the next prompt (Ctrl+V)",
    ),
    ("/detach", "Drop the pending attachments"),
    ("/skills", "List skills discovered in .agents/skills"),
    ("/allow", "List what is allowed without asking"),
    (
        "/remote-control",
        "Answer this session from claude.ai/code or the Claude app: /remote-control [on [name]|off]",
    ),
    (
        "/clear",
        "Start a new conversation (the previous one stays in /resume)",
    ),
    ("/help", "Show commands and shortcuts"),
    ("/quit", "Exit unharness"),
];

/// Names `handle_slash_command` answers to that the list leaves out.
const HIDDEN_ALIASES: &[&str] = &["/sessions", "/exit", "/rc"];

/// Whether `/name` is one of unharness's own commands on some harness
/// (`App::owns_command` says whether on the active one).
fn is_own_command(name: &str) -> bool {
    BASE_COMMANDS.iter().any(|(c, _)| *c == name) || HIDDEN_ALIASES.contains(&name)
}

/// A harness's command as the `/` list shows it: what it takes, then the
/// first line of what it does.
fn describe_command(harness: &str, c: &HarnessCommand) -> String {
    let what = c.description.lines().next().unwrap_or_default();
    match &c.hint {
        Some(hint) => format!("{harness}: {hint} · {what}"),
        None => format!("{harness}: {what}"),
    }
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

/// Lines scrolled per wheel notch, in the transcript and in a dialog's plan
/// or preview.
const WHEEL_LINES: u16 = 3;

/// Lines a dialog's plan or preview scrolls on PageUp/PageDown.
const PREVIEW_PAGE: i32 = 5;

/// Presses this close together on one cell count as a double or triple click.
const MULTI_CLICK: Duration = Duration::from_millis(500);

/// How long a turn has to end after an interrupt before another one ends
/// the CLI's process instead. Every harness stopped well within it in the
/// runs checked; pi waiting in a dialog never did (#89).
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long a status-rule message stays up.
const FLASH: Duration = Duration::from_secs(2);

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

    // ---------------------------------------------------------------- persistence

    /// Printed after the TUI closes, when a conversation was saved.
    pub fn exit_summary(&self) -> Option<String> {
        if !self.conversation.has_content() {
            return None;
        }
        let id = &self.conversation.id;
        let short = &id[..8.min(id.len())];
        let mut harnesses: Vec<&str> = self.session_ids.keys().map(|h| h.as_str()).collect();
        harnesses.sort_unstable();
        Some(format!(
            "Conversation {short} saved ({}). Resume it with:\n  unharness --resume {short}",
            if harnesses.is_empty() {
                "no sessions yet".to_string()
            } else {
                harnesses.join(", ")
            }
        ))
    }

    fn sync_conversation(&mut self) {
        let c = &mut self.conversation;
        c.active_harness = self.active;
        c.sessions = self.session_ids.clone();
        c.bookmarks = self.last_active_index.clone();
        c.blocks = self.transcript.to_records();
        c.usage.insert(self.active, self.session_usage.clone());
        c.plan = self.plan.clone();
        c.anchors = self.anchors.clone();
        c.checkpoints = self.file_checkpoints.clone();
        c.fork_pending = self.fork_pending.iter().copied().collect();
        c.fork_pending.sort_by_key(|h| h.as_str());
        c.policy = self.policy_explicit;
        c.policy_choices = self.policy_choice.clone();
        for h in &self.chosen_models {
            if let Some(m) = self.models.get(h) {
                c.models.insert(*h, m.clone());
            }
        }
        for h in &self.chosen_efforts {
            if let Some(e) = self.efforts.get(h) {
                c.efforts.insert(*h, e.clone());
            }
        }
        if self.session_seen()
            && let Some(p) = self.effective_policy()
        {
            self.conversation.last_policies.insert(self.active, p);
        }
        let c = &mut self.conversation;
        if c.title.is_empty()
            && let Some(p) = &self.first_prompt
        {
            c.title = truncate_title(p);
        }
        c.updated_at = now_rfc3339();
    }

    /// Write the conversation to disk. Failures are reported once per run.
    pub fn persist(&mut self) {
        if let Err(e) = self.save_conversation()
            && !self.persist_failed
        {
            self.persist_failed = true;
            self.transcript
                .push_error(format!("could not save the conversation: {e:#}"));
        }
    }

    /// Write the conversation to disk, if it has anything to keep.
    fn save_conversation(&mut self) -> anyhow::Result<()> {
        self.sync_conversation();
        if !self.conversation.has_content() {
            return Ok(());
        }
        self.store.save(&self.conversation)
    }

    // ----------------------------------------------------------------- accessors

    pub fn harness(&self) -> &dyn Harness {
        self.registry
            .get(self.active)
            .expect("active harness is registered")
    }

    pub fn short_name(&self) -> &'static str {
        self.harness().descriptor().short_name
    }

    pub fn display_name(&self) -> &'static str {
        self.harness().descriptor().display_name
    }

    /// The commands of its own the active harness's session listed.
    pub fn harness_commands(&self) -> &[HarnessCommand] {
        self.live_caps
            .get(&self.active)
            .and_then(|u| u.commands.as_deref())
            .unwrap_or_default()
    }

    /// Declared capabilities of the active harness plus what its session reported.
    pub fn caps(&self) -> Capabilities {
        let mut caps = self.harness().capabilities();
        if let Some(update) = self.live_caps.get(&self.active) {
            caps.apply(update);
        }
        caps
    }

    pub fn harness_binary(&self) -> Option<PathBuf> {
        let d = self.harness().descriptor();
        resolve_binary(d, self.config.binary_override(d.id.as_str()))
    }

    /// The policy the user asked for on the active harness: the one named
    /// for the run, or this harness's configured default.
    pub fn policy_requested(&self) -> PermissionPolicy {
        self.policy_explicit.unwrap_or_else(|| {
            // The config was checked at startup; `ask` if it changed since.
            crate::runner::configured_policy(&self.config, self.active)
                .unwrap_or(PermissionPolicy::Ask)
        })
    }

    /// The policy asked of the active harness: what the user chose for it,
    /// else the requested one.
    pub fn wanted_policy(&self) -> PermissionPolicy {
        self.policy_choice
            .get(&self.active)
            .copied()
            .unwrap_or_else(|| self.policy_requested())
    }

    fn policy_resolution(&self) -> Result<PolicyResolution, PolicyUnavailable> {
        resolve_policy(&self.caps().permission_policies, self.wanted_policy())
    }

    /// `None` until the user chooses, where the requested policy is not
    /// available: a more permissive one is never taken for them.
    pub fn effective_policy(&self) -> Option<PermissionPolicy> {
        self.policy_resolution().ok().map(|r| r.effective)
    }

    /// The effective policy for a message.
    fn policy_label(&self) -> String {
        match self.effective_policy() {
            Some(p) => p.to_string(),
            None => format!("{} (unavailable)", self.wanted_policy()),
        }
    }

    pub fn policy_warning(&self) -> Option<String> {
        match self.policy_resolution() {
            Ok(res) => res.warning,
            Err(e) => Some(format!("{e} (Ctrl+P)")),
        }
    }

    /// Take what the resumed conversation was run with, its policy, models
    /// and efforts, except where this run named its own. What the last
    /// conversation chose goes back to the configured default.
    fn take_conversation_choices(&mut self) {
        let conv = &self.conversation;
        self.policy_explicit = self.run_policy.or(conv.policy);
        // A policy named in this run for every harness holds over what the
        // conversation chose for each, also when it is the one the
        // conversation was named; what this run chose holds over both.
        self.policy_choice = if self.run_policy.is_none() {
            conv.policy_choices.clone()
        } else {
            HashMap::new()
        };
        self.policy_choice.extend(&self.run_choices);

        for h in std::mem::take(&mut self.chosen_models) {
            if self.run_models.contains(&h) {
                self.chosen_models.insert(h);
            } else {
                match self.configured_model(h) {
                    Some(m) => self.models.insert(h, m),
                    None => self.models.remove(&h),
                };
            }
        }
        for h in std::mem::take(&mut self.chosen_efforts) {
            if self.run_efforts.contains(&h) {
                self.chosen_efforts.insert(h);
            } else {
                match self.configured_effort(h) {
                    Some(e) => self.efforts.insert(h, e),
                    None => self.efforts.remove(&h),
                };
            }
        }

        let mut models: Vec<(HarnessId, ModelRef)> = self
            .conversation
            .models
            .iter()
            .map(|(h, m)| (*h, m.clone()))
            .collect();
        models.sort_by_key(|(h, _)| h.as_str());
        // An effort is for the model it was chosen with.
        let mut left = HashSet::new();
        for (h, m) in models {
            let Some(harness) = self.registry.get(h) else {
                continue;
            };
            if self.run_models.contains(&h) {
                continue;
            }
            match self.providers.get(&h) {
                Some(p) if *p != m.provider => {
                    let name = harness.descriptor().short_name;
                    self.transcript.push_notice(format!(
                        "the model chosen for {name} in this conversation ({}) is not used: {name} is on {p} now",
                        m.label(),
                    ));
                    left.insert(h);
                }
                _ => {
                    self.models.insert(h, m);
                    self.chosen_models.insert(h);
                }
            }
        }
        let efforts: Vec<(HarnessId, String)> = self
            .conversation
            .efforts
            .iter()
            .map(|(h, e)| (*h, e.clone()))
            .collect();
        for (h, e) in efforts {
            if self.run_efforts.contains(&h) || left.contains(&h) || self.registry.get(h).is_none()
            {
                continue;
            }
            self.efforts.insert(h, e);
            self.chosen_efforts.insert(h);
        }
    }

    /// The model the config names for `harness`.
    fn configured_model(&self, harness: HarnessId) -> Option<ModelRef> {
        let model = self
            .config
            .harness(harness.as_str())?
            .default_model
            .clone()?;
        let provider = self
            .providers
            .get(&harness)
            .cloned()
            .unwrap_or_else(|| ProviderId::new("default"));
        Some(ModelRef::new(harness, provider, model))
    }

    fn configured_effort(&self, harness: HarnessId) -> Option<String> {
        self.config
            .harness(harness.as_str())?
            .default_effort
            .clone()
    }

    /// Say which conversation was resumed, and whether its harness runs
    /// under another policy than it last did. `at_start`: by `--resume`.
    fn announce_resume(&mut self, at_start: bool) {
        let summary = self.conversation.summary();
        self.transcript.push_notice(format!(
            "resumed conversation {} ({}); continuing on {}",
            &summary.id[..8.min(summary.id.len())],
            summary
                .harnesses
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.short_name()
        ));
        let Some(last) = self.conversation.last_policies.get(&self.active).copied() else {
            return;
        };
        let now = self.effective_policy();
        if now == Some(last) {
            return;
        }
        let name = self.short_name();
        let wanted = self.wanted_policy();
        let why = if now != Some(wanted) {
            format!("{wanted} is not available on {name}")
        } else if self.run_choices.contains_key(&self.active) {
            "chosen in this run".to_string()
        } else if self.policy_choice.contains_key(&self.active) {
            format!("chosen for {name} in this conversation")
        } else if self.run_policy.is_some() {
            if at_start {
                "named with --policy".to_string()
            } else {
                "named in this run".to_string()
            }
        } else if self.policy_explicit.is_some() {
            "the policy named for this conversation".to_string()
        } else {
            "the configured default".to_string()
        };
        self.transcript.push_notice(match now {
            Some(now) => format!(
                "{name} last ran this conversation under {last}; it continues under {now} ({why})"
            ),
            None => format!(
                "{name} last ran this conversation under {last}; it waits for a policy ({why})"
            ),
        });
    }

    /// Ask for a policy where the requested one is not available. Returns
    /// whether one is in effect.
    pub fn require_policy(&mut self) -> bool {
        if self.effective_policy().is_some() {
            return true;
        }
        self.open_policy_picker();
        false
    }

    /// The sandbox level the active harness runs at, and why it is off if
    /// that was not asked for.
    pub fn sandbox_level(&self) -> (SandboxLevel, Option<String>) {
        self.sandbox.level(self.harness().default_sandbox())
    }

    /// What confines the next process of the active harness.
    pub fn session_sandbox(&self) -> anyhow::Result<Sandbox> {
        crate::runner::session_sandbox(
            self.harness(),
            &self.sandbox,
            &self.config,
            self.workspace_root.as_deref().unwrap_or(&self.cwd),
        )
    }

    /// The configured MCP servers the active harness takes for a session.
    /// The ones it does not take are named once per harness.
    pub fn session_mcp_servers(&mut self) -> Vec<McpServer> {
        let (servers, warnings) = mcp::for_harness(
            &self.config.mcp_servers().0,
            self.caps().mcp,
            self.short_name(),
            &self.harness().own_mcp_servers(),
        );
        if !warnings.is_empty() && self.mcp_warned.insert(self.active) {
            for w in warnings {
                self.transcript.push_notice(w);
            }
        }
        servers
    }

    /// Start watching the active harness's configuration files, before its
    /// process starts.
    pub fn arm_guard(&mut self) {
        self.guard = Some(Watch::begin(
            &self
                .harness()
                .guarded(self.workspace_root.as_deref().unwrap_or(&self.cwd)),
            dirs::home_dir().as_deref(),
        ));
    }

    /// Tell the user about configuration files that changed since the last
    /// check.
    fn check_guard(&mut self) {
        let name = self.display_name();
        let Some(watch) = self.guard.as_mut() else {
            return;
        };
        for change in watch.changes(&guard::default_keep_dir()) {
            self.transcript.push_error(change.describe(name));
        }
    }

    /// What the status area warns about: a missing sandbox, a degraded policy.
    pub fn status_warning(&self) -> Option<String> {
        let sandbox = self.sandbox_level().1.map(|s| format!("{s} (/sandbox)"));
        let provider = self.running_providers.get(&self.active).map(|runs_on| {
            format!(
                "runs on {runs_on}, not the chosen {} (/provider)",
                self.current_provider()
                    .map(|p| p.as_str())
                    .unwrap_or("provider")
            )
        });
        let parts: Vec<String> = [sandbox, self.policy_warning(), provider]
            .into_iter()
            .flatten()
            .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    pub fn current_provider(&self) -> Option<&ProviderId> {
        self.providers.get(&self.active)
    }

    /// The provider to tell the active harness, if the user chose one.
    pub fn chosen_provider(&self) -> Option<&ProviderId> {
        self.chosen_providers
            .contains(&self.active)
            .then(|| self.current_provider())
            .flatten()
    }

    pub fn current_model(&self) -> Option<&ModelRef> {
        self.models.get(&self.active)
    }

    pub fn current_effort(&self) -> Option<&str> {
        self.efforts.get(&self.active).map(String::as_str)
    }

    pub fn model_label(&self) -> String {
        match self.current_model() {
            Some(m) => m.label(),
            None => match self.current_provider() {
                Some(p) => format!("{p}/default"),
                None => "default".to_string(),
            },
        }
    }

    pub fn take_actions(&mut self) -> Vec<Action> {
        self.actions.drain(..).collect()
    }

    pub fn is_thinking(&self) -> bool {
        self.is_generating && self.transcript.is_thinking()
    }

    pub fn elapsed_secs(&self) -> f32 {
        if self.is_generating {
            self.generation_start
                .map(|s| s.elapsed().as_secs_f32())
                .unwrap_or(0.0)
        } else {
            self.generation_duration
                .map(|d| d.as_secs_f32())
                .unwrap_or(0.0)
        }
    }

    /// What the agent is doing right now, for the header.
    pub fn status_label(&self) -> String {
        if self.awaiting_answer() {
            return "Waiting for you".to_string();
        }
        if self.compacting {
            return "Compacting".to_string();
        }
        if self.handoff.is_some() {
            return "Writing a handoff summary".to_string();
        }
        if let Some(super::transcript::Block::Tool {
            name, done: false, ..
        }) = self
            .transcript
            .blocks
            .iter()
            .rev()
            .find(|b| matches!(b, super::transcript::Block::Tool { .. }))
        {
            return format!("Running {name}");
        }
        if self.transcript.is_thinking() {
            return "Thinking".to_string();
        }
        if matches!(
            self.transcript.blocks.last(),
            Some(super::transcript::Block::Assistant { duration: None, .. })
        ) {
            return "Streaming".to_string();
        }
        "Working".to_string()
    }

    /// A turn is running, subagents are still at work after one, or a `!`
    /// command runs.
    pub fn is_busy(&self) -> bool {
        self.is_generating || !self.subagents.is_empty() || self.shell.is_some()
    }

    /// What herdr is told about this session: blocked while a harness
    /// request waits for an answer (open or queued behind another modal) or
    /// the policy picker holds the session, working while busy, otherwise
    /// idle.
    pub fn herdr_report(&self) -> super::herdr::Report {
        let waiting = match &self.modal {
            Some(m) if m.is_prompt() => m.waiting_on(),
            Some(Modal::Policy(_)) if self.effective_policy().is_none() => Some(format!(
                "choose a permission policy for {}",
                self.display_name()
            )),
            _ => None,
        }
        .or_else(|| {
            self.pending_prompts
                .front()
                .map(|req| super::herdr::request_message(&req.kind))
        });
        super::herdr::Report::of(self.is_busy(), waiting)
    }

    /// "2 subagents running", when any are.
    pub fn subagents_label(&self) -> Option<String> {
        match self.subagents.len() {
            0 => None,
            1 => Some("1 subagent running".to_string()),
            n => Some(format!("{n} subagents running")),
        }
    }

    /// How long the longest-running subagent has been at work.
    pub fn subagents_elapsed_secs(&self) -> f32 {
        self.subagents
            .iter()
            .map(|a| a.started.elapsed().as_secs_f32())
            .fold(0.0, f32::max)
    }

    pub fn tick_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
    }

    pub fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame]
    }

    // --------------------------------------------------------------- generation

    fn start_generation(&mut self) {
        self.is_generating = true;
        self.interrupted = None;
        self.transcript.begin_turn();
        self.generation_start = Some(Instant::now());
        self.generation_duration = None;
        self.turn_usage = Usage::default();
        self.auto_scroll = true;
    }

    fn finish_generation(&mut self) {
        if !self.is_generating {
            return;
        }
        self.is_generating = false;
        self.interrupted = None;
        self.compacting = false;
        let dur = self
            .generation_start
            .take()
            .map(|s| s.elapsed())
            .unwrap_or_default();
        self.generation_duration = Some(dur);
        self.transcript.finish_turn(dur);
    }

    /// Send the prompt box contents as a turn.
    pub fn submit_prompt(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() || self.is_generating {
            return;
        }
        // A picker the user asked for and moved on from stays shut.
        self.open_when_listed = None;
        if self.shell.is_some() || !self.require_policy() {
            // Kept until the `!` command has ended, or a policy is chosen.
            self.queued.push_back(QueuedPrompt {
                text,
                attachments: std::mem::take(&mut self.attachments),
                harness: self.active,
            });
            return;
        }
        if self.first_prompt.is_none() {
            self.first_prompt = Some(text.clone());
        }

        // A command the harness runs has to start the message, so it goes
        // alone: what would have gone in front of it waits for the next
        // prompt.
        let alone = text.starts_with('/') && self.caps().slash_commands;
        // Bridge context from other harnesses: everything since this harness
        // was last active, or everything on its first visit. Nothing when the
        // live session already saw the whole transcript.
        let from = self.bridge_start(self.active);
        let bridge = if from >= self.transcript.blocks.len() {
            None
        } else {
            self.transcript
                .bridge_text(from, self.bridge_budget(self.active))
        };
        if alone && bridge.is_some() {
            self.last_active_index.insert(self.active, from);
        } else {
            self.last_active_index.remove(&self.active);
        }
        let unsent_shell = self
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, super::transcript::Block::Shell { sent: false, .. }));
        if alone && (bridge.is_some() || unsent_shell) {
            self.transcript.push_notice(format!(
                "{} is told what it has not seen yet with your next prompt: a command has to start its message",
                self.short_name()
            ));
        }
        let bridge = bridge.filter(|_| !alone);
        // `!` commands no agent has been told about yet (the bridge has
        // only those one has).
        let mut shells = Vec::new();
        for b in self.transcript.blocks.iter_mut().filter(|_| !alone) {
            if let super::transcript::Block::Shell {
                command,
                output,
                dropped,
                status,
                sent: sent @ false,
                ..
            } = b
            {
                shells.push(super::shell::context_entry(
                    command,
                    output,
                    *dropped,
                    status,
                    super::shell::CONTEXT_MAX_CHARS,
                ));
                *sent = true;
            }
        }

        let mut shown = text.clone();
        let attachments = self.take_attachments(&mut shown);
        self.transcript.push_user(shown);
        let block = self.transcript.blocks.len() - 1;
        if self.caps().rewind.conversation {
            self.anchor_pending.push_back(block);
        }
        self.checkpoint_files(block);
        self.start_generation();

        let context: Vec<String> = bridge
            .map(|ctx| {
                format!(
                    "[Context: earlier conversation in this unharness session, possibly with other agents]\n{ctx}"
                )
            })
            .into_iter()
            .chain(super::shell::context(&shells))
            .collect();
        let outgoing = if context.is_empty() {
            text
        } else {
            format!(
                "{}\n\n[Current task for {}]:\n{text}",
                context.join("\n\n"),
                self.short_name()
            )
        };

        self.ensure_session();
        self.mark_prompted();
        self.actions.push_back(Action::SendTurn {
            text: outgoing,
            attachments,
        });
    }

    /// A session for the active harness, unless it has one or one is on
    /// its way.
    fn ensure_session(&mut self) {
        if !self.session_alive && !self.start_pending() {
            let resume = self.session_ids.get(&self.active).cloned();
            self.actions.push_back(Action::StartSession { resume });
        }
    }

    /// Start the active harness's session before anything is sent to it,
    /// so that what it reports (its commands above all) is there before the
    /// first prompt. The event loop calls this before it runs the actions;
    /// nothing starts while a policy is still to be chosen, during a turn,
    /// or after a session ended by itself or failed to start.
    pub fn start_when_idle(&mut self) {
        if self.should_quit
            || self.session_alive
            || self.start_pending()
            || self.is_generating
            || self.handoff.is_some()
            || self.idle_start_paused
            || self.effective_policy().is_none()
        {
            return;
        }
        if !self.caps().start_unprompted {
            self.list_commands();
            return;
        }
        let resume = self.session_ids.get(&self.active).cloned();
        self.actions.push_back(Action::StartSession { resume });
        self.session_blank = true;
    }

    /// Ask the active harness for its commands without a session, once,
    /// where the session would not say before the first prompt.
    fn list_commands(&mut self) {
        let request = ListRequest::Commands(self.active);
        let listed = self
            .live_caps
            .get(&self.active)
            .is_some_and(|c| c.commands.is_some());
        if !listed && !self.lists_failed.contains(&request) {
            self.request_list(request);
        }
    }

    /// The session could not be started. The next prompt tries again.
    pub fn start_failed(&mut self, why: String) {
        self.session_blank = false;
        self.blank_session = None;
        self.idle_start_paused = true;
        if self.is_generating {
            self.on_event(AgentEvent::TurnCompleted {
                stop_reason: StopReason::Error(why),
            });
        } else {
            self.transcript.push_error(why);
        }
    }

    /// The session's driver stopped taking commands.
    pub fn driver_gone(&mut self) {
        self.transcript.push_error("session driver is gone");
        self.session_alive = false;
        self.session_blank = false;
        self.blank_session = None;
        self.idle_start_paused = true;
    }

    /// A start is queued, or held until the CLI before it is gone.
    fn start_pending(&self) -> bool {
        self.start_held
            || self
                .actions
                .iter()
                .any(|a| matches!(a, Action::StartSession { .. }))
    }

    /// Whether the live session has been sent something: one started
    /// early has seen nothing of the conversation.
    fn session_seen(&self) -> bool {
        self.session_alive && !self.session_blank
    }

    /// Something goes to the session: from here on it is the
    /// conversation's.
    fn mark_prompted(&mut self) {
        self.session_blank = false;
        if let Some((id, model)) = self.blank_session.take() {
            self.commit_session(id, model);
        }
    }

    /// `id` is the active harness's session in this conversation.
    fn commit_session(&mut self, session_id: String, model: Option<String>) {
        let is_new = self.session_ids.get(&self.active) != Some(&session_id);
        // A branch of the forked-from session now exists under its own id.
        if is_new {
            self.fork_pending.remove(&self.active);
        }
        self.session_ids.insert(self.active, session_id.clone());
        self.persist();
        if is_new {
            self.transcript.push_notice(format!(
                "session {} ({})",
                session_id,
                model.unwrap_or_else(|| "default model".into())
            ));
        }
    }

    /// How long the bridge to `harness` may be: `bridge_max_chars` when
    /// set, else a quarter of the context window its model last reported
    /// here at about four characters a token, else the default.
    pub fn bridge_budget(&self, harness: HarnessId) -> usize {
        self.bridge_max_chars
            .or_else(|| {
                self.context_windows
                    .get(&harness)?
                    .get(&self.model_key(harness))
                    .map(|w| {
                        usize::try_from(*w).unwrap_or(usize::MAX) / BRIDGE_WINDOW_SHARE
                            * CHARS_PER_TOKEN
                    })
            })
            .unwrap_or(DEFAULT_BRIDGE_MAX_CHARS)
    }

    /// The model `harness` runs, as the context windows are kept by.
    fn model_key(&self, harness: HarnessId) -> String {
        self.models
            .get(&harness)
            .map_or_else(|| "default".to_string(), ModelRef::label)
    }

    /// Keep the window the active harness's model reported, for the next
    /// bridge to it.
    fn learn_context_window(&mut self, window: u64) {
        let window = window.clamp(CONTEXT_WINDOW_MIN, CONTEXT_WINDOW_MAX);
        let model = self.model_key(self.active);
        let known = self.context_windows.entry(self.active).or_default();
        if known.get(&model) == Some(&window) {
            return;
        }
        known.insert(model, window);
        // Only a measure: a write that fails costs a default budget later.
        let _ = save_context_windows(&self.context_windows_path, &self.context_windows);
    }

    /// Where the bridge to `harness` starts: where it was last active,
    /// nothing when its live session saw it all, everything on its first
    /// visit.
    fn bridge_start(&self, harness: HarnessId) -> usize {
        let visited = self.session_ids.contains_key(&harness)
            || (harness == self.active && self.session_seen());
        if visited {
            self.last_active_index
                .get(&harness)
                .copied()
                .unwrap_or(self.transcript.blocks.len())
        } else {
            0
        }
    }

    /// Record the working tree as it is before the turn at `block` runs.
    fn checkpoint_files(&mut self, block: usize) {
        let Some(cp) = self.checkpoints.clone() else {
            return;
        };
        match cp.snapshot(&self.checkpoint_name(&block.to_string())) {
            Ok(commit) => {
                self.file_checkpoints.retain(|c| c.block != block);
                self.file_checkpoints
                    .push(CheckpointRecord { block, commit });
            }
            Err(e) if !self.checkpoint_failed => {
                self.checkpoint_failed = true;
                self.transcript.push_notice(format!(
                    "could not checkpoint files (rewind will not be able to restore them): {e:#}"
                ));
            }
            Err(_) => {}
        }
    }

    /// Hold a prompt until the running turn finishes.
    pub fn queue_prompt(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.is_generating && self.shell.is_none() {
            self.submit_prompt(text);
            return;
        }
        self.queued.push_back(QueuedPrompt {
            text,
            attachments: std::mem::take(&mut self.attachments),
            harness: self.active,
        });
    }

    /// Send the oldest queued prompt, if idle. Returns whether one was sent.
    pub fn send_next_queued(&mut self) -> bool {
        if self.is_generating || self.shell.is_some() || self.effective_policy().is_none() {
            return false;
        }
        let Some(q) = self.queued.pop_front() else {
            return false;
        };
        // Whether it runs as a command depends on the harness: one typed
        // for another goes back to the prompt box instead.
        if q.text.starts_with('/') && q.harness != self.active {
            self.transcript.push_notice(format!(
                "'{}' was typed for {}; it is back in the prompt box",
                q.text.lines().next().unwrap_or_default(),
                self.registry
                    .get(q.harness)
                    .map_or(q.harness.as_str(), |h| h.descriptor().short_name)
            ));
            // In front of what is being typed, if anything.
            self.cursor = q.text.chars().count();
            self.input = if self.input.is_empty() {
                q.text
            } else {
                format!("{}\n{}", q.text, self.input)
            };
            self.attachments.extend(q.attachments);
            return self.send_next_queued();
        }
        let pending = std::mem::replace(&mut self.attachments, q.attachments);
        self.submit_prompt(q.text);
        self.attachments = pending;
        true
    }

    /// Put the newest queued prompt back in the prompt box for editing.
    pub fn unqueue_last(&mut self) {
        if !self.input.is_empty() {
            return;
        }
        if let Some(q) = self.queued.pop_back() {
            self.cursor = q.text.chars().count();
            self.input = q.text;
            self.attachments.extend(q.attachments);
        }
    }

    /// Inject a message into the running turn, where the harness can; queue it otherwise.
    /// A handoff summary's turn is not steered: the answer to the message
    /// would be taken for the summary. The message goes to the next harness.
    pub fn steer(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.is_generating || self.compacting || self.handoff.is_some() {
            self.queue_prompt(text);
            return;
        }
        if !self.caps().steer || !self.session_alive {
            self.transcript.push_notice(format!(
                "{} cannot be steered mid-turn; queued for when it finishes",
                self.short_name()
            ));
            self.queue_prompt(text);
            return;
        }
        let mut shown = text.clone();
        let attachments = self.take_attachments(&mut shown);
        self.transcript.push_user(shown);
        self.actions
            .push_back(Action::Command(SessionCommand::Steer { text, attachments }));
    }

    /// The harness's own id for the user turn at `block`, when its session
    /// (running or resumable) can be rewound to it.
    fn native_anchor(&self, block: usize) -> Option<&str> {
        if !self.caps().rewind.conversation || !self.session_ids.contains_key(&self.active) {
            return None;
        }
        // Going back to before the session's first turn leaves nothing of
        // it: a fresh session is the same thing, and Claude refuses to
        // rewind to its first message ("stale target").
        let mut mine = self.anchors.iter().filter(|a| a.harness == self.active);
        if !mine.clone().any(|a| a.block < block) {
            return None;
        }
        mine.find(|a| a.block == block).map(|a| a.id.as_str())
    }

    /// The file checkpoint taken just before the user block at `block`.
    fn checkpoint_at(&self, block: usize) -> Option<&str> {
        self.file_checkpoints
            .iter()
            .find(|c| c.block == block)
            .map(|c| c.commit.as_str())
    }

    /// Name of a checkpoint ref for this conversation (see `core::checkpoints`).
    fn checkpoint_name(&self, suffix: &str) -> String {
        let id = &self.conversation.id;
        format!("{}/{suffix}", &id[..8.min(id.len())])
    }

    /// Whether the active harness's next session must branch the stored
    /// session id (it belongs to the conversation this one was forked from).
    pub fn fork_pending(&self) -> bool {
        self.fork_pending.contains(&self.active)
    }

    pub fn open_rewind_picker(&mut self) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before rewinding");
            return;
        }
        let items: Vec<RewindOption> = self
            .transcript
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(block, b)| match b {
                super::transcript::Block::User { text } => Some(RewindOption {
                    block,
                    text: text.clone(),
                    native: self.native_anchor(block).is_some(),
                    files: self.checkpoint_at(block).is_some(),
                }),
                _ => None,
            })
            .collect();
        if items.is_empty() {
            self.transcript.push_notice("nothing to rewind to yet");
            return;
        }
        let last = items.len() - 1;
        self.modal = Some(Modal::Rewind(
            ListPicker::new(items).with_selected(Some(last)),
        ));
    }

    /// Drop the user turn at `block` and everything after it, and put its
    /// text back in the prompt box. The active harness's session is rewound
    /// natively where it can be; any session that saw the dropped turns and
    /// cannot be rewound is replaced by a fresh one that gets the remaining
    /// conversation as context. With `restore_files`, the working tree is
    /// first put back to how it was before that turn.
    pub fn rewind_to(&mut self, block: usize, restore_files: bool) {
        // A rewind may start a session, which needs a policy.
        if self.is_generating || self.refused_while_start_held() || !self.require_policy() {
            return;
        }
        let Some(super::transcript::Block::User { text }) = self.transcript.blocks.get(block)
        else {
            return;
        };
        // The attachment markers are display only.
        let prompt: String = text
            .lines()
            .filter(|l| !(l.starts_with("[image: ") && l.ends_with(']')))
            .collect::<Vec<_>>()
            .join("\n");

        // Files first: if they cannot be restored, nothing else changes.
        let mut files_note = "files on disk are unchanged".to_string();
        if restore_files {
            let (Some(cp), Some(commit)) = (
                self.checkpoints.clone(),
                self.checkpoint_at(block).map(str::to_string),
            ) else {
                self.transcript
                    .push_error("there is no file checkpoint for that prompt");
                return;
            };
            self.restore_seq += 1;
            let undo_name = self.checkpoint_name(&format!("undo-{}", self.restore_seq));
            match cp.restore(&commit, &undo_name) {
                Ok(report) => {
                    files_note = format!(
                        "{} file(s) put back to how they were before that prompt (/undo-restore reverses this)",
                        report.changed
                    );
                    self.restore_undo = Some(report.undo);
                }
                Err(e) => {
                    self.transcript.push_error(format!(
                        "could not restore files, nothing was rewound: {e:#}"
                    ));
                    return;
                }
            }
        }

        let native = self.native_anchor(block).map(str::to_string);
        let active = self.active;
        match &native {
            Some(anchor) => {
                // A session that is not running is reattached first.
                self.ensure_session();
                self.actions
                    .push_back(Action::Command(SessionCommand::Rewind {
                        anchor: anchor.clone(),
                    }));
                // Rewound, it is the conversation's: a restart now would
                // resume it from before the rewind.
                self.mark_prompted();
            }
            None => self.forget_session(active),
        }
        // Other harnesses that were active after the rewind point saw turns
        // that no longer exist.
        let stale: Vec<HarnessId> = self
            .last_active_index
            .iter()
            .filter(|(h, seen)| **h != active && **seen > block)
            .map(|(h, _)| *h)
            .collect();
        for h in stale {
            self.forget_session(h);
        }

        self.transcript.blocks.truncate(block);
        self.anchors.retain(|a| a.block < block);
        self.file_checkpoints.retain(|c| c.block < block);
        self.anchor_pending.clear();
        self.queued.clear();
        self.cursor = prompt.chars().count();
        self.input = prompt;
        self.transcript.push_system(match native {
            Some(_) => format!(
                "Rewound. {} forgot the later turns; {files_note}.",
                self.short_name()
            ),
            None => format!(
                "Rewound. {} will start a fresh session with the conversation so far; {files_note}.",
                self.short_name()
            ),
        });
        self.auto_scroll = true;
        self.persist();
    }

    /// What was held for a session start that will not come (unharness
    /// quits): a rewind among it never reached the vendor session, which
    /// is then not resumed with the turns the transcript dropped.
    pub fn drop_held(&mut self, held: impl IntoIterator<Item = Action>) {
        let rewind = held
            .into_iter()
            .any(|a| matches!(a, Action::Command(SessionCommand::Rewind { .. })));
        if rewind {
            self.forget_session(self.active);
            self.persist();
        }
    }

    /// Stop using `harness`'s vendor session: its next turn starts a fresh
    /// one and gets the conversation so far as context.
    fn forget_session(&mut self, harness: HarnessId) {
        if harness == self.active {
            self.end_remote_control();
        }
        if harness == self.active && self.session_alive {
            self.shutdown_session();
        }
        self.session_ids.remove(&harness);
        self.last_active_index.remove(&harness);
        self.fork_pending.remove(&harness);
        self.anchors.retain(|a| a.harness != harness);
    }

    /// Reverse the last file restore (itself reversible the same way).
    pub fn undo_restore(&mut self) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn first");
            return;
        }
        let (Some(cp), Some(undo)) = (self.checkpoints.clone(), self.restore_undo.clone()) else {
            self.transcript.push_notice("no file restore to undo");
            return;
        };
        self.restore_seq += 1;
        let name = self.checkpoint_name(&format!("undo-{}", self.restore_seq));
        match cp.restore(&undo, &name) {
            Ok(report) => {
                self.restore_undo = Some(report.undo);
                self.transcript.push_system(format!(
                    "{} file(s) put back to how they were before the restore.",
                    report.changed
                ));
            }
            Err(e) => self
                .transcript
                .push_error(format!("could not undo the restore: {e:#}")),
        }
    }

    /// Continue in a copy of this conversation. Each harness's session is
    /// branched natively where the harness can (the copy then knows exactly
    /// what the original knew); the others start fresh with the transcript
    /// as context. The original and its sessions are left untouched.
    pub fn fork_conversation(&mut self) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before forking");
            return;
        }
        if self.refused_while_start_held() {
            return;
        }
        if !self.conversation.has_content() {
            self.transcript.push_notice("nothing to fork yet");
            return;
        }
        self.persist();
        let original = self.conversation.id.clone();
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session();
        }
        self.idle_start_paused = false;
        // A copy ran where the original ran.
        let mut fork = self.successor();
        fork.title = self.conversation.title.clone();
        fork.last_policies = self.conversation.last_policies.clone();
        self.conversation = fork;

        let mut branched = Vec::new();
        let mut fresh = Vec::new();
        let harnesses: Vec<HarnessId> = self.session_ids.keys().copied().collect();
        for h in harnesses {
            let caps = self.registry.get(h).map(|x| x.capabilities());
            match caps {
                Some(c) if c.fork && c.resume_by_id => {
                    self.fork_pending.insert(h);
                    if !c.rewind.anchors_survive_fork {
                        self.anchors.retain(|a| a.harness != h);
                    }
                    branched.push(h.as_str());
                }
                _ => {
                    self.session_ids.remove(&h);
                    self.last_active_index.remove(&h);
                    self.anchors.retain(|a| a.harness != h);
                    fresh.push(h.as_str());
                }
            }
        }
        branched.sort_unstable();
        fresh.sort_unstable();
        self.reset_usage();
        let mut note = format!(
            "Forked from conversation {}; the original is unchanged.",
            &original[..8.min(original.len())]
        );
        if !branched.is_empty() {
            note.push_str(&format!(" Sessions branched: {}.", branched.join(", ")));
        }
        if !fresh.is_empty() {
            note.push_str(&format!(
                " Fresh sessions with the transcript as context: {}.",
                fresh.join(", ")
            ));
        }
        self.transcript.push_system(note);
        self.persist();
    }

    /// Start a new conversation in this workspace, as a fresh launch would,
    /// keeping the harness and what the user chose to run it with. The one
    /// left is saved first and stays in `/resume`; its sessions stay with
    /// it, so every harness starts afresh in the new one.
    pub fn new_conversation(&mut self) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before clearing");
            return;
        }
        if self.refused_while_start_held() {
            return;
        }
        // They would end with the session, and report into the new one.
        if !self.subagents.is_empty() {
            self.transcript.push_error(format!(
                "{} subagent(s) still at work would be stopped; wait for them or stop them (Ctrl+S) before clearing",
                self.subagents.len()
            ));
            return;
        }
        if self.shell.is_some() {
            self.transcript.push_error(
                "stop the ! command first (Esc); its output belongs to this conversation",
            );
            return;
        }
        // Nothing of it is dropped: one that cannot be saved stays.
        if let Err(e) = self.save_conversation() {
            self.transcript.push_error(format!(
                "could not save the conversation, so /clear keeps it: {e:#}"
            ));
            return;
        }
        let had_content = self.conversation.has_content();
        // A session started with nothing to resume and sent nothing knows
        // nothing of this conversation, and serves the next. Any other ends,
        // a blank one resumed on a session of this conversation too.
        let keep = self.session_alive
            && self.session_blank
            && !self.session_ids.contains_key(&self.active);
        self.end_remote_control();
        let said = self.transcript.blocks.len();
        if self.session_alive && !keep {
            self.shutdown_session();
        }
        // What ending it told (a change to the harness's configuration) is
        // told in the new conversation.
        let told = self.transcript.blocks.split_off(said);
        self.idle_start_paused = false;
        self.conversation = self.successor();
        self.pre_plan.clear();
        self.transcript = Transcript::default();
        self.session_ids.clear();
        self.last_active_index.clear();
        self.fork_pending.clear();
        self.plan.clear();
        self.anchors.clear();
        self.file_checkpoints.clear();
        self.restore_undo = None;
        self.first_prompt = None;
        self.generation_duration = None;
        self.reset_usage();
        self.subagent_focus = None;
        self.close_subagent_view();
        self.show_from_the_end();
        self.transcript.push_system(if had_content {
            "New conversation; the previous one is in /resume."
        } else {
            "New conversation."
        });
        self.transcript.blocks.extend(told);
        // Prompts held after a turn that did not finish are the user's, not
        // the conversation's.
        if !self.queued.is_empty() {
            self.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    /// A new conversation that keeps the models and efforts this one chose.
    fn successor(&self) -> Conversation {
        let mut next = Conversation::new(self.active);
        next.models = self.conversation.models.clone();
        next.efforts = self.conversation.efforts.clone();
        next
    }

    /// Usage and context start again with a session that knows nothing.
    fn reset_usage(&mut self) {
        self.anchor_pending.clear();
        self.session_usage = Usage::default();
        self.turn_usage = Usage::default();
        self.context = ContextUsage::default();
    }

    /// Ask the harness to summarise its context now.
    pub fn compact(&mut self, instructions: Option<String>) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before compacting");
            return;
        }
        if !self.caps().compaction {
            self.transcript
                .push_error(format!("{} cannot compact on request", self.short_name()));
            return;
        }
        if !self.session_seen() {
            self.transcript
                .push_notice("no live session to compact; send a prompt first");
            return;
        }
        self.start_generation();
        self.compacting = true;
        self.actions
            .push_back(Action::Command(SessionCommand::Compact { instructions }));
    }

    /// Why the active harness cannot take this attachment.
    fn refusal(&self, a: &Attachment) -> String {
        match a {
            Attachment::Image { .. } => format!(
                "{} does not accept images with the current model",
                self.short_name()
            ),
            Attachment::File { .. } => format!(
                "{} does not accept file attachments, only images",
                self.short_name()
            ),
        }
    }

    /// The pending attachments the active harness accepts, with their
    /// transcript markers appended to `shown`. The others are dropped, and
    /// said to be: one queued for another harness can end up here.
    fn take_attachments(&mut self, shown: &mut String) -> Vec<Attachment> {
        let caps = self.caps();
        let (sent, dropped): (Vec<_>, Vec<_>) = std::mem::take(&mut self.attachments)
            .into_iter()
            .partition(|a| caps.accepts(a));
        for a in &dropped {
            let why = self.refusal(a);
            self.transcript
                .push_error(format!("{} not sent: {why}", a.label()));
        }
        for a in &sent {
            shown.push_str(&format!("\n{}", a.marker()));
        }
        sent
    }

    /// The attachment `path` makes, or why the active harness gets none.
    fn attachable(&self, path: &Path) -> Result<Attachment, String> {
        match Attachment::from_path(path) {
            Some(a) if !self.caps().accepts(&a) => Err(self.refusal(&a)),
            Some(a) => Ok(a),
            None => {
                Err("only images (png, jpg, gif, webp), PDFs and text files can be attached".into())
            }
        }
    }

    fn push_attachment(&mut self, a: Attachment) {
        self.transcript
            .push_system(format!("Attached {} to the next prompt.", a.label()));
        self.attachments.push(a);
    }

    /// Queue an image or a document for the next prompt.
    pub fn attach(&mut self, path: &str) {
        let path = path.trim().trim_matches(['"', '\'']);
        let path = match path.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None => self.cwd.join(path),
        };
        if !path.is_file() {
            self.transcript
                .push_error(format!("no such file: {}", path.display()));
            return;
        }
        match self.attachable(&path) {
            Ok(a) => self.push_attachment(a),
            Err(why) => self.transcript.push_error(why),
        }
    }

    /// Files dropped onto the terminal: attach those the harness takes.
    /// The others stay what they were, paths in the prompt.
    fn attach_dropped(&mut self, pasted: &str, paths: Vec<PathBuf>) {
        let mut refused = Vec::new();
        for path in &paths {
            match self.attachable(path) {
                Ok(a) => self.push_attachment(a),
                Err(why) => {
                    self.transcript
                        .push_notice(format!("{} not attached: {why}", path.display()));
                    refused.push(path.display().to_string());
                }
            }
        }
        if refused.len() == paths.len() {
            self.insert_str(pasted);
        } else if !refused.is_empty() {
            self.insert_str(&refused.join(" "));
        }
    }

    /// True, with an error shown, while the session's start is held.
    fn refused_while_start_held(&mut self) -> bool {
        if self.start_held {
            self.transcript.push_error(format!(
                "the previous {} is still exiting; try again in a moment",
                self.short_name()
            ));
        }
        self.start_held
    }

    /// Ask the running turn to stop. Asked again once `STOP_GRACE` has
    /// passed without the turn ending, end the CLI's process instead.
    pub fn interrupt(&mut self) {
        if !self.is_generating {
            return;
        }
        match self.interrupted {
            None => {
                self.actions
                    .push_back(Action::Command(SessionCommand::Interrupt));
                self.transcript.push_system("Interrupting…");
                self.interrupted = Some((Instant::now(), false));
            }
            // Said before the process is ended, however long the wait.
            Some((at, false)) => {
                let notice = if at.elapsed() < STOP_GRACE {
                    format!(
                        "waiting for {} to stop; asked again after {}s, its process is ended",
                        self.short_name(),
                        STOP_GRACE.as_secs()
                    )
                } else {
                    format!(
                        "{} has not stopped; asked again, its process is ended",
                        self.short_name()
                    )
                };
                self.transcript.push_notice(notice);
                self.interrupted = Some((at, true));
            }
            Some((at, true)) if at.elapsed() < STOP_GRACE => {}
            Some(_) => self.end_stuck_turn(),
        }
    }

    /// The CLI did not stop when asked: end its process and the turn. The
    /// next prompt starts it again on the same session.
    fn end_stuck_turn(&mut self) {
        self.transcript.push_notice(format!(
            "{} did not stop, so its process was ended; the next prompt resumes the session",
            self.short_name()
        ));
        // What the turn changed is told before the next process is watched.
        self.check_guard();
        self.shutdown_session();
        // Not started again before the next prompt.
        self.idle_start_paused = true;
        self.process_gone();
        self.transcript.end_running_tools();
        self.finish_generation();
        self.transcript.push_system("Turn interrupted.");
        self.finish_handoff(false);
        self.persist();
        if !self.queued.is_empty() {
            self.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    // ------------------------------------------------------------- `!` commands

    /// Run `command`, typed after `!`, in the session's directory under its
    /// sandbox. Not during a turn, and one at a time.
    pub fn run_shell(&mut self, command: &str) {
        let typed = format!("!{command}");
        if command.is_empty() {
            self.transcript
                .push_notice("nothing to run after `!` (`\\!` sends a prompt that starts with !)");
            return;
        }
        if self.is_generating || self.shell.is_some() {
            self.transcript.push_notice(if self.is_generating {
                "a `!` command cannot run during a turn; it is back in the prompt"
            } else {
                "a `!` command is already running (Esc stops it); this one is back in the prompt"
            });
            if self.input.is_empty() {
                self.set_input(&typed);
            }
            return;
        }
        self.transcript.shell_started(command);
        self.shell = Some(ShellRun {
            started: Instant::now(),
            stopping: false,
        });
        self.auto_scroll = true;
        self.actions.push_back(Action::RunShell {
            command: command.to_string(),
        });
    }

    /// Kill the running `!` command (Esc, Ctrl+C).
    pub fn stop_shell(&mut self) {
        if let Some(run) = self.shell.as_mut()
            && !run.stopping
        {
            run.stopping = true;
            self.actions.push_back(Action::StopShell);
        }
    }

    pub fn shell_output(&mut self, line: &str) {
        if self.shell.is_some() {
            self.transcript.shell_output(line);
        }
    }

    /// The `!` command ended with `code`, `None` when a signal ended it.
    pub fn shell_exited(&mut self, code: Option<i32>) {
        let Some(run) = self.shell.take() else {
            return;
        };
        self.transcript.shell_ended(match code {
            Some(code) => ShellStatus::Exited { code },
            None if run.stopping => ShellStatus::Killed,
            None => ShellStatus::Signalled,
        });
        self.after_shell();
    }

    /// The `!` command could not be started.
    pub fn shell_failed(&mut self, error: String) {
        if self.shell.take().is_none() {
            return;
        }
        self.transcript.shell_ended(ShellStatus::Failed { error });
        self.after_shell();
    }

    fn after_shell(&mut self) {
        self.persist();
        // Prompts sent while it ran waited for its output.
        self.send_next_queued();
    }

    /// The list under the prompt: the subagents at work, then those that
    /// have ended, latest first. An ended one stays until the user takes
    /// it off, so that what it did can still be read.
    pub fn subagent_rows(&self) -> Vec<SubagentRow> {
        let mut rows: Vec<SubagentRow> = self
            .transcript
            .listed_agents()
            .into_iter()
            .map(|(id, run, depth)| {
                let live = self.subagents.iter().find(|a| a.id == id);
                SubagentRow {
                    id: id.to_string(),
                    description: run.description.clone(),
                    kind: run.kind.clone(),
                    status: run.status,
                    depth,
                    activity: live.and_then(|a| a.activity.clone()),
                    tools: live.map_or(0, |a| a.tools),
                    secs: run
                        .duration
                        .unwrap_or_else(|| run.started.elapsed())
                        .as_secs_f32(),
                }
            })
            .collect();
        let ended = rows.iter().filter(|r| r.status.is_some()).count();
        // (Stable: the running keep their order; the ended are then reversed.)
        rows.sort_by_key(|r| r.status.is_some());
        let n = rows.len();
        rows[n - ended..].reverse();
        rows
    }

    /// Where the keyboard is in that list, if it is there (and the
    /// subagent it was on is still listed).
    pub fn subagent_focus_index(&self) -> Option<usize> {
        let id = self.subagent_focus.as_ref()?;
        self.subagent_rows().iter().position(|r| &r.id == id)
    }

    /// Down: from the prompt into the list, then along it. False when
    /// there is no list to go into.
    pub fn subagent_list_down(&mut self) -> bool {
        let rows = self.subagent_rows();
        let next = match self.subagent_focus_index() {
            Some(i) => (i + 1).min(rows.len().saturating_sub(1)),
            None => 0,
        };
        self.subagent_focus = rows.get(next).map(|r| r.id.clone());
        self.subagent_focus.is_some()
    }

    /// Up: along the list, and from its first row back to the prompt.
    pub fn subagent_list_up(&mut self) {
        let rows = self.subagent_rows();
        self.subagent_focus = match self.subagent_focus_index() {
            Some(i) if i > 0 => rows.get(i - 1).map(|r| r.id.clone()),
            _ => None,
        };
    }

    /// Take the chosen subagent off the list, if it has ended. The
    /// keyboard moves to the row that takes its place, or back to the
    /// prompt when the list is empty.
    pub fn dismiss_focused_subagent(&mut self) {
        let rows = self.subagent_rows();
        let Some(at) = self.subagent_focus_index() else {
            return;
        };
        if rows[at].status.is_none() {
            self.flash("Still running: open it and press s to stop it");
            return;
        }
        if let Some(run) = self.transcript.agent_mut(&rows[at].id) {
            run.dismissed = true;
        }
        self.persist();
        let rows = self.subagent_rows();
        self.subagent_focus = rows
            .get(at.min(rows.len().saturating_sub(1)))
            .map(|r| r.id.clone());
    }

    /// Enter on the list: that subagent's own transcript.
    pub fn open_focused_subagent(&mut self) {
        match self.subagent_focus_index() {
            Some(_) => {
                let id = self.subagent_focus.clone().unwrap_or_default();
                self.open_subagent(&id);
            }
            None => self.subagent_focus = None,
        }
    }

    /// Every subagent of the conversation, to choose one to look at.
    pub fn open_subagent_picker(&mut self) {
        let items: Vec<SubagentOption> = self
            .transcript
            .agents()
            .into_iter()
            .map(|(id, run, depth)| SubagentOption {
                id: id.to_string(),
                description: run.description.clone(),
                kind: run.kind.clone(),
                status: run.status,
                depth,
            })
            .collect();
        if items.is_empty() {
            self.transcript
                .push_notice("no subagents in this conversation");
            return;
        }
        // Start on the one in view, else the first still at work, else the latest.
        let at = items
            .iter()
            .position(|a| Some(&a.id) == self.viewing.as_ref())
            .or_else(|| items.iter().position(|a| a.status.is_none()))
            .unwrap_or(items.len() - 1);
        self.modal = Some(Modal::Subagents(
            ListPicker::new(items).with_selected(Some(at)),
        ));
    }

    /// Show the transcript of the subagent that the tool call `id` spawned
    /// in place of the main one.
    pub fn open_subagent(&mut self, id: &str) {
        if self.transcript.agent(id).is_none() {
            return;
        }
        if self.viewing.is_none() {
            self.main_scroll = (self.scroll, self.auto_scroll);
        }
        self.viewing = Some(id.to_string());
        // Back from it, the keyboard is on its row of the list, if it has one.
        self.subagent_focus = Some(id.to_string());
        self.show_from_the_end();
    }

    /// Back to the main transcript, where it was.
    pub fn close_subagent_view(&mut self) {
        if self.viewing.take().is_some() {
            self.show_from_the_end();
            (self.scroll, self.auto_scroll) = self.main_scroll;
        }
    }

    /// Another transcript is to be drawn: nothing kept from the last one applies.
    fn show_from_the_end(&mut self) {
        self.transcript_view = TranscriptView::default();
        self.selection = None;
        self.last_click = None;
        self.scroll = 0;
        self.auto_scroll = true;
    }

    /// The subagent in view, if it still exists (a rewind may have removed it).
    pub fn viewed(&self) -> Option<&super::transcript::AgentRun> {
        self.transcript.agent(self.viewing.as_deref()?)
    }

    /// The next (or previous) subagent's transcript, round and round.
    pub fn view_next_subagent(&mut self, back: bool) {
        let ids: Vec<String> = self
            .transcript
            .agents()
            .into_iter()
            .map(|(id, _, _)| id.to_string())
            .collect();
        let Some(at) = ids.iter().position(|i| Some(i) == self.viewing.as_ref()) else {
            return;
        };
        let step = if back { ids.len() - 1 } else { 1 };
        let next = ids[(at + step) % ids.len()].clone();
        self.open_subagent(&next);
    }

    /// The blocks on screen: the main transcript's, or those of the subagent in view.
    pub fn shown_blocks(&self) -> &[super::transcript::Block] {
        match self.viewed() {
            Some(run) => &run.log.blocks,
            None => &self.transcript.blocks,
        }
    }

    /// The transcript on screen, for expanding and collapsing its tool calls.
    pub fn shown_transcript_mut(&mut self) -> &mut Transcript {
        let id = self.viewing.clone().unwrap_or_default();
        if self.transcript.agent(&id).is_some() {
            return &mut self.transcript.agent_mut(&id).unwrap().log;
        }
        &mut self.transcript
    }

    /// Stop the subagent in view.
    pub fn stop_viewed_subagent(&mut self) {
        let Some(id) = self.viewing.clone() else {
            return;
        };
        if !self.subagents.iter().any(|a| a.id == id) {
            self.flash("This subagent is not running");
        } else if !self.caps().subagents.stop || !self.session_alive {
            self.flash(format!(
                "{} cannot stop a subagent from here",
                self.short_name()
            ));
        } else {
            self.flash("Stopping…");
            self.actions
                .push_back(Action::Command(SessionCommand::StopSubagent { id }));
        }
    }

    // ------------------------------------------------------------- session events

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
                self.live_caps.entry(self.active).or_default().merge(update);
                if commands {
                    self.commands_arrived();
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

    /// The session's process is gone, or going: what it was running and
    /// would have reported on will not be.
    fn process_gone(&mut self) {
        self.transcript.end_running_hooks();
        // The next process takes the messages so far as its baseline, so
        // the anchors still owed would land on later turns (pi).
        self.anchor_pending.clear();
        self.drop_prompts();
    }

    /// Requests of a session that is gone: nobody can answer them now.
    fn drop_prompts(&mut self) {
        if self.modal.as_ref().is_some_and(Modal::is_prompt) {
            self.modal = None;
        }
        self.pending_prompts.clear();
    }

    /// The session is going away, and whatever its subagents were doing.
    fn drop_subagents(&mut self) {
        self.subagents.clear();
        self.transcript.end_running_agents();
    }

    /// Whether one of unharness's own commands is offered for the active
    /// harness (in the `/` list and `/help`).
    fn offers_command(&self, name: &str) -> bool {
        !matches!(name, "/remote-control" | "/rc") || self.caps().remote_control
    }

    /// Whether `/name` is one of unharness's own commands for the active
    /// harness, which one of the harness's of that name gives way to.
    fn owns_command(&self, name: &str) -> bool {
        is_own_command(name) && self.offers_command(name)
    }

    /// `/remote-control [on [name] | off | status]`.
    pub fn remote_control_command(&mut self, cmd: &str, args: &str) {
        // A command of the harness's own by that name, if it has one.
        if !self.caps().remote_control {
            self.pass_command(cmd, false);
            return;
        }
        let (word, name) = match args.split_once(char::is_whitespace) {
            Some((w, n)) => (w, Some(n.trim().to_string()).filter(|n| !n.is_empty())),
            None => (args, None),
        };
        match word {
            "" | "on" => self.remote_control_on(name),
            "off" if name.is_none() => self.remote_control_off(),
            "status" if name.is_none() => {
                let says = match &self.remote {
                    None => "Remote Control is off".to_string(),
                    Some(RemoteSession {
                        url: Some(url),
                        state,
                        ..
                    }) => format!(
                        "Remote Control is on ({}): {url}",
                        state.as_deref().unwrap_or("starting")
                    ),
                    Some(_) => "Remote Control is starting".to_string(),
                };
                self.transcript.push_notice(says);
            }
            _ => self
                .transcript
                .push_notice("usage: /remote-control [on [name] | off | status]"),
        }
    }

    fn remote_control_on(&mut self, name: Option<String>) {
        if let Some(remote) = &self.remote {
            let says = match &remote.url {
                Some(url) => format!("Remote Control is already on: {url}"),
                None => "Remote Control is starting".to_string(),
            };
            self.transcript.push_notice(says);
            return;
        }
        self.remote = Some(RemoteSession {
            name: name.clone(),
            ..Default::default()
        });
        if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::RemoteControl {
                    enabled: true,
                    name,
                }));
            self.transcript.push_notice(format!(
                "turning on Remote Control for {}",
                self.short_name()
            ));
        } else {
            // Sent once the session is up (`remote_control_resend`).
            self.transcript.push_notice(format!(
                "Remote Control goes on once {}'s session has started",
                self.short_name()
            ));
        }
    }

    fn remote_control_off(&mut self) {
        if self.remote.take().is_some() && self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::RemoteControl {
                    enabled: false,
                    name: None,
                }));
        }
        self.transcript.push_notice("Remote Control is off");
    }

    /// Remote Control ends with the conversation's tie to this session (a
    /// switch, a fork, a resume, `/clear`, quit): taken off before the session is
    /// shut down, so that the remote side is told.
    fn end_remote_control(&mut self) {
        if self.remote.take().is_none() {
            return;
        }
        if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::RemoteControl {
                    enabled: false,
                    name: None,
                }));
        }
        self.transcript.push_notice("Remote Control ended");
    }

    /// For a session that just started: the command that puts it on Remote
    /// Control again, if the user had it on.
    pub fn remote_control_resend(&mut self) -> Option<SessionCommand> {
        let remote = self.remote.as_mut()?;
        remote.url = None;
        remote.state = None;
        Some(SessionCommand::RemoteControl {
            enabled: true,
            name: remote.name.clone(),
        })
    }

    /// A prompt typed on Remote Control, which the session runs as a turn
    /// of its own: shown, checkpointed and titled as one typed here.
    fn remote_prompt(&mut self, text: String) {
        // What this harness has not been told yet is still owed: it goes
        // with the next prompt typed here, as after a command.
        let from = self.bridge_start(self.active);
        if from < self.transcript.blocks.len() {
            self.last_active_index.insert(self.active, from);
        }
        // The session is the conversation's from here on.
        if self.session_blank {
            self.mark_prompted();
        }
        if self.first_prompt.is_none() {
            self.first_prompt = Some(text.clone());
        }
        self.transcript.push_notice("from Remote Control:");
        self.transcript.push_user(text);
        let block = self.transcript.blocks.len() - 1;
        self.checkpoint_files(block);
    }

    /// What the status bar says of Remote Control.
    pub fn remote_status(&self) -> Option<String> {
        let remote = self.remote.as_ref()?;
        Some(match (&remote.url, remote.state.as_deref()) {
            // Taken up again by the next process.
            _ if !self.session_alive => "remote: paused".to_string(),
            (None, _) => "remote: starting".to_string(),
            (Some(_), None | Some("connected")) => "remote".to_string(),
            (Some(_), Some(state)) => format!("remote: {state}"),
        })
    }

    fn on_remote_control(&mut self, event: RemoteControl) {
        match event {
            // Taken off meanwhile, which follows: nothing to open.
            RemoteControl::On { .. } | RemoteControl::State { .. } if self.remote.is_none() => {}
            RemoteControl::On { url } => {
                if let Some(remote) = self.remote.as_mut() {
                    remote.url = Some(url.clone());
                }
                self.transcript.push_system(format!(
                    "Remote Control is on: open {url} in a browser or the Claude app"
                ));
            }
            RemoteControl::State { state, .. } => {
                if let Some(remote) = self.remote.as_mut() {
                    remote.state = Some(state);
                }
            }
            // Said when it was asked for (`remote_control_off`).
            RemoteControl::Off => {}
            RemoteControl::Failed { reason } => {
                self.remote = None;
                self.transcript
                    .push_error(format!("Remote Control: {reason}"));
            }
        }
    }

    /// End the active harness's session. It is not alive from here on: a
    /// prompt handled before the loop runs the shutdown starts the next.
    fn shutdown_session(&mut self) {
        // What it changed since the last turn ended, or since it started
        // when it never had one, is told before the next process is watched.
        if self.session_alive {
            self.check_guard();
        }
        self.session_alive = false;
        self.session_blank = false;
        self.blank_session = None;
        self.policies_sent.clear();
        self.idle_start_paused = false;
        self.drop_subagents();
        self.drop_prompts();
        // The next session reports where it runs.
        self.running_providers.remove(&self.active);
        self.actions.push_back(Action::Shutdown);
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

    fn on_permission_request(&mut self, req: PermissionRequest) {
        let planning = self.asked_planning.remove(&req.id)
            || self.effective_policy() == Some(PermissionPolicy::Plan);
        if self.handoff.is_some() {
            let decision = match &req.kind {
                PermissionKind::ToolUse { tool, .. } => {
                    self.transcript
                        .push_notice(format!("denied {tool}: a handoff summary uses no tools"));
                    PermissionDecision::Deny {
                        reason: "The user is switching to another agent. Write the handoff \
                                 summary from what you already know, without tools."
                            .into(),
                    }
                }
                // Never approved unseen.
                PermissionKind::PlanApproval { .. } => PermissionDecision::Deny {
                    reason: "The user is switching to another agent. Do not carry out the \
                             plan; write the handoff summary."
                        .into(),
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

    fn answer_prompt(&mut self, decision: PermissionDecision) {
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
    fn show_next_prompt(&mut self) {
        while self.modal.is_none()
            && let Some(req) = self.pending_prompts.pop_front()
        {
            self.on_permission_request(req);
        }
    }

    /// The harness stopped waiting for request `id`: close it unanswered.
    fn withdraw_prompt(&mut self, id: &str) {
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
    fn allow_always(&mut self, scope: Scope, rules: &[Rule]) {
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

    // ------------------------------------------------------------------ settings

    pub fn switch_harness(&mut self, next: HarnessId) {
        if next == self.active {
            return;
        }
        if self.registry.get(next).is_none() {
            self.transcript
                .push_error(format!("harness '{next}' is not available"));
            return;
        }
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before switching harness");
            return;
        }
        if self.refused_while_start_held() {
            return;
        }
        if self.handoff_wanted(next) {
            self.ask_handoff(next);
            return;
        }
        self.switch_now(next);
    }

    /// Whether the conversation will not fit in the bridge to `next`, and
    /// the harness being left can say what it was about: it has a session,
    /// live or one it can resume (a fresh one would summarize nothing), and
    /// that session saw the whole transcript. Not while its subagents are
    /// at work: what they ask or report would land in the summary's turn.
    fn handoff_wanted(&self, next: HarnessId) -> bool {
        self.bridge_summary == BridgeSummary::Auto
            && (self.session_seen()
                || (self.session_ids.contains_key(&self.active) && self.caps().resume_by_id))
            && self.subagents.is_empty()
            && self.bridge_start(self.active) >= self.transcript.blocks.len()
            && self.shell.is_none()
            && self.effective_policy().is_some()
            && super::bridge::needs_summary(
                self.transcript
                    .blocks
                    .get(self.bridge_start(next)..)
                    .unwrap_or_default(),
                self.bridge_budget(next),
            )
    }

    /// Ask the active harness for a summary for `next`, switching when its
    /// turn ends (Esc switches without it). It is told to use no tools,
    /// and what it asks permission for is denied.
    fn ask_handoff(&mut self, next: HarnessId) {
        let to = self
            .registry
            .get(next)
            .map_or("another agent", |h| h.descriptor().short_name);
        self.transcript.push_system(format!(
            "The conversation is longer than the bridge to {to}: asking {} for a handoff summary first (Esc switches without one)",
            self.short_name()
        ));
        self.handoff = Some(Handoff {
            to: next,
            start: self.transcript.blocks.len(),
            held: !self.queued.is_empty(),
        });
        self.start_generation();
        self.ensure_session();
        self.mark_prompted();
        self.actions.push_back(Action::SendTurn {
            text: handoff_prompt(to, self.bridge_budget(next)),
            attachments: Vec::new(),
        });
    }

    /// The handoff turn is over, however it ended: the switch goes ahead.
    fn finish_handoff(&mut self, done: bool) {
        let Some(h) = self.handoff.take() else {
            return;
        };
        let to = self
            .registry
            .get(h.to)
            .map_or("another agent", |h| h.descriptor().short_name);
        if !(done && self.transcript.mark_handoff(h.start, to)) {
            self.transcript.push_notice(format!(
                "no handoff summary; {to} gets the conversation as it is"
            ));
        }
        self.switch_now(h.to);
    }

    fn switch_now(&mut self, next: HarnessId) {
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session();
        }
        self.idle_start_paused = false;
        // What this harness saw: everything but the `!` commands run since
        // its last prompt, which it is told about when it comes back.
        let seen = self
            .transcript
            .blocks
            .iter()
            .position(|b| matches!(b, super::transcript::Block::Shell { sent: false, .. }))
            .unwrap_or(self.transcript.blocks.len());
        // Context held back for this harness (by a command sent alone) is
        // still to be told.
        let seen = self
            .last_active_index
            .get(&self.active)
            .map_or(seen, |held| seen.min(*held));
        self.last_active_index.insert(self.active, seen);
        self.sync_conversation();
        self.active = next;
        self.session_usage = self
            .conversation
            .usage
            .get(&next)
            .cloned()
            .unwrap_or_default();
        self.turn_usage = Usage::default();
        self.context = ContextUsage::default();
        self.anchor_pending.clear();
        self.persist();
        self.transcript.push_system(format!(
            "Switched to {} (model: {}, effort: {}, policy: {}, sandbox: {})",
            self.display_name(),
            self.model_label(),
            self.current_effort().unwrap_or("default"),
            self.policy_label(),
            self.sandbox_level().0
        ));
        if let Some(w) = self.policy_warning() {
            self.transcript.push_notice(w);
        }
        self.require_policy();
    }

    /// Returns whether `p` could be set on the active harness. Only a
    /// policy it has can be: what the user picks is what runs.
    pub fn set_policy(&mut self, p: PermissionPolicy) -> bool {
        let Some(res) = self.offered(p) else {
            return false;
        };
        let before = self.effective_policy();
        if res.effective == PermissionPolicy::Plan
            && let Some(was) = before
            && was != PermissionPolicy::Plan
        {
            self.pre_plan.insert(self.active, was);
        }
        // Only a prompt that waited for this choice is sent by it.
        let was_waiting = before.is_none();
        // For this harness only: the others keep theirs.
        self.policy_choice.insert(self.active, p);
        self.run_choices.insert(self.active, p);
        self.transcript
            .push_system(format!("Permission policy: {}", res.effective));
        if let Some(w) = res.warning {
            self.transcript.push_notice(w);
        }
        self.idle_start_paused = false;
        if self.session_alive && self.session_blank {
            // Started with the policy it had: started again with this one.
            self.shutdown_session();
        } else if self.session_alive && before != Some(res.effective) {
            // Sent only as a change, so that each is reported back once.
            self.policies_sent.push_back(res.effective);
            self.actions
                .push_back(Action::Command(SessionCommand::SetPolicy(res.effective)));
        }
        if was_waiting {
            self.send_next_queued();
        }
        true
    }

    /// The session runs under `p` now. One it moved to by itself (the
    /// agent went into plan mode, or left a mode it could not keep) becomes
    /// the harness's policy, so that what is shown, saved and started again
    /// is what runs.
    fn on_policy_changed(&mut self, p: PermissionPolicy) {
        // The report of a change we sent, maybe after a later one: the
        // policy shown is already the newest choice.
        if let Some(at) = self.policies_sent.iter().position(|s| *s == p) {
            self.policies_sent.drain(..=at);
            return;
        }
        let was = self.effective_policy();
        if was == Some(p) {
            return;
        }
        if p == PermissionPolicy::Plan
            && let Some(was) = was
        {
            self.pre_plan.insert(self.active, was);
        }
        self.policy_choice.insert(self.active, p);
        self.run_choices.insert(self.active, p);
        let name = self.display_name();
        self.transcript.push_notice(match p {
            PermissionPolicy::Plan => {
                format!(
                    "{name} went into plan mode: it asks you to approve its plan before it acts"
                )
            }
            _ => format!("{name} now runs under policy {p}"),
        });
    }

    /// The policies a plan can be carried out under: those the harness has,
    /// but bypass only in a session started under it, since Claude refuses
    /// to switch to it in one that was not (`bypass_not_launched`, 2.1.296).
    fn plan_policies(&self) -> Vec<PermissionPolicy> {
        let bypass = self.session_policy == Some(PermissionPolicy::Bypass);
        self.offered_policies()
            .into_iter()
            .filter(|p| match p {
                PermissionPolicy::Plan => false,
                PermissionPolicy::Bypass => bypass,
                _ => true,
            })
            .collect()
    }

    /// Save `p` as the active harness's default policy in the settings of
    /// `scope` (under the config directory, never in the workspace), and
    /// choose it. Returns whether both were done.
    pub fn save_default_policy(&mut self, p: PermissionPolicy, scope: Scope) -> bool {
        if self.offered(p).is_none() {
            return false;
        }
        let name = self.short_name();
        let id = self.active.as_str();
        let root = self.workspace_root.clone();
        let Some(dir) = self.rules.config_dir().map(Path::to_path_buf) else {
            self.transcript
                .push_error("there is no config directory to save the default policy in");
            return false;
        };
        let Some(path) = Config::scoped_path(&dir, scope, root.as_deref()) else {
            self.transcript
                .push_error("there is no workspace to save the default policy for");
            return false;
        };
        if let Err(e) = Config::set_harness_default_policy(&path, id, p) {
            self.transcript
                .push_error(format!("could not save the default policy: {e:#}"));
            return false;
        }
        let place = match scope {
            Scope::Workspace => "for this workspace",
            Scope::Global => "for every workspace",
        };
        self.transcript.push_system(format!(
            "Saved {p} as {name}'s default policy {place} ({})",
            path.display()
        ));
        // What this run reads as the default from now on, unless the
        // workspace's own settings name another for this harness. One they
        // name for every harness gives way to it, which the user is told.
        let workspace = match scope {
            Scope::Global => Config::scoped_path(&dir, Scope::Workspace, root.as_deref())
                .and_then(|w| Config::load_file(&w).ok().flatten()),
            Scope::Workspace => None,
        };
        let for_harness = workspace
            .as_ref()
            .and_then(|c| c.harness(id)?.default_policy.clone());
        let for_all = workspace.as_ref().and_then(|c| c.default_policy.clone());
        match for_harness {
            Some(w) => self.transcript.push_notice(format!(
                "this workspace's settings make {w} {name}'s default here"
            )),
            None => {
                if let Some(w) = for_all.filter(|w| *w != p.as_str()) {
                    self.transcript.push_notice(format!(
                        "this workspace's default_policy ({w}) no longer applies to {name}: \
                         a harness's own default comes first (save for this workspace to \
                         keep {w} here)"
                    ));
                }
                self.config
                    .harnesses
                    .entry(id.to_string())
                    .or_default()
                    .default_policy = Some(p.to_string());
            }
        }
        self.set_policy(p)
    }

    /// How `p` resolves on the active harness, if it offers it; otherwise
    /// the user is told which it does offer.
    fn offered(&mut self, p: PermissionPolicy) -> Option<PolicyResolution> {
        match resolve_policy(&self.caps().permission_policies, p) {
            Ok(res) if res.effective == p => Some(res),
            _ => {
                let e = PolicyUnavailable {
                    requested: p,
                    supported: self.offered_policies(),
                };
                self.transcript.push_error(e.to_string());
                None
            }
        }
    }

    /// Choose the sandbox level for the run. Returns whether it could be
    /// set. The sandbox is applied when a process is spawned, so a live
    /// session is shut down and comes back under the new level, resumed
    /// where the harness can.
    pub fn set_sandbox(&mut self, level: SandboxLevel) -> bool {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before changing the sandbox");
            return false;
        }
        if let Err(why) = &self.sandbox.backend
            && level != SandboxLevel::Off
        {
            self.transcript
                .push_error(format!("sandbox '{level}' is unavailable: {why}"));
            return false;
        }
        self.sandbox.explicit = Some(level);
        self.idle_start_paused = false;
        let restart = self.session_alive && self.session_sandbox_level != Some(level);
        self.transcript.push_system(if restart {
            format!(
                "Sandbox: {level} ({} restarts under it)",
                self.display_name()
            )
        } else {
            format!("Sandbox: {level}")
        });
        if restart {
            self.shutdown_session();
        }
        true
    }

    /// Choose the active harness's provider. Where the provider is fixed
    /// when the process starts, a live session is shut down and comes back
    /// on the new one, resumed.
    pub fn set_provider(&mut self, provider: ProviderId) {
        let changed = self.current_provider() != Some(&provider);
        let restart = changed && self.session_alive && self.caps().provider_per_process;
        if restart && self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before changing the provider");
            return;
        }
        // They would end with the process.
        if restart && !self.subagents.is_empty() {
            self.transcript.push_error(format!(
                "{} subagent(s) still at work would be stopped; wait for them or stop them (Ctrl+S) before changing the provider",
                self.subagents.len()
            ));
            return;
        }
        if changed {
            self.running_providers.remove(&self.active);
            self.models.remove(&self.active);
            self.chosen_models.remove(&self.active);
            self.run_models.remove(&self.active);
            self.conversation.models.remove(&self.active);
        }
        self.providers.insert(self.active, provider.clone());
        self.chosen_providers.insert(self.active);
        self.idle_start_paused = false;
        self.transcript.push_system(if restart {
            format!(
                "Provider: {provider} ({} restarts on it; pick a model with Ctrl+M)",
                self.display_name()
            )
        } else {
            format!("Provider: {provider} (pick a model with Ctrl+M)")
        });
        if restart {
            self.shutdown_session();
        }
    }

    pub fn set_model(&mut self, model: String) {
        let provider = self
            .current_provider()
            .cloned()
            .unwrap_or_else(|| ProviderId::new("default"));
        let m = ModelRef::new(self.active, provider, model);
        self.transcript.push_system(format!("Model: {}", m.label()));
        self.models.insert(self.active, m.clone());
        self.chosen_models.insert(self.active);
        self.run_models.insert(self.active);
        self.idle_start_paused = false;
        if self.session_alive && self.session_blank {
            // Nothing was sent to it: it starts again with the model.
            self.shutdown_session();
        } else if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::SetModel(m)));
        }
    }

    pub fn set_effort(&mut self, effort: String) {
        let caps = self.caps();
        if caps.effort_levels.is_empty() {
            self.transcript.push_error(format!(
                "{} does not expose reasoning effort",
                self.display_name()
            ));
            return;
        }
        let effort = effort.to_lowercase();
        if !caps.supports_effort(&effort) {
            self.transcript.push_error(format!(
                "unknown effort '{}'; {} supports: {}",
                effort,
                self.short_name(),
                caps.effort_levels.join(", ")
            ));
            return;
        }
        self.efforts.insert(self.active, effort.clone());
        self.chosen_efforts.insert(self.active);
        self.run_efforts.insert(self.active);
        self.transcript.push_system(format!("Effort: {effort}"));
        self.idle_start_paused = false;
        if self.session_alive && self.session_blank {
            // Claude would take the change as a turn: it starts again with
            // the effort instead.
            self.shutdown_session();
        } else if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::SetEffort(Some(effort))));
        }
    }

    /// Replace the current state with a saved conversation.
    pub fn resume_conversation(&mut self, id_or_prefix: String) {
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before resuming");
            return;
        }
        if self.refused_while_start_held() {
            return;
        }
        let conv = match self.store.load(&id_or_prefix) {
            Ok(c) => c,
            Err(e) => {
                self.transcript.push_error(format!("could not resume: {e}"));
                return;
            }
        };
        // Another conversation, perhaps on another harness.
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session();
        }
        self.idle_start_paused = false;
        self.persist();
        let active = if self.registry.get(conv.active_harness).is_some() {
            conv.active_harness
        } else {
            self.active
        };
        self.session_ids = conv.sessions.clone();
        self.last_active_index = conv.bookmarks.clone();
        self.transcript = Transcript::from_records(&conv.blocks);
        self.session_usage = conv.usage.get(&active).cloned().unwrap_or_default();
        self.turn_usage = Usage::default();
        self.context = ContextUsage::default();
        self.plan = conv.plan.clone();
        self.anchors = conv.anchors.clone();
        self.file_checkpoints = conv.checkpoints.clone();
        self.fork_pending = conv.fork_pending.iter().copied().collect();
        self.restore_undo = None;
        self.anchor_pending.clear();
        self.first_prompt = (!conv.title.is_empty()).then(|| conv.title.clone());
        self.generation_duration = None;
        self.modal = None;
        self.pending_prompts.clear();
        self.active = active;
        self.conversation = conv;
        self.pre_plan.clear();
        self.take_conversation_choices();
        self.announce_resume(false);
        if let Some(w) = self.policy_warning() {
            self.transcript.push_notice(w);
        }
        self.require_policy();
        self.auto_scroll = true;
    }

    pub fn quit(&mut self) {
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session();
        }
        // The event loop kills it on the way out.
        if self.shell.take().is_some() {
            self.transcript.shell_ended(ShellStatus::Killed);
        }
        self.persist();
        self.should_quit = true;
    }

    // ------------------------------------------------------------------- pickers

    pub fn open_harness_picker(&mut self) {
        let idx = self
            .harness_options
            .iter()
            .position(|o| o.id == self.active);
        self.modal = Some(Modal::Harness(
            ListPicker::new(self.harness_options.clone())
                .with_disabled(|o| (!o.installed).then(|| "not found on PATH".to_string()))
                .with_selected(idx),
        ));
    }

    pub fn open_provider_picker(&mut self) {
        if !self.caps().multi_provider {
            self.transcript.push_notice(format!(
                "{} has a single provider ({}); use /harness to change agents or /model to change models",
                self.short_name(),
                self.current_provider()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "default".into())
            ));
            return;
        }
        match self.provider_cache.get(&self.active).cloned() {
            Some(providers) => self.show_provider_picker(Ok(providers)),
            None => self.wait_for_list(ListRequest::Providers(self.active)),
        }
    }

    fn show_provider_picker(&mut self, providers: Result<Vec<(ProviderId, String)>, String>) {
        let providers = match providers {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => {
                self.transcript
                    .push_notice("this harness reports no providers");
                return;
            }
            Err(e) => {
                self.transcript.push_error(format!("list providers: {e}"));
                return;
            }
        };
        let items: Vec<ProviderOption> = providers
            .into_iter()
            .map(|(id, name)| ProviderOption { id: id.0, name })
            .collect();
        let idx = self
            .current_provider()
            .and_then(|p| items.iter().position(|i| i.id == p.0));
        self.modal = Some(Modal::Provider(ListPicker::new(items).with_selected(idx)));
    }

    /// The models of `provider` on the active harness if they are known;
    /// otherwise they are asked for, once: after a failure only a picker
    /// the user opens asks again.
    fn cached_models(&mut self, provider: &ProviderId) -> Option<Vec<ModelInfo>> {
        let request = ListRequest::Models(self.active, provider.clone());
        let cached = self
            .model_cache
            .get(&(self.active, provider.0.clone()))
            .cloned();
        if cached.is_none() && !self.lists_failed.contains(&request) {
            self.request_list(request);
        }
        cached
    }

    /// Ask for `request` (once at a time) and open its picker when it comes.
    fn wait_for_list(&mut self, request: ListRequest) {
        let what = match &request {
            ListRequest::Providers(_) => "providers".to_string(),
            ListRequest::Models(_, p) => format!("models on {p}"),
            ListRequest::Commands(_) => "commands".to_string(),
        };
        self.lists_failed.remove(&request);
        self.open_when_listed = Some(request.clone());
        if self.request_list(request) {
            self.transcript
                .push_notice(format!("Asking {} for its {what}…", self.short_name()));
        }
    }

    /// Queue `request` for a thread unless it is already under way.
    /// Returns whether it was queued now.
    fn request_list(&mut self, request: ListRequest) -> bool {
        if self.lists_pending.contains(&request) {
            return false;
        }
        let ready = self
            .harness_binary()
            .ok_or_else(|| format!("{} binary not found", self.short_name()))
            .and_then(|binary| {
                let sandbox = self.session_sandbox().map_err(|e| format!("{e:#}"))?;
                Ok((binary, sandbox))
            });
        match ready {
            Ok((binary, sandbox)) => {
                self.lists_pending.insert(request.clone());
                self.list_jobs.push(ListJob {
                    request,
                    registry: self.registry.clone(),
                    binary,
                    cwd: self.cwd.clone(),
                    sandbox,
                });
                true
            }
            Err(why) => {
                self.list_failed(request, why);
                false
            }
        }
    }

    /// A list could not be had: noted, so that completion does not ask
    /// again by itself, and shown if the user is waiting for it.
    fn list_failed(&mut self, request: ListRequest, why: String) {
        self.lists_failed.insert(request.clone());
        // Asked for by itself, and nothing else would say why the list
        // has none of the harness's commands.
        if let ListRequest::Commands(harness) = request {
            let name = self
                .registry
                .get(harness)
                .map_or("the harness", |h| h.descriptor().short_name);
            self.transcript
                .push_notice(format!("{name}'s commands could not be listed: {why}"));
            return;
        }
        if self.open_when_listed.as_ref() == Some(&request) {
            self.open_when_listed = None;
            self.transcript.push_error(match request {
                ListRequest::Providers(_) => format!("list providers: {why}"),
                ListRequest::Models(..) => format!("list models: {why}"),
                ListRequest::Commands(_) => format!("list commands: {why}"),
            });
        }
    }

    /// Lists to fetch on threads of their own.
    pub fn take_list_jobs(&mut self) -> Vec<ListJob> {
        std::mem::take(&mut self.list_jobs)
    }

    /// A list has come back. It is kept unless it failed, and its picker
    /// opens if the user is still waiting for it: on the same harness and
    /// provider, with no other picker open and nothing being typed.
    pub fn on_list(&mut self, request: ListRequest, result: ListResult) {
        self.lists_pending.remove(&request);
        match result {
            ListResult::Providers(Err(why))
            | ListResult::Models(Err(why))
            | ListResult::Commands(Err(why)) => {
                self.list_failed(request, why);
            }
            // It cannot be told this way: not asked again.
            ListResult::Commands(Ok(None)) => {
                self.lists_failed.insert(request);
            }
            ListResult::Commands(Ok(Some(commands))) => {
                let ListRequest::Commands(harness) = request else {
                    return;
                };
                self.live_caps
                    .entry(harness)
                    .or_default()
                    .merge(CapsUpdate {
                        slash_commands: Some(!commands.is_empty()),
                        commands: Some(commands),
                        ..Default::default()
                    });
                if harness == self.active {
                    self.commands_arrived();
                }
            }
            ListResult::Providers(Ok(providers)) => {
                self.lists_failed.remove(&request);
                let ListRequest::Providers(harness) = request else {
                    return;
                };
                self.provider_cache.insert(harness, providers.clone());
                if self.waiting_for(&request) {
                    self.show_provider_picker(Ok(providers));
                }
            }
            ListResult::Models(Ok(models)) => {
                self.lists_failed.remove(&request);
                let ListRequest::Models(harness, provider) = &request else {
                    return;
                };
                self.model_cache
                    .insert((*harness, provider.0.clone()), models.clone());
                if self.completing_file.is_none() && self.input.starts_with("/model ") {
                    self.update_suggestions();
                }
                if self.waiting_for(&request) {
                    let provider = provider.clone();
                    self.show_model_picker(&provider, Ok(models));
                }
            }
        }
    }

    /// Whether the picker for `request`, which has just arrived, should
    /// open now. Either way the user is no longer waiting for it.
    fn waiting_for(&mut self, request: &ListRequest) -> bool {
        if self.open_when_listed.as_ref() != Some(request) {
            return false;
        }
        self.open_when_listed = None;
        let still_there = match request {
            ListRequest::Providers(h) => *h == self.active,
            ListRequest::Models(h, p) => *h == self.active && self.current_provider() == Some(p),
            ListRequest::Commands(_) => false,
        };
        if !still_there || self.modal.is_some() {
            return false;
        }
        if !self.input.is_empty() {
            self.transcript.push_notice(match request {
                ListRequest::Providers(_) => "The providers are here: /provider".to_string(),
                ListRequest::Models(_, p) => format!("The models on {p} are here: Ctrl+M"),
                ListRequest::Commands(_) => return false,
            });
            return false;
        }
        true
    }

    pub fn open_model_picker(&mut self) {
        let Some(provider) = self.current_provider().cloned() else {
            if self.caps().multi_provider {
                self.open_provider_picker();
            } else {
                self.transcript
                    .push_notice("no provider configured for this harness");
            }
            return;
        };
        match self
            .model_cache
            .get(&(self.active, provider.0.clone()))
            .cloned()
        {
            Some(models) => self.show_model_picker(&provider, Ok(models)),
            None => self.wait_for_list(ListRequest::Models(self.active, provider)),
        }
    }

    fn show_model_picker(&mut self, provider: &ProviderId, models: Result<Vec<ModelInfo>, String>) {
        match models {
            Ok(models) if !models.is_empty() => {
                let idx = self
                    .current_model()
                    .and_then(|m| models.iter().position(|i| i.model_ref.model == m.model));
                self.modal = Some(Modal::Model(ListPicker::new(models).with_selected(idx)));
            }
            Ok(_) => self.transcript.push_notice(format!(
                "no models reported for {provider}; use /model <name> to set one directly"
            )),
            Err(e) => self.transcript.push_error(format!("list models: {e}")),
        }
    }

    pub fn open_effort_picker(&mut self) {
        let levels = self.caps().effort_levels;
        if levels.is_empty() {
            self.transcript.push_error(format!(
                "{} does not expose reasoning effort",
                self.display_name()
            ));
            return;
        }
        let idx = self
            .current_effort()
            .and_then(|e| levels.iter().position(|l| l == e));
        self.modal = Some(Modal::Effort(ListPicker::new(levels).with_selected(idx)));
    }

    /// Whether `id` was found when the harnesses were probed. One that was
    /// not probed is taken as there.
    fn harness_installed(&self, id: HarnessId) -> bool {
        self.harness_options
            .iter()
            .find(|o| o.id == id)
            .is_none_or(|o| o.installed)
    }

    /// The policies the active harness has, least permissive first.
    pub fn offered_policies(&self) -> Vec<PermissionPolicy> {
        let caps = self.caps();
        PermissionPolicy::ALL
            .into_iter()
            .filter(|p| caps.supports_policy(*p).is_some())
            .collect()
    }

    /// Every policy is listed; one the harness does not have cannot be
    /// chosen. The cursor starts on the wanted policy, else on the one in
    /// effect, else on the least permissive one there is.
    pub fn open_policy_picker(&mut self) {
        let offered = self.offered_policies();
        let name = self.display_name();
        let wanted = self.wanted_policy();
        let start = if offered.contains(&wanted) {
            Some(wanted)
        } else {
            self.effective_policy()
        };
        let idx = start.and_then(|s| PermissionPolicy::ALL.iter().position(|p| *p == s));
        self.modal = Some(Modal::Policy(PolicyPicker {
            list: ListPicker::new(PermissionPolicy::ALL.to_vec())
                .with_disabled(|p| (!offered.contains(p)).then(|| format!("not offered by {name}")))
                .with_selected(idx),
            save: None,
            has_workspace: self.rules.has_workspace(),
        }));
    }

    /// The sandbox levels there is a backend for.
    fn available_sandbox_levels(&self) -> Vec<SandboxLevel> {
        SandboxLevel::ALL
            .into_iter()
            .filter(|l| self.sandbox.backend.is_ok() || *l == SandboxLevel::Off)
            .collect()
    }

    pub fn open_sandbox_picker(&mut self) {
        let wanted = self.sandbox_level().0;
        let idx = SandboxLevel::ALL.iter().position(|l| *l == wanted);
        let unavailable = self.sandbox.backend.is_err();
        self.modal = Some(Modal::Sandbox(
            ListPicker::new(SandboxLevel::ALL.to_vec())
                // Why is said once, above the list.
                .with_disabled(|l| {
                    (unavailable && *l != SandboxLevel::Off).then(|| "no sandbox here".to_string())
                })
                .with_selected(idx),
        ));
    }

    pub fn open_resume_picker(&mut self) {
        let rows = self.store.list();
        if rows.is_empty() {
            self.transcript
                .push_notice("no saved conversations in this workspace");
            return;
        }
        let idx = rows.iter().position(|r| r.id == self.conversation.id);
        self.modal = Some(Modal::Resume(ListPicker::new(rows).with_selected(idx)));
    }

    pub fn close_modal(&mut self) {
        if let Some(decision) = self.modal.as_ref().and_then(Modal::dismiss_decision) {
            self.answer_prompt(decision);
        } else {
            self.modal = None;
            self.show_next_prompt();
        }
    }

    // ------------------------------------------------------------------ modal keys

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
                                PermissionKind::ToolUse { tool, action, .. } => {
                                    self.rules.would_allow(&rules, tool, action, &self.cwd)
                                }
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
                                    self.rules.propose(tool, action, &self.cwd),
                                    self.rules.has_workspace(),
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

    // --------------------------------------------------------------- slash commands

    pub fn handle_slash_command(&mut self, cmd: &str) {
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or("");
        let arg = parts.next().map(str::to_string);
        // Everything after the command, for commands that take free text
        // (a path or a message may contain spaces).
        let rest = cmd.trim_start().strip_prefix(name).unwrap_or("").trim();
        match name {
            "/switch" | "/harness" => match arg {
                Some(a) => match self.registry.parse(&a).map(|h| h.descriptor().id) {
                    // Refused as the picker greys it out.
                    Some(id) if !self.harness_installed(id) => self
                        .transcript
                        .push_error(format!("{id} is not installed (not found on PATH)")),
                    Some(id) => self.switch_harness(id),
                    None => self.transcript.push_error(format!("unknown harness '{a}'")),
                },
                None => self.open_harness_picker(),
            },
            "/provider" => match arg {
                Some(a) => self.set_provider(ProviderId::new(a)),
                None => self.open_provider_picker(),
            },
            "/model" => match arg {
                Some(a) => self.set_model(a),
                None => self.open_model_picker(),
            },
            "/effort" | "/think" => match arg {
                Some(a) => self.set_effort(a),
                None => self.open_effort_picker(),
            },
            "/policy" => match arg {
                Some(a) => match PermissionPolicy::parse(&a) {
                    Some(p) => {
                        self.set_policy(p);
                    }
                    None => {
                        let offered: Vec<&str> =
                            self.offered_policies().iter().map(|p| p.as_str()).collect();
                        self.transcript
                            .push_error(format!("unknown policy '{a}' ({})", offered.join(", ")));
                    }
                },
                None => self.open_policy_picker(),
            },
            "/sandbox" => match arg {
                Some(a) => match SandboxLevel::parse(&a) {
                    Some(l) => {
                        self.set_sandbox(l);
                    }
                    None => {
                        let levels: Vec<&str> = self
                            .available_sandbox_levels()
                            .iter()
                            .map(|l| l.as_str())
                            .collect();
                        self.transcript.push_error(format!(
                            "unknown sandbox level '{a}' ({})",
                            levels.join(", ")
                        ));
                    }
                },
                None => self.open_sandbox_picker(),
            },
            "/resume" => match arg {
                Some(a) => self.resume_conversation(a),
                None => self.open_resume_picker(),
            },
            "/conversations" | "/sessions" => {
                let rows = self.store.list();
                if rows.is_empty() {
                    self.transcript.push_notice("no saved conversations");
                } else {
                    let lines: Vec<String> = rows
                        .iter()
                        .map(|r| {
                            format!(
                                "• {}  {}  [{}]  {}",
                                &r.id[..8.min(r.id.len())],
                                r.updated_at,
                                r.harnesses
                                    .iter()
                                    .map(|h| h.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                r.title
                            )
                        })
                        .collect();
                    self.transcript.push_system(format!(
                        "Saved conversations ({}):\n{}",
                        rows.len(),
                        lines.join("\n")
                    ));
                }
            }
            "/usage" => {
                let t = &self.turn_usage;
                let s = &self.session_usage;
                self.transcript.push_system(format!(
                    "Last turn: {}↑ {}↓ (cache read {}, write {}){}\nSession: {} tokens{}",
                    t.input,
                    t.output,
                    t.cache_read,
                    t.cache_write,
                    t.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default(),
                    s.total_tokens(),
                    s.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default(),
                ));
                if let (Some(used), Some(window)) = (self.context.used, self.context.window) {
                    self.transcript.push_system(format!(
                        "Context: {used} of {window} tokens ({}%)",
                        self.context.percent().unwrap_or(0)
                    ));
                }
                if let Some(r) = &self.rate_limit {
                    self.transcript
                        .push_system(format!("Rate limits: {}", rate_limit_summary(r)));
                }
            }
            "/steer" => match rest {
                "" => self.transcript.push_notice("usage: /steer <message>"),
                text => self.steer(text.to_string()),
            },
            "/rewind" => self.open_rewind_picker(),
            "/undo-restore" => self.undo_restore(),
            "/fork" => self.fork_conversation(),
            "/compact" => self.compact((!rest.is_empty()).then(|| rest.to_string())),
            "/remote-control" | "/rc" => self.remote_control_command(cmd, rest),
            "/attach" => match rest {
                path if !path.is_empty() => self.attach(path),
                _ if self.attachments.is_empty() => {
                    self.transcript.push_notice("usage: /attach <path>")
                }
                _ => {
                    let names: Vec<String> = self.attachments.iter().map(|a| a.label()).collect();
                    self.transcript
                        .push_system(format!("Attached: {}", names.join(", ")));
                }
            },
            "/paste" => self.request_clipboard(),
            "/detach" => {
                self.attachments.clear();
                self.transcript.push_system("Attachments cleared.");
            }
            "/subagents" => self.open_subagent_picker(),
            "/plan" => {
                self.show_plan = !self.show_plan;
                if self.plan.is_empty() {
                    self.transcript
                        .push_notice("the agent has not shared a plan");
                } else if !self.show_plan {
                    self.transcript
                        .push_system("Plan hidden (/plan shows it again).");
                }
            }
            "/allow" => {
                let list: Vec<String> = self
                    .rules
                    .iter()
                    .map(|(scope, rule)| {
                        let scope = match scope {
                            Scope::Workspace => "workspace",
                            Scope::Global => "global",
                        };
                        format!("• [{scope}] {}", rule.describe())
                    })
                    .collect();
                let files: Vec<String> = [Scope::Workspace, Scope::Global]
                    .into_iter()
                    .filter_map(|scope| self.rules.path(scope))
                    .map(|path| format!("  {}", path.display()))
                    .collect();
                let files = format!("Kept in (edit to change or remove):\n{}", files.join("\n"));
                if list.is_empty() {
                    self.transcript.push_system(format!(
                        "Nothing is allowed without asking yet. Answering a \
                         permission request with `a` adds a rule.\n{files}"
                    ));
                } else {
                    self.transcript.push_system(format!(
                        "Allowed without asking ({}):\n{}\n{files}",
                        list.len(),
                        list.join("\n")
                    ));
                }
            }
            "/skills" => {
                let mut list = Vec::new();
                if let Some(root) = &self.workspace_root {
                    for s in crate::skills::discover_skills_in_dir(
                        &crate::skills::workspace_skills_dir(root),
                    ) {
                        list.push(format!("• [workspace] {}", s.name));
                    }
                }
                if let Some(dir) = crate::skills::global_skills_dir() {
                    for s in crate::skills::discover_skills_in_dir(&dir) {
                        list.push(format!("• [global] {}", s.name));
                    }
                }
                if list.is_empty() {
                    self.transcript
                        .push_notice("no skills in .agents/skills (try `unharness skills add`)");
                } else {
                    self.transcript.push_system(format!(
                        "Skills ({}):\n{}",
                        list.len(),
                        list.join("\n")
                    ));
                }
            }
            "/clear" => self.new_conversation(),
            "/help" => {
                let mut help = String::from("Commands:\n");
                for (c, d) in BASE_COMMANDS.iter().filter(|(c, _)| self.offers_command(c)) {
                    help.push_str(&format!("  {c:<14} {d}\n"));
                }
                help.push_str(
                    "Shortcuts: Ctrl+H harness · Ctrl+M model · Ctrl+E effort · Ctrl+P policy · Ctrl+R resume · Ctrl+O expand the last tool call (click any call to expand that one, Ctrl+T for all) · Esc/Ctrl+C interrupt or quit\nSubagents: listed above the prompt while they work, also after their turn has ended · listed under the prompt while they work · what each one does is in a transcript of its own · Down from the prompt goes into the list, Enter opens the one chosen, Delete takes a finished one off the list (so does Ctrl+S, /subagents, or a click on the call that spawned it) · there: s stops it, Tab goes to the next, Esc comes back · a prompt sent meanwhile goes straight to the agent\nPrompt: Ctrl+J newline (Shift+Enter too where the terminal can tell it from Enter) · Up/Down move between lines, then through earlier prompts · Home/End (Ctrl+A) line start/end · Ctrl+U clear · Ctrl+G edit in $EDITOR · Ctrl+V attach the clipboard's image (/paste)\nAgent commands: a /command unharness does not have goes to the agent where it runs commands of its own (Claude Code, pi, ACP agents), and the / list offers the ones it reports · \\/command sends one unharness also has (\\/clear, \\/model) or sends a prompt that starts with / to any agent\nShell: !command runs it yourself, in the session's directory and sandbox, with no input; its output is shown here and goes to the agent in front of your next prompt · Esc stops it · not during a turn · \\!text sends a prompt that starts with !\nTranscript: PageUp/PageDown, Shift+Up/Down, the mouse wheel or the scrollbar scroll · click \"Jump to bottom\" or press End (empty prompt) to go back to the end · drag to select and copy the text (double click a word, triple a line)\nDuring a turn: Enter queues the prompt · Alt+Enter steers the running turn · Alt+Up edits the last queued prompt",
                );
                self.transcript.push_system(help);
            }
            "/quit" | "/exit" => self.quit(),
            _ => self.pass_command(cmd, false),
        }
    }

    /// A `/command` unharness does not have, or one sent past unharness's
    /// own with `\/` (`forced`): the active harness runs it, where it runs
    /// commands of its own from a prompt.
    pub fn pass_command(&mut self, text: &str, forced: bool) {
        let text = text.trim();
        let name = text.split_whitespace().next().unwrap_or(text);
        let harness = self.short_name();
        if !self.caps().slash_commands {
            if forced {
                self.queue_prompt(text.to_string());
            } else {
                self.transcript.push_error(format!(
                    "unknown command '{name}'; /help lists commands ({harness} has not \
                     said it runs commands from a prompt; \\{name} sends it as text)"
                ));
            }
            return;
        }
        if name == "/" {
            self.transcript
                .push_error("unknown command '/'; /help lists commands");
            return;
        }
        let listed = self
            .harness_commands()
            .iter()
            .find(|c| c.answers_to(name))
            .map(|c| format!("/{}", c.name));
        self.transcript.push_notice(match &listed {
            Some(_) => format!("{name} is {harness}'s own command: passed on"),
            None => format!("'{name}' is not an unharness command: passed to {harness} as typed"),
        });
        // One that does what one of unharness's does changes the session
        // behind unharness's back.
        if self.owns_command(name) || listed.as_deref().is_some_and(|l| self.owns_command(l)) {
            let clears = name == "/clear" || listed.as_deref() == Some("/clear");
            self.transcript.push_notice(format!(
                "unharness does not see what {harness}'s {name} changes: the model, \
                 transcript and context shown may no longer match its session{}",
                if clears {
                    " (unharness's /clear starts a new conversation)"
                } else {
                    ""
                }
            ));
        }
        self.queue_prompt(text.to_string());
    }

    // ------------------------------------------------------------------ suggestions

    pub fn update_suggestions(&mut self) {
        self.refresh_suggestions(true);
    }

    /// `files` is whether an `@` word may open the list of files: one that
    /// was typed does, one that came in a paste or from the editor does not.
    fn refresh_suggestions(&mut self, files: bool) {
        self.suggestions.clear();
        self.suggestions_closed = false;
        let was_completing_file = self.completing_file.take().is_some();
        // `\/name` is the harness's command even where unharness has one.
        let escaped = self.input.starts_with("\\/");
        if !(self.input.starts_with('/') || escaped) || self.input.contains('\n') {
            self.selected_suggestion = 0;
            if files {
                self.suggest_files(was_completing_file);
            }
            return;
        }
        let query = self.input.to_lowercase();
        let query = query.strip_prefix('\\').unwrap_or(&query);
        let (cmd, sub) = match query.split_once(' ') {
            Some((c, s)) => (c.to_string(), Some(s.trim().to_string())),
            None => (query.to_string(), None),
        };
        let mut out = Vec::new();
        if escaped {
            if sub.is_none() {
                out = self.suggest_harness_commands(&cmd, true);
            }
        } else {
            match (cmd.as_str(), sub.as_deref()) {
                ("/switch" | "/harness", Some(s)) => {
                    for o in self.harness_options.iter().filter(|o| o.installed) {
                        let id = o.id.as_str();
                        if id.starts_with(s) {
                            out.push((format!("{cmd} {id}"), o.display_name.to_string()));
                        }
                    }
                }
                ("/policy", Some(s)) => {
                    for p in self.offered_policies() {
                        if p.as_str().starts_with(s) {
                            out.push((format!("/policy {p}"), p.description().to_string()));
                        }
                    }
                }
                ("/sandbox", Some(s)) => {
                    for l in self.available_sandbox_levels() {
                        if l.as_str().starts_with(s) {
                            out.push((format!("/sandbox {l}"), l.description().to_string()));
                        }
                    }
                }
                ("/effort" | "/think", Some(s)) => {
                    for l in self.caps().effort_levels {
                        if l.starts_with(s) {
                            out.push((format!("{cmd} {l}"), "Reasoning effort".to_string()));
                        }
                    }
                }
                ("/model", Some(s)) => {
                    if let Some(p) = self.current_provider().cloned()
                        && let Some(models) = self.cached_models(&p)
                    {
                        for m in models {
                            let id = m.model_ref.model.to_lowercase();
                            if id.contains(s) || m.display_name.to_lowercase().contains(s) {
                                out.push((
                                    format!("/model {}", m.model_ref.model),
                                    m.description.unwrap_or(m.display_name),
                                ));
                            }
                        }
                    }
                }
                ("/resume", Some(s)) => {
                    for r in self.store.list() {
                        if r.id.starts_with(s) || r.title.to_lowercase().contains(s) {
                            out.push((
                                format!("/resume {}", &r.id[..8.min(r.id.len())]),
                                format!("{}  {}", r.updated_at, r.title),
                            ));
                        }
                    }
                }
                (_, None) => {
                    for (c, d) in BASE_COMMANDS {
                        if c.starts_with(cmd.as_str()) && self.offers_command(c) {
                            out.push((c.to_string(), d.to_string()));
                        }
                    }
                    out.extend(self.suggest_harness_commands(&cmd, false));
                }
                _ => {}
            }
        }
        self.suggestions = out;
        if self.selected_suggestion >= self.suggestions.len() {
            self.selected_suggestion = 0;
        }
    }

    /// The active harness's commands that start with `typed` (`/` and
    /// lowercase), where a prompt runs them. Unless `escaped`, those that
    /// unharness has a command of the same name for are left out.
    fn suggest_harness_commands(&self, typed: &str, escaped: bool) -> Vec<(String, String)> {
        if !self.caps().slash_commands {
            return Vec::new();
        }
        let harness = self.short_name();
        self.harness_commands()
            .iter()
            .map(|c| (format!("/{}", c.name), c))
            .filter(|(name, _)| name.to_lowercase().starts_with(typed))
            .filter(|(name, _)| escaped || !self.owns_command(name))
            .map(|(name, c)| {
                let shown = if escaped { format!("\\{name}") } else { name };
                (shown, describe_command(harness, c))
            })
            .collect()
    }

    /// The files matching the `@` word the cursor ends, if it ends one.
    fn suggest_files(&mut self, already_open: bool) {
        let Some((start, query)) = files::token_at(&self.input, self.cursor) else {
            return;
        };
        self.completing_file = Some(start);
        // Each `@` lists the files anew: agents create and delete them.
        if !already_open {
            self.file_index_requested = true;
        }
        self.suggestions = files::rank(&query, &self.file_index, files::SHOWN)
            .into_iter()
            .map(|path| (path, String::new()))
            .collect();
    }

    /// The directory whose files are to be listed, when a list was asked
    /// for. One asked for while files are being listed waits for that to
    /// end: its list would be older than the `@`.
    pub fn take_file_index_request(&mut self) -> Option<PathBuf> {
        if self.file_walk_pending || !std::mem::take(&mut self.file_index_requested) {
            return None;
        }
        self.file_walk_pending = true;
        Some(self.cwd.clone())
    }

    /// The files under `cwd`, from `files::walk`.
    pub fn set_file_index(&mut self, paths: Vec<String>) {
        self.file_index = paths;
        self.file_walk_pending = false;
        if self.completing_file.is_none() {
            return;
        }
        // An open list takes the new files in, without moving the selection
        // off the file it is on.
        self.refresh_keeping_selection();
    }

    fn refresh_keeping_selection(&mut self) {
        let selected = self.suggestions.get(self.selected_suggestion).cloned();
        self.update_suggestions();
        self.selected_suggestion = selected
            .and_then(|s| self.suggestions.iter().position(|o| *o == s))
            .unwrap_or(0);
    }

    /// The session listed its commands: a `/` being typed has them now
    /// (the session may have started after the `/`).
    fn commands_arrived(&mut self) {
        let typing_command = (self.input.starts_with('/') || self.input.starts_with("\\/"))
            && !self.input.contains('\n');
        if typing_command && self.completing_file.is_none() && !self.suggestions_closed {
            self.refresh_keeping_selection();
        }
    }

    /// Listing the files came to nothing: the last list stays.
    pub fn file_walk_failed(&mut self) {
        self.file_walk_pending = false;
    }

    /// The char range of the `@` word starting at `start`: to the end of
    /// the word the cursor is in.
    fn file_word(&self, start: usize) -> (usize, usize) {
        let cursor = self.cursor.max(start);
        let rest = self.input.chars().skip(cursor);
        (
            start,
            cursor + rest.take_while(|c| !c.is_whitespace()).count(),
        )
    }

    pub fn close_suggestions(&mut self) {
        self.suggestions.clear();
        self.completing_file = None;
        self.suggestions_closed = true;
    }

    /// A list of files belongs to the word the cursor ends: it goes when
    /// the cursor leaves, or when text arrives that was not typed.
    fn close_file_suggestions(&mut self) {
        if self.completing_file.is_some() {
            self.close_suggestions();
        }
    }

    /// The selected file as the active harness wants it written.
    fn selected_file_reference(&self) -> Option<String> {
        let (path, _) = self.suggestions.get(self.selected_suggestion)?;
        Some(self.harness().file_reference(path))
    }

    pub fn suggestion_up(&mut self) {
        if !self.suggestions.is_empty() {
            self.selected_suggestion = if self.selected_suggestion == 0 {
                self.suggestions.len() - 1
            } else {
                self.selected_suggestion - 1
            };
        }
    }

    pub fn suggestion_down(&mut self) {
        if !self.suggestions.is_empty() {
            self.selected_suggestion = (self.selected_suggestion + 1) % self.suggestions.len();
        }
    }

    /// Enter should complete the highlighted suggestion when the typed text
    /// is only a prefix of a command (e.g. `/pro` → `/provider`).
    ///
    /// With a list of files it should unless the word typed already is the
    /// selected file's reference: completing would change nothing.
    pub fn should_accept_suggestion(&self) -> bool {
        if let Some(start) = self.completing_file {
            let (from, to) = self.file_word(start);
            let typed: String = self.input.chars().skip(from).take(to - from).collect();
            return self
                .selected_file_reference()
                .is_some_and(|reference| reference != typed);
        }
        if !(self.input.starts_with('/') || self.input.starts_with("\\/"))
            || self.suggestions.is_empty()
        {
            return false;
        }
        let Some((cmd, _)) = self.suggestions.get(self.selected_suggestion) else {
            return false;
        };
        // The list matches regardless of case.
        let typed = self.input.trim();
        typed != cmd && cmd.to_lowercase().starts_with(&typed.to_lowercase())
    }

    pub fn accept_suggestion(&mut self) {
        if let Some(start) = self.completing_file {
            let Some(mut text) = self.selected_file_reference() else {
                return;
            };
            // The whole word goes, what is after the cursor included.
            let (from, to) = self.file_word(start);
            let (from, to) = (self.byte_index(from), self.byte_index(to));
            if to == self.input.len() {
                text.push(' ');
            }
            self.input.replace_range(from..to, &text);
            self.cursor = start + text.chars().count();
            self.close_suggestions();
            return;
        }
        if let Some((cmd, _)) = self.suggestions.get(self.selected_suggestion) {
            self.input = cmd.clone();
            self.cursor = self.input.chars().count();
            self.update_suggestions();
        }
    }

    // ----------------------------------------------------------------- prompt box

    fn byte_index(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    pub fn insert_char(&mut self, c: char) {
        let idx = self.byte_index(self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
        self.update_suggestions();
    }

    pub fn delete_backwards(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let idx = self.byte_index(self.cursor);
            self.input.remove(idx);
            self.update_suggestions();
        }
    }

    pub fn delete_forwards(&mut self) {
        if self.cursor < self.input.chars().count() {
            let idx = self.byte_index(self.cursor);
            self.input.remove(idx);
            self.update_suggestions();
        }
    }

    pub fn move_cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.close_file_suggestions();
    }

    pub fn move_cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
        self.close_file_suggestions();
    }

    /// The input's visual rows at the current prompt width.
    pub fn prompt_rows(&self) -> Vec<prompt::Row> {
        prompt::rows(&self.input, self.prompt_width)
    }

    /// Insert text at the cursor, newlines included (a paste).
    pub fn insert_str(&mut self, text: &str) {
        let text = prompt::clean(text);
        let idx = self.byte_index(self.cursor);
        self.input.insert_str(idx, &text);
        self.cursor += text.chars().count();
        self.refresh_suggestions(false);
    }

    /// Show the next older sent prompt, keeping what was typed as a draft.
    pub fn history_older(&mut self) {
        if let Some(text) = self.history.older(&self.input) {
            self.input = text.to_string();
            self.show_recalled();
        }
    }

    /// Show the next newer sent prompt, or the draft after the newest.
    /// False when no earlier prompt was on show: there is nothing newer.
    pub fn history_newer(&mut self) -> bool {
        match self.history.newer() {
            Some(text) => {
                self.input = text;
                self.show_recalled();
                true
            }
            None => false,
        }
    }

    fn show_recalled(&mut self) {
        self.cursor = self.input.chars().count();
        // No suggestion list for a recalled command: it would take over
        // the arrows that are stepping through history.
        self.close_suggestions();
    }

    /// A paste goes in whole, into whichever text field has the keyboard;
    /// one that is the paths of dropped files attaches them instead.
    /// It never acts as keystrokes: a modal without a text field ignores
    /// it rather than treat its letters as answers.
    pub fn paste(&mut self, text: &str) {
        let Some(modal) = self.modal.as_mut() else {
            match drop::files(text) {
                Some(paths) => self.attach_dropped(text, paths),
                None => self.insert_str(text),
            }
            return;
        };
        if let Some((field, multiline)) = modal.text_field() {
            let text = prompt::clean(text);
            if multiline {
                field.push_str(&text);
            } else {
                field.push_str(&text.replace('\n', " "));
            }
        }
    }

    pub fn request_clipboard(&mut self) {
        self.clipboard_requested = true;
    }

    pub fn take_clipboard_request(&mut self) -> bool {
        std::mem::take(&mut self.clipboard_requested)
    }

    /// Attach what was read off the clipboard: an image, saved into `dir`
    /// first, or the files copied to it.
    pub fn attach_pasted(&mut self, pasted: anyhow::Result<Pasted>, dir: &Path) {
        match pasted {
            Ok(Pasted::Image { extension, bytes }) => {
                if !self.caps().image_input {
                    let why = format!(
                        "{} does not accept images with the current model",
                        self.short_name()
                    );
                    return self.transcript.push_error(why);
                }
                match clipboard::save_image(dir, extension, &bytes) {
                    Ok(path) => match self.attachable(&path) {
                        Ok(a) => self.push_attachment(a),
                        Err(why) => self.transcript.push_error(why),
                    },
                    Err(e) => self.transcript.push_error(format!("not pasted: {e:#}")),
                }
            }
            Ok(Pasted::Files(paths)) => {
                for path in paths {
                    match self.attachable(&path) {
                        Ok(a) => self.push_attachment(a),
                        Err(why) => self
                            .transcript
                            .push_error(format!("{} not attached: {why}", path.display())),
                    }
                }
            }
            Err(e) => self.transcript.push_error(format!("not pasted: {e:#}")),
        }
    }

    pub fn request_edit(&mut self) {
        self.edit_requested = true;
    }

    pub fn take_edit_request(&mut self) -> bool {
        std::mem::take(&mut self.edit_requested)
    }

    /// Replace the prompt's text, leaving the cursor at its end.
    pub fn set_input(&mut self, text: &str) {
        self.input = prompt::clean(text);
        self.cursor = self.input.chars().count();
        self.refresh_suggestions(false);
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Move to the row above, keeping the column. False on the first row.
    pub fn move_cursor_up(&mut self) -> bool {
        self.move_cursor_rows(-1)
    }

    /// Move to the row below, keeping the column. False on the last row.
    pub fn move_cursor_down(&mut self) -> bool {
        self.move_cursor_rows(1)
    }

    fn move_cursor_rows(&mut self, delta: isize) -> bool {
        let rows = self.prompt_rows();
        let row = prompt::cursor_row(&rows, self.cursor);
        let Some(target) = row
            .checked_add_signed(delta)
            .and_then(|r| rows.get(r).copied())
        else {
            return false;
        };
        let col = prompt::width_between(&self.input, rows[row].start, self.cursor);
        self.cursor = prompt::index_at_column(&self.input, target, col);
        self.close_file_suggestions();
        true
    }

    /// Start of the line the cursor is on.
    pub fn move_cursor_home(&mut self) {
        let before: Vec<char> = self.input.chars().take(self.cursor).collect();
        self.cursor = before.iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1);
        self.close_file_suggestions();
    }

    /// End of the line the cursor is on.
    pub fn move_cursor_end(&mut self) {
        self.cursor += self
            .input
            .chars()
            .skip(self.cursor)
            .take_while(|c| *c != '\n')
            .count();
        self.close_file_suggestions();
    }

    pub fn take_input(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.prompt_scroll = 0;
        self.history.reset();
        self.close_suggestions();
        text
    }

    /// Mouse input, when the TUI has the mouse. A dialog takes the wheel,
    /// for what it scrolls, and a release ends a drag it cut short.
    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        if let Some(modal) = &mut self.modal {
            match ev.kind {
                MouseEventKind::ScrollUp => {
                    modal.scroll(-i32::from(WHEEL_LINES));
                }
                MouseEventKind::ScrollDown => {
                    modal.scroll(i32::from(WHEEL_LINES));
                }
                // Nothing is copied: the press may have been on the dialog.
                MouseEventKind::Up(MouseButton::Left) => {
                    self.end_drag();
                }
                _ => {}
            }
            return;
        }
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_down(WHEEL_LINES),
            MouseEventKind::Down(MouseButton::Left) => self.mouse_press(ev.column, ev.row),
            MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(ev.column, ev.row),
            MouseEventKind::Up(MouseButton::Left) => self.mouse_release(),
            _ => {}
        }
    }

    /// The transcript cell under a screen position. With `clamp`, a position
    /// outside the transcript maps to the nearest cell in view.
    fn transcript_point(&self, column: u16, row: u16, clamp: bool) -> Option<Point> {
        let view = &self.transcript_view;
        let area = view.area;
        if area.width == 0 || area.height == 0 || view.lines.is_empty() {
            return None;
        }
        let inside = column >= area.x
            && column < area.x + area.width
            && row >= area.y
            && row < area.y + area.height;
        if !inside && !clamp {
            return None;
        }
        let column = column.clamp(area.x, area.x + area.width - 1);
        let row = row.clamp(area.y, area.y + area.height - 1);
        let line = view.scroll + (row - area.y) as usize;
        if line >= view.lines.len() && !clamp {
            return None;
        }
        Some(Point {
            line: line.min(view.lines.len() - 1),
            col: (column - area.x) as usize,
        })
    }

    /// Scroll so the scrollbar's thumb follows a pointer on `row`.
    fn scrollbar_follow(&mut self, bar: Scrollbar, row: u16, grab: u16) {
        let top = row as i32 - bar.y as i32 - grab as i32;
        // The renderer goes back to following the end if this reaches it.
        self.auto_scroll = false;
        self.scroll = bar.scroll_for(top);
    }

    pub fn scrollbar_held(&self) -> bool {
        self.scrollbar_grab.is_some()
    }

    fn mouse_press(&mut self, column: u16, row: u16) {
        self.drag_edge = 0;
        if self
            .transcript_view
            .jump
            .is_some_and(|r| r.contains((column, row).into()))
        {
            self.scroll_to_bottom();
            return;
        }
        if let Some(bar) = self.transcript_view.scrollbar
            && bar.contains(column, row)
        {
            // On the thumb: hold it where it was grabbed. On the track:
            // bring the thumb's middle to the pointer, then hold that.
            let offset = row - bar.y;
            let on_thumb = offset >= bar.thumb_top && offset < bar.thumb_top + bar.thumb_len;
            let grab = if on_thumb {
                offset - bar.thumb_top
            } else {
                bar.thumb_len / 2
            };
            self.scrollbar_grab = Some(grab);
            self.scrollbar_follow(bar, row, grab);
            return;
        }
        let Some(point) = self.transcript_point(column, row, false) else {
            self.selection = None;
            self.last_click = None;
            return;
        };
        let now = Instant::now();
        let count = match self.last_click {
            Some((at, p, n)) if p == point && now.duration_since(at) < MULTI_CLICK && n < 3 => {
                n + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, point, count));
        self.selection = Some(Selection {
            anchor: point,
            focus: point,
            granularity: match count {
                1 => Granularity::Char,
                2 => Granularity::Word,
                _ => Granularity::Line,
            },
        });
    }

    fn mouse_drag(&mut self, column: u16, row: u16) {
        if let (Some(grab), Some(bar)) = (self.scrollbar_grab, self.transcript_view.scrollbar) {
            self.scrollbar_follow(bar, row, grab);
            return;
        }
        let Some(point) = self.transcript_point(column, row, true) else {
            return;
        };
        let Some(sel) = self.selection.as_mut() else {
            return;
        };
        sel.focus = point;
        let area = self.transcript_view.area;
        self.drag_edge = if row < area.y {
            -1
        } else if row >= area.y + area.height {
            1
        } else {
            0
        };
    }

    /// Stop scrolling at an edge and let go of the scrollbar; true when its
    /// thumb was held.
    fn end_drag(&mut self) -> bool {
        self.drag_edge = 0;
        self.scrollbar_grab.take().is_some()
    }

    fn mouse_release(&mut self) {
        if self.end_drag() {
            return;
        }
        match self.selected_text() {
            Some(text) if !text.trim().is_empty() => self.copy_request = Some(text),
            _ => {
                // A plain click, or only blank cells: nothing is selected.
                if let Some(s) = self
                    .selection
                    .filter(|s| s.granularity == Granularity::Char)
                {
                    self.selection = None;
                    if s.anchor == s.focus {
                        self.toggle_tool_at(s.anchor.line);
                    }
                }
            }
        }
    }

    /// Expand or collapse the tool call whose first row is rendered line
    /// `line` (a click on it).
    fn toggle_tool_at(&mut self, line: usize) -> bool {
        let Some(i) = self
            .transcript_view
            .blocks
            .iter()
            .position(|b| b.top == line && b.top < b.end)
        else {
            return false;
        };
        match self.shown_transcript_mut().blocks.get_mut(i) {
            // A call that spawned a subagent opens that subagent's transcript.
            Some(super::transcript::Block::Tool {
                id, agent: Some(_), ..
            }) => {
                let id = id.clone();
                self.open_subagent(&id);
                true
            }
            Some(super::transcript::Block::Tool { collapsed, .. }) => {
                *collapsed = !*collapsed;
                true
            }
            _ => false,
        }
    }

    /// The selection as first and last cell, for the highlight.
    pub fn selection_range(&self) -> Option<(Point, Point)> {
        self.selection?
            .range(&self.transcript_view.lines, &self.transcript_view.copy)
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        Some(selection::text(
            &self.transcript_view.lines,
            &self.transcript_view.copy,
            start,
            end,
        ))
    }

    #[cfg(test)]
    pub(crate) fn last_click_forget(&mut self) {
        self.last_click = None;
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.drag_edge = 0;
    }

    pub fn take_copy_request(&mut self) -> Option<String> {
        self.copy_request.take()
    }

    /// Show `message` in the status rule for a moment.
    pub fn flash(&mut self, message: impl Into<String>) {
        self.flash = Some((message.into(), Instant::now()));
    }

    pub fn flash_text(&self) -> Option<&str> {
        self.flash.as_ref().map(|(m, _)| m.as_str())
    }

    /// Timed mouse work: keep scrolling while a drag is held past an edge,
    /// and take down an expired flash. True when the screen changed.
    pub fn tick_mouse(&mut self) -> bool {
        let mut changed = false;
        if self.drag_edge != 0 && self.selection.is_some() {
            let view = &self.transcript_view;
            let last = view.lines.len().saturating_sub(1);
            let line = if self.drag_edge < 0 {
                view.scroll.saturating_sub(1)
            } else {
                (view.scroll + view.area.height as usize).min(last)
            };
            if self.drag_edge < 0 {
                self.scroll_up(1);
            } else {
                self.scroll_down(1);
            }
            if let Some(sel) = self.selection.as_mut() {
                sel.focus.line = line;
            }
            changed = true;
        }
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= FLASH)
        {
            self.flash = None;
            changed = true;
        }
        changed
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.auto_scroll = false;
        self.scroll = self.scroll.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
    }
}

/// The bridge takes at most this fraction (1/n) of the window it goes to.
const BRIDGE_WINDOW_SHARE: usize = 4;
/// About what a token is in English prose and code.
const CHARS_PER_TOKEN: usize = 4;

/// However large the window, a summary longer than this is no longer one.
const HANDOFF_MAX_WORDS: usize = 1_500;

/// What the harness being left is asked for. It stays in that harness's
/// session, so what it forbids is said to hold for this reply only.
/// `budget`: the characters the bridge to `to` may take, half of which
/// the summary is asked to stay within (in words, at about six characters
/// a word), the rest being for the first prompt and what follows it.
fn handoff_prompt(to: &str, budget: usize) -> String {
    let words = (budget / 2 / 6).clamp(50, HANDOFF_MAX_WORDS);
    format!(
        "[unharness] The user is switching this conversation to {to}, another coding agent \
         that will see only part of it. Write a handoff summary for that agent, in at most \
         {words} words: the user's goal and constraints, the decisions made and why, what has \
         been done (files changed, commands run and what came of them), what is unfinished or \
         failing, and the next steps. Be specific (paths, names, error messages). Leave out \
         your own setup (tools, connectors, MCP servers) unless the task depends on it. For \
         this reply only, use no tools and change nothing: answer from what you already know. \
         The conversation may come back to you later, with your tools as before."
    )
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
pub(crate) mod tests {
    use super::*;
    use crate::core::{PermissionKind, Question, QuestionOption, ToolAction};
    use crate::harness::agy::AgyHarness;
    use crate::harness::claude::ClaudeHarness;
    use crossterm::event::KeyEventKind;

    fn test_registry() -> Arc<Registry> {
        Arc::new(
            Registry::empty()
                .with(Box::new(AgyHarness))
                .with(Box::new(ClaudeHarness::default()))
                .with(Box::new(crate::harness::codex::CodexHarness::new(
                    crate::harness::codex::CodexTransport::AppServer,
                )))
                .with(Box::new(crate::harness::pi::PiHarness::default())),
        )
    }

    pub(crate) fn test_app_in(
        cwd: PathBuf,
        harness: HarnessId,
        resume: Option<String>,
        harness_explicit: bool,
    ) -> App {
        let mut app = test_app_asking(cwd, harness, resume, harness_explicit);
        // A harness without `ask` waits for a choice; most tests are not
        // about that.
        if app.effective_policy().is_none() {
            app.policy_choice
                .insert(harness, PermissionPolicy::AcceptEdits);
            app.modal = None;
        }
        app
    }

    /// An app as it starts, with `ask` requested whatever the harness.
    fn test_app_asking(
        cwd: PathBuf,
        harness: HarnessId,
        resume: Option<String>,
        harness_explicit: bool,
    ) -> App {
        test_app_with(
            cwd,
            harness,
            resume,
            harness_explicit,
            Config::default(),
            test_registry(),
        )
    }

    fn test_app_with(
        cwd: PathBuf,
        harness: HarnessId,
        resume: Option<String>,
        harness_explicit: bool,
        config: Config,
        registry: Arc<Registry>,
    ) -> App {
        App::new(test_init(
            cwd,
            harness,
            resume,
            harness_explicit,
            config,
            registry,
        ))
    }

    /// What `test_app_with` starts an app with, for a test to change.
    fn test_init(
        cwd: PathBuf,
        harness: HarnessId,
        resume: Option<String>,
        harness_explicit: bool,
        config: Config,
        registry: Arc<Registry>,
    ) -> AppInit {
        AppInit {
            // Tests never write to the real state directory.
            checkpoint_store: Some(cwd.join(".unharness/test-checkpoints")),
            context_windows: Some(cwd.join(".unharness/test-state/context_windows.json")),
            // Nor to the real config directory.
            rules: Rules::load_in(&cwd.join(".unharness/test-config"), Some(&cwd)).unwrap(),
            cwd: cwd.clone(),
            workspace_root: Some(cwd),
            registry,
            config,
            harness,
            policy: None,
            sandbox: SandboxSetup::off(),
            provider: None,
            model: None,
            effort: None,
            resume,
            harness_explicit,
            default_providers: [
                (HarnessId::CLAUDE, "anthropic".into()),
                (HarnessId::CODEX, "openai".into()),
                (HarnessId::AGY, "google".into()),
            ]
            .into(),
        }
    }

    pub(crate) fn test_app(harness: HarnessId) -> App {
        let tmp = tempfile::tempdir().unwrap();
        test_app_in(tmp.keep(), harness, None, false)
    }

    #[test]
    fn conversation_persists_across_harness_switch_and_resumes() {
        use super::super::transcript::Block;
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.keep();
        let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
        app.submit_prompt("first question".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TextDelta("first answer".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.switch_harness(HarnessId::CODEX);
        app.take_actions();
        app.session_alive = false;
        app.submit_prompt("second question".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "codex-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TextDelta("second answer".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        let conv_id = app.conversation.id.clone();
        app.quit();

        let rows = app.store.list();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, conv_id);
        assert_eq!(rows[0].harnesses, vec![HarnessId::CLAUDE, HarnessId::CODEX]);
        assert_eq!(rows[0].title, "first question");

        let mut again = test_app_in(cwd.clone(), HarnessId::CLAUDE, Some(String::new()), false);
        assert_eq!(again.active, HarnessId::CODEX);
        assert_eq!(again.conversation.id, conv_id);
        assert_eq!(
            again
                .session_ids
                .get(&HarnessId::CLAUDE)
                .map(String::as_str),
            Some("claude-1")
        );
        assert_eq!(
            again.session_ids.get(&HarnessId::CODEX).map(String::as_str),
            Some("codex-1")
        );
        assert!(
            again
                .transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Assistant { text, .. } if text == "second answer"))
        );
        assert!(
            again
                .transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Notice(n) if n.contains("resumed conversation")))
        );

        again.submit_prompt("third".into());
        assert_eq!(
            again.take_actions(),
            vec![
                Action::StartSession {
                    resume: Some("codex-1".into())
                },
                Action::turn("third")
            ]
        );
        again.session_alive = true;
        again.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        again.switch_harness(HarnessId::CLAUDE);
        again.take_actions();
        again.session_alive = false;
        again.submit_prompt("fourth".into());
        let actions = again.take_actions();
        assert_eq!(
            actions[0],
            Action::StartSession {
                resume: Some("claude-1".into())
            }
        );
        match &actions[1] {
            Action::SendTurn { text: t, .. } => {
                assert!(t.contains("second question") && t.contains("third"), "{t}");
                assert!(!t.contains("first question"), "{t}");
            }
            other => panic!("{other:?}"),
        }

        let forced = test_app_in(cwd, HarnessId::PI, Some(conv_id[..8].to_string()), true);
        assert_eq!(forced.active, HarnessId::PI);
        assert_eq!(forced.session_ids.len(), 2);
    }

    #[test]
    fn live_capabilities_and_status_events_apply() {
        use crate::core::{CapsUpdate, ContextUsage, PlanEntry, PlanStatus};
        let mut app = test_app(HarnessId::CLAUDE);
        assert!(app.caps().supports_effort("xhigh"));
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            effort_levels: Some(vec!["low".into()]),
            image_input: Some(true),
            ..Default::default()
        }));
        let caps = app.caps();
        assert!(caps.image_input && !caps.supports_effort("xhigh"));

        app.on_event(AgentEvent::Context(ContextUsage {
            used: Some(10),
            window: Some(100),
        }));
        assert_eq!(app.context.percent(), Some(10));
        app.on_event(AgentEvent::PlanUpdated {
            entries: vec![PlanEntry {
                text: "step".into(),
                status: PlanStatus::InProgress,
            }],
            explanation: None,
        });
        assert_eq!(app.plan.len(), 1);

        // Nothing a subagent does lands in the main transcript, least of
        // all when the call that spawned it is unknown.
        let before = app.transcript.blocks.len();
        app.on_event(AgentEvent::Sub {
            parent: "p".into(),
            event: Box::new(AgentEvent::TextDelta("inner".into())),
        });
        assert_eq!(app.transcript.blocks.len(), before);
        app.on_event(AgentEvent::Sub {
            parent: "p".into(),
            event: Box::new(AgentEvent::ToolCallStarted {
                id: "c".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }),
        });
        assert_eq!(app.transcript.blocks.len(), before);
    }

    #[test]
    fn rate_limit_warns_once_when_a_window_runs_high() {
        use crate::core::{RateLimitInfo, RateLimitWindow};
        let limit = |pct: f32| {
            AgentEvent::RateLimit(RateLimitInfo {
                status: Some("allowed".into()),
                windows: vec![RateLimitWindow {
                    label: "five_hour".into(),
                    used_percent: Some(pct),
                    resets_at: None,
                }],
            })
        };
        let mut app = test_app(HarnessId::CLAUDE);
        let base = app.transcript.blocks.len();
        app.on_event(limit(20.0));
        assert_eq!(app.transcript.blocks.len(), base);
        app.on_event(limit(85.0));
        app.on_event(limit(90.0));
        assert_eq!(app.transcript.blocks.len(), base + 1);
        assert_eq!(
            rate_limit_summary(app.rate_limit.as_ref().unwrap()),
            "five_hour 90% used"
        );
    }

    // Held behind the start of the session it was for, the rewind is
    // dropped when unharness quits first: that session is not resumed.
    #[test]
    fn a_rewind_dropped_at_quit_forgets_the_session_it_was_for() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "s1".into(),
            model: None,
        });
        for (prompt, anchor) in [("one", "a1"), ("two", "a2")] {
            app.submit_prompt(prompt.into());
            app.on_event(AgentEvent::TurnAnchor { id: anchor.into() });
            app.on_event(AgentEvent::TurnCompleted {
                stop_reason: StopReason::Done,
            });
        }
        app.session_alive = false;
        app.take_actions();
        let two = app
            .transcript
            .blocks
            .iter()
            .position(
                |b| matches!(b, super::super::transcript::Block::User { text } if text == "two"),
            )
            .unwrap();
        app.rewind_to(two, false);
        let held = app.take_actions();
        assert!(matches!(held[0], Action::StartSession { .. }), "{held:?}");

        app.drop_held(held);
        assert!(!app.session_ids.contains_key(&HarnessId::CLAUDE));
        // Without a rewind among them, nothing is forgotten.
        app.session_ids.insert(HarnessId::CLAUDE, "s2".into());
        app.drop_held([Action::StartSession {
            resume: Some("s2".into()),
        }]);
        assert!(app.session_ids.contains_key(&HarnessId::CLAUDE));
    }

    #[test]
    fn rewind_uses_the_harness_anchor_or_starts_a_fresh_session() {
        use super::super::transcript::Block;
        let done = || AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        };
        let mut app = test_app(HarnessId::CLAUDE);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "s1".into(),
            model: None,
        });
        for (prompt, anchor) in [("one", "a1"), ("two", "a2"), ("three", "a3")] {
            app.submit_prompt(prompt.into());
            app.session_alive = true;
            app.on_event(AgentEvent::TurnAnchor { id: anchor.into() });
            app.on_event(AgentEvent::TextDelta(format!("re {prompt}")));
            app.on_event(done());
        }
        app.take_actions();
        let block_of = |app: &App, want: &str| {
            app.transcript
                .blocks
                .iter()
                .position(|b| matches!(b, Block::User { text } if text == want))
                .unwrap()
        };

        // Live session with an anchor: the harness drops the turns itself.
        let three = block_of(&app, "three");
        app.rewind_to(three, false);
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::Rewind {
                anchor: "a3".into()
            })]
        );
        assert_eq!(app.input, "three");
        assert!(app.session_ids.contains_key(&HarnessId::CLAUDE));
        assert!(
            !app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::User { text } if text == "three"))
        );
        assert_eq!(app.anchors.len(), 2);
        app.take_input();

        // The session is not running: it is reattached, then rewound.
        app.session_alive = false;
        let two = block_of(&app, "two");
        app.rewind_to(two, false);
        assert_eq!(
            app.take_actions(),
            vec![
                Action::StartSession {
                    resume: Some("s1".into())
                },
                Action::Command(SessionCommand::Rewind {
                    anchor: "a2".into()
                })
            ]
        );
        assert_eq!(app.input, "two");
        app.take_input();

        // The harness reports that it did not rewind: fall back to a fresh
        // session that gets the remaining conversation as context.
        app.session_alive = true;
        app.on_event(AgentEvent::RewindFailed {
            reason: "stale target".into(),
        });
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert!(!app.session_ids.contains_key(&HarnessId::CLAUDE));

        // Back to before a session's first turn nothing of it remains, so a
        // fresh session is used rather than a native rewind.
        let mut app = test_app(HarnessId::CLAUDE);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "s2".into(),
            model: None,
        });
        app.submit_prompt("one".into());
        app.session_alive = true;
        app.on_event(AgentEvent::TurnAnchor { id: "b1".into() });
        app.on_event(done());
        app.take_actions();
        let one = block_of(&app, "one");
        app.rewind_to(one, false);
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert!(!app.session_ids.contains_key(&HarnessId::CLAUDE));

        // A harness that cannot rewind at all goes straight to the fallback.
        let mut app = test_app(HarnessId::AGY);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "g1".into(),
            model: None,
        });
        app.submit_prompt("one".into());
        app.on_event(done());
        app.take_actions();
        let one = block_of(&app, "one");
        app.rewind_to(one, false);
        assert!(app.take_actions().is_empty());
        assert!(!app.session_ids.contains_key(&HarnessId::AGY));
    }

    #[test]
    fn rewind_drops_sessions_that_saw_the_removed_turns() {
        let done = || AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        };
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("one".into());
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(done());
        let after_one = app.transcript.blocks.len();
        app.switch_harness(HarnessId::CODEX);
        app.submit_prompt("two".into());
        app.on_event(AgentEvent::SessionStarted {
            session_id: "codex-1".into(),
            model: None,
        });
        app.on_event(done());
        app.switch_harness(HarnessId::PI);
        app.submit_prompt("three".into());
        app.on_event(done());
        app.take_actions();

        // Rewinding to before "two": Codex saw it and is reset; Claude's
        // session ended before it and stays.
        let two = app
            .transcript
            .blocks
            .iter()
            .position(
                |b| matches!(b, super::super::transcript::Block::User { text } if text == "two"),
            )
            .unwrap();
        assert!(two >= after_one);
        app.rewind_to(two, false);
        assert!(app.session_ids.contains_key(&HarnessId::CLAUDE));
        assert!(!app.session_ids.contains_key(&HarnessId::CODEX));
    }

    #[test]
    fn anchors_attach_in_order_even_when_they_arrive_late() {
        // pi reports a turn's anchor after the turn, when a queued prompt may
        // already have started the next one.
        let mut app = test_app(HarnessId::PI);
        app.submit_prompt("one".into());
        app.session_alive = true;
        app.queue_prompt("two".into());
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.on_event(AgentEvent::TurnAnchor { id: "e1".into() });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        // A turn that never reached the harness reports an empty anchor.
        app.submit_prompt("three".into());
        app.on_event(AgentEvent::TurnAnchor { id: "e2".into() });
        app.on_event(AgentEvent::TurnAnchor { id: String::new() });
        let ids: Vec<(&str, usize)> = app
            .anchors
            .iter()
            .map(|a| (a.id.as_str(), a.block))
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids[0].0 == "e1" && ids[1].0 == "e2" && ids[0].1 < ids[1].1);
    }

    #[test]
    fn fork_branches_sessions_where_the_harness_can() {
        let done = || AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::AGY, None, false);
        app.submit_prompt("one".into());
        app.on_event(AgentEvent::SessionStarted {
            session_id: "agy-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TextDelta("answer".into()));
        app.on_event(done());
        app.switch_harness(HarnessId::CLAUDE);
        app.submit_prompt("two".into());
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnAnchor { id: "u2".into() });
        app.on_event(done());
        app.take_actions();
        app.session_alive = false;

        let original = app.conversation.id.clone();
        app.fork_conversation();
        assert_ne!(app.conversation.id, original);
        // Claude can branch a session: the id is kept, to be forked on next
        // use. Its rewind ids do not carry over into a branch. Antigravity
        // cannot, so the copy starts it fresh.
        assert_eq!(app.session_ids.get(&HarnessId::CLAUDE).unwrap(), "claude-1");
        assert!(app.fork_pending());
        assert!(app.anchors.is_empty());
        assert!(!app.session_ids.contains_key(&HarnessId::AGY));

        // The original is on disk untouched; the copy remembers what is pending.
        let saved = app.store.load(&original).unwrap();
        assert_eq!(saved.sessions[&HarnessId::CLAUDE], "claude-1");
        assert!(saved.fork_pending.is_empty());
        let copy = app.store.load(&app.conversation.id).unwrap();
        assert_eq!(copy.fork_pending, vec![HarnessId::CLAUDE]);

        // The branch knows the whole conversation: nothing is re-sent as text.
        app.submit_prompt("three".into());
        assert_eq!(
            app.take_actions(),
            vec![
                Action::StartSession {
                    resume: Some("claude-1".into())
                },
                Action::turn("three")
            ]
        );
        // Once the branch reports its own id, it is an ordinary session.
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-2".into(),
            model: None,
        });
        assert!(!app.fork_pending());
        assert_eq!(app.session_ids[&HarnessId::CLAUDE], "claude-2");
    }

    #[test]
    fn rewind_can_restore_files_and_undo_it() {
        let git = |dir: &std::path::Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), "before\n").unwrap();

        let mut app = test_app_in(root.to_path_buf(), HarnessId::AGY, None, false);
        app.submit_prompt("change it".into());
        // The agent edits a file and creates another during the turn.
        std::fs::write(root.join("a.txt"), "after\n").unwrap();
        std::fs::write(root.join("new.txt"), "made by the agent\n").unwrap();
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        app.open_rewind_picker();
        assert!(matches!(&app.modal, Some(Modal::Rewind(p)) if p.items[0].files));
        app.handle_modal_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(app.modal.is_none());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "before\n"
        );
        assert!(!root.join("new.txt").exists());
        assert_eq!(app.input, "change it");

        app.handle_slash_command("/undo-restore");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "after\n"
        );
        assert!(root.join("new.txt").exists());
    }

    fn spawn_block<'a>(app: &'a App, want: &str) -> &'a super::super::transcript::AgentRun {
        app.transcript
            .agent(want)
            .unwrap_or_else(|| panic!("no subagent on {want}"))
    }

    /// What a transcript holds, one word per block.
    fn kinds(blocks: &[super::super::transcript::Block]) -> Vec<String> {
        use super::super::transcript::Block;
        blocks
            .iter()
            .map(|b| match b {
                Block::User { .. } => "user".to_string(),
                Block::Assistant { text, .. } => format!("text:{text}"),
                Block::Thought { .. } => "thought".to_string(),
                Block::Handoff { to, .. } => format!("handoff:{to}"),
                Block::Tool { id, .. } => format!("tool:{id}"),
                Block::System(_) => "system".to_string(),
                Block::Notice(_) => "notice".to_string(),
                Block::Error(_) => "error".to_string(),
                Block::Shell { command, .. } => format!("shell:{command}"),
                Block::Hook { name, .. } => format!("hook:{name}"),
            })
            .collect()
    }

    fn sub(parent: &str, event: AgentEvent) -> AgentEvent {
        AgentEvent::Sub {
            parent: parent.into(),
            event: Box::new(event),
        }
    }

    /// A hook whose end will not come is not left running, nor called failed.
    #[test]
    fn hooks_still_running_end_as_unknown() {
        use super::super::transcript::{Block, HookState};
        let state = |app: &App, n: usize| {
            app.transcript
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Hook { state, .. } => Some(*state),
                    _ => None,
                })
                .nth(n)
        };
        let started = |id: &str| AgentEvent::HookStarted {
            id: id.into(),
            name: "Stop".into(),
        };
        let sub_state = |app: &App| match spawn_block(app, "spawn").log.blocks.first() {
            Some(Block::Hook { state, .. }) => Some(*state),
            _ => None,
        };
        let mut app = test_app(HarnessId::CODEX);
        app.submit_prompt("go".into());
        app.on_event(started("a"));
        spawn(&mut app, "spawn", "look around", None);
        app.on_event(sub("spawn", started("s")));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        // An async hook may outlive a turn that finished.
        assert_eq!(state(&app, 0), Some(HookState::Running));
        app.submit_prompt("again".into());
        app.on_event(started("b"));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        });
        assert_eq!(state(&app, 1), Some(HookState::Unknown));
        // Not an earlier turn's, nor a sub-agent's, which outlives it.
        assert_eq!(state(&app, 0), Some(HookState::Running));
        assert_eq!(sub_state(&app), Some(HookState::Running));
        // Its end came after all: it is ended where it is.
        app.on_event(AgentEvent::HookEnded {
            id: "b".into(),
            name: "Stop".into(),
            outcome: crate::core::HookOutcome::Succeeded,
            output: String::new(),
        });
        assert_eq!(
            state(&app, 1),
            Some(HookState::Ended(crate::core::HookOutcome::Succeeded))
        );
        assert_eq!(state(&app, 2), None);
        app.on_event(started("c"));
        app.on_event(AgentEvent::ProcessExited { code: Some(0) });
        assert_eq!(state(&app, 0), Some(HookState::Unknown));
        assert_eq!(state(&app, 2), Some(HookState::Unknown));
        assert_eq!(sub_state(&app), Some(HookState::Unknown));
    }

    /// `/clear` starts a new conversation: the old one is saved as it was,
    /// and no harness takes its session into the new one.
    #[test]
    fn clear_starts_a_new_conversation() {
        let done = || AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        };
        let mut app = test_app(HarnessId::CODEX);
        app.submit_prompt("one".into());
        app.on_event(AgentEvent::SessionStarted {
            session_id: "codex-1".into(),
            model: None,
        });
        app.on_event(done());
        app.switch_harness(HarnessId::CLAUDE);
        app.set_model("opus".into());
        app.submit_prompt("two".into());
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TextDelta("answer".into()));
        app.on_event(AgentEvent::Usage(Usage {
            input: 10,
            output: 5,
            ..Default::default()
        }));
        app.on_event(AgentEvent::Context(ContextUsage {
            used: Some(15),
            window: Some(1000),
        }));
        app.on_event(done());
        app.take_actions();
        app.session_alive = true;
        assert!(!app.last_active_index.is_empty());

        let old = app.conversation.id.clone();
        app.handle_slash_command("/clear");
        assert_ne!(app.conversation.id, old);
        assert!(app.session_ids.is_empty());
        assert!(app.last_active_index.is_empty());
        assert_eq!(app.session_usage, Usage::default());
        assert_eq!(app.context, ContextUsage::default());
        assert_eq!(app.active, HarnessId::CLAUDE);
        assert_eq!(app.models[&HarnessId::CLAUDE].model, "opus");
        // No harness has run in it yet.
        assert!(app.conversation.last_policies.is_empty());
        assert_eq!(
            app.transcript.blocks.len(),
            1,
            "only the new conversation's notice"
        );
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert!(!app.session_alive);

        // The next session knows nothing of the old one.
        app.start_when_idle();
        assert_eq!(
            app.take_actions(),
            vec![Action::StartSession { resume: None }]
        );

        // The old conversation is in the store as it was.
        let saved = app.store.load(&old).unwrap();
        assert_eq!(saved.sessions[&HarnessId::CLAUDE], "claude-1");
        assert_eq!(saved.sessions[&HarnessId::CODEX], "codex-1");
        assert!(saved.last_policies.contains_key(&HarnessId::CLAUDE));
        assert!(saved.blocks.iter().any(
            |b| matches!(b, crate::core::conversations::BlockRecord::Assistant { text, .. } if text == "answer")
        ));

        // A harness used before the clear starts afresh too.
        app.session_alive = false;
        app.switch_harness(HarnessId::CODEX);
        app.submit_prompt("three".into());
        assert_eq!(
            app.take_actions(),
            vec![Action::StartSession { resume: None }, Action::turn("three")]
        );
    }

    /// Nothing is saved for a conversation nothing was sent in, the old one
    /// or the new one.
    #[test]
    fn clear_of_nothing_saves_nothing() {
        let mut app = test_app(HarnessId::CLAUDE);
        let old = app.conversation.id.clone();
        app.handle_slash_command("/clear");
        assert_ne!(app.conversation.id, old);
        assert!(app.store.list().is_empty());
        assert!(matches!(
            app.transcript.blocks.as_slice(),
            [super::super::transcript::Block::System(s)] if s == "New conversation."
        ));
    }

    /// A session started with nothing to resume and sent nothing serves
    /// the new conversation; one started on a session of the old ends.
    #[test]
    fn clear_keeps_a_session_that_knows_nothing() {
        let mut app = test_app(HarnessId::CLAUDE);
        blank_session(&mut app);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.handle_slash_command("/clear");
        assert!(app.take_actions().is_empty());
        assert!(app.session_alive);
        // Its id is the new conversation's once something is sent.
        app.submit_prompt("hello".into());
        assert_eq!(app.session_ids[&HarnessId::CLAUDE], "claude-1");

        let mut app = test_app(HarnessId::CLAUDE);
        app.session_ids.insert(HarnessId::CLAUDE, "claude-0".into());
        blank_session(&mut app);
        app.handle_slash_command("/clear");
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    }

    /// What ending the session told about the harness's configuration is
    /// not lost with the old transcript.
    #[test]
    fn clear_tells_a_configuration_change_in_the_new_conversation() {
        use crate::core::guard::{Guarded, Watch};
        let mut app = test_app(HarnessId::PI);
        app.submit_prompt("one".into());
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.session_alive = true;
        let config = tempfile::tempdir().unwrap();
        app.guard = Some(Watch::begin(
            &[Guarded::Tree(config.path().to_path_buf())],
            None,
        ));
        std::fs::write(config.path().join("extension.ts"), "x").unwrap();
        app.handle_slash_command("/clear");
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("configuration changed while it ran"))
        );
    }

    /// A conversation that cannot be saved is not dropped.
    #[test]
    fn clear_keeps_a_conversation_it_cannot_save() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("one".into());
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        let dir = app.store.root().join("conversations");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::write(&dir, "in the way").unwrap();
        let id = app.conversation.id.clone();
        app.handle_slash_command("/clear");
        assert_eq!(app.conversation.id, id);
        assert!(
            notices(&app)
                .last()
                .unwrap()
                .starts_with("could not save the conversation, so /clear keeps it")
        );
    }

    /// Prompts held after a turn that failed wait for the user in the new
    /// conversation.
    #[test]
    fn clear_keeps_held_prompts() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("one".into());
        app.queue_prompt("two".into());
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Error("boom".into()),
        });
        assert_eq!(app.queued.len(), 1);
        app.handle_slash_command("/clear");
        assert_eq!(app.queued.len(), 1);
        assert!(
            notices(&app)
                .last()
                .unwrap()
                .starts_with("1 queued prompt(s) held")
        );
    }

    /// Not while something of the conversation is still at work: what it
    /// does would land in the new one.
    #[test]
    fn clear_is_refused_while_busy() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("one".into());
        let id = app.conversation.id.clone();
        app.handle_slash_command("/clear");
        assert_eq!(app.conversation.id, id);
        assert!(notices(&app).last().unwrap().contains("current turn"));

        spawn(&mut app, "spawn", "look around", None);
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.handle_slash_command("/clear");
        assert_eq!(app.conversation.id, id);
        assert!(
            notices(&app)
                .last()
                .unwrap()
                .contains("subagent(s) still at work")
        );

        app.subagents.clear();
        app.run_shell("sleep 5");
        app.handle_slash_command("/clear");
        assert_eq!(app.conversation.id, id);
        assert!(notices(&app).last().unwrap().contains("! command"));
    }

    /// A sub-agent's hooks (a Codex child thread's `hook/*`) belong to its
    /// own transcript.
    #[test]
    fn a_subagents_hook_goes_to_its_transcript() {
        let mut app = test_app(HarnessId::CODEX);
        app.submit_prompt("delegate".into());
        spawn(&mut app, "spawn", "look around", None);
        let main = kinds(&app.transcript.blocks);
        app.on_event(sub(
            "spawn",
            AgentEvent::HookStarted {
                id: "h".into(),
                name: "preToolUse".into(),
            },
        ));
        app.on_event(sub(
            "spawn",
            AgentEvent::HookEnded {
                id: "h".into(),
                name: "preToolUse".into(),
                outcome: crate::core::HookOutcome::Blocked,
                output: "no rm".into(),
            },
        ));
        assert_eq!(kinds(&app.transcript.blocks), main);
        let run = spawn_block(&app, "spawn");
        assert_eq!(kinds(&run.log.blocks), ["hook:preToolUse"]);
        assert!(matches!(
            &run.log.blocks[0],
            super::super::transcript::Block::Hook {
                state: super::super::transcript::HookState::Ended(
                    crate::core::HookOutcome::Blocked
                ),
                ..
            }
        ));
    }

    fn spawn(app: &mut App, id: &str, description: &str, kind: Option<&str>) {
        app.on_event(AgentEvent::ToolCallStarted {
            id: id.into(),
            name: "Agent".into(),
            input: serde_json::json!({ "description": description }),
        });
        app.on_event(AgentEvent::SubagentStarted {
            id: id.into(),
            description: description.into(),
            kind: kind.map(str::to_string),
        });
    }

    fn read_call(id: &str, file: &str) -> AgentEvent {
        AgentEvent::ToolCallStarted {
            id: id.into(),
            name: "Read".into(),
            input: serde_json::json!({ "file_path": file }),
        }
    }

    #[test]
    fn an_unannounced_subagent_still_gets_a_transcript_of_its_own() {
        use crate::core::SubagentStatus;
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("delegate".into());
        // A harness that attributes a subagent's work without announcing it.
        app.on_event(AgentEvent::ToolCallStarted {
            id: "spawn".into(),
            name: "Agent".into(),
            input: serde_json::json!({"description": "look around"}),
        });
        let main_len = app.transcript.blocks.len();
        app.on_event(sub("spawn", read_call("inner", "a.txt")));
        app.on_event(sub(
            "spawn",
            AgentEvent::ToolCallResult {
                id: "inner".into(),
                output: "contents".into(),
                is_error: false,
            },
        ));
        app.on_event(sub("spawn", AgentEvent::TextDelta("found it".into())));
        assert_eq!(app.transcript.blocks.len(), main_len);
        let run = spawn_block(&app, "spawn");
        assert_eq!(run.description, "look around");
        assert_eq!(run.status, None);
        assert_eq!(kinds(&run.log.blocks), ["tool:inner", "text:found it"]);
        // It ends when its call does.
        app.on_event(AgentEvent::ToolCallResult {
            id: "spawn".into(),
            output: "found it".into(),
            is_error: false,
        });
        assert_eq!(
            spawn_block(&app, "spawn").status,
            Some(SubagentStatus::Completed)
        );

        // Another harness is told what it reported, not what it called.
        let bridge = app.transcript.bridge_text(0, 10_000).unwrap();
        assert!(bridge.contains("[tool Agent look around → completed] found it"));
        assert!(!bridge.contains("[tool Read"));

        // A conversation saved when subagent calls sat in the main list,
        // pointing at their spawner, comes back with them under it.
        let old: Vec<crate::core::conversations::BlockRecord> =
            serde_json::from_value(serde_json::json!([
                {"kind": "user", "text": "delegate"},
                {"kind": "tool", "id": "spawn", "name": "Agent",
                 "input": {"description": "look around"}, "output": "found it", "is_error": false},
                {"kind": "tool", "id": "inner", "name": "Read", "input": {"file_path": "a.txt"},
                 "output": "contents", "is_error": false, "parent": "spawn"},
                {"kind": "assistant", "text": "done", "sender": "Claude", "secs": 1.0},
            ]))
            .unwrap();
        let back = Transcript::from_records(&old);
        assert_eq!(kinds(&back.blocks), ["user", "tool:spawn", "text:done"]);
        let run = back.agent("spawn").unwrap();
        assert_eq!(kinds(&run.log.blocks), ["tool:inner"]);
        assert_eq!(run.status, Some(SubagentStatus::Completed));
    }

    #[test]
    fn a_background_subagent_outlives_its_call_and_its_turn() {
        use crate::core::SubagentStatus;
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("delegate".into());
        spawn(&mut app, "spawn", "look around", Some("Explore"));
        // The call returns at once and the turn ends; the subagent works on.
        app.on_event(AgentEvent::ToolCallResult {
            id: "spawn".into(),
            output: "Async agent launched successfully.".into(),
            is_error: false,
        });
        app.on_event(AgentEvent::TextDelta("It is on its way.".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        assert!(!app.is_generating);
        assert_eq!(app.subagents.len(), 1);
        assert_eq!(spawn_block(&app, "spawn").status, None);
        let main = kinds(&app.transcript.blocks);

        app.on_event(sub("spawn", AgentEvent::ThinkingDelta("hm".into())));
        app.on_event(sub("spawn", AgentEvent::TextDelta("Looking.".into())));
        app.on_event(sub("spawn", read_call("inner", "a.txt")));
        // Until the harness words it, the call is the activity.
        assert_eq!(app.subagents[0].activity.as_deref(), Some("Read a.txt"));
        app.on_event(AgentEvent::SubagentProgress {
            id: "spawn".into(),
            activity: "Reading a.txt".into(),
        });
        app.on_event(sub("spawn", read_call("inner2", "b.txt")));
        assert_eq!(app.subagents[0].activity.as_deref(), Some("Reading a.txt"));
        assert_eq!(app.subagents[0].tools, 2);
        app.on_event(sub("spawn", AgentEvent::Notice("retrying".into())));
        app.on_event(sub(
            "spawn",
            AgentEvent::TextDelta("The word is alpha.".into()),
        ));
        app.on_event(AgentEvent::SubagentEnded {
            id: "spawn".into(),
            status: SubagentStatus::Completed,
            result: Some("The word is alpha.".into()),
        });
        assert!(app.subagents.is_empty());

        // All of it is in the subagent's transcript, the report (once) last,
        // and none of it in the main one.
        assert_eq!(kinds(&app.transcript.blocks), main);
        let run = spawn_block(&app, "spawn");
        assert_eq!(run.status, Some(SubagentStatus::Completed));
        let log = kinds(&run.log.blocks);
        assert_eq!(
            log,
            [
                "thought",
                "text:Looking.",
                "tool:inner",
                "tool:inner2",
                "notice",
                "text:The word is alpha."
            ]
        );
        assert_eq!(run.report(), "The word is alpha.");

        // Its end was saved though no turn was running, and comes back.
        let saved = app.store.load(&app.conversation.id).unwrap();
        let back = Transcript::from_records(&saved.blocks);
        assert_eq!(kinds(&back.blocks), main);
        let restored = back.agent("spawn").unwrap();
        assert_eq!(restored.status, Some(SubagentStatus::Completed));
        assert_eq!(kinds(&restored.log.blocks), log);
        assert_eq!(restored.kind.as_deref(), Some("Explore"));

        // Another harness is told what the subagent reported, not the receipt.
        let bridge = app.transcript.bridge_text(0, 10_000).unwrap();
        assert!(bridge.contains("completed] The word is alpha."));
        assert!(!bridge.contains("launched"));

        // A report the subagent never wrote out itself is added to its transcript.
        spawn(&mut app, "quiet", "say nothing", None);
        app.on_event(AgentEvent::SubagentEnded {
            id: "quiet".into(),
            status: SubagentStatus::Completed,
            result: Some("done quietly".into()),
        });
        assert_eq!(spawn_block(&app, "quiet").report(), "done quietly");
    }

    #[test]
    fn a_report_after_the_end_is_added_once() {
        use crate::core::SubagentStatus;
        let mut app = test_app(HarnessId::CLAUDE);
        spawn(&mut app, "spawn", "review", Some("reviewer"));
        app.on_event(sub("spawn", AgentEvent::TextDelta("Reviewing.".into())));
        let ended = |app: &mut App, result: Option<&str>| {
            app.on_event(AgentEvent::SubagentEnded {
                id: "spawn".into(),
                status: SubagentStatus::Completed,
                result: result.map(str::to_string),
            })
        };
        ended(&mut app, None);
        assert!(app.subagents.is_empty());
        let took = spawn_block(&app, "spawn").duration;
        assert!(took.is_some());
        ended(&mut app, Some("All good."));
        ended(&mut app, Some("All good."));
        let run = spawn_block(&app, "spawn");
        assert_eq!(run.duration, took);
        assert_eq!(
            kinds(&run.log.blocks),
            ["text:Reviewing.", "text:All good."]
        );
    }

    #[test]
    fn a_subagents_own_subagent_outlives_it() {
        use crate::core::SubagentStatus;
        let mut app = test_app(HarnessId::CLAUDE);
        spawn(&mut app, "outer", "delegate", None);
        app.on_event(sub(
            "outer",
            AgentEvent::ToolCallStarted {
                id: "inner".into(),
                name: "Agent".into(),
                input: serde_json::json!({ "description": "look" }),
            },
        ));
        app.on_event(AgentEvent::SubagentStarted {
            id: "inner".into(),
            description: "look".into(),
            kind: None,
        });
        let ended = |app: &mut App, id: &str| {
            app.on_event(AgentEvent::SubagentEnded {
                id: id.into(),
                status: SubagentStatus::Completed,
                result: None,
            })
        };
        ended(&mut app, "outer");
        let inner = spawn_block(&app, "inner");
        assert_eq!((inner.status, inner.duration), (None, None));
        assert_eq!(app.subagents.len(), 1);
        std::thread::sleep(Duration::from_millis(20));
        ended(&mut app, "inner");
        let inner = spawn_block(&app, "inner");
        assert_eq!(inner.status, Some(SubagentStatus::Completed));
        assert!(inner.duration.unwrap() >= Duration::from_millis(20));
        assert!(app.subagents.is_empty());
    }

    #[test]
    fn a_subagents_transcript_is_opened_read_and_left() {
        let mut app = test_app(HarnessId::CLAUDE);
        // Nothing to open.
        app.open_subagent_picker();
        assert!(app.modal.is_none());

        app.submit_prompt("delegate".into());
        app.session_alive = true;
        spawn(&mut app, "first", "look around", None);
        spawn(&mut app, "second", "look elsewhere", Some("Explore"));
        app.on_event(sub("first", read_call("f1", "a.txt")));
        app.on_event(sub("second", read_call("s1", "b.txt")));
        // A subagent's own subagent lives in its transcript.
        app.on_event(sub(
            "second",
            AgentEvent::ToolCallStarted {
                id: "deep".into(),
                name: "Agent".into(),
                input: serde_json::json!({"description": "deeper"}),
            },
        ));
        app.on_event(sub(
            "second",
            AgentEvent::SubagentStarted {
                id: "deep".into(),
                description: "deeper".into(),
                kind: None,
            },
        ));
        app.on_event(sub("deep", read_call("d1", "c.txt")));
        app.on_event(AgentEvent::SubagentEnded {
            id: "first".into(),
            status: crate::core::SubagentStatus::Completed,
            result: None,
        });
        assert_eq!(
            kinds(&spawn_block(&app, "second").log.blocks),
            ["tool:s1", "tool:deep"]
        );
        assert_eq!(kinds(&spawn_block(&app, "deep").log.blocks), ["tool:d1"]);
        let main = kinds(&app.transcript.blocks);
        assert_eq!(kinds(app.shown_blocks()), main);

        // The picker lists them all, finished or not, and starts on the
        // first still at work.
        app.open_subagent_picker();
        match &app.modal {
            Some(Modal::Subagents(p)) => {
                let rows: Vec<(&str, usize, bool)> = p
                    .items
                    .iter()
                    .map(|a| (a.id.as_str(), a.depth, a.status.is_none()))
                    .collect();
                assert_eq!(
                    rows,
                    [("first", 0, false), ("second", 0, true), ("deep", 1, true)]
                );
                assert_eq!(p.selected, 1);
            }
            _ => panic!("no picker"),
        }
        // Esc opens nothing; Enter opens the chosen one.
        app.handle_modal_key(key(KeyCode::Esc));
        assert!(app.modal.is_none() && app.viewing.is_none());
        app.scroll = 7;
        app.auto_scroll = false;
        app.open_subagent_picker();
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(app.viewing.as_deref(), Some("second"));
        assert_eq!(kinds(app.shown_blocks()), ["tool:s1", "tool:deep"]);
        assert!(app.auto_scroll);

        // Round the others and back.
        app.view_next_subagent(false);
        assert_eq!(kinds(app.shown_blocks()), ["tool:d1"]);
        app.view_next_subagent(false);
        assert_eq!(app.viewing.as_deref(), Some("first"));
        app.view_next_subagent(true);
        assert_eq!(app.viewing.as_deref(), Some("deep"));

        // Expanding is done to the transcript in view.
        app.shown_transcript_mut().toggle_last_tool();
        assert!(matches!(
            spawn_block(&app, "deep").log.blocks[0],
            super::super::transcript::Block::Tool {
                collapsed: false,
                ..
            }
        ));

        // Stopping is done from its own view, and only to it.
        app.take_actions();
        app.stop_viewed_subagent();
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::StopSubagent {
                id: "deep".into()
            })]
        );
        // One that has ended is not asked to stop.
        app.open_subagent("first");
        app.stop_viewed_subagent();
        assert!(app.take_actions().is_empty());

        // Leaving puts the main transcript back where it was.
        app.close_subagent_view();
        assert!(app.viewing.is_none());
        assert_eq!((app.scroll, app.auto_scroll), (7, false));
        assert_eq!(kinds(app.shown_blocks()), main);

        // A prompt sent while they work goes out at once.
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.queue_prompt("meanwhile".into());
        assert!(app.queued.is_empty());
        assert!(matches!(
            app.take_actions().as_slice(),
            [Action::SendTurn { text, .. }] if text == "meanwhile"
        ));

        // The session dies: nothing is left running, however deep.
        app.on_event(AgentEvent::ProcessExited { code: Some(1) });
        assert!(app.subagents.is_empty());
        for id in ["second", "deep"] {
            assert_eq!(
                spawn_block(&app, id).status,
                Some(crate::core::SubagentStatus::Cancelled)
            );
        }

        // A harness that cannot stop one says so.
        let mut app = test_app(HarnessId::PI);
        app.session_alive = true;
        spawn(&mut app, "spawn", "look around", None);
        app.open_subagent("spawn");
        app.stop_viewed_subagent();
        assert!(app.take_actions().is_empty());
        assert!(app.flash_text().is_some_and(|t| t.contains("cannot stop")));
    }

    /// Feed a recorded session through its parser into the app.
    fn replay_fixture(app: &mut App, path: &str, mut feed: impl FnMut(&str) -> Vec<AgentEvent>) {
        let text =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
                .unwrap();
        for line in text.lines() {
            if line.is_empty() || line.starts_with(['#', '>', '!']) {
                continue;
            }
            for ev in feed(line) {
                app.on_event(ev);
            }
        }
    }

    #[test]
    fn recorded_subagent_sessions_end_with_every_report_in_place() {
        use crate::core::SubagentStatus;
        use crate::harness::claude::parse::ClaudeParser;
        use crate::harness::codex::app_server_parse::CodexAppServerParser;
        let reports = |app: &App| -> Vec<(Option<SubagentStatus>, String)> {
            app.transcript
                .agents()
                .into_iter()
                .map(|(_, a, _)| (a.status, a.report().to_string()))
                .collect()
        };
        let done = Some(SubagentStatus::Completed);

        // Claude: three launched in the background, reports interleaved.
        let mut app = test_app(HarnessId::CLAUDE);
        let mut p = ClaudeParser::new();
        replay_fixture(
            &mut app,
            "src/harness/claude/fixtures/subagent_parallel.jsonl",
            |l| p.feed(l),
        );
        assert!(app.subagents.is_empty() && !app.is_generating);
        let got = reports(&app);
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|(s, _)| *s == done));
        assert!(got[0].1.contains("alpha") && got[1].1.contains("bravo"));
        assert!(got[2].1.contains("charlie"));
        // The main transcript holds the three spawning calls and no other;
        // each subagent's one call is in its own.
        let tools = |blocks: &[super::super::transcript::Block]| {
            kinds(blocks)
                .iter()
                .filter(|k| k.starts_with("tool:"))
                .count()
        };
        assert_eq!(tools(&app.transcript.blocks), 3);
        assert!(
            app.transcript
                .agents()
                .iter()
                .all(|(_, a, _)| tools(&a.log.blocks) == 1)
        );

        // Claude: a blocking call's subagent, and one stopped by the agent.
        let mut app = test_app(HarnessId::CLAUDE);
        let mut p = ClaudeParser::new();
        replay_fixture(
            &mut app,
            "src/harness/claude/fixtures/subagent_blocking.jsonl",
            |l| p.feed(l),
        );
        assert_eq!(reports(&app), vec![(done, "alpha".to_string())]);
        let mut app = test_app(HarnessId::CLAUDE);
        let mut p = ClaudeParser::new();
        replay_fixture(
            &mut app,
            "src/harness/claude/fixtures/subagent_stopped.jsonl",
            |l| p.feed(l),
        );
        assert_eq!(
            reports(&app),
            vec![(Some(SubagentStatus::Cancelled), String::new())]
        );

        // Claude: sent back to work twice, it is one subagent throughout.
        let mut app = test_app(HarnessId::CLAUDE);
        let mut p = ClaudeParser::new();
        replay_fixture(
            &mut app,
            "src/harness/claude/fixtures/subagent_resumed.jsonl",
            |l| p.feed(l),
        );
        let got = reports(&app);
        assert_eq!(got.len(), 1);
        assert!(app.subagents.is_empty());
        assert!(got[0].0 == done && got[0].1.ends_with("**alpha**"));

        // Claude: a subagent that starts one of its own and ends first.
        let mut app = test_app(HarnessId::CLAUDE);
        let mut p = ClaudeParser::new();
        replay_fixture(
            &mut app,
            "src/harness/claude/fixtures/subagent_nested.jsonl",
            |l| p.feed(l),
        );
        assert!(app.subagents.is_empty() && !app.is_generating);
        let runs = app.transcript.agents();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|(_, run, _)| run.duration.is_some()));
        assert_eq!((runs[0].1.status, runs[0].1.report()), (done, "waiting"));
        // The inner one is in the outer one's transcript.
        assert_eq!(runs[1].2, 1);
        assert_eq!(runs[1].1.status, done);
        assert!(runs[1].1.report().contains("a.txt"));

        // Claude: with no `task_notification` at all, a task's end still
        // ends its row (#58).
        let mut app = test_app(HarnessId::CLAUDE);
        let mut p = ClaudeParser::new();
        replay_fixture(
            &mut app,
            "src/harness/claude/fixtures/subagent.jsonl",
            |l| {
                if l.contains(r#""subtype":"task_notification""#) {
                    Vec::new()
                } else {
                    p.feed(l)
                }
            },
        );
        assert!(app.subagents.is_empty());
        let got = reports(&app);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, done);

        // Codex: the sub-agent outlives two turns, and nothing follows its
        // end on the main thread, so its report is only here.
        let mut app = test_app(HarnessId::CODEX);
        let mut p = CodexAppServerParser::new();
        replay_fixture(
            &mut app,
            "src/harness/codex/fixtures/app_server_subagent_background.jsonl",
            |l| p.feed(l),
        );
        assert!(app.subagents.is_empty() && !app.is_generating);
        assert_eq!(reports(&app), vec![(done, "alpha".to_string())]);

        let mut app = test_app(HarnessId::CODEX);
        let mut p = CodexAppServerParser::new();
        replay_fixture(
            &mut app,
            "src/harness/codex/fixtures/app_server_subagent_interrupted.jsonl",
            |l| p.feed(l),
        );
        assert_eq!(
            reports(&app),
            vec![(Some(SubagentStatus::Cancelled), String::new())]
        );
    }

    #[test]
    fn prompts_queue_during_a_turn_and_drain_on_completion() {
        let done = || AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        };
        let mut app = test_app(HarnessId::CLAUDE);
        app.queue_prompt("first".into());
        assert!(app.is_generating && app.queued.is_empty());
        app.take_actions();

        app.queue_prompt("second".into());
        app.queue_prompt("third".into());
        assert_eq!(app.queued.len(), 2);
        assert!(app.take_actions().is_empty());

        // The newest one can be pulled back for editing.
        app.unqueue_last();
        assert_eq!(app.input, "third");
        app.take_input();

        app.session_alive = true;
        app.on_event(done());
        assert!(app.is_generating && app.queued.is_empty());
        assert_eq!(app.take_actions(), vec![Action::turn("second")]);

        // After an interrupt the queue is held until the user presses Enter.
        app.queue_prompt("fourth".into());
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        });
        assert!(!app.is_generating && app.queued.len() == 1);
        assert!(app.send_next_queued());
        assert_eq!(app.take_actions(), vec![Action::turn("fourth")]);
    }

    #[test]
    fn steer_goes_into_the_turn_or_falls_back_to_the_queue() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("go".into());
        app.session_alive = true;
        app.take_actions();
        app.steer("left a bit".into());
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::Steer {
                text: "left a bit".into(),
                attachments: Vec::new()
            })]
        );
        assert!(app.queued.is_empty());

        // Antigravity cannot be steered: the message waits for the turn to end.
        let mut app = test_app(HarnessId::AGY);
        app.submit_prompt("go".into());
        app.session_alive = true;
        app.take_actions();
        app.steer("left a bit".into());
        assert!(app.take_actions().is_empty());
        assert_eq!(app.queued.len(), 1);
    }

    #[test]
    fn compact_needs_an_idle_live_session_that_supports_it() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.compact(None);
        assert!(app.take_actions().is_empty());
        app.session_alive = true;
        app.handle_slash_command("/compact keep the API notes");
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::Compact {
                instructions: Some("keep the API notes".into())
            })]
        );
        assert_eq!(app.status_label(), "Compacting");
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        assert!(!app.is_generating);

        let mut app = test_app(HarnessId::AGY);
        app.session_alive = true;
        app.compact(None);
        assert!(app.take_actions().is_empty());
    }

    #[test]
    fn attachments_go_out_with_the_next_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("my shot.png"), b"png").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), b"txt").unwrap();
        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
        std::fs::write(tmp.path().join("a.out"), [0xff, 0xfe]).unwrap();
        app.handle_slash_command("/attach a.out");
        app.handle_slash_command("/attach missing.png");
        assert!(app.attachments.is_empty());
        app.handle_slash_command("/attach my shot.png");
        assert_eq!(app.attachments.len(), 1);

        app.submit_prompt("what is this?".into());
        assert!(app.attachments.is_empty());
        let sent = app.take_actions().into_iter().find_map(|a| match a {
            Action::SendTurn { attachments, .. } => Some(attachments),
            _ => None,
        });
        assert_eq!(sent.unwrap()[0].label(), "my shot.png");
        assert!(matches!(
            app.transcript.blocks.last(),
            Some(super::super::transcript::Block::User { text }) if text.ends_with("[image: my shot.png]")
        ));

        // A harness that takes no images refuses the attachment.
        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::AGY, None, false);
        app.handle_slash_command("/attach my shot.png");
        assert!(app.attachments.is_empty());
    }

    #[test]
    fn files_are_attached_where_the_harness_takes_them() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), b"txt").unwrap();
        std::fs::write(tmp.path().join("shot.png"), b"png").unwrap();
        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
        app.handle_slash_command("/attach notes.txt");
        app.submit_prompt("summarize".into());
        let sent = app.take_actions().into_iter().find_map(|a| match a {
            Action::SendTurn { attachments, .. } => Some(attachments),
            _ => None,
        });
        assert_eq!(sent.unwrap()[0].mime(), "text/plain");
        assert!(matches!(
            app.transcript.blocks.last(),
            Some(super::super::transcript::Block::User { text }) if text.ends_with("[file: notes.txt]")
        ));

        // Codex takes images only.
        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CODEX, None, false);
        app.handle_slash_command("/attach notes.txt");
        assert!(app.attachments.is_empty());
        // A file that got there anyway (queued under another harness) is
        // dropped at send time, not passed off as an image.
        app.handle_slash_command("/attach shot.png");
        app.attachments
            .push(Attachment::file(tmp.path().join("notes.txt")).unwrap());
        app.submit_prompt("look".into());
        let sent = app.take_actions().into_iter().find_map(|a| match a {
            Action::SendTurn { attachments, .. } => Some(attachments),
            _ => None,
        });
        assert_eq!(sent.unwrap().len(), 1);
    }

    #[test]
    fn exit_summary_only_when_saved() {
        let mut app = test_app(HarnessId::CLAUDE);
        assert!(app.exit_summary().is_none());
        app.submit_prompt("hello".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "s1".into(),
            model: None,
        });
        let line = app.exit_summary().unwrap();
        assert!(line.contains("unharness --resume"));
        assert!(line.contains(&app.conversation.id[..8]));
        assert!(line.contains("(claude)"));
    }

    #[test]
    fn resume_errors_are_reported_not_fatal() {
        use super::super::transcript::Block;
        let tmp = tempfile::tempdir().unwrap();
        let app = test_app_in(tmp.keep(), HarnessId::CLAUDE, Some("nope".into()), false);
        assert!(
            app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Error(e) if e.contains("could not resume")))
        );
        assert_eq!(app.active, HarnessId::CLAUDE);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn submit_starts_session_then_sends_turn() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("hello".into());
        assert!(app.is_generating);
        assert_eq!(
            app.take_actions(),
            vec![Action::StartSession { resume: None }, Action::turn("hello")]
        );
        app.session_alive = true;
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        assert!(!app.is_generating);
        app.submit_prompt("again".into());
        assert_eq!(app.take_actions(), vec![Action::turn("again")]);
    }

    /// The single turn the app asked for.
    fn sent_turn(app: &mut App) -> String {
        match &app.take_actions()[..] {
            [.., Action::SendTurn { text, .. }] => text.clone(),
            other => panic!("{other:?}"),
        }
    }

    /// Claude, live, after a turn too long for a bridge of 500 characters.
    fn over_budget_app() -> App {
        let mut app = test_app(HarnessId::CLAUDE);
        app.bridge_max_chars = Some(500);
        app.submit_prompt("the task".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TextDelta("x".repeat(1_000)));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.take_actions();
        app
    }

    #[test]
    fn no_handoff_is_asked_for_while_subagents_are_at_work() {
        let mut app = over_budget_app();
        app.on_event(AgentEvent::SubagentStarted {
            id: "agent-1".into(),
            description: "look around".into(),
            kind: None,
        });
        app.switch_harness(HarnessId::CODEX);
        assert_eq!(app.active, HarnessId::CODEX);
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    }

    #[test]
    fn prompts_held_before_a_handoff_stay_held_after_it() {
        let mut app = over_budget_app();
        app.submit_prompt("second".into());
        app.queue_prompt("later".into());
        app.interrupt();
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        });
        app.take_actions();
        assert_eq!(app.queued.len(), 1);

        app.switch_harness(HarnessId::CODEX);
        assert!(sent_turn(&mut app).contains("Write a handoff summary"));
        app.on_event(AgentEvent::TextDelta("Goal: the task.".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        assert_eq!(app.active, HarnessId::CODEX);
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert_eq!(app.queued.len(), 1);
    }

    #[test]
    fn no_handoff_is_asked_of_a_session_that_cannot_be_resumed() {
        let mut app = over_budget_app();
        app.session_alive = false;
        app.live_caps
            .entry(HarnessId::CLAUDE)
            .or_default()
            .resume_by_id = Some(false);
        app.switch_harness(HarnessId::CODEX);
        assert_eq!(app.active, HarnessId::CODEX);
        assert!(app.take_actions().is_empty());

        // One that can is resumed for it.
        let mut app = over_budget_app();
        app.session_alive = false;
        app.switch_harness(HarnessId::CODEX);
        assert_eq!(app.active, HarnessId::CLAUDE);
        assert!(matches!(
            &app.take_actions()[..],
            [Action::StartSession { resume: Some(id) }, Action::SendTurn { .. }] if id == "claude-1"
        ));
    }

    #[test]
    fn steering_a_handoff_summary_queues_the_message_for_the_next_harness() {
        use super::super::transcript::Block;
        let mut app = over_budget_app();
        app.switch_harness(HarnessId::CODEX);
        app.take_actions();
        app.steer("and the tests".into());
        assert!(app.take_actions().is_empty());
        app.on_event(AgentEvent::TextDelta("Goal: the task.".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        assert_eq!(app.active, HarnessId::CODEX);
        assert!(
            app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Handoff { text, .. } if text == "Goal: the task."))
        );
        let sent = sent_turn(&mut app);
        assert!(sent.ends_with("and the tests"), "{sent}");
    }

    #[test]
    fn a_handoff_prompt_forbids_tools_for_its_own_reply_only() {
        let ask = handoff_prompt("Codex", 24_000);
        assert!(ask.contains("For this reply only, use no tools and change nothing"));
        assert!(ask.contains("with your tools as before"));
        assert!(!ask.contains("Do not use any tools"));
    }

    #[test]
    fn a_switch_over_the_bridge_budget_asks_for_a_handoff_first() {
        use super::super::transcript::Block;
        use crate::core::conversations::BlockRecord;
        let done = |app: &mut App| {
            app.on_event(AgentEvent::TurnCompleted {
                stop_reason: StopReason::Done,
            })
        };
        let mut app = test_app(HarnessId::CLAUDE);
        app.bridge_max_chars = Some(500);
        app.submit_prompt("the task".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TextDelta("x".repeat(1_000)));
        done(&mut app);

        // Claude writes it before its session is shut down, short enough
        // for the bridge.
        app.switch_harness(HarnessId::CODEX);
        let ask = sent_turn(&mut app);
        assert!(ask.contains("Write a handoff summary for that agent, in at most 50 words"));
        assert_eq!(app.active, HarnessId::CLAUDE);
        // It is not to run anything.
        app.on_event(cargo_test_requests("cargo test").remove(0));
        assert!(app.modal.is_none());
        assert!(matches!(
            &app.take_actions()[..],
            [Action::Command(SessionCommand::RespondPermission {
                decision: PermissionDecision::Deny { .. },
                ..
            })]
        ));
        app.on_event(AgentEvent::TextDelta("Goal: the task. Next: tests.".into()));
        done(&mut app);
        assert_eq!(app.active, HarnessId::CODEX);
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        app.session_alive = false;
        let saved = app.store.load(&app.conversation.id).unwrap();
        assert!(saved.blocks.iter().any(
            |b| matches!(b, BlockRecord::Handoff { to, sender, .. } if to == "Codex" && sender == "Claude")
        ));

        // Codex gets the first prompt and the summary in place of the rest.
        app.submit_prompt("go on".into());
        let sent = sent_turn(&mut app);
        assert!(
            sent.contains("User: the task\n\n[Handoff summary Claude wrote"),
            "{sent}"
        );
        assert!(sent.contains("Goal: the task. Next: tests."));
        assert!(!sent.contains("xxxxx"));
        app.session_alive = true;
        app.on_event(AgentEvent::TextDelta("y".repeat(1_000)));
        done(&mut app);

        // Esc: the switch goes ahead without one.
        app.switch_harness(HarnessId::CLAUDE);
        assert!(sent_turn(&mut app).contains("Write a handoff summary"));
        app.interrupt();
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        });
        assert_eq!(app.active, HarnessId::CLAUDE);
        assert!(app.transcript.blocks.iter().any(
            |b| matches!(b, Block::Notice(n) if n == "no handoff summary; Claude gets the conversation as it is")
        ));
        app.take_actions();
        app.session_alive = false;

        // Off: no summary asked for.
        app.bridge_summary = BridgeSummary::Never;
        app.switch_harness(HarnessId::CODEX);
        assert_eq!(app.active, HarnessId::CODEX);
    }

    #[test]
    fn the_bridge_is_measured_by_the_window_its_harness_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.keep();
        let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
        assert_eq!(
            app.bridge_budget(HarnessId::CLAUDE),
            DEFAULT_BRIDGE_MAX_CHARS
        );
        app.on_event(AgentEvent::Context(ContextUsage {
            used: Some(10),
            window: Some(200_000),
        }));
        // A quarter of it, at four characters a token.
        assert_eq!(app.bridge_budget(HarnessId::CLAUDE), 200_000);
        assert_eq!(
            app.bridge_budget(HarnessId::CODEX),
            DEFAULT_BRIDGE_MAX_CHARS
        );

        // Kept for the next run, by model.
        let mut again = test_app_in(cwd, HarnessId::CLAUDE, None, false);
        assert_eq!(again.bridge_budget(HarnessId::CLAUDE), 200_000);
        again.models.insert(
            HarnessId::CLAUDE,
            ModelRef::new(HarnessId::CLAUDE, ProviderId::new("anthropic"), "opus"),
        );
        assert_eq!(
            again.bridge_budget(HarnessId::CLAUDE),
            DEFAULT_BRIDGE_MAX_CHARS
        );

        // A budget the user set wins.
        again.bridge_max_chars = Some(1_000);
        assert_eq!(again.bridge_budget(HarnessId::CLAUDE), 1_000);
    }

    #[test]
    fn context_windows_are_not_taken_from_the_workspace_or_out_of_range() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.keep();
        // What an agent could write: a window that would empty the bridge.
        std::fs::create_dir_all(cwd.join(".unharness")).unwrap();
        std::fs::write(
            cwd.join(".unharness/context_windows.json"),
            r#"{"claude": {"default": 1}}"#,
        )
        .unwrap();
        let mut app = test_app_in(cwd, HarnessId::CLAUDE, None, false);
        assert_eq!(
            app.bridge_budget(HarnessId::CLAUDE),
            DEFAULT_BRIDGE_MAX_CHARS
        );
        app.on_event(AgentEvent::Context(ContextUsage {
            used: None,
            window: Some(1),
        }));
        assert_eq!(app.bridge_budget(HarnessId::CLAUDE), 8_000);
        app.on_event(AgentEvent::Context(ContextUsage {
            used: None,
            window: Some(u64::MAX),
        }));
        assert_eq!(app.bridge_budget(HarnessId::CLAUDE), 2_000_000);
    }

    #[test]
    fn switching_harness_bridges_context_once() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("first question".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::TextDelta("first answer".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        app.session_alive = false;
        // agy has no `ask`.
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        app.modal = None;

        app.submit_prompt("second question".into());
        let actions = app.take_actions();
        assert!(matches!(actions[0], Action::StartSession { resume: None }));
        match &actions[1] {
            Action::SendTurn { text: t, .. } => {
                assert!(t.contains("first question"));
                assert!(t.contains("Claude: first answer"));
                assert!(t.ends_with("second question"));
            }
            other => panic!("{other:?}"),
        }
        app.session_alive = true;
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        // A second turn on the same harness is not bridged.
        app.submit_prompt("third".into());
        assert_eq!(app.take_actions(), vec![Action::turn("third")]);
    }

    #[test]
    fn policy_resolution_per_harness() {
        // As with `--policy auto`: agy, which has no `auto`, runs the nearest
        // less permissive policy and says so.
        let mut app = test_app(HarnessId::CLAUDE);
        app.policy_explicit = Some(PermissionPolicy::Auto);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
        assert!(app.policy_warning().is_none());
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
        assert!(app.policy_warning().unwrap().contains("less permissive"));

        // A policy chosen in the TUI is for the harness it was chosen on.
        assert!(app.set_policy(PermissionPolicy::Bypass));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.switch_harness(HarnessId::CLAUDE);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
    }

    #[test]
    fn herdr_hears_working_blocked_and_idle() {
        use crate::tui::herdr::State;
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
        assert_eq!(app.herdr_report().state, State::Idle);

        app.is_generating = true;
        assert_eq!(app.herdr_report().state, State::Working);

        // Two requests: one in the modal, one queued behind it.
        let mut requests = cargo_test_requests("cargo test");
        app.on_event(requests.remove(0));
        app.on_event(requests.remove(0));
        let report = app.herdr_report();
        assert_eq!(report.state, State::Blocked);
        assert_eq!(report.message.as_deref(), Some("allow Bash?"));

        // A picker the user opened over the queued one: still waiting.
        app.modal = None;
        app.open_effort_picker();
        app.open_sandbox_picker();
        assert!(!app.pending_prompts.is_empty());
        assert_eq!(app.herdr_report().state, State::Blocked);

        app.modal = None;
        app.pending_prompts.clear();
        assert_eq!(app.herdr_report().state, State::Working);
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        assert_eq!(app.herdr_report().state, State::Idle);

        // A policy picker the user opened is not waiting on anything.
        app.open_policy_picker();
        assert_eq!(app.herdr_report().state, State::Idle);
    }

    #[test]
    fn herdr_hears_a_session_held_for_a_policy_as_blocked() {
        use crate::tui::herdr::State;
        let tmp = tempfile::tempdir().unwrap();
        // agy has no `ask`, and nothing below it.
        let app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
        let report = app.herdr_report();
        assert_eq!(report.state, State::Blocked);
        assert!(report.message.unwrap().contains("permission policy"));
    }

    #[test]
    fn a_policy_that_is_not_available_waits_for_a_choice() {
        let tmp = tempfile::tempdir().unwrap();
        // agy has no `ask`, and nothing below it.
        let mut app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
        assert_eq!(app.effective_policy(), None);
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
        assert!(
            app.status_warning()
                .unwrap()
                .contains("accept-edits, bypass")
        );

        // A prompt is kept, not sent.
        app.modal = None;
        app.submit_prompt("do it".into());
        assert!(app.take_actions().is_empty());
        assert!(!app.is_generating);
        assert!(matches!(app.modal, Some(Modal::Policy(_))));

        // `ask` cannot be chosen: the cursor starts on the least permissive
        // policy there is, and never lands on `ask`.
        let current = |app: &App| match &app.modal {
            Some(Modal::Policy(p)) => p.list.current().copied(),
            _ => None,
        };
        assert_eq!(current(&app), Some(PermissionPolicy::AcceptEdits));
        app.handle_modal_key(key(KeyCode::Up));
        assert_eq!(current(&app), Some(PermissionPolicy::Bypass));
        app.handle_modal_key(key(KeyCode::Down));
        assert_eq!(current(&app), Some(PermissionPolicy::AcceptEdits));
        assert_eq!(app.effective_policy(), None);

        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
        let actions = app.take_actions();
        assert!(matches!(actions[0], Action::StartSession { .. }));
        assert!(matches!(actions[1], Action::SendTurn { .. }));

        // The choice is for this harness; the others keep what was asked for.
        assert_eq!(app.policy_requested(), PermissionPolicy::Ask);
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.switch_harness(HarnessId::CLAUDE);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
        assert!(app.modal.is_none());
    }

    /// The sandbox is unharness's, so the level is for the run, and a
    /// live session comes back under it with the next prompt.
    #[test]
    fn the_sandbox_level_is_chosen_for_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
        app.sandbox = SandboxSetup::null(None);
        assert_eq!(app.sandbox_level().0, SandboxLevel::WorkspaceWrite);

        app.submit_prompt("hello".into());
        app.take_actions();
        app.session_alive = true;
        app.session_sandbox_level = Some(SandboxLevel::WorkspaceWrite);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        // Not during a turn.
        assert!(!app.set_sandbox(SandboxLevel::ReadOnly));
        assert_eq!(app.sandbox_level().0, SandboxLevel::WorkspaceWrite);
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        // The level the session already runs under restarts nothing.
        assert!(app.set_sandbox(SandboxLevel::WorkspaceWrite));
        assert!(app.take_actions().is_empty());
        assert!(app.session_alive);

        assert!(app.set_sandbox(SandboxLevel::ReadOnly));
        assert_eq!(app.sandbox_level().0, SandboxLevel::ReadOnly);
        // The prompt typed with it, before the loop has ended the
        // session, starts the next one.
        app.submit_prompt("again".into());
        let actions = app.take_actions();
        assert_eq!(actions[0], Action::Shutdown);
        assert!(
            matches!(&actions[1], Action::StartSession { resume: Some(id) } if id == "claude-1")
        );
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        // The choice holds across harnesses.
        app.switch_harness(HarnessId::CODEX);
        assert_eq!(app.sandbox_level().0, SandboxLevel::ReadOnly);
        assert!(app.status_warning().is_none());
    }

    #[test]
    fn claude_runs_on_the_provider_it_reports_until_one_is_chosen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
        app.sandbox = SandboxSetup::null(None);
        app.submit_prompt("hello".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        assert_eq!(app.chosen_provider(), None);
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            provider: Some("vertex".into()),
            ..Default::default()
        }));
        assert_eq!(app.current_provider(), Some(&"vertex".into()));
        assert_eq!(app.chosen_provider(), None);

        // Not during a turn: the process is started on its provider.
        app.set_provider("bedrock".into());
        assert_eq!(app.current_provider(), Some(&"vertex".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.take_actions();

        app.set_provider("bedrock".into());
        assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
        // The prompt typed with it, before the loop has ended the
        // session, starts the next one.
        app.submit_prompt("again".into());
        let actions = app.take_actions();
        assert_eq!(actions[0], Action::Shutdown);
        assert!(
            matches!(&actions[1], Action::StartSession { resume: Some(id) } if id == "claude-1")
        );
    }

    #[test]
    fn remote_control_is_offered_only_where_declared() {
        use super::super::transcript::Block;
        // pi runs commands of its own: one called /rc is its.
        let mut app = test_app(HarnessId::PI);
        app.session_alive = true;
        app.handle_slash_command("/rc now");
        assert!(app.remote.is_none());
        assert!(
            app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Notice(n) if n.contains("passed to pi as typed")))
        );
        assert!(
            !app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Notice(n) if n.contains("does not see what")))
        );
        assert!(!app.offers_command("/remote-control"));
        assert!(!app.owns_command("/rc"));
        assert!(app.offers_command("/model"));
        let claude = test_app(HarnessId::CLAUDE);
        assert!(claude.owns_command("/rc"));
    }

    #[test]
    fn remote_control_taken_off_before_it_answers_says_off_once() {
        use super::super::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.handle_slash_command("/rc");
        app.handle_slash_command("/rc off");
        // The answers to both, in order.
        app.on_event(AgentEvent::RemoteControl(RemoteControl::On {
            url: "https://claude.ai/code/session_x".into(),
        }));
        app.on_event(AgentEvent::RemoteControl(RemoteControl::Off));
        let said: Vec<&str> = app
            .transcript
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::System(t) | Block::Notice(t) if t.contains("Remote Control") => {
                    Some(t.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            said,
            [
                "turning on Remote Control for Claude",
                "Remote Control is off"
            ]
        );
        assert_eq!(app.remote_status(), None);
    }

    #[test]
    fn remote_control_waits_for_the_session_and_pauses_with_its_process() {
        let mut app = test_app(HarnessId::CLAUDE);
        // No session yet: nothing is started for it.
        app.handle_slash_command("/rc");
        assert!(app.take_actions().is_empty());
        assert_eq!(app.remote_status().as_deref(), Some("remote: paused"));
        app.session_alive = true;
        assert!(app.remote_control_resend().is_some());
        app.on_event(AgentEvent::RemoteControl(RemoteControl::On {
            url: "https://claude.ai/code/session_x".into(),
        }));
        assert_eq!(app.remote_status().as_deref(), Some("remote"));
        app.on_event(AgentEvent::ProcessExited { code: Some(1) });
        assert_eq!(app.remote_status().as_deref(), Some("remote: paused"));
        assert!(app.remote_control_resend().is_some());
    }

    #[test]
    fn a_resume_ends_remote_control() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
        app.submit_prompt("first question".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.persist();
        let id = app.conversation.id.clone();
        app.handle_slash_command("/rc");
        app.take_actions();
        app.resume_conversation(id);
        let actions = app.take_actions();
        assert_eq!(
            actions.first(),
            Some(&Action::Command(SessionCommand::RemoteControl {
                enabled: false,
                name: None
            }))
        );
        assert_eq!(app.remote_status(), None);
    }

    #[test]
    fn a_remote_first_prompt_leaves_the_bridge_owed_and_titles_the_conversation() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.transcript.push_user("asked on codex");
        app.transcript
            .append_assistant("Codex", "answered on codex");
        app.session_alive = true;
        app.session_blank = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::RemotePrompt {
            text: "typed remotely".into(),
        });
        // The next prompt typed here still tells Claude what it missed.
        assert_eq!(app.bridge_start(HarnessId::CLAUDE), 0);
        assert_eq!(app.first_prompt.as_deref(), Some("typed remotely"));
    }

    #[test]
    fn remote_control_comes_back_on_each_new_process_until_a_switch() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.handle_slash_command("/remote-control on my desk");
        assert_eq!(
            app.take_actions(),
            [Action::Command(SessionCommand::RemoteControl {
                enabled: true,
                name: Some("my desk".into())
            })]
        );
        assert_eq!(app.remote_status().as_deref(), Some("remote: starting"));
        app.on_event(AgentEvent::RemoteControl(RemoteControl::On {
            url: "https://claude.ai/code/session_x".into(),
        }));
        app.on_event(AgentEvent::RemoteControl(RemoteControl::State {
            state: "connected".into(),
            detail: None,
        }));
        assert_eq!(app.remote_status().as_deref(), Some("remote"));
        // A new process (a sandbox change, a resume...) is put on it again.
        assert_eq!(
            app.remote_control_resend(),
            Some(SessionCommand::RemoteControl {
                enabled: true,
                name: Some("my desk".into())
            })
        );
        assert_eq!(app.remote_status().as_deref(), Some("remote: starting"));
        // A switch ends it, telling the remote side first.
        app.switch_now(HarnessId::CODEX);
        let actions = app.take_actions();
        assert_eq!(
            actions[0],
            Action::Command(SessionCommand::RemoteControl {
                enabled: false,
                name: None
            })
        );
        assert_eq!(actions[1], Action::Shutdown);
        assert_eq!(app.remote_status(), None);
        assert_eq!(app.remote_control_resend(), None);
    }

    #[test]
    fn a_failed_remote_control_is_dropped() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.handle_slash_command("/rc");
        app.take_actions();
        app.on_event(AgentEvent::RemoteControl(RemoteControl::Failed {
            reason: "your organization does not allow Remote Control".into(),
        }));
        assert_eq!(app.remote_status(), None);
        assert_eq!(app.remote_control_resend(), None);
    }

    #[test]
    fn a_remote_prompt_is_shown_and_makes_the_session_the_conversations() {
        use super::super::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.session_blank = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::RemotePrompt {
            text: "Reply with the single word: pong".into(),
        });
        assert!(!app.session_blank);
        assert_eq!(
            app.session_ids.get(&HarnessId::CLAUDE).map(String::as_str),
            Some("claude-1")
        );
        assert!(matches!(
            app.transcript.blocks.last(),
            Some(Block::User { text }) if text == "Reply with the single word: pong"
        ));
    }

    #[test]
    fn a_chosen_provider_survives_a_session_that_runs_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
        app.sandbox = SandboxSetup::null(None);
        app.set_provider("bedrock".into());
        app.session_alive = true;
        // Claude's settings chose otherwise.
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            provider: Some("anthropic".into()),
            ..Default::default()
        }));
        assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
        assert!(
            app.status_warning()
                .unwrap()
                .contains("runs on anthropic, not the chosen bedrock")
        );
        // Once it runs where it was told, the warning goes.
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            provider: Some("bedrock".into()),
            ..Default::default()
        }));
        assert!(app.status_warning().is_none());

        // Choosing it again changes nothing, and hides nothing.
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            provider: Some("anthropic".into()),
            ..Default::default()
        }));
        app.set_provider("bedrock".into());
        assert!(app.status_warning().is_some());

        // Not while a subagent is at work: it would end with the process.
        app.on_event(AgentEvent::SubagentStarted {
            id: "t1".into(),
            description: "look around".into(),
            kind: None,
        });
        assert!(!app.subagents.is_empty());
        app.set_provider("vertex".into());
        assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
        assert!(app.take_actions().is_empty());
    }

    /// Claude with a binary that exists wherever the test runs; nothing
    /// starts it.
    fn app_with_a_claude_binary() -> App {
        app_with_a_binary(HarnessId::CLAUDE)
    }

    /// `harness` with a binary that exists wherever the test runs, also
    /// where the CLI is not installed; nothing starts it.
    fn app_with_a_binary(harness: HarnessId) -> App {
        let mut config = Config::default();
        config.harnesses.insert(
            harness.as_str().into(),
            crate::config::HarnessSettings {
                binary: Some("/bin/sh".into()),
                ..Default::default()
            },
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_with(tmp.keep(), harness, None, false, config, test_registry());
        // As in `test_app_in`: a harness without `ask` waits for a choice.
        if app.effective_policy().is_none() {
            app.policy_choice
                .insert(harness, PermissionPolicy::AcceptEdits);
            app.modal = None;
        }
        app
    }

    fn vertex_models() -> ListResult {
        ListResult::Models(Ok(vec![ModelInfo {
            model_ref: ModelRef::new(HarnessId::CLAUDE, "vertex", "haiku"),
            display_name: "Haiku".into(),
            description: None,
            effort_levels: None,
        }]))
    }

    #[test]
    fn completion_asks_for_a_list_once() {
        let mut app = app_with_a_claude_binary();
        app.providers.insert(HarnessId::CLAUDE, "vertex".into());
        let wanted = ListRequest::Models(HarnessId::CLAUDE, "vertex".into());
        app.input = "/model ha".into();
        app.update_suggestions();
        assert_eq!(app.take_list_jobs().len(), 1);
        // A failure does not start it again by itself.
        app.on_list(wanted.clone(), ListResult::Models(Err("timed out".into())));
        app.update_suggestions();
        assert!(app.take_list_jobs().is_empty());
        // The picker does, and what comes is offered.
        app.open_model_picker();
        assert_eq!(app.take_list_jobs().len(), 1);
        app.on_list(wanted, vertex_models());
        assert!(app.suggestions.iter().any(|s| s.0 == "/model haiku"));

        // Nor when the list cannot even be asked for (it was a stack
        // overflow: the failure re-ran completion, which asked again).
        let mut app = test_app(HarnessId::CLAUDE);
        app.sandbox = SandboxSetup {
            explicit: Some(SandboxLevel::ReadOnly),
            backend: Err("no kernel".into()),
        };
        assert!(app.session_sandbox().is_err());
        app.input = "/model x".into();
        app.update_suggestions();
        app.update_suggestions();
        assert!(app.take_list_jobs().is_empty());
    }

    #[test]
    fn a_list_the_user_moved_on_from_does_not_open() {
        let mut app = app_with_a_claude_binary();
        app.providers.insert(HarnessId::CLAUDE, "vertex".into());
        let vertex = ListRequest::Models(HarnessId::CLAUDE, "vertex".into());
        app.open_model_picker();
        app.set_provider("bedrock".into());
        app.modal = None;
        app.on_list(vertex.clone(), vertex_models());
        assert!(app.modal.is_none());

        // Nor over a prompt being typed: it is offered instead.
        app.providers.insert(HarnessId::CLAUDE, "vertex".into());
        app.model_cache.clear();
        app.open_model_picker();
        app.input = "half a prompt".into();
        app.on_list(vertex, vertex_models());
        assert!(app.modal.is_none());
    }

    #[test]
    fn model_lists_arrive_without_holding_the_screen() {
        let mut app = app_with_a_claude_binary();
        app.providers.insert(HarnessId::CLAUDE, "vertex".into());
        let wanted = ListRequest::Models(HarnessId::CLAUDE, "vertex".into());
        app.open_model_picker();
        assert!(app.modal.is_none());
        // Asked once, however often the user presses Ctrl+M meanwhile.
        app.open_model_picker();
        let jobs = app.take_list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].request, wanted);

        // A failure is shown and not kept: the next Ctrl+M asks again.
        app.on_list(wanted.clone(), ListResult::Models(Err("timed out".into())));
        assert!(app.modal.is_none());
        app.open_model_picker();
        assert_eq!(app.take_list_jobs().len(), 1);

        let model = ModelInfo {
            model_ref: ModelRef::new(HarnessId::CLAUDE, "vertex", "haiku"),
            display_name: "Haiku".into(),
            description: None,
            effort_levels: None,
        };
        app.on_list(wanted.clone(), ListResult::Models(Ok(vec![model.clone()])));
        assert!(matches!(app.modal, Some(Modal::Model(_))));
        app.modal = None;
        app.open_model_picker();
        assert!(matches!(app.modal, Some(Modal::Model(_))));
        assert!(app.take_list_jobs().is_empty());

        // One that comes after the user moved on is kept, not opened.
        app.modal = None;
        app.open_provider_picker();
        let providers = ListRequest::Providers(HarnessId::CLAUDE);
        assert_eq!(app.take_list_jobs()[0].request, providers);
        app.switch_harness(HarnessId::CODEX);
        app.on_list(
            providers,
            ListResult::Providers(Ok(vec![("anthropic".into(), "Anthropic".into())])),
        );
        assert!(!matches!(app.modal, Some(Modal::Provider(_))));
    }

    #[test]
    fn an_unavailable_sandbox_level_is_refused() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.sandbox = SandboxSetup {
            explicit: None,
            backend: Err("no kernel".into()),
        };
        assert!(app.status_warning().unwrap().contains("(/sandbox)"));
        assert!(!app.set_sandbox(SandboxLevel::ReadOnly));
        assert_eq!(app.sandbox.explicit, None);

        for c in "/sandbox ".chars() {
            app.insert_char(c);
        }
        let offered: Vec<&str> = app.suggestions.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(offered, ["/sandbox off"]);
        app.set_input("");

        // The picker opens on the level in effect, and the cursor never
        // lands on a level that cannot be had.
        app.open_sandbox_picker();
        let Some(Modal::Sandbox(p)) = &app.modal else {
            panic!("no picker");
        };
        assert_eq!(p.disabled_reason(0), Some("no sandbox here"));
        assert_eq!(p.current(), Some(&SandboxLevel::Off));
        app.handle_modal_key(key(KeyCode::Up));
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.sandbox.explicit, Some(SandboxLevel::Off));
        assert!(app.status_warning().is_none());
    }

    #[test]
    fn a_harness_that_is_not_installed_cannot_be_chosen() {
        let mut app = test_app(HarnessId::CLAUDE);
        for o in &mut app.harness_options {
            o.installed = o.id == HarnessId::CLAUDE;
        }
        app.open_harness_picker();
        let Some(Modal::Harness(p)) = &app.modal else {
            panic!("no picker");
        };
        assert_eq!(p.current().map(|o| o.id), Some(HarnessId::CLAUDE));
        assert!(
            (0..p.items.len())
                .filter(|&i| p.items[i].id != HarnessId::CLAUDE)
                .all(|i| p.disabled_reason(i) == Some("not found on PATH"))
        );
        // Every other row is skipped, so Enter keeps the harness.
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.active, HarnessId::CLAUDE);

        // Nor is it offered or taken by name.
        for c in "/switch ".chars() {
            app.insert_char(c);
        }
        let offered: Vec<&str> = app.suggestions.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(offered, ["/switch claude"]);
        app.set_input("");
        app.handle_slash_command("/switch codex");
        assert_eq!(app.active, HarnessId::CLAUDE);
        assert!(app.transcript.blocks.iter().any(|b| matches!(
            b,
            crate::tui::transcript::Block::Error(e) if e == "codex is not installed (not found on PATH)"
        )));

        // A picker with no rows closes on Enter, as Esc would.
        app.modal = Some(Modal::Effort(ListPicker::new(Vec::new())));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
    }

    #[test]
    fn prompts_held_for_a_policy_keep_their_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
        for text in ["first", "second"] {
            app.modal = None;
            app.submit_prompt(text.into());
            assert!(matches!(app.modal, Some(Modal::Policy(_))));
        }
        assert!(app.take_actions().is_empty());
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        let sent = |app: &mut App| {
            app.take_actions()
                .into_iter()
                .find_map(|a| match a {
                    Action::SendTurn { text, .. } => Some(text),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(sent(&mut app), "first");
        app.session_alive = true;
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        assert_eq!(sent(&mut app), "second");
    }

    #[test]
    fn changing_the_policy_does_not_send_a_held_prompt() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("first".into());
        app.take_actions();
        app.session_alive = true;
        app.queue_prompt("later".into());
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        });
        // Held until the user sends it.
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        assert!(
            app.take_actions()
                .iter()
                .all(|a| !matches!(a, Action::SendTurn { .. }))
        );
        assert_eq!(app.queued.len(), 1);
    }

    #[test]
    fn a_harness_default_policy_stays_with_its_harness() {
        let mut config = Config::default();
        config.harnesses.insert(
            "agy".into(),
            crate::config::HarnessSettings {
                default_policy: Some("bypass".into()),
                ..Default::default()
            },
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_with(
            tmp.keep(),
            HarnessId::AGY,
            None,
            false,
            config,
            test_registry(),
        );
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
        app.switch_harness(HarnessId::CLAUDE);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));

        // A policy set in the TUI is for the harness it was set on.
        app.switch_harness(HarnessId::CLAUDE);
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
        app.switch_harness(HarnessId::CLAUDE);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    }

    #[test]
    fn a_choice_made_for_want_of_a_policy_outlives_changes_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
        assert!(app.set_policy(PermissionPolicy::Bypass));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
        // Other policies are chosen on Claude: agy keeps its own.
        app.switch_harness(HarnessId::CLAUDE);
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        assert!(app.set_policy(PermissionPolicy::Ask));
        app.modal = None;
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
        assert!(app.modal.is_none());
    }

    /// Claude's configured default is `accept-edits`.
    fn accept_edits_config() -> Config {
        let mut config = Config::default();
        config.harnesses.insert(
            "claude".into(),
            crate::config::HarnessSettings {
                default_policy: Some("accept-edits".into()),
                ..Default::default()
            },
        );
        config
    }

    /// Resume the last conversation in `cwd`, with `policy` on the command
    /// line.
    fn resume_with(cwd: &Path, policy: Option<PermissionPolicy>) -> App {
        let mut init = test_init(
            cwd.to_path_buf(),
            HarnessId::CLAUDE,
            Some(String::new()),
            false,
            accept_edits_config(),
            test_registry(),
        );
        init.policy = policy;
        App::new(init)
    }

    /// A turn on Claude under `policy`, then quit.
    fn claude_turn_under(cwd: &Path, policy: Option<PermissionPolicy>) {
        let mut app = resume_with(cwd, None);
        if let Some(p) = policy {
            assert!(app.set_policy(p));
        }
        app.submit_prompt("hi".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.quit();
    }

    #[test]
    fn a_resumed_conversation_keeps_the_policy_it_was_run_with() {
        let tmp = tempfile::tempdir().unwrap();
        claude_turn_under(tmp.path(), Some(PermissionPolicy::Auto));
        let app = resume_with(tmp.path(), None);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
        assert!(!notices(&app).iter().any(|n| n.contains("last ran")));
    }

    #[test]
    fn a_policy_named_on_resume_wins_and_is_told() {
        let tmp = tempfile::tempdir().unwrap();
        claude_turn_under(tmp.path(), Some(PermissionPolicy::Auto));
        let app = resume_with(tmp.path(), Some(PermissionPolicy::Ask));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        assert!(notices(&app).iter().any(|n| n
            == "Claude last ran this conversation under auto; it continues under ask (named with --policy)"));
    }

    #[test]
    fn a_conversation_on_the_default_follows_the_config() {
        let tmp = tempfile::tempdir().unwrap();
        claude_turn_under(tmp.path(), None);
        let app = resume_with(tmp.path(), None);
        assert_eq!(app.conversation.policy, None);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));

        // The default changed since: the conversation follows, and says so.
        let init = test_init(
            tmp.path().to_path_buf(),
            HarnessId::CLAUDE,
            Some(String::new()),
            false,
            Config::default(),
            test_registry(),
        );
        let app = App::new(init);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        assert!(notices(&app).iter().any(|n| n
            == "Claude last ran this conversation under accept-edits; it continues under ask (the configured default)"));
    }

    #[test]
    fn a_saved_policy_the_harness_lacks_is_never_loosened() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(Some(tmp.path()), tmp.path());
        let mut conv = Conversation::new(HarnessId::AGY);
        conv.blocks
            .push(crate::core::conversations::BlockRecord::User { text: "hi".into() });
        conv.sessions.insert(HarnessId::AGY, "agy-1".into());
        conv.policy = Some(PermissionPolicy::Ask);
        store.save(&conv).unwrap();
        let app = resume_with(tmp.path(), None);
        assert_eq!(app.active, HarnessId::AGY);
        assert_eq!(app.effective_policy(), None);
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
    }

    #[test]
    fn resuming_in_the_tui_takes_the_conversations_policy() {
        let tmp = tempfile::tempdir().unwrap();
        claude_turn_under(tmp.path(), Some(PermissionPolicy::Auto));
        let store = ConversationStore::open(Some(tmp.path()), tmp.path());
        let auto = store.last().unwrap().id;
        // One that never named a policy, last run under the default.
        let mut plain = Conversation::new(HarnessId::CLAUDE);
        plain
            .blocks
            .push(crate::core::conversations::BlockRecord::User { text: "x".into() });
        plain
            .last_policies
            .insert(HarnessId::CLAUDE, PermissionPolicy::AcceptEdits);
        store.save(&plain).unwrap();

        let mut app = test_app_with(
            tmp.path().to_path_buf(),
            HarnessId::CLAUDE,
            None,
            false,
            accept_edits_config(),
            test_registry(),
        );
        app.resume_conversation(auto);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
        // What the last one ran under does not follow into the next.
        app.resume_conversation(plain.id.clone());
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
        assert!(!notices(&app).iter().any(|n| n.contains("last ran")));
        app.persist();
        assert_eq!(store.load(&plain.id).unwrap().policy, None);
    }

    #[test]
    fn a_policy_named_in_the_run_holds_over_a_resumed_one() {
        let tmp = tempfile::tempdir().unwrap();
        claude_turn_under(tmp.path(), Some(PermissionPolicy::Bypass));
        let id = ConversationStore::open(Some(tmp.path()), tmp.path())
            .last()
            .unwrap()
            .id;
        let mut app = test_app_with(
            tmp.path().to_path_buf(),
            HarnessId::CLAUDE,
            None,
            false,
            accept_edits_config(),
            test_registry(),
        );
        assert!(app.set_policy(PermissionPolicy::Ask));
        app.resume_conversation(id);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        assert!(notices(&app).iter().any(|n| n
            == "Claude last ran this conversation under bypass; it continues under ask (chosen in this run)"));

        // Named with `--policy`, for every harness: over what the
        // conversation chose for Claude too.
        let app = resume_with(tmp.path(), Some(PermissionPolicy::AcceptEdits));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
        assert!(notices(&app).iter().any(|n| n
            == "Claude last ran this conversation under bypass; it continues under accept-edits (named with --policy)"));
    }

    /// A choice the harness no longer offers is not passed off as what runs.
    #[test]
    fn a_resumed_choice_that_is_not_offered_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(Some(tmp.path()), tmp.path());
        let mut conv = Conversation::new(HarnessId::AGY);
        conv.blocks
            .push(crate::core::conversations::BlockRecord::User { text: "hi".into() });
        conv.policy_choices
            .insert(HarnessId::AGY, PermissionPolicy::Ask);
        conv.last_policies
            .insert(HarnessId::AGY, PermissionPolicy::AcceptEdits);
        store.save(&conv).unwrap();
        let app = resume_with(tmp.path(), None);
        assert!(notices(&app).iter().any(|n| n
            == "Antigravity last ran this conversation under accept-edits; it waits for a policy (ask is not available on Antigravity)"), "{:?}", notices(&app));
    }

    #[test]
    fn a_policy_named_on_resume_holds_over_every_choice_even_the_same() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(Some(tmp.path()), tmp.path());
        let mut conv = Conversation::new(HarnessId::AGY);
        conv.blocks
            .push(crate::core::conversations::BlockRecord::User { text: "hi".into() });
        conv.policy = Some(PermissionPolicy::Ask);
        conv.policy_choices
            .insert(HarnessId::AGY, PermissionPolicy::AcceptEdits);
        conv.policy_choices
            .insert(HarnessId::CLAUDE, PermissionPolicy::Bypass);
        store.save(&conv).unwrap();
        // Resumed as it was: its choices hold.
        let app = resume_with(tmp.path(), None);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
        assert!(app.modal.is_none());
        // `--policy ask`, the policy it was named: `ask` everywhere, so agy
        // waits for a choice and Claude does not run under `bypass`.
        let mut app = resume_with(tmp.path(), Some(PermissionPolicy::Ask));
        assert_eq!(app.policy_choice, HashMap::new());
        assert_eq!(app.effective_policy(), None);
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
        app.modal = None;
        app.switch_harness(HarnessId::CLAUDE);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    }

    /// Resume the last conversation in `cwd` with `model` and `effort` on
    /// the command line, Codex's provider configured as `provider`.
    fn resume_codex_with(
        cwd: &Path,
        model: Option<&str>,
        effort: Option<&str>,
        provider: &str,
    ) -> App {
        let mut config = Config::default();
        config.harnesses.insert(
            "codex".into(),
            crate::config::HarnessSettings {
                default_provider: Some(provider.into()),
                default_model: Some("configured-model".into()),
                ..Default::default()
            },
        );
        let mut init = test_init(
            cwd.to_path_buf(),
            HarnessId::CODEX,
            Some(String::new()),
            false,
            config,
            test_registry(),
        );
        init.model = model.map(Into::into);
        init.effort = effort.map(Into::into);
        App::new(init)
    }

    fn codex_model(app: &App) -> Option<&str> {
        app.models.get(&HarnessId::CODEX).map(|m| m.model.as_str())
    }

    /// A turn on Codex after `/model` and `/effort` where given, then quit.
    fn codex_turn_choosing(cwd: &Path, model: Option<&str>, effort: Option<&str>) {
        let mut app = resume_codex_with(cwd, None, None, "openai");
        if let Some(m) = model {
            app.set_model(m.into());
        }
        if let Some(e) = effort {
            app.set_effort(e.into());
        }
        app.submit_prompt("hi".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "codex-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.quit();
    }

    #[test]
    fn a_resumed_conversation_keeps_the_model_and_effort_chosen_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        codex_turn_choosing(tmp.path(), Some("gpt-chosen"), Some("high"));
        let app = resume_codex_with(tmp.path(), None, None, "openai");
        assert_eq!(codex_model(&app), Some("gpt-chosen"));
        assert_eq!(app.current_effort(), Some("high"));

        // Named on the command line, they win.
        let app = resume_codex_with(tmp.path(), Some("gpt-flag"), Some("low"), "openai");
        assert_eq!(codex_model(&app), Some("gpt-flag"));
        assert_eq!(app.current_effort(), Some("low"));
    }

    #[test]
    fn a_configured_model_is_not_kept_with_the_conversation() {
        let tmp = tempfile::tempdir().unwrap();
        codex_turn_choosing(tmp.path(), None, None);
        let app = resume_codex_with(tmp.path(), None, None, "openai");
        assert!(app.conversation.models.is_empty());
        assert!(app.conversation.efforts.is_empty());
        assert_eq!(codex_model(&app), Some("configured-model"));
    }

    #[test]
    fn a_model_chosen_on_another_provider_is_left() {
        let tmp = tempfile::tempdir().unwrap();
        codex_turn_choosing(tmp.path(), Some("gpt-chosen"), Some("high"));
        let app = resume_codex_with(tmp.path(), None, None, "ollama");
        assert_eq!(codex_model(&app), Some("configured-model"));
        // Its effort was for it.
        assert_eq!(app.current_effort(), None);
        assert!(notices(&app).iter().any(|n| n
            == "the model chosen for Codex in this conversation (openai/gpt-chosen) is not used: Codex is on ollama now"));
    }

    #[test]
    fn resuming_in_the_tui_does_not_carry_one_conversations_model_into_another() {
        let tmp = tempfile::tempdir().unwrap();
        codex_turn_choosing(tmp.path(), Some("gpt-chosen"), Some("high"));
        let store = ConversationStore::open(Some(tmp.path()), tmp.path());
        let chosen = store.last().unwrap().id;
        let mut plain = Conversation::new(HarnessId::CODEX);
        plain
            .blocks
            .push(crate::core::conversations::BlockRecord::User { text: "x".into() });
        store.save(&plain).unwrap();

        let mut app = resume_codex_with(tmp.path(), None, None, "openai");
        app.resume_conversation(chosen.clone());
        assert_eq!(codex_model(&app), Some("gpt-chosen"));
        app.resume_conversation(plain.id.clone());
        assert_eq!(codex_model(&app), Some("configured-model"));
        assert_eq!(app.current_effort(), None);
        app.persist();
        let saved = store.load(&plain.id).unwrap();
        assert!(saved.models.is_empty() && saved.efforts.is_empty());

        // One chosen in the run holds.
        app.set_model("gpt-run".into());
        app.resume_conversation(chosen);
        assert_eq!(codex_model(&app), Some("gpt-run"));
        assert_eq!(app.current_effort(), Some("high"));
    }

    #[test]
    fn codex_exec_has_no_ask() {
        let registry = Arc::new(Registry::empty().with(Box::new(
            crate::harness::codex::CodexHarness::new(crate::harness::codex::CodexTransport::Exec),
        )));
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_with(
            tmp.keep(),
            HarnessId::CODEX,
            None,
            false,
            Config::default(),
            registry,
        );
        assert_eq!(app.effective_policy(), None);
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
        assert!(!app.set_policy(PermissionPolicy::Ask));
        assert!(app.set_policy(PermissionPolicy::Auto));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
    }

    /// One command as each harness asks to run it, through its own parser.
    fn cargo_test_requests(command: &str) -> Vec<AgentEvent> {
        requests_in(command, None)
    }

    /// `cwd`: where Codex and the ACP agent say they would run it (they do
    /// say; Claude does not). `None` leaves it out.
    fn requests_in(command: &str, cwd: Option<&Path>) -> Vec<AgentEvent> {
        use crate::harness::acp::parse::AcpParser;
        use crate::harness::claude::parse::ClaudeParser;
        use crate::harness::codex::app_server_parse::CodexAppServerParser;
        use serde_json::json;
        let claude = json!({"type": "control_request", "request_id": "claude", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "t1",
            "input": {"command": command}}});
        let codex = json!({"method": "item/commandExecution/requestApproval", "id": "codex",
            "params": {"itemId": "i1", "cwd": cwd,
                "command": format!("/usr/bin/zsh -lc '{command}'")}});
        let acp = json!({"jsonrpc": "2.0", "id": "acp", "method": "session/request_permission",
            "params": {"options": [], "toolCall": {"toolCallId": "t2", "name": "exec_command",
                "kind": "execute", "rawInput": {"command": command, "cwd": cwd}}}});
        let mut events = ClaudeParser::new().feed(&claude.to_string());
        events.extend(CodexAppServerParser::new().feed(&codex.to_string()));
        events.extend(AcpParser::new(HarnessId::intern("acp-test")).feed(&acp.to_string()));
        events.retain(|e| matches!(e, AgentEvent::PermissionRequest(_)));
        assert_eq!(events.len(), 3, "{events:?}");
        events
    }

    fn allowed(id: &str) -> Action {
        Action::Command(SessionCommand::RespondPermission {
            id: id.into(),
            decision: PermissionDecision::Allow {
                updated_input: None,
            },
        })
    }

    fn denied(id: &str, reason: &str) -> Action {
        Action::Command(SessionCommand::RespondPermission {
            id: id.into(),
            decision: PermissionDecision::Deny {
                reason: reason.into(),
            },
        })
    }

    /// Claude, live and mid-turn, gone from accept-edits into plan mode.
    fn planning_app() -> App {
        let mut app = test_app(HarnessId::CLAUDE);
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        app.submit_prompt("change the greeting".into());
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        assert!(app.set_policy(PermissionPolicy::Plan));
        app.take_actions();
        app
    }

    fn plan_request() -> AgentEvent {
        AgentEvent::PermissionRequest(PermissionRequest {
            id: "plan".into(),
            kind: PermissionKind::PlanApproval {
                plan: "# Plan\n\nSay hi.".into(),
                plan_file: Some("/p.md".into()),
            },
            tool_call_id: Some("t1".into()),
        })
    }

    #[test]
    fn an_approved_plan_is_carried_out_under_the_policy_chosen_with_it() {
        use PermissionPolicy::*;
        let mut app = planning_app();
        assert_eq!(app.effective_policy(), Some(Plan));
        app.on_event(plan_request());
        let Some(Modal::Plan(m)) = &app.modal else {
            panic!("{:?}", app.modal);
        };
        // Not plan, nor bypass, which Claude cannot switch to here; the
        // one it planned from comes first.
        assert_eq!(m.policies, vec![Ask, AcceptEdits, Auto]);
        assert_eq!(m.current(), Some(AcceptEdits));
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        // The policy before the approval: Claude puts back the mode it
        // planned from only while it still plans.
        assert_eq!(
            app.take_actions(),
            vec![
                Action::Command(SessionCommand::SetPolicy(Auto)),
                allowed("plan")
            ]
        );
        assert_eq!(app.effective_policy(), Some(Auto));
        // Claude then reports the mode it was set to.
        app.on_event(AgentEvent::PolicyChanged(Auto));
        assert_eq!(app.effective_policy(), Some(Auto));

        // A session started under bypass can go back to it.
        let mut app = test_app(HarnessId::CLAUDE);
        assert!(app.set_policy(Bypass));
        app.session_alive = true;
        app.session_policy = Some(Bypass);
        assert!(app.set_policy(Plan));
        app.on_event(plan_request());
        let Some(Modal::Plan(m)) = &app.modal else {
            panic!("{:?}", app.modal);
        };
        assert_eq!(m.policies, vec![Ask, AcceptEdits, Auto, Bypass]);
        assert_eq!(m.current(), Some(Bypass));
    }

    #[test]
    fn a_plan_not_approved_keeps_the_agent_planning() {
        let mut app = planning_app();
        app.on_event(plan_request());
        // From accept-edits up past ask to the last row.
        app.handle_modal_key(key(KeyCode::Up));
        app.handle_modal_key(key(KeyCode::Up));
        app.handle_modal_key(key(KeyCode::Enter));
        for c in "Use Howdy, keep it short".chars() {
            app.handle_modal_key(key(KeyCode::Char(c)));
        }
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_actions(),
            vec![denied("plan", "Use Howdy, keep it short")]
        );
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Plan));

        // Esc is never an approval, and drops what was typed.
        app.on_event(plan_request());
        app.handle_modal_key(key(KeyCode::Up));
        app.handle_modal_key(key(KeyCode::Up));
        app.handle_modal_key(key(KeyCode::Enter));
        app.handle_modal_key(key(KeyCode::Char('x')));
        app.handle_modal_key(key(KeyCode::Esc));
        assert!(app.take_actions().is_empty());
        app.handle_modal_key(key(KeyCode::Esc));
        assert_eq!(app.take_actions(), vec![denied("plan", "")]);

        // Nor is a handoff: the summary is written instead.
        app.handoff = Some(Handoff {
            to: HarnessId::CODEX,
            start: 0,
            held: false,
        });
        app.on_event(plan_request());
        assert!(app.modal.is_none());
        assert!(matches!(
            &app.take_actions()[..],
            [Action::Command(SessionCommand::RespondPermission {
                decision: PermissionDecision::Deny { .. },
                ..
            })]
        ));
    }

    #[test]
    fn while_planning_a_rule_answers_only_a_read() {
        let mut app = planning_app();
        app.rules
            .append(Scope::Workspace, &[Rule::edit("**"), Rule::read("**")])
            .unwrap();
        let request = |id: &str, action: ToolAction| {
            AgentEvent::PermissionRequest(PermissionRequest {
                id: id.into(),
                kind: PermissionKind::ToolUse {
                    tool: "Write".into(),
                    input: Value::Null,
                    action,
                    description: None,
                },
                tool_call_id: None,
            })
        };
        let file = app.cwd.join("notes.txt");
        app.on_event(request("read", ToolAction::Read { path: file.clone() }));
        assert_eq!(app.take_actions(), vec![allowed("read")]);
        app.on_event(request(
            "write",
            ToolAction::Edit {
                paths: vec![file.clone()],
            },
        ));
        assert!(app.take_actions().is_empty());
        assert_eq!(
            app.modal.as_ref().and_then(Modal::request_id),
            Some("write")
        );
        app.close_modal();
        app.take_actions();

        // Out of plan mode the same rule answers it.
        app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Ask));
        app.on_event(request("write", ToolAction::Edit { paths: vec![file] }));
        assert_eq!(app.take_actions(), vec![allowed("write")]);
    }

    /// A request made while planning that waited behind the plan's
    /// approval is still one made while planning.
    #[test]
    fn a_request_made_while_planning_is_judged_so_after_the_approval() {
        let mut app = planning_app();
        app.rules
            .append(Scope::Workspace, &[Rule::edit("**")])
            .unwrap();
        app.on_event(plan_request());
        app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
            id: "write".into(),
            kind: PermissionKind::ToolUse {
                tool: "Write".into(),
                input: Value::Null,
                action: ToolAction::Edit {
                    paths: vec![app.cwd.join("notes.txt")],
                },
                description: None,
            },
            tool_call_id: None,
        }));
        assert_eq!(app.pending_prompts.len(), 1);
        // Approved under accept-edits (highlighted: the policy planned from).
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_actions(),
            vec![
                Action::Command(SessionCommand::SetPolicy(PermissionPolicy::AcceptEdits)),
                allowed("plan")
            ]
        );
        assert_eq!(
            app.modal.as_ref().and_then(Modal::request_id),
            Some("write")
        );
    }

    /// Claude reports each mode it is set to; a report that comes after a
    /// later choice is not the agent changing its mind.
    #[test]
    fn a_late_report_of_a_policy_set_does_not_undo_a_later_one() {
        use crate::tui::transcript::Block;
        let mut app = planning_app();
        assert!(app.set_policy(PermissionPolicy::Ask));
        app.take_actions();
        app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Plan));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Ask));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        assert!(
            !app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Notice(n) if n.contains("plan mode")))
        );
        // Choosing the policy in effect sends nothing to report.
        assert!(app.set_policy(PermissionPolicy::Ask));
        assert!(app.take_actions().is_empty());
        // Then one it did not send is the agent's own.
        app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Plan));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Plan));
    }

    /// The model can go into plan mode by itself (`EnterPlanMode`).
    #[test]
    fn the_agent_going_into_plan_mode_by_itself_is_followed() {
        use crate::tui::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        app.session_alive = true;
        app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Plan));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Plan));
        assert!(
            app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Notice(n) if n.contains("went into plan mode")))
        );
        app.on_event(plan_request());
        let Some(Modal::Plan(m)) = &app.modal else {
            panic!("{:?}", app.modal);
        };
        assert_eq!(m.current(), Some(PermissionPolicy::AcceptEdits));
    }

    #[test]
    fn plan_is_offered_only_where_the_harness_has_it() {
        for harness in [HarnessId::PI, HarnessId::CODEX, HarnessId::AGY] {
            let mut app = test_app(harness);
            assert!(
                !app.offered_policies().contains(&PermissionPolicy::Plan),
                "{harness}"
            );
            assert!(!app.set_policy(PermissionPolicy::Plan), "{harness}");
        }
        let app = test_app(HarnessId::CLAUDE);
        assert_eq!(app.offered_policies()[0], PermissionPolicy::Plan);
    }

    #[test]
    fn an_allow_rule_answers_the_request_of_any_harness_without_asking() {
        use crate::tui::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.rules
            .append(Scope::Workspace, &[Rule::shell("cargo test")])
            .unwrap();
        for ev in cargo_test_requests("cargo test --all") {
            app.on_event(ev);
        }
        assert!(app.modal.is_none());
        assert_eq!(
            app.take_actions(),
            vec![allowed("claude"), allowed("codex"), allowed("acp")]
        );
        let said = app
            .transcript
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Notice(n) if n.contains("`cargo test`")))
            .count();
        assert_eq!(said, 3);

        // Several commands, several rules: all of them are named.
        app.rules
            .append(Scope::Workspace, &[Rule::shell("git push")])
            .unwrap();
        app.on_event(cargo_test_requests("cargo test && git push").remove(0));
        assert_eq!(app.take_actions(), vec![allowed("claude")]);
        assert!(
            app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Notice(n)
            if n.contains("`cargo test`, and for shell commands starting with `git push`")))
        );

        // More than the rule covers is still asked about, one at a time.
        for ev in cargo_test_requests("cargo test && rm -rf x") {
            app.on_event(ev);
        }
        assert_eq!(app.take_actions(), vec![]);
        assert_eq!(
            app.modal.as_ref().and_then(Modal::request_id),
            Some("claude")
        );
        assert_eq!(app.pending_prompts.len(), 2);

        // Run in the workspace it is the rule's command, elsewhere it is not.
        let mut app = test_app(HarnessId::CLAUDE);
        app.rules
            .append(Scope::Workspace, &[Rule::shell("cargo test")])
            .unwrap();
        let cwd = app.cwd.clone();
        for ev in requests_in("cargo test", Some(&cwd)).split_off(1) {
            app.on_event(ev);
        }
        assert_eq!(app.take_actions(), vec![allowed("codex"), allowed("acp")]);
        for ev in requests_in("cargo test", Some(Path::new("/tmp"))).split_off(1) {
            app.on_event(ev);
        }
        assert_eq!(app.take_actions(), vec![]);
        assert_eq!(
            app.modal.as_ref().and_then(Modal::request_id),
            Some("codex")
        );

        // A subagent's request is answered the same way.
        let mut app = test_app(HarnessId::CLAUDE);
        app.rules
            .append(Scope::Global, &[Rule::shell("cargo test")])
            .unwrap();
        let ev = cargo_test_requests("cargo test").remove(0);
        app.on_event(AgentEvent::Sub {
            parent: "agent-1".into(),
            event: Box::new(ev),
        });
        assert_eq!(app.take_actions(), vec![allowed("claude")]);
    }

    fn type_keys(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_modal_key(key(KeyCode::Char(c)));
        }
    }

    fn draft(app: &App) -> &AlwaysDraft {
        match &app.modal {
            Some(Modal::Permission(m)) => m.always.as_ref().expect("the always step"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn allow_always_writes_a_rule_that_the_next_harness_knows() {
        let mut requests = cargo_test_requests("cargo test --all");
        let (acp, codex, claude) = (
            requests.pop().unwrap(),
            requests.pop().unwrap(),
            requests.pop().unwrap(),
        );
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.on_event(claude);
        // A second request for the same thing is already waiting.
        app.on_event(acp);

        // `a` shows what would be allowed and sends nothing yet.
        app.handle_modal_key(key(KeyCode::Char('a')));
        assert_eq!(app.take_actions(), vec![]);
        assert_eq!(draft(&app).rules(), [Rule::shell("cargo test")]);
        assert_eq!(draft(&app).scope, Scope::Workspace);
        // Esc goes back to the request, not out of it.
        app.handle_modal_key(key(KeyCode::Esc));
        assert_eq!(app.take_actions(), vec![]);
        assert!(matches!(&app.modal, Some(Modal::Permission(m)) if m.always.is_none()));

        app.handle_modal_key(key(KeyCode::Char('a')));
        app.handle_modal_key(key(KeyCode::Enter));
        // Claude is told "allow" and nothing more, and the request that was
        // waiting is answered by the new rule.
        assert_eq!(app.take_actions(), vec![allowed("claude"), allowed("acp")]);
        assert!(app.modal.is_none());
        let path = app.rules.path(Scope::Workspace).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("command = \"cargo test\""), "{written}");
        assert!(!app.rules.path(Scope::Global).unwrap().exists());

        // After a switch, Codex asks for the same thing and is not asked about.
        app.on_event(codex);
        assert_eq!(app.take_actions(), vec![allowed("codex")]);
        assert!(app.modal.is_none());

        // The rule is there for the next run.
        let reloaded = Rules::load_in(path.parent().unwrap().parent().unwrap(), Some(&app.cwd));
        assert_eq!(
            reloaded.unwrap().iter().collect::<Vec<_>>(),
            [(Scope::Workspace, &Rule::shell("cargo test"))]
        );
    }

    #[test]
    fn the_rule_can_be_changed_and_moved_before_it_is_kept() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.on_event(cargo_test_requests("cargo test --all").remove(0));
        app.handle_modal_key(key(KeyCode::Char('a')));

        // Widened to every cargo command, for every workspace.
        for _ in 0.." test".len() {
            app.handle_modal_key(key(KeyCode::Backspace));
        }
        app.handle_modal_key(key(KeyCode::Tab));
        assert_eq!(draft(&app).rules(), [Rule::shell("cargo")]);
        assert_eq!(draft(&app).scope, Scope::Global);

        // Ctrl+C is not a letter of the pattern.
        app.handle_modal_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(draft(&app).rules(), [Rule::shell("cargo")]);

        // A pattern that would not have allowed this request is refused.
        type_keys(&mut app, " build");
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(app.take_actions(), vec![]);
        assert!(
            draft(&app)
                .problem
                .as_deref()
                .unwrap()
                .contains("not cover")
        );
        // So is one that is no rule at all.
        type_keys(&mut app, " && ls");
        assert!(draft(&app).problem.is_none());
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(app.take_actions(), vec![]);
        assert!(draft(&app).problem.is_some());

        for _ in 0.." build && ls".len() {
            app.handle_modal_key(key(KeyCode::Backspace));
        }
        // A paste goes into the pattern too.
        app.paste(" test");
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(app.take_actions(), vec![allowed("claude")]);
        assert_eq!(
            app.rules.iter().collect::<Vec<_>>(),
            [(Scope::Global, &Rule::shell("cargo test"))]
        );
        assert!(!app.rules.path(Scope::Workspace).unwrap().exists());
    }

    /// pi's gate asking about `command`, as dialog `id`.
    fn pi_gate_request(id: &str, command: &str) -> AgentEvent {
        use crate::harness::pi::parse::PiParser;
        let call = serde_json::json!({"toolCallId": format!("call-{id}"), "toolName": "bash",
            "input": {"command": command}});
        let line = serde_json::json!({"type": "extension_ui_request", "id": id,
            "method": "select", "title": format!("unharness-gate:{call}"),
            "options": ["Allow", "Deny"]});
        let mut events = PiParser::new(None).feed(&line.to_string());
        assert!(
            matches!(events.as_slice(), [AgentEvent::PermissionRequest(_)]),
            "{events:?}"
        );
        events.remove(0)
    }

    #[test]
    fn a_request_behind_a_picker_opens_when_the_picker_closes() {
        let mut app = test_app(HarnessId::PI);
        app.session_alive = true;
        app.start_generation();
        for close in [KeyCode::Esc, KeyCode::Enter] {
            app.open_policy_picker();
            app.on_event(pi_gate_request("g1", "cq query"));
            // It waits, and the user is told so.
            assert!(matches!(app.modal, Some(Modal::Policy(_))));
            assert_eq!(app.status_label(), "Waiting for you");
            app.handle_modal_key(key(close));
            assert_eq!(app.modal.as_ref().and_then(Modal::request_id), Some("g1"));
            app.handle_modal_key(key(KeyCode::Char('y')));
            // After the policy Enter chose, if any.
            assert!(app.take_actions().ends_with(&[allowed("g1")]));
            assert!(!app.awaiting_answer());
        }
    }

    #[test]
    fn a_request_of_a_session_the_picker_ends_goes_with_it() {
        let mut app = test_app(HarnessId::PI);
        app.session_alive = true;
        app.open_sandbox_picker();
        // Between turns: an extension's dialog, say.
        app.on_event(pi_gate_request("g1", "ls"));
        if let Some(Modal::Sandbox(p)) = &mut app.modal {
            p.selected = p
                .items
                .iter()
                .position(|l| *l == SandboxLevel::Off)
                .unwrap();
        }
        app.handle_modal_key(key(KeyCode::Enter));
        // The session restarts under the new level; its request is not
        // put to the user, since nothing could take the answer.
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert!(app.modal.is_none() && !app.awaiting_answer());
    }

    #[test]
    fn a_withdrawn_request_closes_unanswered() {
        use crate::tui::transcript::Block;
        let mut app = test_app(HarnessId::PI);
        app.session_alive = true;
        app.start_generation();
        app.on_event(pi_gate_request("g1", "echo one"));
        app.on_event(pi_gate_request("g2", "echo two"));
        app.on_event(pi_gate_request("g3", "echo three"));
        // One waiting behind the open one goes quietly.
        app.on_event(AgentEvent::PermissionWithdrawn { id: "g2".into() });
        assert_eq!(app.modal.as_ref().and_then(Modal::request_id), Some("g1"));
        // The open one closes, and the next comes up.
        app.on_event(AgentEvent::PermissionWithdrawn { id: "g1".into() });
        assert_eq!(app.modal.as_ref().and_then(Modal::request_id), Some("g3"));
        app.on_event(AgentEvent::PermissionWithdrawn { id: "g3".into() });
        assert!(app.modal.is_none() && !app.awaiting_answer());
        // Nothing was answered for the user.
        assert_eq!(app.take_actions(), vec![]);
        let said = app
            .transcript
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Notice(n) if n.contains("stopped waiting")))
            .count();
        assert_eq!(said, 2);
    }

    #[test]
    fn an_interrupt_the_turn_ignores_ends_the_process() {
        use crate::core::guard::{Guarded, Watch};
        use crate::tui::transcript::Block;
        let mut app = test_app(HarnessId::PI);
        app.session_alive = true;
        app.session_ids.insert(HarnessId::PI, "pi-1".into());
        let config = tempfile::tempdir().unwrap();
        app.guard = Some(Watch::begin(
            &[Guarded::Tree(config.path().to_path_buf())],
            None,
        ));
        app.submit_prompt("look".into());
        app.take_actions();
        app.on_event(AgentEvent::ToolCallStarted {
            id: "t1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "cq query"}),
        });
        app.interrupt();
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::Interrupt)]
        );
        // Asked again within the grace: said once, nothing sent.
        app.interrupt();
        app.interrupt();
        assert_eq!(app.take_actions(), vec![]);
        let notices = |app: &App, start: &str| {
            app.transcript
                .blocks
                .iter()
                .filter(|b| matches!(b, Block::Notice(n) if n.starts_with(start)))
                .count()
        };
        assert_eq!(notices(&app, "waiting for"), 1);
        assert!(app.is_generating);

        // After it, the process is ended and the turn with it.
        std::fs::write(config.path().join("extension.ts"), "x").unwrap();
        app.on_event(pi_gate_request("g1", "ls"));
        app.interrupted = Some((Instant::now() - STOP_GRACE, true));
        app.interrupt();
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert!(!app.is_generating && !app.session_alive && !app.awaiting_answer());
        assert!(matches!(
            app.transcript
                .blocks
                .iter()
                .find(|b| matches!(b, Block::Tool { .. })),
            Some(Block::Tool {
                done: true,
                is_error: true,
                ..
            })
        ));
        // What the turn changed in the harness's configuration is told.
        assert!(app.transcript.blocks.iter().any(
            |b| matches!(b, Block::Error(e) if e.contains("configuration changed while it ran"))
        ));
        // The next prompt resumes the session in a new process.
        app.submit_prompt("go on".into());
        assert_eq!(
            app.take_actions(),
            vec![
                Action::StartSession {
                    resume: Some("pi-1".into())
                },
                Action::turn("go on")
            ]
        );
        // Its anchor is its own: the ended turn's will never come.
        app.on_event(AgentEvent::TurnAnchor { id: "e2".into() });
        let go_on = app
            .transcript
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::User { text } if text == "go on"));
        assert_eq!(app.anchors.last().map(|a| a.block), go_on);
        // A turn that ends when asked leaves nothing behind.
        app.interrupt();
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        });
        assert!(app.interrupted.is_none());
    }

    #[test]
    fn an_interrupt_long_after_the_first_warns_before_ending_the_process() {
        use crate::tui::transcript::Block;
        let mut app = test_app(HarnessId::PI);
        app.session_alive = true;
        app.submit_prompt("look".into());
        app.take_actions();
        app.interrupt();
        app.take_actions();
        app.interrupted = Some((Instant::now() - STOP_GRACE, false));
        app.interrupt();
        assert_eq!(app.take_actions(), vec![]);
        assert!(app.is_generating);
        assert!(app.transcript.blocks.iter().any(
            |b| matches!(b, Block::Notice(n) if n.ends_with("asked again, its process is ended"))
        ));
        app.interrupt();
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    }

    #[test]
    fn a_request_no_rule_can_cover_says_so_and_stays_a_question() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.on_event(cargo_test_requests("cargo test > log").remove(0));
        app.handle_modal_key(key(KeyCode::Char('a')));
        assert_eq!(draft(&app).rules(), []);
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(app.take_actions(), vec![]);
        app.handle_modal_key(key(KeyCode::Esc));
        app.handle_modal_key(key(KeyCode::Char('y')));
        assert_eq!(app.take_actions(), vec![allowed("claude")]);
        assert_eq!(app.rules.iter().count(), 0);
    }

    #[test]
    fn a_rule_that_cannot_be_saved_still_allows_this_once() {
        use crate::tui::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        // Something that is not a rules file is in the way.
        let path = app.rules.path(Scope::Workspace).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not toml [").unwrap();
        app.on_event(cargo_test_requests("cargo test").remove(0));
        app.handle_modal_key(key(KeyCode::Char('a')));
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(app.take_actions(), vec![allowed("claude")]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not toml [");
        assert!(
            app.transcript
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Error(e) if e.contains("could not be saved")))
        );
        app.on_event(cargo_test_requests("cargo test").remove(0));
        assert!(app.modal.is_some());
    }

    #[test]
    fn allow_lists_the_rules_and_where_they_are_kept() {
        use crate::tui::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        app.handle_slash_command("/allow");
        app.rules
            .append(Scope::Workspace, &[Rule::edit("src/**")])
            .unwrap();
        app.rules
            .append(Scope::Global, &[Rule::mcp("probe/*")])
            .unwrap();
        app.handle_slash_command("/allow");
        let said: Vec<&str> = app
            .transcript
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::System(text) if text.contains("without asking") => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(said[0].contains("Nothing is allowed without asking yet"));
        assert!(said[0].contains("allow.toml"));
        assert!(
            said[1].contains("• [workspace] edits to `src/**`"),
            "{}",
            said[1]
        );
        assert!(said[1].contains("• [global] every tool of the MCP server `probe`"));
        assert!(said[1].contains(".allow.toml"));
    }

    #[test]
    fn permission_modal_allow_and_deny_with_reason() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        let req = |id: &str| PermissionRequest {
            id: id.into(),
            kind: PermissionKind::ToolUse {
                tool: "Bash".into(),
                input: serde_json::json!({"command":"ls"}),
                action: crate::core::ToolAction::Opaque,
                description: None,
            },
            tool_call_id: None,
        };
        app.on_event(AgentEvent::PermissionRequest(req("r1")));
        app.on_event(AgentEvent::PermissionRequest(req("r2")));
        assert!(matches!(app.modal, Some(Modal::Permission(_))));

        app.handle_modal_key(key(KeyCode::Char('y')));
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::RespondPermission {
                id: "r1".into(),
                decision: PermissionDecision::Allow {
                    updated_input: None
                }
            })]
        );
        assert_eq!(app.modal.as_ref().and_then(Modal::request_id), Some("r2"));
        app.handle_modal_key(key(KeyCode::Char('n')));
        for c in "nope".chars() {
            app.handle_modal_key(key(KeyCode::Char(c)));
        }
        app.handle_modal_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::RespondPermission {
                id: "r2".into(),
                decision: PermissionDecision::Deny {
                    reason: "nope".into()
                }
            })]
        );
        assert!(app.modal.is_none());
    }

    #[test]
    fn question_modal_answers() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
            id: "q".into(),
            kind: PermissionKind::Question {
                questions: vec![Question {
                    id: "Color?".into(),
                    header: "Color".into(),
                    text: "Color?".into(),
                    options: vec![
                        QuestionOption::new("Red", ""),
                        QuestionOption::new("Blue", ""),
                    ],
                    allow_other: true,
                    multi: false,
                }],
            },
            tool_call_id: None,
        }));
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter));
        match app.take_actions().pop() {
            Some(Action::Command(SessionCommand::RespondPermission { decision, .. })) => {
                assert_eq!(
                    decision,
                    PermissionDecision::Answer(serde_json::json!({"Color?":"Blue"}))
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn several_questions_send_only_from_the_submit_page() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        let question = |id: &str| Question {
            id: id.into(),
            header: id.into(),
            text: id.into(),
            options: vec![
                QuestionOption::new("Yes", ""),
                QuestionOption::new("No", ""),
            ],
            allow_other: true,
            multi: false,
        };
        app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
            id: "q".into(),
            kind: PermissionKind::Question {
                questions: vec![question("One"), question("Two")],
            },
            tool_call_id: None,
        }));
        app.handle_modal_key(key(KeyCode::Enter)); // One = Yes
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter)); // Two = No, now on Submit
        assert!(app.take_actions().is_empty());
        app.handle_modal_key(key(KeyCode::BackTab));
        match &app.modal {
            Some(Modal::Question(m)) => assert_eq!((m.idx, m.cursor), (1, 1)),
            other => panic!("{other:?}"),
        }
        assert!(app.take_actions().is_empty());
        app.handle_modal_key(key(KeyCode::Tab));
        app.handle_modal_key(key(KeyCode::Enter));
        match app.take_actions().pop() {
            Some(Action::Command(SessionCommand::RespondPermission { decision, .. })) => {
                assert_eq!(
                    decision,
                    PermissionDecision::Answer(serde_json::json!({"One": "Yes", "Two": "No"}))
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(app.modal.is_none());
    }

    #[test]
    fn local_picker_enter_and_esc() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.open_policy_picker();
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.wanted_policy(), PermissionPolicy::AcceptEdits);
        app.open_effort_picker();
        app.handle_modal_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.open_harness_picker();
        app.handle_modal_key(key(KeyCode::Char('q')));
        assert!(app.modal.is_none());
    }

    fn index(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn at_completes_a_file_where_the_cursor_is() {
        let mut app = test_app(HarnessId::CODEX);
        app.insert_str("see  for details");
        app.cursor = 4;
        app.insert_char('@');
        // The first `@` asks for the files; there are none to show yet.
        assert_eq!(app.completing_file, Some(4));
        assert!(app.suggestions.is_empty());
        assert_eq!(app.take_file_index_request(), Some(app.cwd.clone()));
        app.insert_char('m');
        assert_eq!(app.take_file_index_request(), None);

        app.set_file_index(index(&["README.md", "src/main.rs", "src/tui/mod.rs"]));
        let listed: Vec<&str> = app.suggestions.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(listed.len(), 3);
        app.insert_char('a');
        assert_eq!(app.suggestions[0].0, "src/main.rs");
        assert!(app.should_accept_suggestion());

        app.accept_suggestion();
        // Whitespace followed already: none is added.
        assert_eq!(app.input, "see src/main.rs for details");
        assert_eq!(app.cursor, 15);
        assert!(app.suggestions.is_empty() && app.completing_file.is_none());

        // At the end of the input a space follows the path.
        app.take_input();
        app.insert_str("read ");
        app.insert_char('@');
        assert!(app.take_file_index_request().is_some());
        for c in "read".chars() {
            app.insert_char(c);
        }
        app.accept_suggestion();
        assert_eq!(app.input, "read README.md ");
        assert_eq!(app.cursor, 15);
    }

    #[test]
    fn file_list_keeps_to_typed_at_words() {
        let mut app = test_app(HarnessId::CODEX);
        app.set_file_index(index(&["a.rs", "b.rs", "c.rs"]));

        // An address is not a file, and a slash command keeps its own list.
        for c in "git@a".chars() {
            app.insert_char(c);
        }
        assert!(app.suggestions.is_empty());
        app.set_input("/");
        app.insert_char('@');
        assert!(app.completing_file.is_none());

        // Pasted text and text from the editor open no list.
        app.take_input();
        app.take_file_index_request();
        app.set_file_index(index(&["a.rs", "b.rs", "c.rs"]));
        app.insert_str("@a");
        assert!(app.suggestions.is_empty());
        app.set_input("look at @a");
        assert!(app.suggestions.is_empty());
        // Nor do they have the files listed.
        assert_eq!(app.take_file_index_request(), None);

        // Typing on does, and the cursor leaving the word closes it.
        app.insert_char('.');
        assert_eq!(app.suggestions.len(), 1);
        app.move_cursor_left();
        assert!(app.suggestions.is_empty() && app.completing_file.is_none());
        app.move_cursor_end();
        app.delete_backwards();
        assert_eq!(app.suggestions.len(), 1);
        app.take_input();
        assert!(app.suggestions.is_empty() && app.completing_file.is_none());
    }

    #[test]
    fn new_file_list_leaves_the_selection_on_its_file() {
        let mut app = test_app(HarnessId::CODEX);
        app.set_file_index(index(&["a.rs", "b.rs"]));
        app.insert_char('@');
        app.suggestion_down();
        assert_eq!(app.suggestions[app.selected_suggestion].0, "b.rs");

        app.set_file_index(index(&["0.rs", "a.rs", "b.rs"]));
        assert_eq!(app.suggestions.len(), 3);
        assert_eq!(app.suggestions[app.selected_suggestion].0, "b.rs");
        app.set_file_index(index(&["a.rs"]));
        assert_eq!(app.selected_suggestion, 0);

        // A path typed out in full is still put into the harness's form.
        for c in "a.rs".chars() {
            app.insert_char(c);
        }
        assert!(app.should_accept_suggestion());
        app.accept_suggestion();
        assert_eq!(app.input, "a.rs ");
    }

    #[test]
    fn file_list_survives_the_cursor_and_the_walk_going_astray() {
        let mut app = test_app(HarnessId::CODEX);
        app.prompt_width = 40;
        // Up from a word no file matches moves the cursor before its `@`.
        app.insert_str("hello\n");
        for c in "@zzz".chars() {
            app.insert_char(c);
        }
        assert!(app.move_cursor_up());
        assert!(app.completing_file.is_none());
        assert!(!app.should_accept_suggestion());

        // The rest of a word edited in its middle goes with it.
        app.take_input();
        app.set_file_index(index(&["src/main.rs"]));
        app.insert_str("see @x/tui now");
        app.cursor = 6;
        app.delete_backwards();
        app.insert_char('m');
        assert_eq!(app.input, "see @m/tui now");
        app.accept_suggestion();
        assert_eq!(app.input, "see src/main.rs now");

        // An `@` typed while the files are being listed has them listed
        // again afterwards; a listing that failed does not end that.
        app.take_input();
        app.insert_char('@');
        assert!(app.take_file_index_request().is_some());
        app.take_input();
        app.insert_char('@');
        assert_eq!(app.take_file_index_request(), None);
        app.file_walk_failed();
        assert!(app.take_file_index_request().is_some());
        app.set_file_index(index(&["a.rs"]));
        assert_eq!(app.take_file_index_request(), None);
        assert_eq!(app.suggestions.len(), 1);
    }

    #[test]
    fn a_file_is_written_the_way_the_active_harness_reads_it() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.set_file_index(index(&["src/main.rs", "my notes/a.txt"]));
        for c in "@main".chars() {
            app.insert_char(c);
        }
        app.accept_suggestion();
        assert_eq!(app.input, "@src/main.rs ");

        // Typed out in the harness's form already: Enter is free to send.
        app.take_input();
        for c in "@src/main.rs".chars() {
            app.insert_char(c);
        }
        assert!(!app.should_accept_suggestion());

        app.take_input();
        for c in "@notes".chars() {
            app.insert_char(c);
        }
        app.accept_suggestion();
        assert_eq!(app.input, "@\"my notes/a.txt\" ");

        app.take_input();
        app.switch_harness(HarnessId::PI);
        for c in "@main".chars() {
            app.insert_char(c);
        }
        app.accept_suggestion();
        assert_eq!(app.input, "src/main.rs ");
    }

    /// pi has `ask` and `bypass` only: the others are listed and cannot
    /// be chosen, from the picker, `/policy` or completion.
    #[test]
    fn a_policy_the_harness_lacks_cannot_be_chosen() {
        let mut app = test_app(HarnessId::PI);
        assert_eq!(
            app.offered_policies(),
            vec![PermissionPolicy::Ask, PermissionPolicy::Bypass]
        );
        app.open_policy_picker();
        let Some(Modal::Policy(p)) = &app.modal else {
            panic!("no picker");
        };
        assert_eq!(p.list.items.len(), 5);
        assert_eq!(p.list.disabled_reason(0), Some("not offered by pi"));
        assert_eq!(p.list.disabled_reason(2), Some("not offered by pi"));
        assert_eq!(p.list.disabled_reason(3), Some("not offered by pi"));
        assert_eq!(p.list.current(), Some(&PermissionPolicy::Ask));
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));

        // Not `ask` in its place: an error, and nothing changes.
        app.handle_slash_command("/policy auto");
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
        assert!(app.transcript.blocks.iter().any(
            |b| matches!(b, crate::tui::transcript::Block::Error(e) if e.contains("'auto'") && e.contains("ask, bypass"))
        ));

        for c in "/policy a".chars() {
            app.insert_char(c);
        }
        let offered: Vec<&str> = app.suggestions.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(offered, ["/policy ask"]);
    }

    /// `d` in the policy picker saves the selected policy as the harness's
    /// default, in this workspace's settings or (Tab) the global ones, both
    /// under the config directory, and chooses it.
    #[test]
    fn the_policy_picker_saves_a_harness_default() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.keep(), HarnessId::PI, None, false);
        let dir = app.cwd.join(".unharness/test-config");
        let saving = |app: &App| match &app.modal {
            Some(Modal::Policy(p)) => p.save,
            _ => None,
        };

        // This workspace: Esc goes back to the list, nothing is written.
        app.open_policy_picker();
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Char('d')));
        assert_eq!(saving(&app), Some(Scope::Workspace));
        app.handle_modal_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Some(Modal::Policy(_))) && saving(&app).is_none());
        app.handle_modal_key(key(KeyCode::Char('d')));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
        let local = Config::scoped_path(&dir, Scope::Workspace, Some(&app.cwd)).unwrap();
        let saved = Config::load_file(&local).unwrap().unwrap();
        assert_eq!(
            saved.harnesses["pi"].default_policy.as_deref(),
            Some("bypass")
        );
        assert_eq!(
            crate::runner::configured_policy(&app.config, HarnessId::PI).unwrap(),
            PermissionPolicy::Bypass
        );
        assert!(!dir.join("config.toml").exists());

        // Every workspace, while this one's settings name another: saved,
        // and the user told which holds here.
        app.open_policy_picker();
        app.handle_modal_key(key(KeyCode::Up));
        app.handle_modal_key(key(KeyCode::Char('d')));
        app.handle_modal_key(key(KeyCode::Tab));
        assert_eq!(saving(&app), Some(Scope::Global));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        let global = Config::load_file(&dir.join("config.toml"))
            .unwrap()
            .unwrap();
        assert_eq!(
            global.harnesses["pi"].default_policy.as_deref(),
            Some("ask")
        );
        assert!(
            notices(&app)
                .iter()
                .any(|n| n == "this workspace's settings make bypass pi's default here")
        );
        assert_eq!(
            crate::runner::configured_policy(&app.config, HarnessId::PI).unwrap(),
            PermissionPolicy::Bypass
        );

        // A row that cannot be chosen cannot be saved either.
        app.open_policy_picker();
        if let Some(Modal::Policy(p)) = &mut app.modal {
            p.list.selected = 2;
        }
        app.handle_modal_key(key(KeyCode::Char('d')));
        assert!(saving(&app).is_none());
    }

    /// A harness's own default comes before a workspace's `default_policy`:
    /// one saved for every workspace says so where that gives way.
    #[test]
    fn saving_over_a_workspace_default_for_every_harness_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
        let dir = app.cwd.join(".unharness/test-config");
        let local = Config::scoped_path(&dir, Scope::Workspace, Some(&app.cwd)).unwrap();
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "default_policy = \"ask\"\n").unwrap();
        assert!(app.save_default_policy(PermissionPolicy::Bypass, Scope::Global));
        assert!(notices(&app).iter().any(|n| {
            n.starts_with("this workspace's default_policy (ask) no longer applies to Claude")
        }));
        // Not when it is the same.
        let before = notices(&app).len();
        assert!(app.save_default_policy(PermissionPolicy::Ask, Scope::Global));
        assert_eq!(notices(&app).len(), before);
    }

    /// A wanted policy that runs as a less permissive one: the cursor is on
    /// the one that runs.
    #[test]
    fn the_policy_picker_starts_on_the_policy_in_effect() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
        assert!(app.set_policy(PermissionPolicy::Auto));
        app.switch_harness(HarnessId::PI);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        app.open_policy_picker();
        assert!(
            matches!(&app.modal, Some(Modal::Policy(p)) if p.list.current() == Some(&PermissionPolicy::Ask))
        );
    }

    #[test]
    fn slash_commands_and_suggestions() {
        let mut app = test_app(HarnessId::CLAUDE);
        // Whatever is on this machine's PATH.
        for o in &mut app.harness_options {
            o.installed = true;
        }
        for c in "/pol".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.suggestions.len(), 1);
        app.accept_suggestion();
        assert_eq!(app.input, "/policy");
        app.insert_char(' ');
        app.insert_char('b');
        assert_eq!(app.suggestions[0].0, "/policy bypass");
        app.set_input("");
        // Levels are offered where there is a backend for them.
        app.sandbox = SandboxSetup::null(Some(SandboxLevel::Off));
        for c in "/sandbox r".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.suggestions[0].0, "/sandbox read-only");
        app.handle_slash_command("/sandbox bogus");
        assert_eq!(app.sandbox.explicit, Some(SandboxLevel::Off));
        app.handle_slash_command("/sandbox");
        assert!(matches!(app.modal, Some(Modal::Sandbox(_))));
        app.modal = None;

        app.handle_slash_command("/effort xhigh");
        assert_eq!(app.current_effort(), Some("xhigh"));
        app.handle_slash_command("/effort bogus");
        assert_eq!(app.current_effort(), Some("xhigh"));
        app.handle_slash_command("/model sonnet");
        assert_eq!(app.model_label(), "anthropic/sonnet");
        app.handle_slash_command("/switch codex");
        assert_eq!(app.active, HarnessId::CODEX);
        app.handle_slash_command("/quit");
        assert!(app.should_quit);
    }

    /// Claude, its session having listed `/hello`, `/clear` and `/model`.
    fn app_with_claude_commands() -> App {
        let mut app = test_app(HarnessId::CLAUDE);
        let command = |name: &str, hint: Option<&str>| {
            HarnessCommand::new(name, Some(&format!("{name} it\nmore")), hint).unwrap()
        };
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            commands: Some(vec![
                command("hello", Some("<name>")),
                HarnessCommand {
                    aliases: vec!["new".into(), "reset".into()],
                    ..command("clear", None)
                },
                command("model", None),
                command("Deploy", None),
            ]),
            ..Default::default()
        }));
        app
    }

    fn notices(app: &App) -> Vec<String> {
        use super::super::transcript::Block;
        app.transcript
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Notice(n) | Block::Error(n) => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_command_unharness_lacks_goes_to_a_harness_that_runs_its_own() {
        let mut app = app_with_claude_commands();
        app.handle_slash_command("/hello world");
        assert_eq!(
            app.take_actions(),
            vec![
                Action::StartSession { resume: None },
                Action::turn("/hello world")
            ]
        );
        assert_eq!(
            notices(&app),
            vec!["/hello is Claude's own command: passed on"]
        );
        app.session_alive = true;
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        // One it did not list goes too, as typed: Claude passes an unknown
        // one to the model.
        app.handle_slash_command("/etc/hosts is it there?");
        assert_eq!(
            app.take_actions(),
            vec![Action::turn("/etc/hosts is it there?")]
        );
        assert!(
            notices(&app)
                .last()
                .unwrap()
                .contains("passed to Claude as typed")
        );
    }

    #[test]
    fn unharness_s_own_command_wins_unless_escaped() {
        let mut app = app_with_claude_commands();
        app.submit_prompt("before".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.handle_slash_command("/clear");
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert_eq!(app.transcript.blocks.len(), 1, "a new conversation");
        app.session_alive = true;
        app.pass_command("/clear", true);
        assert_eq!(app.take_actions(), vec![Action::turn("/clear")]);
        let said = notices(&app);
        assert_eq!(said[0], "/clear is Claude's own command: passed on");
        assert!(said[1].starts_with("unharness does not see what Claude's /clear changes"));
        assert!(said[1].ends_with("(unharness's /clear starts a new conversation)"));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        // `/new` is Claude's `/clear` under another name.
        app.handle_slash_command("/new");
        assert_eq!(app.take_actions(), vec![Action::turn("/new")]);
        let said = notices(&app);
        assert_eq!(said[2], "/new is Claude's own command: passed on");
        assert!(said[3].starts_with("unharness does not see what Claude's /new changes"));
        assert!(said[3].ends_with("(unharness's /clear starts a new conversation)"));
    }

    /// Context held back by a command is still told after the user
    /// switches away and back before the next prompt.
    #[test]
    fn held_context_survives_a_switch_away_and_back() {
        let mut app = test_app(HarnessId::CODEX);
        app.submit_prompt("first question".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::TextDelta("first answer".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.switch_harness(HarnessId::CLAUDE);
        app.take_actions();
        app.session_alive = false;
        app.handle_slash_command("/hello world");
        assert_eq!(sent_turn(&mut app), "/hello world");
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        app.switch_harness(HarnessId::CODEX);
        app.take_actions();
        app.session_alive = false;
        app.switch_harness(HarnessId::CLAUDE);
        app.take_actions();
        app.session_alive = false;
        app.submit_prompt("next".into());
        let next = sent_turn(&mut app);
        assert!(next.contains("first answer"), "{next}");
    }

    /// A `/` prompt queued for one harness is not sent to another, where
    /// it could be a command or not one.
    #[test]
    fn a_slash_prompt_queued_for_one_harness_comes_back_on_another() {
        let mut app = test_app(HarnessId::CODEX);
        app.submit_prompt("go".into());
        app.take_actions();
        app.session_alive = true;
        // Text, on Codex.
        app.pass_command("/review the diff", true);
        app.queue_prompt("plain".into());
        assert_eq!(app.queued.len(), 2);
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted,
        });
        app.switch_harness(HarnessId::CLAUDE);
        app.take_actions();
        app.session_alive = false;
        app.set_input("half typed");
        assert!(app.send_next_queued());
        assert!(sent_turn(&mut app).ends_with("\nplain"));
        assert_eq!(app.input, "/review the diff\nhalf typed");
        assert!(
            notices(&app)
                .last()
                .unwrap()
                .starts_with("'/review the diff' was typed for Codex")
        );
    }

    #[test]
    fn a_harness_that_runs_no_commands_is_not_sent_one() {
        let mut app = test_app(HarnessId::CODEX);
        app.handle_slash_command("/review");
        assert!(app.take_actions().is_empty());
        assert!(notices(&app)[0].starts_with("unknown command '/review'"));
        assert!(notices(&app)[0].contains("\\/review sends it as text"));
        // Escaped, it is a prompt that starts with `/`.
        app.pass_command("/review", true);
        assert_eq!(
            app.take_actions(),
            vec![
                Action::StartSession { resume: None },
                Action::turn("/review")
            ]
        );
        assert_eq!(notices(&app).len(), 1);
    }

    #[test]
    fn the_list_offers_the_harness_s_commands() {
        let mut app = app_with_claude_commands();
        app.set_input("");
        app.insert_char('/');
        let shown: Vec<&str> = app.suggestions.iter().map(|(c, _)| c.as_str()).collect();
        assert!(shown.contains(&"/clear") && shown.contains(&"/hello"));
        // Its `/clear` gives way to unharness's.
        assert_eq!(shown.iter().filter(|c| **c == "/clear").count(), 1);
        for c in "he".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.suggestions.len(), 2, "{:?}", app.suggestions);
        let hello = app.suggestions.iter().find(|(c, _)| c == "/hello").unwrap();
        assert_eq!(hello.1, "Claude: <name> · hello it");
        // Escaped, its own come up, unharness's do not.
        app.set_input("");
        for c in "\\/cl".chars() {
            app.insert_char(c);
        }
        assert_eq!(
            app.suggestions,
            vec![("\\/clear".to_string(), "Claude: clear it".to_string())]
        );
        assert!(app.should_accept_suggestion());
        app.accept_suggestion();
        assert_eq!(app.input, "\\/clear");

        // The list matches regardless of case, and so does Enter.
        app.set_input("");
        for c in "/dep".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.suggestions[0].0, "/Deploy");
        assert!(app.should_accept_suggestion());
        app.accept_suggestion();
        assert_eq!(app.input, "/Deploy");

        // Codex runs none, so none are offered.
        let mut codex = test_app(HarnessId::CODEX);
        codex.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            commands: Some(vec![HarnessCommand::new("hello", None, None).unwrap()]),
            ..Default::default()
        }));
        codex.insert_char('/');
        assert!(codex.suggestions.iter().all(|(c, _)| c != "/hello"));
    }

    /// A command has to start the message: the context of a switch waits
    /// for the next prompt instead of going in front of it.
    #[test]
    fn a_command_goes_alone_and_the_bridge_waits() {
        let mut app = test_app(HarnessId::CODEX);
        app.submit_prompt("first question".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::TextDelta("first answer".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.switch_harness(HarnessId::CLAUDE);
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        app.session_alive = false;

        app.handle_slash_command("/hello world");
        assert_eq!(sent_turn(&mut app), "/hello world");
        assert!(
            notices(&app)
                .last()
                .unwrap()
                .starts_with("Claude is told what it has not seen yet")
        );
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });

        app.submit_prompt("next".into());
        let next = sent_turn(&mut app);
        assert!(next.contains("first answer"), "{next}");
        assert!(next.ends_with("next"), "{next}");
    }

    #[test]
    fn paste_inserts_at_the_cursor_without_submitting() {
        let mut app = test_app(HarnessId::CLAUDE);
        for c in "ab".chars() {
            app.insert_char(c);
        }
        app.move_cursor_left();
        app.paste("one\r\ntwo\rthree\n\tfour\x1b[31m");
        assert_eq!(app.input, "aone\ntwo\nthree\n    four[31mb");
        assert_eq!(app.cursor, app.input.chars().count() - 1);
        assert!(!app.is_generating && app.take_actions().is_empty());
        // Pasted text that starts with a slash is not a command to complete.
        app.take_input();
        app.paste("/model\nsonnet");
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn dropped_files_are_attached_instead_of_pasted() {
        let tmp = tempfile::tempdir().unwrap();
        let file = |name: &str, content: &[u8]| {
            let path = tmp.path().join(name);
            std::fs::write(&path, content).unwrap();
            path.to_str().unwrap().to_string()
        };
        let shot = file("my shot.png", b"png");
        let notes = file("notes.txt", b"txt");
        let binary = file("a.out", &[0xff, 0xfe]);
        let labels =
            |app: &App| -> Vec<String> { app.attachments.iter().map(|a| a.label()).collect() };

        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
        app.paste(&format!("'{shot}' "));
        app.paste(&format!("file://{}\n", notes.replace(' ', "%20")));
        assert_eq!(labels(&app), ["my shot.png", "notes.txt"]);
        assert!(app.input.is_empty());

        // Paths in a sentence, and paths of no file, are text.
        app.paste(&format!("see {notes}"));
        app.paste(&format!(" {notes}.missing"));
        assert_eq!(app.input, format!("see {notes} {notes}.missing"));
        assert_eq!(app.attachments.len(), 2);

        // Codex takes the image; the others stay paths, with the reason.
        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CODEX, None, false);
        app.paste(&format!("{notes}\n{shot}\n{binary}"));
        assert_eq!(labels(&app), ["my shot.png"]);
        assert_eq!(app.input, format!("{notes} {binary}"));
        let notices: Vec<&str> = app
            .transcript
            .blocks
            .iter()
            .filter_map(|b| match b {
                super::super::transcript::Block::Notice(text) => Some(text.as_str()),
                _ => None,
            })
            .filter(|text| text.contains("not attached"))
            .collect();
        assert_eq!(notices.len(), 2);
        assert!(
            notices[0].contains("does not accept file attachments")
                && notices[0].contains("notes.txt not attached: ")
        );
        assert!(notices[1].contains("a.out not attached: only images"));

        // Nothing it can take: the paste goes in as it came.
        app.take_input();
        app.paste(&format!("'{notes}' "));
        assert_eq!(app.input, format!("'{notes}' "));
    }

    #[test]
    fn clipboard_images_are_saved_and_attached() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pasted");
        let image = || {
            Ok(Pasted::Image {
                extension: "png",
                bytes: b"png".to_vec(),
            })
        };
        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
        app.handle_slash_command("/paste");
        assert!(app.take_clipboard_request() && !app.take_clipboard_request());
        app.attach_pasted(image(), &dir);
        let [a] = &app.attachments[..] else {
            panic!("one attachment expected");
        };
        assert!(a.is_image() && a.path().starts_with(&dir));
        assert_eq!(std::fs::read(a.path()).unwrap(), b"png");

        // Copied files are attached where they are.
        std::fs::write(tmp.path().join("notes.txt"), b"txt").unwrap();
        app.attach_pasted(Ok(Pasted::Files(vec![tmp.path().join("notes.txt")])), &dir);
        assert_eq!(app.attachments[1].path(), tmp.path().join("notes.txt"));

        // Nothing read, and the reason, when there was nothing to read.
        app.attach_pasted(Err(anyhow::anyhow!("over ssh …")), &dir);
        assert_eq!(app.attachments.len(), 2);
        assert!(matches!(
            app.transcript.blocks.last(),
            Some(super::super::transcript::Block::Error(text)) if text == "not pasted: over ssh …"
        ));

        // No image is written for a harness that takes none.
        let dir = tmp.path().join("unused");
        let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::AGY, None, false);
        app.attach_pasted(image(), &dir);
        assert!(app.attachments.is_empty() && !dir.exists());
    }

    #[test]
    fn paste_into_a_modal_fills_its_text_field_or_is_ignored() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
            id: "p1".into(),
            kind: PermissionKind::ToolUse {
                tool: "Bash".into(),
                input: serde_json::json!({"command":"ls"}),
                action: crate::core::ToolAction::Opaque,
                description: None,
            },
            tool_call_id: None,
        }));
        // "y" would allow and "a" would always allow if a paste were keys.
        app.paste("yes\nalways");
        assert!(app.modal.is_some() && app.take_actions().is_empty());
        assert!(app.input.is_empty());

        app.handle_modal_key(key(KeyCode::Char('n')));
        app.paste("not\nnow");
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.take_actions().iter().any(|a| matches!(
            a,
            Action::Command(SessionCommand::RespondPermission {
                decision: PermissionDecision::Deny { reason }, ..
            }) if reason == "not now"
        )));
    }

    pub(crate) fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn wheel_scrolls_the_transcript_not_the_prompt() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.history.push("earlier prompt");
        app.insert_str("draft");
        app.scroll = 20;
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 5, 5));
        assert_eq!((app.scroll, app.auto_scroll), (17, false));
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 5, 5));
        assert_eq!(app.scroll, 20);
        assert_eq!(app.input, "draft");

        // Not while a dialog is open, which takes no click either.
        app.open_policy_picker();
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 5, 5));
        assert_eq!(app.scroll, 20);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 5));
        assert!(app.selection.is_none());
    }

    #[test]
    fn a_release_under_a_dialog_ends_the_drag_it_cut_short() {
        let left = MouseButton::Left;
        let mut app = test_app(HarnessId::CLAUDE);
        // Held past the bottom edge, or by the scrollbar thumb, when a
        // request opened a dialog.
        app.drag_edge = 1;
        app.scrollbar_grab = Some(2);
        app.open_policy_picker();
        app.handle_mouse(mouse(MouseEventKind::Drag(left), 5, 5));
        assert!(app.scrollbar_held());
        app.handle_mouse(mouse(MouseEventKind::Up(left), 5, 5));
        assert_eq!(app.drag_edge, 0);
        assert!(!app.scrollbar_held());
        assert!(app.take_copy_request().is_none());
        assert!(app.modal.is_some());
    }

    #[test]
    fn multibyte_input_editing() {
        let mut app = test_app(HarnessId::CLAUDE);
        for c in "héllo".chars() {
            app.insert_char(c);
        }
        app.move_cursor_left();
        app.move_cursor_left();
        app.delete_backwards();
        assert_eq!(app.input, "hélo");
        app.move_cursor_home();
        app.delete_forwards();
        assert_eq!(app.input, "élo");
    }

    #[test]
    fn usage_accumulates() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.on_event(AgentEvent::Usage(Usage {
            input: 10,
            output: 5,
            ..Default::default()
        }));
        app.on_event(AgentEvent::Usage(Usage {
            input: 1,
            output: 1,
            ..Default::default()
        }));
        assert_eq!(app.turn_usage.input, 1);
        assert_eq!(app.session_usage.input, 11);
        app.on_event(AgentEvent::Usage(Usage {
            input: 100,
            cumulative: true,
            ..Default::default()
        }));
        assert_eq!(app.session_usage.input, 100);
    }

    #[test]
    fn mcp_servers_go_to_harnesses_that_take_them_and_the_rest_is_said_once() {
        use super::super::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        app.config.mcp_servers.insert(
            "files".into(),
            crate::core::mcp::McpServerSettings {
                command: Some("files-mcp".into()),
                ..Default::default()
            },
        );
        let notices = |app: &App| {
            app.transcript
                .blocks
                .iter()
                .filter(|b| matches!(b, Block::Notice(n) if n.contains("MCP")))
                .count()
        };
        assert_eq!(app.session_mcp_servers().len(), 1);
        assert_eq!(notices(&app), 0);

        app.switch_harness(HarnessId::PI);
        assert!(app.session_mcp_servers().is_empty());
        assert!(app.session_mcp_servers().is_empty());
        assert_eq!(notices(&app), 1);
    }

    /// A session started before anything was sent to it, as the event loop
    /// starts one.
    fn blank_session(app: &mut App) {
        app.session_alive = true;
        app.session_blank = true;
    }

    #[test]
    fn an_unprompted_session_is_not_saved_until_the_first_prompt() {
        let mut app = test_app(HarnessId::CODEX);
        blank_session(&mut app);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "codex-1".into(),
            model: None,
        });
        assert!(app.session_ids.is_empty());
        assert!(app.store.list().is_empty());
        assert!(!notices(&app).iter().any(|n| n.contains("codex-1")));
        app.quit();
        assert!(app.store.list().is_empty(), "quitting saved nothing");

        let mut app = test_app(HarnessId::CODEX);
        blank_session(&mut app);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "codex-1".into(),
            model: None,
        });
        app.submit_prompt("hello".into());
        assert_eq!(app.take_actions(), vec![Action::turn("hello")]);
        assert_eq!(
            app.session_ids.get(&HarnessId::CODEX).map(String::as_str),
            Some("codex-1")
        );
        assert_eq!(app.store.list().len(), 1);
    }

    #[test]
    fn an_unprompted_session_is_sent_the_whole_bridge() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("first question".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::TextDelta("first answer".into()));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.switch_harness(HarnessId::CODEX);
        app.take_actions();
        blank_session(&mut app);
        app.on_event(AgentEvent::SessionStarted {
            session_id: "codex-1".into(),
            model: None,
        });
        // Away and back before it was prompted: it still saw nothing.
        app.switch_harness(HarnessId::CLAUDE);
        app.take_actions();
        app.switch_harness(HarnessId::CODEX);
        app.take_actions();
        blank_session(&mut app);
        app.submit_prompt("second question".into());
        let text = sent_turn(&mut app);
        assert!(text.contains("first question"), "{text}");
        assert!(text.contains("first answer"), "{text}");
        assert!(text.ends_with("second question"), "{text}");
    }

    #[test]
    fn a_prompt_while_a_start_is_queued_starts_no_second_session() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.actions.push_back(Action::StartSession { resume: None });
        app.session_blank = true;
        app.submit_prompt("hello".into());
        assert_eq!(
            app.take_actions(),
            vec![Action::StartSession { resume: None }, Action::turn("hello")]
        );
        assert!(!app.session_blank);
    }

    fn starts(actions: &[Action]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, Action::StartSession { .. }))
            .count()
    }

    #[test]
    fn an_idle_harness_is_started_once_before_the_first_prompt() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.start_when_idle();
        app.start_when_idle();
        assert_eq!(app.actions, [Action::StartSession { resume: None }]);
        // A prompt typed before the start ran does not start another, and
        // takes the session for the conversation.
        app.submit_prompt("hello".into());
        assert_eq!(
            app.take_actions(),
            vec![Action::StartSession { resume: None }, Action::turn("hello")]
        );
        assert!(!app.session_blank);
        // Running: nothing more to start.
        app.session_alive = true;
        app.start_when_idle();
        assert!(app.take_actions().is_empty());
    }

    #[test]
    fn nothing_starts_before_a_policy_is_chosen() {
        let tmp = tempfile::tempdir().unwrap();
        // Codex's `exec` has no `ask`, and nothing below it.
        let registry = Arc::new(Registry::empty().with(Box::new(
            crate::harness::codex::CodexHarness::new(crate::harness::codex::CodexTransport::Exec),
        )));
        let mut app = test_app_with(
            tmp.keep(),
            HarnessId::CODEX,
            None,
            false,
            Config::default(),
            registry,
        );
        app.start_when_idle();
        assert!(app.take_actions().is_empty());
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        app.start_when_idle();
        assert_eq!(starts(&app.take_actions()), 1);
    }

    #[test]
    fn a_harness_started_with_the_first_prompt_is_asked_for_its_commands() {
        let mut app = app_with_a_binary(HarnessId::AGY);
        app.start_when_idle();
        assert!(app.take_actions().is_empty());
        let jobs = app.take_list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].request, ListRequest::Commands(HarnessId::AGY));
        app.start_when_idle();
        assert!(app.take_list_jobs().is_empty(), "asked twice");

        app.input = "/pine".into();
        app.update_suggestions();
        assert!(app.suggestions.is_empty());
        let skill = HarnessCommand::new("pineapple", Some("A code word"), None).unwrap();
        app.on_list(
            ListRequest::Commands(HarnessId::AGY),
            ListResult::Commands(Ok(Some(vec![skill]))),
        );
        assert!(app.caps().slash_commands);
        assert_eq!(app.suggestions[0].0, "/pineapple");
        app.start_when_idle();
        assert!(app.take_list_jobs().is_empty(), "asked again once listed");

        // One that failed says why, once, and is not asked again.
        let mut app = app_with_a_binary(HarnessId::AGY);
        app.start_when_idle();
        app.take_list_jobs();
        app.on_list(
            ListRequest::Commands(HarnessId::AGY),
            ListResult::Commands(Err("timed out".into())),
        );
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("could not be listed: timed out"))
        );
        app.start_when_idle();
        assert!(app.take_list_jobs().is_empty());
    }

    #[test]
    fn a_harness_that_leaves_a_session_behind_starts_with_the_first_prompt() {
        // agy writes a conversation for every process.
        let mut app = test_app(HarnessId::AGY);
        app.start_when_idle();
        assert!(app.take_actions().is_empty());
        app.submit_prompt("hello".into());
        assert_eq!(starts(&app.take_actions()), 1);
        assert!(!app.session_blank);
    }

    #[test]
    fn a_switch_starts_the_next_harness_on_its_session() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_ids.insert(HarnessId::CODEX, "codex-1".into());
        blank_session(&mut app);
        app.switch_harness(HarnessId::CODEX);
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        app.start_when_idle();
        assert_eq!(
            app.take_actions(),
            vec![Action::StartSession {
                resume: Some("codex-1".into())
            }]
        );
    }

    #[test]
    fn a_session_that_ends_by_itself_waits_for_the_user() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.start_when_idle();
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::ProcessExited { code: Some(1) });
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("exited with code 1 before any prompt"))
        );
        app.start_when_idle();
        assert!(app.take_actions().is_empty(), "started again by itself");
        // A prompt starts it, as does a switch.
        app.submit_prompt("hello".into());
        assert_eq!(starts(&app.take_actions()), 1);

        let mut app = test_app(HarnessId::CLAUDE);
        app.start_failed("could not start claude: not found".into());
        app.start_when_idle();
        assert!(app.take_actions().is_empty());
        assert_eq!(
            notices(&app)
                .iter()
                .filter(|n| n.contains("not found"))
                .count(),
            1
        );
        app.switch_harness(HarnessId::CODEX);
        app.start_when_idle();
        assert_eq!(starts(&app.take_actions()), 1);
    }

    #[test]
    fn an_early_session_that_fails_its_handshake_is_ended() {
        let mut app = test_app(HarnessId::CODEX);
        blank_session(&mut app);
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Error("thread/start failed".into()),
        });
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert!(notices(&app).iter().any(|n| n == "thread/start failed"));
        app.start_when_idle();
        assert!(app.take_actions().is_empty());
        app.submit_prompt("hello".into());
        assert_eq!(starts(&app.take_actions()), 1);
    }

    #[test]
    fn a_choice_before_the_first_prompt_starts_the_session_again_with_it() {
        let mut app = test_app(HarnessId::CLAUDE);
        blank_session(&mut app);
        app.set_effort("high".into());
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        app.start_when_idle();
        assert_eq!(starts(&app.take_actions()), 1);
        app.session_alive = true;
        app.set_model("opus".into());
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);

        // Once prompted, the session is told.
        app.start_when_idle();
        app.submit_prompt("hello".into());
        app.take_actions();
        app.session_alive = true;
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.set_model("sonnet".into());
        assert!(matches!(
            &app.take_actions()[..],
            [Action::Command(SessionCommand::SetModel(_))]
        ));
    }

    #[test]
    fn a_model_chosen_after_a_failed_start_starts_the_session() {
        let mut app = test_app(HarnessId::CODEX);
        app.start_failed("thread/start failed: unknown model".into());
        app.start_when_idle();
        assert!(app.take_actions().is_empty());
        app.set_model("gpt-5".into());
        app.start_when_idle();
        assert_eq!(starts(&app.take_actions()), 1);
    }

    #[test]
    fn what_an_unprompted_session_changed_is_told_when_it_ends() {
        use crate::core::guard::{Guarded, Watch};
        let config = tempfile::tempdir().unwrap();
        let mut app = test_app(HarnessId::CLAUDE);
        app.guard = Some(Watch::begin(
            &[Guarded::Tree(config.path().to_path_buf())],
            None,
        ));
        blank_session(&mut app);
        std::fs::write(config.path().join("settings.json"), "{}").unwrap();
        app.set_model("opus".into());
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        assert!(
            notices(&app)
                .iter()
                .any(|n| n.contains("configuration changed while it ran")),
            "{:?}",
            notices(&app)
        );
    }

    #[test]
    fn a_fork_started_before_the_first_prompt_is_not_the_conversation_s() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("one".into());
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.take_actions();
        app.session_alive = false;
        app.fork_conversation();
        app.take_actions();
        app.start_when_idle();
        assert_eq!(
            app.take_actions(),
            vec![Action::StartSession {
                resume: Some("claude-1".into())
            }]
        );
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-2".into(),
            model: None,
        });
        // The branch is not taken until something is sent to it.
        assert!(app.fork_pending());
        assert_eq!(app.session_ids[&HarnessId::CLAUDE], "claude-1");
        let copy = app.store.load(&app.conversation.id).unwrap();
        assert_eq!(copy.sessions[&HarnessId::CLAUDE], "claude-1");
        assert_eq!(copy.fork_pending, vec![HarnessId::CLAUDE]);
        // Started again, it branches again from the original.
        app.set_model("opus".into());
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        app.start_when_idle();
        assert_eq!(
            app.take_actions(),
            vec![Action::StartSession {
                resume: Some("claude-1".into())
            }]
        );
        app.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "claude-3".into(),
            model: None,
        });
        app.submit_prompt("two".into());
        assert!(!app.fork_pending());
        assert_eq!(app.session_ids[&HarnessId::CLAUDE], "claude-3");
    }

    #[test]
    fn a_rewound_session_is_the_conversation_s() {
        use super::super::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        for (prompt, anchor) in [("one", "a1"), ("two", "a2")] {
            app.submit_prompt(prompt.into());
            app.session_alive = true;
            app.on_event(AgentEvent::SessionStarted {
                session_id: "s1".into(),
                model: None,
            });
            app.on_event(AgentEvent::TurnAnchor { id: anchor.into() });
            app.on_event(AgentEvent::TurnCompleted {
                stop_reason: StopReason::Done,
            });
        }
        app.take_actions();
        // As after a resume: started early on its session.
        app.session_alive = false;
        app.start_when_idle();
        app.take_actions();
        app.session_alive = true;
        let two = app
            .transcript
            .blocks
            .iter()
            .position(|b| matches!(b, Block::User { text } if text == "two"))
            .unwrap();
        app.rewind_to(two, false);
        assert_eq!(
            app.take_actions(),
            vec![Action::Command(SessionCommand::Rewind {
                anchor: "a2".into()
            })]
        );
        // Restarting it would resume it from before the rewind.
        app.set_model("opus".into());
        assert!(matches!(
            &app.take_actions()[..],
            [Action::Command(SessionCommand::SetModel(_))]
        ));
    }

    #[test]
    fn commands_that_arrive_while_a_slash_is_typed_are_listed() {
        let command = |name: &str| HarnessCommand::new(name, None, None).unwrap();
        let listed = |commands| {
            AgentEvent::CapabilitiesChanged(CapsUpdate {
                commands: Some(commands),
                ..Default::default()
            })
        };
        let mut app = test_app(HarnessId::CLAUDE);
        app.input = "/he".into();
        app.update_suggestions();
        assert_eq!(app.suggestions.len(), 1, "{:?}", app.suggestions);
        app.selected_suggestion = 0;
        let selected = app.suggestions[0].clone();
        app.on_event(listed(vec![command("hello"), command("hey")]));
        let names: Vec<&str> = app.suggestions.iter().map(|(c, _)| c.as_str()).collect();
        assert!(
            names.contains(&"/hello") && names.contains(&"/hey"),
            "{names:?}"
        );
        assert_eq!(app.suggestions[app.selected_suggestion], selected);

        // Not after Esc closed it.
        app.close_suggestions();
        app.on_event(listed(vec![command("hello")]));
        assert!(app.suggestions.is_empty());
        // Typing opens it again.
        app.input = "/hel".into();
        app.update_suggestions();
        assert!(app.suggestions.iter().any(|(c, _)| c == "/hello"));
    }
}
