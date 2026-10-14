use super::*;
use crate::tui::modal::AlwaysDraft;

#[test]
fn an_allow_rule_answers_the_request_of_any_harness_without_asking() {
    use crate::core::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.rules
        .append(Scope::Workspace, &[Rule::shell("cargo test")])
        .unwrap();
    for ev in cargo_test_requests("cargo test --all") {
        app.on_event(ev);
    }
    assert!(app.modal.is_none());
    assert_eq!(
        app.take_actions(),
        vec![allowed("claude"), allowed("codex"), allowed("acp")]
    );
    let said = app
        .transcript
        .blocks
        .iter()
        .filter(|b| matches!(b, Block::Notice(n) if n.contains("`cargo test`")))
        .count();
    assert_eq!(said, 3);

    // Several commands, several rules: all of them are named.
    app.rules
        .append(Scope::Workspace, &[Rule::shell("git push")])
        .unwrap();
    app.on_event(cargo_test_requests("cargo test && git push").remove(0));
    assert_eq!(app.take_actions(), vec![allowed("claude")]);
    assert!(
        app.transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Notice(n)
        if n.contains("`cargo test`, and for shell commands starting with `git push`")))
    );

    // More than the rule covers is still asked about, one at a time.
    for ev in cargo_test_requests("cargo test && rm -rf x") {
        app.on_event(ev);
    }
    assert_eq!(app.take_actions(), vec![]);
    assert_eq!(
        app.modal.as_ref().and_then(Modal::request_id),
        Some("claude")
    );
    assert_eq!(app.pending_prompts.len(), 2);

    // Run in the workspace it is the rule's command, elsewhere it is not.
    let mut app = test_app(HarnessId::CLAUDE);
    app.rules
        .append(Scope::Workspace, &[Rule::shell("cargo test")])
        .unwrap();
    let cwd = app.cwd.clone();
    for ev in requests_in("cargo test", Some(&cwd)).split_off(1) {
        app.on_event(ev);
    }
    assert_eq!(app.take_actions(), vec![allowed("codex"), allowed("acp")]);
    for ev in requests_in("cargo test", Some(Path::new("/tmp"))).split_off(1) {
        app.on_event(ev);
    }
    assert_eq!(app.take_actions(), vec![]);
    assert_eq!(
        app.modal.as_ref().and_then(Modal::request_id),
        Some("codex")
    );

    // A subagent's request is answered the same way.
    let mut app = test_app(HarnessId::CLAUDE);
    app.rules
        .append(Scope::Global, &[Rule::shell("cargo test")])
        .unwrap();
    let ev = cargo_test_requests("cargo test").remove(0);
    app.on_event(AgentEvent::Sub {
        parent: "agent-1".into(),
        event: Box::new(ev),
    });
    assert_eq!(app.take_actions(), vec![allowed("claude")]);
}

/// While a handoff summary is written nothing runs, not even what a rule
/// covers, and the agent is told why; a tool call refused is noted. Once
/// it is written the rule answers again, and says so.
#[test]
fn a_handoff_answers_requests_itself_and_notes_a_tool_call() {
    use crate::core::transcript::Block;
    let notices = |app: &App| -> Vec<String> {
        app.transcript
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Notice(n) => Some(n.clone()),
                _ => None,
            })
            .collect()
    };
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.rules
        .append(Scope::Workspace, &[Rule::shell("cargo test")])
        .unwrap();
    for (renew, moving) in [
        (false, "The user is switching to another agent."),
        (
            true,
            "The user is moving this conversation to a new session.",
        ),
    ] {
        app.handoff = Some(Handoff {
            to: HarnessId::CODEX,
            start: 0,
            held: false,
            renew,
            blind: false,
        });
        let before = notices(&app).len();
        app.on_event(cargo_test_requests("cargo test").remove(0));
        app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
            id: "confirm".into(),
            kind: PermissionKind::Confirm {
                title: "Go on?".into(),
                message: None,
            },
            tool_call_id: None,
        }));
        assert!(app.modal.is_none());
        assert_eq!(
            app.take_actions(),
            vec![
                Action::Command(SessionCommand::RespondPermission {
                    id: "claude".into(),
                    decision: PermissionDecision::Deny {
                        reason: format!(
                            "{moving} Write the handoff summary from what you already know, without tools."
                        ),
                    },
                }),
                Action::Command(SessionCommand::RespondPermission {
                    id: "confirm".into(),
                    decision: PermissionDecision::Answer(serde_json::Value::Null),
                }),
            ]
        );
        assert_eq!(
            notices(&app)[before..],
            ["denied Bash: a handoff summary uses no tools"]
        );
    }

    app.handoff = None;
    app.on_event(cargo_test_requests("cargo test").remove(0));
    assert_eq!(app.take_actions(), vec![allowed("claude")]);
    assert_eq!(
        notices(&app).last().map(String::as_str),
        Some("allowed by your rule for shell commands starting with `cargo test`")
    );
}

fn type_keys(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_modal_key(key(KeyCode::Char(c)));
    }
}

fn draft(app: &App) -> &AlwaysDraft {
    match &app.modal {
        Some(Modal::Permission(m)) => m.always.as_ref().expect("the always step"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn allow_always_writes_a_rule_that_the_next_harness_knows() {
    let mut requests = cargo_test_requests("cargo test --all");
    let (acp, codex, claude) = (
        requests.pop().unwrap(),
        requests.pop().unwrap(),
        requests.pop().unwrap(),
    );
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.on_event(claude);
    // A second request for the same thing is already waiting.
    app.on_event(acp);

    // `a` shows what would be allowed and sends nothing yet.
    app.handle_modal_key(key(KeyCode::Char('a')));
    assert_eq!(app.take_actions(), vec![]);
    assert_eq!(draft(&app).rules(), [Rule::shell("cargo test")]);
    assert_eq!(draft(&app).scope, Scope::Workspace);
    // Esc goes back to the request, not out of it.
    app.handle_modal_key(key(KeyCode::Esc));
    assert_eq!(app.take_actions(), vec![]);
    assert!(matches!(&app.modal, Some(Modal::Permission(m)) if m.always.is_none()));

    app.handle_modal_key(key(KeyCode::Char('a')));
    app.handle_modal_key(key(KeyCode::Enter));
    // Claude is told "allow" and nothing more, and the request that was
    // waiting is answered by the new rule.
    assert_eq!(app.take_actions(), vec![allowed("claude"), allowed("acp")]);
    assert!(app.modal.is_none());
    let path = app.rules.path(Scope::Workspace).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("command = \"cargo test\""), "{written}");
    assert!(!app.rules.path(Scope::Global).unwrap().exists());

    // After a switch, Codex asks for the same thing and is not asked about.
    app.on_event(codex);
    assert_eq!(app.take_actions(), vec![allowed("codex")]);
    assert!(app.modal.is_none());

    // The rule is there for the next run.
    let reloaded = Rules::load_in(path.parent().unwrap().parent().unwrap(), Some(&app.cwd));
    assert_eq!(
        reloaded.unwrap().iter().collect::<Vec<_>>(),
        [(Scope::Workspace, &Rule::shell("cargo test"))]
    );
}

#[test]
fn the_rule_can_be_changed_and_moved_before_it_is_kept() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.on_event(cargo_test_requests("cargo test --all").remove(0));
    app.handle_modal_key(key(KeyCode::Char('a')));

    // Widened to every cargo command, for every workspace.
    for _ in 0.." test".len() {
        app.handle_modal_key(key(KeyCode::Backspace));
    }
    app.handle_modal_key(key(KeyCode::Tab));
    assert_eq!(draft(&app).rules(), [Rule::shell("cargo")]);
    assert_eq!(draft(&app).scope, Scope::Global);

    // Ctrl+C is not a letter of the pattern.
    app.handle_modal_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(draft(&app).rules(), [Rule::shell("cargo")]);

    // A pattern that would not have allowed this request is refused.
    type_keys(&mut app, " build");
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(app.take_actions(), vec![]);
    assert!(
        draft(&app)
            .problem
            .as_deref()
            .unwrap()
            .contains("not cover")
    );
    // So is one that is no rule at all.
    type_keys(&mut app, " && ls");
    assert!(draft(&app).problem.is_none());
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(app.take_actions(), vec![]);
    assert!(draft(&app).problem.is_some());

    for _ in 0.." build && ls".len() {
        app.handle_modal_key(key(KeyCode::Backspace));
    }
    // A paste goes into the pattern too.
    app.paste(" test");
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(app.take_actions(), vec![allowed("claude")]);
    assert_eq!(
        app.rules.iter().collect::<Vec<_>>(),
        [(Scope::Global, &Rule::shell("cargo test"))]
    );
    assert!(!app.rules.path(Scope::Workspace).unwrap().exists());
}

#[test]
fn a_request_behind_a_picker_opens_when_the_picker_closes() {
    let mut app = test_app(HarnessId::PI);
    app.session_alive = true;
    app.start_generation();
    for close in [KeyCode::Esc, KeyCode::Enter] {
        app.open_policy_picker();
        app.on_event(pi_gate_request("g1", "cq query"));
        // It waits, and the user is told so.
        assert!(matches!(app.modal, Some(Modal::Policy(_))));
        assert_eq!(app.status_label(), "Waiting for you");
        app.handle_modal_key(key(close));
        assert_eq!(app.modal.as_ref().and_then(Modal::request_id), Some("g1"));
        app.handle_modal_key(key(KeyCode::Char('y')));
        // After the policy Enter chose, if any.
        assert!(app.take_actions().ends_with(&[allowed("g1")]));
        assert!(!app.awaiting_answer());
    }
}

#[test]
fn a_request_of_a_session_the_picker_ends_goes_with_it() {
    let mut app = test_app(HarnessId::PI);
    app.session_alive = true;
    app.open_sandbox_picker();
    // Between turns: an extension's dialog, say.
    app.on_event(pi_gate_request("g1", "ls"));
    if let Some(Modal::Sandbox(p)) = &mut app.modal {
        p.selected = p
            .items
            .iter()
            .position(|l| *l == SandboxLevel::Off)
            .unwrap();
    }
    app.handle_modal_key(key(KeyCode::Enter));
    // The session restarts under the new level; its request is not
    // put to the user, since nothing could take the answer.
    assert_eq!(app.take_actions(), vec![Action::Shutdown]);
    assert!(app.modal.is_none() && !app.awaiting_answer());
}

#[test]
fn a_withdrawn_request_closes_unanswered() {
    use crate::core::transcript::Block;
    let mut app = test_app(HarnessId::PI);
    app.session_alive = true;
    app.start_generation();
    app.on_event(pi_gate_request("g1", "echo one"));
    app.on_event(pi_gate_request("g2", "echo two"));
    app.on_event(pi_gate_request("g3", "echo three"));
    // One waiting behind the open one goes quietly.
    app.on_event(AgentEvent::PermissionWithdrawn { id: "g2".into() });
    assert_eq!(app.modal.as_ref().and_then(Modal::request_id), Some("g1"));
    // The open one closes, and the next comes up.
    app.on_event(AgentEvent::PermissionWithdrawn { id: "g1".into() });
    assert_eq!(app.modal.as_ref().and_then(Modal::request_id), Some("g3"));
    app.on_event(AgentEvent::PermissionWithdrawn { id: "g3".into() });
    assert!(app.modal.is_none() && !app.awaiting_answer());
    // Nothing was answered for the user.
    assert_eq!(app.take_actions(), vec![]);
    let said = app
        .transcript
        .blocks
        .iter()
        .filter(|b| matches!(b, Block::Notice(n) if n.contains("stopped waiting")))
        .count();
    assert_eq!(said, 2);
}

#[test]
fn a_request_no_rule_can_cover_says_so_and_stays_a_question() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.on_event(cargo_test_requests("cargo test > log").remove(0));
    app.handle_modal_key(key(KeyCode::Char('a')));
    assert_eq!(draft(&app).rules(), []);
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(app.take_actions(), vec![]);
    app.handle_modal_key(key(KeyCode::Esc));
    app.handle_modal_key(key(KeyCode::Char('y')));
    assert_eq!(app.take_actions(), vec![allowed("claude")]);
    assert_eq!(app.rules.iter().count(), 0);
}

#[test]
fn a_rule_that_cannot_be_saved_still_allows_this_once() {
    use crate::core::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    // Something that is not a rules file is in the way.
    let path = app.rules.path(Scope::Workspace).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not toml [").unwrap();
    app.on_event(cargo_test_requests("cargo test").remove(0));
    app.handle_modal_key(key(KeyCode::Char('a')));
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(app.take_actions(), vec![allowed("claude")]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not toml [");
    assert!(
        app.transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Error(e) if e.contains("could not be saved")))
    );
    app.on_event(cargo_test_requests("cargo test").remove(0));
    assert!(app.modal.is_some());
}

#[test]
fn allow_lists_the_rules_and_where_they_are_kept() {
    use crate::core::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    app.handle_slash_command("/allow");
    app.rules
        .append(Scope::Workspace, &[Rule::edit("src/**")])
        .unwrap();
    app.rules
        .append(Scope::Global, &[Rule::mcp("probe/*")])
        .unwrap();
    app.handle_slash_command("/allow");
    let said: Vec<&str> = app
        .transcript
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::System(text) if text.contains("without asking") => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(said[0].contains("Nothing is allowed without asking yet"));
    assert!(said[0].contains("allow.toml"));
    assert!(
        said[1].contains("• [workspace] edits to `src/**`"),
        "{}",
        said[1]
    );
    assert!(said[1].contains("• [global] every tool of the MCP server `probe`"));
    assert!(said[1].contains(".allow.toml"));
}

#[test]
fn permission_modal_allow_and_deny_with_reason() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    let req = |id: &str| PermissionRequest {
        id: id.into(),
        kind: PermissionKind::ToolUse {
            tool: "Bash".into(),
            input: serde_json::json!({"command":"ls"}),
            action: crate::core::ToolAction::Opaque,
            description: None,
        },
        tool_call_id: None,
    };
    app.on_event(AgentEvent::PermissionRequest(req("r1")));
    app.on_event(AgentEvent::PermissionRequest(req("r2")));
    assert!(matches!(app.modal, Some(Modal::Permission(_))));

    app.handle_modal_key(key(KeyCode::Char('y')));
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::RespondPermission {
            id: "r1".into(),
            decision: PermissionDecision::Allow {
                updated_input: None
            }
        })]
    );
    assert_eq!(app.modal.as_ref().and_then(Modal::request_id), Some("r2"));
    app.handle_modal_key(key(KeyCode::Char('n')));
    for c in "nope".chars() {
        app.handle_modal_key(key(KeyCode::Char(c)));
    }
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::RespondPermission {
            id: "r2".into(),
            decision: PermissionDecision::Deny {
                reason: "nope".into()
            }
        })]
    );
    assert!(app.modal.is_none());
}
