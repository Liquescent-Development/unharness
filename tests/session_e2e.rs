//! End-to-end session tests against `scripts/fake-harness.py`, which replays
//! recorded vendor output and blocks wherever the recording shows the client
//! sending a line. This exercises the real transport drivers (process
//! spawning, stdin writes, parsing, permission round-trips, shutdown).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use unharness::core::{
    AgentEvent, HarnessId, McpServer, McpTransport, ModelRef, PermissionDecision, PermissionKind,
    PermissionPolicy, SessionCommand, SessionConfig, SessionHandle, StopReason,
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
            model: Some(ModelRef::new(HarnessId::CLAUDE, "anthropic", "haiku")),
            provider: None,
            effort: None,
            policy,
            resume: None,
            fork: false,
            extra_args: vec![],
            env,
            mcp_servers: Vec::new(),
            sandbox: unharness::core::Sandbox::off(),
        }
    }

    fn sent_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// What the driver sent, once the fake has logged at least `count` lines.
    async fn sent_lines_eventually(&self, count: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let sent = self.sent_lines();
            if sent.len() >= count || tokio::time::Instant::now() > deadline {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
    let harness = unharness::harness::claude::ClaudeHarness::default();
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle.send(SessionCommand::turn("run echo")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;

    // The driver tags the turn with a uuid of its own (the rewind target)
    // before anything comes back from the harness.
    let Some(AgentEvent::TurnAnchor { id: anchor }) = events.first() else {
        panic!("expected the turn anchor first, got {:?}", events.first());
    };
    assert!(matches!(
        events.get(1),
        Some(AgentEvent::SessionStarted { .. })
    ));
    // The fixture has no `>>` lines: the fake replays the turn without
    // waiting and logs what it was sent only afterwards.
    let sent = fake.sent_lines_eventually(2).await;
    assert!(
        sent.iter()
            .any(|v| v["type"] == "user" && v["uuid"] == anchor.as_str())
    );
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

/// A chosen provider reaches Claude as its switch, and a session that runs
/// on another one (here the settings chose Vertex) says so.
#[tokio::test]
async fn claude_reports_running_on_another_provider() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/provider_vertex.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness::default();
    for (chosen, error) in [("bedrock", true), ("vertex", false)] {
        let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
        cfg.provider = Some(chosen.into());
        let mut handle = harness.start_session(cfg).unwrap();
        handle.send(SessionCommand::turn("hi")).await.unwrap();
        let events = run_turn(&mut handle, |_| None).await;
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::CapabilitiesChanged(u) if u.provider == Some("vertex".into())
        )));
        let reported = events.iter().any(|e| {
            matches!(e, AgentEvent::Error(m) if m.contains(&format!("runs on vertex, not {chosen}")))
        });
        assert_eq!(reported, error, "{chosen}: {events:?}");
        handle.send(SessionCommand::Shutdown).await.unwrap();
    }
}

#[tokio::test]
async fn claude_permission_round_trip_and_question() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/permission_and_question.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness::default();
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
    let harness = unharness::harness::claude::ClaudeHarness::default();

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

#[cfg(target_os = "linux")]
use unharness::core::process::running;

#[cfg(target_os = "linux")]
async fn gone_within(pid: u32, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while running(pid) {
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// The fake's pid and its background child's, once it wrote them
/// (`UNHARNESS_FAKE_BACKGROUND`).
#[cfg(target_os = "linux")]
async fn background_pids(path: &Path) -> (u32, u32) {
    for _ in 0..500 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Some((cli, child)) = text.trim().split_once(' ')
        {
            return (cli.parse().unwrap(), child.parse().unwrap());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the fake wrote no pids");
}

/// A fake Claude that runs a command in a session of its own and, once its
/// stdin closes, goes on for `linger` seconds: Claude waiting for a
/// Monitor, which it stops when asked to (`interrupt`).
#[cfg(target_os = "linux")]
fn lingering_claude(fake: &Fake, background: &str, linger: &str) -> (SessionHandle, PathBuf) {
    let fixture = repo().join("src/harness/claude/fixtures/basic_turn.jsonl");
    lingering_claude_on(fake, &fixture, background, linger)
}

#[cfg(target_os = "linux")]
fn lingering_claude_on(
    fake: &Fake,
    fixture: &Path,
    background: &str,
    linger: &str,
) -> (SessionHandle, PathBuf) {
    let (cfg, pids) = lingering_config(fake, fixture, background, linger);
    let handle = unharness::harness::claude::ClaudeHarness::default()
        .start_session(cfg)
        .unwrap();
    (handle, pids)
}

#[cfg(target_os = "linux")]
fn lingering_config(
    fake: &Fake,
    fixture: &Path,
    background: &str,
    linger: &str,
) -> (SessionConfig, PathBuf) {
    let pids = fake._tmp.path().join("pids");
    let mut cfg = fake.config(fixture, PermissionPolicy::Bypass, true);
    cfg.env.push((
        "UNHARNESS_FAKE_BACKGROUND".into(),
        format!("{background}:{}", pids.display()),
    ));
    cfg.env
        .push(("UNHARNESS_FAKE_LINGER".into(), linger.into()));
    (cfg, pids)
}

/// A fake pi that takes `get_state` and then a prompt it never answers,
/// and goes on for a minute once its stdin closes.
#[cfg(target_os = "linux")]
async fn lingering_pi(fake: &Fake) -> (SessionHandle, u32, u32) {
    let fixture = fake._tmp.path().join("silent.jsonl");
    std::fs::write(
        &fixture,
        ">> {\"type\": \"get_state\"}\n>> {\"type\": \"prompt\"}\n",
    )
    .unwrap();
    let (cfg, pids) = lingering_config(fake, &fixture, "session", "60");
    let handle = pi_harness().start_session(cfg).unwrap();
    let (cli, child) = background_pids(&pids).await;
    (handle, cli, child)
}

// Given its grace mid-turn, it could go on with the turn unseen.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn ending_a_session_mid_turn_kills_a_cli_that_has_no_interrupt_for_it() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let (handle, cli, child) = lingering_pi(&fake).await;
    handle.send(SessionCommand::turn("hi")).await.unwrap();
    fake.sent_lines_eventually(2).await;
    handle.send(SessionCommand::Shutdown).await.unwrap();
    drop(handle);

    let grace = unharness::core::process::END_GRACE;
    assert!(gone_within(cli, grace / 2).await);
    assert!(gone_within(child, Duration::from_secs(5)).await);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn ending_a_session_between_turns_gives_the_cli_its_grace() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let (handle, cli, child) = lingering_pi(&fake).await;
    fake.sent_lines_eventually(1).await;
    handle.send(SessionCommand::Shutdown).await.unwrap();
    drop(handle);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(running(cli));
    let grace = unharness::core::process::END_GRACE;
    assert!(gone_within(cli, grace + Duration::from_secs(5)).await);
    assert!(gone_within(child, Duration::from_secs(5)).await);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn ending_a_session_kills_what_its_cli_left_in_a_session_of_its_own() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let (handle, pids) = lingering_claude(&fake, "session", "60");
    let (cli, child) = background_pids(&pids).await;
    // As `/clear` and a switch do: the handle goes with the request.
    handle.send(SessionCommand::Shutdown).await.unwrap();
    drop(handle);

    let sent = fake.sent_lines_eventually(2).await;
    assert!(
        sent.iter().any(|l| l["request"]["subtype"] == "interrupt"),
        "{sent:?}"
    );
    // Given its grace, not killed with the handle.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(running(cli));
    let grace = unharness::core::process::END_GRACE;
    assert!(gone_within(cli, grace + Duration::from_secs(5)).await);
    assert!(gone_within(child, Duration::from_secs(5)).await);
    assert!(!fake.sent_lines().iter().any(|l| l["ended"] == true));
}

// The driver is still sending an event when the handle goes: it finds
// the `Shutdown` queued behind it instead of killing.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_shutdown_queued_behind_an_event_still_ends_the_cli_gracefully() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let delta =
        std::fs::read_to_string(repo().join("src/harness/claude/fixtures/basic_turn.jsonl"))
            .unwrap()
            .lines()
            .find(|l| l.contains("text_delta"))
            .unwrap()
            .to_string();
    let fixture = fake._tmp.path().join("chatty.jsonl");
    let chatty = vec![delta; unharness::core::session::EVENT_CHANNEL_CAPACITY + 100];
    std::fs::write(&fixture, chatty.join("\n") + "\n").unwrap();
    let (handle, pids) = lingering_claude_on(&fake, &fixture, "session", "60");
    let (cli, child) = background_pids(&pids).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.events.len() < unharness::core::session::EVENT_CHANNEL_CAPACITY {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the events never filled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.send(SessionCommand::Shutdown).await.unwrap();
    drop(handle);

    let sent = fake.sent_lines_eventually(2).await;
    assert!(
        sent.iter().any(|l| l["request"]["subtype"] == "interrupt"),
        "{sent:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(running(cli));
    let grace = unharness::core::process::END_GRACE;
    assert!(gone_within(cli, grace + Duration::from_secs(5)).await);
    assert!(gone_within(child, Duration::from_secs(5)).await);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cli_ending_by_itself_is_not_cut_short_and_its_group_goes_with_it() {
    if !python_available() {
        eprintln!("python3 not available; skipping");
        return;
    }
    let fake = Fake::new();
    let (handle, pids) = lingering_claude(&fake, "group", "0.5");
    let (cli, child) = background_pids(&pids).await;
    handle.send(SessionCommand::Shutdown).await.unwrap();
    drop(handle);

    // It finished what it was doing after the handle was gone.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !fake.sent_lines().iter().any(|l| l["ended"] == true) {
        assert!(tokio::time::Instant::now() < deadline, "cut short");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone_within(cli, Duration::from_secs(5)).await);
    // What it left in its process group did not outlive it.
    assert!(gone_within(child, Duration::from_secs(5)).await);
}

/// pi with its gate extension installed in a scratch directory.
fn pi_harness() -> unharness::harness::pi::PiHarness {
    unharness::harness::pi::PiHarness {
        gate_dir: Some(tempfile::tempdir().unwrap().keep()),
    }
}

#[tokio::test]
async fn pi_gate_asks_before_a_tool_acts() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/gate.jsonl");
    let mut handle = pi_harness()
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle
        .send(SessionCommand::turn("read, touch, write"))
        .await
        .unwrap();
    // As recorded: the command is allowed, the write is not.
    let events = run_turn(&mut handle, |ev| match ev {
        AgentEvent::PermissionRequest(req) => match &req.kind {
            PermissionKind::ToolUse { tool, .. } if tool == "bash" => {
                Some(PermissionDecision::Allow {
                    updated_input: None,
                })
            }
            _ => Some(PermissionDecision::Deny {
                reason: "no".into(),
            }),
        },
        _ => None,
    })
    .await;
    let asked: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::PermissionRequest(req) => match &req.kind {
                PermissionKind::ToolUse { tool, action, .. } if req.tool_call_id.is_some() => {
                    Some(format!("{tool}: {}", action.summary()))
                }
                other => panic!("{other:?}"),
            },
            _ => None,
        })
        .collect();
    // The read before them was not asked about.
    assert_eq!(
        asked,
        ["bash: shell \"touch touched.txt\"", "write: edit made.txt"]
    );
    let answers: Vec<Value> = fake
        .sent_lines()
        .into_iter()
        .filter(|v| v["type"] == "extension_ui_response")
        .map(|v| v["value"].clone())
        .collect();
    assert_eq!(answers, ["Allow", "Deny"]);
}

#[tokio::test]
async fn pi_gate_is_answered_without_asking_under_bypass() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/gate.jsonl");
    let mut handle = pi_harness()
        .start_session(fake.config(&fixture, PermissionPolicy::Bypass, true))
        .unwrap();
    handle
        .send(SessionCommand::turn("read, touch, write"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionRequest(_))),
        "{events:?}"
    );
    let answers: Vec<Value> = fake
        .sent_lines()
        .into_iter()
        .filter(|v| v["type"] == "extension_ui_response")
        .map(|v| v["value"].clone())
        .collect();
    assert_eq!(answers, ["Allow", "Allow"]);
}

#[tokio::test]
async fn pi_rpc_session_streams_tool_and_text() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/basic_and_bash.jsonl");
    let harness = pi_harness();
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    // The driver sends get_state and get_commands first; the fixture then
    // replays their answers before the first prompt.
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionStarted { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CapabilitiesChanged(u) if u.commands.as_ref().is_some_and(|c| c.len() == 3)
    )));
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
    assert_eq!(sent[1]["type"], "get_commands");
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

/// A thread says which provider it runs on; a chosen one goes on the
/// command line, and a thread on another says so.
#[tokio::test]
async fn codex_app_server_reports_its_provider() {
    if !python_available() {
        return;
    }
    let fixture = repo().join("src/harness/codex/fixtures/app_server_two_turns.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    );
    for (chosen, error) in [
        (Some("llama"), true),
        (Some("openai"), false),
        (None, false),
    ] {
        let fake = Fake::new();
        let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
        cfg.provider = chosen.map(Into::into);
        let mut handle = harness.start_session(cfg).unwrap();
        handle.send(SessionCommand::turn("pong?")).await.unwrap();
        let events = run_turn(&mut handle, |_| None).await;
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::CapabilitiesChanged(u) if u.provider == Some("openai".into())
        )));
        let reported = events.iter().any(
            |e| matches!(e, AgentEvent::Error(m) if m.starts_with("Codex runs on openai, not llama")),
        );
        assert_eq!(reported, error, "{chosen:?}: {events:?}");
        handle.send(SessionCommand::Shutdown).await.unwrap();
    }
}

#[tokio::test]
async fn codex_app_server_asks_before_an_mcp_tool_call() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_mcp_server.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    );
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle.send(SessionCommand::turn("call it")).await.unwrap();
    let events = run_turn(&mut handle, |ev| match ev {
        AgentEvent::PermissionRequest(_) => Some(PermissionDecision::Allow {
            updated_input: None,
        }),
        _ => None,
    })
    .await;
    // The server that could not start is named, once.
    let failures = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Notice(n) if n.contains("`broken` failed to start")))
        .count();
    assert_eq!(failures, 1);
    // Codex asks through an elicitation; it is a tool permission to the
    // user, attached to the call it is about.
    let call = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallStarted { id, name, .. } if name == "probe/magic_word" => Some(id),
            _ => None,
        })
        .expect("the MCP tool call");
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::PermissionRequest(r)
            if r.tool_call_id.as_ref() == Some(call)
                && matches!(&r.kind, PermissionKind::ToolUse { tool, .. } if tool == "probe/magic_word")
    )));
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolCallResult { output, is_error: false, .. } if output.contains("zanzibar argA"))
    ));

    let answer = fake
        .sent_lines()
        .into_iter()
        .find(|v| v["id"] == 0 && v.get("result").is_some())
        .expect("approval response");
    assert_eq!(
        answer["result"],
        serde_json::json!({"action": "accept", "content": {}})
    );

    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn codex_app_server_steers_the_running_turn_and_compacts() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_steer_and_compact.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    );
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle.send(SessionCommand::turn("run it")).await.unwrap();
    // Steer once the command has started, as a user would mid-turn.
    loop {
        if matches!(
            next_event(&mut handle).await,
            AgentEvent::ToolCallStarted { .. }
        ) {
            break;
        }
    }
    handle
        .send(SessionCommand::Steer {
            text: "also say STEERED".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then_some(PermissionDecision::Allow {
            updated_input: None,
        })
    })
    .await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.ends_with("done STEERED"), "{text}");

    // Compaction runs as a turn of its own.
    handle
        .send(SessionCommand::Compact { instructions: None })
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(n) if n == "context compacted"))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));

    let sent = fake.sent_lines();
    let turn_start = sent
        .iter()
        .position(|v| v["method"] == "turn/start")
        .unwrap();
    let steer = &sent[turn_start + 1];
    assert_eq!(steer["method"], "turn/steer");
    assert_eq!(steer["params"]["input"][0]["text"], "also say STEERED");
    // The turn id comes from the fixture's turn/started notification.
    assert!(
        steer["params"]["expectedTurnId"]
            .as_str()
            .is_some_and(|id| id.starts_with("01a103c7"))
    );
    assert!(sent.iter().any(|v| v["method"] == "thread/compact/start"));

    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn codex_sub_agent_events_are_attributed_and_do_not_end_the_turn() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_subagent.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    );
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle.send(SessionCommand::turn("delegate")).await.unwrap();
    let mut events = Vec::new();
    loop {
        let ev = next_event(&mut handle).await;
        // The sub-agent asks to run a command; its request arrives wrapped.
        if let AgentEvent::Sub { event, .. } = &ev
            && let AgentEvent::PermissionRequest(req) = event.as_ref()
        {
            handle
                .send(SessionCommand::RespondPermission {
                    id: req.id.clone(),
                    decision: PermissionDecision::Allow {
                        updated_input: None,
                    },
                })
                .await
                .unwrap();
        }
        let done = matches!(ev, AgentEvent::TurnCompleted { .. });
        events.push(ev);
        if done {
            break;
        }
    }

    // The sub-agent's own turn/completed did not end ours: the main agent's
    // answer comes after the sub-agent's work.
    let spawn = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallStarted { id, name, .. } if name == "agent" => Some(id.clone()),
            _ => None,
        })
        .expect("sub-agent announced");
    let sub_tool = events.iter().position(|e| {
        matches!(e, AgentEvent::Sub { parent, event }
            if *parent == spawn && matches!(event.as_ref(), AgentEvent::ToolCallStarted { name, .. } if name == "shell"))
    });
    let answer = events
        .iter()
        .rposition(|e| matches!(e, AgentEvent::TextDelta(t) if t == "alpha"));
    assert!(sub_tool.is_some() && answer > sub_tool);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
            .count(),
        1
    );

    let sent = fake.sent_lines();
    assert!(
        sent.iter()
            .any(|v| v["id"] == 0 && v["result"]["decision"] == "accept")
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// Read events until the subagent ends; returns how.
async fn subagent_end(handle: &mut SessionHandle) -> unharness::core::SubagentStatus {
    loop {
        if let AgentEvent::SubagentEnded { status, .. } = next_event(handle).await {
            return status;
        }
    }
}

/// The tool call that spawned the subagent announced in `events`.
fn spawned(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubagentStarted { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("subagent announced")
}

#[tokio::test]
async fn claude_stops_the_chosen_subagent_by_its_task() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/subagent_stop_task.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness::default();
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle.send(SessionCommand::turn("delegate")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    // The turn is over and the subagent it launched is not.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubagentEnded { .. }))
    );
    let id = spawned(&events);

    handle
        .send(SessionCommand::StopSubagent { id })
        .await
        .unwrap();
    assert_eq!(
        subagent_end(&mut handle).await,
        unharness::core::SubagentStatus::Cancelled
    );
    // The request names the task, which is not the tool call's id.
    let sent = fake.sent_lines();
    let stop = &sent.last().unwrap()["request"];
    assert_eq!(stop["subtype"], "stop_task");
    assert!(stop["task_id"].is_string());
    // Claude then reports the stop in a turn of its own.
    let events = run_turn(&mut handle, |_| None).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn codex_stops_the_chosen_sub_agent_on_its_own_thread() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_subagent_interrupted.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    );
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle.send(SessionCommand::turn("delegate")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let id = spawned(&events);

    handle
        .send(SessionCommand::StopSubagent { id })
        .await
        .unwrap();
    assert_eq!(
        subagent_end(&mut handle).await,
        unharness::core::SubagentStatus::Cancelled
    );
    // The interrupt names the sub-agent's thread and turn, not the main one's.
    let sent = fake.sent_lines();
    let main_thread = sent
        .iter()
        .find(|v| v["method"] == "turn/start")
        .map(|v| v["params"]["threadId"].clone())
        .unwrap();
    let interrupt = sent
        .iter()
        .find(|v| v["method"] == "turn/interrupt")
        .expect("turn/interrupt sent");
    assert!(interrupt["params"]["threadId"].is_string());
    assert_ne!(interrupt["params"]["threadId"], main_thread);
    assert!(interrupt["params"]["turnId"].is_string());
    handle.send(SessionCommand::Shutdown).await.unwrap();
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
async fn agy_stream_session_runs_two_turns() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/agy/fixtures/turn.jsonl");
    let harness = unharness::harness::agy::AgyHarness;
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::AcceptEdits, true))
        .unwrap();

    // The first turn ends where agy refused a command it could not ask about.
    handle.send(SessionCommand::turn("echo hi")).await.unwrap();
    let mut events = run_turn(&mut handle, |_| None).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id == "7d61bda4-46b7-4d1c-9764-809ad9b86fda")));
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolCallResult { output, .. } if output == "2 lines, 17 bytes")
    ));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));
    // The refusal is on stderr, a pipe of its own, so it can land on either
    // side of the result, and whichever comes first gives the one notice
    // (only the result's names `RunCommand`); the fake sends nothing more
    // until the next turn.
    let refusal =
        |e: &AgentEvent| matches!(e, AgentEvent::Notice(n) if n.starts_with("agy refused"));
    if !events.iter().any(refusal) {
        events.push(next_event(&mut handle).await);
    }
    assert!(events.iter().any(refusal));

    // The second result lists that refusal again; it is not reported twice.
    handle.send(SessionCommand::turn("write")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Notice(_))));
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::ToolCallStarted { name, .. } if name == "write_to_file")
        )
    );

    let sent = fake.sent_lines();
    assert_eq!(sent[0]["event"], "user");
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
    let harness = unharness::harness::agy::AgyHarness;
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::AcceptEdits, false))
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

fn acp_harness() -> unharness::harness::acp::AcpHarness {
    unharness::harness::acp::AcpHarness::new(
        "fake-acp",
        None,
        &[fake_harness().to_string_lossy().into_owned()],
    )
    .unwrap()
    .asking_permission(true)
}

#[tokio::test]
async fn acp_handshake_turns_and_permission_round_trip() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/claude_agent_acp.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let mut handle = acp_harness().start_session(cfg).unwrap();

    // Sent before the handshake finishes: the driver holds it until the
    // session exists.
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id.starts_with("201664ad"))
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "pong"))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));

    handle.send(SessionCommand::turn("write it")).await.unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then_some(PermissionDecision::Allow {
            updated_input: None,
        })
    })
    .await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolCallStarted { name, input, .. } if name == "Write" && input["content"] == "hello\n")
    ));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallResult {
            is_error: false,
            ..
        }
    )));

    let sent = fake.sent_lines();
    assert_eq!(sent[0]["method"], "initialize");
    assert_eq!(sent[0]["params"]["protocolVersion"], 1);
    assert_eq!(sent[1]["method"], "session/new");
    assert_eq!(sent[1]["params"]["mcpServers"], serde_json::json!([]));
    assert_eq!(sent[2]["method"], "session/prompt");
    assert_eq!(sent[2]["params"]["prompt"][0]["text"], "pong?");
    assert!(
        sent[2]["params"]["sessionId"]
            .as_str()
            .is_some_and(|id| id.starts_with("201664ad"))
    );
    let answer = sent
        .iter()
        .find(|v| v.get("result").is_some())
        .expect("permission answer");
    assert_eq!(answer["id"], 0);
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow-once");

    handle.send(SessionCommand::Shutdown).await.unwrap();
    assert!(matches!(
        next_event(&mut handle).await,
        AgentEvent::ProcessExited { .. }
    ));
}

#[tokio::test]
async fn acp_session_gets_the_mcp_servers_and_runs_their_tools() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/claude_agent_acp_mcp.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    // The server of the recording, and an http one the agent also takes.
    cfg.mcp_servers = vec![
        McpServer {
            name: "probe".into(),
            transport: McpTransport::Stdio {
                command: "/usr/bin/python3".into(),
                args: vec!["/WORKSPACE/probe_mcp.py".into(), "argA".into()],
                env: [("PROBE_WORD".to_string(), "zanzibar".to_string())].into(),
            },
        },
        McpServer {
            name: "docs".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: Default::default(),
            },
        },
    ];
    let mut handle = acp_harness().start_session(cfg).unwrap();

    handle.send(SessionCommand::turn("call it")).await.unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then_some(PermissionDecision::Allow {
            updated_input: None,
        })
    })
    .await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallResult { output, is_error: false, .. } if output == "zanzibar argA"
    )));

    // What the recorder sent for the probe server is what the driver sends.
    let recorded: Value = std::fs::read_to_string(&fixture)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.strip_prefix(">> ")?).ok())
        .find(|v| v["method"] == "session/new")
        .expect("the recorded session/new");
    let sent = fake.sent_lines();
    let servers = &sent[1]["params"]["mcpServers"];
    assert_eq!(sent[1]["method"], "session/new");
    assert_eq!(servers[0], recorded["params"]["mcpServers"][0]);
    // The agent announced `mcpCapabilities.http`, so the http one goes along.
    assert_eq!(
        servers[1],
        serde_json::json!({"type": "http", "name": "docs", "url": "https://example.com/mcp", "headers": []})
    );

    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn acp_bypass_answers_permission_requests_itself() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/claude_agent_acp.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Bypass, true);
    cfg.model = None;
    let mut handle = acp_harness().start_session(cfg).unwrap();

    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    run_turn(&mut handle, |_| None).await;
    handle.send(SessionCommand::turn("write it")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    // The tool call is shown, the question is not.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallStarted { name, .. } if name == "Write"))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionRequest(_)))
    );
    let sent = fake.sent_lines();
    assert!(
        sent.iter()
            .any(|v| v["result"]["outcome"]["optionId"] == "allow-once")
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn acp_resumes_a_session_and_applies_the_configured_model() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/claude_agent_acp_resume.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.resume = Some("201664ad-6914-49da-8f50-0e36f73b8d3b".into());
    let mut handle = acp_harness().start_session(cfg).unwrap();

    handle
        .send(SessionCommand::turn("what was it?"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::SessionStarted { .. }))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "hello"))
    );
    // The model switch drops the effort option: the capability update says so.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CapabilitiesChanged(u) if u.effort_levels.as_deref() == Some(&[][..])
    )));

    let sent = fake.sent_lines();
    assert_eq!(sent[1]["method"], "session/resume");
    assert_eq!(
        sent[1]["params"]["sessionId"],
        "201664ad-6914-49da-8f50-0e36f73b8d3b"
    );
    assert_eq!(sent[2]["method"], "session/set_config_option");
    assert_eq!(sent[2]["params"]["configId"], "model");
    assert_eq!(sent[2]["params"]["value"], "haiku");
    assert_eq!(sent[3]["method"], "session/prompt");
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn pi_rewind_forks_and_holds_the_next_turn_until_the_fork_is_in_place() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/rewind.jsonl");
    let harness = pi_harness();
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    // Each turn is followed by its anchor: pi lists the user messages (and
    // their entry ids) once the turn has settled.
    let mut anchors = Vec::new();
    for prompt in ["alpha", "beta"] {
        handle.send(SessionCommand::turn(prompt)).await.unwrap();
        run_turn(&mut handle, |_| None).await;
        loop {
            if let AgentEvent::TurnAnchor { id } = next_event(&mut handle).await {
                anchors.push(id);
                break;
            }
        }
    }
    assert_eq!(anchors.len(), 2);

    // Rewind to before the second turn, and send the next prompt at once:
    // the driver must not pass it on before the fork has answered.
    handle
        .send(SessionCommand::Rewind {
            anchor: anchors[1].clone(),
        })
        .await
        .unwrap();
    handle.send(SessionCommand::turn("which?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "alpha"))
    );
    // The fork is a new pi session.
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id.starts_with("01a103de"))
    ));

    let sent = fake.sent_lines();
    let pos = |pred: &dyn Fn(&Value) -> bool| sent.iter().position(pred).unwrap();
    let fork = pos(&|v| v["type"] == "fork");
    assert_eq!(sent[fork]["entryId"], anchors[1].as_str());
    let prompt = pos(&|v| v["type"] == "prompt" && v["message"] == "which?");
    let state_after_fork = sent
        .iter()
        .skip(fork)
        .position(|v| v["type"] == "get_state")
        .map(|i| i + fork)
        .unwrap();
    assert!(fork < state_after_fork && state_after_fork < prompt);

    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn codex_fork_branches_the_thread_instead_of_resuming_it() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_fork.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    );
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.resume = Some("01a103e9-859f-7680-ac4c-56b3e6512414".into());
    cfg.fork = true;
    // A chosen provider goes with the branch, as with a resumed thread.
    cfg.provider = Some("openai".into());
    let mut handle = harness.start_session(cfg).unwrap();

    handle.send(SessionCommand::turn("which?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    // The branch is a thread of its own, and turns go to it.
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id.starts_with("01a103fc"))
    ));
    let sent = fake.sent_lines();
    assert_eq!(sent[2]["method"], "thread/fork");
    assert_eq!(
        sent[2]["params"]["threadId"],
        "01a103e9-859f-7680-ac4c-56b3e6512414"
    );
    assert_eq!(sent[2]["params"]["modelProvider"], "openai");
    assert_eq!(sent[2]["params"]["excludeTurns"], true);
    let turn = sent.iter().find(|v| v["method"] == "turn/start").unwrap();
    assert!(
        turn["params"]["threadId"]
            .as_str()
            .is_some_and(|t| t.starts_with("01a103fc"))
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A workspace, a harness state directory holding the fake's log, a
/// directory outside both and a home with credentials, plus the sandbox
/// that confines a fake harness to them.
struct Confined {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    sandbox: unharness::core::Sandbox,
}

impl Confined {
    /// `None` when this machine has no sandbox backend.
    fn new(level: unharness::core::SandboxLevel) -> Option<Self> {
        use unharness::core::sandbox::{self, SandboxEnv, SandboxPaths, SandboxRequest};
        let backend = sandbox::detect();
        if let Err(why) = &backend {
            eprintln!("no sandbox backend ({why}); skipping");
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["ws", "state", "outside", "home/.gnupg"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("home/.gnupg/id_key"), "secret").unwrap();
        std::fs::write(root.join("outside/notes"), "plain").unwrap();
        let sandbox = sandbox::resolve(
            &SandboxRequest {
                explicit: Some(level),
                default: level,
                workspace: &root.join("ws"),
                harness: &SandboxPaths {
                    writable: vec![root.join("state")],
                },
                extra_writable: &[],
                extra_readable: &[],
                extra_deny_read: &[],
            },
            &backend,
            &SandboxEnv {
                home: Some(root.join("home")),
                scratch: vec![PathBuf::from("/dev")],
                protected: vec![],
            },
        )
        .unwrap();
        Some(Confined {
            _tmp: tmp,
            root,
            sandbox,
        })
    }

    fn config(&self, fixture: &Path, hang: bool) -> SessionConfig {
        let at = |p: &str| self.root.join(p).display().to_string();
        let mut env = vec![
            (
                "UNHARNESS_FAKE_FIXTURE".to_string(),
                fixture.to_string_lossy().to_string(),
            ),
            ("UNHARNESS_FAKE_LOG".to_string(), at("state/sent.log")),
            (
                "UNHARNESS_FAKE_PROBE".to_string(),
                format!(
                    "write:{};write:{};read:{};read:{}",
                    at("ws/new"),
                    at("outside/new"),
                    at("outside/notes"),
                    at("home/.gnupg/id_key")
                ),
            ),
        ];
        if hang {
            env.push(("UNHARNESS_FAKE_HANG".to_string(), "1".to_string()));
        }
        SessionConfig {
            binary: fake_harness(),
            cwd: self.root.join("ws"),
            model: Some(ModelRef::new(HarnessId::CLAUDE, "anthropic", "haiku")),
            provider: None,
            effort: None,
            policy: PermissionPolicy::AcceptEdits,
            resume: None,
            fork: false,
            extra_args: vec![],
            env,
            mcp_servers: Vec::new(),
            sandbox: self.sandbox.clone(),
        }
    }

    /// What the fake managed to do, in order: (kind, file name, ok).
    fn probes(&self) -> Vec<(String, String, bool)> {
        std::fs::read_to_string(self.root.join("state/sent.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| {
                let path = Path::new(v["path"].as_str()?);
                Some((
                    v["probe"].as_str()?.to_string(),
                    path.file_name()?.to_string_lossy().into_owned(),
                    v["ok"].as_bool()?,
                ))
            })
            .collect()
    }
}

fn probe_results(ws_write: bool) -> Vec<(String, String, bool)> {
    [
        ("write", "new", ws_write),
        ("write", "new", false),
        ("read", "notes", true),
        ("read", "id_key", false),
    ]
    .iter()
    .map(|(k, f, ok)| (k.to_string(), f.to_string(), *ok))
    .collect()
}

#[tokio::test]
async fn sandbox_confines_a_long_lived_harness() {
    use unharness::core::SandboxLevel;
    if !python_available() {
        return;
    }
    let Some(confined) = Confined::new(SandboxLevel::WorkspaceWrite) else {
        return;
    };
    let fixture = repo().join("src/harness/claude/fixtures/basic_turn.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness::default();
    let mut handle = harness
        .start_session(confined.config(&fixture, true))
        .unwrap();
    handle.send(SessionCommand::turn("run echo")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));

    // The workspace is writable and the rest readable, except credentials.
    assert_eq!(confined.probes(), probe_results(true));
    assert!(confined.root.join("ws/new").exists());
    assert!(!confined.root.join("outside/new").exists());

    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn sandbox_confines_every_turn_of_a_per_turn_harness() {
    use unharness::core::SandboxLevel;
    if !python_available() {
        return;
    }
    let Some(confined) = Confined::new(SandboxLevel::ReadOnly) else {
        return;
    };
    let fixture = repo().join("src/harness/codex/fixtures/exec_resume_command.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::Exec,
    );
    let mut handle = harness
        .start_session(confined.config(&fixture, false))
        .unwrap();
    for prompt in ["one", "two"] {
        handle.send(SessionCommand::turn(prompt)).await.unwrap();
        let events = run_turn(&mut handle, |_| None).await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                stop_reason: StopReason::Done
            })
        ));
    }

    // Read-only: not even the workspace, and each turn's process is confined.
    let twice = [probe_results(false), probe_results(false)].concat();
    assert_eq!(confined.probes(), twice);
    assert!(!confined.root.join("ws/new").exists());

    handle.send(SessionCommand::Shutdown).await.unwrap();
}

fn headless_run(
    fake: &Fake,
    format: unharness::headless::Format,
    report_turn: bool,
) -> unharness::headless::Headless<Vec<u8>, Vec<u8>> {
    let rules =
        unharness::core::Rules::load_in(&fake._tmp.path().join("config"), Some(fake._tmp.path()))
            .unwrap();
    let mut run = unharness::headless::Headless::new(
        format,
        rules,
        fake._tmp.path().to_path_buf(),
        "fake",
        report_turn,
        Vec::new(),
        Vec::new(),
    );
    run.begin(&unharness::headless::RunInfo {
        harness: "fake".into(),
        policy: "ask".into(),
        sandbox: "off".into(),
        cwd: fake._tmp.path().to_path_buf(),
    });
    run
}

#[tokio::test]
async fn headless_denies_what_no_rule_allows_and_ends_with_the_turn() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/permission_and_question.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness::default();
    let handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
    let mut run = headless_run(&fake, unharness::headless::Format::Json, true);
    tokio::time::timeout(
        Duration::from_secs(10),
        unharness::headless::drive(handle, "create spike3.txt".into(), &mut run),
    )
    .await
    .expect("the run ends");
    assert_eq!(run.finish(), 0);

    let result: Value = serde_json::from_slice(&run.out).unwrap();
    assert_eq!(result["status"], "done");
    assert_eq!(result["turns"], 1);
    assert_eq!(result["denied"][0]["tool"], "Write");
    assert_eq!(result["denied"][0]["action"]["kind"], "edit");
    let sent = fake.sent_lines();
    let responses: Vec<&Value> = sent
        .iter()
        .filter(|v| v["type"] == "control_response")
        .collect();
    assert_eq!(responses.len(), 1, "{sent:?}");
    assert_eq!(responses[0]["response"]["response"]["behavior"], "deny");
    // One prompt, and the session is shut down after it.
    assert_eq!(sent.iter().filter(|v| v["type"] == "user").count(), 1);
}

#[tokio::test]
async fn headless_runs_an_acp_agent() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/claude_agent_acp.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let handle = acp_harness().start_session(cfg).unwrap();
    let mut run = headless_run(&fake, unharness::headless::Format::StreamJson, false);
    tokio::time::timeout(
        Duration::from_secs(10),
        unharness::headless::drive(handle, "pong?".into(), &mut run),
    )
    .await
    .expect("the run ends");
    assert_eq!(run.finish(), 0);

    let lines: Vec<Value> = String::from_utf8_lossy(&run.out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "start");
    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "text_delta" && l["text"] == "pong")
    );
    assert!(lines.iter().any(|l| l["type"] == "process_exited"));
    let result = lines.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["text"], "pong");
    assert!(
        result["session_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("201664ad"))
    );
}
