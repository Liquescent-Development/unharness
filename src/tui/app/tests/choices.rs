use super::*;

#[test]
fn claude_runs_on_the_provider_it_reports_until_one_is_chosen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    app.sandbox = SandboxSetup::null(None);
    app.submit_prompt("hello".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    assert_eq!(app.chosen_provider(), None);
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        provider: Some("vertex".into()),
        ..Default::default()
    }));
    assert_eq!(app.current_provider(), Some(&"vertex".into()));
    assert_eq!(app.chosen_provider(), None);

    // Not during a turn: the process is started on its provider.
    app.set_provider("bedrock".into());
    assert_eq!(app.current_provider(), Some(&"vertex".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();

    app.set_provider("bedrock".into());
    assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
    // The session is told, and its driver starts Claude again on it,
    // resuming the session (`claude_starts_again_on_a_new_provider_…`,
    // e2e). The prompt typed with it waits for that.
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::SetProvider(
            "bedrock".into()
        ))]
    );
    app.submit_prompt("again".into());
    assert!(app.take_actions().is_empty());
    app.on_event(AgentEvent::Restarted);
    let restarts = format!(
        "Provider: bedrock ({} restarts on it; pick a model with Ctrl+M)",
        app.display_name()
    );
    assert!(
        app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, super::super::transcript::Block::System(s) if *s == restarts))
    );
    assert_eq!(app.take_actions(), vec![Action::turn("again")]);
}

#[test]
fn a_chosen_provider_survives_a_session_that_runs_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    app.sandbox = SandboxSetup::null(None);
    app.set_provider("bedrock".into());
    app.session_alive = true;
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
    assert!(!app.subagents.is_empty());
    app.set_provider("vertex".into());
    assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
    assert!(app.take_actions().is_empty());
}

/// Claude with a binary that exists wherever the test runs; nothing
/// starts it.
fn app_with_a_claude_binary() -> App {
    app_with_a_binary(HarnessId::CLAUDE)
}

/// `harness` with a binary that exists wherever the test runs, also
/// where the CLI is not installed; nothing starts it.
fn app_with_a_binary(harness: HarnessId) -> App {
    let mut config = Config::default();
    config.harnesses.insert(
        harness.as_str().into(),
        crate::config::HarnessSettings {
            binary: Some("/bin/sh".into()),
            ..Default::default()
        },
    );
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_with(tmp.keep(), harness, None, false, config, test_registry());
    // As in `test_app_in`: a harness without `ask` waits for a choice.
    if app.effective_policy().is_none() {
        app.controller
            .policy
            .choose_for_conversation(harness, PermissionPolicy::AcceptEdits);
        app.modal = None;
    }
    app
}

fn vertex_models() -> ListResult {
    ListResult::Models(Ok(vec![ModelInfo {
        model_ref: ModelRef::new(HarnessId::CLAUDE, "vertex", "haiku"),
        display_name: "Haiku".into(),
        description: None,
        effort_levels: None,
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
    app.sandbox = SandboxSetup {
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

#[test]
fn a_choice_before_the_first_prompt_starts_the_session_again_with_it() {
    let mut app = test_app(HarnessId::CLAUDE);
    blank_session(&mut app);
    app.set_effort("high".into());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.start_when_idle();
    assert_eq!(starts(&app.take_actions()), 1);
    app.session_alive = true;
    app.set_model("opus".into());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);

    // Once prompted, the session is told.
    app.start_when_idle();
    app.submit_prompt("hello".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.set_model("sonnet".into());
    assert!(matches!(
        &app.take_actions()[..],
        [Action::Command(SessionCommand::SetModel(_))]
    ));
}

/// Before the first prompt a change starts the session again with it.
#[test]
fn a_choice_at_spawn_before_the_first_prompt_restarts_the_session() {
    let (mut app, _cwd) = vibe_app();
    assert!(app.caps().between_turns.effort);
    blank_session(&mut app);
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate::efforts(vec![
        "off".into(),
        "high".into(),
    ])));
    app.set_effort("off".into());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession { resume: None }]
    );
}

/// vibe keeps the model and effort a session last ran: after a prompt
/// a change is sent to the session, whose driver says it needs a new one
/// (`SessionRenewed`); the session is left, and the next prompt tells the
/// new one the conversation so far. Not while a turn runs. A prompt typed
/// before the answer waits for it, and goes to the new session.
#[test]
fn a_choice_at_spawn_after_a_prompt_starts_a_new_bridged_session() {
    let (mut app, _cwd) = vibe_app();
    prompted_vibe(&mut app);

    // Mid-turn: refused, nothing chosen.
    app.set_effort("off".into());
    app.set_model("glm-5-3".into());
    assert!(app.take_actions().is_empty());
    assert_eq!(app.current_effort(), None);
    assert_eq!(app.current_model(), None);

    app.on_event(AgentEvent::TextDelta("noted".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.set_effort("off".into());
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::SetEffort(Some(
            "off".into()
        )))]
    );
    assert_eq!(app.current_effort(), Some("off"));
    // The session is left only once its driver says so.
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    vibe_renews(&mut app, "effort off");
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("starts a new session"))
    );
    // The conversation no longer names the old session.
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    assert!(
        !app.controller
            .conversation
            .record
            .sessions
            .contains_key(&HarnessId::VIBE)
    );

    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession { resume: None }]
    );
    app.session_alive = true;
    app.session_blank = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "v2".into(),
        model: Some("mistral-medium-3.5".into()),
    });
    let sent = {
        app.submit_prompt("which word?".into());
        sent_turn(&mut app)
    };
    assert!(sent.contains("remember the word kumquat"), "{sent}");
    assert!(sent.contains("noted"), "{sent}");
    assert!(sent.ends_with("which word?"));
    assert_eq!(
        app.controller
            .conversation
            .session_ids
            .get(&HarnessId::VIBE)
            .map(String::as_str),
        Some("v2")
    );
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    // A model change goes the same way, and the bridge holds both turns,
    // also for a prompt typed before the session answered.
    app.set_model("glm-5-3".into());
    assert!(matches!(
        &app.take_actions()[..],
        [Action::Command(SessionCommand::SetModel(m))] if m.model == "glm-5-3"
    ));
    app.submit_prompt("and now?".into());
    assert!(app.take_actions().is_empty());
    assert_eq!(app.queued.len(), 1);
    vibe_renews(&mut app, "model glm-5-3");
    assert_eq!(app.current_model().unwrap().model, "glm-5-3");
    assert!(app.queued.is_empty());
    let actions = app.take_actions();
    assert_eq!(
        actions[..2],
        [Action::Shutdown, Action::StartSession { resume: None }]
    );
    let Some(Action::SendTurn { text, .. }) = actions.get(2) else {
        panic!("{actions:?}");
    };
    assert!(text.contains("remember the word kumquat") && text.contains("which word?"));
    assert!(text.ends_with("and now?"), "{text}");
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
}

/// A stored vibe session the TUI started before the first prompt, or none
/// live at all: no driver says what the session keeps, so the declaration
/// does (`Capabilities::stored_session_keeps`), and the conversation goes
/// on in a new session, never on the stored one started again as it ran.
#[test]
fn a_choice_for_a_stored_vibe_session_with_none_prompted_starts_a_new_one() {
    let (mut app, _cwd) = vibe_app();
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    // Resumed, nothing sent yet.
    app.session_blank = true;
    app.set_effort("off".into());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("starts a new session for effort off"))
    );
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );

    // None live: the same, before anything starts.
    let (mut app, _cwd) = vibe_app();
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    app.take_actions();
    app.set_model("glm-5-3".into());
    assert!(app.take_actions().is_empty());
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    app.start_when_idle();
    assert_eq!(
        app.take_actions(),
        vec![Action::StartSession { resume: None }]
    );
}

/// With a provider in `[harnesses.vibe.providers]` vibe has more than
/// one, chosen when it starts as its model is: before the first prompt
/// a change starts the session again, after one it starts a new
/// session told the conversation, and never mid-turn.
#[test]
fn a_vibe_provider_goes_with_the_model() {
    let (mut app, _cwd) = vibe_app_with_provider();
    assert!(app.caps().multi_provider && app.caps().between_turns.provider);

    blank_session(&mut app);
    app.set_provider("lan".into());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert_eq!(app.chosen_provider(), Some(&"lan".into()));

    app.start_when_idle();
    app.take_actions();
    prompted_vibe(&mut app);
    // Mid-turn: refused.
    app.set_provider("agent".into());
    assert!(app.take_actions().is_empty());
    assert_eq!(app.chosen_provider(), Some(&"lan".into()));

    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.set_provider("agent".into());
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::SetProvider("agent".into()))]
    );
    assert_eq!(app.chosen_provider(), Some(&"agent".into()));
    vibe_renews(&mut app, "provider agent");
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("starts a new session for provider agent"))
    );
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    // Said with the answer, before the new session's notice.
    let said = |b: &super::super::transcript::Block| match b {
        super::super::transcript::Block::System(s) => s.starts_with("Provider: agent"),
        super::super::transcript::Block::Notice(n) => n.contains("for provider agent"),
        _ => false,
    };
    let lines: Vec<_> = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .filter(|b| said(b))
        .collect();
    assert!(
        matches!(
            lines[..],
            [
                super::super::transcript::Block::System(s),
                super::super::transcript::Block::Notice(_)
            ] if s == "Provider: agent (pick a model with Ctrl+M)"
        ),
        "{lines:?}"
    );
}

/// A conversation too long for the new session's bridge: the session
/// left writes a handoff summary first, as for a switch.
#[test]
fn a_new_session_for_a_choice_gets_a_handoff_when_over_budget() {
    let (mut app, _cwd) = vibe_app();
    app.bridge_max_chars = Some(300);
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::TextDelta("x".repeat(600)));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.set_effort("off".into());
    app.take_actions();
    vibe_renews(&mut app, "effort off");
    let ask = sent_turn(&mut app);
    assert!(ask.contains("new session of yours"), "{ask}");
    assert!(app.is_generating);
    // The old session is still the one asked.
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    app.on_event(AgentEvent::TextDelta("Goal: remember kumquat.".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert!(app.take_actions().contains(&Action::Shutdown));
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    assert!(app.controller.conversation.transcript.blocks.iter().any(|b| matches!(
        b,
        super::super::transcript::Block::Handoff { to, .. } if to == "a new Mistral Vibe session"
    )));
}

#[test]
fn a_model_chosen_after_a_failed_start_starts_the_session() {
    let mut app = test_app(HarnessId::CODEX);
    app.start_failed("thread/start failed: unknown model".into());
    app.start_when_idle();
    assert!(app.take_actions().is_empty());
    app.set_model("gpt-5".into());
    app.start_when_idle();
    assert_eq!(starts(&app.take_actions()), 1);
}

/// Claude after a turn, with a provider sent to it and not yet answered.
fn claude_taking_bedrock() -> App {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    app.sandbox = SandboxSetup::null(None);
    app.submit_prompt("hello".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.set_provider("bedrock".into());
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::SetProvider(
            "bedrock".into()
        ))]
    );
    app
}

/// Until the session has said how it took a choice, another it takes only
/// between turns is refused, and so is what would end the session: the
/// first answer is the only one, and a prompt typed meanwhile goes to the
/// process started for it, once.
#[test]
fn a_choice_waits_for_the_one_before_to_be_taken() {
    let mut app = claude_taking_bedrock();
    let still = format!(
        "{} is still taking the provider just chosen; try again in a moment",
        app.short_name()
    );
    app.set_provider("vertex".into());
    // Nor a model or an effort, which would go to the new process (Claude
    // would take an effort as a turn).
    app.set_model("opus".into());
    app.set_effort("high".into());
    app.switch_harness(HarnessId::CODEX);
    app.handle_slash_command("/clear");
    assert!(app.take_actions().is_empty());
    assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
    assert_eq!(app.current_model(), None);
    assert_eq!(app.current_effort(), None);
    assert_eq!(app.active, HarnessId::CLAUDE);
    assert_eq!(
        notices(&app).iter().filter(|n| **n == still).count(),
        5,
        "{:?}",
        notices(&app)
    );
    app.submit_prompt("again".into());
    assert!(app.take_actions().is_empty());
    app.on_event(AgentEvent::Restarted);
    assert_eq!(app.take_actions(), vec![Action::turn("again")]);
    assert!(app.is_generating && !app.session_blank);
    // Taken: the next choice goes.
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.set_provider("vertex".into());
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::SetProvider(
            "vertex".into()
        ))]
    );
}

/// Two vibe choices in one burst, over the bridge's budget: the second is
/// refused, and the old session is asked for one handoff summary.
#[test]
fn a_second_vibe_choice_before_the_answer_is_refused() {
    let (mut app, _cwd) = vibe_app();
    app.bridge_max_chars = Some(300);
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::TextDelta("x".repeat(600)));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.set_effort("off".into());
    app.set_model("glm-5-3".into());
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::SetEffort(Some(
            "off".into()
        )))]
    );
    assert_eq!(app.current_model(), None);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.ends_with("is still taking the effort just chosen; try again in a moment"))
    );
    vibe_renews(&mut app, "effort off");
    let actions = app.take_actions();
    assert_eq!(
        actions
            .iter()
            .filter(|a| matches!(a, Action::SendTurn { .. }))
            .count(),
        1,
        "{actions:?}"
    );
}

/// A driver that answers one choice twice, the second time while the
/// session writes the handoff summary its first answer asked for: the
/// summary goes on, and no second one or renewal is started.
#[test]
fn a_second_renewal_during_the_summary_changes_nothing() {
    let (mut app, _cwd) = vibe_app();
    app.bridge_max_chars = Some(300);
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::TextDelta("x".repeat(600)));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.set_effort("off".into());
    app.take_actions();
    vibe_renews(&mut app, "effort off");
    assert!(sent_turn(&mut app).contains("new session of yours"));
    let said = app.controller.conversation.transcript.blocks.len();
    vibe_renews(&mut app, "effort off");
    assert!(app.take_actions().is_empty());
    assert_eq!(app.controller.conversation.transcript.blocks.len(), said);
    assert!(app.is_generating);
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
}

/// Quitting before vibe has answered a choice: the stored session, which
/// keeps what it last ran, is forgotten as the answer would have had it,
/// so that a resume does not bring it back as it ran.
#[test]
fn a_vibe_session_left_before_it_answers_a_choice_is_forgotten() {
    let (mut app, _cwd) = vibe_app();
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    app.set_effort("off".into());
    app.take_actions();
    app.quit();
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    let saved = app
        .controller
        .store
        .load(&app.controller.conversation.record.id)
        .unwrap();
    assert!(!saved.sessions.contains_key(&HarnessId::VIBE));
    assert_eq!(app.current_effort(), Some("off"));

    // So does a session that exits before it answers.
    let (mut app, _cwd) = vibe_app();
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.set_model("glm-5-3".into());
    app.submit_prompt("and now?".into());
    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    assert!(
        !app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );
    // What was typed after it waits for Enter.
    assert_eq!(app.queued.len(), 1);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n == "1 queued prompt(s) held; press Enter to send the next")
    );
}

/// Prompts held after an interrupted turn are the user's to send: a
/// choice taken sends only what was typed after it.
#[test]
fn a_choice_taken_sends_only_what_was_typed_after_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    app.sandbox = SandboxSetup::null(None);
    app.submit_prompt("hello".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.queue_prompt("held".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    app.take_actions();
    assert_eq!(app.queued.len(), 1);
    app.set_provider("bedrock".into());
    app.take_actions();
    app.submit_prompt("after".into());
    app.on_event(AgentEvent::Restarted);
    assert_eq!(app.take_actions(), vec![Action::turn("after")]);
    assert_eq!(app.queued.len(), 1);
    assert_eq!(app.queued[0].text, "held");
}

/// A vibe choice for a stored session waits for the vibe before it to
/// exit (the start held for it resumes that session); Claude's provider,
/// which a held start takes up, does not.
#[test]
fn a_choice_waits_for_the_cli_before_only_where_the_stored_session_keeps_it() {
    let (mut app, _cwd) = vibe_app();
    prompted_vibe(&mut app);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.take_actions();
    app.session_alive = false;
    app.start_held = true;
    app.set_effort("off".into());
    assert_eq!(app.current_effort(), None);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.ends_with("is still exiting; try again in a moment"))
    );
    assert!(
        app.controller
            .conversation
            .session_ids
            .contains_key(&HarnessId::VIBE)
    );

    let mut app = claude_taking_bedrock();
    app.on_event(AgentEvent::Restarted);
    app.session_alive = false;
    app.start_held = true;
    app.set_provider("vertex".into());
    assert_eq!(app.chosen_provider(), Some(&"vertex".into()));
}
