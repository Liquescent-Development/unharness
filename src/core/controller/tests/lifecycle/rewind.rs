use super::*;

// Held behind the start of the session it was for, the rewind is
// dropped when unharness quits first: that session is not resumed.
#[test]
fn a_rewind_dropped_at_quit_forgets_the_session_it_was_for() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "s1".into(),
        model: None,
    });
    for (prompt, anchor) in [("one", "a1"), ("two", "a2")] {
        app.submit_prompt(prompt.into());
        app.on_event(AgentEvent::TurnAnchor { id: anchor.into() });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
    }
    app.controller.session_alive = false;
    app.take_actions();
    let two = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, crate::core::transcript::Block::User { text } if text == "two"))
        .unwrap();
    app.rewind_to(two, false);
    let held = app.take_actions();
    assert!(matches!(held[0], Action::StartSession { .. }), "{held:?}");

    app.drop_held(held);
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::CLAUDE)
    );
    // Without a rewind among them, nothing is forgotten.
    app.controller
        .conversation
        .session_ids
        .insert(HarnessId::CLAUDE, "s2".into());
    app.drop_held([Action::StartSession {
        resume: Some("s2".into()),
    }]);
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::CLAUDE)
    );
}

#[test]
fn rewind_drops_sessions_that_saw_the_removed_turns() {
    let done = || AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    };
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(done());
    let after_one = app.controller.conversation.transcript.blocks.len();
    app.switch_harness(HarnessId::CODEX);
    app.submit_prompt("two".into());
    app.on_event(AgentEvent::SessionStarted {
        session_id: "codex-1".into(),
        model: None,
    });
    app.on_event(done());
    app.switch_harness(HarnessId::PI);
    app.submit_prompt("three".into());
    app.on_event(done());
    app.take_actions();

    // Rewinding to before "two": Codex saw it and is reset; Claude's
    // session ended before it and stays.
    let two = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, crate::core::transcript::Block::User { text } if text == "two"))
        .unwrap();
    assert!(two >= after_one);
    app.rewind_to(two, false);
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::CLAUDE)
    );
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::CODEX)
    );
}

#[test]
fn anchors_attach_in_order_even_when_they_arrive_late() {
    // pi reports a turn's anchor after the turn, when a queued prompt may
    // already have started the next one.
    let mut app = test_app(HarnessId::PI);
    app.submit_prompt("one".into());
    app.controller.session_alive = true;
    app.queue_prompt("two".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.on_event(AgentEvent::TurnAnchor { id: "e1".into() });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    // A turn that never reached the harness reports an empty anchor.
    app.submit_prompt("three".into());
    app.on_event(AgentEvent::TurnAnchor { id: "e2".into() });
    app.on_event(AgentEvent::TurnAnchor { id: String::new() });
    let ids: Vec<(&str, usize)> = app
        .controller
        .conversation
        .anchors
        .iter()
        .map(|a| (a.id.as_str(), a.block))
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids[0].0 == "e1" && ids[1].0 == "e2" && ids[0].1 < ids[1].1);
}

#[test]
fn fork_branches_sessions_where_the_harness_can() {
    let done = || AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    };
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::AGY, None, false);
    app.submit_prompt("one".into());
    app.on_event(AgentEvent::SessionStarted {
        session_id: "agy-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TextDelta("answer".into()));
    app.on_event(done());
    app.switch_harness(HarnessId::CLAUDE);
    app.submit_prompt("two".into());
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnAnchor { id: "u2".into() });
    app.on_event(done());
    app.take_actions();
    app.controller.session_alive = false;

    let original = app.controller.conversation.record.id.clone();
    app.fork_conversation();
    assert_ne!(app.controller.conversation.record.id, original);
    // Claude can branch a session: the id is kept, to be forked on next
    // use. Its rewind ids do not carry over into a branch. Antigravity
    // cannot, so the copy starts it fresh.
    assert_eq!(
        app.controller
            .conversation
            .session_ids
            .get(&HarnessId::CLAUDE)
            .unwrap(),
        "claude-1"
    );
    assert!(app.controller.fork_pending(app.controller.active));
    assert!(app.controller.conversation.anchors.is_empty());
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::AGY)
    );

    // The original is on disk untouched; the copy remembers what is pending.
    let saved = app.controller.store.load(&original).unwrap();
    assert_eq!(saved.sessions[&HarnessId::CLAUDE], "claude-1");
    assert!(saved.fork_pending.is_empty());
    let copy = app
        .controller
        .store
        .load(&app.controller.conversation.record.id)
        .unwrap();
    assert_eq!(copy.fork_pending, vec![HarnessId::CLAUDE]);

    // The branch knows the whole conversation: nothing is re-sent as text.
    app.submit_prompt("three".into());
    assert_eq!(
        app.take_actions(),
        vec![
            Action::StartSession {
                resume: Some("claude-1".into())
            },
            Action::turn("three")
        ]
    );
    // Once the branch reports its own id, it is an ordinary session.
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-2".into(),
        model: None,
    });
    assert!(!app.controller.fork_pending(app.controller.active));
    assert_eq!(
        app.controller.conversation.session_ids[&HarnessId::CLAUDE],
        "claude-2"
    );
}

#[test]
fn a_fork_started_before_the_first_prompt_is_not_the_conversation_s() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.controller.session_alive = false;
    app.fork_conversation();
    app.take_actions();
    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession {
            resume: Some("claude-1".into())
        }]
    );
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-2".into(),
        model: None,
    });
    // The branch is not taken until something is sent to it.
    assert!(app.controller.fork_pending(app.controller.active));
    assert_eq!(
        app.controller.conversation.session_ids[&HarnessId::CLAUDE],
        "claude-1"
    );
    let copy = app
        .controller
        .store
        .load(&app.controller.conversation.record.id)
        .unwrap();
    assert_eq!(copy.sessions[&HarnessId::CLAUDE], "claude-1");
    assert_eq!(copy.fork_pending, vec![HarnessId::CLAUDE]);
    // Started again, it branches again from the original.
    app.set_model("opus".into());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession {
            resume: Some("claude-1".into())
        }]
    );
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-3".into(),
        model: None,
    });
    app.submit_prompt("two".into());
    assert!(!app.controller.fork_pending(app.controller.active));
    assert_eq!(
        app.controller.conversation.session_ids[&HarnessId::CLAUDE],
        "claude-3"
    );
}

#[test]
fn a_rewound_session_is_the_conversation_s() {
    use crate::core::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    for (prompt, anchor) in [("one", "a1"), ("two", "a2")] {
        app.submit_prompt(prompt.into());
        app.controller.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: "s1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnAnchor { id: anchor.into() });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
    }
    app.take_actions();
    // As after a resume: started early on its session.
    app.controller.session_alive = false;
    app.start_when_idle();
    app.take_actions();
    app.controller.session_alive = true;
    let two = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, Block::User { text } if text == "two"))
        .unwrap();
    app.rewind_to(two, false);
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Rewind {
            anchor: "a2".into()
        })]
    );
    // Restarting it would resume it from before the rewind.
    app.set_model("opus".into());
    assert!(matches!(
        &app.take_actions()[..],
        [Action::Command(SessionCommand::SetModel(_))]
    ));
}
