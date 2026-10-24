//! How full the context window is, and the last turn's usage: kept with
//! each harness, told again only for the session and model measured, and
//! filled from the window the model last reported (#309).

use super::*;

fn ctx(used: Option<u64>, window: Option<u64>) -> ContextUsage {
    ContextUsage { used, window }
}

fn context(app: &TestApp) -> ContextUsage {
    app.controller.conversation.context
}

/// A turn of the active harness's session `id`, which reports `used`
/// tokens in a context of `window`, after which its process is ended.
fn turn(app: &mut TestApp, id: &str, used: u64, window: Option<u64>) {
    app.submit_prompt("go on".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: id.into(),
        model: None,
    });
    app.on_event(AgentEvent::Usage(Usage {
        input: used,
        output: 1,
        ..Default::default()
    }));
    app.on_event(AgentEvent::Context(ctx(Some(used), window)));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.controller.session_alive = false;
}

fn switch(app: &mut TestApp, next: HarnessId) {
    app.switch_harness(next);
    app.take_actions();
    app.controller.session_alive = false;
}

/// The windows file of a test app opened in `cwd` (`test_init`).
fn write_windows(cwd: &Path, json: &str) {
    let path = cwd.join(".unharness/test-state/context_windows.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

#[test]
fn a_resumed_conversation_tells_its_context_and_last_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
    turn(&mut app, "claude-1", 50_000, Some(200_000));
    let id = app.controller.conversation.record.id.clone();
    app.quit();

    // `--resume`.
    let again = test_app_in(cwd.clone(), HarnessId::CLAUDE, Some(String::new()), false);
    assert_eq!(context(&again), ctx(Some(50_000), Some(200_000)));
    assert_eq!(again.controller.conversation.turn_usage.input, 50_000);
    assert_eq!(again.controller.conversation.context.percent(), Some(25));

    // `/resume`, from a new conversation that told nothing.
    let mut fresh = test_app_in(cwd, HarnessId::CLAUDE, None, false);
    assert_eq!(context(&fresh), ctx(None, None));
    fresh.resume_conversation(id);
    assert_eq!(context(&fresh), ctx(Some(50_000), Some(200_000)));
    assert_eq!(fresh.controller.conversation.turn_usage.input, 50_000);
}

/// A fill the harness no longer knows (pi asked after a compaction it
/// gave no estimate for, #319) is dropped, also from what is saved; the
/// window stands, and a figure told after takes its place.
#[test]
fn a_fill_no_longer_known_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
    turn(&mut app, "claude-1", 50_000, Some(200_000));
    app.on_event(AgentEvent::ContextUnknown);
    assert_eq!(context(&app), ctx(None, Some(200_000)));
    app.quit();

    let mut again = test_app_in(cwd, HarnessId::CLAUDE, Some(String::new()), false);
    assert_eq!(context(&again), ctx(None, Some(200_000)));
    again.on_event(AgentEvent::Context(ctx(Some(30_000), None)));
    assert_eq!(context(&again), ctx(Some(30_000), Some(200_000)));
}

/// A run naming another model than the conversation ran: the fill was
/// measured on the one before, the window is the named model's.
#[test]
fn a_conversation_resumed_on_another_model_takes_that_model_s_window() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
    turn(&mut app, "claude-1", 50_000, Some(200_000));
    app.quit();
    write_windows(
        &cwd,
        r#"{"claude": {"default": 200000, "anthropic/opus": 1000000}}"#,
    );

    let mut init = test_init(
        cwd,
        HarnessId::CLAUDE,
        Some(String::new()),
        false,
        Config::default(),
        test_registry(),
    );
    init.launch.model = Some("opus".into());
    let app = TestApp::new(init);
    assert_eq!(app.current_model().unwrap().label(), "anthropic/opus");
    assert_eq!(context(&app), ctx(None, Some(1_000_000)));
    // The last turn happened, whatever runs next.
    assert_eq!(app.controller.conversation.turn_usage.input, 50_000);
}

#[test]
fn each_harness_keeps_its_own_context_across_a_switch() {
    let mut app = test_app(HarnessId::CLAUDE);
    turn(&mut app, "claude-1", 50_000, Some(200_000));

    switch(&mut app, HarnessId::CODEX);
    assert_eq!(context(&app), ctx(None, None));
    assert_eq!(app.controller.conversation.turn_usage, Usage::default());
    turn(&mut app, "codex-1", 10_000, Some(258_000));

    switch(&mut app, HarnessId::CLAUDE);
    assert_eq!(context(&app), ctx(Some(50_000), Some(200_000)));
    assert_eq!(app.controller.conversation.turn_usage.input, 50_000);

    switch(&mut app, HarnessId::CODEX);
    assert_eq!(context(&app), ctx(Some(10_000), Some(258_000)));
    assert_eq!(app.controller.conversation.turn_usage.input, 10_000);
}

/// Switched to before it has run in this conversation, a harness's
/// window is the one its model last reported here.
#[test]
fn a_harness_switched_to_takes_its_model_s_window() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    write_windows(
        &cwd,
        r#"{"codex": {"openai/default": 258400, "default": 400000}}"#,
    );
    let mut app = test_app_in(cwd, HarnessId::CLAUDE, None, false);
    turn(&mut app, "claude-1", 50_000, Some(200_000));
    switch(&mut app, HarnessId::CODEX);
    assert_eq!(context(&app), ctx(None, Some(400_000)));
}

/// A session forgotten is not resumed: what it filled is not told for the
/// next one, of the harness active or not, now or when resumed.
#[test]
fn a_forgotten_session_s_fill_is_not_told_again() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
    turn(&mut app, "claude-1", 50_000, Some(200_000));
    switch(&mut app, HarnessId::CODEX);
    turn(&mut app, "codex-1", 10_000, Some(258_000));

    // Codex, active, cannot rewind its session and starts a fresh one.
    app.on_event(AgentEvent::RewindFailed {
        reason: "no".into(),
    });
    assert_eq!(context(&app), ctx(None, Some(258_000)));
    // Claude, not active, has its session forgotten (a rewind before
    // where it was last active).
    app.controller.forget_ids(HarnessId::CLAUDE);
    switch(&mut app, HarnessId::CLAUDE);
    assert_eq!(context(&app), ctx(None, Some(200_000)));
    // Its last turn still happened.
    assert_eq!(app.controller.conversation.turn_usage.input, 50_000);
    app.quit();

    let again = test_app_in(cwd, HarnessId::CLAUDE, Some(String::new()), false);
    assert_eq!(context(&again), ctx(None, Some(200_000)));
}

/// A record whose fill is of another session than the one it stores for
/// that harness (written by a run that did not forget it) does not tell it.
#[test]
fn a_fill_of_another_session_than_the_one_stored_is_not_told() {
    let (_dir, mut c) = super::super::controller(None);
    let mut saved = Conversation::new(HarnessId::CLAUDE);
    saved.sessions.insert(HarnessId::CLAUDE, "claude-2".into());
    saved.context.insert(
        HarnessId::CLAUDE,
        crate::core::conversations::ContextRecord {
            used: Some(50_000),
            window: Some(200_000),
            model: "default".into(),
            session: Some("claude-1".into()),
        },
    );
    c.install(saved, HarnessId::CLAUDE);
    assert_eq!(c.conversation.context, ctx(None, Some(200_000)));
}

#[test]
fn a_new_session_in_place_of_one_not_resumed_drops_the_fill() {
    let mut app = test_app(HarnessId::CLAUDE);
    turn(&mut app, "claude-1", 50_000, Some(200_000));
    app.submit_prompt("again".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionReplaced {
        reason: "could not resume".into(),
    });
    assert_eq!(context(&app), ctx(None, Some(200_000)));
}

#[test]
fn a_model_change_drops_the_fill_and_takes_that_model_s_window() {
    let mut app = test_app(HarnessId::CLAUDE);
    turn(&mut app, "claude-1", 50_000, Some(200_000));

    app.set_model("opus".into());
    assert_eq!(context(&app), ctx(None, None));
    turn(&mut app, "claude-1", 60_000, Some(1_000_000));

    app.set_model("sonnet".into());
    assert_eq!(context(&app), ctx(None, None));
    app.set_model("opus".into());
    assert_eq!(context(&app), ctx(None, Some(1_000_000)));
    turn(&mut app, "claude-1", 70_000, None);
    assert_eq!(context(&app), ctx(Some(70_000), Some(1_000_000)));

    // The same model again changes nothing.
    app.set_model("opus".into());
    assert_eq!(context(&app), ctx(Some(70_000), Some(1_000_000)));
}

/// Claude reports its window only when a turn ends: the one its model
/// last reported here is there from the session's start, so that the
/// fill it reports at the first request can be told. One the session
/// reports takes its place.
#[test]
fn a_session_starts_with_its_model_s_window_until_it_reports_one() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    write_windows(&cwd, r#"{"claude": {"default": 1000000}}"#);
    let mut app = test_app_in(cwd, HarnessId::CLAUDE, None, false);
    assert_eq!(context(&app), ctx(None, None));
    app.submit_prompt("go".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    assert_eq!(context(&app), ctx(None, Some(1_000_000)));
    app.on_event(AgentEvent::Context(ctx(Some(100_000), None)));
    assert_eq!(app.controller.conversation.context.percent(), Some(10));
    app.on_event(AgentEvent::Context(ctx(None, Some(200_000))));
    assert_eq!(context(&app), ctx(Some(100_000), Some(200_000)));
}

/// A branch knows what the original knew, so its fill holds; a harness
/// started fresh in the copy has none yet. The usage is the copy's own.
#[test]
fn a_fork_keeps_the_fill_of_the_sessions_it_branches() {
    let mut app = test_app(HarnessId::AGY);
    turn(&mut app, "agy-1", 5_000, Some(100_000));
    switch(&mut app, HarnessId::CLAUDE);
    turn(&mut app, "claude-1", 50_000, Some(200_000));

    app.fork_conversation();
    assert!(app.controller.fork_pending(HarnessId::CLAUDE));
    assert_eq!(context(&app), ctx(Some(50_000), Some(200_000)));
    assert_eq!(app.controller.conversation.turn_usage, Usage::default());

    switch(&mut app, HarnessId::AGY);
    assert_eq!(context(&app), ctx(None, Some(100_000)));
}

#[test]
fn a_fork_drops_the_fill_of_a_session_it_cannot_branch() {
    let mut app = test_app(HarnessId::AGY);
    turn(&mut app, "agy-1", 5_000, Some(100_000));
    app.fork_conversation();
    assert!(!app.controller.fork_pending(HarnessId::AGY));
    assert_eq!(context(&app), ctx(None, Some(100_000)));
}

/// The session renewed for another model writes its handoff summary on
/// the model it ran: what it reports is not kept as the new one's window,
/// and the new session starts with that model's.
#[test]
fn a_session_renewed_for_another_model_starts_with_that_model_s_window() {
    let (mut app, _cwd) = vibe_app();
    prompted_vibe(&mut app);
    app.controller.handoff = Some(crate::core::controller::Handoff {
        to: HarnessId::VIBE,
        start: 0,
        held: false,
        renew: true,
        blind: false,
    });
    app.on_event(AgentEvent::Context(ctx(Some(9_000), Some(128_000))));
    assert!(app.controller.context_windows.is_empty());
    app.controller.handoff = None;
    app.with(|c, front| c.renew_now(front));
    assert_eq!(context(&app), ctx(None, None));
}

/// A window filled from the file is not saved: the file may know better
/// when the conversation is resumed (learned in another one meanwhile).
#[test]
fn a_window_only_filled_is_not_saved_over_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    write_windows(&cwd, r#"{"codex": {"default": 258000}}"#);
    let mut app = test_app_in(cwd.clone(), HarnessId::CLAUDE, None, false);
    turn(&mut app, "claude-1", 50_000, Some(200_000));
    switch(&mut app, HarnessId::CODEX);
    assert_eq!(context(&app), ctx(None, Some(258_000)));
    app.quit();
    let saved = app.controller.conversation.record.context[&HarnessId::CODEX].clone();
    assert_eq!(saved.window, None);

    write_windows(&cwd, r#"{"codex": {"default": 400000}}"#);
    let again = test_app_in(cwd, HarnessId::CLAUDE, Some(String::new()), false);
    assert_eq!(again.controller.active, HarnessId::CODEX);
    assert_eq!(context(&again), ctx(None, Some(400_000)));
}

/// Rewound in place, a session holds less than it said: its fill is
/// told again when it reports it.
#[test]
fn a_native_rewind_drops_the_fill() {
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
        app.on_event(AgentEvent::Context(ctx(Some(50_000), Some(200_000))));
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
    }
    app.take_actions();
    let two = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, Block::User { text, .. } if text == "two"))
        .unwrap();
    app.rewind_to(two, false);
    assert!(
        app.take_actions()
            .iter()
            .any(|a| matches!(a, Action::Command(SessionCommand::Rewind { .. })))
    );
    assert_eq!(context(&app), ctx(None, Some(200_000)));
}
