//! A prompt sent at once in place of the running turn (`/now`,
//! Ctrl+Enter, #333): interrupted and followed where every interrupt
//! works, taken with the interrupt by Claude, and started again after the
//! exit of a CLI whose interrupt ends it (agy).

use super::*;
use crate::core::controller::STOP_GRACE;
use crate::core::transcript::Block;

/// `harness`, live, in the turn of the prompt "go", whose anchor (if it
/// has them) has come.
fn running(harness: HarnessId) -> TestApp {
    let mut app = test_app(harness);
    app.submit_prompt("go".into());
    app.controller.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "s1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnAnchor { id: "a1".into() });
    app.take_actions();
    app
}

fn send_now(app: &mut TestApp, text: &str) {
    app.with(|c, front| c.send_now(text.to_string(), front));
}

fn ended(app: &mut TestApp, stop_reason: StopReason) {
    app.on_event(AgentEvent::TurnCompleted { stop_reason });
}

fn queued(app: &TestApp) -> Vec<String> {
    app.controller
        .queued
        .iter()
        .map(|q| q.text.clone())
        .collect()
}

fn users(app: &TestApp) -> Vec<String> {
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::User { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn system_lines(app: &TestApp) -> Vec<String> {
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::System(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Codex app-server (and pi, ACP): the turn is interrupted, and the
/// prompt is the next turn once the interrupted one has ended, ahead of
/// what was queued, which goes as usual after it.
#[test]
fn the_prompt_follows_the_interrupted_turn() {
    let mut app = running(HarnessId::CODEX);
    app.queue_prompt("later".into());
    send_now(&mut app, "now");
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    assert!(app.controller.is_generating);
    ended(&mut app, StopReason::Interrupted);
    assert_eq!(app.take_actions(), vec![Action::turn("now")]);
    assert!(app.controller.is_generating);
    assert_eq!(queued(&app), ["later"]);
    assert_eq!(users(&app), ["go", "now"]);
    assert!(system_lines(&app).contains(&"Turn interrupted.".to_string()));
    // Asked again after its own turn, an interrupt is one again.
    assert_eq!(app.controller.interrupted, None);

    ended(&mut app, StopReason::Done);
    assert_eq!(app.take_actions(), vec![Action::turn("later")]);
}

/// From an empty prompt box, the oldest queued prompt goes; the others
/// keep their places.
#[test]
fn an_empty_box_sends_the_head_of_the_queue() {
    let mut app = running(HarnessId::CODEX);
    app.queue_prompt("first".into());
    app.queue_prompt("second".into());
    send_now(&mut app, "");
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    assert_eq!(queued(&app), ["second"]);
    ended(&mut app, StopReason::Interrupted);
    assert_eq!(app.take_actions(), vec![Action::turn("first")]);
    assert_eq!(queued(&app), ["second"]);

    // Nothing queued and nothing typed: nothing is interrupted.
    let mut app = running(HarnessId::CODEX);
    send_now(&mut app, "  ");
    assert!(app.take_actions().is_empty());
    assert!(has_notice(
        &app,
        "nothing to send now: type a prompt, or queue one first"
    ));
}

/// With no turn running it is what Enter does.
#[test]
fn idle_it_is_enter() {
    let mut app = test_app(HarnessId::CODEX);
    app.controller.session_alive = true;
    send_now(&mut app, "hello");
    assert_eq!(app.take_actions(), vec![Action::turn("hello")]);

    app.queue_prompt("held".into());
    ended(&mut app, StopReason::Interrupted);
    // Held after the interrupt: an empty box sends it, as Enter does.
    assert!(app.take_actions().is_empty());
    assert!(!app.controller.is_generating);
    send_now(&mut app, "");
    assert_eq!(app.take_actions(), vec![Action::turn("held")]);
}

/// Pi ends an aborted turn as done: the prompt goes all the same, not the
/// queue's head.
#[test]
fn a_turn_aborted_as_done_is_followed_by_the_prompt() {
    let mut app = running(HarnessId::PI);
    app.queue_prompt("later".into());
    send_now(&mut app, "now");
    app.take_actions();
    ended(&mut app, StopReason::Done);
    assert_eq!(app.take_actions(), vec![Action::turn("now")]);
    assert_eq!(queued(&app), ["later"]);
}

/// Claude has the prompt with the interrupt and starts its turn itself:
/// nothing is sent when the interrupted turn ends (Claude's
/// `error_during_execution`, which its driver tells as the interrupt it
/// was). Its anchor comes once Claude started on it, for the prompt's
/// block.
#[test]
fn claude_takes_the_prompt_with_the_interrupt() {
    let mut app = running(HarnessId::CLAUDE);
    app.queue_prompt("later".into());
    send_now(&mut app, "now");
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::SendNow {
            text: "now".into(),
            attachments: Vec::new(),
        })]
    );
    assert_eq!(users(&app), ["go"]);
    ended(&mut app, StopReason::Interrupted);
    assert!(app.take_actions().is_empty());
    assert!(app.controller.is_generating);
    assert_eq!(users(&app), ["go", "now"]);
    assert!(system_lines(&app).contains(&"Turn interrupted.".to_string()));
    assert!(
        !notices(&app)
            .iter()
            .any(|n| n.contains("error_during_execution")),
        "{:?}",
        notices(&app)
    );
    app.on_event(AgentEvent::TurnAnchor { id: "n1".into() });
    let anchors = &app.controller.conversation.anchors;
    let now = users_block(&app, "now");
    assert!(anchors.iter().any(|a| a.block == now && a.id == "n1"));

    ended(&mut app, StopReason::Done);
    assert_eq!(app.take_actions(), vec![Action::turn("later")]);
}

/// Claude took the prompt into the running turn (what the turn waited on
/// went to the background): its anchor comes before that turn's end. It
/// is shown there, and the turn goes on; Esc interrupts it anew.
#[test]
fn claude_takes_the_prompt_into_the_running_turn() {
    let mut app = running(HarnessId::CLAUDE);
    app.queue_prompt("later".into());
    send_now(&mut app, "now");
    app.take_actions();
    app.on_event(AgentEvent::TurnAnchor { id: "n1".into() });
    assert_eq!(users(&app), ["go", "now"]);
    assert!(app.controller.sending_now.is_none());
    let now = users_block(&app, "now");
    assert!(
        app.controller
            .conversation
            .anchors
            .iter()
            .any(|a| a.block == now && a.id == "n1")
    );
    app.interrupt();
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    ended(&mut app, StopReason::Done);
    assert_eq!(app.take_actions(), vec![Action::turn("later")]);
}

/// An anchor still owed for an earlier turn is that turn's, not a sign
/// that the prompt went into the turn.
#[test]
fn an_owed_anchor_is_not_taken_for_the_prompt() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.submit_prompt("go".into());
    app.controller.session_alive = true;
    app.take_actions();
    send_now(&mut app, "now");
    app.take_actions();
    app.on_event(AgentEvent::TurnAnchor { id: "a1".into() });
    assert!(app.controller.sending_now.is_some());
    assert_eq!(users(&app), ["go"]);
}

/// Esc while it waits: it is not sent, and goes back where it came from;
/// the turn still ends. Claude is asked to drop it with the turn.
#[test]
fn an_interrupt_while_it_waits_gives_it_back() {
    let mut app = running(HarnessId::CODEX);
    app.front.attachments = vec![Attachment::Image {
        path: "/x/a.png".into(),
        mime: "image/png".into(),
    }];
    send_now(&mut app, "typed");
    app.take_actions();
    app.interrupt();
    assert!(app.take_actions().is_empty());
    assert_eq!(app.front.input, "typed");
    assert_eq!(app.front.attachments.len(), 1);
    assert!(has_notice(
        &app,
        "'typed' was not sent; it is back in the prompt box"
    ));
    ended(&mut app, StopReason::Interrupted);
    assert!(app.take_actions().is_empty());
    assert!(!app.controller.is_generating);

    // One taken from the queue goes back to its head.
    let mut app = running(HarnessId::CODEX);
    app.queue_prompt("first".into());
    app.queue_prompt("second".into());
    send_now(&mut app, "");
    app.take_actions();
    app.interrupt();
    assert_eq!(queued(&app), ["first", "second"]);
    assert_eq!(app.front.input, "");
    // The next Esc presses the turn, as a second interrupt does.
    app.interrupt();
    assert!(notices(&app).iter().any(|n| n.starts_with("waiting for")));

    let mut app = running(HarnessId::CLAUDE);
    send_now(&mut app, "typed");
    app.take_actions();
    app.interrupt();
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    assert_eq!(app.front.input, "typed");
    ended(&mut app, StopReason::Interrupted);
    assert!(app.take_actions().is_empty());
    assert!(!app.controller.is_generating);
    assert!(system_lines(&app).contains(&"Turn interrupted.".to_string()));
    assert!(!notices(&app).iter().any(|n| n.contains("error_during")));

    // A Claude that cannot be made to drop it (no `cancel_queued`): Esc
    // leaves it with Claude, says so, and Claude's turn for it is shown.
    let mut app = running(HarnessId::CLAUDE);
    app.on_event(AgentEvent::CapabilitiesChanged(CapsUpdate {
        take_back_sent_now: Some(false),
        ..Default::default()
    }));
    send_now(&mut app, "typed");
    app.take_actions();
    app.interrupt();
    assert!(app.take_actions().is_empty());
    assert_eq!(app.front.input, "");
    assert!(has_notice(
        &app,
        "Claude already has 'typed' and runs it once the turn has stopped"
    ));
    ended(&mut app, StopReason::Interrupted);
    assert!(app.controller.is_generating);
    assert_eq!(users(&app), ["go", "typed"]);
}

/// A plain Esc on Claude: its driver tells the stopped turn as
/// interrupted, and so it is shown, not as an error.
#[test]
fn a_plain_interrupt_on_claude_is_shown_as_one() {
    let mut app = running(HarnessId::CLAUDE);
    app.interrupt();
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    ended(&mut app, StopReason::Interrupted);
    assert!(system_lines(&app).contains(&"Turn interrupted.".to_string()));
    assert!(
        !app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Error(_)))
    );
}

/// What cannot be sent now is queued for when the turn ends, and said to
/// be; nothing is interrupted.
#[test]
fn what_cannot_go_now_is_queued() {
    // A compaction's turn.
    let mut app = running(HarnessId::CODEX);
    ended(&mut app, StopReason::Done);
    app.with(|c, front| c.compact(None, front));
    app.take_actions();
    send_now(&mut app, "now");
    assert!(app.take_actions().is_empty());
    assert_eq!(queued(&app), ["now"]);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.ends_with("'now' is queued for when the turn ends"))
    );

    // A handoff summary's turn: the answer would be taken for the summary.
    let mut app = over_budget_app();
    app.switch_harness(HarnessId::CODEX);
    assert!(app.controller.handoff.is_some());
    app.take_actions();
    send_now(&mut app, "now");
    assert!(app.take_actions().is_empty());
    assert_eq!(queued(&app), ["now"]);
    assert!(has_notice(
        &app,
        "a handoff summary is being written: 'now' is queued for when the turn ends"
    ));

    // A session that is not there to interrupt; the head of the queue
    // stays the head.
    let mut app = running(HarnessId::CODEX);
    app.queue_prompt("first".into());
    app.queue_prompt("second".into());
    app.controller.session_alive = false;
    send_now(&mut app, "");
    assert!(app.take_actions().is_empty());
    assert_eq!(queued(&app), ["first", "second"]);

    // A choice the session has not said how it took.
    let mut app = running(HarnessId::CODEX);
    app.controller.pending = Some(crate::core::controller::PendingChoice {
        queued: 0,
        what: "provider",
        provider: Some(ProviderId::new("openai")),
    });
    send_now(&mut app, "now");
    assert!(app.take_actions().is_empty());
    assert_eq!(queued(&app), ["now"]);
}

/// Asked again while it waits: what was typed queues behind it, and the
/// turn is pressed; past `STOP_GRACE` the CLI is ended, and the prompt is
/// the resumed session's first.
#[test]
fn a_stuck_turn_is_ended_and_the_prompt_resumes_the_session() {
    let mut app = running(HarnessId::PI);
    app.queue_prompt("later".into());
    send_now(&mut app, "now");
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    send_now(&mut app, "also");
    assert!(app.take_actions().is_empty());
    assert_eq!(queued(&app), ["also", "later"]);
    assert!(notices(&app).iter().any(|n| n.starts_with("waiting for")));
    app.controller.interrupted = Some((Instant::now() - STOP_GRACE, true));
    send_now(&mut app, "");
    assert_eq!(
        app.take_actions(),
        vec![
            Action::Shutdown,
            Action::StartSession {
                resume: Some("s1".into())
            },
            Action::turn("now"),
        ]
    );
    assert!(app.controller.is_generating);
    assert_eq!(queued(&app), ["also", "later"]);

    // So too on Claude, whose CLI had it: the new one is sent it.
    let mut app = running(HarnessId::CLAUDE);
    send_now(&mut app, "now");
    app.take_actions();
    app.controller.interrupted = Some((Instant::now() - STOP_GRACE, true));
    send_now(&mut app, "");
    assert_eq!(
        app.take_actions(),
        vec![
            Action::Shutdown,
            Action::StartSession {
                resume: Some("s1".into())
            },
            Action::turn("now"),
        ]
    );
}

/// agy's interrupt ends its process: the prompt waits for the exit, and
/// starts it again on the conversation.
#[test]
fn agy_is_started_again_with_the_prompt() {
    let mut app = running(HarnessId::AGY);
    send_now(&mut app, "now");
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    ended(&mut app, StopReason::Interrupted);
    assert!(app.take_actions().is_empty());
    assert!(!app.controller.is_generating);
    app.on_event(AgentEvent::ProcessExited { code: None });
    assert_eq!(
        app.take_actions(),
        vec![
            Action::StartSession {
                resume: Some("s1".into())
            },
            Action::turn("now"),
        ]
    );
    assert!(app.controller.is_generating);

    // Its process gone before the turn's end was told: the same, and the
    // turn is said to be interrupted, not to have failed.
    let mut app = running(HarnessId::AGY);
    send_now(&mut app, "now");
    app.take_actions();
    app.on_event(AgentEvent::ProcessExited { code: None });
    assert_eq!(
        app.take_actions(),
        vec![
            Action::StartSession {
                resume: Some("s1".into())
            },
            Action::turn("now"),
        ]
    );
    assert!(!notices(&app).iter().any(|n| n.contains("exited")));
}

/// Codex `exec` kills the turn's process and resumes the thread in the
/// next one, which its driver starts for the next turn: the prompt
/// follows the interrupted turn's end, as on app-server.
#[test]
fn codex_exec_resumes_with_the_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = Arc::new(Registry::empty().with(Box::new(
        crate::harness::codex::CodexHarness::new(crate::harness::codex::CodexTransport::Exec),
    )));
    let mut app = test_app_with(
        tmp.path().to_path_buf(),
        HarnessId::CODEX,
        None,
        false,
        Config::default(),
        registry,
    );
    if app.effective_policy().is_none() {
        app.controller
            .policy
            .choose_for_conversation(HarnessId::CODEX, PermissionPolicy::AcceptEdits);
    }
    app.submit_prompt("go".into());
    app.controller.session_alive = true;
    app.take_actions();
    send_now(&mut app, "now");
    assert_eq!(
        app.take_actions(),
        vec![Action::Command(SessionCommand::Interrupt)]
    );
    ended(&mut app, StopReason::Interrupted);
    assert_eq!(app.take_actions(), vec![Action::turn("now")]);
}

/// A CLI that exits while the prompt waits (not one whose interrupt ends
/// it) takes nothing with it: the prompt goes back.
#[test]
fn a_cli_gone_meanwhile_gives_it_back() {
    let mut app = running(HarnessId::CODEX);
    send_now(&mut app, "now");
    app.take_actions();
    app.on_event(AgentEvent::ProcessExited { code: Some(1) });
    assert!(app.take_actions().is_empty());
    assert_eq!(app.front.input, "now");
    assert!(!app.controller.is_generating);
}

fn users_block(app: &TestApp, text: &str) -> usize {
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, Block::User { text: t } if t == text))
        .unwrap()
}
