use super::*;

#[test]
fn live_capabilities_and_status_events_apply() {
    use crate::core::{CapsUpdate, ContextUsage, PlanEntry, PlanStatus};
    let mut app = test_app(HarnessId::CLAUDE);
    assert!(app.caps().supports_effort("xhigh"));
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        effort_levels: Some(vec!["low".into()]),
        image_input: Some(true),
        ..Default::default()
    }));
    let caps = app.caps();
    assert!(caps.image_input && !caps.supports_effort("xhigh"));

    app.on_event(AgentEvent::Context(ContextUsage {
        used: Some(10),
        window: Some(100),
    }));
    assert_eq!(app.controller.conversation.context.percent(), Some(10));
    app.on_event(AgentEvent::PlanUpdated {
        entries: vec![PlanEntry {
            text: "step".into(),
            status: PlanStatus::InProgress,
        }],
        explanation: None,
    });
    assert_eq!(app.controller.conversation.plan.len(), 1);

    // Nothing a subagent does lands in the main transcript, least of
    // all when the call that spawned it is unknown.
    let before = app.controller.conversation.transcript.blocks.len();
    app.on_event(AgentEvent::Sub {
        parent: "p".into(),
        event: Box::new(AgentEvent::TextDelta("inner".into())),
    });
    assert_eq!(app.controller.conversation.transcript.blocks.len(), before);
    app.on_event(AgentEvent::Sub {
        parent: "p".into(),
        event: Box::new(AgentEvent::ToolCallStarted {
            id: "c".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }),
    });
    assert_eq!(app.controller.conversation.transcript.blocks.len(), before);
}

/// What the controller makes of a session's report and of a policy the
/// session moved to by itself (`Controller::report`,
/// `Controller::on_policy_changed`) is said in the transcript, in order.
#[test]
fn what_a_session_reports_is_said() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller.session_alive = true;
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        effort_levels: Some(vec!["low".into(), "high".into()]),
        provider: Some("vertex".into()),
        ..Default::default()
    }));
    app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Auto));
    let said = notices(&app);
    let at = |text: &str| {
        said.iter()
            .position(|n| n == text)
            .unwrap_or_else(|| panic!("{text:?} not in {said:?}"))
    };
    assert!(
        at("effort levels now: low, high")
            < at("Claude runs on vertex, chosen by its own configuration")
    );
    at("Claude Code (claude) now runs under policy auto");
}

#[test]
fn rate_limit_warns_once_when_a_window_runs_high() {
    use crate::core::{RateLimitInfo, RateLimitWindow};
    let limit = |pct: f32| {
        AgentEvent::RateLimit(RateLimitInfo {
            status: Some("allowed".into()),
            windows: vec![RateLimitWindow {
                label: "five_hour".into(),
                used_percent: Some(pct),
                resets_at: None,
            }],
        })
    };
    let mut app = test_app(HarnessId::CLAUDE);
    let base = app.controller.conversation.transcript.blocks.len();
    app.on_event(limit(20.0));
    assert_eq!(app.controller.conversation.transcript.blocks.len(), base);
    app.on_event(limit(85.0));
    app.on_event(limit(90.0));
    assert_eq!(
        app.controller.conversation.transcript.blocks.len(),
        base + 1
    );
    assert_eq!(
        rate_limit_summary(app.controller.rate_limit.as_ref().unwrap()),
        "five_hour 90% used"
    );
}

/// A hook whose end will not come is not left running, nor called failed.
#[test]
fn hooks_still_running_end_as_unknown() {
    use crate::core::transcript::{Block, HookState};
    let state = |app: &TestApp, n: usize| {
        app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Hook { state, .. } => Some(*state),
                _ => None,
            })
            .nth(n)
    };
    let started = |id: &str| AgentEvent::HookStarted {
        id: id.into(),
        name: "Stop".into(),
    };
    let sub_state = |app: &TestApp| match spawn_block(app, "spawn").log.blocks.first() {
        Some(Block::Hook { state, .. }) => Some(*state),
        _ => None,
    };
    let mut app = test_app(HarnessId::CODEX);
    app.submit_prompt("go".into());
    app.on_event(started("a"));
    spawn(&mut app, "spawn", "look around", None);
    app.on_event(sub("spawn", started("s")));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    // An async hook may outlive a turn that finished.
    assert_eq!(state(&app, 0), Some(HookState::Running));
    app.submit_prompt("again".into());
    app.on_event(started("b"));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    assert_eq!(state(&app, 1), Some(HookState::Unknown));
    // Not an earlier turn's, nor a sub-agent's, which outlives it.
    assert_eq!(state(&app, 0), Some(HookState::Running));
    assert_eq!(sub_state(&app), Some(HookState::Running));
    // Its end came after all: it is ended where it is.
    app.on_event(AgentEvent::HookEnded {
        id: "b".into(),
        name: "Stop".into(),
        outcome: crate::core::HookOutcome::Succeeded,
        output: String::new(),
    });
    assert_eq!(
        state(&app, 1),
        Some(HookState::Ended(crate::core::HookOutcome::Succeeded))
    );
    assert_eq!(state(&app, 2), None);
    app.on_event(started("c"));
    app.on_event(AgentEvent::ProcessExited { code: Some(0) });
    assert_eq!(state(&app, 0), Some(HookState::Unknown));
    assert_eq!(state(&app, 2), Some(HookState::Unknown));
    assert_eq!(sub_state(&app), Some(HookState::Unknown));
}

#[test]
fn usage_accumulates() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.on_event(AgentEvent::Usage(Usage {
        input: 10,
        output: 5,
        ..Default::default()
    }));
    app.on_event(AgentEvent::Usage(Usage {
        input: 1,
        output: 1,
        ..Default::default()
    }));
    assert_eq!(app.controller.conversation.turn_usage.input, 1);
    assert_eq!(app.controller.conversation.session_usage.input, 11);
    app.on_event(AgentEvent::Usage(Usage {
        input: 100,
        cumulative: true,
        ..Default::default()
    }));
    assert_eq!(app.controller.conversation.session_usage.input, 100);
}

/// A Codex session under `transport = "auto"` that came to `exec` reports
/// what it lacks of the app-server capabilities `auto` declares (#288):
/// the TUI stops offering it. `ask` goes too, and with nothing below it the
/// user chooses a policy. What the session lacks stays when it ends, for
/// what is decided between sessions (a fork); what it refused goes with
/// it, as for any session.
#[test]
fn a_codex_session_come_to_exec_takes_away_what_exec_cannot_do() {
    use crate::core::transcript::Block;
    use crate::harness::Harness;
    use crate::harness::codex::{CodexHarness, CodexTransport};
    let cwd = tempfile::tempdir().unwrap();
    let registry =
        Arc::new(Registry::empty().with(Box::new(CodexHarness::new(CodexTransport::Auto))));
    let mut app = test_app_with(
        cwd.path().to_path_buf(),
        HarnessId::CODEX,
        None,
        false,
        Config::default(),
        registry,
    );
    let report = CodexHarness::new(CodexTransport::Auto)
        .capabilities()
        .lacking(&CodexHarness::new(CodexTransport::Exec).capabilities());
    let waits = "Codex (codex) waits for another policy (Ctrl+P)";
    assert!(app.caps().compaction && app.caps().fork);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));

    // Started before the first prompt, under `ask`, it came to `exec`.
    blank_session(&mut app);
    app.on_event(AgentEvent::CapabilitiesChanged(report.clone()));
    let caps = app.caps();
    assert!(
        !caps.steer
            && !caps.compaction
            && !caps.rewind.conversation
            && !caps.subagents.stop
            && !caps.interactive_permissions
            && !caps.fork,
        "{caps:?}"
    );
    assert_eq!(
        app.offered_policies(),
        [
            PermissionPolicy::AcceptEdits,
            PermissionPolicy::Auto,
            PermissionPolicy::Bypass
        ]
    );
    assert_eq!(app.effective_policy(), None);
    assert_eq!(app.front.shown, Some(Shown::Policy));
    assert!(has_notice(&app, waits));

    // Chosen, the session nothing was sent to starts again under it, and
    // says the same: the policy stands.
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    assert_eq!(app.take_actions(), [Action::Shutdown]);
    blank_session(&mut app);
    app.on_event(AgentEvent::CapabilitiesChanged(report.clone()));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    assert_eq!(notices(&app).iter().filter(|n| *n == waits).count(), 1);

    // In a turn, a message waits for its end instead of steering it, and
    // no rewind anchor is waited for.
    app.submit_prompt("go".into());
    app.take_actions();
    app.on_event(AgentEvent::SessionStarted {
        session_id: "t1".into(),
        model: None,
    });
    assert!(app.controller.conversation.anchor_pending.is_empty());
    app.steer("left a bit".into());
    assert!(app.take_actions().is_empty());
    assert_eq!(app.controller.queued.len(), 1);
    assert!(has_notice(
        &app,
        "Codex cannot be steered mid-turn; queued for when it finishes"
    ));
    app.controller.queued.clear();
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    // A compaction is refused before anything is sent.
    app.with(|c, front| c.compact(None, front));
    assert!(app.take_actions().is_empty() && !app.controller.compacting());

    // Ended, the session leaves what it lacks, not what it refused; a
    // fork now starts Codex fresh, as `exec` cannot branch its thread.
    app.on_event(AgentEvent::ProcessExited { code: Some(0) });
    assert!(!app.caps().compaction && !app.caps().fork);
    assert!(app.offered_policies().contains(&PermissionPolicy::Ask));
    app.fork_conversation();
    assert!(
        app.controller.conversation.transcript.blocks.iter().any(|b| matches!(
            b,
            Block::System(s) if s.ends_with("Fresh sessions with the transcript as context: codex.")
        )),
        "{:?}",
        app.controller.conversation.transcript.blocks
    );
}
