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
    AlwaysDraft, HarnessOption, ListPicker, Modal, ProviderOption, QuestionStep, RewindOption,
};
use super::prompt;
use super::selection::{self, Granularity, Point, Selection};
use super::transcript::{DEFAULT_BRIDGE_MAX_CHARS, Transcript, tool_summary};
use crate::config::{BridgeSummary, Config};
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{
    CheckpointRecord, ContextWindows, Conversation, ConversationStore, ShellStatus,
    TurnAnchorRecord, now_rfc3339, truncate_title,
};
use crate::core::guard::{self, Watch};
use crate::core::mcp::{self, McpServer};
use crate::core::registry::Registry;
use crate::core::rules::Scope;
use crate::core::sandbox::{Sandbox, SandboxLevel, SandboxSetup};
use crate::core::{
    AgentEvent, Attachment, Capabilities, CapsUpdate, ContextUsage, HarnessId, ModelRef,
    PermissionDecision, PermissionKind, PermissionPolicy, PermissionRequest, PlanEntry,
    PolicyResolution, PolicyUnavailable, ProviderId, RateLimitInfo, Rule, Rules, SessionCommand,
    StopReason, Usage, resolve_policy,
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
}

#[derive(Debug, Clone)]
pub enum ListResult {
    Providers(Result<Vec<(ProviderId, String)>, String>),
    Models(Result<Vec<ModelInfo>, String>),
}

/// Everything a thread needs to answer a `ListRequest`.
pub struct ListJob {
    pub request: ListRequest,
    registry: Arc<Registry>,
    binary: PathBuf,
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
            (ListRequest::Providers(_), None) => {
                ListResult::Providers(Err("not a registered harness".into()))
            }
            (ListRequest::Models(..), None) => {
                ListResult::Models(Err("not a registered harness".into()))
            }
        };
        (self.request, result)
    }

    fn harness(&self) -> HarnessId {
        match &self.request {
            ListRequest::Providers(h) | ListRequest::Models(h, _) => *h,
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
    ("/clear", "Clear the transcript"),
    ("/help", "Show commands and shortcuts"),
    ("/quit", "Exit unharness"),
];

pub struct App {
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub registry: Arc<Registry>,
    pub config: Config,

    pub active: HarnessId,
    pub harness_options: Vec<HarnessOption>,
    /// The policy the user asked for; the effective one is per harness.
    /// The policy named on the command line or set in the TUI: it holds
    /// for every harness. Without one each harness has its configured
    /// default (`policy_requested`).
    pub policy_explicit: Option<PermissionPolicy>,
    /// What the user chose on a harness where the requested policy is not
    /// available. It holds for that harness only.
    pub policy_choice: HashMap<HarnessId, PermissionPolicy>,
    /// The sandbox level asked for (flag, config, or `/sandbox`) and the
    /// platform's backend. A change reaches the harness's next process.
    pub sandbox: SandboxSetup,
    /// The level the live session's process was started under.
    pub session_sandbox_level: Option<SandboxLevel>,
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
    handoff: Option<Handoff>,
    /// Transcript length when each harness was last active (bridge start).
    last_active_index: HashMap<HarnessId, usize>,
    /// Session ids seen this run (or chosen via /resume), per harness.
    pub session_ids: HashMap<HarnessId, String>,
    pub session_alive: bool,
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
    /// Requests these allow are answered without asking.
    rules: Rules,
    pub suggestions: Vec<(String, String)>,
    pub selected_suggestion: usize,
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
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Conversation id or prefix; empty string = most recent.
    pub resume: Option<String>,
    /// The harness came from the CLI, so it overrides a resumed conversation's.
    pub harness_explicit: bool,
    /// Where file-checkpoint shadow repositories go; `None` = the user's
    /// state directory.
    pub checkpoint_store: Option<PathBuf>,
    /// The user's allow rules, and where "allow always" adds to them.
    pub rules: Rules,
    /// Each harness's guess at its own provider (`Harness::default_provider`),
    /// which reads the user's vendor configuration; tests pass their own.
    pub default_providers: HashMap<HarnessId, ProviderId>,
}

/// Transcript lines scrolled per wheel notch.
const WHEEL_LINES: u16 = 3;

/// Lines a question's preview scrolls on PageUp/PageDown.
const PREVIEW_PAGE: i32 = 5;

/// Presses this close together on one cell count as a double or triple click.
const MULTI_CLICK: Duration = Duration::from_millis(500);

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
    /// Per transcript block: the fingerprint it was rendered from, and
    /// where its lines end in `rendered` and `lines`.
    pub blocks: Vec<(u64, usize)>,
    /// The width `rendered` was laid out for.
    pub width: u16,
    /// The scrollbar, when there is more transcript than fits.
    pub scrollbar: Option<Scrollbar>,
    /// The jump-to-bottom label, shown while scrolled away from the end.
    pub jump: Option<Rect>,
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
        if let Some(m) = init.model {
            let p = providers
                .get(&init.harness)
                .cloned()
                .unwrap_or_else(|| ProviderId::new("default"));
            models.insert(init.harness, ModelRef::new(init.harness, p, m));
        }
        if let Some(e) = init.effort {
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
        let mut app = App {
            cwd: init.cwd,
            workspace_root: init.workspace_root,
            git_branch,
            registry,
            bridge_max_chars: config.bridge_max_chars,
            context_windows: store.context_windows(),
            bridge_summary: config.bridge_summary.unwrap_or_default(),
            handoff: None,
            config,
            active,
            harness_options,
            policy_explicit: init.policy,
            policy_choice: HashMap::new(),
            sandbox: init.sandbox,
            session_sandbox_level: None,
            guard: None,
            providers,
            chosen_providers,
            running_providers: HashMap::new(),
            models,
            efforts,
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
            rules: init.rules,
            suggestions: Vec::new(),
            selected_suggestion: 0,
            completing_file: None,
            file_index: Vec::new(),
            // Listed once at the start, so that the first `@` has files.
            file_index_requested: true,
            file_walk_pending: false,
            should_quit: false,
            actions: VecDeque::new(),
        };

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
            let summary = app.conversation.summary();
            app.transcript.push_notice(format!(
                "resumed conversation {} ({}); continuing on {}",
                &summary.id[..8.min(summary.id.len())],
                summary
                    .harnesses
                    .iter()
                    .map(|h| h.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                app.short_name()
            ));
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
        if c.title.is_empty()
            && let Some(p) = &self.first_prompt
        {
            c.title = truncate_title(p);
        }
        c.updated_at = now_rfc3339();
    }

    /// Write the conversation to disk. Failures are reported once per run.
    pub fn persist(&mut self) {
        self.sync_conversation();
        if !self.conversation.has_content() {
            return;
        }
        if let Err(e) = self.store.save(&self.conversation)
            && !self.persist_failed
        {
            self.persist_failed = true;
            self.transcript
                .push_error(format!("could not save the conversation: {e:#}"));
        }
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

    /// The policy asked of the active harness: the requested one, or where
    /// that is not available, what the user chose there instead.
    pub fn wanted_policy(&self) -> PermissionPolicy {
        let policies = self.caps().permission_policies;
        let requested = self.policy_requested();
        match self.policy_choice.get(&self.active) {
            Some(choice) if resolve_policy(&policies, requested).is_err() => *choice,
            _ => requested,
        }
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
        if self.modal.as_ref().is_some_and(Modal::is_prompt) {
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
        if !text.starts_with('/') {
            self.open_when_listed = None;
        }
        if self.shell.is_some() || !self.require_policy() {
            // Kept until the `!` command has ended, or a policy is chosen.
            self.queued.push_back(QueuedPrompt {
                text,
                attachments: std::mem::take(&mut self.attachments),
            });
            return;
        }
        if self.first_prompt.is_none() {
            self.first_prompt = Some(text.clone());
        }

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
        self.last_active_index.remove(&self.active);
        // `!` commands no agent has been told about yet (the bridge has
        // only those one has).
        let mut shells = Vec::new();
        for b in self.transcript.blocks.iter_mut() {
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

        if !self.session_alive {
            let resume = self.session_ids.get(&self.active).cloned();
            self.actions.push_back(Action::StartSession { resume });
        }
        self.actions.push_back(Action::SendTurn {
            text: outgoing,
            attachments,
        });
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
        let model = self.model_key(self.active);
        let known = self.context_windows.entry(self.active).or_default();
        if known.get(&model) == Some(&window) {
            return;
        }
        known.insert(model, window);
        // Only a measure: a write that fails costs a default budget later.
        let _ = self.store.save_context_windows(&self.context_windows);
    }

    /// Where the bridge to `harness` starts: where it was last active,
    /// nothing when its live session saw it all, everything on its first
    /// visit.
    fn bridge_start(&self, harness: HarnessId) -> usize {
        let visited = self.session_ids.contains_key(&harness)
            || (harness == self.active && self.session_alive);
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
    pub fn steer(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.is_generating || self.compacting {
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
        if self.is_generating || !self.require_policy() {
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
                if !self.session_alive {
                    let resume = self.session_ids.get(&active).cloned();
                    self.actions.push_back(Action::StartSession { resume });
                }
                self.actions
                    .push_back(Action::Command(SessionCommand::Rewind {
                        anchor: anchor.clone(),
                    }));
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

    /// Stop using `harness`'s vendor session: its next turn starts a fresh
    /// one and gets the conversation so far as context.
    fn forget_session(&mut self, harness: HarnessId) {
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
        if !self.conversation.has_content() {
            self.transcript.push_notice("nothing to fork yet");
            return;
        }
        self.persist();
        let original = self.conversation.id.clone();
        if self.session_alive {
            self.shutdown_session();
        }
        let mut fork = Conversation::new(self.active);
        fork.title = self.conversation.title.clone();
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
        self.anchor_pending.clear();
        self.session_usage = Usage::default();
        self.turn_usage = Usage::default();
        self.context = ContextUsage::default();
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
        if !self.session_alive {
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

    pub fn interrupt(&mut self) {
        if self.is_generating {
            self.actions
                .push_back(Action::Command(SessionCommand::Interrupt));
            self.transcript.push_system("Interrupting…");
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
                self.live_caps.entry(self.active).or_default().merge(update);
            }
            AgentEvent::TurnCompleted { stop_reason } => {
                let done = stop_reason == StopReason::Done;
                match stop_reason {
                    StopReason::Done => {}
                    StopReason::Interrupted => self.transcript.push_system("Turn interrupted."),
                    StopReason::Error(e) => self.transcript.push_error(e),
                }
                self.check_guard();
                self.finish_generation();
                self.finish_handoff(done);
                self.persist();
                // A clean finish moves on to the next queued prompt; after an
                // interrupt or error the user decides (Enter sends it).
                if done {
                    self.send_next_queued();
                } else if !self.queued.is_empty() {
                    self.transcript.push_notice(format!(
                        "{} queued prompt(s) held; press Enter to send the next",
                        self.queued.len()
                    ));
                }
            }
            AgentEvent::Notice(n) => self.transcript.push_notice(n),
            AgentEvent::Error(e) => self.transcript.push_error(e),
            AgentEvent::ProcessExited { code } => {
                self.session_alive = false;
                self.drop_subagents();
                if self.is_generating {
                    self.finish_generation();
                    self.transcript.push_error(format!(
                        "{} exited{} before the turn completed",
                        self.short_name(),
                        code.map(|c| format!(" with code {c}")).unwrap_or_default()
                    ));
                }
                if self.modal.as_ref().is_some_and(Modal::is_prompt) {
                    self.modal = None;
                }
                self.pending_prompts.clear();
                self.finish_handoff(false);
            }
        }
    }

    /// The session is going away, and whatever its subagents were doing.
    fn drop_subagents(&mut self) {
        self.subagents.clear();
        self.transcript.end_running_agents();
    }

    /// End the active harness's session.
    fn shutdown_session(&mut self) {
        self.drop_subagents();
        // The next session reports where it runs.
        self.running_providers.remove(&self.active);
        self.actions.push_back(Action::Shutdown);
    }

    /// The allow rules that answer a request, if the user has them for it.
    fn allowing_rules(&self, req: &PermissionRequest) -> Option<Vec<&Rule>> {
        match &req.kind {
            PermissionKind::ToolUse { tool, action, .. } => {
                self.rules.allows(tool, action, &self.cwd)
            }
            _ => None,
        }
    }

    fn on_permission_request(&mut self, req: PermissionRequest) {
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
                _ => PermissionDecision::Answer(Value::Null),
            };
            self.actions
                .push_back(Action::Command(SessionCommand::RespondPermission {
                    id: req.id,
                    decision,
                }));
            return;
        }
        if let Some(rules) = self.allowing_rules(&req) {
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
            self.modal = Some(Modal::for_request(req));
        } else {
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
        // The next one waiting that still needs an answer: a rule added in
        // the meantime may cover some of them.
        while self.modal.is_none()
            && let Some(req) = self.pending_prompts.pop_front()
        {
            self.on_permission_request(req);
        }
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
        if self.handoff_wanted(next) {
            self.ask_handoff(next);
            return;
        }
        self.switch_now(next);
    }

    /// Whether the conversation will not fit in the bridge to `next`, and
    /// the harness being left can say what it was about: it has a session,
    /// and that session saw the whole transcript.
    fn handoff_wanted(&self, next: HarnessId) -> bool {
        self.bridge_summary == BridgeSummary::Auto
            && (self.session_alive || self.session_ids.contains_key(&self.active))
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
        });
        self.start_generation();
        if !self.session_alive {
            let resume = self.session_ids.get(&self.active).cloned();
            self.actions.push_back(Action::StartSession { resume });
        }
        self.actions.push_back(Action::SendTurn {
            text: handoff_prompt(to),
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
        if self.session_alive {
            self.shutdown_session();
        }
        // What this harness saw: everything but the `!` commands run since
        // its last prompt, which it is told about when it comes back.
        let seen = self
            .transcript
            .blocks
            .iter()
            .position(|b| matches!(b, super::transcript::Block::Shell { sent: false, .. }))
            .unwrap_or(self.transcript.blocks.len());
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

    /// Returns whether `p` could be set on the active harness.
    pub fn set_policy(&mut self, p: PermissionPolicy) -> bool {
        let policies = self.caps().permission_policies;
        let res = match resolve_policy(&policies, p) {
            Ok(res) => res,
            Err(e) => {
                self.transcript.push_error(e.to_string());
                return false;
            }
        };
        // Only a prompt that waited for this choice is sent by it.
        let was_waiting = self.effective_policy().is_none();
        if resolve_policy(&policies, self.policy_requested()).is_err() {
            // Chosen because the requested policy cannot be had here: the
            // other harnesses keep the requested one.
            self.policy_choice.insert(self.active, p);
        } else {
            self.policy_explicit = Some(p);
            // What was chosen in place of another request is not kept for
            // this one.
            self.policy_choice.clear();
        }
        self.transcript
            .push_system(format!("Permission policy: {}", res.effective));
        if let Some(w) = res.warning {
            self.transcript.push_notice(w);
        }
        if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::SetPolicy(res.effective)));
        }
        if was_waiting {
            self.send_next_queued();
        }
        true
    }

    /// Choose the sandbox level for the run. Returns whether it could be
    /// set. The sandbox is applied when a process is spawned, so a live
    /// session is shut down and comes back under the new level with the
    /// next prompt, resumed where the harness can.
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
        let restart = self.session_alive && self.session_sandbox_level != Some(level);
        self.transcript.push_system(if restart {
            format!(
                "Sandbox: {level} ({} restarts under it with the next prompt)",
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
    /// on the new one with the next prompt, resumed.
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
        }
        self.providers.insert(self.active, provider.clone());
        self.chosen_providers.insert(self.active);
        self.transcript.push_system(if restart {
            format!(
                "Provider: {provider} ({} restarts on it with the next prompt; pick a model with Ctrl+M)",
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
        if self.session_alive {
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
        self.transcript.push_system(format!("Effort: {effort}"));
        if self.session_alive {
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
        let conv = match self.store.load(&id_or_prefix) {
            Ok(c) => c,
            Err(e) => {
                self.transcript.push_error(format!("could not resume: {e}"));
                return;
            }
        };
        if self.session_alive {
            self.shutdown_session();
        }
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
        let summary = conv.summary();
        self.conversation = conv;
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
        self.auto_scroll = true;
    }

    pub fn quit(&mut self) {
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
            ListPicker::new(self.harness_options.clone()).with_selected(idx),
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
        if self.open_when_listed.as_ref() == Some(&request) {
            self.open_when_listed = None;
            self.transcript.push_error(match request {
                ListRequest::Providers(_) => format!("list providers: {why}"),
                ListRequest::Models(..) => format!("list models: {why}"),
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
            ListResult::Providers(Err(why)) | ListResult::Models(Err(why)) => {
                self.list_failed(request, why);
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
        };
        if !still_there || self.modal.is_some() {
            return false;
        }
        if !self.input.is_empty() {
            self.transcript.push_notice(match request {
                ListRequest::Providers(_) => "The providers are here: /provider".to_string(),
                ListRequest::Models(_, p) => format!("The models on {p} are here: Ctrl+M"),
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

    pub fn open_policy_picker(&mut self) {
        let idx = PermissionPolicy::ALL
            .iter()
            .position(|p| *p == self.wanted_policy());
        self.modal = Some(Modal::Policy(
            ListPicker::new(PermissionPolicy::ALL.to_vec()).with_selected(idx),
        ));
    }

    pub fn open_sandbox_picker(&mut self) {
        let wanted = self.sandbox_level().0;
        let idx = SandboxLevel::ALL.iter().position(|l| *l == wanted);
        self.modal = Some(Modal::Sandbox(
            ListPicker::new(SandboxLevel::ALL.to_vec()).with_selected(idx),
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
        }
    }

    // ------------------------------------------------------------------ modal keys

    /// Route a key to the open modal. Local pickers close on Enter/Esc; prompt
    /// modals send their decision through `answer_prompt`.
    pub fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        let mut choice = None;
        let outcome = match &mut modal {
            Modal::Harness(p) => picker_nav(p, key.code).map(|c| {
                c.and_then(|_| p.current().map(|o| (o.id, o.installed)))
                    .map(ModalChoice::Harness)
            }),
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
            Modal::Policy(p) => picker_nav(p, key.code)
                .map(|c| c.and_then(|_| p.current().map(|pol| ModalChoice::Policy(*pol)))),
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
                        KeyCode::PageDown => {
                            m.scroll_preview(PREVIEW_PAGE);
                            QuestionStep::Stay
                        }
                        KeyCode::PageUp => {
                            m.scroll_preview(-PREVIEW_PAGE);
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
                    ModalChoice::Harness((id, installed)) => {
                        self.modal = None;
                        if installed {
                            self.switch_harness(id);
                        } else {
                            self.transcript.push_error(format!("{id} is not installed"));
                        }
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
                    ModalChoice::Always(scope, rules) => self.allow_always(scope, &rules),
                    ModalChoice::Dismiss => self.close_modal(),
                }
            }
        }
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
                    None => self.transcript.push_error(format!(
                        "unknown policy '{a}' (ask, accept-edits, auto, bypass)"
                    )),
                },
                None => self.open_policy_picker(),
            },
            "/sandbox" => match arg {
                Some(a) => match SandboxLevel::parse(&a) {
                    Some(l) => {
                        self.set_sandbox(l);
                    }
                    None => self.transcript.push_error(format!(
                        "unknown sandbox level '{a}' (read-only, workspace-write, off)"
                    )),
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
            "/clear" => {
                self.transcript.clear();
                self.plan.clear();
                self.queued.clear();
                self.anchors.clear();
                self.file_checkpoints.clear();
                self.fork_pending.clear();
                self.anchor_pending.clear();
                self.transcript.push_system("Transcript cleared.");
                self.persist();
            }
            "/help" => {
                let mut help = String::from("Commands:\n");
                for (c, d) in BASE_COMMANDS {
                    help.push_str(&format!("  {c:<14} {d}\n"));
                }
                help.push_str(
                    "Shortcuts: Ctrl+H harness · Ctrl+M model · Ctrl+E effort · Ctrl+P policy · Ctrl+R resume · Ctrl+O expand the last tool call (click any call to expand that one, Ctrl+T for all) · Esc/Ctrl+C interrupt or quit\nSubagents: listed above the prompt while they work, also after their turn has ended · listed under the prompt while they work · what each one does is in a transcript of its own · Down from the prompt goes into the list, Enter opens the one chosen, Delete takes a finished one off the list (so does Ctrl+S, /subagents, or a click on the call that spawned it) · there: s stops it, Tab goes to the next, Esc comes back · a prompt sent meanwhile goes straight to the agent\nPrompt: Ctrl+J newline (Shift+Enter too where the terminal can tell it from Enter) · Up/Down move between lines, then through earlier prompts · Home/End (Ctrl+A) line start/end · Ctrl+U clear · Ctrl+G edit in $EDITOR · Ctrl+V attach the clipboard's image (/paste)\nShell: !command runs it yourself, in the session's directory and sandbox, with no input; its output is shown here and goes to the agent in front of your next prompt · Esc stops it · not during a turn · \\!text sends a prompt that starts with !\nTranscript: PageUp/PageDown, Shift+Up/Down, the mouse wheel or the scrollbar scroll · click \"Jump to bottom\" or press End (empty prompt) to go back to the end · drag to select and copy (double click a word, triple a row)\nDuring a turn: Enter queues the prompt · Alt+Enter steers the running turn · Alt+Up edits the last queued prompt",
                );
                self.transcript.push_system(help);
            }
            "/quit" | "/exit" => self.quit(),
            other => self
                .transcript
                .push_error(format!("unknown command '{other}'; /help lists commands")),
        }
    }

    // ------------------------------------------------------------------ suggestions

    pub fn update_suggestions(&mut self) {
        self.refresh_suggestions(true);
    }

    /// `files` is whether an `@` word may open the list of files: one that
    /// was typed does, one that came in a paste or from the editor does not.
    fn refresh_suggestions(&mut self, files: bool) {
        self.suggestions.clear();
        let was_completing_file = self.completing_file.take().is_some();
        if !self.input.starts_with('/') || self.input.contains('\n') {
            self.selected_suggestion = 0;
            if files {
                self.suggest_files(was_completing_file);
            }
            return;
        }
        let query = self.input.to_lowercase();
        let (cmd, sub) = match query.split_once(' ') {
            Some((c, s)) => (c.to_string(), Some(s.trim().to_string())),
            None => (query.clone(), None),
        };
        let mut out = Vec::new();
        match (cmd.as_str(), sub.as_deref()) {
            ("/switch" | "/harness", Some(s)) => {
                for o in &self.harness_options {
                    let id = o.id.as_str();
                    if id.starts_with(s) {
                        out.push((format!("{cmd} {id}"), o.display_name.to_string()));
                    }
                }
            }
            ("/policy", Some(s)) => {
                for p in PermissionPolicy::ALL {
                    if p.as_str().starts_with(s) {
                        out.push((format!("/policy {p}"), p.description().to_string()));
                    }
                }
            }
            ("/sandbox", Some(s)) => {
                for l in SandboxLevel::ALL {
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
                    if c.starts_with(cmd.as_str()) {
                        out.push((c.to_string(), d.to_string()));
                    }
                }
            }
            _ => {}
        }
        self.suggestions = out;
        if self.selected_suggestion >= self.suggestions.len() {
            self.selected_suggestion = 0;
        }
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
        let selected = self.suggestions.get(self.selected_suggestion).cloned();
        self.update_suggestions();
        self.selected_suggestion = selected
            .and_then(|s| self.suggestions.iter().position(|o| *o == s))
            .unwrap_or(0);
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
        if !self.input.starts_with('/') || self.suggestions.is_empty() {
            return false;
        }
        let Some((cmd, _)) = self.suggestions.get(self.selected_suggestion) else {
            return false;
        };
        let typed = self.input.trim();
        typed != cmd && cmd.starts_with(typed)
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

    /// Mouse input, when the TUI has the mouse. A dialog keeps it out.
    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        if self.modal.is_some() {
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

    fn mouse_release(&mut self) {
        self.drag_edge = 0;
        if self.scrollbar_grab.take().is_some() {
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
        let mut start = 0;
        let mut at = None;
        for (i, (_, end)) in self.transcript_view.blocks.iter().enumerate() {
            if line == start {
                at = Some(i);
            }
            if line < *end {
                break;
            }
            start = *end;
        }
        let Some(i) = at else {
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
        self.selection?.range(&self.transcript_view.lines)
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        Some(selection::text(&self.transcript_view.lines, start, end))
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

/// Current branch from `.git/HEAD` without spawning git.
/// The bridge takes at most this fraction (1/n) of the window it goes to.
const BRIDGE_WINDOW_SHARE: usize = 4;
/// About what a token is in English prose and code.
const CHARS_PER_TOKEN: usize = 4;

/// What the harness being left is asked for.
fn handoff_prompt(to: &str) -> String {
    format!(
        "[unharness] The user is switching this conversation to {to}, another coding agent \
         that will see only part of it. Write a handoff summary for that agent: the user's \
         goal and constraints, the decisions made and why, what has been done (files changed, \
         commands run and what came of them), what is unfinished or failing, and the next \
         steps. Be specific (paths, names, error messages) and brief. Do not use any tools \
         and do not change anything: answer from what you already know."
    )
}

fn git_branch(root: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(b) => Some(b.to_string()),
        None => Some(head.chars().take(8).collect()),
    }
}

enum ModalChoice {
    Harness((HarnessId, bool)),
    Provider(String),
    Model(String),
    Effort(String),
    Policy(PermissionPolicy),
    Sandbox(SandboxLevel),
    Subagent(String),
    Resume(String),
    /// (user block, also restore files)
    Rewind(usize, bool),
    Decision(PermissionDecision),
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
    use crate::core::{PermissionKind, Question, QuestionOption};
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
        App::new(AppInit {
            // Tests never write to the real state directory.
            checkpoint_store: Some(cwd.join(".unharness/test-checkpoints")),
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
        })
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
            })
            .collect()
    }

    fn sub(parent: &str, event: AgentEvent) -> AgentEvent {
        AgentEvent::Sub {
            parent: parent.into(),
            event: Box::new(event),
        }
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

        // Claude writes it before its session is shut down.
        app.switch_harness(HarnessId::CODEX);
        assert!(sent_turn(&mut app).contains("Write a handoff summary"));
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
        let mut app = test_app(HarnessId::CLAUDE);
        app.set_policy(PermissionPolicy::Auto);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
        assert!(app.policy_warning().is_none());
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
        assert!(app.policy_warning().unwrap().contains("less permissive"));
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

        // `ask` is the first row and cannot be taken: the picker stays.
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
        assert_eq!(app.effective_policy(), None);

        app.handle_modal_key(key(KeyCode::Down));
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
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        app.session_alive = false;
        app.submit_prompt("again".into());
        let actions = app.take_actions();
        assert!(
            matches!(&actions[0], Action::StartSession { resume: Some(id) } if id == "claude-1")
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
        assert_eq!(app.take_actions(), vec![Action::Shutdown]);
        app.session_alive = false;
        app.submit_prompt("again".into());
        let actions = app.take_actions();
        assert!(
            matches!(&actions[0], Action::StartSession { resume: Some(id) } if id == "claude-1")
        );
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
        let mut config = Config::default();
        config.harnesses.insert(
            "claude".into(),
            crate::config::HarnessSettings {
                binary: Some("/bin/sh".into()),
                ..Default::default()
            },
        );
        let tmp = tempfile::tempdir().unwrap();
        test_app_with(
            tmp.keep(),
            HarnessId::CLAUDE,
            None,
            false,
            config,
            test_registry(),
        )
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

        // The picker opens on the level in effect and stays open on a row
        // that cannot be had.
        app.open_sandbox_picker();
        app.handle_modal_key(key(KeyCode::Up));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Sandbox(_))));
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.sandbox.explicit, Some(SandboxLevel::Off));
        assert!(app.status_warning().is_none());
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

        // A policy set in the TUI is for the run, on every harness.
        app.switch_harness(HarnessId::CLAUDE);
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    }

    #[test]
    fn a_choice_made_for_want_of_a_policy_does_not_outlive_the_request() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
        assert!(app.set_policy(PermissionPolicy::Bypass));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
        // Another policy is asked for on Claude, then `ask` again.
        app.switch_harness(HarnessId::CLAUDE);
        assert!(app.set_policy(PermissionPolicy::AcceptEdits));
        assert!(app.set_policy(PermissionPolicy::Ask));
        app.modal = None;
        app.switch_harness(HarnessId::AGY);
        assert_eq!(app.effective_policy(), None);
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
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
        assert_eq!(app.policy_requested(), PermissionPolicy::AcceptEdits);
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

    #[test]
    fn slash_commands_and_suggestions() {
        let mut app = test_app(HarnessId::CLAUDE);
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

        // Not while a dialog is open.
        app.open_policy_picker();
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 5, 5));
        assert_eq!(app.scroll, 20);
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
}
