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

    // A model with none takes them all away.
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate::efforts(
        Vec::new(),
    )));
    assert_eq!(notices(&app).last().unwrap(), "no effort levels now");
    assert_eq!(app.controller.effort_label(HarnessId::CLAUDE), None);
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
        rate_limit_summary(
            app.controller
                .account_status(HarnessId::CLAUDE)
                .and_then(|s| s.rate_limit.as_ref())
                .unwrap()
        ),
        "five_hour 90% used"
    );
}

/// Rate limits are the account's of the harness that reported them: they
/// are not shown for another, and each warns on its own.
#[test]
fn rate_limits_stay_with_the_harness_that_reported_them() {
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
    app.on_event(limit(85.0));
    app.switch_harness(HarnessId::CODEX);
    assert!(app.controller.account_status(HarnessId::CODEX).is_none());
    assert!(app.controller.account_status(HarnessId::CLAUDE).is_some());
    let before = notices(&app).len();
    app.on_event(limit(90.0));
    assert_eq!(notices(&app).len(), before + 1, "Codex warns for itself");
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

/// A usage report of `scope` with `input` and `output` tokens.
fn usage(input: u64, output: u64, scope: UsageScope) -> AgentEvent {
    AgentEvent::Usage(Usage {
        input,
        output,
        scope,
        ..Default::default()
    })
}

fn turn_ends(app: &mut TestApp, stop_reason: StopReason) {
    app.on_event(AgentEvent::TurnCompleted { stop_reason });
}

/// What the row shows: the turn's input and output, and the session's.
fn row(app: &TestApp) -> (u64, u64, u64, u64) {
    let c = &app.controller.conversation;
    (
        c.turn_usage.input,
        c.turn_usage.output,
        c.session_usage.input,
        c.session_usage.output,
    )
}

/// The row keeps the last turn's numbers when the next turn starts: a
/// harness that reports at the end of a turn would otherwise show
/// nothing for the whole of it (#307).
#[test]
fn a_new_turn_keeps_the_last_turns_usage_until_it_reports() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    app.controller.session_alive = true;
    app.on_event(usage(10, 5, UsageScope::Turn));
    turn_ends(&mut app, StopReason::Done);
    app.submit_prompt("two".into());
    assert!(app.controller.is_generating);
    assert_eq!(row(&app), (10, 5, 10, 5));
    app.on_event(usage(20, 7, UsageScope::Turn));
    assert_eq!(row(&app), (20, 7, 30, 12));
}

/// A report with only a cost (Claude's `/compact`) adds it to the
/// session and leaves the token numbers as they were (#307).
#[test]
fn a_cost_only_report_adds_its_cost_and_leaves_the_tokens() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    app.controller.session_alive = true;
    app.on_event(AgentEvent::Usage(Usage {
        input: 10,
        output: 5,
        cost_usd: Some(0.5),
        ..Default::default()
    }));
    turn_ends(&mut app, StopReason::Done);
    app.submit_prompt("/compact".into());
    app.on_event(AgentEvent::Usage(Usage {
        cost_usd: Some(0.25),
        ..Default::default()
    }));
    turn_ends(&mut app, StopReason::Done);
    let c = &app.controller.conversation;
    assert_eq!(row(&app), (10, 5, 10, 5));
    assert_eq!(c.turn_usage.cost_usd, Some(0.5));
    assert_eq!(c.session_usage.cost_usd, Some(0.75));
}

/// A turn that fails before it reports (a rate limit, a provider's
/// error) leaves the numbers before it (#307).
#[test]
fn a_turn_ended_by_an_error_without_usage_leaves_the_last_numbers() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    app.controller.session_alive = true;
    app.on_event(usage(10, 5, UsageScope::Turn));
    turn_ends(&mut app, StopReason::Done);
    app.submit_prompt("two".into());
    turn_ends(
        &mut app,
        StopReason::Error("You've hit your session limit".into()),
    );
    assert!(!app.controller.is_generating);
    assert_eq!(row(&app), (10, 5, 10, 5));
}

/// A harness that reports each request of a turn (pi's `turn_end`, Codex
/// app-server's `tokenUsage`) has them added up into the turn, which
/// counts up as it runs, and each added once to the session. The next
/// turn's first report starts its total again (#312).
#[test]
fn requests_add_up_into_their_turn_and_the_next_turn_starts_again() {
    let mut app = test_app(HarnessId::PI);
    app.submit_prompt("one".into());
    app.controller.session_alive = true;
    app.on_event(usage(100, 10, UsageScope::Request));
    assert_eq!(row(&app), (100, 10, 100, 10));
    app.on_event(usage(30, 3, UsageScope::Request));
    assert_eq!(row(&app), (130, 13, 130, 13));
    turn_ends(&mut app, StopReason::Done);

    app.submit_prompt("two".into());
    assert_eq!(row(&app), (130, 13, 130, 13));
    // A request that took no tokens (Codex's compaction) starts nothing.
    app.on_event(usage(0, 0, UsageScope::Request));
    assert_eq!(row(&app), (130, 13, 130, 13));
    app.on_event(usage(7, 1, UsageScope::Request));
    assert_eq!(row(&app), (7, 1, 137, 14));
    app.on_event(AgentEvent::Usage(Usage {
        cost_usd: Some(0.1),
        scope: UsageScope::Request,
        ..Default::default()
    }));
    assert_eq!(app.controller.conversation.turn_usage.cost_usd, Some(0.1));
    app.on_event(usage(2, 2, UsageScope::Request));
    assert_eq!(row(&app), (9, 3, 139, 16));
    turn_ends(&mut app, StopReason::Done);
    assert_eq!(row(&app), (9, 3, 139, 16));
}

/// Whatever puts a turn's numbers in place puts back whether its turn
/// reported (#309 restores them on a switch back): a request reported
/// before the next turn starts starts the numbers again, not adds to
/// those of a turn of another session.
#[test]
fn a_switch_away_and_back_starts_the_turns_numbers_again() {
    let mut app = test_app(HarnessId::PI);
    app.submit_prompt("one".into());
    app.controller.session_alive = true;
    app.on_event(usage(130, 13, UsageScope::Request));
    turn_ends(&mut app, StopReason::Done);
    assert!(app.controller.conversation.turn_reported);
    app.switch_harness(HarnessId::CODEX);
    app.switch_harness(HarnessId::PI);
    assert_eq!(app.controller.active, HarnessId::PI);
    assert!(!app.controller.conversation.turn_reported);
    app.on_event(usage(5, 1, UsageScope::Request));
    let t = &app.controller.conversation.turn_usage;
    assert_eq!((t.input, t.output), (5, 1));
}

/// A whole turn's report replaces the row, a second one in the same turn
/// too, and a session total replaces the session's.
#[test]
fn turn_reports_replace_the_row_and_session_totals_the_session() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("one".into());
    app.controller.session_alive = true;
    app.on_event(usage(10, 5, UsageScope::Turn));
    app.on_event(usage(1, 1, UsageScope::Turn));
    assert_eq!(row(&app), (1, 1, 11, 6));
    app.on_event(AgentEvent::Usage(Usage {
        input: 100,
        cumulative: true,
        ..Default::default()
    }));
    assert_eq!(row(&app), (1, 1, 100, 0));
}

/// Codex app-server reports each request's numbers and then the thread's
/// own total: after every report the session's figure is Codex's total
/// (cache reads apart from the input), also where adding up the requests
/// drifts from it, after a compaction and after a rollback (#311).
#[test]
fn a_codex_session_reads_codexs_own_total() {
    use crate::core::Parser;
    use crate::core::testing::fixtures_dir;
    use crate::harness::codex::app_server_parse::CodexAppServerParser;
    let dir = fixtures_dir("src/harness/codex/app_server_parse.rs");
    for case in ["app_server_steer_and_compact", "app_server_rewind"] {
        let mut app = test_app(HarnessId::CODEX);
        app.submit_prompt("one".into());
        app.controller.session_alive = true;
        let mut parser = CodexAppServerParser::new();
        let jsonl = std::fs::read_to_string(dir.join(format!("{case}.jsonl"))).unwrap();
        let mut updates = 0;
        for line in jsonl.lines().filter(|l| l.starts_with('{')) {
            for ev in parser.feed(line) {
                if matches!(ev, AgentEvent::Usage(_)) {
                    app.on_event(ev);
                }
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v["method"] != "thread/tokenUsage/updated" {
                continue;
            }
            updates += 1;
            let total = &v["params"]["tokenUsage"]["total"];
            let n = |k: &str| total[k].as_u64().unwrap();
            let s = &app.controller.conversation.session_usage;
            assert_eq!(
                (s.input, s.cache_read, s.output),
                (
                    n("inputTokens") - n("cachedInputTokens"),
                    n("cachedInputTokens"),
                    n("outputTokens")
                ),
                "{case}, update {updates}"
            );
        }
        assert!(updates >= 3, "{case}");
    }
}

/// A Codex session under `transport = "auto"` that came to `exec` reports
/// what it lacks of the app-server capabilities `auto` declares (#288):
/// the TUI stops offering it. `ask` goes too, and with nothing below it the
/// user chooses a policy. What the session lacks, `ask` included, holds
/// for the run (`auto` is resolved once per run): it stays when the
/// session ends, for what is chosen and decided between sessions (the
/// policy, a fork).
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
            && !caps.fork
            && !caps.context_usage,
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

    // Ended, the session leaves what it lacks: `ask` is not offered
    // between sessions either, and a fork now starts Codex fresh, as
    // `exec` cannot branch its thread.
    app.on_event(AgentEvent::ProcessExited { code: Some(0) });
    assert!(!app.caps().compaction && !app.caps().fork);
    assert!(!app.offered_policies().contains(&PermissionPolicy::Ask));
    assert!(!app.set_policy(PermissionPolicy::Ask));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
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
