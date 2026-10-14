use super::*;

#[test]
fn conversation_persists_across_harness_switch_and_resumes() {
    use crate::core::transcript::Block;
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
    app.submit_prompt("first question".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TextDelta("first answer".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.switch_harness(HarnessId::CODEX);
    app.take_actions();
    app.controller.session_alive = false;
    app.submit_prompt("second question".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "codex-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TextDelta("second answer".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    let conv_id = app.controller.conversation.record.id.clone();
    app.quit();

    let rows = app.controller.store.list();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, conv_id);
    assert_eq!(rows[0].harnesses, vec![HarnessId::CLAUDE, HarnessId::CODEX]);
    assert_eq!(rows[0].title, "first question");

    let mut again = test_app_in(cwd.clone(), HarnessId::CLAUDE, Some(String::new()), false);
    assert_eq!(again.controller.active, HarnessId::CODEX);
    assert_eq!(again.controller.conversation.record.id, conv_id);
    assert_eq!(
        again
            .controller
            .conversation
            .session_ids
            .get(&HarnessId::CLAUDE)
            .map(String::as_str),
        Some("claude-1")
    );
    assert_eq!(
        again
            .controller
            .conversation
            .session_ids
            .get(&HarnessId::CODEX)
            .map(String::as_str),
        Some("codex-1")
    );
    assert!(
        again
            .controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Assistant { text, .. } if text == "second answer"))
    );
    assert!(
        again
            .controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Notice(n) if n.contains("resumed conversation")))
    );

    again.submit_prompt("third".into());
    assert_eq!(
        again.take_actions(),
        vec![
            Action::StartSession {
                resume: Some("codex-1".into())
            },
            Action::turn("third")
        ]
    );
    again.controller.session_alive = true;
    again.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    again.switch_harness(HarnessId::CLAUDE);
    again.take_actions();
    again.controller.session_alive = false;
    again.submit_prompt("fourth".into());
    let actions = again.take_actions();
    assert_eq!(
        actions[0],
        Action::StartSession {
            resume: Some("claude-1".into())
        }
    );
    match &actions[1] {
        Action::SendTurn { text: t, .. } => {
            assert!(t.contains("second question") && t.contains("third"), "{t}");
            assert!(!t.contains("first question"), "{t}");
        }
        other => panic!("{other:?}"),
    }

    let forced = test_app_in(cwd, HarnessId::PI, Some(conv_id[..8].to_string()), true);
    assert_eq!(forced.controller.active, HarnessId::PI);
    assert_eq!(forced.controller.conversation.session_ids.len(), 2);
}

/// `/clear` starts a new conversation: the old one is saved as it was,
/// and no harness takes its session into the new one.
#[test]
fn clear_starts_a_new_conversation() {
    let done = || AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    };
    let mut app = test_app(HarnessId::CODEX);
    app.submit_prompt("one".into());
    app.on_event(AgentEvent::SessionStarted {
        session_id: "codex-1".into(),
        model: None,
    });
    app.on_event(done());
    app.switch_harness(HarnessId::CLAUDE);
    app.set_model("opus".into());
    app.submit_prompt("two".into());
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TextDelta("answer".into()));
    app.on_event(AgentEvent::Usage(Usage {
        input: 10,
        output: 5,
        ..Default::default()
    }));
    app.on_event(AgentEvent::Context(ContextUsage {
        used: Some(15),
        window: Some(1000),
    }));
    app.on_event(done());
    app.take_actions();
    app.controller.session_alive = true;
    assert!(!app.controller.conversation.last_active_index.is_empty());

    let old = app.controller.conversation.record.id.clone();
    app.new_conversation();
    assert_ne!(app.controller.conversation.record.id, old);
    assert!(app.controller.conversation.session_ids.is_empty());
    assert!(app.controller.conversation.last_active_index.is_empty());
    assert_eq!(app.controller.conversation.session_usage, Usage::default());
    assert_eq!(app.controller.conversation.context, ContextUsage::default());
    assert_eq!(app.controller.active, HarnessId::CLAUDE);
    assert_eq!(
        app.controller.model(HarnessId::CLAUDE).unwrap().model,
        "opus"
    );
    // No harness has run in it yet.
    assert!(app.controller.conversation.record.last_policies.is_empty());
    assert_eq!(
        app.controller.conversation.transcript.blocks.len(),
        1,
        "only the new conversation's notice"
    );
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(!app.controller.session_alive);

    // The next session knows nothing of the old one.
    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession { resume: None }]
    );

    // The old conversation is in the store as it was.
    let saved = app.controller.store.load(&old).unwrap();
    assert_eq!(saved.sessions[&HarnessId::CLAUDE], "claude-1");
    assert_eq!(saved.sessions[&HarnessId::CODEX], "codex-1");
    assert!(saved.last_policies.contains_key(&HarnessId::CLAUDE));
    assert!(saved.blocks.iter().any(
        |b| matches!(b, crate::core::conversations::BlockRecord::Assistant { text, .. } if text == "answer")
    ));

    // A harness used before the clear starts afresh too.
    app.controller.session_alive = false;
    app.switch_harness(HarnessId::CODEX);
    app.submit_prompt("three".into());
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession { resume: None }, Action::turn("three")]
    );
}

/// Nothing is saved for a conversation nothing was sent in, the old one
/// or the new one.
#[test]
fn clear_of_nothing_saves_nothing() {
    let mut app = test_app(HarnessId::CLAUDE);
    let old = app.controller.conversation.record.id.clone();
    app.new_conversation();
    assert_ne!(app.controller.conversation.record.id, old);
    assert!(app.controller.store.list().is_empty());
    assert!(matches!(
        app.controller.conversation.transcript.blocks.as_slice(),
        [crate::core::transcript::Block::System(s)] if s == "New conversation."
    ));
}

/// A session started with nothing to resume and sent nothing serves
/// the new conversation; one started on a session of the old ends.
#[test]
fn clear_keeps_a_session_that_knows_nothing() {
    let mut app = test_app(HarnessId::CLAUDE);
    blank_session(&mut app);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.new_conversation();
    assert!(app.take_actions().is_empty());
    assert!(app.controller.session_alive);
    // Its id is the new conversation's once something is sent.
    app.submit_prompt("hello".into());
    assert_eq!(
        app.controller.conversation.session_ids[&HarnessId::CLAUDE],
        "claude-1"
    );

    let mut app = test_app(HarnessId::CLAUDE);
    app.controller
        .conversation
        .session_ids
        .insert(HarnessId::CLAUDE, "claude-0".into());
    blank_session(&mut app);
    app.new_conversation();
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
}

/// What ending the session told about the harness's configuration is
/// not lost with the old transcript.
#[test]
fn clear_tells_a_configuration_change_in_the_new_conversation() {
    use crate::core::guard::{Guarded, Watch};
    let mut app = test_app(HarnessId::PI);
    app.submit_prompt("one".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.controller.session_alive = true;
    let config = tempfile::tempdir().unwrap();
    app.controller.guard = Some(Watch::begin(
        &[Guarded::Tree(config.path().to_path_buf())],
        None,
    ));
    std::fs::write(config.path().join("extension.ts"), "x").unwrap();
    app.new_conversation();
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("configuration changed while it ran"))
    );
}

/// A conversation that cannot be saved is not dropped.
#[test]
fn clear_keeps_a_conversation_it_cannot_save() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    let dir = app.controller.store.root().join("conversations");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::write(&dir, "in the way").unwrap();
    let id = app.controller.conversation.record.id.clone();
    app.new_conversation();
    assert_eq!(app.controller.conversation.record.id, id);
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .starts_with("could not save the conversation, so /clear keeps it")
    );
}

/// Prompts held after a turn that failed wait for the user in the new
/// conversation.
#[test]
fn clear_keeps_held_prompts() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    app.queue_prompt("two".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Error("boom".into()),
    });
    assert_eq!(app.controller.queued.len(), 1);
    app.new_conversation();
    assert_eq!(app.controller.queued.len(), 1);
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .starts_with("1 queued prompt(s) held")
    );
}

/// Not while something of the conversation is still at work: what it
/// does would land in the new one.
#[test]
fn clear_is_refused_while_busy() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    let id = app.controller.conversation.record.id.clone();
    app.new_conversation();
    assert_eq!(app.controller.conversation.record.id, id);
    assert!(notices(&app).last().unwrap().contains("current turn"));

    spawn(&mut app, "spawn", "look around", None);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.new_conversation();
    assert_eq!(app.controller.conversation.record.id, id);
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .contains("subagent(s) still at work")
    );

    app.controller.tracking.subagents.clear();
    app.run_shell("sleep 5");
    app.new_conversation();
    assert_eq!(app.controller.conversation.record.id, id);
    assert!(notices(&app).last().unwrap().contains("! command"));
}

#[test]
fn exit_summary_only_when_saved() {
    let mut app = test_app(HarnessId::CLAUDE);
    assert!(app.controller.exit_summary().is_none());
    app.submit_prompt("hello".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "s1".into(),
        model: None,
    });
    let line = app.controller.exit_summary().unwrap();
    assert!(line.contains("unharness --resume"));
    assert!(line.contains(&app.controller.conversation.record.id[..8]));
    assert!(line.contains("(claude)"));
}

#[test]
fn resume_errors_are_reported_not_fatal() {
    use crate::core::transcript::Block;
    let tmp = tempfile::tempdir().unwrap();
    let app = test_app_in(
        tmp.path().to_path_buf(),
        HarnessId::CLAUDE,
        Some("nope".into()),
        false,
    );
    assert!(
        app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Error(e) if e.contains("could not resume")))
    );
    assert_eq!(app.controller.active, HarnessId::CLAUDE);
}
