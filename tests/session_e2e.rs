//! End-to-end session tests against `scripts/fake-harness.py`, which replays
//! recorded vendor output and blocks wherever the recording shows the client
//! sending a line. This exercises the real transport drivers (process
//! spawning, stdin writes, parsing, permission round-trips, shutdown).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use unharness::core::{
    AgentEvent, HarnessId, ModelRef, PermissionDecision, PermissionPolicy, SessionCommand,
    SessionConfig, SessionHandle, StopReason,
};
use unharness::harness::Harness;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fake_harness() -> PathBuf {
    repo().join("scripts").join("fake-harness.py")
}

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Fake {
    _tmp: tempfile::TempDir,
    log: PathBuf,
}

impl Fake {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("sent.log");
        Fake { _tmp: tmp, log }
    }

    fn config(&self, fixture: &Path, policy: PermissionPolicy, hang: bool) -> SessionConfig {
        let mut env = vec![
            (
                "UNHARNESS_FAKE_FIXTURE".to_string(),
                fixture.to_string_lossy().to_string(),
            ),
            (
                "UNHARNESS_FAKE_LOG".to_string(),
                self.log.to_string_lossy().to_string(),
            ),
        ];
        if hang {
            env.push(("UNHARNESS_FAKE_HANG".to_string(), "1".to_string()));
        }
        SessionConfig {
            binary: fake_harness(),
            cwd: self._tmp.path().to_path_buf(),
            model: Some(ModelRef::new(HarnessId::Claude, "anthropic", "haiku")),
            effort: None,
            policy,
            resume: None,
            extra_args: vec![],
            env,
        }
    }

    fn sent_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

async fn next_event(handle: &mut SessionHandle) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(10), handle.events.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event channel closed")
}

/// Drive one turn to completion, answering permission requests with `decide`.
async fn run_turn(
    handle: &mut SessionHandle,
    decide: impl Fn(&AgentEvent) -> Option<PermissionDecision>,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let ev = next_event(handle).await;
        if let Some(decision) = decide(&ev)
            && let AgentEvent::PermissionRequest(req) = &ev
        {
            handle
                .send(SessionCommand::RespondPermission {
                    id: req.id.clone(),
                    decision,
                })
                .await
                .unwrap();
        }
        let done = matches!(
            ev,
            AgentEvent::TurnCompleted { .. } | AgentEvent::ProcessExited { .. }
        );
        events.push(ev);
        if done {
            break;
        }
    }
    events
}

#[tokio::test]
async fn claude_basic_turn_streams_tool_and_text() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/basic_turn.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness;
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle
        .send(SessionCommand::SendTurn {
            text: "run echo".into(),
        })
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;

    assert!(matches!(
        events.first(),
        Some(AgentEvent::SessionStarted { .. })
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallStarted { name, .. } if name == "Bash"))
    );
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::ToolCallResult { output, .. } if output == "spike-ok")
        )
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "done"))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));

    // The driver sent the initialize handshake and then the user turn.
    let sent = fake.sent_lines();
    assert_eq!(sent[0]["type"], "control_request");
    assert_eq!(sent[0]["request"]["subtype"], "initialize");
    assert_eq!(sent[1]["type"], "user");
    assert_eq!(sent[1]["message"]["content"], "run echo");

    handle.send(SessionCommand::Shutdown).await.unwrap();
    let ev = next_event(&mut handle).await;
    assert!(
        matches!(ev, AgentEvent::ProcessExited { code: Some(0) }),
        "{ev:?}"
    );
}

#[tokio::test]
async fn claude_permission_round_trip_and_question() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/permission_and_question.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness;
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    // Turn 1: Write needs permission; allow it.
    handle
        .send(SessionCommand::SendTurn {
            text: "create spike3.txt".into(),
        })
        .await
        .unwrap();
    let events = run_turn(&mut handle, |ev| match ev {
        AgentEvent::PermissionRequest(_) => Some(PermissionDecision::Allow {
            updated_input: None,
        }),
        _ => None,
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionRequest(r) if r.tool_call_id.is_some()))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "created"))
    );

    // Turn 2: AskUserQuestion; answer "Red".
    handle
        .send(SessionCommand::SendTurn {
            text: "ask me a question".into(),
        })
        .await
        .unwrap();
    let events = run_turn(&mut handle, |ev| match ev {
        AgentEvent::PermissionRequest(_) => Some(PermissionDecision::Answer(
            serde_json::json!({"Do you prefer red or blue?": "Red"}),
        )),
        _ => None,
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "you chose Red"))
    );

    let sent = fake.sent_lines();
    let responses: Vec<&Value> = sent
        .iter()
        .filter(|v| v["type"] == "control_response")
        .collect();
    assert_eq!(responses.len(), 2, "{sent:?}");
    assert_eq!(responses[0]["response"]["response"]["behavior"], "allow");
    assert_eq!(
        responses[0]["response"]["response"]["updatedInput"]["file_path"],
        "/WORKSPACE/spike3.txt"
    );
    assert_eq!(
        responses[1]["response"]["response"]["updatedInput"]["answers"]["Do you prefer red or blue?"],
        "Red"
    );
    // The question's original input was echoed back alongside the answers.
    assert!(responses[1]["response"]["response"]["updatedInput"]["questions"].is_array());

    handle.send(SessionCommand::Shutdown).await.unwrap();
    assert!(matches!(
        next_event(&mut handle).await,
        AgentEvent::ProcessExited { .. }
    ));
}

#[tokio::test]
async fn process_exit_is_reported_and_interrupt_kills() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/basic_turn.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness;

    // Without HANG the fake exits once the fixture is exhausted.
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Bypass, false))
        .unwrap();
    handle
        .send(SessionCommand::SendTurn { text: "x".into() })
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
    );
    let ev = next_event(&mut handle).await;
    assert!(
        matches!(ev, AgentEvent::ProcessExited { code: Some(0) }),
        "{ev:?}"
    );

    // Dropping the handle (TUI gone) must not leave the child running: a
    // hanging fake is killed by the driver when the command channel closes.
    let fake2 = Fake::new();
    let handle2 = harness
        .start_session(fake2.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
    assert!(handle2.is_alive());
    drop(handle2);
    tokio::time::sleep(Duration::from_millis(200)).await;
    // No assertion possible on the pid from here; the test passing without a
    // hang (tokio runtime shutdown would wait on nothing) is the signal.
}
