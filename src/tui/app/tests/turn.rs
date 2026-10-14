use super::*;
use crate::core::controller::STOP_GRACE;

#[test]
fn prompts_queue_during_a_turn_and_drain_on_completion() {
    let done = || AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    };
    let mut app = test_app(HarnessId::CLAUDE);
    app.queue_prompt("first".into());
    assert!(app.controller.is_generating && app.controller.queued.is_empty());
    app.take_actions();

    app.queue_prompt("second".into());
    app.queue_prompt("third".into());
    assert_eq!(app.controller.queued.len(), 2);
    assert!(app.take_actions().is_empty());

    // The newest one can be pulled back for editing.
    app.unqueue_last();
    assert_eq!(app.input, "third");
    app.take_input();

    app.controller.session_alive = true;
    app.on_event(done());
    assert!(app.controller.is_generating && app.controller.queued.is_empty());
    assert_eq!(app.take_actions(), vec![Action::turn("second")]);

    // After an interrupt the queue is held until the user presses Enter.
    app.queue_prompt("fourth".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    assert!(!app.controller.is_generating && app.controller.queued.len() == 1);
    assert!(app.send_next_queued());
    assert_eq!(app.take_actions(), vec![Action::turn("fourth")]);
}

#[test]
fn compact_needs_an_idle_live_session_that_supports_it() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.compact(None);
    assert!(app.take_actions().is_empty());
    app.controller.session_alive = true;
    app.handle_slash_command("/compact keep the API notes");
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Compact {
            instructions: Some("keep the API notes".into())
        })]
    );
    assert_eq!(app.status_label(), "Compacting");
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert!(!app.controller.is_generating);

    let mut app = test_app(HarnessId::AGY);
    app.controller.session_alive = true;
    app.compact(None);
    assert!(app.take_actions().is_empty());
}

#[test]
fn an_interrupt_the_turn_ignores_ends_the_process() {
    use crate::core::guard::{Guarded, Watch};
    use crate::core::transcript::Block;
    let mut app = test_app(HarnessId::PI);
    app.controller.session_alive = true;
    app.controller
        .conversation
        .session_ids
        .insert(HarnessId::PI, "pi-1".into());
    let config = tempfile::tempdir().unwrap();
    app.controller.guard = Some(Watch::begin(
        &[Guarded::Tree(config.path().to_path_buf())],
        None,
    ));
    app.submit_prompt("look".into());
    app.take_actions();
    app.on_event(AgentEvent::ToolCallStarted {
        id: "t1".into(),
        name: "bash".into(),
        input: serde_json::json!({"command": "cq query"}),
    });
    app.interrupt();
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    // Asked again within the grace: said once, nothing sent.
    app.interrupt();
    app.interrupt();
    assert_eq!(app.take_actions(), vec![]);
    let notices = |app: &App, start: &str| {
        app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Notice(n) if n.starts_with(start)))
            .count()
    };
    assert_eq!(notices(&app, "waiting for"), 1);
    assert!(app.controller.is_generating);

    // After it, the process is ended and the turn with it.
    std::fs::write(config.path().join("extension.ts"), "x").unwrap();
    app.on_event(pi_gate_request("g1", "ls"));
    app.controller.interrupted = Some((Instant::now() - STOP_GRACE, true));
    app.interrupt();
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(
        !app.controller.is_generating && !app.controller.session_alive && !app.awaiting_answer()
    );
    assert!(matches!(
        app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .find(|b| matches!(b, Block::Tool { .. })),
        Some(Block::Tool {
            done: true,
            is_error: true,
            ..
        })
    ));
    // What the turn changed in the harness's configuration is told.
    assert!(
        app.controller.conversation.transcript.blocks.iter().any(
            |b| matches!(b, Block::Error(e) if e.contains("configuration changed while it ran"))
        )
    );
    // The next prompt resumes the session in a new process.
    app.submit_prompt("go on".into());
    assert_eq!(
        app.take_actions(),
        vec![
            Action::StartSession {
                resume: Some("pi-1".into())
            },
            Action::turn("go on")
        ]
    );
    // Its anchor is its own: the ended turn's will never come.
    app.on_event(AgentEvent::TurnAnchor { id: "e2".into() });
    let go_on = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .rposition(|b| matches!(b, Block::User { text } if text == "go on"));
    assert_eq!(
        app.controller.conversation.anchors.last().map(|a| a.block),
        go_on
    );
    // A turn that ends when asked leaves nothing behind.
    app.interrupt();
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    assert!(app.controller.interrupted.is_none());
}
