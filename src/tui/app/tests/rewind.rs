use super::*;

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
        app.controller.session_alive = true;
        app.on_event(AgentEvent::TurnAnchor { id: anchor.into() });
        app.on_event(AgentEvent::TextDelta(format!("re {prompt}")));
        app.on_event(done());
    }
    app.take_actions();
    let block_of = |app: &App, want: &str| {
        app.controller
            .conversation
            .transcript
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
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::CLAUDE)
    );
    assert!(
        !app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::User { text } if text == "three"))
    );
    assert_eq!(app.controller.conversation.anchors.len(), 2);
    app.take_input();

    // The session is not running: it is reattached, then rewound.
    app.controller.session_alive = false;
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
    app.controller.session_alive = true;
    app.on_event(AgentEvent::RewindFailed {
        reason: "stale target".into(),
    });
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::CLAUDE)
    );

    // Back to before a session's first turn nothing of it remains, so a
    // fresh session is used rather than a native rewind.
    let mut app = test_app(HarnessId::CLAUDE);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "s2".into(),
        model: None,
    });
    app.submit_prompt("one".into());
    app.controller.session_alive = true;
    app.on_event(AgentEvent::TurnAnchor { id: "b1".into() });
    app.on_event(done());
    app.take_actions();
    let one = block_of(&app, "one");
    app.rewind_to(one, false);
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::CLAUDE)
    );

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
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::AGY)
    );
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
        app.controller.session_alive = false;
        app.fork_conversation();
        (
            app.controller.conversation.session_ids.get(&acp).cloned(),
            app.controller.fork_pending(app.controller.active),
        )
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
