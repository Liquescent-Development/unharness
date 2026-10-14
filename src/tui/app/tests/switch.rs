use super::*;
use crate::tui::app::switch::handoff_prompt;
use crate::tui::modal::ListPicker;
use crate::tui::transcript::DEFAULT_BRIDGE_MAX_CHARS;

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
    app.controller.overlay(
        HarnessId::CLAUDE,
        CapsUpdate {
            resume_by_id: Some(false),
            ..Default::default()
        },
    );
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

/// A handoff asked of a session that could not be resumed is written by
/// a new one, which knows nothing to summarize: the switch goes ahead
/// without it, and the new session is not kept.
#[test]
fn a_handoff_from_a_replaced_session_is_no_summary() {
    use super::super::transcript::Block;
    let mut app = over_budget_app();
    app.session_alive = false;
    app.switch_harness(HarnessId::CODEX);
    assert!(matches!(
        &app.take_actions()[..],
        [Action::StartSession { resume: Some(id) }, Action::SendTurn { .. }] if id == "claude-1"
    ));
    app.session_alive = true;
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
    assert_eq!(app.active, HarnessId::CODEX);
    assert!(
        !app.transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Handoff { .. }))
    );
    assert!(has_notice(
        &app,
        "no handoff summary; Codex gets the conversation as it is"
    ));
    assert!(!app.session_ids.contains_key(&HarnessId::CLAUDE));
    assert_eq!(app.bridge_start(HarnessId::CLAUDE), 0);
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
    // On anthropic, the provider it guessed.
    again
        .controller
        .choose_model(HarnessId::CLAUDE, "opus".into());
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
