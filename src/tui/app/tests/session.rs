use super::*;
use crate::harness::Harness;

/// A harness that could not resume the conversation's session runs a
/// new one (#187): the next prompt tells it the conversation, within
/// the bridge's budget, and the conversation keeps the new session.
#[test]
fn a_session_in_place_of_one_not_resumed_is_told_the_conversation() {
    let mut app = over_budget_app();
    app.session_alive = false;
    app.start_when_idle();
    assert!(matches!(
        &app.take_actions()[..],
        [Action::StartSession { resume: Some(id) }] if id == "claude-1"
    ));
    app.session_alive = true;
    app.on_event(AgentEvent::SessionReplaced {
        reason: "could not resume the session (Session not found)".into(),
    });
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-2".into(),
        model: None,
    });
    assert!(has_notice(
        &app,
        "could not resume the session (Session not found): Claude starts a new session, and your next prompt tells it the conversation so far"
    ));
    app.submit_prompt("what was the task?".into());
    let sent = sent_turn(&mut app);
    assert!(
        sent.starts_with("[Context: earlier conversation") && sent.contains("User: the task"),
        "{sent}"
    );
    // Within the budget of 500 characters.
    assert!(!sent.contains(&"x".repeat(600)), "{sent}");
    assert!(sent.ends_with("what was the task?"), "{sent}");
    assert_eq!(
        app.controller
            .conversation
            .session_ids
            .get(&HarnessId::CLAUDE)
            .map(String::as_str),
        Some("claude-2")
    );
    let saved = app
        .controller
        .store
        .load(&app.controller.conversation.record.id)
        .unwrap();
    assert_eq!(
        saved.sessions.get(&HarnessId::CLAUDE).map(String::as_str),
        Some("claude-2")
    );
}

/// pi reports the session it made in place of a missing one under the
/// id it was given, and before the listing that shows it empty.
#[test]
fn a_session_replaced_under_the_same_id_is_told_the_conversation() {
    let mut app = test_app(HarnessId::PI);
    app.controller
        .conversation
        .transcript
        .push_user("The code word is pineapple.");
    app.controller
        .conversation
        .transcript
        .append_assistant("pi", "ok");
    app.controller
        .conversation
        .session_ids
        .insert(HarnessId::PI, "pi-1".into());
    app.session_alive = true;
    app.session_blank = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "pi-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::SessionReplaced {
        reason: "could not resume the session".into(),
    });
    app.submit_prompt("What was the code word?".into());
    let sent = sent_turn(&mut app);
    assert!(sent.contains("pineapple"), "{sent}");
    assert_eq!(
        app.controller
            .conversation
            .session_ids
            .get(&HarnessId::PI)
            .map(String::as_str),
        Some("pi-1")
    );
}

/// A replacement found after the prompt that started it went (without
/// the conversation, the old session having seen it): the next prompt
/// tells the new session all of it.
#[test]
fn a_session_replaced_under_a_prompt_is_told_the_conversation_next() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("The code word is pineapple. Reply with: ok".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TextDelta("ok".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    // It ended by itself: the next prompt starts it to resume.
    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    app.take_actions();
    app.submit_prompt("Say hi".into());
    assert!(matches!(
        &app.take_actions()[..],
        [Action::StartSession { resume: Some(id) }, Action::SendTurn { text, .. }]
            if id == "claude-1" && text == "Say hi"
    ));
    app.session_alive = true;
    app.on_event(AgentEvent::SessionReplaced {
        reason: "this agent cannot resume sessions".into(),
    });
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-2".into(),
        model: None,
    });
    assert!(has_notice(
        &app,
        "this agent cannot resume sessions: Claude runs a new session without the conversation so far; your next prompt tells it"
    ));
    app.on_event(AgentEvent::TextDelta("hi".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.submit_prompt("What was the code word?".into());
    let sent = sent_turn(&mut app);
    assert!(
        sent.starts_with("[Context: earlier conversation") && sent.contains("pineapple"),
        "{sent}"
    );
    assert_eq!(
        app.controller
            .conversation
            .session_ids
            .get(&HarnessId::CLAUDE)
            .map(String::as_str),
        Some("claude-2")
    );
    // Told once.
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.submit_prompt("Thanks".into());
    assert_eq!(sent_turn(&mut app), "Thanks");
}

/// pi does not send a prompt that started a resume of a session it no
/// longer has (#214): the turn fails, and the prompt sent again goes
/// with the conversation.
#[test]
fn a_prompt_a_replaced_session_was_not_sent_goes_again_with_the_conversation() {
    let mut app = test_app(HarnessId::PI);
    app.submit_prompt("The code word is pineapple. Reply with: ok".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "pi-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TextDelta("ok".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    app.take_actions();
    app.submit_prompt("Say hi".into());
    assert!(matches!(
        &app.take_actions()[..],
        [Action::StartSession { resume: Some(id) }, Action::SendTurn { .. }] if id == "pi-1"
    ));
    app.session_alive = true;
    // pi names the id it made an empty session under, then lists it.
    app.on_event(AgentEvent::SessionStarted {
        session_id: "pi-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::SessionReplaced {
        reason: "could not resume the session (pi has none under its id, and started an empty one)"
            .into(),
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Error(
            "pi: the session to resume is gone, so this prompt was not sent; send it again to have the new session told the conversation".into(),
        ),
    });
    app.submit_prompt("Say hi".into());
    let sent = sent_turn(&mut app);
    assert!(
        sent.starts_with("[Context: earlier conversation")
            && sent.contains("pineapple")
            && sent.ends_with("Say hi"),
        "{sent}"
    );
}

/// Claude, with something to say before each session.
struct Noting(ClaudeHarness);

impl Harness for Noting {
    fn descriptor(&self) -> &'static crate::harness::HarnessDescriptor {
        self.0.descriptor()
    }
    fn capabilities(&self) -> crate::core::Capabilities {
        self.0.capabilities()
    }
    fn probe(&self, binary_override: Option<&Path>) -> crate::harness::Probe {
        self.0.probe(binary_override)
    }
    fn list_models(
        &self,
        binary: &Path,
        provider: &crate::core::ProviderId,
        sandbox: &crate::core::Sandbox,
    ) -> anyhow::Result<Vec<ModelInfo>> {
        self.0.list_models(binary, provider, sandbox)
    }
    fn session_notice(&self, cwd: &Path) -> Option<String> {
        Some(format!("about {}", cwd.display()))
    }
    fn start_session(
        &self,
        cfg: crate::core::SessionConfig,
    ) -> anyhow::Result<crate::core::SessionHandle> {
        self.0.start_session(cfg)
    }
    fn build_print_command(
        &self,
        cfg: &crate::harness::PrintConfig,
    ) -> anyhow::Result<std::process::Command> {
        self.0.build_print_command(cfg)
    }
}

/// A session starts again on every model, policy or sandbox change:
/// the harness's notice is said with the first.
#[test]
fn a_session_notice_is_said_once() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let registry = Arc::new(Registry::empty().with(Box::new(Noting(ClaudeHarness::default()))));
    let mut app = test_app_with(
        cwd.clone(),
        HarnessId::CLAUDE,
        None,
        false,
        Config::default(),
        registry,
    );
    let before = notices(&app).len();
    app.show_session_notice();
    app.show_session_notice();
    let said = notices(&app);
    assert_eq!(said.len(), before + 1, "{said:?}");
    assert_eq!(said.last().unwrap(), &format!("about {}", cwd.display()));
    // A harness with nothing to say adds nothing.
    let mut app = test_app_in(cwd, HarnessId::CLAUDE, None, false);
    let before = notices(&app).len();
    app.show_session_notice();
    assert_eq!(notices(&app).len(), before);
}

#[test]
fn mcp_servers_go_to_harnesses_that_take_them_and_the_rest_is_said_once() {
    use super::super::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller.config.mcp_servers.insert(
        "files".into(),
        crate::core::mcp::McpServerSettings {
            command: Some("files-mcp".into()),
            ..Default::default()
        },
    );
    let notices = |app: &App| {
        app.controller
            .conversation
            .transcript
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

#[test]
fn an_unprompted_session_is_not_saved_until_the_first_prompt() {
    let mut app = test_app(HarnessId::CODEX);
    blank_session(&mut app);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "codex-1".into(),
        model: None,
    });
    assert!(app.controller.conversation.session_ids.is_empty());
    assert!(app.controller.store.list().is_empty());
    assert!(!notices(&app).iter().any(|n| n.contains("codex-1")));
    app.quit();
    assert!(
        app.controller.store.list().is_empty(),
        "quitting saved nothing"
    );

    let mut app = test_app(HarnessId::CODEX);
    blank_session(&mut app);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "codex-1".into(),
        model: None,
    });
    app.submit_prompt("hello".into());
    assert_eq!(app.take_actions(), vec![Action::turn("hello")]);
    assert_eq!(
        app.controller
            .conversation
            .session_ids
            .get(&HarnessId::CODEX)
            .map(String::as_str),
        Some("codex-1")
    );
    assert_eq!(app.controller.store.list().len(), 1);
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

/// An ACP agent that is not signed in fails the turn (or the session
/// started early) with what to run, and its driver ends the process:
/// told once, and nothing starts again until the user does something.
#[test]
fn an_agent_that_is_not_signed_in_is_told_once_and_waits_for_the_user() {
    let why = "vibe is not signed in (Missing API key for mistral provider). It offers: \
               \"Sign in through Mistral AI Studio\". unharness does not sign agents in: \
               run `vibe --setup` in a terminal, then try again";
    let told = |app: &App| {
        let notices = notices(app);
        assert!(!notices.iter().any(|n| n.contains("exited")), "{notices:?}");
        notices
            .iter()
            .filter(|n| n.contains("vibe --setup"))
            .count()
    };
    let failed = || AgentEvent::TurnCompleted {
        stop_reason: StopReason::Error(why.into()),
    };

    // Started before the first prompt.
    let mut app = test_app(HarnessId::CODEX);
    blank_session(&mut app);
    app.on_event(failed());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.on_event(AgentEvent::ProcessExited { code: Some(0) });
    app.start_when_idle();
    assert!(app.take_actions().is_empty());
    assert_eq!(told(&app), 1);

    // Prompted: the turn fails and the process goes by itself.
    let mut app = test_app(HarnessId::CODEX);
    app.start_when_idle();
    app.take_actions();
    app.session_alive = true;
    app.submit_prompt("hello".into());
    app.take_actions();
    assert!(app.is_generating);
    app.on_event(failed());
    app.on_event(AgentEvent::ProcessExited { code: Some(0) });
    assert!(!app.is_generating);
    assert_eq!(told(&app), 1);
    app.start_when_idle();
    assert!(app.take_actions().is_empty());
    app.submit_prompt("hello".into());
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
