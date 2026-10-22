use super::*;
use crate::config::Config;
use crate::core::controller::GrantScope;
use crate::core::sandbox::{Access, Sandbox, SandboxEnv};

/// The sandbox is unharness's, so the level is for the run, and a
/// live session comes back under it with the next prompt.
#[test]
fn the_sandbox_level_is_chosen_for_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.controller.sandbox = SandboxSetup::null(None);
    assert_eq!(app.sandbox_level().0, SandboxLevel::WorkspaceWrite);

    app.submit_prompt("hello".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    app.controller.session_sandbox_level = Some(SandboxLevel::WorkspaceWrite);
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
    assert!(app.controller.session_alive);

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
    app.controller.sandbox = SandboxSetup {
        explicit: None,
        backend: Err("no kernel".into()),
        grants: Vec::new(),
    };
    assert!(app.status_warning().unwrap().contains("(/sandbox)"));
    assert!(!app.set_sandbox(SandboxLevel::ReadOnly));
    assert_eq!(app.controller.sandbox.explicit, None);

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
    assert_eq!(app.controller.sandbox.explicit, Some(SandboxLevel::Off));
    assert!(app.status_warning().is_none());
}

/// A sandboxed app in a workspace of its own, with a directory outside
/// it to grant.
fn granting_app() -> (tempfile::TempDir, App, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let ws = root.join("ws");
    let data = root.join("data");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let mut app = test_app_in(ws, HarnessId::CLAUDE, None, false);
    app.controller.sandbox = SandboxSetup::null(None);
    // The test's temp directory is not scratch, as /tmp is on the machine.
    crate::core::sandbox::TEST_ENV.with(|e| {
        *e.borrow_mut() = Some(SandboxEnv {
            home: Some(root.join("home")),
            scratch: Vec::new(),
            protected: vec![root.join("state")],
        })
    });
    (tmp, app, data)
}

fn writable(app: &App) -> Vec<PathBuf> {
    match app.session_sandbox().unwrap() {
        Sandbox::Active { profile, .. } => profile.writable,
        Sandbox::Off { .. } => panic!("off"),
    }
}

/// `/sandbox allow` grants a path for the run: the session restarts
/// under it now, or when the turn running ends.
#[test]
fn a_path_is_granted_for_the_run() {
    let (_tmp, mut app, data) = granting_app();
    assert!(!writable(&app).contains(&data));

    app.submit_prompt("hello".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });

    // During the turn: kept, and the restart waits for the turn's end.
    app.handle_slash_command(&format!("/sandbox allow {}", data.display()));
    let Some(Modal::SandboxGrant(d)) = &app.modal else {
        panic!("no dialog");
    };
    assert_eq!(d.path, data.display().to_string());
    assert_eq!(d.said, None);
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert!(writable(&app).contains(&data));
    assert!(app.take_actions().is_empty());
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert_eq!(app.take_actions(), [Action::Shutdown]);
    assert!(!app.controller.session_alive);

    // Asked again, it is said to be there already.
    assert!(app.grant_sandbox(&data, Access::Write, GrantScope::Run, false));
    assert_eq!(app.controller.sandbox.grants.len(), 1);
    // It holds across harnesses.
    app.switch_harness(HarnessId::CODEX);
    assert!(writable(&app).contains(&data));
}

#[test]
fn an_idle_session_restarts_under_a_grant() {
    let (_tmp, mut app, data) = granting_app();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    // A file in a directory not there yet: the directory is offered, and
    // made when it is granted.
    app.open_sandbox_grant(
        &format!("{}/new/deeper/file", data.display()),
        Access::Write,
    );
    let Some(Modal::SandboxGrant(d)) = &app.modal else {
        panic!("no dialog");
    };
    assert_eq!(d.path, data.join("new").display().to_string());
    assert!(d.make());
    assert!(
        d.notes[0].text.contains("Allowing creates it"),
        "{:?}",
        d.notes
    );
    app.handle_modal_key(key(KeyCode::Enter));
    assert_eq!(app.take_actions(), [Action::Shutdown]);
    assert!(data.join("new").is_dir());
    assert!(writable(&app).contains(&data.join("new")));
}

/// A tool call the sandbox refused is told during the turn, and offered
/// when the turn ends; granted with `r`, the session restarts under it
/// and the agent is asked to try again. A path is offered once.
#[test]
fn a_write_the_sandbox_refused_is_offered_when_the_turn_ends() {
    let (_tmp, mut app, data) = granting_app();
    app.submit_prompt("write it".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    let target = data.join("out.txt");
    for id in ["t1", "t2"] {
        app.on_event(AgentEvent::ToolCallStarted {
            id: id.into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": format!("touch {}", target.display())}),
        });
        app.on_event(AgentEvent::ToolCallResult {
            id: id.into(),
            output: format!(
                "Exit code 1\ntouch: cannot touch '{}': Permission denied",
                target.display()
            ),
            is_error: true,
        });
    }
    // Something writable already, or that no sandbox kept it from, is
    // not offered.
    app.on_event(AgentEvent::ToolCallResult {
        id: "t3".into(),
        output: "zsh:1: permission denied: /proc/1/mem".into(),
        is_error: true,
    });
    let told: Vec<_> = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .filter(|b| format!("{b:?}").contains("tried to write"))
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(app.modal.is_none());

    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    let Some(Modal::SandboxGrant(d)) = &app.modal else {
        panic!("not offered: {:?}", app.modal);
    };
    assert!(d.blocked.is_some());
    // A new file in a directory that exists: the directory.
    assert_eq!(d.path, data.display().to_string());
    let said = d.said.as_deref().unwrap();
    assert!(said.contains(&target.display().to_string()), "{said}");
    // Enter allows and asks it to try again.
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert!(writable(&app).contains(&data));
    let actions = app.take_actions();
    assert_eq!(actions[0], Action::Shutdown);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::StartSession { resume: Some(id) } if id == "claude-1")),
        "{actions:?}"
    );
    assert!(format!("{actions:?}").contains("Please try that again"));
}

/// Esc leaves it: it is not offered again in the run.
#[test]
fn a_refused_path_left_is_not_offered_again() {
    let (_tmp, mut app, data) = granting_app();
    let refuse = |app: &mut App| {
        app.submit_prompt("write it".into());
        app.take_actions();
        app.controller.session_alive = true;
        app.controller.sandbox_started();
        app.controller.sandbox_started();
        app.on_event(AgentEvent::ToolCallStarted {
            id: "w".into(),
            name: "Write".into(),
            input: serde_json::json!({"file_path": data.join("a/b.txt")}),
        });
        app.on_event(AgentEvent::ToolCallResult {
            id: "w".into(),
            output: format!(
                "EACCES: permission denied, mkdir '{}'",
                data.join("a").display()
            ),
            is_error: true,
        });
        app.on_event(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        });
    };
    refuse(&mut app);
    let Some(Modal::SandboxGrant(d)) = &app.modal else {
        panic!("not offered");
    };
    // Both suggest `a`, not there yet: it is offered once, to be made.
    assert_eq!(d.path, data.join("a").display().to_string());
    app.handle_modal_key(key(KeyCode::Esc));
    assert!(app.modal.is_none());
    assert!(!data.join("a").exists());
    refuse(&mut app);
    assert!(app.modal.is_none());
}

/// Saved for the workspace, a grant is in its settings file under the
/// config directory, and the run has it without being started again.
#[test]
fn a_grant_is_saved_for_the_workspace() {
    let (_tmp, mut app, data) = granting_app();
    app.open_sandbox_grant(&data.display().to_string(), Access::Write);
    app.handle_modal_key(key(KeyCode::Down));
    let Some(Modal::SandboxGrant(d)) = &app.modal else {
        panic!("no dialog");
    };
    assert_eq!(d.scope(), GrantScope::Saved(Scope::Workspace));
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.controller.sandbox.grants.is_empty());
    assert!(writable(&app).contains(&data));

    let dir = app
        .controller
        .tracking
        .rules
        .config_dir()
        .unwrap()
        .to_path_buf();
    let ws = app.controller.workspace_root.clone().unwrap();
    let saved = Config::load_workspace_in(&dir.join("workspaces"), &ws)
        .unwrap()
        .unwrap();
    assert_eq!(saved.sandbox.writable, [data]);
}

#[test]
fn a_missing_path_or_one_without_a_sandbox_is_not_granted() {
    let (tmp, mut app, _) = granting_app();
    // To write, one not there is made; to read, it cannot be.
    assert!(!app.grant_sandbox(
        &tmp.path().join("nope"),
        Access::Read,
        GrantScope::Run,
        true
    ));
    assert!(app.controller.sandbox.grants.is_empty());
    // Only when asked to (`make`): a file name is never made a folder.
    assert!(!app.grant_sandbox(
        &tmp.path().join("new/out.txt"),
        Access::Write,
        GrantScope::Run,
        false
    ));
    assert!(!tmp.path().join("new").exists());
    assert!(app.grant_sandbox(
        &tmp.path().join("new/dir"),
        Access::Write,
        GrantScope::Run,
        true
    ));
    assert!(tmp.path().join("new/dir").is_dir());
    app.controller.sandbox.grants.clear();
    app.controller.sandbox = SandboxSetup::off();
    assert!(!app.grant_sandbox(tmp.path(), Access::Write, GrantScope::Run, false));
    assert!(app.controller.sandbox.grants.is_empty());
}

/// `a` allows without asking the agent to try again.
#[test]
fn a_refused_path_can_be_allowed_without_a_retry() {
    let (_tmp, mut app, data) = granting_app();
    app.submit_prompt("write it".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    app.on_event(AgentEvent::ToolCallResult {
        id: "w".into(),
        output: format!("zsh:1: permission denied: {}", data.join("x").display()),
        is_error: true,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert!(matches!(&app.modal, Some(Modal::SandboxGrant(_))));
    app.handle_modal_key(key(KeyCode::Char('a')));
    assert!(writable(&app).contains(&data));
    assert_eq!(app.take_actions(), [Action::Shutdown]);
}

/// `rmdir` of a path granted on its own is a change to the directory
/// it is in: that is offered, named as what it is (conversation
/// 161a2ad9).
#[test]
fn removing_a_granted_path_offers_its_parent() {
    let (_tmp, mut app, data) = granting_app();
    let sub = data.join("sub");
    std::fs::create_dir(&sub).unwrap();
    assert!(app.grant_sandbox(&sub, Access::Write, GrantScope::Run, false));
    app.submit_prompt("remove it".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    app.on_event(AgentEvent::ToolCallStarted {
        id: "rm".into(),
        name: "Bash".into(),
        input: serde_json::json!({"command": format!("rmdir {}", sub.display())}),
    });
    app.on_event(AgentEvent::ToolCallResult {
        id: "rm".into(),
        output: format!(
            "rmdir: failed to remove '{}': Permission denied",
            sub.display()
        ),
        is_error: false,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    let Some(Modal::SandboxGrant(d)) = &app.modal else {
        panic!("not offered: {:?}", app.modal);
    };
    assert_eq!(d.path, data.display().to_string());
    let said = d.blocked.as_ref().unwrap().describe("Claude");
    assert!(
        said.starts_with("Claude tried to remove or rename"),
        "{said}"
    );
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(writable(&app).contains(&data));
}

/// Removing a path made in a directory that holds unharness's own state
/// (the home directory, on the machine) cannot be allowed: it is said,
/// and nothing is offered.
#[test]
fn what_cannot_be_allowed_is_said_and_not_offered() {
    let (tmp, mut app, data) = granting_app();
    // unharness's state, as it is there on the machine.
    std::fs::create_dir(tmp.path().join("state")).unwrap();
    assert!(app.grant_sandbox(&data, Access::Write, GrantScope::Run, false));
    app.submit_prompt("remove it".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    app.on_event(AgentEvent::ToolCallResult {
        id: "rm".into(),
        output: format!(
            "rmdir: failed to remove '{}': Permission denied",
            data.display()
        ),
        is_error: true,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert!(app.modal.is_none(), "{:?}", app.modal);
    let root = tmp.path().canonicalize().unwrap();
    let said = format!("{:?}", app.controller.conversation.transcript.blocks);
    assert!(said.contains("tried to remove or rename"), "{said}");
    assert!(
        said.contains(&format!(
            "{} holds unharness's own settings",
            root.display()
        )),
        "{said}"
    );
}

/// A refused write, told during a turn of `app`'s session.
fn refuse_write(app: &mut App, id: &str, path: &Path) {
    app.on_event(AgentEvent::ToolCallStarted {
        id: id.into(),
        name: "Bash".into(),
        input: serde_json::json!({"command": format!("touch {}", path.display())}),
    });
    app.on_event(AgentEvent::ToolCallResult {
        id: id.into(),
        output: format!(
            "touch: cannot touch '{}': Permission denied",
            path.display()
        ),
        is_error: true,
    });
}

fn told(app: &App, what: &str) -> usize {
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .filter(|b| format!("{b:?}").contains(what))
        .count()
}

/// What one harness was kept from is not offered to the next, and the
/// next is told of its own refusal of the same path.
#[test]
fn a_switch_forgets_what_the_harness_before_was_kept_from() {
    let (_tmp, mut app, data) = granting_app();
    app.submit_prompt("write it".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    refuse_write(&mut app, "c1", &data.join("x"));
    // A picker is open as the turn ends: the offer waits behind it.
    app.open_sandbox_picker();
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    assert!(matches!(app.modal, Some(Modal::Sandbox(_))));
    app.switch_harness(HarnessId::CODEX);
    app.handle_modal_key(key(KeyCode::Esc));
    assert!(app.modal.is_none(), "{:?}", app.modal);

    app.submit_prompt("write it".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    refuse_write(&mut app, "x1", &data.join("x"));
    assert_eq!(told(&app, "Codex tried to write"), 1);
}

/// A path granted during a turn reaches only the next process: the one
/// running is still kept from it, and that is told.
#[test]
fn a_refusal_is_judged_against_the_process_that_ran_it() {
    let (_tmp, mut app, data) = granting_app();
    app.submit_prompt("write it".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    assert!(app.grant_sandbox(&data, Access::Write, GrantScope::Run, false));
    refuse_write(&mut app, "t1", &data.join("x"));
    assert_eq!(told(&app, "tried to write"), 1);
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    // Granted already: not offered again.
    assert!(app.modal.is_none(), "{:?}", app.modal);
}

/// The folder offered is made; a file name typed in its place is not
/// made into one.
#[test]
fn an_edited_file_name_is_not_made_a_folder() {
    let (_tmp, mut app, data) = granting_app();
    app.submit_prompt("write it".into());
    app.take_actions();
    app.controller.session_alive = true;
    app.controller.sandbox_started();
    refuse_write(&mut app, "t1", &data.join("a/b.txt"));
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    let Some(Modal::SandboxGrant(d)) = &app.modal else {
        panic!("not offered");
    };
    assert_eq!(d.path, data.join("a").display().to_string());
    assert!(d.make());
    app.handle_modal_key(key(KeyCode::Char('e')));
    for c in "/b.txt".chars() {
        app.handle_modal_key(key(KeyCode::Char(c)));
    }
    app.handle_modal_key(key(KeyCode::Enter));
    let Some(Modal::SandboxGrant(d)) = &app.modal else {
        panic!("closed");
    };
    assert!(!d.make());
    assert!(d.notes[0].text.contains("End it with /"), "{:?}", d.notes);
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(!data.join("a").exists());
    assert!(!writable(&app).iter().any(|w| w.starts_with(&data)));
}
