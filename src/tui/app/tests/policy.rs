use super::*;

#[test]
fn policy_resolution_per_harness() {
    // As with `--policy auto`: agy, which has no `auto`, runs the nearest
    // less permissive policy and says so.
    let mut app = test_app(HarnessId::CLAUDE);
    app.policy_explicit = Some(PermissionPolicy::Auto);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
    assert!(app.policy_warning().is_none());
    app.switch_harness(HarnessId::AGY);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    assert!(app.policy_warning().unwrap().contains("less permissive"));

    // A policy chosen in the TUI is for the harness it was chosen on.
    assert!(app.set_policy(PermissionPolicy::Bypass));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.switch_harness(HarnessId::CLAUDE);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
    app.switch_harness(HarnessId::AGY);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
}

/// A session that cannot hold a policy (an ACP agent's mode did not
/// take) takes it out of what is offered: the nearest less permissive
/// one takes its place and the session is told, or, with none left at
/// or below it, the picker opens and prompts wait.
#[test]
fn a_policy_the_session_refuses_gives_way_as_an_unsupported_one() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    app.session_alive = true;
    app.take_actions();
    let refuse = |app: &mut App, policies: &[PermissionPolicy]| {
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            refused_policies: Some(policies.to_vec()),
            ..Default::default()
        }));
    };

    // One not in effect changes nothing.
    refuse(&mut app, &[PermissionPolicy::Bypass]);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    assert!(app.take_actions().is_empty());

    refuse(
        &mut app,
        &[PermissionPolicy::AcceptEdits, PermissionPolicy::Bypass],
    );
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    assert!(matches!(
        app.take_actions().as_slice(),
        [Action::Command(SessionCommand::SetPolicy(
            PermissionPolicy::Ask
        ))]
    ));
    assert!(app.modal.is_none());

    refuse(
        &mut app,
        &[
            PermissionPolicy::Plan,
            PermissionPolicy::Ask,
            PermissionPolicy::AcceptEdits,
            PermissionPolicy::Bypass,
        ],
    );
    assert_eq!(app.effective_policy(), None);
    assert!(matches!(app.modal, Some(Modal::Policy(_))));
    assert!(!app.offered_policies().contains(&PermissionPolicy::Ask));
    app.modal = None;
    app.submit_prompt("do it".into());
    assert!(app.take_actions().is_empty());
    assert!(!app.is_generating);
}

/// A refusal is for the session that made it (#216): it holds while
/// that session lives, and once it ends, by `/clear`, by exiting or by
/// a switch away, the policy is offered again and the next session is
/// asked for it.
#[test]
fn a_refused_policy_is_offered_again_once_the_session_ends() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    let start = |app: &mut App, id: &str| {
        blank_session(app);
        app.on_event(AgentEvent::SessionStarted {
            session_id: id.into(),
            model: None,
        });
        app.take_actions();
    };
    let refuse = |app: &mut App| {
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            refused_policies: Some(vec![PermissionPolicy::AcceptEdits]),
            ..Default::default()
        }));
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
        assert!(
            !app.offered_policies()
                .contains(&PermissionPolicy::AcceptEdits)
        );
        app.take_actions();
    };
    let offered_again = |app: &App| {
        assert!(
            app.offered_policies()
                .contains(&PermissionPolicy::AcceptEdits)
        );
        assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
        assert!(app.policy_warning().is_none());
    };

    // Refused by a session nothing was sent to yet (at launch).
    start(&mut app, "claude-1");
    refuse(&mut app);
    // It holds while the session lives, whatever else it reports.
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        slash_commands: Some(true),
        ..Default::default()
    }));
    app.open_policy_picker();
    assert!(matches!(
        &app.modal,
        Some(Modal::Policy(p)) if p.list.current() == Some(&PermissionPolicy::Ask)
    ));
    app.modal = None;
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    // `/clear` ends it (a blank session is otherwise kept), so that the
    // next one, started by the event loop, is asked again.
    app.handle_slash_command("/clear");
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(!app.session_alive);
    offered_again(&app);
    // The event loop starts the next one unprompted, under it.
    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession { resume: None }]
    );

    // Refused by a session a turn went to, which then exits.
    start(&mut app, "claude-2");
    refuse(&mut app);
    app.submit_prompt("one".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert!(
        !app.offered_policies()
            .contains(&PermissionPolicy::AcceptEdits)
    );
    app.on_event(AgentEvent::ProcessExited { code: Some(0) });
    offered_again(&app);

    // Refused, then a switch away and back.
    start(&mut app, "claude-3");
    refuse(&mut app);
    app.switch_harness(HarnessId::CODEX);
    app.switch_harness(HarnessId::CLAUDE);
    offered_again(&app);

    // Refused, then the driver is gone.
    start(&mut app, "claude-4");
    refuse(&mut app);
    app.driver_gone();
    offered_again(&app);
}

/// A policy picker opened on what a session refused is not left
/// showing it once that session is gone: one that waited for a policy
/// closes when the wanted one is in effect again, and the prompts that
/// waited are the user's to send; one the user opened is redone.
#[test]
fn a_policy_picker_on_a_refusal_follows_the_session_ending() {
    use crate::tui::herdr::State;
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    let start = |app: &mut App, id: &str| {
        blank_session(app);
        app.on_event(AgentEvent::SessionStarted {
            session_id: id.into(),
            model: None,
        });
        app.take_actions();
    };
    let refuse = |app: &mut App, policies: &[PermissionPolicy]| {
        app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
            refused_policies: Some(policies.to_vec()),
            ..Default::default()
        }));
    };
    let disabled = |app: &App, policy: PermissionPolicy| match &app.modal {
        Some(Modal::Policy(p)) => {
            let idx = PermissionPolicy::ALL
                .iter()
                .position(|q| *q == policy)
                .unwrap();
            !p.list.is_enabled(idx)
        }
        _ => panic!("no policy picker"),
    };

    // Nothing left at or below: the picker waits, and so does a prompt.
    start(&mut app, "claude-1");
    refuse(&mut app, &PermissionPolicy::ALL);
    assert_eq!(app.effective_policy(), None);
    assert!(disabled(&app, PermissionPolicy::Ask));
    app.submit_prompt("do it".into());
    assert_eq!(app.queued.len(), 1);
    assert!(app.take_actions().is_empty());
    assert_eq!(app.herdr_report().state, State::Blocked);

    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    assert!(app.modal.is_none());
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    assert_ne!(app.herdr_report().state, State::Blocked);
    assert_eq!(app.queued.len(), 1);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.starts_with("1 queued prompt(s) held"))
    );
    app.queued.clear();

    // Opened by the user on a refusal that left a policy in effect.
    start(&mut app, "claude-2");
    refuse(&mut app, &[PermissionPolicy::AcceptEdits]);
    app.open_policy_picker();
    assert!(disabled(&app, PermissionPolicy::AcceptEdits));
    app.driver_gone();
    assert!(!disabled(&app, PermissionPolicy::AcceptEdits));
}

#[test]
fn a_policy_that_is_not_available_waits_for_a_choice() {
    let tmp = tempfile::tempdir().unwrap();
    // agy has no `ask`, and nothing below it.
    let mut app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
    assert_eq!(app.effective_policy(), None);
    assert!(matches!(app.modal, Some(Modal::Policy(_))));
    assert!(
        app.status_warning()
            .unwrap()
            .contains("accept-edits, bypass")
    );

    // A prompt is kept, not sent.
    app.modal = None;
    app.submit_prompt("do it".into());
    assert!(app.take_actions().is_empty());
    assert!(!app.is_generating);
    assert!(matches!(app.modal, Some(Modal::Policy(_))));

    // `ask` cannot be chosen: the cursor starts on the least permissive
    // policy there is, and never lands on `ask`.
    let current = |app: &App| match &app.modal {
        Some(Modal::Policy(p)) => p.list.current().copied(),
        _ => None,
    };
    assert_eq!(current(&app), Some(PermissionPolicy::AcceptEdits));
    app.handle_modal_key(key(KeyCode::Up));
    assert_eq!(current(&app), Some(PermissionPolicy::Bypass));
    app.handle_modal_key(key(KeyCode::Down));
    assert_eq!(current(&app), Some(PermissionPolicy::AcceptEdits));
    assert_eq!(app.effective_policy(), None);

    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    let actions = app.take_actions();
    assert!(matches!(actions[0], Action::StartSession { .. }));
    assert!(matches!(actions[1], Action::SendTurn { .. }));

    // The choice is for this harness; the others keep what was asked for.
    assert_eq!(app.policy_requested(), PermissionPolicy::Ask);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.switch_harness(HarnessId::CLAUDE);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    app.switch_harness(HarnessId::AGY);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    assert!(app.modal.is_none());
}

#[test]
fn prompts_held_for_a_policy_keep_their_order() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
    for text in ["first", "second"] {
        app.modal = None;
        app.submit_prompt(text.into());
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
    }
    assert!(app.take_actions().is_empty());
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    let sent = |app: &mut App| {
        app.take_actions()
            .into_iter()
            .find_map(|a| match a {
                Action::SendTurn { text, .. } => Some(text),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(sent(&mut app), "first");
    app.session_alive = true;
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert_eq!(sent(&mut app), "second");
}

#[test]
fn changing_the_policy_does_not_send_a_held_prompt() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("first".into());
    app.take_actions();
    app.session_alive = true;
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
    assert_eq!(app.queued.len(), 1);
}

#[test]
fn a_harness_default_policy_stays_with_its_harness() {
    let mut config = Config::default();
    config.harnesses.insert(
        "agy".into(),
        crate::config::HarnessSettings {
            default_policy: Some("bypass".into()),
            ..Default::default()
        },
    );
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_with(
        tmp.keep(),
        HarnessId::AGY,
        None,
        false,
        config,
        test_registry(),
    );
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
    app.switch_harness(HarnessId::CLAUDE);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    app.switch_harness(HarnessId::AGY);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));

    // A policy set in the TUI is for the harness it was set on.
    app.switch_harness(HarnessId::CLAUDE);
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    app.switch_harness(HarnessId::AGY);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
    app.switch_harness(HarnessId::CLAUDE);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
}

#[test]
fn a_choice_made_for_want_of_a_policy_outlives_changes_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_asking(tmp.keep(), HarnessId::AGY, None, false);
    assert!(app.set_policy(PermissionPolicy::Bypass));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
    // Other policies are chosen on Claude: agy keeps its own.
    app.switch_harness(HarnessId::CLAUDE);
    assert!(app.set_policy(PermissionPolicy::AcceptEdits));
    assert!(app.set_policy(PermissionPolicy::Ask));
    app.modal = None;
    app.switch_harness(HarnessId::AGY);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
    assert!(app.modal.is_none());
}

#[test]
fn codex_exec_has_no_ask() {
    let registry = Arc::new(Registry::empty().with(Box::new(
        crate::harness::codex::CodexHarness::new(crate::harness::codex::CodexTransport::Exec),
    )));
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_with(
        tmp.keep(),
        HarnessId::CODEX,
        None,
        false,
        Config::default(),
        registry,
    );
    assert_eq!(app.effective_policy(), None);
    assert!(matches!(app.modal, Some(Modal::Policy(_))));
    assert!(!app.set_policy(PermissionPolicy::Ask));
    assert!(app.set_policy(PermissionPolicy::Auto));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
}

/// pi has `ask` and `bypass` only: the others are listed and cannot
/// be chosen, from the picker, `/policy` or completion.
#[test]
fn a_policy_the_harness_lacks_cannot_be_chosen() {
    let mut app = test_app(HarnessId::PI);
    assert_eq!(
        app.offered_policies(),
        vec![PermissionPolicy::Ask, PermissionPolicy::Bypass]
    );
    app.open_policy_picker();
    let Some(Modal::Policy(p)) = &app.modal else {
        panic!("no picker");
    };
    assert_eq!(p.list.items.len(), 5);
    assert_eq!(p.list.disabled_reason(0), Some("not offered by pi"));
    assert_eq!(p.list.disabled_reason(2), Some("not offered by pi"));
    assert_eq!(p.list.disabled_reason(3), Some("not offered by pi"));
    assert_eq!(p.list.current(), Some(&PermissionPolicy::Ask));
    app.handle_modal_key(key(KeyCode::Down));
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));

    // Not `ask` in its place: an error, and nothing changes.
    app.handle_slash_command("/policy auto");
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
    assert!(app.transcript.blocks.iter().any(
        |b| matches!(b, crate::tui::transcript::Block::Error(e) if e.contains("'auto'") && e.contains("ask, bypass"))
    ));

    for c in "/policy a".chars() {
        app.insert_char(c);
    }
    let offered: Vec<&str> = app.suggestions.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(offered, ["/policy ask"]);
}

/// `d` in the policy picker saves the selected policy as the harness's
/// default, in this workspace's settings or (Tab) the global ones, both
/// under the config directory, and chooses it.
#[test]
fn the_policy_picker_saves_a_harness_default() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::PI, None, false);
    let dir = app.cwd.join(".unharness/test-config");
    let saving = |app: &App| match &app.modal {
        Some(Modal::Policy(p)) => p.save,
        _ => None,
    };

    // This workspace: Esc goes back to the list, nothing is written.
    app.open_policy_picker();
    app.handle_modal_key(key(KeyCode::Down));
    app.handle_modal_key(key(KeyCode::Char('d')));
    assert_eq!(saving(&app), Some(Scope::Workspace));
    app.handle_modal_key(key(KeyCode::Esc));
    assert!(matches!(app.modal, Some(Modal::Policy(_))) && saving(&app).is_none());
    app.handle_modal_key(key(KeyCode::Char('d')));
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Bypass));
    let local = Config::scoped_path(&dir, Scope::Workspace, Some(&app.cwd)).unwrap();
    let saved = Config::load_file(&local).unwrap().unwrap();
    assert_eq!(
        saved.harnesses["pi"].default_policy.as_deref(),
        Some("bypass")
    );
    assert_eq!(
        crate::runner::configured_policy(&app.config, HarnessId::PI).unwrap(),
        PermissionPolicy::Bypass
    );
    assert!(!dir.join("config.toml").exists());

    // Every workspace, while this one's settings name another: saved,
    // and the user told which holds here.
    app.open_policy_picker();
    app.handle_modal_key(key(KeyCode::Up));
    app.handle_modal_key(key(KeyCode::Char('d')));
    app.handle_modal_key(key(KeyCode::Tab));
    assert_eq!(saving(&app), Some(Scope::Global));
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    let global = Config::load_file(&dir.join("config.toml"))
        .unwrap()
        .unwrap();
    assert_eq!(
        global.harnesses["pi"].default_policy.as_deref(),
        Some("ask")
    );
    assert!(
        notices(&app)
            .iter()
            .any(|n| n == "this workspace's settings make bypass pi's default here")
    );
    assert_eq!(
        crate::runner::configured_policy(&app.config, HarnessId::PI).unwrap(),
        PermissionPolicy::Bypass
    );

    // A row that cannot be chosen cannot be saved either.
    app.open_policy_picker();
    if let Some(Modal::Policy(p)) = &mut app.modal {
        p.list.selected = 2;
    }
    app.handle_modal_key(key(KeyCode::Char('d')));
    assert!(saving(&app).is_none());
}

/// A harness's own default comes before a workspace's `default_policy`:
/// one saved for every workspace says so where that gives way.
#[test]
fn saving_over_a_workspace_default_for_every_harness_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    let dir = app.cwd.join(".unharness/test-config");
    let local = Config::scoped_path(&dir, Scope::Workspace, Some(&app.cwd)).unwrap();
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, "default_policy = \"ask\"\n").unwrap();
    assert!(app.save_default_policy(PermissionPolicy::Bypass, Scope::Global));
    assert!(notices(&app).iter().any(|n| {
        n.starts_with("this workspace's default_policy (ask) no longer applies to Claude")
    }));
    // Not when it is the same.
    let before = notices(&app).len();
    assert!(app.save_default_policy(PermissionPolicy::Ask, Scope::Global));
    assert_eq!(notices(&app).len(), before);
}

/// A wanted policy that runs as a less permissive one: the cursor is on
/// the one that runs.
#[test]
fn the_policy_picker_starts_on_the_policy_in_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    assert!(app.set_policy(PermissionPolicy::Auto));
    app.switch_harness(HarnessId::PI);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    app.open_policy_picker();
    assert!(
        matches!(&app.modal, Some(Modal::Policy(p)) if p.list.current() == Some(&PermissionPolicy::Ask))
    );
}
