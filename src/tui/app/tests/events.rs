use super::*;

#[test]
fn herdr_hears_working_blocked_and_idle() {
    use crate::tui::herdr::State;
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    assert_eq!(app.herdr_report().state, State::Idle);

    app.controller.is_generating = true;
    assert_eq!(app.herdr_report().state, State::Working);

    // Two requests: one in the modal, one queued behind it.
    let mut requests = cargo_test_requests("cargo test");
    app.on_event(requests.remove(0));
    app.on_event(requests.remove(0));
    let report = app.herdr_report();
    assert_eq!(report.state, State::Blocked);
    assert_eq!(report.message.as_deref(), Some("allow Bash?"));

    // A picker the user opened over the queued one: still waiting.
    app.modal = None;
    app.open_effort_picker();
    app.open_sandbox_picker();
    assert!(!app.controller.tracking.pending_prompts.is_empty());
    assert_eq!(app.herdr_report().state, State::Blocked);

    app.modal = None;
    app.controller.tracking.pending_prompts.clear();
    assert_eq!(app.herdr_report().state, State::Working);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert_eq!(app.herdr_report().state, State::Idle);

    // A policy picker the user opened is not waiting on anything.
    app.open_policy_picker();
    assert_eq!(app.herdr_report().state, State::Idle);
}

#[test]
fn herdr_hears_a_session_held_for_a_policy_as_blocked() {
    use crate::tui::herdr::State;
    let tmp = tempfile::tempdir().unwrap();
    // agy has no `ask`, and nothing below it.
    let app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
    assert!(matches!(app.modal, Some(Modal::Policy(_))));
    let report = app.herdr_report();
    assert_eq!(report.state, State::Blocked);
    assert!(report.message.unwrap().contains("permission policy"));
}
