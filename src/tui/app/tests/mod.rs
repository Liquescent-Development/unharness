use super::*;
use crate::core::rules::Scope;
use crate::core::transcript::Transcript;
use crate::core::{
    CapsUpdate, ContextUsage, ModelRef, Parser, PermissionDecision, PermissionKind, Question,
    QuestionOption, Rule, ToolAction, Usage,
};
use crate::harness::agy::AgyHarness;
use crate::harness::claude::ClaudeHarness;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use std::path::Path;

mod choices;
mod commands;
mod conversation;
mod conversation_choices;
mod events;
mod input;
mod modal_keys;
mod mouse;
mod permissions;
mod plan;
mod policy;
mod remote;
mod rewind;
mod sandbox;
mod session;
mod subagents;
mod suggestions;
mod switch;
mod turn;

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
        app.controller
            .policy
            .choose_for_conversation(harness, PermissionPolicy::AcceptEdits);
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

fn spawn_block<'a>(app: &'a App, want: &str) -> &'a super::transcript::AgentRun {
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
        unreported_end: false,
    });
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
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

fn has_notice(app: &App, text: &str) -> bool {
    use super::transcript::Block;
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Notice(n) if n == text))
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
    let mut input = json!({"command": command});
    if let Some(cwd) = cwd {
        input["cwd"] = json!(cwd);
    }
    let acp = json!({"jsonrpc": "2.0", "id": "acp", "method": "session/request_permission",
        "params": {"options": [], "toolCall": {"toolCallId": "t2", "name": "exec_command",
            "kind": "execute", "rawInput": input}}});
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

fn notices(app: &App) -> Vec<String> {
    use super::transcript::Block;
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

pub(crate) fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// A session started before anything was sent to it, as the event loop
/// starts one.
fn blank_session(app: &mut App) {
    app.session_alive = true;
    app.session_blank = true;
}

fn starts(actions: &[Action]) -> usize {
    actions
        .iter()
        .filter(|a| matches!(a, Action::StartSession { .. }))
        .count()
}

/// An app on the built-in vibe harness, whose model and effort are
/// chosen when its process starts, and kept by a session as it last ran.
fn vibe_app() -> (App, tempfile::TempDir) {
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
fn prompted_vibe(app: &mut App) {
    app.submit_prompt("remember the word kumquat".into());
    app.take_actions();
    app.session_alive = true;
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
fn vibe_renews(app: &mut App, choice: &str) {
    app.on_event(AgentEvent::SessionRenewed {
        reason: format!("{choice}: {}", crate::harness::vibe::KEEPS_CHOICES),
    });
}

/// An app on vibe with a provider `lan` in `[harnesses.vibe.providers]`.
fn vibe_app_with_provider() -> (App, tempfile::TempDir) {
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
