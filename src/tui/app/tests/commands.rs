use super::*;

#[test]
fn slash_commands_and_suggestions() {
    let mut app = test_app(HarnessId::CLAUDE);
    // Whatever is on this machine's PATH.
    for o in &mut app.harness_options {
        o.installed = true;
    }
    for c in "/pol".chars() {
        app.insert_char(c);
    }
    assert_eq!(app.suggestions.len(), 1);
    app.accept_suggestion();
    assert_eq!(app.input, "/policy");
    app.insert_char(' ');
    app.insert_char('b');
    assert_eq!(app.suggestions[0].0, "/policy bypass");
    app.set_input("");
    // Levels are offered where there is a backend for them.
    app.sandbox = SandboxSetup::null(Some(SandboxLevel::Off));
    for c in "/sandbox r".chars() {
        app.insert_char(c);
    }
    assert_eq!(app.suggestions[0].0, "/sandbox read-only");
    app.handle_slash_command("/sandbox bogus");
    assert_eq!(app.sandbox.explicit, Some(SandboxLevel::Off));
    app.handle_slash_command("/sandbox");
    assert!(matches!(app.modal, Some(Modal::Sandbox(_))));
    app.modal = None;

    app.handle_slash_command("/effort xhigh");
    assert_eq!(app.current_effort(), Some("xhigh"));
    app.handle_slash_command("/effort bogus");
    assert_eq!(app.current_effort(), Some("xhigh"));
    app.handle_slash_command("/model sonnet");
    assert_eq!(app.model_label(), "anthropic/sonnet");
    app.handle_slash_command("/switch codex");
    assert_eq!(app.active, HarnessId::CODEX);
    app.handle_slash_command("/quit");
    assert!(app.should_quit);
}

/// Claude, its session having listed `/hello`, `/clear` and `/model`.
fn app_with_claude_commands() -> App {
    let mut app = test_app(HarnessId::CLAUDE);
    let command = |name: &str, hint: Option<&str>| {
        HarnessCommand::new(name, Some(&format!("{name} it\nmore")), hint).unwrap()
    };
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        commands: Some(vec![
            command("hello", Some("<name>")),
            HarnessCommand {
                aliases: vec!["new".into(), "reset".into()],
                ..command("clear", None)
            },
            command("model", None),
            command("Deploy", None),
        ]),
        ..Default::default()
    }));
    app
}

#[test]
fn a_command_unharness_lacks_goes_to_a_harness_that_runs_its_own() {
    let mut app = app_with_claude_commands();
    app.handle_slash_command("/hello world");
    assert_eq!(
        app.take_actions(),
        vec![
            Action::StartSession { resume: None },
            Action::turn("/hello world")
        ]
    );
    assert_eq!(
        notices(&app),
        vec!["/hello is Claude's own command: passed on"]
    );
    app.session_alive = true;
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    // One it did not list goes too, as typed: Claude passes an unknown
    // one to the model.
    app.handle_slash_command("/etc/hosts is it there?");
    assert_eq!(
        app.take_actions(),
        vec![Action::turn("/etc/hosts is it there?")]
    );
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .contains("passed to Claude as typed")
    );
}

#[test]
fn unharness_s_own_command_wins_unless_escaped() {
    let mut app = app_with_claude_commands();
    app.submit_prompt("before".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.handle_slash_command("/clear");
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert_eq!(app.transcript.blocks.len(), 1, "a new conversation");
    app.session_alive = true;
    app.pass_command("/clear", true);
    assert_eq!(app.take_actions(), vec![Action::turn("/clear")]);
    let said = notices(&app);
    assert_eq!(said[0], "/clear is Claude's own command: passed on");
    assert!(said[1].starts_with("unharness does not see what Claude's /clear changes"));
    assert!(said[1].ends_with("(unharness's /clear starts a new conversation)"));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    // `/new` is Claude's `/clear` under another name.
    app.handle_slash_command("/new");
    assert_eq!(app.take_actions(), vec![Action::turn("/new")]);
    let said = notices(&app);
    assert_eq!(said[2], "/new is Claude's own command: passed on");
    assert!(said[3].starts_with("unharness does not see what Claude's /new changes"));
    assert!(said[3].ends_with("(unharness's /clear starts a new conversation)"));
}

/// A `/` prompt queued for one harness is not sent to another, where
/// it could be a command or not one.
#[test]
fn a_slash_prompt_queued_for_one_harness_comes_back_on_another() {
    let mut app = test_app(HarnessId::CODEX);
    app.submit_prompt("go".into());
    app.take_actions();
    app.session_alive = true;
    // Text, on Codex.
    app.pass_command("/review the diff", true);
    app.queue_prompt("plain".into());
    assert_eq!(app.queued.len(), 2);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    app.switch_harness(HarnessId::CLAUDE);
    app.take_actions();
    app.session_alive = false;
    app.set_input("half typed");
    assert!(app.send_next_queued());
    assert!(sent_turn(&mut app).ends_with("\nplain"));
    assert_eq!(app.input, "/review the diff\nhalf typed");
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .starts_with("'/review the diff' was typed for Codex")
    );
}

#[test]
fn a_harness_that_runs_no_commands_is_not_sent_one() {
    let mut app = test_app(HarnessId::CODEX);
    app.handle_slash_command("/review");
    assert!(app.take_actions().is_empty());
    assert!(notices(&app)[0].starts_with("unknown command '/review'"));
    assert!(notices(&app)[0].contains("\\/review sends it as text"));
    // Escaped, it is a prompt that starts with `/`.
    app.pass_command("/review", true);
    assert_eq!(
        app.take_actions(),
        vec![
            Action::StartSession { resume: None },
            Action::turn("/review")
        ]
    );
    assert_eq!(notices(&app).len(), 1);
}

#[test]
fn the_list_offers_the_harness_s_commands() {
    let mut app = app_with_claude_commands();
    app.set_input("");
    app.insert_char('/');
    let shown: Vec<&str> = app.suggestions.iter().map(|(c, _)| c.as_str()).collect();
    assert!(shown.contains(&"/clear") && shown.contains(&"/hello"));
    // Its `/clear` gives way to unharness's.
    assert_eq!(shown.iter().filter(|c| **c == "/clear").count(), 1);
    for c in "he".chars() {
        app.insert_char(c);
    }
    assert_eq!(app.suggestions.len(), 2, "{:?}", app.suggestions);
    let hello = app.suggestions.iter().find(|(c, _)| c == "/hello").unwrap();
    assert_eq!(hello.1, "Claude: <name> · hello it");
    // Escaped, its own come up, unharness's do not.
    app.set_input("");
    for c in "\\/cl".chars() {
        app.insert_char(c);
    }
    assert_eq!(
        app.suggestions,
        vec![("\\/clear".to_string(), "Claude: clear it".to_string())]
    );
    assert!(app.should_accept_suggestion());
    app.accept_suggestion();
    assert_eq!(app.input, "\\/clear");

    // The list matches regardless of case, and so does Enter.
    app.set_input("");
    for c in "/dep".chars() {
        app.insert_char(c);
    }
    assert_eq!(app.suggestions[0].0, "/Deploy");
    assert!(app.should_accept_suggestion());
    app.accept_suggestion();
    assert_eq!(app.input, "/Deploy");

    // Codex runs none, so none are offered.
    let mut codex = test_app(HarnessId::CODEX);
    codex.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        commands: Some(vec![HarnessCommand::new("hello", None, None).unwrap()]),
        ..Default::default()
    }));
    codex.insert_char('/');
    assert!(codex.suggestions.iter().all(|(c, _)| c != "/hello"));
}

/// A command has to start the message: the context of a switch waits
/// for the next prompt instead of going in front of it.
#[test]
fn a_command_goes_alone_and_the_bridge_waits() {
    let mut app = test_app(HarnessId::CODEX);
    app.submit_prompt("first question".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::TextDelta("first answer".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.switch_harness(HarnessId::CLAUDE);
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.session_alive = false;

    app.handle_slash_command("/hello world");
    assert_eq!(sent_turn(&mut app), "/hello world");
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .starts_with("Claude is told what it has not seen yet")
    );
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    app.submit_prompt("next".into());
    let next = sent_turn(&mut app);
    assert!(next.contains("first answer"), "{next}");
    assert!(next.ends_with("next"), "{next}");
}

#[test]
fn commands_that_arrive_while_a_slash_is_typed_are_listed() {
    let command = |name: &str| HarnessCommand::new(name, None, None).unwrap();
    let listed = |commands| {
        AgentEvent::CapabilitiesChanged(CapsUpdate {
            commands: Some(commands),
            ..Default::default()
        })
    };
    let mut app = test_app(HarnessId::CLAUDE);
    app.input = "/he".into();
    app.update_suggestions();
    assert_eq!(app.suggestions.len(), 1, "{:?}", app.suggestions);
    app.selected_suggestion = 0;
    let selected = app.suggestions[0].clone();
    app.on_event(listed(vec![command("hello"), command("hey")]));
    let names: Vec<&str> = app.suggestions.iter().map(|(c, _)| c.as_str()).collect();
    assert!(
        names.contains(&"/hello") && names.contains(&"/hey"),
        "{names:?}"
    );
    assert_eq!(app.suggestions[app.selected_suggestion], selected);

    // Not after Esc closed it.
    app.close_suggestions();
    app.on_event(listed(vec![command("hello")]));
    assert!(app.suggestions.is_empty());
    // Typing opens it again.
    app.input = "/hel".into();
    app.update_suggestions();
    assert!(app.suggestions.iter().any(|(c, _)| c == "/hello"));
}
