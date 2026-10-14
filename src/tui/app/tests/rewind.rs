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
    app.session_alive = false;
    app.take_actions();
    let two = app
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, super::super::transcript::Block::User { text } if text == "two"))
        .unwrap();
    app.rewind_to(two, false);
    let held = app.take_actions();
    assert!(matches!(held[0], Action::StartSession { .. }), "{held:?}");

    app.drop_held(held);
    assert!(!app.session_ids.contains_key(&HarnessId::CLAUDE));
    // Without a rewind among them, nothing is forgotten.
    app.session_ids.insert(HarnessId::CLAUDE, "s2".into());
    app.drop_held([Action::StartSession {
        resume: Some("s2".into()),
    }]);
    assert!(app.session_ids.contains_key(&HarnessId::CLAUDE));
}

#[test]
fn rewind_uses_the_harness_anchor_or_starts_a_fresh_session() {
    use super::super::transcript::Block;
    let done = || AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    };
    let mut app = test_app(HarnessId::CLAUDE);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "s1".into(),
        model: None,
    });
    for (prompt, anchor) in [("one", "a1"), ("two", "a2"), ("three", "a3")] {
        app.submit_prompt(prompt.into());
        app.session_alive = true;
        app.on_event(AgentEvent::TurnAnchor { id: anchor.into() });
        app.on_event(AgentEvent::TextDelta(format!("re {prompt}")));
        app.on_event(done());
    }
    app.take_actions();
    let block_of = |app: &App, want: &str| {
        app.transcript
            .blocks
            .iter()
            .position(|b| matches!(b, Block::User { text } if text == want))
            .unwrap()
    };

    // Live session with an anchor: the harness drops the turns itself.
    let three = block_of(&app, "three");
    app.rewind_to(three, false);
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Rewind {
            anchor: "a3".into()
        })]
    );
    assert_eq!(app.input, "three");
    assert!(app.session_ids.contains_key(&HarnessId::CLAUDE));
    assert!(
        !app.transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::User { text } if text == "three"))
    );
    assert_eq!(app.anchors.len(), 2);
    app.take_input();

    // The session is not running: it is reattached, then rewound.
    app.session_alive = false;
    let two = block_of(&app, "two");
    app.rewind_to(two, false);
    assert_eq!(
        app.take_actions(),
        vec![
            Action::StartSession {
                resume: Some("s1".into())
            },
            Action::Command(SessionCommand::Rewind {
                anchor: "a2".into()
            })
        ]
    );
    assert_eq!(app.input, "two");
    app.take_input();

    // The harness reports that it did not rewind: fall back to a fresh
    // session that gets the remaining conversation as context.
    app.session_alive = true;
    app.on_event(AgentEvent::RewindFailed {
        reason: "stale target".into(),
    });
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(!app.session_ids.contains_key(&HarnessId::CLAUDE));

    // Back to before a session's first turn nothing of it remains, so a
    // fresh session is used rather than a native rewind.
    let mut app = test_app(HarnessId::CLAUDE);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "s2".into(),
        model: None,
    });
    app.submit_prompt("one".into());
    app.session_alive = true;
    app.on_event(AgentEvent::TurnAnchor { id: "b1".into() });
    app.on_event(done());
    app.take_actions();
    let one = block_of(&app, "one");
    app.rewind_to(one, false);
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(!app.session_ids.contains_key(&HarnessId::CLAUDE));

    // A harness that cannot rewind at all goes straight to the fallback.
    let mut app = test_app(HarnessId::AGY);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "g1".into(),
        model: None,
    });
    app.submit_prompt("one".into());
    app.on_event(done());
    app.take_actions();
    let one = block_of(&app, "one");
    app.rewind_to(one, false);
    assert!(app.take_actions().is_empty());
    assert!(!app.session_ids.contains_key(&HarnessId::AGY));
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
    let after_one = app.transcript.blocks.len();
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
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, super::super::transcript::Block::User { text } if text == "two"))
        .unwrap();
    assert!(two >= after_one);
    app.rewind_to(two, false);
    assert!(app.session_ids.contains_key(&HarnessId::CLAUDE));
    assert!(!app.session_ids.contains_key(&HarnessId::CODEX));
}

#[test]
fn anchors_attach_in_order_even_when_they_arrive_late() {
    // pi reports a turn's anchor after the turn, when a queued prompt may
    // already have started the next one.
    let mut app = test_app(HarnessId::PI);
    app.submit_prompt("one".into());
    app.session_alive = true;
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
    app.session_alive = false;

    let original = app.conversation.id.clone();
    app.fork_conversation();
    assert_ne!(app.conversation.id, original);
    // Claude can branch a session: the id is kept, to be forked on next
    // use. Its rewind ids do not carry over into a branch. Antigravity
    // cannot, so the copy starts it fresh.
    assert_eq!(app.session_ids.get(&HarnessId::CLAUDE).unwrap(), "claude-1");
    assert!(app.fork_pending());
    assert!(app.anchors.is_empty());
    assert!(!app.session_ids.contains_key(&HarnessId::AGY));

    // The original is on disk untouched; the copy remembers what is pending.
    let saved = app.store.load(&original).unwrap();
    assert_eq!(saved.sessions[&HarnessId::CLAUDE], "claude-1");
    assert!(saved.fork_pending.is_empty());
    let copy = app.store.load(&app.conversation.id).unwrap();
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
    assert!(!app.fork_pending());
    assert_eq!(app.session_ids[&HarnessId::CLAUDE], "claude-2");
}

/// An ACP agent declares no fork until its session says it can
/// (`sessionCapabilities.fork` at `initialize`): then its session is
/// branched like Claude's; one that did not say so starts fresh with
/// the transcript.
#[test]
fn fork_branches_an_acp_session_once_the_agent_says_it_can() {
    let acp = HarnessId::intern("acp-fork-test");
    let forked = |reported: Option<bool>| {
        let registry = Arc::new(
            Registry::empty().with(Box::new(
                crate::harness::acp::AcpHarness::new(
                    acp.as_str(),
                    None,
                    &["acp-fork-test".to_string()],
                    crate::harness::acp::AcpProfile::default(),
                )
                .unwrap(),
            )),
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app_with(
            tmp.path().to_path_buf(),
            acp,
            None,
            false,
            Config::default(),
            registry,
        );
        if app.effective_policy().is_none() {
            app.controller
                .policy
                .choose_for_conversation(acp, PermissionPolicy::Bypass);
            app.modal = None;
        }
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            resume_by_id: Some(true),
            fork: reported,
            ..Default::default()
        }));
        app.submit_prompt("one".into());
        app.on_event(AgentEvent::SessionStarted {
            session_id: "acp-1".into(),
            model: None,
        });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
        app.take_actions();
        app.session_alive = false;
        app.fork_conversation();
        (app.session_ids.get(&acp).cloned(), app.fork_pending())
    };
    assert_eq!(forked(Some(true)), (Some("acp-1".to_string()), true));
    for reported in [None, Some(false)] {
        assert_eq!(forked(reported), (None, false), "{reported:?}");
    }
}

#[test]
fn rewind_can_restore_files_and_undo_it() {
    let git = |dir: &std::path::Path, args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("a.txt"), "before\n").unwrap();

    let mut app = test_app_in(root.to_path_buf(), HarnessId::AGY, None, false);
    app.submit_prompt("change it".into());
    // The agent edits a file and creates another during the turn.
    std::fs::write(root.join("a.txt"), "after\n").unwrap();
    std::fs::write(root.join("new.txt"), "made by the agent\n").unwrap();
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    app.open_rewind_picker();
    assert!(matches!(&app.modal, Some(Modal::Rewind(p)) if p.items[0].files));
    app.handle_modal_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
    assert!(app.modal.is_none());
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "before\n"
    );
    assert!(!root.join("new.txt").exists());
    assert_eq!(app.input, "change it");

    app.handle_slash_command("/undo-restore");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "after\n"
    );
    assert!(root.join("new.txt").exists());
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
    app.session_alive = false;
    app.fork_conversation();
    app.take_actions();
    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession {
            resume: Some("claude-1".into())
        }]
    );
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-2".into(),
        model: None,
    });
    // The branch is not taken until something is sent to it.
    assert!(app.fork_pending());
    assert_eq!(app.session_ids[&HarnessId::CLAUDE], "claude-1");
    let copy = app.store.load(&app.conversation.id).unwrap();
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
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-3".into(),
        model: None,
    });
    app.submit_prompt("two".into());
    assert!(!app.fork_pending());
    assert_eq!(app.session_ids[&HarnessId::CLAUDE], "claude-3");
}

#[test]
fn a_rewound_session_is_the_conversation_s() {
    use super::super::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    for (prompt, anchor) in [("one", "a1"), ("two", "a2")] {
        app.submit_prompt(prompt.into());
        app.session_alive = true;
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
    app.session_alive = false;
    app.start_when_idle();
    app.take_actions();
    app.session_alive = true;
    let two = app
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
