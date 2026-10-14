use super::*;
use crate::core::controller::STOP_GRACE;

#[test]
fn steer_goes_into_the_turn_or_falls_back_to_the_queue() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("go".into());
    app.controller.session_alive = true;
    app.take_actions();
    app.steer("left a bit".into());
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Steer {
            text: "left a bit".into(),
            attachments: Vec::new()
        })]
    );
    assert!(app.controller.queued.is_empty());

    // Antigravity cannot be steered: the message waits for the turn to end.
    let mut app = test_app(HarnessId::AGY);
    app.submit_prompt("go".into());
    app.controller.session_alive = true;
    app.take_actions();
    app.steer("left a bit".into());
    assert!(app.take_actions().is_empty());
    assert_eq!(app.controller.queued.len(), 1);
}

#[test]
fn submit_starts_session_then_sends_turn() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("hello".into());
    assert!(app.controller.is_generating);
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession { resume: None }, Action::turn("hello")]
    );
    app.controller.session_alive = true;
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert!(!app.controller.is_generating);
    app.submit_prompt("again".into());
    assert_eq!(app.take_actions(), vec![Action::turn("again")]);
}

#[test]
fn an_interrupt_long_after_the_first_warns_before_ending_the_process() {
    use crate::core::transcript::Block;
    let mut app = test_app(HarnessId::PI);
    app.controller.session_alive = true;
    app.submit_prompt("look".into());
    app.take_actions();
    app.interrupt();
    app.take_actions();
    app.controller.interrupted = Some((Instant::now() - STOP_GRACE, false));
    app.interrupt();
    assert_eq!(app.take_actions(), vec![]);
    assert!(app.controller.is_generating);
    assert!(app.controller.conversation.transcript.blocks.iter().any(
        |b| matches!(b, Block::Notice(n) if n.ends_with("asked again, its process is ended"))
    ));
    app.interrupt();
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
}

/// A CLI that did not stop while it wrote a handoff summary for a new
/// session (vibe takes a new effort only in one) has its process ended,
/// and the notice says what comes next: a new session with the change,
/// not the one ended resumed (#226). A turn of the conversation still
/// says the session is resumed.
#[test]
fn a_stuck_handoff_for_a_new_session_says_a_new_one_starts() {
    let stuck = |app: &mut TestApp| {
        app.interrupt();
        app.take_actions();
        app.controller.interrupted = Some((Instant::now() - STOP_GRACE, true));
        app.interrupt();
        notices(app)
            .into_iter()
            .filter(|n| n.contains("did not stop"))
            .collect::<Vec<_>>()
    };

    let (mut app, _cwd) = vibe_app();
    prompted_vibe(&mut app);
    assert_eq!(
        stuck(&mut app),
        [
            "Mistral Vibe did not stop, so its process was ended; the next prompt resumes the session"
        ]
    );
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );

    let (mut app, _cwd) = vibe_app();
    app.controller.bridge_max_chars = Some(300);
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::TextDelta("x".repeat(600)));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.set_effort("off".into());
    app.take_actions();
    vibe_renews(&mut app, "effort off");
    assert!(sent_turn(&mut app).contains("new session of yours"));
    assert_eq!(
        stuck(&mut app),
        [
            "Mistral Vibe did not stop, so its process was ended; a new session starts with the change, and your next prompt tells it the conversation so far"
        ]
    );
    // The session it wrote in is ended and forgotten: the next is new.
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::Shutdown, Action::StartSession { resume: None }]
    );

    // One for a switch: the switch goes ahead, which says so itself.
    let mut app = over_budget_app();
    app.switch_harness(HarnessId::CODEX);
    assert!(app.controller.handoff.is_some());
    assert_eq!(
        stuck(&mut app),
        ["Claude did not stop, so its process was ended"]
    );
    assert_eq!(app.controller.active, HarnessId::CODEX);
}
