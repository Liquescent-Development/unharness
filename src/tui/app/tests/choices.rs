use super::*;

#[test]
fn a_chosen_provider_survives_a_session_that_runs_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.controller.sandbox = SandboxSetup::null(None);
    app.set_provider("bedrock".into());
    app.controller.session_alive = true;
    // Claude's settings chose otherwise.
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        provider: Some("anthropic".into()),
        ..Default::default()
    }));
    assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
    assert!(
        app.status_warning()
            .unwrap()
            .contains("runs on anthropic, not the chosen bedrock")
    );
    // Once it runs where it was told, the warning goes.
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        provider: Some("bedrock".into()),
        ..Default::default()
    }));
    assert!(app.status_warning().is_none());

    // Choosing it again changes nothing, and hides nothing.
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        provider: Some("anthropic".into()),
        ..Default::default()
    }));
    app.set_provider("bedrock".into());
    assert!(app.status_warning().is_some());

    // Not while a subagent is at work: it would end with the process.
    app.on_event(AgentEvent::SubagentStarted {
        id: "t1".into(),
        description: "look around".into(),
        kind: None,
        unreported_end: false,
    });
    assert!(!app.controller.tracking.subagents.is_empty());
    app.set_provider("vertex".into());
    assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
    assert!(app.take_actions().is_empty());
}

/// Claude with a binary that exists wherever the test runs; nothing
/// starts it.
fn app_with_a_claude_binary() -> TestApp {
    app_with_a_binary(HarnessId::CLAUDE)
}

/// `harness` with a binary that exists wherever the test runs, also
/// where the CLI is not installed; nothing starts it.
fn app_with_a_binary(harness: HarnessId) -> TestApp {
    let mut config = Config::default();
    config.harnesses.insert(
        harness.as_str().into(),
        crate::config::HarnessSettings {
            binary: Some("/bin/sh".into()),
            ..Default::default()
        },
    );
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_with(
        tmp.path().to_path_buf(),
        harness,
        None,
        false,
        config,
        test_registry(),
    );
    // As in `test_app_in`: a harness without `ask` waits for a choice.
    if app.effective_policy().is_none() {
        app.controller
            .policy
            .choose_for_conversation(harness, PermissionPolicy::AcceptEdits);
        app.modal = None;
    }
    TestApp::holding(app, tmp)
}

fn vertex_models() -> ListResult {
    ListResult::Models(Ok(vec![ModelInfo {
        model_ref: ModelRef::new(HarnessId::CLAUDE, "vertex", "haiku"),
        display_name: "Haiku".into(),
        description: None,
        effort_levels: None,
        is_default: false,
    }]))
}

#[test]
fn completion_asks_for_a_list_once() {
    let mut app = app_with_a_claude_binary();
    app.controller
        .put_provider(HarnessId::CLAUDE, "vertex".into());
    let wanted = ListRequest::Models(HarnessId::CLAUDE, "vertex".into());
    app.input = "/model ha".into();
    app.update_suggestions();
    assert_eq!(app.take_list_jobs().len(), 1);
    // A failure does not start it again by itself.
    app.on_list(wanted.clone(), ListResult::Models(Err("timed out".into())));
    app.update_suggestions();
    assert!(app.take_list_jobs().is_empty());
    // The picker does, and what comes is offered.
    app.open_model_picker();
    assert_eq!(app.take_list_jobs().len(), 1);
    app.on_list(wanted, vertex_models());
    assert!(app.suggestions.iter().any(|s| s.0 == "/model haiku"));

    // Nor when the list cannot even be asked for (it was a stack
    // overflow: the failure re-ran completion, which asked again).
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller.sandbox = SandboxSetup {
        explicit: Some(SandboxLevel::ReadOnly),
        backend: Err("no kernel".into()),
    };
    assert!(app.session_sandbox().is_err());
    app.input = "/model x".into();
    app.update_suggestions();
    app.update_suggestions();
    assert!(app.take_list_jobs().is_empty());
}

#[test]
fn a_list_the_user_moved_on_from_does_not_open() {
    let mut app = app_with_a_claude_binary();
    app.controller
        .put_provider(HarnessId::CLAUDE, "vertex".into());
    let vertex = ListRequest::Models(HarnessId::CLAUDE, "vertex".into());
    app.open_model_picker();
    app.set_provider("bedrock".into());
    app.modal = None;
    app.on_list(vertex.clone(), vertex_models());
    assert!(app.modal.is_none());

    // Nor over a prompt being typed: it is offered instead.
    app.controller
        .put_provider(HarnessId::CLAUDE, "vertex".into());
    app.controller.forget_model_lists();
    app.open_model_picker();
    app.input = "half a prompt".into();
    app.on_list(vertex, vertex_models());
    assert!(app.modal.is_none());
}

#[test]
fn model_lists_arrive_without_holding_the_screen() {
    let mut app = app_with_a_claude_binary();
    app.controller
        .put_provider(HarnessId::CLAUDE, "vertex".into());
    let wanted = ListRequest::Models(HarnessId::CLAUDE, "vertex".into());
    app.open_model_picker();
    assert!(app.modal.is_none());
    // Asked once, however often the user presses Ctrl+M meanwhile.
    app.open_model_picker();
    let jobs = app.take_list_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].request, wanted);

    // A failure is shown and not kept: the next Ctrl+M asks again.
    app.on_list(wanted.clone(), ListResult::Models(Err("timed out".into())));
    assert!(app.modal.is_none());
    app.open_model_picker();
    assert_eq!(app.take_list_jobs().len(), 1);

    let model = ModelInfo {
        model_ref: ModelRef::new(HarnessId::CLAUDE, "vertex", "haiku"),
        display_name: "Haiku".into(),
        description: None,
        effort_levels: None,
        is_default: false,
    };
    app.on_list(wanted.clone(), ListResult::Models(Ok(vec![model.clone()])));
    assert!(matches!(app.modal, Some(Modal::Model(_))));
    app.modal = None;
    app.open_model_picker();
    assert!(matches!(app.modal, Some(Modal::Model(_))));
    assert!(app.take_list_jobs().is_empty());

    // One that comes after the user moved on is kept, not opened.
    app.modal = None;
    app.open_provider_picker();
    let providers = ListRequest::Providers(HarnessId::CLAUDE);
    assert_eq!(app.take_list_jobs()[0].request, providers);
    app.switch_harness(HarnessId::CODEX);
    app.on_list(
        providers,
        ListResult::Providers(Ok(vec![("anthropic".into(), "Anthropic".into())])),
    );
    assert!(!matches!(app.modal, Some(Modal::Provider(_))));
}

#[test]
fn a_harness_started_with_the_first_prompt_is_asked_for_its_commands() {
    let mut app = app_with_a_binary(HarnessId::AGY);
    app.start_when_idle();
    assert!(app.take_actions().is_empty());
    let jobs = app.take_list_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].request, ListRequest::Commands(HarnessId::AGY));
    app.start_when_idle();
    assert!(app.take_list_jobs().is_empty(), "asked twice");

    app.input = "/pine".into();
    app.update_suggestions();
    assert!(app.suggestions.is_empty());
    let skill = HarnessCommand::new("pineapple", Some("A code word"), None).unwrap();
    app.on_list(
        ListRequest::Commands(HarnessId::AGY),
        ListResult::Commands(Ok(Some(vec![skill]))),
    );
    assert!(app.caps().slash_commands);
    assert_eq!(app.suggestions[0].0, "/pineapple");
    app.start_when_idle();
    assert!(app.take_list_jobs().is_empty(), "asked again once listed");

    // One that failed says why, once, and is not asked again.
    let mut app = app_with_a_binary(HarnessId::AGY);
    app.start_when_idle();
    app.take_list_jobs();
    app.on_list(
        ListRequest::Commands(HarnessId::AGY),
        ListResult::Commands(Err("timed out".into())),
    );
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("could not be listed: timed out"))
    );
    app.start_when_idle();
    assert!(app.take_list_jobs().is_empty());
}
