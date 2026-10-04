//! TUI state. Pure with respect to I/O: key handling mutates state and queues
//! `Action`s that the event loop in `mod.rs` executes against the session.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use serde_json::Value;

use super::history::PromptHistory;
use super::modal::{HarnessOption, ListPicker, Modal, ProviderOption, RewindOption};
use super::prompt;
use super::selection::{self, Granularity, Point, Selection};
use super::transcript::{DEFAULT_BRIDGE_MAX_CHARS, Transcript};
use crate::config::Config;
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{
    CheckpointRecord, Conversation, ConversationStore, TurnAnchorRecord, now_rfc3339,
    truncate_title,
};
use crate::core::registry::Registry;
use crate::core::{
    AgentEvent, Attachment, Capabilities, CapsUpdate, ContextUsage, HarnessId, ModelRef,
    PermissionDecision, PermissionPolicy, PermissionRequest, PlanEntry, ProviderId, RateLimitInfo,
    SessionCommand, StopReason, Usage, resolve_policy,
};
use crate::harness::{Harness, ModelInfo, ProviderSource, resolve_binary};

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
        "Attach an image to the next prompt: /attach <path>",
    ),
    ("/detach", "Drop the pending attachments"),
    ("/skills", "List skills discovered in .agents/skills"),
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
    pub policy_requested: PermissionPolicy,
    pub providers: HashMap<HarnessId, ProviderId>,
    pub models: HashMap<HarnessId, ModelRef>,
    pub efforts: HashMap<HarnessId, String>,
    model_cache: HashMap<(HarnessId, String), Vec<ModelInfo>>,

    pub transcript: Transcript,
    pub bridge_max_chars: usize,
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
    /// Selected text waiting for the event loop to put it on the clipboard.
    copy_request: Option<String>,
    /// A short-lived message in the status rule, and when it appeared.
    flash: Option<(String, Instant)>,
    /// The user asked to edit the prompt in their editor; the event loop
    /// owns the terminal, so it does the work.
    edit_requested: bool,
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
    compacting: bool,
    /// What live sessions reported on top of the declared capabilities.
    live_caps: HashMap<HarnessId, CapsUpdate>,

    pub modal: Option<Modal>,
    pending_prompts: VecDeque<PermissionRequest>,
    pub suggestions: Vec<(String, String)>,
    pub selected_suggestion: usize,

    pub should_quit: bool,
    actions: VecDeque<Action>,
}

pub struct AppInit {
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub registry: Arc<Registry>,
    pub config: Config,
    pub harness: HarnessId,
    pub policy: PermissionPolicy,
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
}

/// Transcript lines scrolled per wheel notch.
const WHEEL_LINES: u16 = 3;

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
        let mut models = HashMap::new();
        let mut efforts = HashMap::new();
        for h in registry.all() {
            let id = h.descriptor().id;
            let settings = config.harness(id.as_str());
            let provider = settings
                .and_then(|s| s.default_provider.clone())
                .or_else(|| match h.descriptor().providers {
                    ProviderSource::Static(list) => list.first().map(|(p, _)| p.to_string()),
                    ProviderSource::Dynamic => None,
                });
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
            bridge_max_chars: config.bridge_max_chars.unwrap_or(DEFAULT_BRIDGE_MAX_CHARS),
            config,
            active,
            harness_options,
            policy_requested: init.policy,
            providers,
            models,
            efforts,
            model_cache: HashMap::new(),
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
            copy_request: None,
            flash: None,
            edit_requested: false,
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
            compacting: false,
            live_caps: HashMap::new(),
            modal: None,
            pending_prompts: VecDeque::new(),
            suggestions: Vec::new(),
            selected_suggestion: 0,
            should_quit: false,
            actions: VecDeque::new(),
        };

        let res = resolve_policy(&app.caps(), app.policy_requested);
        app.transcript.push_system(format!(
            "Welcome to unharness. Harness: {}  Policy: {}. Ctrl+H harness, Ctrl+M model, Ctrl+E effort, Ctrl+P policy, /help.",
            app.display_name(),
            res.effective
        ));
        if let Some(w) = res.warning {
            app.transcript.push_notice(w);
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

    pub fn effective_policy(&self) -> PermissionPolicy {
        resolve_policy(&self.caps(), self.policy_requested).effective
    }

    pub fn policy_warning(&self) -> Option<String> {
        resolve_policy(&self.caps(), self.policy_requested).warning
    }

    pub fn current_provider(&self) -> Option<&ProviderId> {
        self.providers.get(&self.active)
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
        if self.first_prompt.is_none() {
            self.first_prompt = Some(text.clone());
        }

        // Bridge context from other harnesses: everything since this harness
        // was last active, or everything on its first visit. Nothing when the
        // live session already saw the whole transcript.
        let visited = self.session_ids.contains_key(&self.active) || self.session_alive;
        let from = if visited {
            self.last_active_index
                .get(&self.active)
                .copied()
                .unwrap_or(self.transcript.blocks.len())
        } else {
            0
        };
        let bridge = if from >= self.transcript.blocks.len() {
            None
        } else {
            self.transcript.bridge_text(from, self.bridge_max_chars)
        };
        self.last_active_index.remove(&self.active);

        let attachments = std::mem::take(&mut self.attachments);
        let mut shown = text.clone();
        for a in &attachments {
            shown.push_str(&format!("\n[image: {}]", a.label()));
        }
        self.transcript.push_user(shown);
        let block = self.transcript.blocks.len() - 1;
        if self.caps().rewind.conversation {
            self.anchor_pending.push_back(block);
        }
        self.checkpoint_files(block);
        self.start_generation();

        let outgoing = match bridge {
            Some(ctx) => format!(
                "[Context: earlier conversation in this unharness session, possibly with other agents]\n{ctx}\n\n[Current task for {}]:\n{text}",
                self.short_name()
            ),
            None => text,
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
        if !self.is_generating {
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
        if self.is_generating {
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
        let attachments = std::mem::take(&mut self.attachments);
        let mut shown = text.clone();
        for a in &attachments {
            shown.push_str(&format!("\n[image: {}]", a.label()));
        }
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
        if self.is_generating {
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
            self.actions.push_back(Action::Shutdown);
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
            self.actions.push_back(Action::Shutdown);
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

    /// Queue an image for the next prompt.
    pub fn attach(&mut self, path: &str) {
        if !self.caps().image_input {
            self.transcript.push_error(format!(
                "{} does not accept images with the current model",
                self.short_name()
            ));
            return;
        }
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
        match Attachment::image(&path) {
            Some(a) => {
                self.transcript
                    .push_system(format!("Attached {} to the next prompt.", a.label()));
                self.attachments.push(a);
            }
            None => self
                .transcript
                .push_error("only png, jpg, gif and webp images can be attached"),
        }
    }

    pub fn interrupt(&mut self) {
        if self.is_generating {
            self.actions
                .push_back(Action::Command(SessionCommand::Interrupt));
            self.transcript.push_system("Interrupting…");
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
            AgentEvent::Sub { parent, event } => match *event {
                AgentEvent::ToolCallStarted { id, name, input } => {
                    self.transcript
                        .tool_started_in(Some(&parent), &id, &name, input)
                }
                // The subagent's prose streams into its spawning call while
                // that is still running; the call's result replaces it.
                AgentEvent::TextDelta(t) => self.transcript.tool_delta(&parent, &t),
                ev @ (AgentEvent::ToolCallDelta { .. }
                | AgentEvent::ToolCallResult { .. }
                | AgentEvent::PermissionRequest(_)
                | AgentEvent::Sub { .. }
                | AgentEvent::Notice(_)
                | AgentEvent::Error(_)) => self.on_event(ev),
                _ => {}
            },
            AgentEvent::Context(c) => self.context.merge(c),
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
                if let Some(models) = &update.models {
                    let provider = self
                        .current_provider()
                        .map(|p| p.0.clone())
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
                self.finish_generation();
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
            }
        }
    }

    fn on_permission_request(&mut self, req: PermissionRequest) {
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
        self.modal = self.pending_prompts.pop_front().map(Modal::for_request);
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
        if self.session_alive {
            self.actions.push_back(Action::Shutdown);
        }
        self.last_active_index
            .insert(self.active, self.transcript.blocks.len());
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
            "Switched to {} (model: {}, effort: {}, policy: {})",
            self.display_name(),
            self.model_label(),
            self.current_effort().unwrap_or("default"),
            self.effective_policy()
        ));
        if let Some(w) = self.policy_warning() {
            self.transcript.push_notice(w);
        }
    }

    pub fn set_policy(&mut self, p: PermissionPolicy) {
        self.policy_requested = p;
        let res = resolve_policy(&self.caps(), p);
        self.transcript
            .push_system(format!("Permission policy: {}", res.effective));
        if let Some(w) = res.warning {
            self.transcript.push_notice(w);
        }
        if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::SetPolicy(res.effective)));
        }
    }

    pub fn set_provider(&mut self, provider: ProviderId) {
        if self.current_provider() != Some(&provider) {
            self.models.remove(&self.active);
        }
        self.providers.insert(self.active, provider.clone());
        self.transcript
            .push_system(format!("Provider: {provider} (pick a model with Ctrl+M)"));
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
            self.actions.push_back(Action::Shutdown);
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
            self.actions.push_back(Action::Shutdown);
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
        let Some(binary) = self.harness_binary() else {
            self.transcript
                .push_error(format!("{} binary not found", self.short_name()));
            return;
        };
        let providers = match self.harness().list_providers(&binary) {
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

    fn models_for(&mut self, provider: &ProviderId) -> Result<Vec<ModelInfo>, String> {
        let key = (self.active, provider.0.clone());
        if let Some(m) = self.model_cache.get(&key) {
            return Ok(m.clone());
        }
        let binary = self
            .harness_binary()
            .ok_or_else(|| format!("{} binary not found", self.short_name()))?;
        let models = self
            .harness()
            .list_models(&binary, provider)
            .map_err(|e| e.to_string())?;
        self.model_cache.insert(key, models.clone());
        Ok(models)
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
        match self.models_for(&provider) {
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
            .position(|p| *p == self.policy_requested);
        self.modal = Some(Modal::Policy(
            ListPicker::new(PermissionPolicy::ALL.to_vec()).with_selected(idx),
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
                if m.denying {
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
                            Some(Some(ModalChoice::Decision(PermissionDecision::AllowAlways)))
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
                if m.editing_other {
                    match key.code {
                        KeyCode::Enter | KeyCode::Esc => m.editing_other = false,
                        KeyCode::Backspace => {
                            m.other[m.idx].pop();
                        }
                        KeyCode::Char(c) => m.other[m.idx].push(c),
                        _ => {}
                    }
                    None
                } else {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            m.up();
                            None
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            m.down();
                            None
                        }
                        KeyCode::Char(' ') => {
                            m.choose();
                            None
                        }
                        KeyCode::Left => {
                            m.prev_page();
                            None
                        }
                        KeyCode::Right | KeyCode::Tab => {
                            m.next_page();
                            None
                        }
                        KeyCode::Enter => {
                            let page_done = m.choose() || m.has_answer(m.idx);
                            if page_done
                                && !m.next_page()
                                && (0..m.questions.len()).all(|q| m.has_answer(q))
                            {
                                Some(Some(ModalChoice::Decision(PermissionDecision::Answer(
                                    m.answers(),
                                ))))
                            } else {
                                None
                            }
                        }
                        KeyCode::Esc => Some(Some(ModalChoice::Dismiss)),
                        _ => None,
                    }
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
                        self.modal = None;
                        self.set_policy(p);
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
                    Some(p) => self.set_policy(p),
                    None => self.transcript.push_error(format!(
                        "unknown policy '{a}' (ask, accept-edits, auto, bypass)"
                    )),
                },
                None => self.open_policy_picker(),
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
                    self.transcript.push_notice("usage: /attach <image path>")
                }
                _ => {
                    let names: Vec<String> = self.attachments.iter().map(|a| a.label()).collect();
                    self.transcript
                        .push_system(format!("Attached: {}", names.join(", ")));
                }
            },
            "/detach" => {
                self.attachments.clear();
                self.transcript.push_system("Attachments cleared.");
            }
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
                    "Shortcuts: Ctrl+H harness · Ctrl+M model · Ctrl+E effort · Ctrl+P policy · Ctrl+R resume · Ctrl+O expand tool output · Esc/Ctrl+C interrupt or quit\nPrompt: Ctrl+J newline (Shift+Enter too where the terminal can tell it from Enter) · Up/Down move between lines, then through earlier prompts · Home/End (Ctrl+A) line start/end · Ctrl+U clear · Ctrl+G edit in $EDITOR\nTranscript: PageUp/PageDown, Shift+Up/Down or the mouse wheel scroll · End (empty prompt) back to the bottom · drag to select and copy (double click a word, triple a row)\nDuring a turn: Enter queues the prompt · Alt+Enter steers the running turn · Alt+Up edits the last queued prompt",
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
        self.suggestions.clear();
        if !self.input.starts_with('/') || self.input.contains('\n') {
            self.selected_suggestion = 0;
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
            ("/effort" | "/think", Some(s)) => {
                for l in self.caps().effort_levels {
                    if l.starts_with(s) {
                        out.push((format!("{cmd} {l}"), "Reasoning effort".to_string()));
                    }
                }
            }
            ("/model", Some(s)) => {
                if let Some(p) = self.current_provider().cloned()
                    && let Ok(models) = self.models_for(&p)
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
    pub fn should_accept_suggestion(&self) -> bool {
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
    }

    pub fn move_cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
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
        self.update_suggestions();
    }

    /// Show the next older sent prompt, keeping what was typed as a draft.
    pub fn history_older(&mut self) {
        if let Some(text) = self.history.older(&self.input) {
            self.input = text.to_string();
            self.show_recalled();
        }
    }

    /// Show the next newer sent prompt, or the draft after the newest.
    pub fn history_newer(&mut self) {
        if let Some(text) = self.history.newer() {
            self.input = text;
            self.show_recalled();
        }
    }

    fn show_recalled(&mut self) {
        self.cursor = self.input.chars().count();
        // No suggestion list for a recalled command: it would take over
        // the arrows that are stepping through history.
        self.suggestions.clear();
    }

    /// A paste goes in whole, into whichever text field has the keyboard.
    /// It never acts as keystrokes: a modal without a text field ignores
    /// it rather than treat its letters as answers.
    pub fn paste(&mut self, text: &str) {
        let Some(modal) = self.modal.as_mut() else {
            self.insert_str(text);
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
        self.update_suggestions();
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
        true
    }

    /// Start of the line the cursor is on.
    pub fn move_cursor_home(&mut self) {
        let before: Vec<char> = self.input.chars().take(self.cursor).collect();
        self.cursor = before.iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1);
    }

    /// End of the line the cursor is on.
    pub fn move_cursor_end(&mut self) {
        self.cursor += self
            .input
            .chars()
            .skip(self.cursor)
            .take_while(|c| *c != '\n')
            .count();
    }

    pub fn take_input(&mut self) -> String {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.prompt_scroll = 0;
        self.history.reset();
        self.suggestions.clear();
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

    fn mouse_press(&mut self, column: u16, row: u16) {
        self.drag_edge = 0;
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
        match self.selected_text() {
            Some(text) if !text.trim().is_empty() => self.copy_request = Some(text),
            _ => {
                // A plain click, or only blank cells: nothing is selected.
                if self
                    .selection
                    .is_some_and(|s| s.granularity == Granularity::Char)
                {
                    self.selection = None;
                }
            }
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
    Resume(String),
    /// (user block, also restore files)
    Rewind(usize, bool),
    Decision(PermissionDecision),
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
    use crate::core::{PermissionKind, Question};
    use crate::harness::agy::AgyHarness;
    use crate::harness::claude::ClaudeHarness;
    use crossterm::event::KeyEventKind;

    fn test_registry() -> Arc<Registry> {
        Arc::new(
            Registry::empty()
                .with(Box::new(AgyHarness::default()))
                .with(Box::new(ClaudeHarness))
                .with(Box::new(crate::harness::codex::CodexHarness::new(
                    crate::harness::codex::CodexTransport::Exec,
                )))
                .with(Box::new(crate::harness::pi::PiHarness)),
        )
    }

    pub(crate) fn test_app_in(
        cwd: PathBuf,
        harness: HarnessId,
        resume: Option<String>,
        harness_explicit: bool,
    ) -> App {
        App::new(AppInit {
            // Tests never write to the real state directory.
            checkpoint_store: Some(cwd.join(".unharness/test-checkpoints")),
            cwd: cwd.clone(),
            workspace_root: Some(cwd),
            registry: test_registry(),
            config: Config::default(),
            harness,
            policy: PermissionPolicy::Ask,
            provider: None,
            model: None,
            effort: None,
            resume,
            harness_explicit,
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

        // Subagent tool calls land in the transcript; subagent prose does not.
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
        assert_eq!(app.transcript.blocks.len(), before + 1);
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

    #[test]
    fn subagent_calls_hang_off_their_spawner() {
        use super::super::transcript::Block;
        let mut app = test_app(HarnessId::CLAUDE);
        app.submit_prompt("delegate".into());
        app.on_event(AgentEvent::ToolCallStarted {
            id: "spawn".into(),
            name: "Agent".into(),
            input: serde_json::json!({"description": "look around"}),
        });
        let sub = |event| AgentEvent::Sub {
            parent: "spawn".into(),
            event: Box::new(event),
        };
        app.on_event(sub(AgentEvent::ToolCallStarted {
            id: "inner".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "a.txt"}),
        }));
        app.on_event(sub(AgentEvent::ToolCallResult {
            id: "inner".into(),
            output: "contents".into(),
            is_error: false,
        }));
        // The subagent's prose shows as the spawning call's live output.
        app.on_event(sub(AgentEvent::TextDelta("found it".into())));
        let find = |app: &App, want: &str| {
            app.transcript
                .blocks
                .iter()
                .find_map(|b| match b {
                    Block::Tool {
                        id, parent, output, ..
                    } if id == want => Some((parent.clone(), output.clone())),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(
            find(&app, "inner"),
            (Some("spawn".into()), "contents".into())
        );
        assert_eq!(find(&app, "spawn"), (None, "found it".into()));

        // Saved and restored with the link; left out of the bridge.
        let records = app.transcript.to_records();
        let back = Transcript::from_records(&records);
        assert!(back.blocks.iter().any(
            |b| matches!(b, Block::Tool { id, parent: Some(p), .. } if id == "inner" && p == "spawn")
        ));
        let bridge = app.transcript.bridge_text(0, 10_000).unwrap();
        assert!(bridge.contains("[tool Agent") && !bridge.contains("[tool Read"));
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
        app.handle_slash_command("/attach notes.txt");
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
        let mut app = test_app(HarnessId::AGY);
        app.set_policy(PermissionPolicy::Auto);
        assert_eq!(app.effective_policy(), PermissionPolicy::AcceptEdits);
        app.switch_harness(HarnessId::CLAUDE);
        assert_eq!(app.effective_policy(), PermissionPolicy::Auto);
        assert!(app.policy_warning().is_none());
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
                suggestions: None,
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
                    options: vec![("Red".into(), "".into()), ("Blue".into(), "".into())],
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
    fn local_picker_enter_and_esc() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.open_policy_picker();
        app.handle_modal_key(key(KeyCode::Down));
        app.handle_modal_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.policy_requested, PermissionPolicy::AcceptEdits);
        app.open_effort_picker();
        app.handle_modal_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.open_harness_picker();
        app.handle_modal_key(key(KeyCode::Char('q')));
        assert!(app.modal.is_none());
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
    fn paste_into_a_modal_fills_its_text_field_or_is_ignored() {
        let mut app = test_app(HarnessId::CLAUDE);
        app.session_alive = true;
        app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
            id: "p1".into(),
            kind: PermissionKind::ToolUse {
                tool: "Bash".into(),
                input: serde_json::json!({"command":"ls"}),
                suggestions: None,
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
}
