use super::*;

#[test]
fn changing_the_policy_does_not_send_a_held_prompt() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("first".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.queue_prompt("later".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    // Held until the user sends it.
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    assert!(
        app.take_actions()
            .iter()
            .all(|a| !matches!(a, Action::SendTurn { .. }))
    );
    assert_eq!(app.controller.queued.len(), 1);
}
