use super::*;
use crate::core::RemoteControl;

#[test]
fn remote_control_is_offered_only_where_declared() {
    use super::super::transcript::Block;
    // pi runs commands of its own: one called /rc is its.
    let mut app = test_app(HarnessId::PI);
    app.session_alive = true;
    app.handle_slash_command("/rc now");
    assert!(app.remote.is_none());
    assert!(
        app.transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Notice(n) if n.contains("passed to pi as typed")))
    );
    assert!(
        !app.transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Notice(n) if n.contains("does not see what")))
    );
    assert!(!app.offers_command("/remote-control"));
    assert!(!app.owns_command("/rc"));
    assert!(app.offers_command("/model"));
    let claude = test_app(HarnessId::CLAUDE);
    assert!(claude.owns_command("/rc"));
}

#[test]
fn remote_control_taken_off_before_it_answers_says_off_once() {
    use super::super::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.handle_slash_command("/rc");
    app.handle_slash_command("/rc off");
    // The answers to both, in order.
    app.on_event(AgentEvent::RemoteControl(RemoteControl::On {
        url: "https://claude.ai/code/session_x".into(),
    }));
    app.on_event(AgentEvent::RemoteControl(RemoteControl::Off));
    let said: Vec<&str> = app
        .transcript
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::System(t) | Block::Notice(t) if t.contains("Remote Control") => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        said,
        [
            "turning on Remote Control for Claude",
            "Remote Control is off"
        ]
    );
    assert_eq!(app.remote_status(), None);
}

#[test]
fn remote_control_waits_for_the_session_and_pauses_with_its_process() {
    let mut app = test_app(HarnessId::CLAUDE);
    // No session yet: nothing is started for it.
    app.handle_slash_command("/rc");
    assert!(app.take_actions().is_empty());
    assert_eq!(app.remote_status().as_deref(), Some("remote: paused"));
    app.session_alive = true;
    assert!(app.remote_control_resend().is_some());
    app.on_event(AgentEvent::RemoteControl(RemoteControl::On {
        url: "https://claude.ai/code/session_x".into(),
    }));
    assert_eq!(app.remote_status().as_deref(), Some("remote"));
    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    assert_eq!(app.remote_status().as_deref(), Some("remote: paused"));
    assert!(app.remote_control_resend().is_some());
}

#[test]
fn a_resume_ends_remote_control() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app_in(tmp.keep(), HarnessId::CLAUDE, None, false);
    app.submit_prompt("first question".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.persist();
    let id = app.conversation.id.clone();
    app.handle_slash_command("/rc");
    app.take_actions();
    app.resume_conversation(id);
    let actions = app.take_actions();
    assert_eq!(
        actions.first(),
        Some(&Action::Command(SessionCommand::RemoteControl {
            enabled: false,
            name: None
        }))
    );
    assert_eq!(app.remote_status(), None);
}

#[test]
fn a_remote_first_prompt_leaves_the_bridge_owed_and_titles_the_conversation() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.transcript.push_user("asked on codex");
    app.transcript
        .append_assistant("Codex", "answered on codex");
    app.session_alive = true;
    app.session_blank = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::RemotePrompt {
        text: "typed remotely".into(),
    });
    // The next prompt typed here still tells Claude what it missed.
    assert_eq!(app.bridge_start(HarnessId::CLAUDE), 0);
    assert_eq!(app.first_prompt.as_deref(), Some("typed remotely"));
}

#[test]
fn remote_control_comes_back_on_each_new_process_until_a_switch() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.handle_slash_command("/remote-control on my desk");
    assert_eq!(
        app.take_actions(),
        [Action::Command(SessionCommand::RemoteControl {
            enabled: true,
            name: Some("my desk".into())
        })]
    );
    assert_eq!(app.remote_status().as_deref(), Some("remote: starting"));
    app.on_event(AgentEvent::RemoteControl(RemoteControl::On {
        url: "https://claude.ai/code/session_x".into(),
    }));
    app.on_event(AgentEvent::RemoteControl(RemoteControl::State {
        state: "connected".into(),
        detail: None,
    }));
    assert_eq!(app.remote_status().as_deref(), Some("remote"));
    // A new process (a sandbox change, a resume...) is put on it again.
    assert_eq!(
        app.remote_control_resend(),
        Some(SessionCommand::RemoteControl {
            enabled: true,
            name: Some("my desk".into())
        })
    );
    assert_eq!(app.remote_status().as_deref(), Some("remote: starting"));
    // A switch ends it, telling the remote side first.
    app.switch_now(HarnessId::CODEX);
    let actions = app.take_actions();
    assert_eq!(
        actions[0],
        Action::Command(SessionCommand::RemoteControl {
            enabled: false,
            name: None
        })
    );
    assert_eq!(actions[1], Action::Shutdown);
    assert_eq!(app.remote_status(), None);
    assert_eq!(app.remote_control_resend(), None);
}

#[test]
fn a_failed_remote_control_is_dropped() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.handle_slash_command("/rc");
    app.take_actions();
    app.on_event(AgentEvent::RemoteControl(RemoteControl::Failed {
        reason: "your organization does not allow Remote Control".into(),
    }));
    assert_eq!(app.remote_status(), None);
    assert_eq!(app.remote_control_resend(), None);
}

#[test]
fn a_remote_prompt_is_shown_and_makes_the_session_the_conversations() {
    use super::super::transcript::Block;
    let mut app = test_app(HarnessId::CLAUDE);
    app.session_alive = true;
    app.session_blank = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::RemotePrompt {
        text: "Reply with the single word: pong".into(),
    });
    assert!(!app.session_blank);
    assert_eq!(
        app.session_ids.get(&HarnessId::CLAUDE).map(String::as_str),
        Some("claude-1")
    );
    assert!(matches!(
        app.transcript.blocks.last(),
        Some(Block::User { text }) if text == "Reply with the single word: pong"
    ));
}
