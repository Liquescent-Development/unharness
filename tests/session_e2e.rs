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
        if std::env::var("UNHARNESS_E2E_DEBUG").is_ok() {
            eprintln!("[e2e] {}", ev.summary());
        }
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

    handle.send(SessionCommand::turn("run echo")).await.unwrap();
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
        .send(SessionCommand::turn("create spike3.txt"))
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
        .send(SessionCommand::turn("ask me a question"))
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
    handle.send(SessionCommand::turn("x")).await.unwrap();
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

#[tokio::test]
async fn pi_rpc_session_streams_tool_and_text() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/basic_and_bash.jsonl");
    let harness = unharness::harness::pi::PiHarness;
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    // The driver sends get_state first; the fixture then replays the catalog
    // responses before the first prompt.
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "pong"))
    );

    handle.send(SessionCommand::turn("run bash")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallStarted { name, .. } if name == "bash"))
    );
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolCallResult { output, .. } if output == "pi-spike-ok\n")
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "done"))
    );

    let sent = fake.sent_lines();
    assert_eq!(sent[0]["type"], "get_state");
    assert!(
        sent.iter()
            .any(|v| v["type"] == "prompt" && v["message"] == "run bash")
    );

    handle.send(SessionCommand::Shutdown).await.unwrap();
    assert!(matches!(
        next_event(&mut handle).await,
        AgentEvent::ProcessExited { .. }
    ));
}

#[tokio::test]
async fn codex_app_server_handshake_turns_and_approval() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_two_turns.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    );
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id.starts_with("01a10023-e1c2"))));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "pong"))
    );

    handle.send(SessionCommand::turn("run it")).await.unwrap();
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
            .any(|e| matches!(e, AgentEvent::PermissionRequest(r) if r.id == "0"))
    );
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolCallResult { output, .. } if output == "codex-app-ok\n")
    ));

    let sent = fake.sent_lines();
    assert_eq!(sent[0]["method"], "initialize");
    assert_eq!(sent[1]["method"], "initialized");
    assert_eq!(sent[2]["method"], "thread/start");
    assert_eq!(sent[2]["params"]["approvalPolicy"], "untrusted");
    assert_eq!(sent[3]["method"], "turn/start");
    assert_eq!(sent[3]["params"]["input"][0]["text"], "pong?");
    let approval = sent
        .iter()
        .find(|v| v["id"] == 0 && v.get("result").is_some())
        .expect("approval response");
    assert_eq!(approval["result"]["decision"], "accept");

    handle.send(SessionCommand::Shutdown).await.unwrap();
    assert!(matches!(
        next_event(&mut handle).await,
        AgentEvent::ProcessExited { .. }
    ));
}

#[tokio::test]
async fn codex_exec_per_turn_resumes_by_thread() {
    if !python_available() {
        return;
    }
    // The fake replays one fixture per spawned child; use the resume fixture
    // for both turns (its thread.started carries the id either way).
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/exec_resume_command.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::Exec,
    );
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::AcceptEdits, false))
        .unwrap();

    handle.send(SessionCommand::turn("one")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallStarted { name, .. } if name == "shell"))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));

    handle.send(SessionCommand::turn("two")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));

    // Each turn wrote its prompt to the child's stdin.
    let log = std::fs::read_to_string(&fake.log).unwrap();
    assert!(log.contains("one") && log.contains("two"));

    handle.send(SessionCommand::Shutdown).await.unwrap();
    assert!(matches!(
        next_event(&mut handle).await,
        AgentEvent::ProcessExited { .. }
    ));
}

#[tokio::test]
async fn agy_stream_session_against_synthetic_fixture() {
    if !python_available() {
        return;
    }
    // The fixture is synthetic (no agy account yet); this exercises the
    // transport plumbing, not the vendor protocol.
    let fake = Fake::new();
    let fixture = repo().join("src/harness/agy/fixtures/synthetic_turn.jsonl");
    let harness = unharness::harness::agy::AgyHarness::default();
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::AcceptEdits, true))
        .unwrap();

    handle.send(SessionCommand::turn("echo hi")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id == "conv-synthetic-1")));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallResult { output, .. } if output == "hi\n"))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));

    handle.send(SessionCommand::turn("delete")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("denied")))
    );

    let sent = fake.sent_lines();
    assert_eq!(sent[0]["type"], "user");
    assert_eq!(sent[0]["message"]["content"], "echo hi");

    handle.send(SessionCommand::Shutdown).await.unwrap();
    assert!(matches!(
        next_event(&mut handle).await,
        AgentEvent::ProcessExited { .. }
    ));
}

#[tokio::test]
async fn agy_auth_failure_ends_turn_with_error() {
    if !python_available() {
        return;
    }
    // Real recording: agy exits after a `result` with status ERROR.
    let fake = Fake::new();
    let fixture = repo().join("src/harness/agy/fixtures/auth_required.jsonl");
    let harness = unharness::harness::agy::AgyHarness::default();
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, false))
        .unwrap();
    handle.send(SessionCommand::turn("pong")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnCompleted { stop_reason: StopReason::Error(m) } if m.contains("authentication"))));
    // The stderr explanation and the exit arrive after the result line.
    let mut tail = Vec::new();
    loop {
        let ev = next_event(&mut handle).await;
        let exited = matches!(ev, AgentEvent::ProcessExited { .. });
        tail.push(ev);
        if exited {
            break;
        }
    }
    assert!(
        tail.iter()
            .any(|e| matches!(e, AgentEvent::Error(m) if m.contains("log in"))),
        "{tail:?}"
    );
    assert!(
        matches!(
            tail.last(),
            Some(AgentEvent::ProcessExited { code: Some(1) })
        ),
        "{tail:?}"
    );
}
