use super::*;

/// What a transcript holds, one word per block.
fn kinds(blocks: &[crate::core::transcript::Block]) -> Vec<String> {
    use crate::core::transcript::Block;
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
        crate::core::transcript::Block::Hook {
            state: crate::core::transcript::HookState::Ended(crate::core::HookOutcome::Blocked),
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
    assert!(!app.controller.is_generating);
    assert_eq!(app.controller.tracking.subagents.len(), 1);
    assert_eq!(spawn_block(&app, "spawn").status, None);
    let main = kinds(&app.controller.conversation.transcript.blocks);

    app.on_event(sub("spawn", AgentEvent::ThinkingDelta("hm".into())));
    app.on_event(sub("spawn", AgentEvent::TextDelta("Looking.".into())));
    app.on_event(sub("spawn", read_call("inner", "a.txt")));
    // Until the harness words it, the call is the activity.
    assert_eq!(
        app.controller.tracking.subagents[0].activity.as_deref(),
        Some("Read a.txt")
    );
    app.on_event(AgentEvent::SubagentProgress {
        id: "spawn".into(),
        activity: "Reading a.txt".into(),
    });
    app.on_event(sub("spawn", read_call("inner2", "b.txt")));
    assert_eq!(
        app.controller.tracking.subagents[0].activity.as_deref(),
        Some("Reading a.txt")
    );
    assert_eq!(app.controller.tracking.subagents[0].tools, 2);
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
    assert!(app.controller.tracking.subagents.is_empty());

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
    let ended = |app: &mut TestApp, result: Option<&str>| {
        app.on_event(AgentEvent::SubagentEnded {
            id: "spawn".into(),
            status: SubagentStatus::Completed,
            result: result.map(str::to_string),
        })
    };
    ended(&mut app, None);
    assert!(app.controller.tracking.subagents.is_empty());
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
    let ended = |app: &mut TestApp, id: &str| {
        app.on_event(AgentEvent::SubagentEnded {
            id: id.into(),
            status: SubagentStatus::Completed,
            result: None,
        })
    };
    ended(&mut app, "outer");
    let inner = spawn_block(&app, "inner");
    assert_eq!((inner.status, inner.duration), (None, None));
    assert_eq!(app.controller.tracking.subagents.len(), 1);
    std::thread::sleep(Duration::from_millis(20));
    ended(&mut app, "inner");
    let inner = spawn_block(&app, "inner");
    assert_eq!(inner.status, Some(SubagentStatus::Completed));
    assert!(inner.duration.unwrap() >= Duration::from_millis(20));
    assert!(app.controller.tracking.subagents.is_empty());
}

/// Feed a recorded session through its parser into the app.
fn replay_fixture(app: &mut TestApp, path: &str, mut feed: impl FnMut(&str) -> Vec<AgentEvent>) {
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
    let reports = |app: &TestApp| -> Vec<(Option<SubagentStatus>, String)> {
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
    assert!(app.controller.tracking.subagents.is_empty() && !app.controller.is_generating);
    let got = reports(&app);
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|(s, _)| *s == done));
    assert!(got[0].1.contains("alpha") && got[1].1.contains("bravo"));
    assert!(got[2].1.contains("charlie"));
    // The main transcript holds the three spawning calls and no other;
    // each subagent's one call is in its own.
    let tools = |blocks: &[crate::core::transcript::Block]| {
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
    assert!(app.controller.tracking.subagents.is_empty());
    assert!(got[0].0 == done && got[0].1.ends_with("**alpha**"));

    // Claude: a subagent that starts one of its own and ends first.
    let mut app = test_app(HarnessId::CLAUDE);
    let mut p = ClaudeParser::new();
    replay_fixture(
        &mut app,
        "src/harness/claude/fixtures/subagent_nested.jsonl",
        |l| p.feed(l),
    );
    assert!(app.controller.tracking.subagents.is_empty() && !app.controller.is_generating);
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
    assert!(app.controller.tracking.subagents.is_empty());
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
    assert!(app.controller.tracking.subagents.is_empty() && !app.controller.is_generating);
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
