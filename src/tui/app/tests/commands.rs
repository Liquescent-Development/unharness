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
            HarnessCommand {
                aliases: vec!["cost".into(), "stats".into()],
                ..command("usage", None)
            },
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
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });

    // Claude's `/usage` under another name only shows something, as
    // unharness's does: nothing to warn about.
    app.handle_slash_command("/cost");
    assert_eq!(app.take_actions(), vec![Action::turn("/cost")]);
    let said = notices(&app);
    assert_eq!(said[4..], ["/cost is Claude's own command: passed on"]);
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

/// The rows of a section of what `/status` shows now, one per line.
fn status_section(app: &TestApp, title: &str) -> String {
    let report = app.status_report(std::time::SystemTime::now());
    let section = report
        .sections
        .iter()
        .find(|s| s.title == title)
        .unwrap_or_else(|| panic!("no {title} in /status"));
    section
        .rows
        .iter()
        .map(|r| format!("{}: {}", r.label, r.value))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `/status` and `/usage` open the overlay and leave the transcript as
/// it was.
#[test]
fn status_opens_an_overlay_not_a_block() {
    let mut app = test_app(HarnessId::CLAUDE);
    let blocks = app.controller.conversation.transcript.blocks.len();
    for cmd in ["/status", "/usage"] {
        app.handle_slash_command(cmd);
        assert!(matches!(app.modal, Some(Modal::Status(_))), "{cmd}");
        app.modal = None;
    }
    assert_eq!(app.controller.conversation.transcript.blocks.len(), blocks);
    let titles: Vec<&str> = app
        .status_report(std::time::SystemTime::now())
        .sections
        .iter()
        .map(|s| s.title)
        .collect();
    assert_eq!(
        titles,
        ["Session", "Account", "Usage", "Context", "Rate limits"]
    );
}

/// `/status` calls the status row's `session N` billed tokens and breaks
/// it down, for the last turn and the session, with the cost (#314).
#[test]
fn status_breaks_billed_tokens_down() {
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
        ..Default::default()
    }));
    assert_eq!(
        status_section(&app, "Usage"),
        "Last turn: 47,271 (uncached input 18, output 249, \
         cache reads 41,805, cache writes 5,199), $0.0158\n\
         Session: 162,261,798 (uncached input 826, output 375,600, \
         cache reads 157,702,965, cache writes 4,182,407), $41.2658"
    );
    assert_eq!(
        status_section(&app, "Context"),
        "Filled: 47,022 of 200,000 tokens (23%)"
    );
    assert_eq!(
        status_section(&app, "Rate limits"),
        "Status: allowed\n5-hour: █░░░░░░░░░  12% used\n: as of 0s ago"
    );
}

/// Context fill the harness never reports is said to be missing; one it
/// has not reported yet is not known yet, as the status row tells them
/// apart (#313). So are rate limits.
#[test]
fn status_tells_never_reported_from_not_known_yet() {
    let app = test_app(HarnessId::AGY);
    assert!(!app.caps().context_usage);
    assert_eq!(
        status_section(&app, "Context"),
        ": not reported by Antigravity (agy)"
    );
    // Asked of a run of its CLI apart, on `/status` and `r`.
    assert_eq!(
        status_section(&app, "Rate limits"),
        ": none yet: r asks Antigravity (agy)"
    );
    let app = test_app(HarnessId::PI);
    assert_eq!(status_section(&app, "Rate limits"), ": not reported by pi");

    let mut app = test_app(HarnessId::CLAUDE);
    assert!(app.caps().context_usage);
    assert_eq!(status_section(&app, "Context"), "Filled: not known yet");
    assert_eq!(
        status_section(&app, "Rate limits"),
        ": none yet: Claude Code (claude) reports them with a turn"
    );
    // The window a model last reported, before the session says how full.
    app.controller.conversation.context.window = Some(200_000);
    assert_eq!(
        status_section(&app, "Context"),
        "Filled: not known yet, of a 200,000-token window"
    );
    app.controller.conversation.context.window = None;
    app.on_event(AgentEvent::Context(ContextUsage {
        used: Some(1_500),
        window: None,
    }));
    assert_eq!(
        status_section(&app, "Context"),
        "Filled: 1,500 tokens, of a window not known yet"
    );
}

/// The account a harness reported is shown, and only for that harness.
#[test]
fn status_shows_the_account_of_the_active_harness() {
    use crate::core::{AccountInfo, Credits};
    let mut app = test_app(HarnessId::CODEX);
    app.on_event(AgentEvent::Account(AccountInfo {
        plan: Some("prolite".into()),
        auth: Some("chatgpt".into()),
        credits: Some(Credits {
            has_credits: true,
            unlimited: false,
            balance: Some("62500.0000000000".into()),
        }),
        ..Default::default()
    }));
    assert_eq!(
        status_section(&app, "Account"),
        "Plan: prolite\nSigned in with: chatgpt\nCredits: 62500"
    );
    // Switched to directly: `/switch` refuses a harness not installed,
    // and CI has none.
    app.switch_harness(HarnessId::CLAUDE);
    assert_eq!(app.controller.active, HarnessId::CLAUDE);
    assert!(!status_section(&app, "Account").contains("prolite"));
}

/// agy's limits come from a run of `agy --print=/usage` apart, which
/// `/status` queues, with or without a session.
#[test]
fn status_asks_agy_apart_and_shows_its_answer() {
    use crate::core::{RateLimitInfo, RateLimitWindow};
    use crate::tui::app::{ListRequest, ListResult};
    let mut app = test_app(HarnessId::AGY);
    // Any binary will do: the job is not run here (CI has no agy).
    app.controller
        .config
        .harnesses
        .entry("agy".into())
        .or_default()
        .binary = Some("/bin/sh".into());
    app.take_list_jobs();
    app.handle_slash_command("/status");
    let jobs = app.take_list_jobs();
    assert_eq!(
        jobs.iter().map(|j| j.request.clone()).collect::<Vec<_>>(),
        [ListRequest::Status(HarnessId::AGY)]
    );
    assert!(status_section(&app, "Rate limits").contains("asking"));
    app.on_list(
        ListRequest::Status(HarnessId::AGY),
        ListResult::Status(Ok(vec![AgentEvent::RateLimit(RateLimitInfo {
            windows: vec![RateLimitWindow {
                label: "Gemini weekly".into(),
                used_percent: Some(12.0),
                resets_at: None,
            }],
            ..Default::default()
        })])),
    );
    assert!(
        status_section(&app, "Rate limits").contains("Gemini weekly: █░░░░░░░░░  12% used"),
        "{}",
        status_section(&app, "Rate limits")
    );
}

/// pi's own count of its session shows beside unharness's, while that
/// session runs.
#[test]
fn status_shows_pi_s_own_count_of_its_session() {
    use crate::core::SessionStats;
    let mut app = test_app(HarnessId::PI);
    app.on_event(AgentEvent::SessionStarted {
        session_id: "s1".into(),
        model: None,
    });
    app.on_event(AgentEvent::SessionStats(SessionStats {
        user_messages: Some(2),
        assistant_messages: Some(2),
        tool_calls: Some(1),
        tokens: Some(10_236),
        cost_usd: Some(0.0282),
    }));
    let usage = status_section(&app, "Usage");
    assert!(
        usage.contains(
            "pi's count: 10,236 tokens, $0.0282 · 2 prompts, 2 replies, 1 tool call\n: its whole session"
        ),
        "{usage}"
    );
    // A count of nothing (a turn that failed) is no row.
    app.on_event(AgentEvent::SessionStats(SessionStats {
        tokens: Some(0),
        user_messages: Some(0),
        ..Default::default()
    }));
    assert!(!status_section(&app, "Usage").contains("pi's count"));
    // Another session's count is not this one's.
    app.on_event(AgentEvent::SessionStarted {
        session_id: "s2".into(),
        model: None,
    });
    assert!(!status_section(&app, "Usage").contains("pi's count"));
}

/// Without an account reported, what the CLI said at startup.
#[test]
fn status_falls_back_to_the_sign_in_found_at_startup() {
    let mut app = test_app(HarnessId::CLAUDE);
    for o in &mut app.harness_options {
        o.auth = crate::harness::AuthInfo {
            authenticated: true,
            details: Some("plan: Claude Max".into()),
        };
    }
    assert_eq!(
        status_section(&app, "Account"),
        "Signed in: yes, at startup\n: plan: Claude Max"
    );
}
