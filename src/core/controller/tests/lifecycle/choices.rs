use super::*;

#[test]
fn claude_runs_on_the_provider_it_reports_until_one_is_chosen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.controller.sandbox = SandboxSetup::null(None);
    app.submit_prompt("hello".into());
    app.take_actions();
    app.controller.session_alive = true;
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
            .any(|b| matches!(b, crate::core::transcript::Block::System(s) if *s == restarts))
    );
    assert_eq!(app.take_actions(), vec![Action::turn("again")]);
}

#[test]
fn a_choice_before_the_first_prompt_starts_the_session_again_with_it() {
    let mut app = test_app(HarnessId::CLAUDE);
    blank_session(&mut app);
    app.set_effort("high".into());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.start_when_idle();
    assert_eq!(starts(&app.take_actions()), 1);
    app.controller.session_alive = true;
    app.set_model("opus".into());
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);

    // Once prompted, the session is told.
    app.start_when_idle();
    app.submit_prompt("hello".into());
    app.take_actions();
    app.controller.session_alive = true;
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
    app.controller.session_alive = true;
    app.controller.session_blank = true;
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
    assert_eq!(app.controller.queued.len(), 1);
    vibe_renews(&mut app, "model glm-5-3");
    assert_eq!(app.current_model().unwrap().model, "glm-5-3");
    assert!(app.controller.queued.is_empty());
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
    app.controller.session_blank = true;
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
    let said = |b: &crate::core::transcript::Block| match b {
        crate::core::transcript::Block::System(s) => s.starts_with("Provider: agent"),
        crate::core::transcript::Block::Notice(n) => n.contains("for provider agent"),
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
                crate::core::transcript::Block::System(s),
                crate::core::transcript::Block::Notice(_)
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
    app.controller.bridge_max_chars = Some(300);
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
    assert!(app.controller.is_generating);
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
        crate::core::transcript::Block::Handoff { to, .. } if to == "a new Mistral Vibe session"
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
fn claude_taking_bedrock() -> TestApp {
    let tmp = tempfile::tempdir().unwrap();
    let mut app =
        test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false).holding(tmp);
    app.controller.sandbox = SandboxSetup::null(None);
    app.submit_prompt("hello".into());
    app.take_actions();
    app.controller.session_alive = true;
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
    app.new_conversation();
    assert!(app.take_actions().is_empty());
    assert_eq!(app.chosen_provider(), Some(&"bedrock".into()));
    assert_eq!(app.current_model(), None);
    assert_eq!(app.current_effort(), None);
    assert_eq!(app.controller.active, HarnessId::CLAUDE);
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
    assert!(app.controller.is_generating && !app.controller.session_blank);
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
    app.controller.bridge_max_chars = Some(300);
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
    app.controller.bridge_max_chars = Some(300);
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
    assert!(app.controller.is_generating);
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
    assert_eq!(app.controller.queued.len(), 1);
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
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.controller.sandbox = SandboxSetup::null(None);
    app.submit_prompt("hello".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.queue_prompt("held".into());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    app.take_actions();
    assert_eq!(app.controller.queued.len(), 1);
    app.set_provider("bedrock".into());
    app.take_actions();
    app.submit_prompt("after".into());
    app.on_event(AgentEvent::Restarted);
    assert_eq!(app.take_actions(), vec![Action::turn("after")]);
    assert_eq!(app.controller.queued.len(), 1);
    assert_eq!(app.controller.queued[0].text, "held");
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
    app.controller.session_alive = false;
    app.controller.start_held = true;
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
    app.controller.session_alive = false;
    app.controller.start_held = true;
    app.set_provider("vertex".into());
    assert_eq!(app.chosen_provider(), Some(&"vertex".into()));
}

/// The model a session reports is shown as the CLI's default while none is
/// chosen, and only for as long as its process runs on the provider it
/// reported it on (#329).
#[test]
fn the_reported_model_is_the_default_while_its_process_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.controller.sandbox = SandboxSetup::null(None);
    let started = |app: &mut TestApp, id: &str, model: Option<&str>| {
        app.controller.session_alive = true;
        app.on_event(AgentEvent::SessionStarted {
            session_id: id.into(),
            model: model.map(str::to_string),
        });
    };
    let haiku = Some("claude-haiku-4-5-20251001");
    let unreported = "anthropic (CLI default)";
    let reported = "anthropic/claude-haiku-4-5-20251001 (default)";
    assert_eq!(app.controller.model_label(), unreported);
    app.submit_prompt("hello".into());
    app.take_actions();
    started(&mut app, "claude-1", haiku);
    assert_eq!(app.controller.model_label(), reported);
    // A start that names no model leaves the one reported.
    started(&mut app, "claude-1", None);
    assert_eq!(app.controller.model_label(), reported);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    // The CLI exits: what it runs on is not known until the next one says.
    app.on_event(AgentEvent::ProcessExited { code: Some(0) });
    assert_eq!(app.controller.model_label(), unreported);
    started(&mut app, "claude-1", haiku);
    // A switch ends the session; the harness switched to has its own.
    app.switch_harness(HarnessId::CODEX);
    app.take_actions();
    assert_eq!(app.controller.model_label(), "openai (CLI default)");
    app.switch_harness(HarnessId::CLAUDE);
    app.take_actions();
    assert_eq!(app.controller.model_label(), unreported);

    // Another provider runs another default.
    started(&mut app, "claude-1", haiku);
    app.set_provider("bedrock".into());
    assert_eq!(app.controller.model_label(), "bedrock (CLI default)");
    app.take_actions();
    app.on_event(AgentEvent::Restarted);
    app.take_actions();

    // A model chosen is named as chosen, and what a session reports for
    // it is not taken as the default.
    started(&mut app, "claude-1", haiku);
    app.set_model("sonnet".into());
    assert_eq!(app.controller.model_label(), "bedrock/sonnet");
    app.take_actions();
    started(&mut app, "claude-2", Some("claude-sonnet-5-5"));
    assert_eq!(
        app.controller
            .choices(HarnessId::CLAUDE)
            .unwrap()
            .reported_model,
        None
    );
}
