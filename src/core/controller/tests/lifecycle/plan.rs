use super::*;

/// Claude, live and mid-turn, gone from accept-edits into plan mode.
fn planning_app() -> TestApp {
    let mut app = test_app(HarnessId::CLAUDE);
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    app.submit_prompt("change the greeting".into());
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    assert!(app.set_policy(PermissionPolicy::Plan));
    app.take_actions();
    app
}

/// Claude reports each mode it is set to; a report that comes after a
/// later choice is not the agent changing its mind.
#[test]
fn a_late_report_of_a_policy_set_does_not_undo_a_later_one() {
    use crate::core::transcript::Block;
    let mut app = planning_app();
    assert!(app.set_policy(PermissionPolicy::Ask));
    app.take_actions();
    app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Plan));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Ask));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    assert!(
        !app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Notice(n) if n.contains("plan mode")))
    );
    // Choosing the policy in effect sends nothing to report.
    assert!(app.set_policy(PermissionPolicy::Ask));
    assert!(app.take_actions().is_empty());
    // Then one it did not send is the agent's own.
    app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Plan));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Plan));
}

#[test]
fn plan_is_offered_only_where_the_harness_has_it() {
    for harness in [HarnessId::PI, HarnessId::CODEX, HarnessId::AGY] {
        let mut app = test_app(harness);
        assert!(
            !app.offered_policies().contains(&PermissionPolicy::Plan),
            "{harness}"
        );
        assert!(!app.set_policy(PermissionPolicy::Plan), "{harness}");
    }
    let app = test_app(HarnessId::CLAUDE);
    assert_eq!(app.offered_policies()[0], PermissionPolicy::Plan);
}
