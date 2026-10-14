//! The active harness's session as the TUI's App drives it, with a front
//! end that records what the controller asks of it in place of the
//! screen. These tests moved here from `tui::app::tests` with the
//! behaviour they check; in them `app` is the controller and that front
//! end, opened as a run opens it (`Controller::open`, `greet`), and the
//! helpers are the App tests' own. The App's tests of the view, of the
//! commands typed and of its `Front` stay with it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::{Controller, Front, Launch, Opening, PlanAsk, PolicyAsk};
use crate::config::Config;
use crate::core::controller::{Action, rate_limit_summary};
use crate::core::conversations::{Conversation, ConversationStore};
use crate::core::registry::Registry;
use crate::core::sandbox::SandboxSetup;
use crate::core::transcript::Transcript;
use crate::core::{
    AgentEvent, Attachment, Capabilities, CapsUpdate, ContextUsage, HarnessId, ModelRef, Parser,
    PermissionPolicy, PermissionRequest, ProviderId, Rules, SessionCommand, StopReason, Usage,
};
use crate::harness::ModelInfo;
use crate::harness::agy::AgyHarness;
use crate::harness::claude::ClaudeHarness;

mod choices;
mod conversation;
mod conversation_choices;
mod events;
mod plan;
mod policy;
mod remote;
mod rewind;
mod session;
mod subagents;
mod switch;
mod turn;

/// What the screen shows of what the controller asked for: one dialog at
/// a time, as the App's modal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shown {
    /// The dialog of the request with this id.
    Request(String),
    /// The policy picker.
    Policy,
}

/// The front end the controller is driven with here: what it was asked
/// to show and change, kept as the App's `Screen` keeps it.
#[derive(Debug, Default)]
struct Recording {
    shown: Option<Shown>,
    attachments: Vec<Attachment>,
    input: String,
    flashes: Vec<String>,
}

impl Front for Recording {
    fn dialog_open(&self) -> bool {
        self.shown.is_some()
    }

    fn ask(&mut self, req: PermissionRequest, _plan: Option<PlanAsk>) {
        self.shown = Some(Shown::Request(req.id));
    }

    fn withdraw(&mut self, id: &str) -> bool {
        if matches!(&self.shown, Some(Shown::Request(open)) if open == id) {
            self.shown = None;
            return true;
        }
        false
    }

    fn drop_request(&mut self) {
        if matches!(self.shown, Some(Shown::Request(_))) {
            self.shown = None;
        }
    }

    fn choosing_policy(&self) -> bool {
        self.shown == Some(Shown::Policy)
    }

    fn close_policy_picker(&mut self) {
        self.shown = None;
    }

    fn ask_policy(&mut self, _ask: PolicyAsk) {
        self.shown = Some(Shown::Policy);
    }

    fn attachments(&mut self) -> &mut Vec<Attachment> {
        &mut self.attachments
    }

    fn put_back(&mut self, text: String, attachments: Vec<Attachment>) {
        self.input = if self.input.is_empty() {
            text
        } else {
            format!("{text}\n{}", self.input)
        };
        self.attachments.extend(attachments);
    }

    fn edit(&mut self, text: String) {
        self.input = text;
    }

    fn follow(&mut self) {}

    fn flash(&mut self, message: String) {
        self.flashes.push(message);
    }

    fn prompt_submitted(&mut self) {}

    fn commands_arrived(&mut self) {}

    fn conversation_installed(&mut self) {
        self.shown = None;
    }
}

/// The controller and the front end it works on, called as the App calls
/// it.
struct TestApp {
    controller: Controller,
    front: Recording,
    /// The directory a helper opened it in, removed after the controller
    /// is dropped (fields are dropped in order).
    dir: Option<tempfile::TempDir>,
}

impl TestApp {
    /// Opened as `App::new` opens it.
    fn new(init: Opening) -> Self {
        let (controller, greeting) = Controller::open(init);
        let mut app = TestApp {
            controller,
            front: Recording::default(),
            dir: None,
        };
        app.with(|c, front| c.greet(greeting, front));
        app
    }

    /// This, keeping `dir`, which it was opened in, until it is dropped.
    fn holding(mut self, dir: tempfile::TempDir) -> Self {
        self.dir = Some(dir);
        self
    }

    fn with<R>(&mut self, f: impl FnOnce(&mut Controller, &mut Recording) -> R) -> R {
        f(&mut self.controller, &mut self.front)
    }

    fn on_event(&mut self, ev: AgentEvent) {
        self.with(|c, front| c.on_event(ev, front));
    }

    fn take_actions(&mut self) -> Vec<Action> {
        self.controller.take_actions()
    }

    fn submit_prompt(&mut self, text: String) {
        self.with(|c, front| c.submit_prompt(text, front));
    }

    fn queue_prompt(&mut self, text: String) {
        self.with(|c, front| c.queue_prompt(text, front));
    }

    fn steer(&mut self, text: String) {
        self.with(|c, front| c.steer(text, front));
    }

    fn interrupt(&mut self) {
        self.with(|c, front| c.interrupt(front));
    }

    /// The App also asks a harness that starts nothing before the first
    /// prompt for its commands here; no test here looks at that.
    fn start_when_idle(&mut self) {
        self.controller.start_when_idle();
    }

    fn start_failed(&mut self, why: String) {
        self.with(|c, front| c.start_failed(why, front));
    }

    fn quit(&mut self) {
        self.with(|c, front| c.quit(front));
    }

    fn switch_harness(&mut self, next: HarnessId) {
        self.with(|c, front| c.switch_harness(next, front));
    }

    fn set_provider(&mut self, provider: ProviderId) {
        self.with(|c, front| c.set_provider(provider, front));
    }

    fn set_model(&mut self, model: String) {
        self.with(|c, front| c.set_model(model, front));
    }

    fn set_effort(&mut self, effort: String) {
        self.with(|c, front| c.set_effort(effort, front));
    }

    fn set_policy(&mut self, p: PermissionPolicy) -> bool {
        self.with(|c, front| c.set_policy(p, front))
    }

    fn new_conversation(&mut self) {
        self.with(|c, front| c.new_conversation(front));
    }

    fn resume_conversation(&mut self, id_or_prefix: String) {
        self.with(|c, front| c.resume_conversation(id_or_prefix, front));
    }

    fn rewind_to(&mut self, block: usize, restore_files: bool) {
        self.with(|c, front| c.rewind_to(block, restore_files, front));
    }

    fn drop_held(&mut self, held: impl IntoIterator<Item = Action>) {
        self.with(|c, front| c.drop_held(held, front));
    }

    fn fork_conversation(&mut self) {
        self.with(|c, front| c.fork_conversation(front));
    }

    /// As the App runs it: refused, it goes back to an empty prompt box.
    fn run_shell(&mut self, command: &str) {
        let typed = format!("!{command}");
        if self.with(|c, front| c.run_shell(command, front)) && self.front.input.is_empty() {
            self.front.input = typed;
        }
    }

    /// `/remote-control` with `args` on a harness that has it.
    fn remote_control_command(&mut self, args: &str) {
        self.controller.remote_control_command(args);
    }

    fn remote_status(&self) -> Option<String> {
        self.controller.remote_status()
    }

    fn effective_policy(&self) -> Option<PermissionPolicy> {
        self.controller.effective_policy(self.controller.active)
    }

    fn offered_policies(&self) -> Vec<PermissionPolicy> {
        self.controller.offered_policies(self.controller.active)
    }

    fn caps(&self) -> Capabilities {
        self.controller.caps()
    }

    fn short_name(&self) -> &'static str {
        self.controller.short_name()
    }

    fn display_name(&self) -> &'static str {
        self.controller.display_name()
    }

    fn current_provider(&self) -> Option<&ProviderId> {
        self.controller.provider(self.controller.active)
    }

    fn chosen_provider(&self) -> Option<&ProviderId> {
        self.controller.chosen_provider(self.controller.active)
    }

    fn current_model(&self) -> Option<&ModelRef> {
        self.controller.model(self.controller.active)
    }

    fn current_effort(&self) -> Option<&str> {
        self.controller.effort(self.controller.active)
    }
}

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

fn test_app_in(
    cwd: PathBuf,
    harness: HarnessId,
    resume: Option<String>,
    harness_explicit: bool,
) -> TestApp {
    let mut app = test_app_asking(cwd, harness, resume, harness_explicit);
    // A harness without `ask` waits for a choice; most tests are not
    // about that.
    if app.effective_policy().is_none() {
        app.controller
            .policy
            .choose_for_conversation(harness, PermissionPolicy::AcceptEdits);
        app.front.shown = None;
    }
    app
}

/// An app as it starts, with `ask` requested whatever the harness.
fn test_app_asking(
    cwd: PathBuf,
    harness: HarnessId,
    resume: Option<String>,
    harness_explicit: bool,
) -> TestApp {
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
) -> TestApp {
    TestApp::new(test_init(
        cwd,
        harness,
        resume,
        harness_explicit,
        config,
        registry,
    ))
}

/// What `test_app_with` opens the controller with, for a test to change.
fn test_init(
    cwd: PathBuf,
    harness: HarnessId,
    resume: Option<String>,
    harness_explicit: bool,
    config: Config,
    registry: Arc<Registry>,
) -> Opening {
    Opening {
        registry,
        config,
        launch: Launch {
            harness,
            policy: None,
            provider: None,
            model: None,
            effort: None,
            default_providers: [
                (HarnessId::CLAUDE, "anthropic".into()),
                (HarnessId::CODEX, "openai".into()),
                (HarnessId::AGY, "google".into()),
            ]
            .into(),
            // Nor to the real config directory.
            rules: Rules::load_in(&cwd.join(".unharness/test-config"), Some(&cwd)).unwrap(),
            cwd: cwd.clone(),
            workspace_root: Some(cwd.clone()),
            sandbox: SandboxSetup::off(),
        },
        resume,
        harness_explicit,
        // Tests never write to the real state directory.
        checkpoint_store: Some(cwd.join(".unharness/test-checkpoints")),
        context_windows: Some(cwd.join(".unharness/test-state/context_windows.json")),
    }
}

fn test_app(harness: HarnessId) -> TestApp {
    let tmp = tempfile::tempdir().unwrap();
    test_app_in(tmp.path().to_path_buf(), harness, None, false).holding(tmp)
}

fn spawn_block<'a>(app: &'a TestApp, want: &str) -> &'a crate::core::transcript::AgentRun {
    app.controller
        .conversation
        .transcript
        .agent(want)
        .unwrap_or_else(|| panic!("no subagent on {want}"))
}

fn sub(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Sub {
        parent: parent.into(),
        event: Box::new(event),
    }
}

fn spawn(app: &mut TestApp, id: &str, description: &str, kind: Option<&str>) {
    app.on_event(AgentEvent::ToolCallStarted {
        id: id.into(),
        name: "Agent".into(),
        input: serde_json::json!({ "description": description }),
    });
    app.on_event(AgentEvent::SubagentStarted {
        id: id.into(),
        description: description.into(),
        kind: kind.map(str::to_string),
        unreported_end: false,
    });
}

/// The single turn the app asked for.
fn sent_turn(app: &mut TestApp) -> String {
    match &app.take_actions()[..] {
        [.., Action::SendTurn { text, .. }] => text.clone(),
        other => panic!("{other:?}"),
    }
}

/// Claude, live, after a turn too long for a bridge of 500 characters.
fn over_budget_app() -> TestApp {
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller.bridge_max_chars = Some(500);
    app.submit_prompt("the task".into());
    app.take_actions();
    app.controller.session_alive = true;
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

fn has_notice(app: &TestApp, text: &str) -> bool {
    use crate::core::transcript::Block;
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Notice(n) if n == text))
}

fn notices(app: &TestApp) -> Vec<String> {
    use crate::core::transcript::Block;
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Notice(n) | Block::Error(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// A session started before anything was sent to it, as the event loop
/// starts one.
fn blank_session(app: &mut TestApp) {
    app.controller.session_alive = true;
    app.controller.session_blank = true;
}

fn starts(actions: &[Action]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, Action::StartSession { .. }))
        .count()
}

/// An app on the built-in vibe harness, whose model and effort are
/// chosen when its process starts, and kept by a session as it last ran.
fn vibe_app() -> (TestApp, tempfile::TempDir) {
    let cwd = tempfile::tempdir().unwrap();
    let registry =
        Arc::new(Registry::empty().with(Box::new(crate::harness::vibe::VibeHarness::new())));
    let app = test_app_with(
        cwd.path().to_path_buf(),
        HarnessId::VIBE,
        None,
        false,
        Config::default(),
        registry,
    );
    (app, cwd)
}

/// A vibe session, live, after one turn.
fn prompted_vibe(app: &mut TestApp) {
    app.submit_prompt("remember the word kumquat".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "v1".into(),
        model: Some("mistral-medium-3.5".into()),
    });
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate::efforts(vec![
        "off".into(),
        "high".into(),
    ])));
}

/// What vibe's driver answers a session told `choice` (the e2e test
/// `vibe_chooses_model_and_effort_at_spawn_never_by_option`).
fn vibe_renews(app: &mut TestApp, choice: &str) {
    app.on_event(AgentEvent::SessionRenewed {
        reason: format!("{choice}: {}", crate::harness::vibe::KEEPS_CHOICES),
    });
}

/// An app on vibe with a provider `lan` in `[harnesses.vibe.providers]`.
fn vibe_app_with_provider() -> (TestApp, tempfile::TempDir) {
    let cwd = tempfile::tempdir().unwrap();
    let settings = crate::config::HarnessSettings {
        rest: toml::toml! {
            [providers.lan]
            api_base = "http://127.0.0.1:8080/v1"
            models = ["qwen3-coder"]
        },
        ..Default::default()
    };
    let vibe = crate::harness::vibe::VibeHarness::from_settings(&settings).unwrap();
    let registry = Arc::new(Registry::empty().with(Box::new(vibe)));
    let app = test_app_with(
        cwd.path().to_path_buf(),
        HarnessId::VIBE,
        None,
        false,
        Config::default(),
        registry,
    );
    (app, cwd)
}
