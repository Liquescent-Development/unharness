use super::*;
use serde_json::Value;

fn denied(id: &str, reason: &str) -> Action {
    Action::Command(SessionCommand::RespondPermission {
        id: id.into(),
        decision: PermissionDecision::Deny {
            reason: reason.into(),
        },
    })
}

/// Claude, live and mid-turn, gone from accept-edits into plan mode.
fn planning_app() -> App {
    let mut app = test_app(HarnessId::CLAUDE);
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    app.submit_prompt("change the greeting".into());
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    assert!(app.set_policy(PermissionPolicy::Plan));
    app.take_actions();
    app
}

fn plan_request() -> AgentEvent {
    AgentEvent::PermissionRequest(PermissionRequest {
        id: "plan".into(),
        kind: PermissionKind::PlanApproval {
            plan: "# Plan\n\nSay hi.".into(),
            plan_file: Some("/p.md".into()),
        },
        tool_call_id: Some("t1".into()),
    })
}

#[test]
fn an_approved_plan_is_carried_out_under_the_policy_chosen_with_it() {
    use PermissionPolicy::*;
    let mut app = planning_app();
    assert_eq!(app.effective_policy(), Some(Plan));
    app.on_event(plan_request());
    let Some(Modal::Plan(m)) = &app.modal else {
        panic!("{:?}", app.modal);
    };
    // Not plan, nor bypass, which Claude cannot switch to here; the
    // one it planned from comes first.
    assert_eq!(m.policies, vec![Ask, AcceptEdits, Auto]);
    assert_eq!(m.current(), Some(AcceptEdits));
    app.handle_modal_key(key(KeyCode::Down));
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    // The policy before the approval: Claude puts back the mode it
    // planned from only while it still plans.
    assert_eq!(
        app.take_actions(),
        vec![
            Action::Command(SessionCommand::SetPolicy(Auto)),
            allowed("plan")
        ]
    );
    assert_eq!(app.effective_policy(), Some(Auto));
    // Claude then reports the mode it was set to.
    app.on_event(AgentEvent::PolicyChanged(Auto));
    assert_eq!(app.effective_policy(), Some(Auto));

    // A session started under bypass can go back to it.
    let mut app = test_app(HarnessId::CLAUDE);
    assert!(app.set_policy(Bypass));
    app.session_alive = true;
    app.session_policy = Some(Bypass);
    assert!(app.set_policy(Plan));
    app.on_event(plan_request());
    let Some(Modal::Plan(m)) = &app.modal else {
        panic!("{:?}", app.modal);
    };
    assert_eq!(m.policies, vec![Ask, AcceptEdits, Auto, Bypass]);
    assert_eq!(m.current(), Some(Bypass));
}

#[test]
fn a_plan_not_approved_keeps_the_agent_planning() {
    let mut app = planning_app();
    app.on_event(plan_request());
    // From accept-edits up past ask to the last row.
    app.handle_modal_key(key(KeyCode::Up));
    app.handle_modal_key(key(KeyCode::Up));
    app.handle_modal_key(key(KeyCode::Enter));
    for c in "Use Howdy, keep it short".chars() {
        app.handle_modal_key(key(KeyCode::Char(c)));
    }
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(
        app.take_actions(),
        vec![denied("plan", "Use Howdy, keep it short")]
    );
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Plan));

    // Esc is never an approval, and drops what was typed.
    app.on_event(plan_request());
    app.handle_modal_key(key(KeyCode::Up));
    app.handle_modal_key(key(KeyCode::Up));
    app.handle_modal_key(key(KeyCode::Enter));
    app.handle_modal_key(key(KeyCode::Char('x')));
    app.handle_modal_key(key(KeyCode::Esc));
    assert!(app.take_actions().is_empty());
    app.handle_modal_key(key(KeyCode::Esc));
    assert_eq!(app.take_actions(), vec![denied("plan", "")]);

    // Nor is a handoff: the summary is written instead.
    app.handoff = Some(Handoff {
        to: HarnessId::CODEX,
        start: 0,
        held: false,
        renew: false,
        blind: false,
    });
    app.on_event(plan_request());
    assert!(app.modal.is_none());
    assert!(matches!(
        &app.take_actions()[..],
        [Action::Command(SessionCommand::RespondPermission {
            decision: PermissionDecision::Deny { .. },
            ..
        })]
    ));
}

#[test]
fn while_planning_a_rule_answers_only_a_read() {
    let mut app = planning_app();
    app.rules
        .append(Scope::Workspace, &[Rule::edit("**"), Rule::read("**")])
        .unwrap();
    let request = |id: &str, action: ToolAction| {
        AgentEvent::PermissionRequest(PermissionRequest {
            id: id.into(),
            kind: PermissionKind::ToolUse {
                tool: "Write".into(),
                input: Value::Null,
                action,
                description: None,
            },
            tool_call_id: None,
        })
    };
    let file = app.cwd.join("notes.txt");
    app.on_event(request("read", ToolAction::Read { path: file.clone() }));
    assert_eq!(app.take_actions(), vec![allowed("read")]);
    app.on_event(request(
        "write",
        ToolAction::Edit {
            paths: vec![file.clone()],
        },
    ));
    assert!(app.take_actions().is_empty());
    assert_eq!(
        app.modal.as_ref().and_then(Modal::request_id),
        Some("write")
    );
    app.close_modal();
    app.take_actions();

    // Out of plan mode the same rule answers it.
    app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Ask));
    app.on_event(request("write", ToolAction::Edit { paths: vec![file] }));
    assert_eq!(app.take_actions(), vec![allowed("write")]);
}

/// A request made while planning that waited behind the plan's
/// approval is still one made while planning.
#[test]
fn a_request_made_while_planning_is_judged_so_after_the_approval() {
    let mut app = planning_app();
    app.rules
        .append(Scope::Workspace, &[Rule::edit("**")])
        .unwrap();
    app.on_event(plan_request());
    app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
        id: "write".into(),
        kind: PermissionKind::ToolUse {
            tool: "Write".into(),
            input: Value::Null,
            action: ToolAction::Edit {
                paths: vec![app.cwd.join("notes.txt")],
            },
            description: None,
        },
        tool_call_id: None,
    }));
    assert_eq!(app.pending_prompts.len(), 1);
    // Approved under accept-edits (highlighted: the policy planned from).
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(
        app.take_actions(),
        vec![
            Action::Command(SessionCommand::SetPolicy(PermissionPolicy::AcceptEdits)),
            allowed("plan")
        ]
    );
    assert_eq!(
        app.modal.as_ref().and_then(Modal::request_id),
        Some("write")
    );
}

/// Claude reports each mode it is set to; a report that comes after a
/// later choice is not the agent changing its mind.
#[test]
fn a_late_report_of_a_policy_set_does_not_undo_a_later_one() {
    use crate::tui::transcript::Block;
    let mut app = planning_app();
    assert!(app.set_policy(PermissionPolicy::Ask));
    app.take_actions();
    app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Plan));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Ask));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    assert!(
        !app.transcript
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

/// The model can go into plan mode by itself (`EnterPlanMode`).
#[test]
fn the_agent_going_into_plan_mode_by_itself_is_followed() {
    use crate::tui::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    app.session_alive = true;
    app.on_event(AgentEvent::PolicyChanged(PermissionPolicy::Plan));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Plan));
    assert!(
        app.transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Notice(n) if n.contains("went into plan mode")))
    );
    app.on_event(plan_request());
    let Some(Modal::Plan(m)) = &app.modal else {
        panic!("{:?}", app.modal);
    };
    assert_eq!(m.current(), Some(PermissionPolicy::AcceptEdits));
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
