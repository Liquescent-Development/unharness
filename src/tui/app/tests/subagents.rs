use super::*;

/// What a transcript holds, one word per block.
fn kinds(blocks: &[super::super::transcript::Block]) -> Vec<String> {
    use super::super::transcript::Block;
    blocks
        .iter()
        .map(|b| match b {
            Block::User { .. } => "user".to_string(),
            Block::Assistant { text, .. } => format!("text:{text}"),
            Block::Thought { .. } => "thought".to_string(),
            Block::Handoff { to, .. } => format!("handoff:{to}"),
            Block::Tool { id, .. } => format!("tool:{id}"),
            Block::System(_) => "system".to_string(),
            Block::Notice(_) => "notice".to_string(),
            Block::Error(_) => "error".to_string(),
            Block::Shell { command, .. } => format!("shell:{command}"),
            Block::Hook { name, .. } => format!("hook:{name}"),
            Block::Compaction { .. } => "compaction".to_string(),
        })
        .collect()
}

fn read_call(id: &str, file: &str) -> AgentEvent {
    AgentEvent::ToolCallStarted {
        id: id.into(),
        name: "Read".into(),
        input: serde_json::json!({ "file_path": file }),
    }
}

#[test]
fn a_subagents_transcript_is_opened_read_and_left() {
    let mut app = test_app(HarnessId::CLAUDE);
    // Nothing to open.
    app.open_subagent_picker();
    assert!(app.modal.is_none());

    app.submit_prompt("delegate".into());
    app.controller.session_alive = true;
    spawn(&mut app, "first", "look around", None);
    spawn(&mut app, "second", "look elsewhere", Some("Explore"));
    app.on_event(sub("first", read_call("f1", "a.txt")));
    app.on_event(sub("second", read_call("s1", "b.txt")));
    // A subagent's own subagent lives in its transcript.
    app.on_event(sub(
        "second",
        AgentEvent::ToolCallStarted {
            id: "deep".into(),
            name: "Agent".into(),
            input: serde_json::json!({"description": "deeper"}),
        },
    ));
    app.on_event(sub(
        "second",
        AgentEvent::SubagentStarted {
            id: "deep".into(),
            description: "deeper".into(),
            kind: None,
            unreported_end: false,
        },
    ));
    app.on_event(sub("deep", read_call("d1", "c.txt")));
    app.on_event(AgentEvent::SubagentEnded {
        id: "first".into(),
        status: crate::core::SubagentStatus::Completed,
        result: None,
    });
    assert_eq!(
        kinds(&spawn_block(&app, "second").log.blocks),
        ["tool:s1", "tool:deep"]
    );
    assert_eq!(kinds(&spawn_block(&app, "deep").log.blocks), ["tool:d1"]);
    let main = kinds(&app.controller.conversation.transcript.blocks);
    assert_eq!(kinds(app.shown_blocks()), main);

    // The picker lists them all, finished or not, and starts on the
    // first still at work.
    app.open_subagent_picker();
    match &app.modal {
        Some(Modal::Subagents(p)) => {
            let rows: Vec<(&str, usize, bool)> = p
                .items
                .iter()
                .map(|a| (a.id.as_str(), a.depth, a.status.is_none()))
                .collect();
            assert_eq!(
                rows,
                [("first", 0, false), ("second", 0, true), ("deep", 1, true)]
            );
            assert_eq!(p.selected, 1);
        }
        _ => panic!("no picker"),
    }
    // Esc opens nothing; Enter opens the chosen one.
    app.handle_modal_key(key(KeyCode::Esc));
    assert!(app.modal.is_none() && app.viewing.is_none());
    app.scroll = 7;
    app.auto_scroll = false;
    app.open_subagent_picker();
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(app.viewing.as_deref(), Some("second"));
    assert_eq!(kinds(app.shown_blocks()), ["tool:s1", "tool:deep"]);
    assert!(app.auto_scroll);

    // Round the others and back.
    app.view_next_subagent(false);
    assert_eq!(kinds(app.shown_blocks()), ["tool:d1"]);
    app.view_next_subagent(false);
    assert_eq!(app.viewing.as_deref(), Some("first"));
    app.view_next_subagent(true);
    assert_eq!(app.viewing.as_deref(), Some("deep"));

    // Expanding is done to the transcript in view.
    app.shown_transcript_mut().toggle_last();
    assert!(matches!(
        spawn_block(&app, "deep").log.blocks[0],
        super::super::transcript::Block::Tool {
            collapsed: false,
            ..
        }
    ));

    // Stopping is done from its own view, and only to it.
    app.take_actions();
    app.stop_viewed_subagent();
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::StopSubagent {
            id: "deep".into()
        })]
    );
    // One that has ended is not asked to stop.
    app.open_subagent("first");
    app.stop_viewed_subagent();
    assert!(app.take_actions().is_empty());

    // Leaving puts the main transcript back where it was.
    app.close_subagent_view();
    assert!(app.viewing.is_none());
    assert_eq!((app.scroll, app.auto_scroll), (7, false));
    assert_eq!(kinds(app.shown_blocks()), main);

    // A prompt sent while they work goes out at once.
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.queue_prompt("meanwhile".into());
    assert!(app.controller.queued.is_empty());
    assert!(matches!(
        app.take_actions().as_slice(),
        [Action::SendTurn { text, .. }] if text == "meanwhile"
    ));

    // The session dies: nothing is left running, however deep.
    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    assert!(app.controller.tracking.subagents.is_empty());
    for id in ["second", "deep"] {
        assert_eq!(
            spawn_block(&app, id).status,
            Some(crate::core::SubagentStatus::Cancelled)
        );
    }

    // A harness that cannot stop one says so.
    let mut app = test_app(HarnessId::PI);
    app.controller.session_alive = true;
    spawn(&mut app, "spawn", "look around", None);
    app.open_subagent("spawn");
    app.stop_viewed_subagent();
    assert!(app.take_actions().is_empty());
    assert!(app.flash_text().is_some_and(|t| t.contains("cannot stop")));
}

/// A vibe session after a turn that spawned a subagent the agent did
/// not wait for: vibe never tells its end (#215).
fn vibe_with_unseen_child(app: &mut App) {
    use crate::harness::vibe::subagents::UNSEEN_END;
    prompted_vibe(app);
    app.on_event(AgentEvent::ToolCallStarted {
        id: "s1".into(),
        name: "Agent".into(),
        input: serde_json::json!({ "description": "scribe" }),
    });
    // As vibe's reader says it (`vibe::subagents`).
    app.on_event(AgentEvent::SubagentStarted {
        id: "s1".into(),
        description: "scribe".into(),
        kind: Some("scribe".into()),
        unreported_end: true,
    });
    app.on_event(AgentEvent::SubagentProgress {
        id: "s1".into(),
        activity: UNSEEN_END.into(),
    });
    app.on_event(AgentEvent::TextDelta("started scribe".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
}

/// A subagent whose end may never be told (its start says so,
/// `unreported_end`) stays listed at work with its note, but keeps nothing
/// busy and holds back nothing that would end the session; it goes with
/// the session.
#[test]
fn a_subagent_whose_end_is_never_told_holds_nothing_back() {
    use crate::core::SubagentStatus;
    use crate::harness::vibe::subagents::UNSEEN_END;
    let (mut app, _cwd) = vibe_app();
    assert!(!app.caps().subagents.stop);
    vibe_with_unseen_child(&mut app);
    let rows = app.subagent_rows();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].status.is_none());
    assert_eq!(rows[0].activity.as_deref(), Some(UNSEEN_END));
    // The status rule says ready, and herdr idle; the row's spinner
    // and time still run.
    assert!(!app.is_busy());
    assert_eq!(app.subagents_label(), None);
    assert!(app.has_running_subagents());
    // Delete on it names no key that would stop it.
    assert!(app.subagent_list_down());
    app.dismiss_focused_subagent();
    let flash = app.flash_text().unwrap_or_default().to_string();
    assert!(flash.contains("Mistral Vibe cannot stop"), "{flash}");
    assert!(!flash.contains("press s"), "{flash}");
    app.subagent_list_up();

    // An effort change starts a new session, and the subagent goes
    // with the old one.
    app.set_effort("off".into());
    app.take_actions();
    vibe_renews(&mut app, "effort off");
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert_eq!(app.current_effort(), Some("off"));
    assert!(app.controller.tracking.subagents.is_empty());
    assert_eq!(
        spawn_block(&app, "s1").status,
        Some(SubagentStatus::Cancelled)
    );
    assert!(!notices(&app).iter().any(|n| n.contains("subagent(s)")));

    // So does a model change.
    let (mut app, _cwd) = vibe_app();
    vibe_with_unseen_child(&mut app);
    app.set_model("glm-5-3".into());
    app.take_actions();
    vibe_renews(&mut app, "model glm-5-3");
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert_eq!(app.current_model().unwrap().model, "glm-5-3");

    // And /clear.
    let (mut app, _cwd) = vibe_app();
    vibe_with_unseen_child(&mut app);
    let id = app.controller.conversation.record.id.clone();
    app.handle_slash_command("/clear");
    assert_ne!(app.controller.conversation.record.id, id);
    assert!(app.controller.tracking.subagents.is_empty() && app.subagent_rows().is_empty());

    // And a provider change.
    let (mut app, _cwd) = vibe_app_with_provider();
    vibe_with_unseen_child(&mut app);
    app.set_provider("lan".into());
    app.take_actions();
    vibe_renews(&mut app, "provider lan");
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert_eq!(app.chosen_provider(), Some(&"lan".into()));

    // A conversation over the new session's budget is summarized
    // first.
    let (mut app, _cwd) = vibe_app();
    app.controller.bridge_max_chars = Some(300);
    vibe_with_unseen_child(&mut app);
    app.on_event(AgentEvent::TextDelta("x".repeat(600)));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.set_effort("off".into());
    app.take_actions();
    vibe_renews(&mut app, "effort off");
    let ask = sent_turn(&mut app);
    assert!(ask.contains("new session of yours"), "{ask}");
}

/// Claude's subagents tell their end and can be stopped: one at work
/// keeps the session busy and holds back what would end it, pointing
/// at stopping it; a harness that cannot stop one says only to wait.
#[test]
fn a_subagent_whose_end_is_told_holds_back_what_would_end_the_session() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller.sandbox = SandboxSetup::null(None);
    assert!(app.caps().subagents.stop);
    app.controller.bridge_max_chars = Some(300);
    app.submit_prompt("delegate".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    spawn(&mut app, "s1", "look around", None);
    app.on_event(AgentEvent::TextDelta("x".repeat(600)));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    assert!(app.is_busy());
    assert_eq!(app.subagents_label().as_deref(), Some("1 subagent running"));

    let id = app.controller.conversation.record.id.clone();
    app.handle_slash_command("/clear");
    assert_eq!(app.controller.conversation.record.id, id);
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .ends_with("wait for them or stop them (Ctrl+S) before clearing")
    );
    app.set_provider("bedrock".into());
    assert_eq!(app.chosen_provider(), None);
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .ends_with("wait for them or stop them (Ctrl+S) before changing the provider")
    );
    assert!(app.take_actions().is_empty());
    assert!(app.subagent_list_down());
    app.dismiss_focused_subagent();
    assert_eq!(
        app.flash_text(),
        Some("Still running: open it and press s to stop it")
    );
    app.subagent_list_up();
    // A switch asks no summary while it works.
    app.switch_harness(HarnessId::CODEX);
    assert!(!app.controller.is_generating);
    assert_eq!(app.controller.active, HarnessId::CODEX);

    // Antigravity tells the end but cannot stop one: the user is only
    // told to wait.
    let mut app = test_app(HarnessId::AGY);
    assert!(app.caps().subagents.reported);
    assert!(!app.caps().subagents.stop);
    app.submit_prompt("delegate".into());
    app.take_actions();
    app.controller.session_alive = true;
    spawn(&mut app, "s1", "look around", None);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    let id = app.controller.conversation.record.id.clone();
    app.handle_slash_command("/clear");
    assert_eq!(app.controller.conversation.record.id, id);
    let refused = notices(&app).last().unwrap().clone();
    assert!(
        refused.ends_with("wait for them before clearing") && !refused.contains("Ctrl+S"),
        "{refused}"
    );
    assert!(app.subagent_list_down());
    app.dismiss_focused_subagent();
    let flash = app.flash_text().unwrap_or_default();
    assert!(
        flash.contains("cannot stop") && !flash.contains("press s"),
        "{flash}"
    );
}
