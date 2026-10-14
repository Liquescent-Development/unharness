use super::*;

#[test]
fn question_modal_answers() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller.session_alive = true;
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
    app.controller.session_alive = true;
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
