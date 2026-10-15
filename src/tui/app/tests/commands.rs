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
    app.controller.sandbox = SandboxSetup::null(Some(SandboxLevel::Off));
    for c in "/sandbox r".chars() {
        app.insert_char(c);
    }
    assert_eq!(app.suggestions[0].0, "/sandbox read-only");
    app.handle_slash_command("/sandbox bogus");
    assert_eq!(app.controller.sandbox.explicit, Some(SandboxLevel::Off));
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
    assert_eq!(app.controller.active, HarnessId::CODEX);
    app.handle_slash_command("/quit");
    assert!(app.controller.should_quit);
}

/// Claude, its session having listed `/hello`, `/clear` and `/model`.
fn app_with_claude_commands() -> TestApp {
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
    app.controller.session_alive = true;
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
    app.controller.session_alive = true;
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.handle_slash_command("/clear");
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert_eq!(
        app.controller.conversation.transcript.blocks.len(),
        1,
        "a new conversation"
    );
    app.controller.session_alive = true;
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
    app.controller.session_alive = true;
    // Text, on Codex.
    app.pass_command("/review the diff", true);
    app.queue_prompt("plain".into());
    assert_eq!(app.controller.queued.len(), 2);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Interrupted,
    });
    app.switch_harness(HarnessId::CLAUDE);
    app.take_actions();
    app.controller.session_alive = false;
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
    app.controller.session_alive = true;
    app.on_event(AgentEvent::TextDelta("first answer".into()));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.switch_harness(HarnessId::CLAUDE);
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    app.controller.session_alive = false;

    app.handle_slash_command("/hello world");
    assert_eq!(sent_turn(&mut app), "/hello world");
    assert!(
        notices(&app)
            .last()
            .unwrap()
            .starts_with("Claude is told what it has not seen yet")
    );
    app.controller.session_alive = true;
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

/// What `/usage` says last.
fn usage_text(app: &mut TestApp) -> String {
    use super::transcript::Block;
    app.handle_slash_command("/usage");
    match app.controller.conversation.transcript.blocks.last() {
        Some(Block::System(s)) => s.clone(),
        other => panic!("no /usage answer: {other:?}"),
    }
}

/// `/usage` calls the status row's `session N` billed tokens and breaks
/// it down, for the last turn and the session, with the cost (#314).
#[test]
fn usage_breaks_billed_tokens_down() {
    use crate::core::{RateLimitInfo, RateLimitWindow};
    let mut app = test_app(HarnessId::CLAUDE);
    let turn = |input, output, cache_read, cache_write, cost| {
        AgentEvent::Usage(Usage {
            input,
            output,
            cache_read,
            cache_write,
            cost_usd: Some(cost),
            ..Default::default()
        })
    };
    app.on_event(turn(808, 375_351, 157_661_160, 4_177_208, 41.25));
    app.on_event(turn(18, 249, 41_805, 5_199, 0.0158));
    app.on_event(AgentEvent::Context(ContextUsage {
        used: Some(47_022),
        window: Some(200_000),
    }));
    app.on_event(AgentEvent::RateLimit(RateLimitInfo {
        status: Some("allowed".into()),
        windows: vec![RateLimitWindow {
            label: "five_hour".into(),
            used_percent: Some(12.0),
            resets_at: None,
        }],
    }));
    assert_eq!(
        usage_text(&mut app),
        "Billed tokens, last turn: 47,271 (uncached input 18, output 249, \
         cache reads 41,805, cache writes 5,199), $0.0158\n\
         Billed tokens, session: 162,261,798 (uncached input 826, output 375,600, \
         cache reads 157,702,965, cache writes 4,182,407), $41.2658\n\
         Context: 47,022 of 200,000 tokens (23%)\n\
         Rate limits: five_hour 12% used"
    );
}

/// Context fill the harness never reports is said to be missing; one it
/// has not reported yet is not known yet, as the status row tells them
/// apart (#313).
#[test]
fn usage_tells_context_never_reported_from_not_known_yet() {
    let mut app = test_app(HarnessId::AGY);
    assert!(!app.caps().context_usage);
    assert_eq!(
        usage_text(&mut app),
        "Billed tokens, last turn: 0 (uncached input 0, output 0, cache reads 0, cache writes 0)\n\
         Billed tokens, session: 0 (uncached input 0, output 0, cache reads 0, cache writes 0)\n\
         Context: not reported by Antigravity (agy)"
    );

    let mut app = test_app(HarnessId::CLAUDE);
    assert!(app.caps().context_usage);
    assert!(usage_text(&mut app).ends_with("\nContext: not known yet"));
    // The window a model last reported, before the session says how full.
    app.controller.conversation.context.window = Some(200_000);
    assert!(usage_text(&mut app).ends_with("\nContext: not known yet, of a 200,000-token window"));
    app.controller.conversation.context.window = None;
    app.on_event(AgentEvent::Context(ContextUsage {
        used: Some(1_500),
        window: None,
    }));
    assert!(usage_text(&mut app).ends_with("\nContext: 1,500 tokens, of a window not known yet"));
}

#[test]
fn grouped_puts_digits_in_threes() {
    use super::super::commands::grouped;
    assert_eq!(grouped(0), "0");
    assert_eq!(grouped(999), "999");
    assert_eq!(grouped(1_000), "1,000");
    assert_eq!(grouped(100_000), "100,000");
    assert_eq!(grouped(u64::MAX), "18,446,744,073,709,551,615");
}
