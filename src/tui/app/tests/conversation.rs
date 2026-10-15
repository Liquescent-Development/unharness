use super::*;

/// What a conversation and the screen showing it hold, set on purpose so
/// that a test can see it go.
fn leave_traces(app: &mut App) {
    use crate::tui::selection::{Granularity, Point, Selection};
    // The conversation's: a file restore to undo, the last turn's duration
    // and usage, the context's fill, an anchor still owed.
    let c = &mut app.controller.conversation;
    c.restore_undo = Some("undo".into());
    c.generation_duration = Some(Duration::from_secs(3));
    c.turn_usage = Usage {
        input: 1,
        ..Default::default()
    };
    c.context = ContextUsage {
        used: Some(1),
        window: Some(10),
    };
    c.window_reported = true;
    c.anchor_pending.push_back(0);
    // The screen's: a subagent's transcript in view, text selected, the
    // transcript scrolled, a picker open and a request waiting behind it.
    spawn(app, "spawn", "look around", None);
    app.on_event(AgentEvent::SubagentEnded {
        id: "spawn".into(),
        status: crate::core::SubagentStatus::Completed,
        result: None,
    });
    app.open_subagent("spawn");
    app.selection = Some(Selection {
        anchor: Point::default(),
        focus: Point { line: 0, col: 3 },
        granularity: Granularity::Char,
    });
    app.scroll = 5;
    app.auto_scroll = false;
    app.open_policy_picker();
    app.controller
        .tracking
        .pending_prompts
        .push_back(PermissionRequest {
            id: "r1".into(),
            kind: PermissionKind::Confirm {
                title: "Go on?".into(),
                message: None,
            },
            tool_call_id: None,
        });
    app.controller.tracking.asked_planning.insert("r1".into());
}

/// What is left of the traces is `usage` and `context`, the
/// conversation's own.
fn assert_no_traces(app: &App, after: &str, usage: Usage, context: ContextUsage) {
    let c = &app.controller.conversation;
    assert_eq!(c.restore_undo, None, "{after}");
    assert_eq!(c.generation_duration, None, "{after}");
    assert_eq!(c.turn_usage, usage, "{after}");
    assert_eq!(c.context, context, "{after}");
    assert!(c.anchor_pending.is_empty(), "{after}");
    assert!(
        app.viewing.is_none() && app.subagent_focus.is_none(),
        "{after}"
    );
    assert!(app.selection.is_none(), "{after}");
    assert_eq!((app.scroll, app.auto_scroll), (0, true), "{after}");
    assert!(app.modal.is_none(), "{after}");
    assert!(
        app.controller.tracking.pending_prompts.is_empty()
            && app.controller.tracking.asked_planning.is_empty(),
        "{after}"
    );
}

/// `/clear` and `/resume` put a conversation in place the same way
/// (#136): nothing of the one before is left, of what the controller
/// holds of it or of what is on screen.
#[test]
fn clear_and_resume_leave_nothing_of_the_conversation_before() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.submit_prompt("one".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    let saved = app.controller.conversation.record.id.clone();

    leave_traces(&mut app);
    app.handle_slash_command("/clear");
    assert_ne!(app.controller.conversation.record.id, saved);
    assert_no_traces(&app, "/clear", Usage::default(), ContextUsage::default());

    leave_traces(&mut app);
    app.handle_slash_command(&format!("/resume {saved}"));
    assert_eq!(app.controller.conversation.record.id, saved);
    // The last turn's usage and the context the first left in it when it
    // was cleared are its own, and come back with it.
    assert_no_traces(
        &app,
        "/resume",
        Usage {
            input: 1,
            ..Default::default()
        },
        ContextUsage {
            used: Some(1),
            window: Some(10),
        },
    );
}
