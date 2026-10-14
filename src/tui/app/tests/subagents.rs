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
        })
        .collect()
}

/// A sub-agent's hooks (a Codex child thread's `hook/*`) belong to its
/// own transcript.
#[test]
fn a_subagents_hook_goes_to_its_transcript() {
    let mut app = test_app(HarnessId::CODEX);
    app.submit_prompt("delegate".into());
    spawn(&mut app, "spawn", "look around", None);
    let main = kinds(&app.controller.conversation.transcript.blocks);
    app.on_event(sub(
        "spawn",
        AgentEvent::HookStarted {
            id: "h".into(),
            name: "preToolUse".into(),
        },
    ));
    app.on_event(sub(
        "spawn",
        AgentEvent::HookEnded {
            id: "h".into(),
            name: "preToolUse".into(),
            outcome: crate::core::HookOutcome::Blocked,
            output: "no rm".into(),
        },
    ));
    assert_eq!(kinds(&app.controller.conversation.transcript.blocks), main);
    let run = spawn_block(&app, "spawn");
    assert_eq!(kinds(&run.log.blocks), ["hook:preToolUse"]);
    assert!(matches!(
        &run.log.blocks[0],
        super::super::transcript::Block::Hook {
            state: super::super::transcript::HookState::Ended(crate::core::HookOutcome::Blocked),
            ..
        }
    ));
}

fn read_call(id: &str, file: &str) -> AgentEvent {
    AgentEvent::ToolCallStarted {
        id: id.into(),
        name: "Read".into(),
        input: serde_json::json!({ "file_path": file }),
    }
}

#[test]
fn an_unannounced_subagent_still_gets_a_transcript_of_its_own() {
    use crate::core::SubagentStatus;
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("delegate".into());
    // A harness that attributes a subagent's work without announcing it.
    app.on_event(AgentEvent::ToolCallStarted {
        id: "spawn".into(),
        name: "Agent".into(),
        input: serde_json::json!({"description": "look around"}),
    });
    let main_len = app.controller.conversation.transcript.blocks.len();
    app.on_event(sub("spawn", read_call("inner", "a.txt")));
    app.on_event(sub(
        "spawn",
        AgentEvent::ToolCallResult {
            id: "inner".into(),
            output: "contents".into(),
            is_error: false,
        },
    ));
    app.on_event(sub("spawn", AgentEvent::TextDelta("found it".into())));
    assert_eq!(
        app.controller.conversation.transcript.blocks.len(),
        main_len
    );
    let run = spawn_block(&app, "spawn");
    assert_eq!(run.description, "look around");
    assert_eq!(run.status, None);
    assert_eq!(kinds(&run.log.blocks), ["tool:inner", "text:found it"]);
    // It ends when its call does.
    app.on_event(AgentEvent::ToolCallResult {
        id: "spawn".into(),
        output: "found it".into(),
        is_error: false,
    });
    assert_eq!(
        spawn_block(&app, "spawn").status,
        Some(SubagentStatus::Completed)
    );

    // Another harness is told what it reported, not what it called.
    let bridge = app
        .controller
        .conversation
        .transcript
        .bridge_text(0, 10_000)
        .unwrap();
    assert!(bridge.contains("[tool Agent look around → completed] found it"));
    assert!(!bridge.contains("[tool Read"));

    // A conversation saved when subagent calls sat in the main list,
    // pointing at their spawner, comes back with them under it.
    let old: Vec<crate::core::conversations::BlockRecord> =
        serde_json::from_value(serde_json::json!([
            {"kind": "user", "text": "delegate"},
            {"kind": "tool", "id": "spawn", "name": "Agent",
             "input": {"description": "look around"}, "output": "found it", "is_error": false},
            {"kind": "tool", "id": "inner", "name": "Read", "input": {"file_path": "a.txt"},
             "output": "contents", "is_error": false, "parent": "spawn"},
            {"kind": "assistant", "text": "done", "sender": "Claude", "secs": 1.0},
        ]))
        .unwrap();
    let back = Transcript::from_records(&old);
    assert_eq!(kinds(&back.blocks), ["user", "tool:spawn", "text:done"]);
    let run = back.agent("spawn").unwrap();
    assert_eq!(kinds(&run.log.blocks), ["tool:inner"]);
    assert_eq!(run.status, Some(SubagentStatus::Completed));
}

#[test]
fn a_background_subagent_outlives_its_call_and_its_turn() {
    use crate::core::SubagentStatus;
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("delegate".into());
    spawn(&mut app, "spawn", "look around", Some("Explore"));
    // The call returns at once and the turn ends; the subagent works on.
    app.on_event(AgentEvent::ToolCallResult {
        id: "spawn".into(),
        output: "Async agent launched successfully.".into(),
        is_error: false,
    });
    app.on_event(AgentEvent::TextDelta("It is on its way.".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert!(!app.is_generating);
    assert_eq!(app.subagents.len(), 1);
    assert_eq!(spawn_block(&app, "spawn").status, None);
    let main = kinds(&app.controller.conversation.transcript.blocks);

    app.on_event(sub("spawn", AgentEvent::ThinkingDelta("hm".into())));
    app.on_event(sub("spawn", AgentEvent::TextDelta("Looking.".into())));
    app.on_event(sub("spawn", read_call("inner", "a.txt")));
    // Until the harness words it, the call is the activity.
    assert_eq!(app.subagents[0].activity.as_deref(), Some("Read a.txt"));
    app.on_event(AgentEvent::SubagentProgress {
        id: "spawn".into(),
        activity: "Reading a.txt".into(),
    });
    app.on_event(sub("spawn", read_call("inner2", "b.txt")));
    assert_eq!(app.subagents[0].activity.as_deref(), Some("Reading a.txt"));
    assert_eq!(app.subagents[0].tools, 2);
    app.on_event(sub("spawn", AgentEvent::Notice("retrying".into())));
    app.on_event(sub(
        "spawn",
        AgentEvent::TextDelta("The word is alpha.".into()),
    ));
    app.on_event(AgentEvent::SubagentEnded {
        id: "spawn".into(),
        status: SubagentStatus::Completed,
        result: Some("The word is alpha.".into()),
    });
    assert!(app.subagents.is_empty());

    // All of it is in the subagent's transcript, the report (once) last,
    // and none of it in the main one.
    assert_eq!(kinds(&app.controller.conversation.transcript.blocks), main);
    let run = spawn_block(&app, "spawn");
    assert_eq!(run.status, Some(SubagentStatus::Completed));
    let log = kinds(&run.log.blocks);
    assert_eq!(
        log,
        [
            "thought",
            "text:Looking.",
            "tool:inner",
            "tool:inner2",
            "notice",
            "text:The word is alpha."
        ]
    );
    assert_eq!(run.report(), "The word is alpha.");

    // Its end was saved though no turn was running, and comes back.
    let saved = app
        .controller
        .store
        .load(&app.controller.conversation.record.id)
        .unwrap();
    let back = Transcript::from_records(&saved.blocks);
    assert_eq!(kinds(&back.blocks), main);
    let restored = back.agent("spawn").unwrap();
    assert_eq!(restored.status, Some(SubagentStatus::Completed));
    assert_eq!(kinds(&restored.log.blocks), log);
    assert_eq!(restored.kind.as_deref(), Some("Explore"));

    // Another harness is told what the subagent reported, not the receipt.
    let bridge = app
        .controller
        .conversation
        .transcript
        .bridge_text(0, 10_000)
        .unwrap();
    assert!(bridge.contains("completed] The word is alpha."));
    assert!(!bridge.contains("launched"));

    // A report the subagent never wrote out itself is added to its transcript.
    spawn(&mut app, "quiet", "say nothing", None);
    app.on_event(AgentEvent::SubagentEnded {
        id: "quiet".into(),
        status: SubagentStatus::Completed,
        result: Some("done quietly".into()),
    });
    assert_eq!(spawn_block(&app, "quiet").report(), "done quietly");
}

#[test]
fn a_report_after_the_end_is_added_once() {
    use crate::core::SubagentStatus;
    let mut app = test_app(HarnessId::CLAUDE);
    spawn(&mut app, "spawn", "review", Some("reviewer"));
    app.on_event(sub("spawn", AgentEvent::TextDelta("Reviewing.".into())));
    let ended = |app: &mut App, result: Option<&str>| {
        app.on_event(AgentEvent::SubagentEnded {
            id: "spawn".into(),
            status: SubagentStatus::Completed,
            result: result.map(str::to_string),
        })
    };
    ended(&mut app, None);
    assert!(app.subagents.is_empty());
    let took = spawn_block(&app, "spawn").duration;
    assert!(took.is_some());
    ended(&mut app, Some("All good."));
    ended(&mut app, Some("All good."));
    let run = spawn_block(&app, "spawn");
    assert_eq!(run.duration, took);
    assert_eq!(
        kinds(&run.log.blocks),
        ["text:Reviewing.", "text:All good."]
    );
}

#[test]
fn a_subagents_own_subagent_outlives_it() {
    use crate::core::SubagentStatus;
    let mut app = test_app(HarnessId::CLAUDE);
    spawn(&mut app, "outer", "delegate", None);
    app.on_event(sub(
        "outer",
        AgentEvent::ToolCallStarted {
            id: "inner".into(),
            name: "Agent".into(),
            input: serde_json::json!({ "description": "look" }),
        },
    ));
    app.on_event(AgentEvent::SubagentStarted {
        id: "inner".into(),
        description: "look".into(),
        kind: None,
        unreported_end: false,
    });
    let ended = |app: &mut App, id: &str| {
        app.on_event(AgentEvent::SubagentEnded {
            id: id.into(),
            status: SubagentStatus::Completed,
            result: None,
        })
    };
    ended(&mut app, "outer");
    let inner = spawn_block(&app, "inner");
    assert_eq!((inner.status, inner.duration), (None, None));
    assert_eq!(app.subagents.len(), 1);
    std::thread::sleep(Duration::from_millis(20));
    ended(&mut app, "inner");
    let inner = spawn_block(&app, "inner");
    assert_eq!(inner.status, Some(SubagentStatus::Completed));
    assert!(inner.duration.unwrap() >= Duration::from_millis(20));
    assert!(app.subagents.is_empty());
}

#[test]
fn a_subagents_transcript_is_opened_read_and_left() {
    let mut app = test_app(HarnessId::CLAUDE);
    // Nothing to open.
    app.open_subagent_picker();
    assert!(app.modal.is_none());

    app.submit_prompt("delegate".into());
    app.session_alive = true;
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
    app.shown_transcript_mut().toggle_last_tool();
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
    assert!(app.queued.is_empty());
    assert!(matches!(
        app.take_actions().as_slice(),
        [Action::SendTurn { text, .. }] if text == "meanwhile"
    ));

    // The session dies: nothing is left running, however deep.
    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    assert!(app.subagents.is_empty());
    for id in ["second", "deep"] {
        assert_eq!(
            spawn_block(&app, id).status,
            Some(crate::core::SubagentStatus::Cancelled)
        );
    }

    // A harness that cannot stop one says so.
    let mut app = test_app(HarnessId::PI);
    app.session_alive = true;
    spawn(&mut app, "spawn", "look around", None);
    app.open_subagent("spawn");
    app.stop_viewed_subagent();
    assert!(app.take_actions().is_empty());
    assert!(app.flash_text().is_some_and(|t| t.contains("cannot stop")));
}

/// Feed a recorded session through its parser into the app.
fn replay_fixture(app: &mut App, path: &str, mut feed: impl FnMut(&str) -> Vec<AgentEvent>) {
    let text = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap();
    for line in text.lines() {
        if line.is_empty() || line.starts_with(['#', '>', '!']) {
            continue;
        }
        for ev in feed(line) {
            app.on_event(ev);
        }
    }
}

#[test]
fn recorded_subagent_sessions_end_with_every_report_in_place() {
    use crate::core::SubagentStatus;
    use crate::harness::claude::parse::ClaudeParser;
    use crate::harness::codex::app_server_parse::CodexAppServerParser;
    let reports = |app: &App| -> Vec<(Option<SubagentStatus>, String)> {
        app.controller
            .conversation
            .transcript
            .agents()
            .into_iter()
            .map(|(_, a, _)| (a.status, a.report().to_string()))
            .collect()
    };
    let done = Some(SubagentStatus::Completed);

    // Claude: three launched in the background, reports interleaved.
    let mut app = test_app(HarnessId::CLAUDE);
    let mut p = ClaudeParser::new();
    replay_fixture(
        &mut app,
        "src/harness/claude/fixtures/subagent_parallel.jsonl",
        |l| p.feed(l),
    );
    assert!(app.subagents.is_empty() && !app.is_generating);
    let got = reports(&app);
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|(s, _)| *s == done));
    assert!(got[0].1.contains("alpha") && got[1].1.contains("bravo"));
    assert!(got[2].1.contains("charlie"));
    // The main transcript holds the three spawning calls and no other;
    // each subagent's one call is in its own.
    let tools = |blocks: &[super::super::transcript::Block]| {
        kinds(blocks)
            .iter()
            .filter(|k| k.starts_with("tool:"))
            .count()
    };
    assert_eq!(tools(&app.controller.conversation.transcript.blocks), 3);
    assert!(
        app.controller
            .conversation
            .transcript
            .agents()
            .iter()
            .all(|(_, a, _)| tools(&a.log.blocks) == 1)
    );

    // Claude: a blocking call's subagent, and one stopped by the agent.
    let mut app = test_app(HarnessId::CLAUDE);
    let mut p = ClaudeParser::new();
    replay_fixture(
        &mut app,
        "src/harness/claude/fixtures/subagent_blocking.jsonl",
        |l| p.feed(l),
    );
    assert_eq!(reports(&app), vec![(done, "alpha".to_string())]);
    let mut app = test_app(HarnessId::CLAUDE);
    let mut p = ClaudeParser::new();
    replay_fixture(
        &mut app,
        "src/harness/claude/fixtures/subagent_stopped.jsonl",
        |l| p.feed(l),
    );
    assert_eq!(
        reports(&app),
        vec![(Some(SubagentStatus::Cancelled), String::new())]
    );

    // Claude: sent back to work twice, it is one subagent throughout.
    let mut app = test_app(HarnessId::CLAUDE);
    let mut p = ClaudeParser::new();
    replay_fixture(
        &mut app,
        "src/harness/claude/fixtures/subagent_resumed.jsonl",
        |l| p.feed(l),
    );
    let got = reports(&app);
    assert_eq!(got.len(), 1);
    assert!(app.subagents.is_empty());
    assert!(got[0].0 == done && got[0].1.ends_with("**alpha**"));

    // Claude: a subagent that starts one of its own and ends first.
    let mut app = test_app(HarnessId::CLAUDE);
    let mut p = ClaudeParser::new();
    replay_fixture(
        &mut app,
        "src/harness/claude/fixtures/subagent_nested.jsonl",
        |l| p.feed(l),
    );
    assert!(app.subagents.is_empty() && !app.is_generating);
    let runs = app.controller.conversation.transcript.agents();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|(_, run, _)| run.duration.is_some()));
    assert_eq!((runs[0].1.status, runs[0].1.report()), (done, "waiting"));
    // The inner one is in the outer one's transcript.
    assert_eq!(runs[1].2, 1);
    assert_eq!(runs[1].1.status, done);
    assert!(runs[1].1.report().contains("a.txt"));

    // Claude: with no `task_notification` at all, a task's end still
    // ends its row (#58).
    let mut app = test_app(HarnessId::CLAUDE);
    let mut p = ClaudeParser::new();
    replay_fixture(
        &mut app,
        "src/harness/claude/fixtures/subagent.jsonl",
        |l| {
            if l.contains(r#""subtype":"task_notification""#) {
                Vec::new()
            } else {
                p.feed(l)
            }
        },
    );
    assert!(app.subagents.is_empty());
    let got = reports(&app);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, done);

    // Codex: the sub-agent outlives two turns, and nothing follows its
    // end on the main thread, so its report is only here.
    let mut app = test_app(HarnessId::CODEX);
    let mut p = CodexAppServerParser::new();
    replay_fixture(
        &mut app,
        "src/harness/codex/fixtures/app_server_subagent_background.jsonl",
        |l| p.feed(l),
    );
    assert!(app.subagents.is_empty() && !app.is_generating);
    assert_eq!(reports(&app), vec![(done, "alpha".to_string())]);

    let mut app = test_app(HarnessId::CODEX);
    let mut p = CodexAppServerParser::new();
    replay_fixture(
        &mut app,
        "src/harness/codex/fixtures/app_server_subagent_interrupted.jsonl",
        |l| p.feed(l),
    );
    assert_eq!(
        reports(&app),
        vec![(Some(SubagentStatus::Cancelled), String::new())]
    );
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
    assert!(app.subagents.is_empty());
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
    assert!(app.subagents.is_empty() && app.subagent_rows().is_empty());

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
    app.bridge_max_chars = Some(300);
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
    app.sandbox = SandboxSetup::null(None);
    assert!(app.caps().subagents.stop);
    app.bridge_max_chars = Some(300);
    app.submit_prompt("delegate".into());
    app.take_actions();
    app.session_alive = true;
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
    assert!(!app.is_generating);
    assert_eq!(app.active, HarnessId::CODEX);

    // Antigravity tells the end but cannot stop one: the user is only
    // told to wait.
    let mut app = test_app(HarnessId::AGY);
    assert!(app.caps().subagents.reported);
    assert!(!app.caps().subagents.stop);
    app.submit_prompt("delegate".into());
    app.take_actions();
    app.session_alive = true;
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
