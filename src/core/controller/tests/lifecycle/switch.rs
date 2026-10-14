use super::*;
use crate::core::controller::handoff_prompt;
use crate::core::transcript::DEFAULT_BRIDGE_MAX_CHARS;

#[test]
fn no_handoff_is_asked_for_while_subagents_are_at_work() {
    let mut app = over_budget_app();
    app.on_event(AgentEvent::SubagentStarted {
        id: "agent-1".into(),
        description: "look around".into(),
        kind: None,
        unreported_end: false,
    });
    app.switch_harness(HarnessId::CODEX);
    assert_eq!(app.controller.active, HarnessId::CODEX);
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
    assert_eq!(app.controller.queued.len(), 1);

    app.switch_harness(HarnessId::CODEX);
    assert!(sent_turn(&mut app).contains("Write a handoff summary"));
    app.on_event(AgentEvent::TextDelta("Goal: the task.".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert_eq!(app.controller.active, HarnessId::CODEX);
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert_eq!(app.controller.queued.len(), 1);
}

#[test]
fn no_handoff_is_asked_of_a_session_that_cannot_be_resumed() {
    let mut app = over_budget_app();
    app.controller.session_alive = false;
    app.controller.overlay(
        HarnessId::CLAUDE,
        CapsUpdate {
            resume_by_id: Some(false),
            ..Default::default()
        },
    );
    app.switch_harness(HarnessId::CODEX);
    assert_eq!(app.controller.active, HarnessId::CODEX);
    assert!(app.take_actions().is_empty());

    // One that can is resumed for it.
    let mut app = over_budget_app();
    app.controller.session_alive = false;
    app.switch_harness(HarnessId::CODEX);
    assert_eq!(app.controller.active, HarnessId::CLAUDE);
    assert!(matches!(
        &app.take_actions()[..],
        [Action::StartSession { resume: Some(id) }, Action::SendTurn { .. }] if id == "claude-1"
    ));
}

/// A handoff asked of a session that could not be resumed is written by
/// a new one, which knows nothing to summarize: the switch goes ahead
/// without it, and the new session is not kept.
#[test]
fn a_handoff_from_a_replaced_session_is_no_summary() {
    use crate::core::transcript::Block;
    let mut app = over_budget_app();
    app.controller.session_alive = false;
    app.switch_harness(HarnessId::CODEX);
    assert!(matches!(
        &app.take_actions()[..],
        [Action::StartSession { resume: Some(id) }, Action::SendTurn { .. }] if id == "claude-1"
    ));
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionReplaced {
        reason: "could not resume the session (Session not found)".into(),
    });
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-2".into(),
        model: None,
    });
    app.on_event(AgentEvent::TextDelta(
        "There is nothing to summarize.".into(),
    ));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert_eq!(app.controller.active, HarnessId::CODEX);
    assert!(
        !app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Handoff { .. }))
    );
    assert!(has_notice(
        &app,
        "no handoff summary; Codex gets the conversation as it is"
    ));
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::CLAUDE)
    );
    assert_eq!(app.controller.bridge_start(HarnessId::CLAUDE), 0);
}

#[test]
fn steering_a_handoff_summary_queues_the_message_for_the_next_harness() {
    use crate::core::transcript::Block;
    let mut app = over_budget_app();
    app.switch_harness(HarnessId::CODEX);
    app.take_actions();
    app.steer("and the tests".into());
    assert!(app.take_actions().is_empty());
    app.on_event(AgentEvent::TextDelta("Goal: the task.".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert_eq!(app.controller.active, HarnessId::CODEX);
    assert!(
        app.controller
            .conversation
            .transcript
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
fn the_bridge_is_measured_by_the_window_its_harness_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.keep();
    let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
    assert_eq!(
        app.controller.bridge_budget(HarnessId::CLAUDE),
        DEFAULT_BRIDGE_MAX_CHARS
    );
    app.on_event(AgentEvent::Context(ContextUsage {
        used: Some(10),
        window: Some(200_000),
    }));
    // A quarter of it, at four characters a token.
    assert_eq!(app.controller.bridge_budget(HarnessId::CLAUDE), 200_000);
    assert_eq!(
        app.controller.bridge_budget(HarnessId::CODEX),
        DEFAULT_BRIDGE_MAX_CHARS
    );

    // Kept for the next run, by model.
    let mut again = test_app_in(cwd, HarnessId::CLAUDE, None, false);
    assert_eq!(again.controller.bridge_budget(HarnessId::CLAUDE), 200_000);
    // On anthropic, the provider it guessed.
    again
        .controller
        .choose_model(HarnessId::CLAUDE, "opus".into());
    assert_eq!(
        again.controller.bridge_budget(HarnessId::CLAUDE),
        DEFAULT_BRIDGE_MAX_CHARS
    );

    // A budget the user set wins.
    again.controller.bridge_max_chars = Some(1_000);
    assert_eq!(again.controller.bridge_budget(HarnessId::CLAUDE), 1_000);
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
        app.controller.bridge_budget(HarnessId::CLAUDE),
        DEFAULT_BRIDGE_MAX_CHARS
    );
    app.on_event(AgentEvent::Context(ContextUsage {
        used: None,
        window: Some(1),
    }));
    assert_eq!(app.controller.bridge_budget(HarnessId::CLAUDE), 8_000);
    app.on_event(AgentEvent::Context(ContextUsage {
        used: None,
        window: Some(u64::MAX),
    }));
    assert_eq!(app.controller.bridge_budget(HarnessId::CLAUDE), 2_000_000);
}

#[test]
fn a_switch_starts_the_next_harness_on_its_session() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller
        .conversation
        .session_ids
        .insert(HarnessId::CODEX, "codex-1".into());
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
