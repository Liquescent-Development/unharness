use super::*;
use crate::tui::modal::ListPicker;

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
    app.controller.bridge_max_chars = Some(500);
    app.submit_prompt("the task".into());
    app.take_actions();
    app.controller.session_alive = true;
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
    assert_eq!(app.controller.active, HarnessId::CLAUDE);
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
    assert_eq!(app.controller.active, HarnessId::CODEX);
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.controller.session_alive = false;
    let saved = app
        .controller
        .store
        .load(&app.controller.conversation.record.id)
        .unwrap();
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
    app.controller.session_alive = true;
    app.on_event(AgentEvent::TextDelta("y".repeat(1_000)));
    done(&mut app);

    // Esc: the switch goes ahead without one.
    app.switch_harness(HarnessId::CLAUDE);
    assert!(sent_turn(&mut app).contains("Write a handoff summary"));
    app.interrupt();
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    assert_eq!(app.controller.active, HarnessId::CLAUDE);
    assert!(app.controller.conversation.transcript.blocks.iter().any(
        |b| matches!(b, Block::Notice(n) if n == "no handoff summary; Claude gets the conversation as it is")
    ));
    app.take_actions();
    app.controller.session_alive = false;

    // Off: no summary asked for.
    app.controller.bridge_summary = BridgeSummary::Never;
    app.switch_harness(HarnessId::CODEX);
    assert_eq!(app.controller.active, HarnessId::CODEX);
}

#[test]
fn switching_harness_bridges_context_once() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("first question".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::TextDelta("first answer".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    app.switch_harness(HarnessId::AGY);
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.controller.session_alive = false;
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
    app.controller.session_alive = true;
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
    assert_eq!(app.controller.active, HarnessId::CLAUDE);

    // Nor is it offered or taken by name.
    for c in "/switch ".chars() {
        app.insert_char(c);
    }
    let offered: Vec<&str> = app.suggestions.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(offered, ["/switch claude"]);
    app.set_input("");
    app.handle_slash_command("/switch codex");
    assert_eq!(app.controller.active, HarnessId::CLAUDE);
    assert!(app.controller.conversation.transcript.blocks.iter().any(|b| matches!(
        b,
        crate::core::transcript::Block::Error(e) if e == "codex is not installed (not found on PATH)"
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
    app.controller.session_alive = true;
    app.on_event(AgentEvent::TextDelta("first answer".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.switch_harness(HarnessId::CLAUDE);
    app.take_actions();
    app.controller.session_alive = false;
    app.handle_slash_command("/hello world");
    assert_eq!(sent_turn(&mut app), "/hello world");
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    app.switch_harness(HarnessId::CODEX);
    app.take_actions();
    app.controller.session_alive = false;
    app.switch_harness(HarnessId::CLAUDE);
    app.take_actions();
    app.controller.session_alive = false;
    app.submit_prompt("next".into());
    let next = sent_turn(&mut app);
    assert!(next.contains("first answer"), "{next}");
}

#[test]
fn the_switch_notice_names_an_effort_only_where_the_harness_has_levels() {
    use super::super::transcript::Block;
    let notice = |app: &App| {
        app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .rev()
            .find_map(|b| match b {
                Block::System(s) if s.starts_with("Switched to") => Some(s.clone()),
                _ => None,
            })
            .expect("a switch notice")
    };
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller.choose_effort(HarnessId::AGY, "high".into());
    app.switch_harness(HarnessId::CODEX);
    assert!(notice(&app).contains("effort: default"), "{}", notice(&app));
    app.switch_harness(HarnessId::AGY);
    let text = notice(&app);
    assert!(text.starts_with("Switched to Antigravity"), "{text}");
    assert!(!text.contains("effort"), "{text}");
}
