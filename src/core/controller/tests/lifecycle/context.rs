//! How full the context window is, and the last turn's usage: kept with
//! each harness, told again only for the session and model measured
//! (#309).

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
