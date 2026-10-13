use super::*;

/// The sandbox is unharness's, so the level is for the run, and a
/// live session comes back under it with the next prompt.
#[test]
fn the_sandbox_level_is_chosen_for_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    app.sandbox = SandboxSetup::null(None);
    assert_eq!(app.sandbox_level().0, SandboxLevel::WorkspaceWrite);

    app.submit_prompt("hello".into());
    app.take_actions();
    app.session_alive = true;
    app.session_sandbox_level = Some(SandboxLevel::WorkspaceWrite);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    // Not during a turn.
    assert!(!app.set_sandbox(SandboxLevel::ReadOnly));
    assert_eq!(app.sandbox_level().0, SandboxLevel::WorkspaceWrite);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    // The level the session already runs under restarts nothing.
    assert!(app.set_sandbox(SandboxLevel::WorkspaceWrite));
    assert!(app.take_actions().is_empty());
    assert!(app.session_alive);

    assert!(app.set_sandbox(SandboxLevel::ReadOnly));
    assert_eq!(app.sandbox_level().0, SandboxLevel::ReadOnly);
    // The prompt typed with it, before the loop has ended the
    // session, starts the next one.
    app.submit_prompt("again".into());
    let actions = app.take_actions();
    assert_eq!(actions[0], Action::Shutdown);
    assert!(matches!(&actions[1], Action::StartSession { resume: Some(id) } if id == "claude-1"));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    // The choice holds across harnesses.
    app.switch_harness(HarnessId::CODEX);
    assert_eq!(app.sandbox_level().0, SandboxLevel::ReadOnly);
    assert!(app.status_warning().is_none());
}

#[test]
fn an_unavailable_sandbox_level_is_refused() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.sandbox = SandboxSetup {
        explicit: None,
        backend: Err("no kernel".into()),
    };
    assert!(app.status_warning().unwrap().contains("(/sandbox)"));
    assert!(!app.set_sandbox(SandboxLevel::ReadOnly));
    assert_eq!(app.sandbox.explicit, None);

    for c in "/sandbox ".chars() {
        app.insert_char(c);
    }
    let offered: Vec<&str> = app.suggestions.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(offered, ["/sandbox off"]);
    app.set_input("");

    // The picker opens on the level in effect, and the cursor never
    // lands on a level that cannot be had.
    app.open_sandbox_picker();
    let Some(Modal::Sandbox(p)) = &app.modal else {
        panic!("no picker");
    };
    assert_eq!(p.disabled_reason(0), Some("no sandbox here"));
    assert_eq!(p.current(), Some(&SandboxLevel::Off));
    app.handle_modal_key(key(KeyCode::Up));
    app.handle_modal_key(key(KeyCode::Down));
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert_eq!(app.sandbox.explicit, Some(SandboxLevel::Off));
    assert!(app.status_warning().is_none());
}
