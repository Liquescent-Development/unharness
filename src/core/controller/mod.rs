//! The session controller: what decides how each harness runs, and the
//! state machine of the active harness's session, apart from what the TUI
//! shows of it. Per harness: the permission policy asked for and in effect
//! ([`PolicyState`]), the provider, model and effort chosen and where each
//! came from ([`ChoiceState`]) with the lists they are chosen from, and
//! what its live sessions reported on top of its declared capabilities
//! ([`Controller::caps_of`]). For the active harness's session: when a
//! choice may be made, how it reaches the session ([`Route`]), and a
//! choice sent that the session has not said how it took
//! ([`PendingChoice`]), which holds back what follows it. And the
//! conversation it runs ([`ConversationState`]): installed as one, saved,
//! and its file checkpoints.
//!
//! The lifecycle of that session is here too: started early or with a
//! prompt, committed to the conversation, replaced, renewed and ended
//! (`session.rs`); turns, the prompts queued behind one, steering,
//! sending a prompt at once and interrupting (`turn.rs`); a switch to another harness and its handoff
//! summary (`switch.rs`); the permission requests it makes
//! (`requests.rs`); its subagents (`subagents.rs`); Remote Control
//! (`remote.rs`); rewind and fork (`rewind.rs`); the sandbox it runs in
//! (`sandbox.rs`); the `!` command (`shell_command.rs`); and what each of
//! its events does (`events.rs`). What that asks of the session and of the
//! event loop is returned as [`Action`]s, which [`run_actions`] carries
//! out against the session, holding a start until the CLI that ended the
//! session before it is gone ([`Ending`]).
//!
//! What any front end tracks of the session's events, with or without that
//! lifecycle, is [`Tracking`]: the subagents at work and the turn ends
//! owed for them, the session's id, model and usage, and who answers a
//! permission request, also with nobody there to ask. The controller keeps
//! one ([`Controller::tracking`]) and drives the parts the TUI uses.
//!
//! The TUI calls into it and reads from it. What it says of the
//! conversation it holds it puts in that conversation's transcript itself;
//! where its logic reads or changes what is on screen (a request's dialog,
//! the policy picker, the prompt box), it goes through the [`Front`] it is
//! given. `headless` resolves its policy through the same [`PolicyState`],
//! and tracks its session's events in a [`Tracking`] of its own.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{BridgeSummary, Config};
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{
    ContextWindows, Conversation, ConversationStore, load_context_windows,
};
use crate::core::guard::Watch;
use crate::core::registry::Registry;
use crate::core::sandbox::{SandboxLevel, SandboxSetup};
use crate::core::{
    Attachment, Capabilities, CapsUpdate, HarnessCommand, HarnessId, ModelInfo, ModelRef,
    PermissionPolicy, PolicySupport, ProviderId, Rules, SessionCommand,
};
use crate::harness::{Harness, resolve_binary};

mod account;
mod choices;
mod conversation;
mod events;
mod front;
mod open;
mod policy;
mod remote;
mod requests;
mod rewind;
mod run;
mod sandbox;
mod session;
mod shell_command;
mod subagents;
mod switch;
mod tracking;
mod turn;

pub use account::{AccountStatus, Accounts};
pub use choices::{Choice, ChoiceState, PendingChoice, Route, SessionNow, subagents_refusal};
pub use conversation::{ConversationState, Keeping};
pub use front::{Front, PlanAsk, PolicyAsk};
pub use open::{Greeting, Opening};
pub use policy::{PolicySet, PolicySource, PolicyState, Refused, configured_policy};
pub use run::{Ending, run_actions};
pub use switch::handoff_prompt;
pub use tracking::{RunningSubagent, Tracking, Unasked};
pub use turn::STOP_GRACE;

/// Where a choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The harness's own: a guess at its default, or what its session
    /// reported.
    Harness,
    /// The configuration's default for the harness.
    Configured,
    /// Chosen for the conversation resumed.
    Conversation,
    /// Chosen in this run: on the command line, or in the TUI.
    User,
}

/// A value and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick<T> {
    pub value: T,
    pub origin: Origin,
}

impl<T> Pick<T> {
    pub fn new(value: T, origin: Origin) -> Self {
        Pick { value, origin }
    }
}

/// What a live session reported, as it changed what the TUI offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapsReport {
    /// What the user is told of it, in order.
    pub notices: Vec<String>,
    /// It listed commands of its own, or said whether it runs them.
    pub commands: bool,
    /// It refused policies, and that moved the one in effect.
    pub refused: Option<Refused>,
}

/// What a run named on its command line, for the harness it starts on,
/// and where it runs.
#[derive(Debug, Clone)]
pub struct Launch {
    pub harness: HarnessId,
    /// The policy named for every harness.
    pub policy: Option<PermissionPolicy>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Each harness's guess at its own provider (`Harness::default_provider`),
    /// which reads the user's vendor configuration.
    pub default_providers: HashMap<HarnessId, ProviderId>,
    /// The session's directory.
    pub cwd: PathBuf,
    /// The workspace root found above it, if any.
    pub workspace_root: Option<PathBuf>,
    /// The sandbox level asked for (flag or config) and the platform's
    /// backend.
    pub sandbox: SandboxSetup,
    /// The user's allow rules, and where "allow always" adds to them.
    pub rules: Rules,
}

/// Side effects the event loop performs on the controller's behalf.
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

impl Action {
    /// A text-only turn.
    pub fn turn(text: impl Into<String>) -> Self {
        Action::SendTurn {
            text: text.into(),
            attachments: Vec::new(),
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
pub(crate) struct Handoff {
    pub(crate) to: HarnessId,
    /// Where its turn began in the transcript.
    pub(crate) start: usize,
    /// Prompts were held when it was asked for (after an interrupt): the
    /// user sends those, also after the switch.
    pub(crate) held: bool,
    /// The next session is a new one of the same harness, for another
    /// model or effort (`Controller::session_renewed`).
    pub(crate) renew: bool,
    /// The session asked for it could not be resumed and runs new
    /// (`AgentEvent::SessionReplaced`): what it writes summarizes nothing.
    pub(crate) blind: bool,
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

/// A prompt sent at once in place of the running turn
/// (`Controller::send_now`), until its own turn begins.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SendingNow {
    /// As it was typed or queued, which is what goes back if it is not
    /// sent.
    pub(crate) prompt: QueuedPrompt,
    /// It was the head of the queue (sent from an empty prompt box), and
    /// goes back there; otherwise it goes back to the prompt box.
    pub(crate) from_queue: bool,
    /// The CLI has it already (`SendNow::Native`) and starts on it by
    /// itself: what the transcript shows of it, once it does.
    pub(crate) with_cli: Option<String>,
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

/// The state that decides how each harness runs, and the active harness's
/// session as it goes. See the module.
pub struct Controller {
    registry: Arc<Registry>,
    /// The run's configuration: each harness's defaults, the bridge's
    /// budget, the MCP servers, what a CLI is started with.
    pub config: Config,
    pub policy: PolicyState,
    choices: HashMap<HarnessId, ChoiceState>,
    /// The models each harness offers on a provider, as last listed.
    model_lists: HashMap<(HarnessId, String), Vec<ModelInfo>>,
    /// The providers each harness offers, as last listed.
    provider_lists: HashMap<HarnessId, Vec<(ProviderId, String)>>,
    /// What live sessions reported on top of the declared capabilities.
    live: HashMap<HarnessId, CapsUpdate>,
    /// A choice sent to the active harness's session that it takes only
    /// between turns, until it has said how it took it: prompts typed
    /// meanwhile wait, and another choice and what would end the session
    /// are refused.
    pending: Option<PendingChoice>,
    /// The conversation it runs, installed as one (`Controller::install`).
    pub conversation: ConversationState,
    /// Where the workspace's conversations are kept.
    pub store: ConversationStore,
    /// Saving it failed, which is said once per run.
    persist_failed: bool,
    /// File checkpoints of the workspace; `None` outside a git repository
    /// or when switched off.
    checkpoints: Option<Checkpoints>,
    checkpoint_failed: bool,
    restore_seq: usize,

    /// The harness the conversation runs on now.
    pub active: HarnessId,
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    /// The sandbox level asked for (flag, config, or `/sandbox`) and the
    /// platform's backend. A change reaches the harness's next process.
    pub sandbox: SandboxSetup,
    /// The level the live session's process was started under.
    pub session_sandbox_level: Option<SandboxLevel>,
    /// The active harness's own configuration files as they were when its
    /// session started, to notice a change (`core::guard`).
    pub guard: Option<Watch>,
    /// The bridge's budget, when the user set one.
    pub bridge_max_chars: Option<usize>,
    pub(crate) bridge_summary: BridgeSummary,
    /// What the bridge to a harness is measured against otherwise.
    context_windows: ContextWindows,
    context_windows_path: PathBuf,
    pub(crate) handoff: Option<Handoff>,
    pub session_alive: bool,
    /// The vendor's remote control the user put the active harness's
    /// session on (`/remote-control`). It outlives the harness's process:
    /// each new one is put on it again (`remote_control_resend`); a
    /// switch, a fork, a resume, `/clear` and quit end it.
    remote: Option<RemoteSession>,
    /// The live or starting session was started before anything was sent
    /// to it (so that its commands are listed): until the first prompt it
    /// is not part of the conversation.
    pub(crate) session_blank: bool,
    /// The id (and model) such a session reported, committed with the
    /// first prompt.
    blank_session: Option<(String, Option<String>)>,
    /// The last session ended by itself or could not start: the next
    /// one waits for the user (a prompt, a switch, a resume).
    idle_start_paused: bool,
    /// The session's start waits for the CLI that ended the one before it
    /// to be gone ([`Ending`]): what would start another is refused
    /// meanwhile, as during a turn.
    pub start_held: bool,

    pub is_generating: bool,
    pub generation_start: Option<Instant>,
    /// When the running turn was asked to stop, and whether the user has
    /// been told it has not yet.
    pub(crate) interrupted: Option<(Instant, bool)>,
    /// What each harness reported of its account in this run.
    accounts: Accounts,
    /// What it tracks of the session's events as any front end would. The
    /// TUI drives the subagents at work (`subagent_started`, `_progress`,
    /// `_tool_started`, `_ended`, `clear_subagents`, `awaited`) and its
    /// permission requests (`route`, `wait`, `next_waiting`, `withdraw`,
    /// `drop_waiting`), with `planning` from the policy in effect. The rest
    /// is for a run with nobody to ask (`--print`, `headless::Headless`),
    /// which drives it in its own `Tracking`: here the session's id, model
    /// and usage stay unset (`session_started` and `usage_reported` are not
    /// called; the conversation keeps them per harness), `owed` stays 0
    /// (`report_turns`, `turn_expected`, `turn_ended` and `tool_result`
    /// are not called), and nothing asks `unawaited` or `nobody_to_ask`.
    pub tracking: Tracking,
    /// Prompts entered during a turn; one is sent each time a turn completes.
    pub queued: VecDeque<QueuedPrompt>,
    /// The prompt sent at once, waiting for the running turn to end.
    pub(crate) sending_now: Option<SendingNow>,
    /// The `!` command running now.
    pub shell: Option<ShellRun>,
    compacting: bool,
    /// The harnesses already told which MCP servers they do not get.
    mcp_warned: HashSet<HarnessId>,
    /// The harnesses whose [`Harness::session_notice`] was shown.
    session_noticed: HashSet<HarnessId>,
    pub should_quit: bool,
    actions: VecDeque<Action>,
}

impl Controller {
    /// Every harness starts with its configured provider, else its guess at
    /// its own, and its configured model and effort; the run's harness with
    /// what `launch` names. A configured model of a harness whose provider
    /// is not known is given to the run's harness only. It runs a new
    /// conversation, kept and checkpointed as `keeping` says, on the run's
    /// harness, in its directory, under its sandbox and allow rules, with
    /// no session yet.
    pub fn new(registry: Arc<Registry>, config: Config, launch: Launch, keeping: Keeping) -> Self {
        let mut choices: HashMap<HarnessId, ChoiceState> = HashMap::new();
        for h in registry.all() {
            let id = h.descriptor().id;
            let settings = config.harness(id.as_str());
            let provider = match settings.and_then(|s| s.default_provider.clone()) {
                Some(p) => Some(Pick::new(ProviderId::new(p), Origin::Configured)),
                None => launch
                    .default_providers
                    .get(&id)
                    .map(|p| Pick::new(p.clone(), Origin::Harness)),
            };
            let state = choices.entry(id).or_default();
            if let (Some(p), Some(m)) = (&provider, settings.and_then(|s| s.default_model.clone()))
            {
                state.model = Some(Pick::new(
                    ModelRef::new(id, p.value.clone(), m),
                    Origin::Configured,
                ));
            }
            if let Some(e) = settings.and_then(|s| s.default_effort.clone()) {
                state.effort = Some(Pick::new(e, Origin::Configured));
            }
            state.provider = provider;
        }
        let state = choices.entry(launch.harness).or_default();
        if let Some(p) = launch.provider {
            state.provider = Some(Pick::new(ProviderId::new(p), Origin::User));
        }
        // `launch.model` is only a flag: the configured model of a harness
        // whose provider is not known yet is still given to it.
        let configured_model = config
            .harness(launch.harness.as_str())
            .and_then(|s| s.default_model.clone());
        if let Some(m) = launch
            .model
            .clone()
            .or_else(|| configured_model.filter(|_| state.model.is_none()))
        {
            let origin = if launch.model.is_some() {
                Origin::User
            } else {
                Origin::Configured
            };
            let p = state
                .provider()
                .cloned()
                .unwrap_or_else(|| ProviderId::new("default"));
            state.model = Some(Pick::new(ModelRef::new(launch.harness, p, m), origin));
        }
        if let Some(e) = launch.effort {
            state.effort = Some(Pick::new(e, Origin::User));
        }
        let bridge_max_chars = config.bridge_max_chars;
        let bridge_summary = config.bridge_summary.unwrap_or_default();
        Controller {
            registry,
            config,
            policy: PolicyState::new(launch.policy),
            choices,
            model_lists: HashMap::new(),
            provider_lists: HashMap::new(),
            live: HashMap::new(),
            pending: None,
            conversation: ConversationState::new(Conversation::new(launch.harness), launch.harness),
            store: keeping.store,
            persist_failed: false,
            checkpoints: keeping.checkpoints,
            checkpoint_failed: false,
            restore_seq: 0,
            active: launch.harness,
            cwd: launch.cwd,
            workspace_root: launch.workspace_root,
            sandbox: launch.sandbox,
            session_sandbox_level: None,
            guard: None,
            bridge_max_chars,
            bridge_summary,
            context_windows: load_context_windows(&keeping.context_windows),
            context_windows_path: keeping.context_windows,
            handoff: None,
            session_alive: false,
            remote: None,
            session_blank: false,
            blank_session: None,
            idle_start_paused: false,
            start_held: false,
            is_generating: false,
            generation_start: None,
            interrupted: None,
            accounts: Accounts::new(),
            tracking: Tracking::new(launch.rules),
            queued: VecDeque::new(),
            sending_now: None,
            shell: None,
            compacting: false,
            mcp_warned: HashSet::new(),
            session_noticed: HashSet::new(),
            should_quit: false,
            actions: VecDeque::new(),
        }
    }

    /// `harness`'s session process is ended: what was sent to it will not
    /// be reported back, and the next one reports where it runs and on
    /// which model.
    pub fn process_ended(&mut self, harness: HarnessId) {
        self.policy.process_ended();
        if let Some(state) = self.choices.get_mut(&harness) {
            state.runs_on = None;
            state.reported_model = None;
        }
    }

    /// The harnesses whose capabilities it reads.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Declared capabilities of `harness` plus what a session of it
    /// reported in this run (an ACP agent says at `initialize` whether it
    /// can fork, a Codex session under `transport = "auto"` that came to
    /// `exec` what it lacks); `None` for one not in the registry.
    pub fn caps_of(&self, harness: HarnessId) -> Option<Capabilities> {
        let mut caps = self.registry.get(harness)?.capabilities();
        if let Some(update) = self.live.get(&harness) {
            caps.apply(update);
        }
        Some(caps)
    }

    /// What `harness` offers of the policies, as its sessions left it.
    fn policies(&self, harness: HarnessId) -> Vec<PolicySupport> {
        self.caps_of(harness)
            .map(|c| c.permission_policies)
            .unwrap_or_default()
    }

    /// The display name of `harness`, for what is said of it.
    fn display_name_of(&self, harness: HarnessId) -> &'static str {
        self.registry
            .get(harness)
            .map_or("the harness", |h| h.descriptor().display_name)
    }

    fn short_name_of(&self, harness: HarnessId) -> &'static str {
        self.registry
            .get(harness)
            .map_or("the harness", |h| h.descriptor().short_name)
    }

    /// The active harness.
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

    /// Declared capabilities of the active harness plus what its session
    /// reported (`Controller::caps_of`).
    pub fn caps(&self) -> Capabilities {
        self.caps_of(self.active)
            .unwrap_or_else(|| self.harness().capabilities())
    }

    /// Where the active harness's CLI is.
    pub fn harness_binary(&self) -> Option<PathBuf> {
        let d = self.harness().descriptor();
        resolve_binary(d, self.config.binary_override(d.id.as_str()))
    }

    /// What the event loop is to do for it, oldest first.
    pub fn take_actions(&mut self) -> Vec<Action> {
        self.actions.drain(..).collect()
    }

    /// A handoff summary is being written.
    pub fn writing_handoff(&self) -> bool {
        self.handoff.is_some()
    }

    /// The running turn compacts the context.
    pub fn compacting(&self) -> bool {
        self.compacting
    }

    /// `harness`'s live session reported `update`. A policy it refused or
    /// lacks moves the one in effect as an unsupported policy would; a
    /// live session (`alive`) is told where it went.
    pub fn report(&mut self, harness: HarnessId, update: CapsUpdate, alive: bool) -> CapsReport {
        let mut notices = Vec::new();
        if let Some(levels) = &update.effort_levels
            && self
                .caps_of(harness)
                .is_some_and(|c| c.effort_levels != *levels)
        {
            notices.push(if levels.is_empty() {
                "no effort levels now".to_string()
            } else {
                format!("effort levels now: {}", levels.join(", "))
            });
        }
        // A guess at the harness's own default gives way to what it
        // reports. A choice stays the user's, and is what the next session
        // is told; the driver reported the mismatch and the status line
        // keeps showing it.
        if let Some(reported) = &update.provider {
            let name = self.short_name_of(harness);
            let state = self.choices.entry(harness).or_default();
            if state.chosen_provider().is_some() {
                state.runs_on = (state.provider() != Some(reported)).then(|| reported.clone());
            } else if state.provider() != Some(reported) {
                notices.push(format!(
                    "{name} runs on {reported}, chosen by its own configuration"
                ));
                state.provider = Some(Pick::new(reported.clone(), Origin::Harness));
                if let Some(m) = state.model.as_mut() {
                    m.value.provider = reported.clone();
                }
            }
        }
        if let Some(models) = &update.models {
            let provider = models
                .first()
                .map(|m| m.model_ref.provider.0.clone())
                .or_else(|| self.provider(harness).map(|p| p.0.clone()))
                .unwrap_or_else(|| "default".into());
            self.model_lists.insert((harness, provider), models.clone());
        }
        let commands = update.commands.is_some() || update.slash_commands.is_some();
        let refusal = update.refused_policies.is_some() || update.lacks_policies.is_some();
        let before = self.effective_policy(harness);
        self.live.entry(harness).or_default().merge(update);
        let refused = if refusal {
            let policies = self.policies(harness);
            self.policy
                .on_refused(&self.config, harness, before, &policies, alive)
        } else {
            None
        };
        CapsReport {
            notices,
            commands,
            refused,
        }
    }

    /// `harness`'s session is gone. What it refused was for it
    /// (`CapsUpdate::refused_policies`: a mode that did not take, a
    /// profile shadowed): the next session is asked for the wanted policy
    /// again, and refuses it again if it still cannot hold it. The rest of
    /// what it reported is the agent's, kept until the next one says, the
    /// policies it lacks for the run (`CapsUpdate::lacks_policies`) among
    /// it. Returns whether it had refused any.
    pub fn drop_refusals(&mut self, harness: HarnessId) -> bool {
        self.live
            .get_mut(&harness)
            .is_some_and(|u| u.refused_policies.take().is_some())
    }

    /// `harness`'s session refused a policy.
    pub fn refused_policies(&self, harness: HarnessId) -> bool {
        self.live
            .get(&harness)
            .and_then(|u| u.refused_policies.as_ref())
            .is_some_and(|r| !r.is_empty())
    }

    /// The commands of its own `harness`'s session listed.
    pub fn harness_commands(&self, harness: HarnessId) -> &[HarnessCommand] {
        self.live
            .get(&harness)
            .and_then(|u| u.commands.as_deref())
            .unwrap_or_default()
    }

    /// Whether `harness` has listed its commands, by a session or asked.
    pub fn commands_listed(&self, harness: HarnessId) -> bool {
        self.live
            .get(&harness)
            .is_some_and(|c| c.commands.is_some())
    }

    /// `harness` listed `commands` without a session.
    pub fn commands_arrived(&mut self, harness: HarnessId, commands: Vec<HarnessCommand>) {
        self.live.entry(harness).or_default().merge(CapsUpdate {
            slash_commands: Some(!commands.is_empty()),
            commands: Some(commands),
            ..Default::default()
        });
    }

    /// Take what a resumed `conversation` was run with, its policy, models
    /// and efforts, except where this run named its own. What the last
    /// conversation chose goes back to the configured default. Returns
    /// what the user is told, in order.
    pub fn take_conversation(&mut self, conversation: &Conversation) -> Vec<String> {
        self.policy.take_conversation(conversation);
        let harnesses: Vec<HarnessId> = self.choices.keys().copied().collect();
        for h in harnesses {
            let model = self.configured_model(h);
            let effort = self
                .config
                .harness(h.as_str())
                .and_then(|s| s.default_effort.clone());
            let Some(state) = self.choices.get_mut(&h) else {
                continue;
            };
            if state
                .model
                .as_ref()
                .is_some_and(|m| m.origin == Origin::Conversation)
            {
                state.model = model.map(|m| Pick::new(m, Origin::Configured));
            }
            if state
                .effort
                .as_ref()
                .is_some_and(|e| e.origin == Origin::Conversation)
            {
                state.effort = effort.map(|e| Pick::new(e, Origin::Configured));
            }
        }

        let mut notices = Vec::new();
        let mut models: Vec<(HarnessId, ModelRef)> = conversation
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
            let state = self.choices.entry(h).or_default();
            if state
                .model
                .as_ref()
                .is_some_and(|m| m.origin == Origin::User)
            {
                continue;
            }
            match state.provider() {
                Some(p) if *p != m.provider => {
                    let name = harness.descriptor().short_name;
                    notices.push(format!(
                        "the model chosen for {name} in this conversation ({}) is not used: {name} is on {p} now",
                        m.label(),
                    ));
                    left.insert(h);
                }
                _ => state.model = Some(Pick::new(m, Origin::Conversation)),
            }
        }
        for (h, e) in &conversation.efforts {
            if left.contains(h) || self.registry.get(*h).is_none() {
                continue;
            }
            let state = self.choices.entry(*h).or_default();
            if state
                .effort
                .as_ref()
                .is_some_and(|e| e.origin == Origin::User)
            {
                continue;
            }
            state.effort = Some(Pick::new(e.clone(), Origin::Conversation));
        }
        notices
    }

    /// What the conversation keeps of the choices. `prompted`: `active`'s
    /// live session was sent something, so the policy in effect is the
    /// one it last ran under.
    fn sync_choices(&mut self, active: HarnessId, prompted: bool) {
        let last = if prompted {
            self.effective_policy(active)
        } else {
            None
        };
        let conversation = &mut self.conversation.record;
        self.policy.sync_conversation(conversation);
        for (h, state) in &self.choices {
            if let Some(m) = state.chosen_model() {
                conversation.models.insert(*h, m.clone());
            }
            if let Some(e) = state.chosen_effort() {
                conversation.efforts.insert(*h, e.clone());
            }
        }
        if let Some(p) = last {
            conversation.last_policies.insert(active, p);
        }
    }

    /// Take `update` as if a session of `harness` had reported it, without
    /// what a report sets off.
    #[cfg(test)]
    pub fn overlay(&mut self, harness: HarnessId, update: CapsUpdate) {
        self.live.entry(harness).or_default().merge(update);
    }
}

#[cfg(test)]
mod tests;
