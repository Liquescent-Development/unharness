//! End-to-end session tests against `scripts/fake-harness.py`, which replays
//! recorded vendor output and blocks wherever the recording shows the client
//! sending a line. This exercises the real transport drivers (process
//! spawning, stdin writes, parsing, permission round-trips, shutdown).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use unharness::core::{
    AgentEvent, CapsUpdate, HarnessId, Kept, McpServer, McpTransport, ModelRef, PermissionDecision,
    PermissionKind, PermissionPolicy, PermissionRequest, RemoteControl, SessionCommand,
    SessionConfig, SessionHandle, StopReason, ToolAction,
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
    /// vibe's home for a session that names none of its own, so that a
    /// vibe harness never reads the developer's `~/.vibe` (#219).
    vibe_home: tempfile::TempDir,
    log: PathBuf,
}

impl Fake {
    fn new() -> Self {
        // The CLIs run below a reaper, as unharness runs them.
        unharness::core::reaper::enable_with(PathBuf::from(env!("CARGO_BIN_EXE_unharness")));
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("sent.log");
        Fake {
            _tmp: tmp,
            vibe_home: tempfile::tempdir().unwrap(),
            log,
        }
    }

    /// Where a vibe harness writes its profiles in a test.
    fn profiles(&self) -> PathBuf {
        self._tmp.path().join("uh-profiles")
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
            (
                "VIBE_HOME".to_string(),
                self.vibe_home.path().to_string_lossy().to_string(),
            ),
        ];
        if hang {
            env.push(("UNHARNESS_FAKE_HANG".to_string(), "1".to_string()));
        }
        SessionConfig {
            binary: fake_harness(),
            cwd: self._tmp.path().to_path_buf(),
            workspace_root: None,
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

/// What the fake logged, a word each: its environment and arguments as it
/// started (`env …`, `argv`), a control request by its subtype (with `on`
/// or `off` for Remote Control), a JSON-RPC request by its method, `user`
/// for a turn, `ended` once its input closed.
fn logged(fake: &Fake) -> Vec<String> {
    fake.sent_lines()
        .iter()
        .map(|v| {
            if let Some(env) = v.get("env") {
                format!("env {env}")
            } else if v.get("argv").is_some() {
                "argv".into()
            } else if v.get("ended").is_some() {
                "ended".into()
            } else if let Some(subtype) = v.pointer("/request/subtype").and_then(Value::as_str) {
                match v.pointer("/request/enabled").and_then(Value::as_bool) {
                    Some(true) => format!("{subtype} on"),
                    Some(false) => format!("{subtype} off"),
                    None => subtype.to_string(),
                }
            } else if let Some(method) = v["method"].as_str() {
                method.to_string()
            } else {
                v["type"].as_str().unwrap_or("?").to_string()
            }
        })
        .collect()
}

/// The arguments each process of the fake was started with.
fn argvs(fake: &Fake) -> Vec<Vec<String>> {
    fake.sent_lines()
        .iter()
        .filter_map(|v| v.get("argv")?.as_array().cloned())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .collect()
}

/// Claude takes its provider from its environment: its driver ends the
/// process as a shutdown does (`interrupt`, then its input closed), starts
/// it again once it is gone, resuming the session on the new provider with
/// no model, greets it as a session's and puts it back on Remote Control,
/// and tells the TUI `Restarted` (#275: before, the TUI ended the session
/// and started it again, which sent the same lines).
#[tokio::test]
async fn claude_starts_again_on_a_new_provider_resumed_and_back_on_remote_control() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/remote_control_prompt.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.env.extend([
        ("UNHARNESS_FAKE_ARGV".to_string(), "1".to_string()),
        (
            "UNHARNESS_FAKE_ENV".to_string(),
            "CLAUDE_CODE_USE_BEDROCK".to_string(),
        ),
        ("UNHARNESS_FAKE_LINGER".to_string(), "0.2".to_string()),
    ]);
    let mut handle = unharness::harness::claude::ClaudeHarness::default()
        .start_session(cfg)
        .unwrap();
    handle
        .send(SessionCommand::RemoteControl {
            enabled: true,
            name: Some("unharness-rec".into()),
        })
        .await
        .unwrap();
    handle
        .send(SessionCommand::turn("Reply with the single word: ready."))
        .await
        .unwrap();
    // The local turn, then the remote one.
    let mut turns = 0;
    while turns < 2 {
        if matches!(
            next_event(&mut handle).await,
            AgentEvent::TurnCompleted { .. }
        ) {
            turns += 1;
        }
    }
    handle
        .send(SessionCommand::SetProvider("bedrock".into()))
        .await
        .unwrap();
    while next_event(&mut handle).await != AgentEvent::Restarted {}
    let sent = fake.sent_lines_eventually(11).await;
    assert_eq!(
        logged(&fake),
        [
            r#"env {"CLAUDE_CODE_USE_BEDROCK":null}"#,
            "argv",
            "initialize",
            "remote_control on",
            "user",
            "interrupt",
            "ended",
            r#"env {"CLAUDE_CODE_USE_BEDROCK":"1"}"#,
            "argv",
            "initialize",
            "remote_control on",
        ],
        "{sent:?}"
    );
    let argvs = argvs(&fake);
    let has = |argv: &[String], flag: &str| argv.iter().any(|a| a == flag);
    assert!(has(&argvs[0], "--model") && !has(&argvs[0], "--resume"));
    let resume = argvs[1].iter().position(|a| a == "--resume").unwrap();
    assert_eq!(argvs[1][resume + 1], "4d8c8163-a279-4dd8-8d61-ed5ec4f6c939");
    assert!(!has(&argvs[1], "--model") && !has(&argvs[1], "--fork-session"));
    let named = sent
        .iter()
        .filter(|v| v.pointer("/request/name") == Some(&serde_json::json!("unharness-rec")))
        .count();
    assert_eq!(named, 2);
    shut_down(&mut handle).await;
}

/// A provider Claude does not have: no process can be started on it, so
/// the session ends as a shutdown ends it (`interrupt`, its input closed,
/// its grace), never killed, and the TUI is told why and `ProcessExited`.
#[tokio::test]
async fn claude_ends_as_a_shutdown_does_on_a_provider_it_cannot_start_on() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/provider_vertex.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.env
        .push(("UNHARNESS_FAKE_LINGER".to_string(), "0.2".to_string()));
    let mut handle = unharness::harness::claude::ClaudeHarness::default()
        .start_session(cfg)
        .unwrap();
    handle.send(SessionCommand::turn("hi")).await.unwrap();
    run_turn(&mut handle, |_| None).await;
    handle
        .send(SessionCommand::SetProvider("nowhere".into()))
        .await
        .unwrap();
    let mut events = Vec::new();
    loop {
        let ev = next_event(&mut handle).await;
        let exited = matches!(ev, AgentEvent::ProcessExited { .. });
        events.push(ev);
        if exited {
            break;
        }
    }
    assert!(
        matches!(&events[..], [.., AgentEvent::Error(e), AgentEvent::ProcessExited { .. }]
            if e.starts_with("could not start claude again: ")),
        "{events:?}"
    );
    // Its input closed after `interrupt`, it finished by itself.
    assert_eq!(
        logged(&fake)[2..],
        ["interrupt".to_string(), "ended".to_string()],
        "{:?}",
        logged(&fake)
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
    let handle = pi_harness(fake).start_session(cfg).unwrap();
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

/// pi with its gate extension installed in `fake`'s scratch directory,
/// which goes with it.
fn pi_harness(fake: &Fake) -> unharness::harness::pi::PiHarness {
    unharness::harness::pi::PiHarness {
        gate_dir: Some(fake._tmp.path().join("pi-gate")),
    }
}

/// Once a compaction has ended, pi is asked for its session's entries
/// from the last prompt it listed on, which say which turn the entry it
/// kept from is in (`fixtures/compaction_split.jsonl`, pi 1.1.0: a cut
/// inside the second turn).
#[tokio::test]
async fn pi_asks_for_its_entries_once_a_compaction_ends() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/compaction_split.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let mut handle = pi_harness(&fake).start_session(cfg).unwrap();
    let mut anchors = Vec::new();
    for prompt in ["one", "two"] {
        handle.send(SessionCommand::turn(prompt)).await.unwrap();
        run_turn(&mut handle, |_| None).await;
        loop {
            if let AgentEvent::TurnAnchor { id } = next_event(&mut handle).await {
                anchors.push(id);
                break;
            }
        }
    }
    handle
        .send(SessionCommand::Compact { instructions: None })
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::Compacted(c) if c.kept == Kept::From("9a7cbc9e".into()))
        )
    );
    // pi answers with the entries once the next prompt went (recorded so).
    handle.send(SessionCommand::turn("three")).await.unwrap();
    let kept = loop {
        if let AgentEvent::CompactionKept(kept) = next_event(&mut handle).await {
            break kept;
        }
    };
    assert_eq!(kept, Kept::InTurnOf(vec![anchors[1].clone()]));

    let sent = fake.sent_lines();
    let pos = |kind: &str| sent.iter().position(|v| v["type"] == kind).unwrap();
    assert!(pos("compact") < pos("get_entries"));
    assert_eq!(sent[pos("get_entries")]["since"], anchors[1].as_str());
    assert_eq!(
        sent.iter().filter(|v| v["type"] == "get_entries").count(),
        1
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A resumed pi is asked how full its context is before any turn ends
/// (#355): a session whose last turn was a compaction answers `null`
/// (`fixtures/resume_compacted.jsonl`, pi 1.1.0), so the figure saved
/// from before it is dropped, until the next response tells one.
#[tokio::test]
async fn pi_asks_a_resumed_session_how_full_its_context_is() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/resume_compacted.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some("401bf0eb-8893-4cae-89d6-77f7c3f2efef".into());
    let mut handle = pi_harness(&fake).start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("three")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let unknown = events
        .iter()
        .position(|e| *e == AgentEvent::ContextUnknown)
        .expect("the fill from before the compaction is dropped");
    let told = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Context(c) if c.used == Some(1881)))
        .expect("the response tells the fill");
    assert!(unknown < told, "{events:?}");
    handle.send(SessionCommand::Shutdown).await.unwrap();
    until_exited(&mut handle).await;
    assert_eq!(
        fake.sent_lines()
            .iter()
            .filter_map(|v| v["type"].as_str())
            .take(5)
            .collect::<Vec<_>>(),
        [
            "get_state",
            "get_commands",
            "get_fork_messages",
            "get_session_stats",
            "prompt"
        ],
    );
}

/// A turn sent to a resumed pi waits for the first `get_fork_messages`
/// answer (#214): pi saves a prompt under the id it was given, so one sent
/// to the empty session pi made for a missing id
/// (`fixtures/resume_missing.jsonl`) would make that id resume next time.
/// There the turn fails unsent; where the listing shows the session, it
/// goes once the listing is in, and until then an interrupt drops it
/// without anything reaching pi.
#[tokio::test]
async fn pi_holds_a_resumed_turn_until_its_listing_answers() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/resume_missing.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some("3c0c2f6e-5b1a-4d2e-9f7a-1b2c3d4e5f62".into());
    let mut handle = pi_harness(&fake).start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("hi")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionReplaced { .. })),
        "{events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted { stop_reason: StopReason::Error(e) })
                if e.contains("this prompt was not sent")
        ),
        "{events:?}"
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
    until_exited(&mut handle).await;
    assert!(
        !fake.sent_lines().iter().any(|v| v["type"] == "prompt"),
        "{:?}",
        fake.sent_lines()
    );

    // The session is there: the turn goes after the listing.
    let fake = Fake::new();
    let fixture = fake._tmp.path().join("resumed.jsonl");
    std::fs::write(
        &fixture,
        concat!(
            ">> {\"type\": \"get_state\"}\n",
            ">> {\"type\": \"get_commands\"}\n",
            ">> {\"type\": \"get_fork_messages\"}\n",
            ">> {\"type\": \"get_session_stats\"}\n",
            "{\"id\":\"u3\",\"type\":\"response\",\"command\":\"get_fork_messages\",\"success\":true,",
            "\"data\":{\"messages\":[{\"entryId\":\"e1\",\"text\":\"earlier\"}]}}\n",
            ">> {\"type\": \"prompt\"}\n",
        ),
    )
    .unwrap();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some("s1".into());
    let handle = pi_harness(&fake).start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("hi")).await.unwrap();
    let sent = fake.sent_lines_eventually(5).await;
    assert_eq!(
        sent.iter()
            .filter_map(|v| v["type"].as_str())
            .collect::<Vec<_>>(),
        [
            "get_state",
            "get_commands",
            "get_fork_messages",
            "get_session_stats",
            "prompt"
        ],
    );
    assert_eq!(sent[4]["message"], "hi");
    handle.send(SessionCommand::Shutdown).await.unwrap();

    // Before the listing answers the turn is still held: an interrupt
    // ends it at once, and neither it nor an `abort` reaches pi (the fake
    // logs what it reads, so it reads everything once the gate opens).
    let fake = Fake::new();
    let gate = fake._tmp.path().join("gate");
    let fixture = fake._tmp.path().join("resumed_gated.jsonl");
    std::fs::write(
        &fixture,
        concat!(
            ">> {\"type\": \"get_state\"}\n",
            ">> {\"type\": \"get_commands\"}\n",
            ">> {\"type\": \"get_fork_messages\"}\n",
            ">> {\"type\": \"get_session_stats\"}\n",
            "# gate\n",
            "{\"id\":\"u3\",\"type\":\"response\",\"command\":\"get_fork_messages\",\"success\":true,",
            "\"data\":{\"messages\":[{\"entryId\":\"e1\",\"text\":\"earlier\"}]}}\n",
        ),
    )
    .unwrap();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some("s1".into());
    cfg.env.push((
        "UNHARNESS_FAKE_GATE".into(),
        gate.to_string_lossy().into_owned(),
    ));
    let mut handle = pi_harness(&fake).start_session(cfg).unwrap();
    fake.sent_lines_eventually(4).await;
    handle.send(SessionCommand::turn("hi")).await.unwrap();
    handle.send(SessionCommand::Interrupt).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                stop_reason: StopReason::Interrupted
            })
        ),
        "{events:?}"
    );
    std::fs::write(&gate, "").unwrap();
    // The listing comes, and nothing more is sent.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fake.sent_lines()
            .iter()
            .filter_map(|v| v["type"].as_str())
            .collect::<Vec<_>>(),
        [
            "get_state",
            "get_commands",
            "get_fork_messages",
            "get_session_stats"
        ],
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn pi_gate_asks_before_a_tool_acts() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/gate.jsonl");
    let mut handle = pi_harness(&fake)
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
async fn pi_interrupt_closes_an_open_gate_dialog() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    // As recorded on pi 1.0.4: `abort` while the gate's dialog is open.
    let fixture = repo().join("src/harness/pi/fixtures/gate_abort.jsonl");
    let mut handle = pi_harness(&fake)
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
    handle.send(SessionCommand::turn("echo hi")).await.unwrap();
    let mut asked = None;
    let mut after = Vec::new();
    loop {
        let ev = next_event(&mut handle).await;
        if let AgentEvent::PermissionRequest(req) = &ev {
            asked = Some(req.id.clone());
            handle.send(SessionCommand::Interrupt).await.unwrap();
            continue;
        }
        if asked.is_some() {
            after.push(ev.clone());
        }
        if matches!(ev, AgentEvent::TurnCompleted { .. }) {
            break;
        }
    }
    // The dialog is withdrawn before the turn ends, which says it was
    // interrupted; nothing answered it.
    let asked = asked.expect("the gate asked");
    let tail: Vec<&AgentEvent> = after
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::PermissionWithdrawn { .. } | AgentEvent::TurnCompleted { .. }
            )
        })
        .collect();
    assert_eq!(
        tail,
        [
            &AgentEvent::PermissionWithdrawn { id: asked },
            &AgentEvent::TurnCompleted {
                stop_reason: StopReason::Interrupted
            }
        ]
    );
    let sent: Vec<Value> = fake.sent_lines();
    assert!(sent.iter().any(|v| v["type"] == "abort"));
    assert!(!sent.iter().any(|v| v["type"] == "extension_ui_response"));
}

#[tokio::test]
async fn pi_gate_is_answered_without_asking_under_bypass() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/gate.jsonl");
    let mut handle = pi_harness(&fake)
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
    let harness = pi_harness(&fake);
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
    // The skills are asked for once there is a thread.
    assert_eq!(sent[3]["method"], "skills/list");
    assert_eq!(sent[3]["id"], "skills");
    assert_eq!(sent[4]["method"], "turn/start");
    assert_eq!(sent[4]["params"]["input"][0]["text"], "pong?");
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

/// Codex's skills are listed once there is a thread, and `/name` for one
/// goes as its own interface sends it: `$name` with the skill. A prompt
/// sent before the list came waits for it.
#[tokio::test]
async fn codex_app_server_lists_skills_and_runs_one_from_a_slash() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_skill.jsonl");
    let mut handle = codex_app_server()
        .start_session(fake.config(&fixture, PermissionPolicy::Bypass, true))
        .unwrap();
    handle
        .send(SessionCommand::turn("/pineapple hello there"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let listed = events.iter().find_map(|e| match e {
        AgentEvent::CapabilitiesChanged(CapsUpdate {
            commands: Some(c),
            slash_commands,
            ..
        }) => Some((c.clone(), *slash_commands)),
        _ => None,
    });
    let (commands, slash) = listed.expect("skills listed");
    assert_eq!(slash, Some(true));
    assert!(
        commands
            .iter()
            .any(|c| c.name == "superpowers:brainstorming")
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "42"))
    );

    let sent = fake.sent_lines();
    let turn = sent.iter().find(|v| v["method"] == "turn/start").unwrap();
    assert_eq!(
        turn["params"]["input"],
        serde_json::json!([
            {"type": "text", "text": "$pineapple hello there"},
            {"type": "skill", "name": "pineapple", "path": "/WORKSPACE/.agents/skills/pineapple/SKILL.md"},
        ])
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// `/status` asks Claude for its account's windows (`get_usage`), before
/// a turn and after it; the answers are read as rate limits.
#[tokio::test]
async fn claude_get_usage_on_request() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/get_usage.jsonl");
    let mut handle = unharness::harness::claude::ClaudeHarness::default()
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
    handle.send(SessionCommand::QueryStatus).await.unwrap();
    handle.send(SessionCommand::turn("hi")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let windows = |events: &[AgentEvent]| {
        events.iter().any(|e| {
            matches!(e, AgentEvent::RateLimit(r)
                if r.status.is_none() && r.windows.iter().any(|w| w.label == "five_hour"
                    && w.resets_at.is_some() && w.used_percent.is_some_and(|p| p > 1.0)))
        })
    };
    assert!(windows(&events), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error(_))));
    handle.send(SessionCommand::QueryStatus).await.unwrap();
    let mut after = Vec::new();
    while !windows(&after) {
        after.push(next_event(&mut handle).await);
    }
    let sent = fake.sent_lines();
    let asked: Vec<&serde_json::Value> = sent
        .iter()
        .filter(|v| v["request"]["subtype"] == "get_usage")
        .collect();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0]["request"]["skip_behaviors"], true);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// `/status` asks app-server for the account's limits and plan
/// (`account/rateLimits/read`, `account/read`), under the names whose
/// answers the parser reads, before a turn and after it.
#[tokio::test]
async fn codex_app_server_reads_rate_limits_and_account_on_request() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_account_read.jsonl");
    let mut handle = codex_app_server()
        .start_session(fake.config(&fixture, PermissionPolicy::Bypass, true))
        .unwrap();
    handle.send(SessionCommand::QueryStatus).await.unwrap();
    handle.send(SessionCommand::turn("hi")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let signed_in = |events: &[AgentEvent]| {
        events.iter().any(|e| {
            matches!(e, AgentEvent::Account(a)
                if a.auth.as_deref() == Some("chatgpt") && a.email.is_some())
        })
    };
    let limited = |events: &[AgentEvent]| {
        events.iter().any(|e| {
            matches!(e, AgentEvent::RateLimit(r)
                if r.limit.as_deref() == Some("codex") && !r.windows.is_empty())
        })
    };
    assert!(signed_in(&events) && limited(&events), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error(_))));

    handle.send(SessionCommand::QueryStatus).await.unwrap();
    let mut after = Vec::new();
    while !(signed_in(&after) && limited(&after)) {
        after.push(next_event(&mut handle).await);
    }
    let sent = fake.sent_lines();
    let reads: Vec<(&str, &str)> = sent
        .iter()
        .filter(|v| {
            v["method"]
                .as_str()
                .is_some_and(|m| m.starts_with("account/"))
        })
        .map(|v| (v["id"].as_str().unwrap(), v["method"].as_str().unwrap()))
        .collect();
    assert_eq!(
        reads,
        [
            ("rate-limits", "account/rateLimits/read"),
            ("account", "account/read"),
            ("rate-limits", "account/rateLimits/read"),
            ("account", "account/read"),
        ]
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
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

/// Codex's app-server takes its provider on its command line and a thread
/// on `thread/resume`: its driver ends the server as a shutdown does (its
/// input closed), starts it again once it is gone on the new provider,
/// resuming the thread, and tells the TUI `Restarted` (#275: before, the
/// TUI ended the session and started it again, which sent the same lines).
#[tokio::test]
async fn codex_app_server_starts_again_on_a_new_provider_resuming_its_thread() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/app_server_two_turns.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.env.extend([
        ("UNHARNESS_FAKE_ARGV".to_string(), "1".to_string()),
        ("UNHARNESS_FAKE_LINGER".to_string(), "0.2".to_string()),
    ]);
    let mut handle = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    )
    .start_session(cfg)
    .unwrap();
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    run_turn(&mut handle, |_| None).await;
    handle
        .send(SessionCommand::SetProvider("llama".into()))
        .await
        .unwrap();
    while next_event(&mut handle).await != AgentEvent::Restarted {}
    let sent = fake.sent_lines_eventually(12).await;
    assert_eq!(
        logged(&fake),
        [
            "argv",
            "initialize",
            "initialized",
            "thread/start",
            "skills/list",
            "turn/start",
            "ended",
            "argv",
            "initialize",
            "initialized",
            "thread/resume",
            "skills/list",
        ],
        "{sent:?}"
    );
    let argvs = argvs(&fake);
    let chosen = r#"model_provider="llama""#;
    assert!(!argvs[0].iter().any(|a| a == chosen));
    assert!(argvs[1].iter().any(|a| a == chosen), "{:?}", argvs[1]);
    let resume = sent
        .iter()
        .find(|v| v["method"] == "thread/resume")
        .unwrap();
    assert_eq!(
        resume["params"],
        serde_json::json!({"threadId": "01a10023-e1c2-7e82-a9e5-4d53ffccb2be", "modelProvider": "llama"})
    );
    shut_down(&mut handle).await;
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
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Compacted(_))));
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

/// A Codex without `app-server` under `transport = "auto"` (the fake
/// fails `app-server --help`, having no fixture there): the session runs
/// `exec`, and says first what it lacks of the app-server capabilities
/// `auto` declares, so that the TUI stops offering them. A compaction or a
/// rewind asked all the same reaches no CLI, and is refused in what ends
/// the TUI's wait on it (#288).
#[tokio::test]
async fn codex_auto_without_app_server_runs_exec_and_says_what_it_lacks() {
    use unharness::core::Ability;
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/codex/fixtures/exec_resume_command.jsonl");
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::Auto,
    );
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::AcceptEdits, false))
        .unwrap();

    let AgentEvent::CapabilitiesChanged(report) = next_event(&mut handle).await else {
        panic!("the session did not say what it lacks first");
    };
    assert_eq!(report.lacks, Some(Ability::ALL.to_vec()));
    assert_eq!(report.lacks_policies, Some(vec![PermissionPolicy::Ask]));
    assert_eq!(report.fork, Some(false));

    handle
        .send(SessionCommand::Compact { instructions: None })
        .await
        .unwrap();
    handle
        .send(SessionCommand::Rewind { anchor: "a".into() })
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut handle).await,
        AgentEvent::TurnCompleted {
            stop_reason: StopReason::Error("codex cannot compact its context on request".into())
        }
    );
    assert_eq!(
        next_event(&mut handle).await,
        AgentEvent::RewindFailed {
            reason: "not supported".into()
        }
    );

    // A turn runs as on `exec`, and is all that reached a CLI.
    handle.send(SessionCommand::turn("one")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));
    assert_eq!(std::fs::read_to_string(&fake.log).unwrap(), "one\n");

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

/// agy's model is a flag: the turn after a change goes to a new process,
/// which the session's process slot names from then on, and which starts
/// only once the one it replaces is gone (#286: that one was killed, and
/// the new one started beside it).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn agy_starts_again_for_a_new_model_and_its_slot_names_the_new_process() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/agy/fixtures/turn.jsonl");
    let pids = fake._tmp.path().join("pids");
    let mut cfg = fake.config(&fixture, PermissionPolicy::AcceptEdits, true);
    cfg.env.extend([
        (
            "UNHARNESS_FAKE_BACKGROUND".into(),
            format!("group:{}", pids.display()),
        ),
        // Each process says when it starts, and one whose input closed
        // takes a while to finish.
        ("UNHARNESS_FAKE_ARGV".into(), "1".into()),
        ("UNHARNESS_FAKE_LINGER".into(), "0.5".into()),
    ]);
    let mut handle = unharness::harness::agy::AgyHarness
        .start_session(cfg)
        .unwrap();
    let (first, _) = background_pids(&pids).await;
    handle.send(SessionCommand::turn("echo hi")).await.unwrap();
    run_turn(&mut handle, |_| None).await;
    std::fs::remove_file(&pids).unwrap();

    handle
        .send(SessionCommand::SetModel(ModelRef::new(
            HarnessId::AGY,
            "google",
            "gemini-3.7-flash-high",
        )))
        .await
        .unwrap();
    handle.send(SessionCommand::turn("again")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::Notice(n) if n == "agy restarted with the new settings")
        ),
        "{events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));
    let (second, _) = background_pids(&pids).await;
    assert_ne!(first, second);
    assert!(!running(first), "started beside the process it replaces");
    // The first was ended (its input closed), not killed, and had finished
    // before the second started.
    let started: Vec<String> = logged(&fake)
        .into_iter()
        .filter(|l| l == "argv" || l == "ended")
        .collect();
    assert_eq!(started, ["argv", "ended", "argv"]);
    let turns: Vec<Value> = fake
        .sent_lines()
        .into_iter()
        .filter(|l| l["event"] == "user")
        .map(|l| l["message"]["content"].clone())
        .collect();
    assert_eq!(turns, ["echo hi", "again"]);

    handle.process_slot().kill();
    while !matches!(
        next_event(&mut handle).await,
        AgentEvent::ProcessExited { .. }
    ) {}
    assert!(gone_within(second, Duration::from_secs(5)).await);
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

/// The fake ACP agent with `profile`.
fn fake_acp(profile: unharness::harness::acp::AcpProfile) -> unharness::harness::acp::AcpHarness {
    unharness::harness::acp::AcpHarness::new(
        "fake-acp",
        None,
        &[fake_harness().to_string_lossy().into_owned()],
        profile,
    )
    .unwrap()
}

/// What the config says of an ACP agent that asks, with no mode map.
fn asking() -> unharness::harness::acp::AcpProfile {
    unharness::harness::acp::AcpProfile {
        asks: true,
        ..Default::default()
    }
}

/// An ACP agent the config says asks, with no mode map.
fn acp_harness() -> unharness::harness::acp::AcpHarness {
    fake_acp(asking())
}

/// What the config says of an ACP agent that asks, with `modes`.
fn asking_with_modes(modes: &[(PermissionPolicy, &str)]) -> unharness::harness::acp::AcpProfile {
    unharness::harness::acp::AcpProfile {
        modes: unharness::harness::acp::ModeMap::new(modes.iter().map(|(p, m)| (*p, *m))),
        ..asking()
    }
}

fn acp_with_modes(modes: &[(PermissionPolicy, &str)]) -> unharness::harness::acp::AcpHarness {
    fake_acp(asking_with_modes(modes))
}

/// claude-agent-acp 0.85.1, whose `default` mode asks (and which every
/// recording starts in, so nothing is sent to set it).
fn claude_acp() -> unharness::harness::acp::AcpHarness {
    acp_with_modes(&[(PermissionPolicy::Ask, "default")])
}

/// vibe-acp 2.26.1: `ask` asks before every write and command; the agent
/// stays in it under accept-edits too, and unharness answers the edits.
const VIBE_MODES: &[(PermissionPolicy, &str)] = &[
    (PermissionPolicy::Ask, "ask"),
    (PermissionPolicy::Bypass, "auto-approve"),
];

fn vibe_acp() -> unharness::harness::acp::AcpHarness {
    acp_with_modes(VIBE_MODES)
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
    let mut handle = claude_acp().start_session(cfg).unwrap();

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
    let mut handle = claude_acp().start_session(cfg).unwrap();

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
    let mut handle = claude_acp().start_session(cfg).unwrap();

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

/// Events until the process has exited, after a shutdown.
async fn shut_down(handle: &mut SessionHandle) {
    handle.send(SessionCommand::Shutdown).await.unwrap();
    while !matches!(next_event(handle).await, AgentEvent::ProcessExited { .. }) {}
}

/// What an ACP agent does not offer (`AcpHarness::capabilities`) never
/// reaches it: each is answered with one event, in order with what it
/// takes. A compaction ends the turn the TUI runs it as, and a rewind
/// fails; the rest are notices.
#[tokio::test]
async fn acp_answers_a_command_it_cannot_carry_out_once() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = fake._tmp.path().join("handshake.jsonl");
    std::fs::write(
        &fixture,
        [
            r#">> {"method": "initialize"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            r#">> {"method": "session/new"}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#,
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Bypass, true);
    cfg.model = None;
    let mut handle = acp_harness().start_session(cfg).unwrap();
    events_until(&mut handle, |e| {
        matches!(e, AgentEvent::SessionStarted { .. })
    })
    .await;
    for cmd in [
        SessionCommand::Steer {
            text: "x".into(),
            attachments: Vec::new(),
        },
        SessionCommand::Compact { instructions: None },
        SessionCommand::Rewind { anchor: "a".into() },
        SessionCommand::StopSubagent { id: "s".into() },
        SessionCommand::RemoteControl {
            enabled: true,
            name: None,
        },
        // Taken: the driver says what it makes of it.
        SessionCommand::RespondPermission {
            id: "none".into(),
            decision: PermissionDecision::Deny {
                reason: String::new(),
            },
        },
    ] {
        handle.send(cmd).await.unwrap();
    }
    let answers: Vec<AgentEvent> = events_until(
        &mut handle,
        |e| matches!(e, AgentEvent::Notice(n) if n.starts_with("no pending")),
    )
    .await
    .into_iter()
    .filter(|e| {
        matches!(
            e,
            AgentEvent::Notice(_)
                | AgentEvent::TurnCompleted { .. }
                | AgentEvent::RewindFailed { .. }
        )
    })
    .collect();
    let notice = |n: &str| AgentEvent::Notice(n.into());
    assert_eq!(
        answers,
        [
            notice("fake-acp cannot steer a running turn"),
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::Error(
                    "fake-acp cannot compact its context on request".into()
                )
            },
            AgentEvent::RewindFailed {
                reason: "not supported".into()
            },
            notice("fake-acp cannot stop a subagent"),
            notice("fake-acp cannot go on remote control"),
            notice("no pending permission request none"),
        ]
    );
    assert_eq!(sent_methods(&fake), ["initialize", "session/new"]);
    shut_down(&mut handle).await;
}

/// As recorded on claude-agent-acp 0.85.1: a PermissionRequest hook denies
/// the Write while the request waits for the user, and the agent withdraws
/// the request with `$/cancel_request`. The modal is closed, the request
/// answered as cancelled, and the user's answer given meanwhile not sent.
#[tokio::test]
async fn acp_cancel_request_withdraws_an_open_permission_request() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/claude_agent_acp_withdrawn.jsonl");
    let cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    let mut handle = claude_acp().start_session(cfg).unwrap();

    handle
        .send(SessionCommand::turn(
            "Write the word hi into a new file note.txt with the Write tool. Do nothing else.",
        ))
        .await
        .unwrap();
    let mut asked = None;
    let mut events = Vec::new();
    loop {
        let ev = next_event(&mut handle).await;
        if let AgentEvent::PermissionRequest(req) = &ev {
            asked = Some(req.id.clone());
        }
        // The user answers just as the agent withdraws the request.
        if let AgentEvent::PermissionWithdrawn { id } = &ev {
            handle
                .send(SessionCommand::RespondPermission {
                    id: id.clone(),
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
    let asked = asked.expect("the Write was asked about");
    assert!(events.contains(&AgentEvent::PermissionWithdrawn { id: asked }));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallResult { is_error: true, output, .. }
            if output.contains("reviewed by hand")
    )));
    shut_down(&mut handle).await;
    // The late answer said nothing either.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(_) | AgentEvent::Error(_))),
        "{events:?}"
    );
    // The only answer to the request is the protocol's for a cancelled one.
    let answers: Vec<Value> = fake
        .sent_lines()
        .into_iter()
        .filter(|v| v["id"] == 0 && v.get("method").is_none())
        .collect();
    assert_eq!(
        answers,
        [serde_json::json!({
            "jsonrpc": "2.0", "id": 0,
            "error": {"code": -32800, "message": "Request cancelled"},
        })]
    );
}

/// A `$/cancel_request` for a request already answered, or for one never
/// sent, is ignored. Synthetic: the recording above with the request
/// answered before the agent withdraws it, and an unknown id withdrawn too
/// (no agent was seen to send either).
#[tokio::test]
async fn acp_cancel_request_for_an_answered_or_unknown_request_is_ignored() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let recorded = std::fs::read_to_string(
        repo().join("src/harness/acp/fixtures/claude_agent_acp_withdrawn.jsonl"),
    )
    .unwrap();
    let mut lines = Vec::new();
    for line in recorded.lines() {
        if line.contains("\"$/cancel_request\"") {
            lines.push(r#">> {"jsonrpc": "2.0", "id": 0, "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}}"#);
            lines.push(line);
            lines.push(
                r#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":"never-sent"}}"#,
            );
        } else if !line.contains("-32800") {
            lines.push(line);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("answered_then_withdrawn.jsonl");
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    let mut handle = claude_acp()
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();

    handle.send(SessionCommand::turn("write it")).await.unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then_some(PermissionDecision::Allow {
            updated_input: None,
        })
    })
    .await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::PermissionWithdrawn { .. } | AgentEvent::Notice(_) | AgentEvent::Error(_)
        )),
        "{events:?}"
    );
    shut_down(&mut handle).await;
    let sent = fake.sent_lines();
    assert!(!sent.iter().any(|v| v.get("error").is_some()), "{sent:?}");
    let answers: Vec<&Value> = sent
        .iter()
        .filter(|v| v["id"] == 0 && v.get("method").is_none())
        .collect();
    assert_eq!(answers.len(), 1, "{sent:?}");
    assert_eq!(answers[0]["result"]["outcome"]["optionId"], "allow-once");
}

/// The answers the driver sent to the agent's request `id`.
fn answers_to(sent: &[Value], id: u64) -> Vec<Value> {
    sent.iter()
        .filter(|v| v["id"] == id && v.get("method").is_none())
        .cloned()
        .collect()
}

/// A vibe fixture whose session is left in the mode it starts in: under
/// bypass, with no mode map and no model, the driver sends nothing the
/// recording did not.
fn vibe_as_recorded(fake: &Fake, fixture: &Path) -> SessionHandle {
    let mut cfg = fake.config(fixture, PermissionPolicy::Bypass, true);
    cfg.model = None;
    acp_harness().start_session(cfg).unwrap()
}

/// vibe-acp 2.26.1 asks with a form elicitation once unharness declares
/// it: the question goes to the user also under bypass, and the answer
/// is the form's content.
#[tokio::test]
async fn acp_form_elicitation_is_asked_and_answered() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_question.jsonl");
    let mut handle = vibe_as_recorded(&fake, &fixture);
    handle
        .send(SessionCommand::turn("ask me my colour"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| match e {
        AgentEvent::PermissionRequest(req) => {
            let PermissionKind::Question { questions } = &req.kind else {
                panic!("{req:?}")
            };
            assert_eq!(questions[0].options.len(), 2);
            Some(PermissionDecision::Answer(
                serde_json::json!({"q0": "Blue"}),
            ))
        }
        _ => None,
    })
    .await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CapabilitiesChanged(CapsUpdate {
            ask_user_question: Some(true),
            ..
        })
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::PermissionRequest(req)
            if req.tool_call_id.as_deref() == Some("effect-70979aa9-c4d8-54a8-a3d8-de5d4837ca50")
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));
    shut_down(&mut handle).await;
    let sent = fake.sent_lines();
    assert_eq!(
        sent[0]["params"]["clientCapabilities"],
        serde_json::json!({"elicitation": {"form": {}}})
    );
    assert_eq!(
        answers_to(&sent, 0),
        [serde_json::json!({"jsonrpc": "2.0", "id": 0,
            "result": {"action": "accept", "content": {"q0": "Blue"}}})]
    );
}

/// A multiple choice answered, then a question dismissed (Esc, or
/// `--print`, which has nobody to ask): declined.
#[tokio::test]
async fn acp_dismissed_elicitation_is_declined() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_question_multi.jsonl");
    let mut handle = vibe_as_recorded(&fake, &fixture);
    handle.send(SessionCommand::turn("colours")).await.unwrap();
    run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_))
            .then(|| PermissionDecision::Answer(serde_json::json!({"q0": ["Red", "Blue"]})))
    })
    .await;
    handle.send(SessionCommand::turn("drinks")).await.unwrap();
    run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_))
            .then_some(PermissionDecision::Answer(Value::Null))
    })
    .await;
    shut_down(&mut handle).await;
    let sent = fake.sent_lines();
    assert_eq!(
        answers_to(&sent, 0)[0]["result"],
        serde_json::json!({"action": "accept", "content": {"q0": ["Red", "Blue"]}})
    );
    assert_eq!(
        answers_to(&sent, 1)[0]["result"],
        serde_json::json!({"action": "decline"})
    );
}

/// claude-agent-acp 0.85.1 names the AskUserQuestion call; a typed answer
/// goes to the "Other" property it puts beside the question.
#[tokio::test]
async fn acp_typed_answer_goes_to_claude_agent_acps_other_property() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/claude_agent_acp_question.jsonl");
    let mut handle = claude_acp()
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
    handle.send(SessionCommand::turn("colour")).await.unwrap();
    run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_))
            .then(|| PermissionDecision::Answer(serde_json::json!({"question_0": "Blue"})))
    })
    .await;
    handle.send(SessionCommand::turn("colours")).await.unwrap();
    run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_))
            .then(|| PermissionDecision::Answer(serde_json::json!({"question_0": "Teal"})))
    })
    .await;
    shut_down(&mut handle).await;
    let sent = fake.sent_lines();
    assert_eq!(
        answers_to(&sent, 0)[0]["result"],
        serde_json::json!({"action": "accept", "content": {"question_0": "Blue"}})
    );
    assert_eq!(
        answers_to(&sent, 1)[0]["result"],
        serde_json::json!({"action": "accept", "content": {"question_0_custom": "Teal"}})
    );
}

/// The vibe question recording with what follows the elicitation changed:
/// `edit` gets each line and returns the lines to replay instead.
fn vibe_question_with(edit: impl Fn(&str) -> Vec<String>) -> (tempfile::TempDir, PathBuf) {
    let recorded =
        std::fs::read_to_string(repo().join("src/harness/acp/fixtures/vibe_acp_question.jsonl"))
            .unwrap();
    let lines: Vec<String> = recorded.lines().flat_map(&edit).collect();
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("question.jsonl");
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    (dir, fixture)
}

/// `$/cancel_request` for an open elicitation closes its modal and answers
/// it as a cancelled request; the user's answer given meanwhile is not
/// sent. Synthetic: no agent was seen to withdraw an elicitation.
#[tokio::test]
async fn acp_cancel_request_withdraws_an_open_elicitation() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (_dir, fixture) = vibe_question_with(|line| {
        if line.contains("\"elicitation/create\"") {
            vec![
                line.to_string(),
                r#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":0}}"#.into(),
            ]
        } else {
            vec![line.to_string()]
        }
    });
    let mut handle = vibe_as_recorded(&fake, &fixture);
    handle.send(SessionCommand::turn("colour")).await.unwrap();
    let mut events = Vec::new();
    loop {
        let ev = next_event(&mut handle).await;
        if let AgentEvent::PermissionWithdrawn { id } = &ev {
            handle
                .send(SessionCommand::RespondPermission {
                    id: id.clone(),
                    decision: PermissionDecision::Answer(serde_json::json!({"q0": "Red"})),
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
    assert!(events.contains(&AgentEvent::PermissionWithdrawn { id: "0".into() }));
    shut_down(&mut handle).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(_) | AgentEvent::Error(_))),
        "{events:?}"
    );
    assert_eq!(
        answers_to(&fake.sent_lines(), 0),
        [serde_json::json!({"jsonrpc": "2.0", "id": 0,
            "error": {"code": -32800, "message": "Request cancelled"}})]
    );
}

/// An interrupt with an elicitation open: the elicitation answered
/// `cancel`, then `session/cancel` once its call has ended. Synthetic,
/// from the vibe recording (its call's update is the one vibe sent after
/// the accepted answer).
#[tokio::test]
async fn acp_interrupt_cancels_an_open_elicitation() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (_dir, fixture) = vibe_question_with(|line| {
        if line.starts_with(">> ") && line.contains("\"action\"") {
            vec![
                r#">> {"id": 0, "result": {"action": "cancel"}}"#.into(),
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"52781132-721c-6b47-6ab0-160b86c84aea","update":{"toolCallId":"effect-70979aa9-c4d8-54a8-a3d8-de5d4837ca50","kind":"other","status":"completed","sessionUpdate":"tool_call_update"}}}"#.into(),
                r#">> {"method": "session/cancel"}"#.into(),
                r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#.into(),
            ]
        } else if line.contains("\"stopReason\"") || line.contains("tool_call_update") {
            vec![]
        } else {
            vec![line.to_string()]
        }
    });
    let mut handle = vibe_as_recorded(&fake, &fixture);
    handle.send(SessionCommand::turn("colour")).await.unwrap();
    let mut events = Vec::new();
    loop {
        let ev = next_event(&mut handle).await;
        if matches!(ev, AgentEvent::PermissionRequest(_)) {
            handle.send(SessionCommand::Interrupt).await.unwrap();
        }
        let done = matches!(ev, AgentEvent::TurnCompleted { .. });
        events.push(ev);
        if done {
            break;
        }
    }
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted
        })
    ));
    shut_down(&mut handle).await;
    let sent = fake.sent_lines();
    assert_eq!(
        answers_to(&sent, 0),
        [serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {"action": "cancel"}})]
    );
    let cancel = sent
        .iter()
        .position(|v| v["method"] == "session/cancel")
        .unwrap();
    let answer = sent.iter().position(|v| v["id"] == 0).unwrap();
    assert!(answer < cancel);
}

/// vibe-acp 2.26.1, interrupted with a write's permission request open
/// (#183): the request is answered `cancelled` and the TUI told it is
/// withdrawn; vibe reports the call failed, and only then does
/// `session/cancel` go, which ends the turn `cancelled`. (Sent the other
/// way round, vibe's session store failed and the prompt was never
/// answered.)
#[tokio::test]
async fn acp_interrupt_answers_an_open_request_before_cancelling() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_interrupt.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn(
            "create the file a.txt containing the word hi, using your file tool",
        ))
        .await
        .unwrap();
    let mut asked = None;
    let mut after = Vec::new();
    loop {
        let ev = next_event(&mut handle).await;
        if let AgentEvent::PermissionRequest(req) = &ev {
            asked = Some(req.id.clone());
            handle.send(SessionCommand::Interrupt).await.unwrap();
            continue;
        }
        let done = matches!(ev, AgentEvent::TurnCompleted { .. });
        if asked.is_some() {
            after.push(ev);
        }
        if done {
            break;
        }
    }
    let asked = asked.expect("vibe asked");
    let tail: Vec<&AgentEvent> = after
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::PermissionWithdrawn { .. } | AgentEvent::TurnCompleted { .. }
            )
        })
        .collect();
    assert_eq!(
        tail,
        [
            &AgentEvent::PermissionWithdrawn { id: asked },
            &AgentEvent::TurnCompleted {
                stop_reason: StopReason::Interrupted
            }
        ]
    );
    shut_down(&mut handle).await;
    let sent = fake.sent_lines();
    assert_eq!(
        answers_to(&sent, 0),
        [
            serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {"outcome": {"outcome": "cancelled"}}})
        ]
    );
    let answer = sent.iter().position(|v| v["id"] == 0).unwrap();
    let cancel = sent
        .iter()
        .position(|v| v["method"] == "session/cancel")
        .unwrap();
    assert!(answer < cancel);
}

/// The lines of a synthetic ACP turn in which a write is asked about,
/// up to the answer to the request, then `after`.
fn asked_then(after: &[&str]) -> Vec<String> {
    let mut lines: Vec<String> = [
        r#">> {"method": "initialize"}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
        r#">> {"method": "session/new"}"#,
        r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#,
        r#">> {"method": "session/prompt"}"#,
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"Writing a.txt","kind":"edit","status":"pending"}}}"#,
        r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"c1"},"options":[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"},{"optionId":"reject_once","name":"Reject","kind":"reject_once"}]}}"#,
        r#">> {"id": 0, "result": {"outcome": {"outcome": "cancelled"}}}"#,
    ]
    .map(String::from)
    .to_vec();
    lines.extend(after.iter().map(|l| l.to_string()));
    lines
}

/// Interrupt at the first request and run the turn to its end.
async fn interrupt_at_request(handle: &mut SessionHandle) -> Vec<AgentEvent> {
    handle.send(SessionCommand::turn("write")).await.unwrap();
    events_until(handle, |e| matches!(e, AgentEvent::PermissionRequest(_))).await;
    handle.send(SessionCommand::Interrupt).await.unwrap();
    events_until(handle, |e| matches!(e, AgentEvent::TurnCompleted { .. })).await
}

/// The `session/cancel` of an interrupt waits until the call of the
/// request it answered `cancelled` is reported ended.
#[tokio::test]
async fn acp_interrupt_waits_for_the_answered_call_before_cancelling() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let lines = asked_then(&[
        "# gate",
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"failed"}}}"#,
        r#">> {"method": "session/cancel"}"#,
        r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#,
    ]);
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let (mut cfg, gate) = gated_config(&fake, &lines);
    cfg.policy = PermissionPolicy::Ask;
    let mut handle = acp_harness().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("write")).await.unwrap();
    events_until(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_))
    })
    .await;
    handle.send(SessionCommand::Interrupt).await.unwrap();
    events_until(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionWithdrawn { .. })
    })
    .await;
    // Answered, and nothing more while the call has not ended.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = fake.sent_lines();
    assert_eq!(answers_to(&sent, 0).len(), 1);
    assert!(!sent.iter().any(|v| v["method"] == "session/cancel"));
    std::fs::write(&gate, "").unwrap();
    let events = events_until(&mut handle, |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    assert!(interrupted(&events), "{events:?}");
    assert!(sent_methods(&fake).contains(&"session/cancel".to_string()));
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A call never reported ended does not hold the `session/cancel` past
/// `SETTLE_WAIT`.
#[tokio::test]
async fn acp_interrupt_cancels_after_a_while_without_the_call() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let lines = asked_then(&[
        r#">> {"method": "session/cancel"}"#,
        r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#,
    ]);
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let (mut cfg, _gate) = gated_config(&fake, &lines);
    cfg.policy = PermissionPolicy::Ask;
    let mut handle = acp_harness().start_session(cfg).unwrap();
    let started = tokio::time::Instant::now();
    let events = interrupt_at_request(&mut handle).await;
    assert!(interrupted(&events), "{events:?}");
    assert!(started.elapsed() >= unharness::harness::acp::transport::SETTLE_WAIT);
    let sent = fake.sent_lines();
    let answer = sent.iter().position(|v| v["id"] == 0).unwrap();
    let cancel = sent
        .iter()
        .position(|v| v["method"] == "session/cancel")
        .unwrap();
    assert!(answer < cancel);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A request that crosses the interrupt is answered `cancelled` at once,
/// never shown, and its call waited for too.
#[tokio::test]
async fn acp_request_during_an_interrupt_is_answered_cancelled() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let lines = asked_then(&[
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"c2","title":"touch x","kind":"execute","status":"pending"}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"c2"},"options":[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"}]}}"#,
        r#">> {"id": 1, "result": {"outcome": {"outcome": "cancelled"}}}"#,
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"failed"}}}"#,
        "# gate",
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call_update","toolCallId":"c2","status":"failed"}}}"#,
        r#">> {"method": "session/cancel"}"#,
        r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#,
    ]);
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let (mut cfg, gate) = gated_config(&fake, &lines);
    cfg.policy = PermissionPolicy::Ask;
    let mut handle = acp_harness().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("write")).await.unwrap();
    events_until(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_))
    })
    .await;
    handle.send(SessionCommand::Interrupt).await.unwrap();
    // The second call's request is answered; its call still holds the cancel.
    let sent = fake.sent_lines_eventually(5).await;
    assert_eq!(
        answers_to(&sent, 1),
        [
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"outcome": {"outcome": "cancelled"}}})
        ]
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!sent_methods(&fake).contains(&"session/cancel".to_string()));
    std::fs::write(&gate, "").unwrap();
    let events = events_until(&mut handle, |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    assert!(interrupted(&events), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionRequest(_))),
        "{events:?}"
    );
    assert!(sent_methods(&fake).contains(&"session/cancel".to_string()));
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A URL elicitation, never declared, is refused with an error and the
/// user told. Synthetic: the vibe recording's elicitation in URL mode.
#[tokio::test]
async fn acp_url_elicitation_is_refused() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (_dir, fixture) = vibe_question_with(|line| {
        if line.contains("\"elicitation/create\"") {
            vec![r#"{"jsonrpc":"2.0","id":0,"method":"elicitation/create","params":{"sessionId":"s","mode":"url","elicitationId":"e1","url":"https://example.com/auth","message":"Sign in"}}"#.into()]
        } else {
            vec![line.to_string()]
        }
    });
    let mut handle = vibe_as_recorded(&fake, &fixture);
    handle.send(SessionCommand::turn("colour")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::PermissionRequest(_)
                | AgentEvent::CapabilitiesChanged(CapsUpdate {
                    ask_user_question: Some(_),
                    ..
                })
        )),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("url elicitation"))),
        "{events:?}"
    );
    shut_down(&mut handle).await;
    let answers = answers_to(&fake.sent_lines(), 0);
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0]["error"]["code"], -32602);
}

/// The optionIds the driver answered permission requests with, in order.
fn permission_answers(fake: &Fake) -> Vec<Value> {
    fake.sent_lines()
        .into_iter()
        .filter_map(|v| v["result"]["outcome"]["optionId"].as_str().map(Value::from))
        .collect()
}

fn asked(events: &[AgentEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::PermissionRequest(_)))
}

/// vibe-acp 2.26.1 names the call in a request by its id alone; the kind
/// and the file came on the `tool_call` before it. Under accept-edits the
/// write in the workspace is answered by the policy, and the one outside it
/// is asked about (#204).
#[tokio::test]
async fn acp_accept_edits_answers_only_an_edit_inside_the_workspace() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_accept_edits_outside.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::AcceptEdits, true);
    cfg.model = None;
    // The recording's workspace; the session runs in a directory below it
    // as far as the policy is concerned, which takes the root.
    cfg.workspace_root = Some("/WORKSPACE".into());
    // vibe started in `ask`, the mode accept-edits maps to: nothing is sent.
    let mut handle = vibe_acp().start_session(cfg).unwrap();

    // The edit in the workspace is answered by the policy: shown, not asked.
    handle
        .send(SessionCommand::turn("write in.txt"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallStarted { name, .. } if name == "edit"))
    );
    assert!(!asked(&events), "{events:?}");
    // The one outside is the user's, and the user's answer is what is sent.
    handle
        .send(SessionCommand::turn("write outside"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then(|| PermissionDecision::Deny {
            reason: "no".into(),
        })
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(e,
            AgentEvent::PermissionRequest(PermissionRequest {
                kind: PermissionKind::ToolUse { action: ToolAction::Edit { paths }, .. },
                ..
            }) if paths == &[PathBuf::from("/tmp/l204/outside/x.txt")])),
        "{events:?}"
    );
    assert_eq!(permission_answers(&fake), ["allow_once", "reject_once"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// The same requests with the workspace where the session runs (no root
/// above it): the first write is outside it too, and both are asked about.
#[tokio::test]
async fn acp_accept_edits_takes_the_sessions_directory_without_a_root() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_accept_edits_outside.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::AcceptEdits, true);
    cfg.model = None;
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    let allow = |e: &AgentEvent| {
        matches!(e, AgentEvent::PermissionRequest(_)).then_some(PermissionDecision::Allow {
            updated_input: None,
        })
    };
    let deny = |e: &AgentEvent| {
        matches!(e, AgentEvent::PermissionRequest(_)).then(|| PermissionDecision::Deny {
            reason: "no".into(),
        })
    };
    handle
        .send(SessionCommand::turn("write in.txt"))
        .await
        .unwrap();
    assert!(asked(&run_turn(&mut handle, allow).await));
    handle
        .send(SessionCommand::turn("write outside"))
        .await
        .unwrap();
    assert!(asked(&run_turn(&mut handle, deny).await));
    assert_eq!(permission_answers(&fake), ["allow_once", "reject_once"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// vibe-acp 2.26.1 writes the path in an edit's diff as the model wrote it
/// (`a.txt`): for an agent not known to put such a path in the session's
/// directory it is unknown, and accept-edits asks about the edit (#204),
/// as about the command.
#[tokio::test]
async fn acp_accept_edits_asks_about_an_edit_of_a_relative_path() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_ask.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::AcceptEdits, true);
    cfg.model = None;
    cfg.workspace_root = Some("/WORKSPACE".into());
    let mut handle = vibe_acp().start_session(cfg).unwrap();

    handle
        .send(SessionCommand::turn("write a.txt"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then_some(PermissionDecision::Allow {
            updated_input: None,
        })
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::PermissionRequest(PermissionRequest {
                kind: PermissionKind::ToolUse {
                    action: ToolAction::Opaque,
                    ..
                },
                ..
            })
        )),
        "{events:?}"
    );
    handle.send(SessionCommand::turn("touch x")).await.unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then(|| PermissionDecision::Deny {
            reason: "no".into(),
        })
    })
    .await;
    assert!(asked(&events));
    assert_eq!(permission_answers(&fake), ["allow_once", "reject_once"]);
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
    let mut handle = claude_acp().start_session(cfg).unwrap();

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

/// Mistral Vibe's reasoning level is the option `thinking` under a category
/// the spec does not name, and setting it rewrites vibe's own config file.
/// An agent that did not declare it as its effort is never sent it, also
/// with an effort configured.
#[tokio::test]
async fn acp_sends_no_effort_to_an_option_it_was_not_told_is_one() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp.jsonl");
    // Left in the mode it starts in: no map, and bypass, the one policy
    // that leaves an agent's mode alone.
    let mut cfg = fake.config(&fixture, PermissionPolicy::Bypass, true);
    cfg.model = None;
    cfg.effort = Some("high".into());
    let mut handle = acp_harness().start_session(cfg).unwrap();

    handle
        .send(SessionCommand::turn("Reply with the single word: pong"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::CapabilitiesChanged(u) if u.effort_levels.as_deref() == Some(&[][..])
    )));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "pong"))
    );

    let methods: Vec<String> = fake
        .sent_lines()
        .iter()
        .filter_map(|v| v["method"].as_str().map(str::to_string))
        .collect();
    assert_eq!(methods, ["initialize", "session/new", "session/prompt"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// The methods the driver sent, in order.
fn sent_methods(fake: &Fake) -> Vec<String> {
    fake.sent_lines()
        .iter()
        .filter_map(|v| v["method"].as_str().map(str::to_string))
        .collect()
}

fn refused(events: &[AgentEvent], policy: PermissionPolicy) -> bool {
    events.iter().any(|e| {
        matches!(e, AgentEvent::CapabilitiesChanged(CapsUpdate { refused_policies: Some(r), .. }) if r.contains(&policy))
    })
}

fn turn_error(events: &[AgentEvent]) -> Option<&str> {
    events.iter().find_map(|e| match e {
        AgentEvent::TurnCompleted {
            stop_reason: StopReason::Error(why),
        } => Some(why.as_str()),
        _ => None,
    })
}

/// vibe-acp 2.26.1 starts in its accept-edits mode, which writes without
/// asking. Under `ask` the mapped mode is set, and read back from the
/// answer, before the first prompt goes, and the write then comes as a
/// request. A policy change sets the mode again before the next prompt.
#[tokio::test]
async fn acp_sets_the_mapped_mode_before_the_first_prompt() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_mode.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let mut handle = vibe_acp().start_session(cfg).unwrap();

    // Sent before the handshake: it waits for the session and its mode.
    handle
        .send(SessionCommand::turn("write p.txt"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then(|| PermissionDecision::Deny {
            reason: "no".into(),
        })
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionRequest(_))),
        "{events:?}"
    );
    let sent = fake.sent_lines();
    assert_eq!(sent[1]["method"], "session/new");
    assert_eq!(sent[2]["method"], "session/set_config_option");
    assert_eq!(sent[2]["params"]["configId"], "mode");
    assert_eq!(sent[2]["params"]["value"], "ask");
    assert_eq!(sent[3]["method"], "session/prompt");
    assert_eq!(sent[4]["result"]["outcome"]["optionId"], "reject_once");

    // bypass maps to auto-approve: set before the next prompt, whose
    // write then runs without a request.
    handle
        .send(SessionCommand::SetPolicy(PermissionPolicy::Bypass))
        .await
        .unwrap();
    handle
        .send(SessionCommand::turn("write q.txt"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionRequest(_)))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallResult {
            is_error: false,
            ..
        }
    )));
    let sent = fake.sent_lines();
    assert_eq!(sent[5]["method"], "session/set_config_option");
    assert_eq!(sent[5]["params"]["value"], "auto-approve");
    assert_eq!(sent[6]["method"], "session/prompt");
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// Events until the process has exited.
async fn until_exited(handle: &mut SessionHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let ev = next_event(handle).await;
        let exited = matches!(ev, AgentEvent::ProcessExited { .. });
        events.push(ev);
        if exited {
            return events;
        }
    }
}

/// The methods the driver sent, in order, with `ended` where the fake saw
/// its stdin close (`UNHARNESS_FAKE_LINGER`).
fn sent_until_ended(fake: &Fake) -> Vec<String> {
    fake.sent_lines()
        .iter()
        .map(|v| match (v.get("method"), v.get("ended")) {
            (Some(m), _) => m.as_str().unwrap_or("").to_string(),
            (None, Some(_)) => "ended".into(),
            (None, None) => "answer".into(),
        })
        .collect()
}

/// vibe-acp 2.26.1 advertises `sessionCapabilities.close`: a shutdown
/// between turns sends `session/close` for the session before stdin
/// closes, and the agent's `{}` answer is no event.
#[tokio::test]
async fn acp_closes_the_session_before_stdin_at_shutdown() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_close.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.env.push(("UNHARNESS_FAKE_LINGER".into(), "0.2".into()));
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "pong")),
        "{events:?}"
    );

    let started = tokio::time::Instant::now();
    handle.send(SessionCommand::Shutdown).await.unwrap();
    let tail = until_exited(&mut handle).await;
    // Answered: stdin closed then, not after the wait.
    assert!(
        started.elapsed() < unharness::harness::acp::transport::CLOSE_WAIT,
        "{:?}",
        started.elapsed()
    );
    assert!(
        !tail.iter().any(|e| matches!(e, AgentEvent::Error(_))),
        "{tail:?}"
    );
    assert_eq!(
        sent_until_ended(&fake),
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt",
            "session/close",
            "ended",
        ]
    );
    let close = &fake.sent_lines()[4];
    assert_eq!(
        close["params"]["sessionId"],
        fake.sent_lines()[3]["params"]["sessionId"]
    );
    assert!(close["id"].is_u64(), "a request: {close}");
}

/// An agent that does not advertise `session/close` is not sent it: its
/// session ends with its stdin, as before.
#[tokio::test]
async fn acp_sends_no_close_to_an_agent_that_does_not_take_it() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let recorded =
        std::fs::read_to_string(repo().join("src/harness/acp/fixtures/vibe_acp_close.jsonl"))
            .unwrap();
    assert!(recorded.contains(r#","close":{}}"#));
    let fixture = fake._tmp.path().join("no_close.jsonl");
    let lines: Vec<&str> = recorded
        .lines()
        .take_while(|l| !(l.starts_with(">>") && l.contains("session/close")))
        .collect();
    std::fs::write(&fixture, lines.join("\n").replace(r#","close":{}}"#, "}")).unwrap();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.env.push(("UNHARNESS_FAKE_LINGER".into(), "0.2".into()));
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    run_turn(&mut handle, |_| None).await;
    handle.send(SessionCommand::Shutdown).await.unwrap();
    until_exited(&mut handle).await;
    assert_eq!(
        sent_until_ended(&fake),
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt",
            "ended",
        ]
    );
}

/// An agent that does not answer `session/close` has its stdin closed
/// after `CLOSE_WAIT`, and its grace then as at any shutdown.
#[tokio::test]
async fn acp_closes_stdin_when_session_close_is_not_answered() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let recorded =
        std::fs::read_to_string(repo().join("src/harness/acp/fixtures/vibe_acp_close.jsonl"))
            .unwrap();
    // Everything up to the close, which is read and never answered.
    let fixture = fake._tmp.path().join("unanswered.jsonl");
    let mut lines = Vec::new();
    for l in recorded.lines() {
        lines.push(l);
        if l.starts_with(">>") && l.contains("session/close") {
            break;
        }
    }
    std::fs::write(&fixture, lines.join("\n")).unwrap();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.env.push(("UNHARNESS_FAKE_LINGER".into(), "0.2".into()));
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    run_turn(&mut handle, |_| None).await;
    let started = tokio::time::Instant::now();
    handle.send(SessionCommand::Shutdown).await.unwrap();
    until_exited(&mut handle).await;
    let wait = unharness::harness::acp::transport::CLOSE_WAIT;
    let took = started.elapsed();
    assert!(took >= wait, "{took:?}");
    assert!(
        took < wait + unharness::core::process::END_GRACE,
        "{took:?}"
    );
    assert_eq!(sent_until_ended(&fake)[4..], ["session/close", "ended"]);
}

/// Mid-turn the agent is killed at once, with nothing sent first: given
/// its grace, `session/close` or not, it could go on with the turn.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn acp_sends_no_close_mid_turn() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let recorded =
        std::fs::read_to_string(repo().join("src/harness/acp/fixtures/vibe_acp_close.jsonl"))
            .unwrap();
    // The prompt is never answered.
    let fixture = fake._tmp.path().join("mid_turn.jsonl");
    let mut lines = Vec::new();
    for l in recorded.lines() {
        lines.push(l);
        if l.starts_with(">>") && l.contains("session/prompt") {
            break;
        }
    }
    std::fs::write(&fixture, lines.join("\n")).unwrap();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.env.push(("UNHARNESS_FAKE_LINGER".into(), "60".into()));
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    fake.sent_lines_eventually(4).await;
    let started = tokio::time::Instant::now();
    handle.send(SessionCommand::Shutdown).await.unwrap();
    until_exited(&mut handle).await;
    assert!(started.elapsed() < unharness::core::process::END_GRACE);
    assert_eq!(
        sent_until_ended(&fake),
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt",
        ]
    );
}

/// A loaded vibe session comes back in the mode it was left in
/// (auto-approve): it is set again before the prompt.
#[tokio::test]
async fn acp_sets_the_mode_again_after_a_load() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_mode_load.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some("6addc339-ecf5-d995-a253-f231a4c8b779".into());
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "pong"))
    );
    assert_eq!(
        sent_methods(&fake),
        [
            "initialize",
            "session/load",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    assert_eq!(fake.sent_lines()[2]["params"]["value"], "ask");
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// The session `fixtures/vibe_acp_fork.jsonl` forks, and its fork.
const VIBE_FORK_SOURCE: &str = "9f1b157d-47e8-4f99-a17a-f0d98c9a179c";
const VIBE_FORK: &str = "8db2586d-4d39-b98f-20da-c7ea3669df58";

/// A fork (`SessionConfig::fork`) of a vibe-acp 2.26.1 session in a new
/// process: `session/fork` of the stored session, refused until the
/// source is loaded, then answered with a new session in the source's
/// mode (`auto-approve`). The answer goes through `session_ready`, so the
/// mode `ask` is set before the fork's first prompt, which goes to the
/// fork and is answered from the source's turn; the source is sent no
/// prompt, mode or close, and its replayed history is not shown.
#[tokio::test]
async fn acp_forks_a_stored_session_and_sets_its_mode_again() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_fork.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some(VIBE_FORK_SOURCE.into());
    cfg.fork = true;
    cfg.env.push(("UNHARNESS_FAKE_LINGER".into(), "0.2".into()));
    let cwd = cfg.cwd.clone();
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("What was the code word?"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let started: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(started, [VIBE_FORK]);
    let said: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(said, "pineapple", "only the fork's turn: {events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error(_) | AgentEvent::Notice(_))),
        "the refused first fork is the driver's: {events:?}"
    );
    assert!(!refused(&events, PermissionPolicy::Ask));

    handle.send(SessionCommand::Shutdown).await.unwrap();
    until_exited(&mut handle).await;
    assert_eq!(
        sent_until_ended(&fake),
        [
            "initialize",
            "session/fork",
            "session/load",
            "session/fork",
            "session/set_config_option",
            "session/prompt",
            "session/close",
            "ended",
        ]
    );
    let sent = fake.sent_lines();
    for line in &sent[1..4] {
        assert_eq!(
            line["params"],
            serde_json::json!({"sessionId": VIBE_FORK_SOURCE, "cwd": cwd, "mcpServers": []}),
            "{line}"
        );
    }
    assert_eq!(
        sent[4]["params"],
        serde_json::json!({"sessionId": VIBE_FORK, "configId": "mode", "value": "ask"})
    );
    assert_eq!(sent[5]["params"]["sessionId"], VIBE_FORK);
    assert_eq!(sent[6]["params"]["sessionId"], VIBE_FORK);
}

/// codex-acp 2.1.1 forks a stored session without opening it and leaves
/// the fork closed (unresumed, its first prompt was never answered): an
/// agent that resumes sessions has the fork resumed, and the prompt goes
/// to it.
#[tokio::test]
async fn acp_resumes_the_fork_where_the_agent_resumes_sessions() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/codex_acp_fork.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Bypass, true);
    cfg.model = None;
    cfg.resume = Some("01a12d15-cd81-7b92-b701-865f87098d7b".into());
    cfg.fork = true;
    let mut handle = acp_harness().start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("What was the code word?"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let fork = "01a12d18-f272-7af2-b725-0965213dee58";
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::SessionStarted { session_id, .. } if session_id == fork)
        ),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("pin")))
    );
    assert_eq!(
        sent_methods(&fake),
        [
            "initialize",
            "session/fork",
            "session/resume",
            "session/prompt"
        ]
    );
    let sent = fake.sent_lines();
    assert_eq!(sent[2]["params"]["sessionId"], fork);
    assert_eq!(sent[3]["params"]["sessionId"], fork);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// An agent that does not advertise `session/fork` is never reattached to
/// the session it was to fork (that one is the original conversation's):
/// it gets a new session, reported as one that replaces the fork, so that
/// the TUI sends it the conversation.
#[tokio::test]
async fn acp_starts_a_new_session_when_the_agent_cannot_fork() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let recorded =
        std::fs::read_to_string(repo().join("src/harness/acp/fixtures/vibe_acp_close.jsonl"))
            .unwrap();
    assert!(recorded.contains(r#""fork":{},"#));
    let fixture = fake._tmp.path().join("no_fork.jsonl");
    std::fs::write(&fixture, recorded.replace(r#""fork":{},"#, "")).unwrap();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some("some-stored-session".into());
    cfg.fork = true;
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::SessionReplaced { reason } if reason == "this agent cannot fork sessions")
        ),
        "{events:?}"
    );
    assert_eq!(
        sent_methods(&fake),
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// How a session that could not be reattached or forked started
/// (`fixtures/vibe_acp_{load,fork}_missing.jsonl`): the driver reports
/// that a new session replaces it before that session's `SessionStarted`,
/// and the turn goes to the new one.
async fn replaced_session(fixture: &str, fork: bool) -> (Fake, String) {
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures").join(fixture);
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some(
        if fork {
            MISSING_FORK_SOURCE
        } else {
            MISSING_SESSION
        }
        .into(),
    );
    cfg.fork = fork;
    cfg.env.push(("UNHARNESS_FAKE_LINGER".into(), "0.2".into()));
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("Reply with the single word: pong"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    handle.send(SessionCommand::Shutdown).await.unwrap();
    until_exited(&mut handle).await;
    let said: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let starts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionReplaced { reason } => Some(format!("replaced: {reason}")),
            AgentEvent::SessionStarted { session_id, .. } => Some(format!("started {session_id}")),
            _ => None,
        })
        .collect();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error(_) | AgentEvent::Notice(_))),
        "the failed answer is the driver's: {events:?}"
    );
    assert_eq!(said, "pong");
    (fake, starts.join("\n"))
}

/// A session reattached by `session/load` names the model it runs: the
/// load answer's `configOptions` (vibe-acp 2.26.1,
/// `fixtures/vibe_acp_load.jsonl`), so that a resumed conversation shows
/// its default model as a new session does (#329).
#[tokio::test]
async fn acp_reattached_session_names_its_model() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_load.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some("7d4ac1a9-0de3-c46a-1c64-c2469dd67154".into());
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    // Announced as the load is answered, before any prompt.
    let started = loop {
        if let AgentEvent::SessionStarted { session_id, model } = next_event(&mut handle).await {
            break (session_id, model);
        }
    };
    assert_eq!(
        started,
        (
            "7d4ac1a9-0de3-c46a-1c64-c2469dd67154".to_string(),
            Some("glm-5-3".to_string())
        )
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// Sessions vibe-acp 2.26.1 did not have when asked for them.
const MISSING_SESSION: &str = "3c0c2f6e-5b1a-4d2e-9f7a-1b2c3d4e5f60";
const MISSING_FORK_SOURCE: &str = "3c0c2f6e-5b1a-4d2e-9f7a-1b2c3d4e5f61";

/// A resume the agent refuses (vibe-acp 2.26.1 `session/load` of a session
/// it does not have) starts a new session, reported as one that replaces
/// the session to resume (#187), so that the TUI tells it the
/// conversation.
#[tokio::test]
async fn acp_reports_the_new_session_in_place_of_one_it_could_not_resume() {
    if !python_available() {
        return;
    }
    let (fake, starts) = replaced_session("vibe_acp_load_missing.jsonl", false).await;
    assert_eq!(
        starts,
        format!(
            "replaced: could not resume the session (Session not found: {MISSING_SESSION})\n\
             started 4b47571d-ea77-0161-9bf3-0f91f0369bbf"
        )
    );
    assert_eq!(
        sent_until_ended(&fake),
        [
            "initialize",
            "session/load",
            "session/new",
            "session/set_config_option",
            "session/prompt",
            "session/close",
            "ended",
        ]
    );
}

/// A fork that fails twice (vibe-acp 2.26.1: the source is neither forked
/// nor loaded, since it does not have it) starts a new session, reported
/// as one that replaces the fork (#187).
#[tokio::test]
async fn acp_reports_the_new_session_in_place_of_a_fork_that_failed() {
    if !python_available() {
        return;
    }
    let (fake, starts) = replaced_session("vibe_acp_fork_missing.jsonl", true).await;
    assert_eq!(
        starts,
        format!(
            "replaced: could not fork the session (Session not found: {MISSING_FORK_SOURCE})\n\
             started 2cda0472-ef5b-7b86-2b00-1694a93b61b2"
        )
    );
    assert_eq!(
        sent_until_ended(&fake),
        [
            "initialize",
            "session/fork",
            "session/load",
            "session/new",
            "session/set_config_option",
            "session/prompt",
            "session/close",
            "ended",
        ]
    );
}

/// An agent with modes of its own and no map has no policy below bypass:
/// the prompt is failed, not sent, and the policy taken out of what the
/// session offers. (A vibe session loaded in auto-approve would otherwise
/// run commands unasked under accept-edits.)
#[tokio::test]
async fn acp_refuses_every_policy_below_bypass_for_an_agent_with_modes_and_no_map() {
    if !python_available() {
        return;
    }
    for policy in [PermissionPolicy::Ask, PermissionPolicy::AcceptEdits] {
        let fake = Fake::new();
        let fixture = repo().join("src/harness/acp/fixtures/vibe_acp.jsonl");
        let mut cfg = fake.config(&fixture, policy, true);
        cfg.model = None;
        let mut handle = acp_harness().start_session(cfg).unwrap();
        handle.send(SessionCommand::turn("pong?")).await.unwrap();
        let events = run_turn(&mut handle, |_| None).await;
        let why = turn_error(&events).expect("the turn fails");
        assert!(
            why.contains(&format!("policy '{policy}' is not available for fake-acp"))
                && why.contains("[harnesses.fake-acp] has no `modes` map"),
            "{why}"
        );
        assert!(refused(&events, policy), "{events:?}");
        assert_eq!(sent_methods(&fake), ["initialize", "session/new"]);
        handle.send(SessionCommand::Shutdown).await.unwrap();
    }
}

/// A mapped mode the agent does not offer is never sent (vibe-acp answers
/// `session/set_mode` of one with `{}` and stays where it is).
#[tokio::test]
async fn acp_refuses_a_mode_the_agent_does_not_offer() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let mut handle = acp_with_modes(&[(PermissionPolicy::Ask, "plan")])
        .start_session(cfg)
        .unwrap();
    handle.send(SessionCommand::turn("pong?")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let why = turn_error(&events).expect("the turn fails");
    assert!(why.contains("does not offer mode 'plan'"), "{why}");
    assert!(refused(&events, PermissionPolicy::Ask));
    assert_eq!(sent_methods(&fake), ["initialize", "session/new"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A read-back that differs from the mode asked for holds the session: no
/// prompt goes. (The lines are vibe-acp 2.26.1's, the answer changed.)
#[tokio::test]
async fn acp_refuses_a_mode_whose_read_back_differs() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let option = |current: &str| {
        format!(
            r#"[{{"id":"mode","category":"mode","type":"select","currentValue":"{current}","options":[{{"value":"ask"}},{{"value":"accept-edits"}}]}}]"#
        )
    };
    let new = format!(
        r#"{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s1","modes":{{"currentModeId":"accept-edits","availableModes":[{{"id":"ask"}},{{"id":"accept-edits"}}]}},"configOptions":{}}}}}"#,
        option("accept-edits")
    );
    let answer = format!(
        r#"{{"jsonrpc":"2.0","id":3,"result":{{"configOptions":{}}}}}"#,
        option("accept-edits")
    );
    let (mut cfg, _gate) = gated_config(
        &fake,
        &[
            r#">> {"method": "initialize"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            r#">> {"method": "session/new"}"#,
            &new,
            r#">> {"method": "session/set_config_option"}"#,
            &answer,
        ],
    );
    cfg.policy = PermissionPolicy::Ask;
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    let events = events_until(&mut handle, |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    let why = turn_error(&events).expect("the turn fails");
    assert!(
        why.contains("was set to mode 'ask' and is in 'accept-edits'"),
        "{why}"
    );
    assert!(refused(&events, PermissionPolicy::Ask));
    assert_eq!(
        sent_methods(&fake),
        ["initialize", "session/new", "session/set_config_option"]
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A resumed session whose answer names no modes cannot be checked: a
/// mapped policy is refused rather than left to the mode it was left in.
#[tokio::test]
async fn acp_refuses_a_mapped_policy_after_a_resume_that_names_no_modes() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (mut cfg, _gate) = gated_config(
        &fake,
        &[
            r#">> {"method": "initialize"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}}}}"#,
            r#">> {"method": "session/resume"}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{}}"#,
        ],
    );
    cfg.policy = PermissionPolicy::Ask;
    cfg.resume = Some("s1".into());
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    let events = events_until(&mut handle, |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    let why = turn_error(&events).expect("the turn fails");
    assert!(why.contains("did not say which modes"), "{why}");
    assert_eq!(sent_methods(&fake), ["initialize", "session/resume"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// The agent going into a wider mode by itself mid-turn stops the turn and
/// is set back. (vibe-acp 2.26.1 was not seen to change its own mode; the
/// update is the spec's.)
#[tokio::test]
async fn acp_sets_the_mode_back_when_the_agent_leaves_it() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let option = r#"[{"id":"mode","category":"mode","type":"select","currentValue":"ask","options":[{"value":"ask"},{"value":"auto-approve"}]}]"#;
    let new = format!(
        r#"{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s1","modes":{{"currentModeId":"ask","availableModes":[{{"id":"ask"}},{{"id":"auto-approve"}}]}},"configOptions":{option}}}}}"#
    );
    let (mut cfg, _gate) = gated_config(
        &fake,
        &[
            r#">> {"method": "initialize"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            r#">> {"method": "session/new"}"#,
            &new,
            r#">> {"method": "session/prompt"}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"current_mode_update","currentModeId":"auto-approve"}}}"#,
            r#">> {"method": "session/cancel"}"#,
            r#">> {"method": "session/set_config_option"}"#,
            r#"{"jsonrpc":"2.0","id":4,"result":{"configOptions":[{"id":"mode","category":"mode","type":"select","currentValue":"ask","options":[{"value":"ask"},{"value":"auto-approve"}]}]}}"#,
            r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#,
        ],
    );
    cfg.policy = PermissionPolicy::Ask;
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(interrupted(&events), "{events:?}");
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::Notice(n) if n.contains("went into its mode 'auto-approve'"))
    ));
    let sent = fake.sent_lines();
    assert_eq!(
        sent_methods(&fake),
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/cancel",
            "session/set_config_option"
        ]
    );
    assert_eq!(sent[4]["params"]["value"], "ask");
    // The mode took again: the next prompt goes.
    assert!(!refused(&events, PermissionPolicy::Ask));
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// The lines of a session in `ask` (vibe-acp 2.26.1's shape) that answered
/// one turn: `session/new` in accept-edits, the mode option set to `ask`
/// (id 3) and read back, the prompt (id 4) answered.
fn mode_session_lines() -> Vec<String> {
    vec![
        r#">> {"method": "initialize"}"#.into(),
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#.into(),
        r#">> {"method": "session/new"}"#.into(),
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s1","modes":{{"currentModeId":"accept-edits","availableModes":[{{"id":"ask"}},{{"id":"accept-edits"}},{{"id":"auto-approve"}}]}},"configOptions":{}}}}}"#,
            mode_option("accept-edits")
        ),
        r#">> {"method": "session/set_config_option"}"#.into(),
        mode_answer(3, "ask"),
        r#">> {"method": "session/prompt"}"#.into(),
        r#"{"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}"#.into(),
    ]
}

/// vibe-acp's mode option, in `current`.
fn mode_option(current: &str) -> String {
    format!(
        r#"[{{"id":"mode","category":"mode","type":"select","currentValue":"{current}","options":[{{"value":"ask"}},{{"value":"accept-edits"}},{{"value":"auto-approve"}}]}}]"#
    )
}

/// The answer to mode request `id`, reading back `current`.
fn mode_answer(id: u64, current: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"configOptions":{}}}}}"#,
        mode_option(current)
    )
}

/// What the driver sent: each line's method and, for a mode request, the
/// mode it asked for.
fn sent_modes(fake: &Fake) -> Vec<String> {
    fake.sent_lines()
        .iter()
        .map(|v| {
            let method = v["method"].as_str().unwrap_or("answer");
            match v["params"]["value"].as_str() {
                Some(mode) if method == "session/set_config_option" => format!("{method}={mode}"),
                _ => method.to_string(),
            }
        })
        .collect()
}

/// Events until the driver has handled every command sent before: it
/// takes them in order, and answers `RemoteControl` (which no ACP agent
/// has) with a notice at once.
async fn barrier(handle: &mut SessionHandle) -> Vec<AgentEvent> {
    handle
        .send(SessionCommand::RemoteControl {
            enabled: true,
            name: None,
        })
        .await
        .unwrap();
    events_until(
        handle,
        |e| matches!(e, AgentEvent::Notice(n) if n.ends_with("cannot go on remote control")),
    )
    .await
}

/// A line the fake sends for a test to wait on: a command list, which
/// comes out as `CapabilitiesChanged`.
const MARKER: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"marker","description":"marker"}]}}}"#;

fn is_marker(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::CapabilitiesChanged(CapsUpdate { commands: Some(c), .. })
        if c.iter().any(|c| c.name == "marker"))
}

fn turn_started(events: &[AgentEvent]) -> bool {
    events.iter().any(|e| matches!(e, AgentEvent::TurnStarted))
}

/// #211: in `ask`, the policy goes to bypass and back to ask before the
/// agent answers the mode request for bypass (`auto-approve`). The session
/// last said `ask`, but the agent takes `auto-approve`: the mode is applied
/// again from that answer, so `ask` is set (only then: never two mode
/// requests out at once), and the prompt waits for its answer.
#[tokio::test]
async fn acp_sets_the_mode_again_when_the_policy_changes_back_before_an_answer() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let mut lines = mode_session_lines();
    lines.extend([
        r#">> {"method": "session/set_config_option", "value": "auto-approve"}"#.into(),
        // Held until the policy is back at ask and the prompt waits.
        "# gate".into(),
        mode_answer(5, "auto-approve"),
        r#">> {"method": "session/set_config_option", "value": "ask"}"#.into(),
        // Everything the driver did before the second answer comes first.
        MARKER.into(),
        mode_answer(6, "ask"),
        r#">> {"method": "session/prompt"}"#.into(),
        r#"{"jsonrpc":"2.0","id":7,"result":{"stopReason":"end_turn"}}"#.into(),
    ]);
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let (mut cfg, gate) = gated_config(&fake, &lines);
    cfg.policy = PermissionPolicy::Ask;
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    run_turn(&mut handle, |_| None).await;

    for policy in [PermissionPolicy::Bypass, PermissionPolicy::Ask] {
        handle
            .send(SessionCommand::SetPolicy(policy))
            .await
            .unwrap();
    }
    handle.send(SessionCommand::turn("y")).await.unwrap();
    let mut before = barrier(&mut handle).await;
    std::fs::write(&gate, "").unwrap();
    before.extend(events_until(&mut handle, is_marker).await);
    // The answer for bypass has come and `ask` has gone out; until its
    // answer no prompt goes.
    assert!(!turn_started(&before), "{before:?}");
    let events = run_turn(&mut handle, |_| None).await;
    assert!(turn_started(&events), "{events:?}");
    assert_eq!(turn_error(&events), None, "{before:?} {events:?}");
    assert!(!refused(&before, PermissionPolicy::Ask) && !refused(&events, PermissionPolicy::Ask));
    assert_eq!(
        sent_modes(&fake),
        [
            "initialize",
            "session/new",
            "session/set_config_option=ask",
            "session/prompt",
            "session/set_config_option=auto-approve",
            "session/set_config_option=ask",
            "session/prompt",
        ]
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// #211: the policy goes to bypass and back to ask during a turn. The
/// answer for bypass says the agent is in `auto-approve`, which can run
/// what ask asks about: the turn is stopped, `ask` set again, and the next
/// prompt goes only once it is read back.
#[tokio::test]
async fn acp_stops_a_turn_when_a_late_mode_answer_is_wider_than_the_policy() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let mut lines = mode_session_lines();
    lines.extend([
        r#">> {"method": "session/prompt"}"#.into(),
        r#">> {"method": "session/set_config_option", "value": "auto-approve"}"#.into(),
        "# gate".into(),
        mode_answer(6, "auto-approve"),
        r#">> {"method": "session/cancel"}"#.into(),
        r#">> {"method": "session/set_config_option", "value": "ask"}"#.into(),
        mode_answer(7, "ask"),
        r#"{"jsonrpc":"2.0","id":5,"result":{"stopReason":"cancelled"}}"#.into(),
        r#">> {"method": "session/prompt"}"#.into(),
        r#"{"jsonrpc":"2.0","id":8,"result":{"stopReason":"end_turn"}}"#.into(),
    ]);
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let (mut cfg, gate) = gated_config(&fake, &lines);
    cfg.policy = PermissionPolicy::Ask;
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    run_turn(&mut handle, |_| None).await;

    handle.send(SessionCommand::turn("y")).await.unwrap();
    events_until(&mut handle, |e| matches!(e, AgentEvent::TurnStarted)).await;
    for policy in [PermissionPolicy::Bypass, PermissionPolicy::Ask] {
        handle
            .send(SessionCommand::SetPolicy(policy))
            .await
            .unwrap();
    }
    barrier(&mut handle).await;
    std::fs::write(&gate, "").unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(interrupted(&events), "{events:?}");
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Notice(n)
            if n.contains("is in mode 'auto-approve' (the policy changed to ask while a mode was being set)")
                && n.contains("the turn is stopped"))),
        "{events:?}"
    );
    handle.send(SessionCommand::turn("z")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert_eq!(turn_error(&events), None, "{events:?}");
    assert_eq!(
        sent_modes(&fake),
        [
            "initialize",
            "session/new",
            "session/set_config_option=ask",
            "session/prompt",
            "session/prompt",
            "session/set_config_option=auto-approve",
            "session/cancel",
            "session/set_config_option=ask",
            "session/prompt",
        ]
    );
    assert!(!refused(&events, PermissionPolicy::Ask));
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// `--print` fails where the TUI would ask for another policy.
#[tokio::test]
async fn headless_fails_when_an_acp_agent_cannot_hold_ask() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let handle = acp_harness().start_session(cfg).unwrap();
    let mut run = headless_run(&fake, unharness::headless::Format::Json, false);
    tokio::time::timeout(
        Duration::from_secs(10),
        unharness::headless::drive(handle, "pong?".into(), &mut run),
    )
    .await
    .expect("the run ends");
    assert_eq!(run.finish(), 1);
    let result: Value = serde_json::from_slice(&run.out).unwrap();
    assert_eq!(result["status"], "error");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|e| e.contains("has no `modes` map")),
        "{result}"
    );
    // No prompt went; the session may be closed by now (vibe takes
    // `session/close`).
    let methods = sent_methods(&fake);
    assert_eq!(methods[..2], ["initialize", "session/new"]);
    assert!(
        methods[2..].iter().all(|m| m == "session/close"),
        "{methods:?}"
    );
}

#[tokio::test]
async fn pi_rewind_forks_and_holds_the_next_turn_until_the_fork_is_in_place() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/rewind.jsonl");
    let harness = pi_harness(&fake);
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
async fn pi_interrupt_drops_a_turn_held_for_a_fork() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/rewind.jsonl");
    let mut handle = pi_harness(&fake)
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
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
    handle
        .send(SessionCommand::Rewind {
            anchor: anchors[1].clone(),
        })
        .await
        .unwrap();
    handle.send(SessionCommand::turn("which?")).await.unwrap();
    handle.send(SessionCommand::Interrupt).await.unwrap();
    // The held turn ends at once, interrupted; pi never sees it, and is not
    // sent an abort for a turn it does not have.
    let events = run_turn(&mut handle, |_| None).await;
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted
        })
    );
    // Let the fork answer and the driver re-read the session.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !fake
        .sent_lines()
        .iter()
        .skip_while(|v| v["type"] != "fork")
        .any(|v| v["type"] == "get_fork_messages")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fork never answered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let sent = fake.sent_lines();
    assert!(!sent.iter().any(|v| v["message"] == "which?"), "{sent:?}");
    assert!(!sent.iter().any(|v| v["type"] == "abort"), "{sent:?}");
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
                    ..Default::default()
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
            workspace_root: None,
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
    headless_run_with(
        fake,
        format,
        unharness::core::SubagentSupport {
            report_turn,
            ..Default::default()
        },
    )
}

fn headless_run_with(
    fake: &Fake,
    format: unharness::headless::Format,
    subagents: unharness::core::SubagentSupport,
) -> unharness::headless::Headless<Vec<u8>, Vec<u8>> {
    let rules =
        unharness::core::Rules::load_in(&fake._tmp.path().join("config"), Some(fake._tmp.path()))
            .unwrap();
    let mut run = unharness::headless::Headless::new(
        format,
        rules,
        fake._tmp.path().to_path_buf(),
        "fake",
        subagents,
        Vec::new(),
        Vec::new(),
    );
    run.begin(&unharness::headless::RunInfo {
        harness: "fake".into(),
        policy: "ask".into(),
        sandbox: "off".into(),
        cwd: fake._tmp.path().to_path_buf(),
        resume: None,
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

/// vibe-acp 2.26.1 tells a subagent's end only when the agent waits for
/// it: a print run does not wait for one the agent left at work, and says
/// so (#170).
#[tokio::test]
async fn headless_does_not_wait_for_a_vibe_subagent_nobody_waits_for() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_subagent_background.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let vibe = vibe_harness_recorded_in_ask(&fake.profiles());
    let handle = vibe.start_session(cfg).unwrap();
    let mut run = headless_run_with(
        &fake,
        unharness::headless::Format::Json,
        vibe.capabilities().subagents,
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        unharness::headless::drive(handle, "spawn pinger".into(), &mut run),
    )
    .await
    .expect("the run ends");
    assert_eq!(run.finish(), 0);
    let result: Value = serde_json::from_slice(&run.out).unwrap();
    assert_eq!(result["status"], "done");
    assert_eq!(result["text"], "spawned");
    let err = String::from_utf8_lossy(&run.err);
    assert!(
        err.contains("not waiting for 1 subagent(s): fake tells a subagent's end only when"),
        "{err}"
    );
}

/// `--print --resume <id>` of a session the agent no longer has: pi 1.1.0
/// creates an empty one under the id (`fixtures/resume_missing.jsonl`),
/// vibe-acp 2.26.1 answers `session/load` with "Session not found" and the
/// driver starts a new one (`vibe_acp_load_missing.jsonl`). Either fails
/// the run with the reason rather than answer from the new session, and
/// pi is never sent the prompt (#214).
#[tokio::test]
async fn headless_fails_a_resume_the_agent_replaced_with_a_new_session() {
    if !python_available() {
        return;
    }
    let resuming = |fake: &Fake, format, id: &str| {
        let rules = unharness::core::Rules::load_in(
            &fake._tmp.path().join("config"),
            Some(fake._tmp.path()),
        )
        .unwrap();
        let mut run = unharness::headless::Headless::new(
            format,
            rules,
            fake._tmp.path().to_path_buf(),
            "fake",
            Default::default(),
            Vec::new(),
            Vec::new(),
        );
        run.begin(&unharness::headless::RunInfo {
            harness: "fake".into(),
            policy: "ask".into(),
            sandbox: "off".into(),
            cwd: fake._tmp.path().to_path_buf(),
            resume: Some(id.into()),
        });
        run
    };

    let pi_missing = "3c0c2f6e-5b1a-4d2e-9f7a-1b2c3d4e5f62";
    let fake = Fake::new();
    let fixture = repo().join("src/harness/pi/fixtures/resume_missing.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some(pi_missing.into());
    let handle = pi_harness(&fake).start_session(cfg).unwrap();
    let mut run = resuming(&fake, unharness::headless::Format::Json, pi_missing);
    tokio::time::timeout(
        Duration::from_secs(10),
        unharness::headless::drive(handle, "pong?".into(), &mut run),
    )
    .await
    .expect("the run ends");
    assert_eq!(run.finish(), 1);
    let result: Value = serde_json::from_slice(&run.out).unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(
        result["error"],
        format!(
            "fake: could not resume the session (pi has none under its id, and started an \
             empty one); stopped, since the session started in its place knows nothing of \
             {pi_missing}"
        )
    );
    // pi made an empty session under the id: it is not handed out.
    assert_eq!(result["session_id"], Value::Null);
    // The prompt waited for pi's listing and was never sent: pi saves a
    // prompt under the id it was given, which would make the next resume
    // of that id pass for one.
    let sent = fake.sent_lines();
    assert_eq!(
        sent.iter()
            .filter_map(|v| v["type"].as_str())
            .collect::<Vec<_>>(),
        [
            "get_state",
            "get_commands",
            "get_fork_messages",
            "get_session_stats"
        ],
        "{sent:?}"
    );

    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_load_missing.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.resume = Some(MISSING_SESSION.into());
    let handle = vibe_acp().start_session(cfg).unwrap();
    let mut run = resuming(&fake, unharness::headless::Format::Text, MISSING_SESSION);
    tokio::time::timeout(
        Duration::from_secs(10),
        unharness::headless::drive(handle, "Reply with the single word: pong".into(), &mut run),
    )
    .await
    .expect("the run ends");
    assert_eq!(run.finish(), 1);
    assert!(run.out.is_empty(), "{}", String::from_utf8_lossy(&run.out));
    let err = String::from_utf8_lossy(&run.err);
    assert!(
        err.contains(&format!(
            "fake: could not resume the session (Session not found: {MISSING_SESSION}); \
             stopped, since the session started in its place knows nothing of {MISSING_SESSION}"
        )),
        "{err}"
    );
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
    let handle = claude_acp().start_session(cfg).unwrap();
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

/// `--print` has nobody to answer an ACP agent's question: it is
/// declined, as Claude's AskUserQuestion is dismissed, and the run goes on.
#[tokio::test]
async fn headless_declines_an_acp_question() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_question_multi.jsonl");
    let handle = vibe_as_recorded(&fake, &fixture);
    let mut run = headless_run(&fake, unharness::headless::Format::StreamJson, false);
    tokio::time::timeout(
        Duration::from_secs(10),
        unharness::headless::drive(handle, "colours".into(), &mut run),
    )
    .await
    .expect("the run ends");
    assert_eq!(run.finish(), 0);
    let lines: Vec<Value> = String::from_utf8_lossy(&run.out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(lines.iter().any(|l| l["type"] == "permission_request"
        && l["kind"] == "question"
        && l["questions"][0]["multi"] == true));
    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "permission_answered" && l["decision"] == "dismiss")
    );
    assert!(
        lines
            .iter()
            .any(|l| l["type"] == "capabilities_changed" && l["ask_user_question"] == true)
    );
    assert_eq!(
        answers_to(&fake.sent_lines(), 0)[0]["result"],
        serde_json::json!({"action": "decline"})
    );
}

/// A fake that replays `lines` and holds at its `# gate` until the file
/// returned is created.
fn gated_config(fake: &Fake, lines: &[&str]) -> (SessionConfig, PathBuf) {
    let fixture = fake._tmp.path().join("gated.jsonl");
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    let gate = fake._tmp.path().join("gate");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Bypass, true);
    cfg.model = None;
    cfg.env
        .push(("UNHARNESS_FAKE_GATE".into(), gate.display().to_string()));
    (cfg, gate)
}

/// Events up to the one `stop` matches.
async fn events_until(
    handle: &mut SessionHandle,
    stop: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let ev = next_event(handle).await;
        let done = stop(&ev);
        events.push(ev);
        if done {
            return events;
        }
    }
}

/// Every command sent before has been handled once this returns: an
/// answer to no request comes back as a notice.
async fn settle(handle: &mut SessionHandle) -> Vec<AgentEvent> {
    handle
        .send(SessionCommand::RespondPermission {
            id: "settle".into(),
            decision: PermissionDecision::Deny {
                reason: String::new(),
            },
        })
        .await
        .unwrap();
    events_until(handle, |e| matches!(e, AgentEvent::Notice(_))).await
}

fn interrupted(events: &[AgentEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::Interrupted
            }
        )
    })
}

fn codex_app_server() -> unharness::harness::codex::CodexHarness {
    unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    )
}

// Ctrl+C in `--print` reaches the CLI only as `Interrupt`.
#[tokio::test]
async fn codex_interrupts_a_turn_sent_before_it_had_an_id() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (cfg, gate) = gated_config(
        &fake,
        &[
            r#">> {"method": "initialize"}"#,
            r#"{"id":1,"result":{}}"#,
            r#">> {"method": "initialized"}"#,
            r#">> {"method": "thread/start"}"#,
            r#"{"id":2,"result":{"thread":{"id":"t1"}}}"#,
            r#">> {"method": "skills/list"}"#,
            r#">> {"method": "turn/start"}"#,
            "# gate",
            r#"{"id":3,"result":{"turn":{"id":"u1","status":"inProgress"}}}"#,
            r#">> {"method": "turn/interrupt"}"#,
            r#"{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","status":"interrupted"}}}"#,
        ],
    );
    let mut handle = codex_app_server().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    fake.sent_lines_eventually(5).await;
    handle.send(SessionCommand::Interrupt).await.unwrap();
    let before = settle(&mut handle).await;
    assert!(!interrupted(&before), "{before:?}");
    std::fs::write(&gate, "").unwrap();

    let events = events_until(&mut handle, |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    assert!(interrupted(&events), "{events:?}");
    let sent = fake.sent_lines();
    let interrupt = sent
        .iter()
        .find(|l| l["method"] == "turn/interrupt")
        .expect("turn/interrupt");
    assert_eq!(interrupt["params"]["turnId"], "u1");
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

#[tokio::test]
async fn codex_drops_a_turn_interrupted_while_it_waits_for_the_thread() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (cfg, gate) = gated_config(
        &fake,
        &[
            r#">> {"method": "initialize"}"#,
            r#"{"id":1,"result":{}}"#,
            r#">> {"method": "initialized"}"#,
            r#">> {"method": "thread/start"}"#,
            "# gate",
            r#"{"id":2,"result":{"thread":{"id":"t1"}}}"#,
            r#"{"method":"thread/started","params":{"thread":{"id":"t1"}}}"#,
        ],
    );
    let mut handle = codex_app_server().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    fake.sent_lines_eventually(3).await;
    handle.send(SessionCommand::Interrupt).await.unwrap();
    let events = events_until(&mut handle, |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    assert!(interrupted(&events), "{events:?}");

    // The thread comes, and the turn does not go out on it.
    std::fs::write(&gate, "").unwrap();
    events_until(&mut handle, |e| {
        matches!(e, AgentEvent::SessionStarted { .. })
    })
    .await;
    handle.send(SessionCommand::Shutdown).await.unwrap();
    events_until(&mut handle, |e| {
        matches!(e, AgentEvent::ProcessExited { .. })
    })
    .await;
    let sent = fake.sent_lines();
    assert!(
        !sent.iter().any(|l| l["method"] == "turn/start"),
        "{sent:?}"
    );
}

#[tokio::test]
async fn acp_drops_a_turn_interrupted_while_it_waits_for_the_session() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (cfg, gate) = gated_config(
        &fake,
        &[
            r#">> {"method": "initialize"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            r#">> {"method": "session/new"}"#,
            "# gate",
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#,
        ],
    );
    let mut handle = acp_harness().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    fake.sent_lines_eventually(2).await;
    handle.send(SessionCommand::Interrupt).await.unwrap();
    let events = events_until(&mut handle, |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    assert!(interrupted(&events), "{events:?}");

    std::fs::write(&gate, "").unwrap();
    events_until(&mut handle, |e| {
        matches!(e, AgentEvent::SessionStarted { .. })
    })
    .await;
    handle.send(SessionCommand::Shutdown).await.unwrap();
    events_until(&mut handle, |e| {
        matches!(e, AgentEvent::ProcessExited { .. })
    })
    .await;
    let sent = fake.sent_lines();
    assert!(
        !sent.iter().any(|l| l["method"] == "session/prompt"),
        "{sent:?}"
    );
}

/// Every line `p` writes, until it exits, and its exit code.
#[cfg(target_os = "linux")]
async fn run_out(mut p: unharness::core::process::LineProcess) -> (Vec<String>, Option<i32>) {
    use unharness::core::process::RawLine;
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), p.lines.recv()).await {
            Ok(Some(RawLine::Stdout(l) | RawLine::Stderr(l))) => out.push(l),
            Ok(Some(RawLine::Exited(code))) => return (out, code),
            other => panic!("{other:?} after {out:?}"),
        }
    }
}

// The reaper blocks signals to take them one at a time; what it starts
// gets none of that, and runs below it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cli_starts_below_its_reaper_with_no_signal_blocked() {
    if !python_available() {
        return;
    }
    let _fake = Fake::new();
    // Not a shell: dash clears the mask it is given, python keeps it.
    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg("-c").arg(
        "import os\n\
         print([l for l in open('/proc/self/status') if l.startswith('SigBlk')][0].strip())\n\
         print(open(f'/proc/{os.getppid()}/cmdline').read().replace(chr(0), ' '))",
    );
    let p = unharness::core::process::LineProcess::spawn(
        cmd,
        &unharness::core::sandbox::Sandbox::off(),
    )
    .unwrap();
    let (out, code) = run_out(p).await;
    assert_eq!(code, Some(0), "{out:?}");
    assert_eq!(out[0].split_whitespace().nth(1), Some("0000000000000000"));
    assert!(
        out.iter().any(|l| l.contains("__unharness-reap")),
        "{out:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cli_killed_by_a_signal_is_reported_so_through_its_reaper() {
    if !python_available() {
        return;
    }
    let _fake = Fake::new();
    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg("-c")
        .arg("import os, signal, time\nos.kill(os.getpid(), signal.SIGTERM)\ntime.sleep(5)");
    let p = unharness::core::process::LineProcess::spawn(
        cmd,
        &unharness::core::sandbox::Sandbox::off(),
    )
    .unwrap();
    let (out, code) = run_out(p).await;
    assert_eq!(code, None, "{out:?}");
}

// As without the reaper: an error from the spawn, not an exit.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cli_that_cannot_start_fails_to_spawn_also_through_its_reaper() {
    let _fake = Fake::new();
    let cmd = tokio::process::Command::new("/nonexistent/unharness-cli");
    let err = unharness::core::process::LineProcess::spawn(
        cmd,
        &unharness::core::sandbox::Sandbox::off(),
    )
    .err()
    .expect("spawned");
    let io = err.root_cause().downcast_ref::<std::io::Error>().unwrap();
    assert_eq!(io.kind(), std::io::ErrorKind::NotFound, "{err:#}");
}

#[tokio::test]
async fn claude_remote_control_turns_on_withdraws_remote_answers_and_turns_off() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    // Recorded on Claude Code 2.1.296: Remote Control on, a local turn, a
    // prompt typed on claude.ai/code whose Write was allowed there, off.
    let fixture = repo().join("src/harness/claude/fixtures/remote_control_permission.jsonl");
    let harness = unharness::harness::claude::ClaudeHarness::default();
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
    handle
        .send(SessionCommand::RemoteControl {
            enabled: true,
            name: Some("unharness-rec".into()),
        })
        .await
        .unwrap();
    handle
        .send(SessionCommand::turn("Reply with the single word: ready."))
        .await
        .unwrap();
    let mut events = Vec::new();
    // The local turn, then the remote one.
    let mut turns = 0;
    while turns < 2 {
        let ev = next_event(&mut handle).await;
        if matches!(ev, AgentEvent::TurnCompleted { .. }) {
            turns += 1;
        }
        events.push(ev);
    }
    assert!(
        events.contains(&AgentEvent::RemoteControl(RemoteControl::On {
            url: "https://claude.ai/code/session_REDACTED".into()
        }))
    );
    // The answer to our request is the driver's, not a generic one.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error(_) | AgentEvent::Notice(_)))
    );
    let asked = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::PermissionRequest(req) => Some(req.id.clone()),
            _ => None,
        })
        .expect("the remote turn's Write was asked about");
    assert!(events.contains(&AgentEvent::PermissionWithdrawn { id: asked }));

    handle
        .send(SessionCommand::RemoteControl {
            enabled: false,
            name: Some("ignored".into()),
        })
        .await
        .unwrap();
    loop {
        if next_event(&mut handle).await == AgentEvent::RemoteControl(RemoteControl::Off) {
            break;
        }
    }
    let sent: Vec<Value> = fake
        .sent_lines()
        .into_iter()
        .filter(|v| v["request"]["subtype"] == "remote_control")
        .map(|v| v["request"].clone())
        .collect();
    assert_eq!(
        sent,
        [
            serde_json::json!({"subtype": "remote_control", "enabled": true, "name": "unharness-rec"}),
            serde_json::json!({"subtype": "remote_control", "enabled": false}),
        ]
    );
}

/// The built-in vibe harness, its command the fake, writing its profiles
/// (#196) in `profiles`, never in the real state directory, and taking no
/// `VIBE_*` variable from the environment the tests run in (#219).
fn vibe_harness(profiles: &Path) -> unharness::harness::vibe::VibeHarness {
    unharness::harness::vibe::VibeHarness::from_settings(&unharness::config::HarnessSettings {
        command: Some(vec![fake_harness().to_string_lossy().into_owned()]),
        ..Default::default()
    })
    .unwrap()
    .writing_profiles_in(profiles)
    .inheriting_env(vec![])
}

/// The vibe harness for a recording made before #196, with vibe's own
/// `ask` behind `ask` (the map of that time, as a `modes` table): those
/// sessions list no `unharness-ask`.
fn vibe_harness_recorded_in_ask(profiles: &Path) -> unharness::harness::vibe::VibeHarness {
    unharness::harness::vibe::VibeHarness::from_settings(&unharness::config::HarnessSettings {
        command: Some(vec![fake_harness().to_string_lossy().into_owned()]),
        rest: toml::toml! {
            modes = { ask = "ask", auto = "smart-approve", bypass = "auto-approve" }
        },
        ..Default::default()
    })
    .unwrap()
    .writing_profiles_in(profiles)
    .inheriting_env(vec![])
}

/// A vibe session for `model` at `effort`, its VIBE_HOME an empty `home`.
fn vibe_config(fake: &Fake, home: &Path, model: &str, effort: &str) -> SessionConfig {
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_spawn_env.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = Some(ModelRef::new(HarnessId::VIBE, "agent", model));
    cfg.effort = Some(effort.into());
    cfg.env.extend([
        (
            "UNHARNESS_FAKE_ENV".to_string(),
            "VIBE_ACTIVE_MODEL,VIBE_MODELS".to_string(),
        ),
        ("VIBE_HOME".to_string(), home.to_string_lossy().into_owned()),
    ]);
    cfg
}

/// What the fake logged: each process's environment (`env`) and the
/// methods the driver sent, in order.
fn env_and_methods(fake: &Fake) -> Vec<String> {
    fake.sent_lines()
        .iter()
        .filter_map(|v| match (v.get("env"), v["method"].as_str()) {
            (Some(env), _) => Some(format!("env {env}")),
            (None, Some(m)) if m == "session/set_config_option" => {
                Some(format!("{m} {}", v["params"]["configId"]))
            }
            (None, Some(m)) => Some(m.to_string()),
            _ => None,
        })
        .collect()
}

/// vibe-acp 2.26.1 writes a model or thinking level set over ACP into its
/// own config.toml (#164): the vibe harness puts them in the agent's
/// environment instead and never sends either option. The level needs
/// the model's name, which only a session says: the first process of a
/// run is started again once its session has said it, before anything
/// is sent to it; a later session of the run starts with it.
#[tokio::test]
async fn vibe_chooses_model_and_effort_at_spawn_never_by_option() {
    if !python_available() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let vibe = vibe_harness_recorded_in_ask(&home.path().join("uh-profiles"));
    let entry = r#"[{"alias":"mistral-medium-3.5","name":"mistral-vibe-cli-latest","provider":"mistral","thinking":"off"}]"#;
    let model_only = r#"env {"VIBE_ACTIVE_MODEL":"mistral-medium-3.5","VIBE_MODELS":null}"#;
    let with_entry = format!(
        r#"env {{"VIBE_ACTIVE_MODEL":"mistral-medium-3.5","VIBE_MODELS":{}}}"#,
        serde_json::to_string(entry).unwrap()
    );

    let fake = Fake::new();
    let cfg = vibe_config(&fake, home.path(), "mistral-medium-3.5", "off");
    let mut handle = vibe.start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("Reply with the single word: pong"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "ong")),
        "{events:?}"
    );
    // The first process's session is never announced.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::SessionStarted { .. }))
            .count(),
        1
    );
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Notice(_))));
    assert_eq!(
        env_and_methods(&fake),
        [
            model_only,
            "initialize",
            "session/new",
            &with_entry,
            "initialize",
            "session/new",
            "session/set_config_option \"mode\"",
            "session/prompt",
        ]
    );
    shut_down(&mut handle).await;

    // The next session of the run starts with the entry.
    let fake = Fake::new();
    let cfg = vibe_config(&fake, home.path(), "mistral-medium-3.5", "off");
    let mut handle = vibe.start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("again")).await.unwrap();
    run_turn(&mut handle, |_| None).await;
    assert_eq!(
        env_and_methods(&fake),
        [
            with_entry.as_str(),
            "initialize",
            "session/new",
            "session/set_config_option \"mode\"",
            "session/prompt",
        ]
    );
    // A change sent to the running session is not passed on: the session
    // keeps what it last ran, so the conversation is to go on in a new one
    // (`SessionRenewed`), and nothing goes to vibe (#275: before, the TUI
    // decided this, and the driver only said it applied from the next
    // start).
    handle
        .send(SessionCommand::SetEffort(Some("high".into())))
        .await
        .unwrap();
    handle
        .send(SessionCommand::SetModel(ModelRef::new(
            HarnessId::VIBE,
            "agent",
            "other",
        )))
        .await
        .unwrap();
    handle
        .send(SessionCommand::SetProvider("lan".into()))
        .await
        .unwrap();
    let why = unharness::harness::vibe::KEEPS_CHOICES;
    for choice in ["effort high", "model other", "provider lan"] {
        assert_eq!(
            next_event(&mut handle).await,
            AgentEvent::SessionRenewed {
                reason: format!("{choice}: {why}")
            }
        );
    }
    let sent = fake.sent_lines();
    assert!(
        !sent
            .iter()
            .any(|v| v["params"]["configId"] == "model" || v["params"]["configId"] == "thinking")
    );
    assert_eq!(sent.last().unwrap()["method"], "session/prompt");
    shut_down(&mut handle).await;
}

/// A fork (#160) whose first process is started again for its level
/// (#164): the reattached source's answer names the model, which is learned
/// before the fork is made, so that only the process started again forks.
/// Before #222 the first process forked too, and its fork was left in
/// vibe's store, never closed.
#[tokio::test]
async fn vibe_forks_only_in_the_process_started_again_for_its_level() {
    if !python_available() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let vibe = vibe_harness_recorded_in_ask(&home.path().join("uh-profiles"));
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_fork.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = Some(ModelRef::new(
        HarnessId::VIBE,
        "agent",
        "mistral-medium-3.5",
    ));
    cfg.effort = Some("off".into());
    cfg.resume = Some(VIBE_FORK_SOURCE.into());
    cfg.fork = true;
    cfg.env.extend([
        (
            "UNHARNESS_FAKE_ENV".to_string(),
            "VIBE_ACTIVE_MODEL,VIBE_MODELS".to_string(),
        ),
        (
            "VIBE_HOME".to_string(),
            home.path().to_string_lossy().into_owned(),
        ),
    ]);
    let mut handle = vibe.start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("What was the code word?"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let started: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(started, [VIBE_FORK]);
    let said: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(said, "pineapple", "{events:?}");

    let entry = r#"[{"alias":"mistral-medium-3.5","name":"mistral-vibe-cli-latest","provider":"mistral","thinking":"off"}]"#;
    let with_entry = format!(
        r#"env {{"VIBE_ACTIVE_MODEL":"mistral-medium-3.5","VIBE_MODELS":{}}}"#,
        serde_json::to_string(entry).unwrap()
    );
    assert_eq!(
        env_and_methods(&fake),
        [
            r#"env {"VIBE_ACTIVE_MODEL":"mistral-medium-3.5","VIBE_MODELS":null}"#,
            "initialize",
            "session/fork",
            "session/load",
            &with_entry,
            "initialize",
            "session/fork",
            "session/load",
            "session/fork",
            "session/set_config_option \"mode\"",
            "session/prompt",
        ]
    );
    shut_down(&mut handle).await;
}

/// vibe-acp 2.26.1's subagents through the built-in harness under `ask`
/// (#170): the spawn starts one, the child's write, asked about by an id
/// no update named, reaches the user as a request no rule can know, and
/// the wait's answer ends it with its report.
#[tokio::test]
async fn vibe_reports_a_subagent_and_asks_for_its_write() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_subagent.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let mut handle = vibe_harness_recorded_in_ask(&fake.profiles())
        .start_session(cfg)
        .unwrap();
    handle
        .send(SessionCommand::turn("use the scribe subagent"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then_some(PermissionDecision::Allow {
            updated_input: None,
        })
    })
    .await;
    let spawn = "effect-f30db937-2271-52a4-9828-ce2c89567293";
    let summaries: Vec<String> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::SubagentStarted { .. }
                    | AgentEvent::SubagentEnded { .. }
                    | AgentEvent::PermissionRequest(_)
            )
        })
        .map(|e| e.summary())
        .collect();
    assert_eq!(
        summaries,
        [
            format!(r#"SubagentStarted id={spawn} kind=scribe "scribe""#),
            "PermissionRequest id=0 ToolUse tool [opaque]".to_string(),
            format!(
                r#"SubagentEnded id={spawn} completed "Done. Created `note.txt` with the word \"hello\".""#
            ),
        ]
    );
    let answers: Vec<Value> = fake
        .sent_lines()
        .into_iter()
        .filter_map(|v| v["result"]["outcome"]["optionId"].as_str().map(Value::from))
        .collect();
    assert_eq!(answers, ["allow_once"]);
    // The child's call ends with the answer: nothing more comes about it
    // (#198).
    let asked = events
        .iter()
        .position(|e| matches!(e, AgentEvent::PermissionRequest(_)))
        .unwrap();
    assert!(matches!(
        &events[asked + 1],
        AgentEvent::ToolCallResult { id, is_error: false, .. } if id == VIBE_CHILD_CALL
    ));
    assert_eq!(open_calls(&events), Vec::<String>::new());
    shut_down(&mut handle).await;
}

/// The call of a vibe subagent's request in `vibe_acp_subagent.jsonl`,
/// which no update names.
const VIBE_CHILD_CALL: &str = "effect-996cefd7-ea1a-59ec-94d3-077046b0e175";

/// The tool calls started in `events` and not ended.
fn open_calls(events: &[AgentEvent]) -> Vec<String> {
    let mut open = Vec::new();
    for ev in events {
        match ev {
            AgentEvent::ToolCallStarted { id, .. } => open.push(id.clone()),
            AgentEvent::ToolCallResult { id, .. } => open.retain(|o| o != id),
            _ => {}
        }
    }
    open
}

/// Denied, a vibe subagent's call no update names ends as denied.
#[tokio::test]
async fn vibe_a_denied_call_only_its_request_named_ends_denied() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_subagent.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let mut handle = vibe_harness_recorded_in_ask(&fake.profiles())
        .start_session(cfg)
        .unwrap();
    handle
        .send(SessionCommand::turn("use the scribe subagent"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then(|| PermissionDecision::Deny {
            reason: "no".into(),
        })
    })
    .await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallResult { id, output, is_error: true }
            if id == VIBE_CHILD_CALL && output == "denied"
    )));
    assert_eq!(open_calls(&events), Vec::<String>::new());
    shut_down(&mut handle).await;
}

/// Under bypass the request is answered without asking, and its call,
/// which no update names, ends with that answer.
#[tokio::test]
async fn acp_bypass_ends_a_call_only_its_request_named() {
    if !python_available() {
        return;
    }
    // No mode map: nothing is sent to set the mode, and the prompt is
    // the third request.
    let recorded =
        std::fs::read_to_string(repo().join("src/harness/acp/fixtures/vibe_acp_subagent.jsonl"))
            .unwrap();
    let lines: Vec<String> = recorded
        .lines()
        .filter(|l| !l.contains("session/set_config_option") && !l.contains(r#""id":3,"#))
        .map(|l| l.replace(r#""id":4,"result""#, r#""id":3,"result""#))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("subagent.jsonl");
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    let fake = Fake::new();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Bypass, true);
    cfg.model = None;
    let mut handle = acp_harness().start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("use the scribe subagent"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionRequest(_)))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallResult { id, is_error: false, .. } if id == VIBE_CHILD_CALL
    )));
    assert_eq!(open_calls(&events), Vec::<String>::new());
    shut_down(&mut handle).await;
}

/// Under auto vibe runs in its smart-approve mode (#174): its process is
/// started offering the mode and with new sessions in it, so its session
/// answers `currentModeId: "smart-approve"` and nothing is sent to set it.
/// What the classifier blocks comes back as a failed call; what it
/// escalates, here an edit inside the workspace (a repeat of a blocked
/// one), is the user's to answer, not allowed by the policy as an edit
/// is under accept-edits, and only "allow once" is sent.
#[tokio::test]
async fn vibe_auto_is_smart_approve_and_its_escalations_are_asked() {
    if !python_available() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_auto.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Auto, true);
    cfg.model = None;
    cfg.env.extend([
        (
            "UNHARNESS_FAKE_ENV".to_string(),
            "VIBE_DEFAULT_AGENT,VIBE_SMART_APPROVE_AVAILABLE".to_string(),
        ),
        ("VIBE_HOME".to_string(), home.path().display().to_string()),
    ]);
    let vibe = vibe_harness(&fake.profiles());
    assert!(
        vibe.capabilities()
            .supports_policy(PermissionPolicy::Auto)
            .is_some()
    );
    let mut handle = vibe.start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn(
            "Read alpha.txt, then append the line edited to it, then write the text hi into the file /tmp/l174/outside/x.txt.",
        ))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| {
        matches!(e, AgentEvent::PermissionRequest(_)).then(|| PermissionDecision::Allow {
            updated_input: None,
        })
    })
    .await;
    let asked = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::PermissionRequest(_)))
        .count();
    assert_eq!(asked, 1, "{events:?}");
    let blocked = events
        .iter()
        .filter(|e| {
            matches!(e, AgentEvent::ToolCallResult { is_error: true, output, .. }
                if output.starts_with("Smart approve blocked this action"))
        })
        .count();
    assert_eq!(blocked, 2);
    assert_eq!(
        env_and_methods(&fake),
        [
            r#"env {"VIBE_DEFAULT_AGENT":"smart-approve","VIBE_SMART_APPROVE_AVAILABLE":"true"}"#,
            "initialize",
            "session/new",
            "session/prompt",
        ]
    );
    let answers: Vec<Value> = fake
        .sent_lines()
        .into_iter()
        .filter_map(|v| v["result"]["outcome"]["optionId"].as_str().map(Value::from))
        .collect();
    assert_eq!(answers, ["allow_once"]);
    shut_down(&mut handle).await;
}

/// A vibe session that does not offer smart-approve (vibe-acp 2.26.1
/// outside the experiment's cohort lists ask, accept-edits and
/// auto-approve, `fixtures/vibe_acp_close.jsonl`) refuses auto, and the
/// turn fails rather than run in another mode; the TUI then takes the
/// nearest less permissive policy, accept-edits, whose mode (`ask`) is set.
#[tokio::test]
async fn vibe_auto_without_smart_approve_is_refused() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_close.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Auto, true);
    cfg.model = None;
    let mut handle = vibe_acp_with_auto().start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("write p.txt"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(refused(&events, PermissionPolicy::Auto), "{events:?}");
    let why = turn_error(&events).expect("the turn fails");
    assert!(why.contains("does not offer mode 'smart-approve'"), "{why}");
    assert_eq!(sent_methods(&fake), ["initialize", "session/new"]);
    handle
        .send(SessionCommand::SetPolicy(PermissionPolicy::AcceptEdits))
        .await
        .unwrap();
    let sent = fake.sent_lines_eventually(3).await;
    assert_eq!(sent[2]["method"], "session/set_config_option");
    assert_eq!(sent[2]["params"]["value"], "ask");
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// vibe's mode map with auto (`vibe::mode_map`), on the generic harness.
fn vibe_acp_with_auto() -> unharness::harness::acp::AcpHarness {
    acp_with_modes(&[
        (PermissionPolicy::Ask, "ask"),
        (PermissionPolicy::Auto, "smart-approve"),
        (PermissionPolicy::Bypass, "auto-approve"),
    ])
}

/// vibe falls back to its default model for one it does not have, and
/// resets a level the model does not offer, both without an error: the
/// session's answer is compared with the choice.
#[tokio::test]
async fn vibe_says_when_the_session_runs_another_model_or_level() {
    if !python_available() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    // The model and its name from the user's config: nothing to wait for.
    std::fs::write(
        home.path().join("config.toml"),
        "[[models]]\nalias = \"gone\"\nname = \"gone-model\"\nprovider = \"mistral\"\n",
    )
    .unwrap();
    let fake = Fake::new();
    let cfg = vibe_config(&fake, home.path(), "gone", "high");
    let mut handle = vibe_harness_recorded_in_ask(&fake.profiles())
        .start_session(cfg)
        .unwrap();
    handle.send(SessionCommand::turn("hi")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let notices: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices[0].contains("'mistral-medium-3.5', not 'gone'"));
    assert!(notices[1].contains("'off', not 'high'"));
    assert_eq!(
        env_and_methods(&fake)
            .iter()
            .filter(|l| *l == "initialize")
            .count(),
        1
    );
    shut_down(&mut handle).await;
}

/// A provider of `[harnesses.vibe.providers]` chosen for a session goes to
/// vibe in its environment as valid JSON with no key, in one process, and
/// the session's models are reported as that provider's, its own only
/// (`fixtures/vibe_acp_provider.jsonl`, vibe-acp 2.26.1 on a local
/// OpenAI-compatible server, #173). Neither option is sent.
#[tokio::test]
async fn vibe_runs_on_a_configured_provider_by_environment() {
    if !python_available() {
        return;
    }
    let settings: unharness::config::Config = toml::from_str(&format!(
        "[harnesses.vibe]\ncommand = [{:?}]\n\
         modes = {{ ask = \"ask\", auto = \"smart-approve\", bypass = \"auto-approve\" }}\n\
         [harnesses.vibe.providers.lan]\napi_base = \"https://LOCAL-LLM/v1\"\n\
         models = [\"qwen3.8-q5kxl-x2\"]\n",
        fake_harness().to_string_lossy()
    ))
    .unwrap();
    // Recorded before #196, in vibe's own `ask`: the map of that time.
    let fake = Fake::new();
    let vibe = unharness::harness::vibe::VibeHarness::from_settings(&settings.harnesses["vibe"])
        .unwrap()
        .writing_profiles_in(&fake.profiles())
        .inheriting_env(vec![]);
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_provider.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.provider = Some("lan".into());
    cfg.env.push((
        "UNHARNESS_FAKE_ENV".to_string(),
        "VIBE_PROVIDERS,VIBE_MODELS,VIBE_ACTIVE_MODEL".to_string(),
    ));
    let mut handle = vibe.start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("Reply with the single word: pong"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "pong")),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Notice(_))),
        "{events:?}"
    );
    let models: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::CapabilitiesChanged(CapsUpdate {
                models: Some(m),
                provider,
                ..
            }) => Some((provider.clone(), m.clone())),
            _ => None,
        })
        .collect();
    assert!(!models.is_empty());
    for (provider, list) in models {
        assert_eq!(provider.map(|p| p.0), Some("lan".to_string()));
        let listed: Vec<(String, String)> = list
            .iter()
            .map(|m| (m.model_ref.provider.0.clone(), m.model_ref.model.clone()))
            .collect();
        assert_eq!(
            listed,
            [("lan".to_string(), "qwen3.8-q5kxl-x2".to_string())]
        );
    }

    let logged = fake.sent_lines();
    let envs: Vec<&Value> = logged.iter().filter_map(|v| v.get("env")).collect();
    assert_eq!(envs.len(), 1, "one process");
    let json =
        |key: &str| -> Value { serde_json::from_str(envs[0][key].as_str().unwrap()).unwrap() };
    assert_eq!(
        json("VIBE_PROVIDERS"),
        serde_json::json!([{"name": "unharness-lan", "api_base": "https://LOCAL-LLM/v1",
            "api_style": "openai", "backend": "generic", "api_key_env_var": ""}])
    );
    assert_eq!(
        json("VIBE_MODELS"),
        serde_json::json!([{"alias": "qwen3.8-q5kxl-x2", "name": "qwen3.8-q5kxl-x2",
            "provider": "unharness-lan"}])
    );
    assert_eq!(envs[0]["VIBE_ACTIVE_MODEL"], "qwen3.8-q5kxl-x2");
    assert_eq!(
        env_and_methods(&fake)[1..],
        [
            "initialize",
            "session/new",
            "session/set_config_option \"mode\"",
            "session/prompt",
        ]
    );
    shut_down(&mut handle).await;

    // One it does not have is refused before anything starts.
    let fake = Fake::new();
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.provider = Some("cloud".into());
    assert!(vibe.start_session(cfg).is_err());
}

/// vibe-acp 2.26.1 with no API key (`fixtures/vibe_acp_not_signed_in.jsonl`).
fn vibe_not_signed_in() -> PathBuf {
    repo().join("src/harness/acp/fixtures/vibe_acp_not_signed_in.jsonl")
}

/// vibe as a config agent that names its sign-in command.
fn vibe_signing_in() -> unharness::harness::acp::AcpHarness {
    fake_acp(unharness::harness::acp::AcpProfile {
        sign_in: Some("vibe --setup".into()),
        ..asking_with_modes(VIBE_MODES)
    })
}

/// What a turn failed with for want of sign-in, once the process has gone
/// by itself (nothing was sent to end it): no other error on the way, the
/// stderr vibe wrote included.
async fn not_signed_in(handle: &mut SessionHandle) -> String {
    let events = until_exited(handle).await;
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
        "{events:?}"
    );
    turn_error(&events).expect("the turn fails").to_string()
}

/// vibe-acp 2.26.1 without a key answers `session/new` with the spec's
/// auth-required error. The turn fails with the methods it offered and
/// what to run; no other session is asked for, and the process is ended
/// so that the next prompt starts one that reads the key afresh.
#[tokio::test]
async fn acp_not_signed_in_tells_the_methods_and_what_to_run() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let mut cfg = fake.config(&vibe_not_signed_in(), PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.env.push(("UNHARNESS_FAKE_LINGER".into(), "0.2".into()));
    let mut handle = vibe_signing_in().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("say hi")).await.unwrap();
    let why = not_signed_in(&mut handle).await;
    assert_eq!(
        why,
        "fake-acp is not signed in (Missing API key for mistral provider). It offers: \
         \"Sign in through Mistral AI Studio\" (Sign into Mistral Vibe through your Mistral \
         AI Studio account). unharness does not sign agents in: run `vibe --setup` in a \
         terminal, then try again"
    );
    assert_eq!(
        sent_until_ended(&fake),
        ["initialize", "session/new", "ended"]
    );
    // No terminal sign-in is declared, so vibe offers none.
    assert_eq!(
        fake.sent_lines()[0]["params"]["clientCapabilities"],
        serde_json::json!({"elicitation": {"form": {}}})
    );
}

/// The same answer to a session started before the first prompt (the
/// TUI's): the failure is a turn's all the same, which the TUI shows and
/// holds the session on.
#[tokio::test]
async fn acp_not_signed_in_before_a_prompt_fails_the_same_way() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let mut cfg = fake.config(&vibe_not_signed_in(), PermissionPolicy::Bypass, true);
    cfg.model = None;
    let mut handle = acp_harness().start_session(cfg).unwrap();
    let why = not_signed_in(&mut handle).await;
    assert!(
        why.starts_with("fake-acp is not signed in (Missing API key")
            && why.contains("\"Sign in through Mistral AI Studio\"")
            && why.ends_with("sign in with fake-acp's own command line, then try again"),
        "{why}"
    );
    assert_eq!(sent_methods(&fake), ["initialize", "session/new"]);
}

/// A reattach (vibe loads) or a fork answered with auth-required is not
/// followed by a new session, which would not know the conversation, nor
/// by the reattach-and-fork-again a refused fork gets.
#[tokio::test]
async fn acp_not_signed_in_at_a_load_or_a_fork_starts_no_other_session() {
    if !python_available() {
        return;
    }
    for (fork, sent) in [(false, "session/load"), (true, "session/fork")] {
        let fake = Fake::new();
        let mut cfg = fake.config(&vibe_not_signed_in(), PermissionPolicy::Bypass, true);
        cfg.model = None;
        cfg.resume = Some("stored".into());
        cfg.fork = fork;
        let mut handle = vibe_signing_in().start_session(cfg).unwrap();
        handle.send(SessionCommand::turn("say hi")).await.unwrap();
        let why = not_signed_in(&mut handle).await;
        assert!(why.contains("run `vibe --setup`"), "{why}");
        assert_eq!(sent_methods(&fake), ["initialize", sent], "fork: {fork}");
    }
}

/// A prompt answered with auth-required fails its turn the same way, and
/// the process is ended.
#[tokio::test]
async fn acp_not_signed_in_at_a_prompt_fails_the_turn_and_ends_the_process() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (mut cfg, _gate) = gated_config(
        &fake,
        &[
            r#">> {"method": "initialize"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[{"id":"login","name":"Log in"}]}}"#,
            r#">> {"method": "session/new"}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#,
            r#">> {"method": "session/prompt"}"#,
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"Authentication required"}}"#,
        ],
    );
    cfg.env.push(("UNHARNESS_FAKE_LINGER".into(), "0.2".into()));
    let mut handle = acp_harness().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    let why = not_signed_in(&mut handle).await;
    assert!(
        why.starts_with(
            "fake-acp is not signed in (Authentication required). It offers: \"Log in\"."
        ),
        "{why}"
    );
    assert_eq!(
        sent_until_ended(&fake),
        ["initialize", "session/new", "session/prompt", "ended"]
    );
}

/// Another error at `session/new` is reported as before, with no sign-in
/// text of its own beyond the hint.
#[tokio::test]
async fn acp_other_session_errors_are_not_taken_for_sign_in() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let (cfg, _gate) = gated_config(
        &fake,
        &[
            r#">> {"method": "initialize"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            r#">> {"method": "session/new"}"#,
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"Internal error"}}"#,
        ],
    );
    let mut handle = acp_harness().start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("x")).await.unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    let why = turn_error(&events).expect("the turn fails");
    assert!(
        why.starts_with("Internal error (if this is an authentication error"),
        "{why}"
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// The built-in vibe harness under `ask` (vibe-acp 2.26.1, nothing in the
/// config): vibe's `ask` mode is set through its mode option before the
/// prompt goes, the write and the command are each asked about, known by
/// their path and command, and the user's answers go as given.
#[tokio::test]
async fn vibe_asks_before_a_write_and_a_command() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_allow_deny.jsonl");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    let mut handle = vibe_harness_recorded_in_ask(&fake.profiles())
        .start_session(cfg)
        .unwrap();
    handle
        .send(SessionCommand::turn("write a.txt, then touch b.txt"))
        .await
        .unwrap();
    // The write is allowed, the command denied.
    let events = run_turn(&mut handle, |e| match e {
        AgentEvent::PermissionRequest(req) => match &req.kind {
            PermissionKind::ToolUse {
                action: unharness::core::ToolAction::Edit { .. },
                ..
            } => Some(PermissionDecision::Allow {
                updated_input: None,
            }),
            _ => Some(PermissionDecision::Deny {
                reason: "no".into(),
            }),
        },
        _ => None,
    })
    .await;
    let actions: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::PermissionRequest(req) => match &req.kind {
                PermissionKind::ToolUse { action, .. } => Some(format!("{action:?}")),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        actions,
        [
            r#"Edit { paths: ["/WORKSPACE/a.txt"] }"#,
            r#"Shell { command: "touch /WORKSPACE/b.txt", cwd: None }"#,
        ]
    );
    // vibe reports the denied command as a failed call.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallResult { is_error: true, output, .. }
            if output.contains("denied")
    )));
    let sent = fake.sent_lines();
    let methods: Vec<&str> = sent.iter().filter_map(|v| v["method"].as_str()).collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    assert_eq!(sent[2]["params"]["configId"], "mode");
    assert_eq!(sent[2]["params"]["value"], "ask");
    let answers: Vec<&str> = sent
        .iter()
        .filter_map(|v| v["result"]["outcome"]["optionId"].as_str())
        .collect();
    assert_eq!(answers, ["allow_once", "reject_once"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// The nonce of the profiles `fixtures/vibe_acp_own_profile*.jsonl` were
/// recorded with: put in a test's profile directory, the sessions there
/// describe unharness's modes as its own.
const RECORDED_NONCE: &str = "47afaea60b0f49e4b85044ff3d752d7f";

/// A vibe home whose configuration lets everything run unasked in vibe's
/// own `ask` (#196): bypass on, `write_file` and `bash` always, and an
/// `agents/ask.toml` that replaces the built-in `ask`.
fn permissive_vibe_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "bypass_tool_permissions = true\n[tools.write_file]\npermission = \"always\"\n\
         [tools.bash]\npermission = \"always\"\n",
    )
    .unwrap();
    std::fs::create_dir(home.path().join("agents")).unwrap();
    std::fs::write(
        home.path().join("agents/ask.toml"),
        "[tools.write_file]\npermission = \"always\"\n",
    )
    .unwrap();
    home
}

/// A vibe session on the built-in harness under `policy`, its home
/// `home`, its profiles in the fake's directory with `nonce` (none: a new
/// one), the agent's `VIBE_AGENT_PATHS` logged.
fn vibe_own_profile(
    fake: &Fake,
    home: &Path,
    fixture: &str,
    policy: PermissionPolicy,
    nonce: Option<&str>,
) -> SessionHandle {
    if let Some(nonce) = nonce {
        std::fs::create_dir_all(fake.profiles()).unwrap();
        std::fs::write(fake.profiles().join("nonce"), format!("{nonce}\n")).unwrap();
    }
    let fixture = repo().join("src/harness/acp/fixtures").join(fixture);
    let mut cfg = fake.config(&fixture, policy, true);
    cfg.model = None;
    cfg.env.extend([
        (
            "UNHARNESS_FAKE_ENV".to_string(),
            "VIBE_AGENT_PATHS".to_string(),
        ),
        ("VIBE_HOME".to_string(), home.to_string_lossy().into_owned()),
    ]);
    vibe_harness(&fake.profiles()).start_session(cfg).unwrap()
}

/// The directory the agent was given in `VIBE_AGENT_PATHS`.
fn agent_paths(fake: &Fake) -> Vec<PathBuf> {
    let logged = fake
        .sent_lines()
        .into_iter()
        .find_map(|v| v.get("env").cloned())
        .expect("the environment was logged");
    serde_json::from_str(logged["VIBE_AGENT_PATHS"].as_str().unwrap()).unwrap()
}

fn tool_actions(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::PermissionRequest(req) => match &req.kind {
                PermissionKind::ToolUse { action, .. } => Some(format!("{action:?}")),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// `ask` on vibe is unharness's own profile (#196): written under the
/// state directory (here the test's), named in `VIBE_AGENT_PATHS`, set as
/// the session's mode once the session describes it with the nonce. With
/// a configuration under which vibe's own `ask` ran everything unasked,
/// the command and the write are asked about, and denied
/// (`fixtures/vibe_acp_own_profile.jsonl`, vibe-acp 2.26.1).
#[tokio::test]
async fn vibe_ask_is_unharness_own_profile() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let home = permissive_vibe_home();
    let mut handle = vibe_own_profile(
        &fake,
        home.path(),
        "vibe_acp_own_profile.jsonl",
        PermissionPolicy::Ask,
        Some(RECORDED_NONCE),
    );
    handle
        .send(SessionCommand::turn("touch b.txt, write a.txt"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| match e {
        AgentEvent::PermissionRequest(_) => Some(PermissionDecision::Deny {
            reason: "no".into(),
        }),
        _ => None,
    })
    .await;
    assert_eq!(
        tool_actions(&events),
        [
            r#"Shell { command: "touch /WORKSPACE/b.txt", cwd: None }"#,
            r#"Edit { paths: ["/WORKSPACE/a.txt"] }"#,
        ]
    );
    assert!(turn_error(&events).is_none(), "{events:?}");
    // The profiles, in the directory the agent was given.
    let dirs = agent_paths(&fake);
    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].starts_with(fake.profiles()), "{dirs:?}");
    let ask: toml::Table = std::fs::read_to_string(dirs[0].join("unharness-ask.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(ask["bypass_tool_permissions"].as_bool(), Some(false));
    assert_eq!(
        ask["tools"]["write_file"]["permission"].as_str(),
        Some("ask")
    );
    assert_eq!(ask["tools"]["bash"]["permission"].as_str(), Some("ask"));
    let sent = fake.sent_lines();
    let mode = sent
        .iter()
        .find(|v| v["method"] == "session/set_config_option")
        .unwrap();
    assert_eq!(mode["params"]["configId"], "mode");
    assert_eq!(mode["params"]["value"], "unharness-ask");
    let answers: Vec<&str> = sent
        .iter()
        .filter_map(|v| v["result"]["outcome"]["optionId"].as_str())
        .collect();
    assert_eq!(answers, ["reject_once", "reject_once"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// `accept-edits` is unharness's `unharness-accept-edits`: vibe runs the
/// write in the workspace with no request, and asks about the write
/// outside it and the command, which go to the user
/// (`fixtures/vibe_acp_own_profile_edits.jsonl`, vibe-acp 2.26.1).
#[tokio::test]
async fn vibe_accept_edits_is_unharness_own_profile() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let home = permissive_vibe_home();
    let mut handle = vibe_own_profile(
        &fake,
        home.path(),
        "vibe_acp_own_profile_edits.jsonl",
        PermissionPolicy::AcceptEdits,
        Some(RECORDED_NONCE),
    );
    handle
        .send(SessionCommand::turn("three things"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |e| match e {
        AgentEvent::PermissionRequest(_) => Some(PermissionDecision::Deny {
            reason: "no".into(),
        }),
        _ => None,
    })
    .await;
    assert_eq!(
        tool_actions(&events),
        [
            r#"Edit { paths: ["/tmp/l196/outside.txt"] }"#,
            r#"Shell { command: "touch /WORKSPACE/b.txt", cwd: None }"#,
        ]
    );
    let sent = fake.sent_lines();
    let mode = sent
        .iter()
        .find(|v| v["method"] == "session/set_config_option")
        .unwrap();
    assert_eq!(mode["params"]["value"], "unharness-accept-edits");
    let answers: Vec<&str> = sent
        .iter()
        .filter_map(|v| v["result"]["outcome"]["optionId"].as_str())
        .collect();
    assert_eq!(answers, ["reject_once", "reject_once"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A session whose `unharness-ask` is described with another nonce got
/// a profile of that name from somewhere vibe looks first (an
/// `agent_paths` directory of its config): `ask` is refused for it, no
/// prompt goes, and nothing is set. (The recording's nonce is not the
/// test's.)
#[tokio::test]
async fn vibe_refuses_ask_when_unharness_ask_is_not_its_own() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let home = permissive_vibe_home();
    let mut handle = vibe_own_profile(
        &fake,
        home.path(),
        "vibe_acp_own_profile.jsonl",
        PermissionPolicy::Ask,
        Some("00000000000000000000000000000000"),
    );
    handle
        .send(SessionCommand::turn("write a.txt"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(refused(&events, PermissionPolicy::Ask), "{events:?}");
    let why = turn_error(&events).expect("the turn fails");
    assert!(
        why.contains("its mode 'unharness-ask' is not the one unharness put in place"),
        "{why}"
    );
    assert!(why.contains(RECORDED_NONCE), "{why}");
    assert_eq!(sent_methods(&fake), ["initialize", "session/new"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// A profile named `unharness-ask` in an `agent_paths` directory of the
/// user's vibe config, which vibe reads before unharness's: `ask` is
/// refused at spawn with the file named, before the session's description
/// is even looked at (here it would match: the recorded nonce).
#[tokio::test]
async fn vibe_refuses_ask_when_its_config_lists_another_unharness_ask() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let home = permissive_vibe_home();
    let theirs = home.path().join("theirs");
    std::fs::create_dir(&theirs).unwrap();
    std::fs::write(theirs.join("unharness-ask.toml"), "").unwrap();
    let config = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("agent_paths = [{:?}]\n{config}", theirs.to_string_lossy()),
    )
    .unwrap();
    let mut handle = vibe_own_profile(
        &fake,
        home.path(),
        "vibe_acp_own_profile.jsonl",
        PermissionPolicy::Ask,
        Some(RECORDED_NONCE),
    );
    handle
        .send(SessionCommand::turn("write a.txt"))
        .await
        .unwrap();
    let events = run_turn(&mut handle, |_| None).await;
    assert!(refused(&events, PermissionPolicy::Ask), "{events:?}");
    let why = turn_error(&events).expect("the turn fails");
    assert!(
        why.contains("would take its mode 'unharness-ask' from"),
        "{why}"
    );
    let file = theirs.join("unharness-ask.toml");
    assert!(why.contains(&*file.to_string_lossy()), "{why}");
    assert_eq!(sent_methods(&fake), ["initialize", "session/new"]);
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

/// vibe's hooks' rewind alone: its `_rewind/to`.
struct Rewinding;

impl unharness::harness::acp::AgentHooks for Rewinding {
    fn rewind_method(&self) -> Option<&'static str> {
        Some(unharness::harness::vibe::REWIND_METHOD)
    }
}

/// The fake ACP agent as vibe-acp, which rewinds with its `_rewind/to`.
fn vibe_rewinding(fake: &Fake, fixture: &Path) -> SessionHandle {
    let mut cfg = fake.config(fixture, PermissionPolicy::Bypass, true);
    cfg.model = None;
    fake_acp(unharness::harness::acp::AcpProfile {
        hooks: std::sync::Arc::new(Rewinding),
        ..asking()
    })
    .start_session(cfg)
    .unwrap()
}

fn turn_anchors(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnAnchor { id } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// vibe-acp 2.26.1: each turn's anchor is the `messageId` it echoed the
/// prompt with, and a rewind to the second one goes as `_rewind/to` with
/// `restoreFiles: false` (files are unharness's), before the next turn,
/// which then knows only the first turn's code word.
#[tokio::test]
async fn acp_vibe_rewinds_through_its_extension() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_rewind.jsonl");
    let mut handle = vibe_rewinding(&fake, &fixture);

    handle.send(SessionCommand::turn("alpha")).await.unwrap();
    let first = run_turn(&mut handle, |_| None).await;
    assert_eq!(
        turn_anchors(&first),
        ["0b774d1b-00a0-4a1f-b468-92bad5480550"]
    );
    handle.send(SessionCommand::turn("beta")).await.unwrap();
    let second = run_turn(&mut handle, |_| None).await;
    let anchor = turn_anchors(&second);
    assert_eq!(anchor, ["f0539b32-76a6-4bb8-976b-af5b729addcd"]);

    handle
        .send(SessionCommand::Rewind {
            anchor: anchor[0].clone(),
        })
        .await
        .unwrap();
    handle
        .send(SessionCommand::turn("which code words?"))
        .await
        .unwrap();
    let third = run_turn(&mut handle, |_| None).await;
    assert!(
        !third
            .iter()
            .any(|e| matches!(e, AgentEvent::RewindFailed { .. } | AgentEvent::Error(_))),
        "{third:?}"
    );
    assert_eq!(
        turn_anchors(&third),
        ["29e7094a-5c47-4bec-8f20-7ae5dccb1d56"]
    );
    assert!(
        third
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "PINEAPPLE"))
    );
    shut_down(&mut handle).await;

    let sent = fake.sent_lines();
    let methods: Vec<&str> = sent.iter().filter_map(|v| v["method"].as_str()).collect();
    assert_eq!(
        methods[..6],
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/prompt",
            "_rewind/to",
            "session/prompt"
        ]
    );
    assert_eq!(
        sent[4]["params"],
        serde_json::json!({
            "sessionId": "e87bcb64-c3ef-212a-cc8a-efefd9580b99",
            "messageId": "f0539b32-76a6-4bb8-976b-af5b729addcd",
            "restoreFiles": false,
        })
    );
}

/// A rewind vibe refuses (an id it never gave: -32602) is `RewindFailed`
/// with its reason, and the turn after it still goes; a rewind to the
/// first message empties the session.
#[tokio::test]
async fn acp_vibe_rewind_refused_is_reported() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/acp/fixtures/vibe_acp_rewind_first.jsonl");
    let mut handle = vibe_rewinding(&fake, &fixture);

    handle.send(SessionCommand::turn("kiwi")).await.unwrap();
    let first = run_turn(&mut handle, |_| None).await;
    let anchor = turn_anchors(&first);
    assert_eq!(anchor, ["f0304197-cf76-4a4f-a15f-c2aae9798282"]);

    handle
        .send(SessionCommand::Rewind {
            anchor: "no-such-message".into(),
        })
        .await
        .unwrap();
    let failed = next_event(&mut handle).await;
    assert!(
        matches!(&failed, AgentEvent::RewindFailed { reason }
            if reason.contains("Rewindable history entry not found")),
        "{failed:?}"
    );
    handle
        .send(SessionCommand::Rewind {
            anchor: anchor[0].clone(),
        })
        .await
        .unwrap();
    handle
        .send(SessionCommand::turn("which code words?"))
        .await
        .unwrap();
    let second = run_turn(&mut handle, |_| None).await;
    assert!(
        !second
            .iter()
            .any(|e| matches!(e, AgentEvent::RewindFailed { .. } | AgentEvent::Error(_))),
        "{second:?}"
    );
    assert!(
        second
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "NONE"))
    );
    shut_down(&mut handle).await;
}

/// One anchor goes for every turn, empty for one whose message the agent
/// did not echo, so that the next turn's is not taken for it. Synthetic:
/// the rewind recording without the first turn's `user_message_chunk`.
#[tokio::test]
async fn acp_vibe_turn_without_an_echo_has_an_empty_anchor() {
    if !python_available() {
        return;
    }
    let recorded =
        std::fs::read_to_string(repo().join("src/harness/acp/fixtures/vibe_acp_rewind.jsonl"))
            .unwrap();
    let lines: Vec<&str> = recorded
        .lines()
        .filter(|l| !l.contains("\"messageId\":\"0b774d1b-00a0-4a1f-b468-92bad5480550\""))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("rewind.jsonl");
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    let fake = Fake::new();
    let mut handle = vibe_rewinding(&fake, &fixture);

    handle.send(SessionCommand::turn("alpha")).await.unwrap();
    let first = run_turn(&mut handle, |_| None).await;
    assert!(turn_anchors(&first).is_empty());
    assert_eq!(
        next_event(&mut handle).await,
        AgentEvent::TurnAnchor { id: String::new() }
    );
    handle.send(SessionCommand::turn("beta")).await.unwrap();
    let second = run_turn(&mut handle, |_| None).await;
    assert_eq!(
        turn_anchors(&second),
        ["f0539b32-76a6-4bb8-976b-af5b729addcd"]
    );
    handle.send(SessionCommand::Shutdown).await.unwrap();
}

// ---- Sending a prompt at once (`/now`, #333) ----

/// The events that come within `within`, none of which may match `not`.
async fn none_within(
    handle: &mut SessionHandle,
    within: Duration,
    not: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, handle.events.recv()).await {
        assert!(!not(&ev), "{} came too early", ev.summary());
        seen.push(ev);
    }
    seen
}

/// Events until (and with) the `n`th `TurnCompleted`, permission requests
/// allowed.
async fn until_turn_ends(handle: &mut SessionHandle, n: usize) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let mut ended = 0;
    while ended < n {
        let ev = next_event(handle).await;
        if let AgentEvent::PermissionRequest(req) = &ev {
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
        ended += matches!(ev, AgentEvent::TurnCompleted { .. }) as usize;
        events.push(ev);
    }
    events
}

/// `fixture` with each `(from, to)` replaced, in `fake`'s directory.
fn rewritten(fake: &Fake, fixture: &str, name: &str, edits: &[(&str, &str)]) -> PathBuf {
    let mut text = std::fs::read_to_string(repo().join(fixture)).unwrap();
    for (from, to) in edits {
        assert!(text.contains(from), "{from} not in {fixture}");
        text = text.replace(from, to);
    }
    let path = fake._tmp.path().join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// Claude in its tool call (allowed), then sent "now" at once; the events
/// from then until the CLI has said what came of it.
async fn claude_sent_now(fake: &Fake, fixture: &Path, then_interrupt: bool) -> Vec<AgentEvent> {
    let harness = unharness::harness::claude::ClaudeHarness::default();
    let mut handle = harness
        .start_session(fake.config(fixture, PermissionPolicy::Ask, true))
        .unwrap();
    handle.send(SessionCommand::turn("sleep")).await.unwrap();
    loop {
        let ev = next_event(&mut handle).await;
        if let AgentEvent::PermissionRequest(req) = &ev {
            handle
                .send(SessionCommand::RespondPermission {
                    id: req.id.clone(),
                    decision: PermissionDecision::Allow {
                        updated_input: None,
                    },
                })
                .await
                .unwrap();
            break;
        }
    }
    handle
        .send(SessionCommand::SendNow {
            text: "Stop. Reply with exactly: now-ok".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    if then_interrupt {
        handle.send(SessionCommand::Interrupt).await.unwrap();
        let mut events = until_turn_ends(&mut handle, 1).await;
        events.extend(none_within(&mut handle, Duration::from_millis(500), |_| false).await);
        return events;
    }
    until_turn_ends(&mut handle, 2).await
}

/// The anchor told after the first turn's end (an interrupted one) and
/// before the second's text, and the second turn's end.
fn anchored_after_the_stop(events: &[AgentEvent]) -> String {
    let stop = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
        .unwrap();
    // Claude's `error_during_execution`, told as the interrupt it was.
    assert_eq!(
        events[stop],
        AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted
        }
    );
    let rest = &events[stop + 1..];
    let anchor = rest
        .iter()
        .position(|e| matches!(e, AgentEvent::TurnAnchor { .. }))
        .expect("no anchor for the prompt sent at once");
    let text = rest
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("now-ok")))
        .unwrap();
    assert!(anchor < text);
    assert!(matches!(
        rest.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));
    match &rest[anchor] {
        AgentEvent::TurnAnchor { id } => id.clone(),
        _ => unreachable!(),
    }
}

/// On a CLI that advertises `interrupt_send_now_v1` (2.1.301): the message
/// with a uuid, then `interrupt` with `send_now` naming it. The anchor is
/// told once the CLI says it started on it, after the stopped turn's end.
#[tokio::test]
async fn claude_sends_now_with_the_message_named_in_the_interrupt() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/send_now.jsonl");
    let events = claude_sent_now(&fake, &fixture, false).await;
    let anchor = anchored_after_the_stop(&events);
    let sent = fake.sent_lines();
    let message = sent
        .iter()
        .position(|v| {
            v["type"] == "user" && v["message"]["content"] == "Stop. Reply with exactly: now-ok"
        })
        .unwrap();
    assert_eq!(sent[message]["uuid"], anchor.as_str());
    assert!(sent[message].get("priority").is_none());
    let interrupt = &sent[message + 1];
    assert_eq!(interrupt["request"]["subtype"], "interrupt");
    assert_eq!(interrupt["request"]["send_now"], true);
    assert_eq!(interrupt["request"]["message_uuid"], anchor.as_str());
}

/// On a CLI that does not advertise it: a plain interrupt, then the
/// message, which is the next turn. Without `command_lifecycle` either,
/// the anchor is told right after the stopped turn's end.
#[tokio::test]
async fn claude_sends_now_as_an_interrupt_and_the_message_where_it_must() {
    if !python_available() {
        return;
    }
    for (name, edits) in [
        (
            "plain.jsonl",
            &[("\"interrupt_send_now_v1\"", "\"x_send_now\"")][..],
        ),
        (
            "plain_no_lifecycle.jsonl",
            &[
                ("\"interrupt_send_now_v1\"", "\"x_send_now\""),
                ("\"msg_lifecycle_v1\"", "\"x_lifecycle\""),
            ][..],
        ),
    ] {
        let fake = Fake::new();
        let fixture = rewritten(
            &fake,
            "src/harness/claude/fixtures/send_now_plain.jsonl",
            name,
            edits,
        );
        let events = claude_sent_now(&fake, &fixture, false).await;
        let anchor = anchored_after_the_stop(&events);
        let sent = fake.sent_lines();
        let message = sent
            .iter()
            .position(|v| v["type"] == "user" && v["uuid"] == anchor.as_str())
            .unwrap_or_else(|| panic!("{name}: {sent:?}"));
        let interrupt = &sent[message - 1];
        assert_eq!(interrupt["request"]["subtype"], "interrupt", "{name}");
        assert!(interrupt["request"].get("send_now").is_none(), "{name}");
    }
}

/// Esc right after it: `interrupt` with `cancel_queued` takes the message
/// back, and nothing is anchored for it.
#[tokio::test]
async fn claude_takes_back_a_prompt_sent_now_on_an_interrupt() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = repo().join("src/harness/claude/fixtures/send_now_cancelled.jsonl");
    let events = claude_sent_now(&fake, &fixture, true).await;
    let stop = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
        .unwrap();
    assert!(
        !events[stop..]
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnAnchor { .. } | AgentEvent::TurnStarted)),
        "{:?}",
        events[stop..]
            .iter()
            .map(|e| e.summary())
            .collect::<Vec<_>>()
    );
    let sent = fake.sent_lines();
    let last = sent
        .iter()
        .rev()
        .find(|v| v["type"] == "control_request")
        .unwrap();
    assert_eq!(last["request"]["subtype"], "interrupt");
    assert_eq!(last["request"]["cancel_queued"], true);
}

/// Codex app-server tells the interrupted turn's end on its
/// `turn/completed`, not on the interrupt's answer: a `turn/start` sent
/// before would steer the running turn. The next prompt then goes
/// (`fixtures/app_server_interrupt_then_prompt.jsonl`, 0.162.1).
#[tokio::test]
async fn codex_app_server_ends_an_interrupted_turn_on_its_turn_completed() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let completed = "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"01a14203-ddbf-79d1-ab7e-11810aa59f59\",\"turn\":{\"id\":\"01a14203-dde7";
    let fixture = rewritten(
        &fake,
        "src/harness/codex/fixtures/app_server_interrupt_then_prompt.jsonl",
        "interrupt.jsonl",
        &[(completed, &format!("# gate\n{completed}"))],
    );
    let gate = fake._tmp.path().join("gate");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.env
        .push(("UNHARNESS_FAKE_GATE".into(), gate.display().to_string()));
    let harness = unharness::harness::codex::CodexHarness::new(
        unharness::harness::codex::CodexTransport::AppServer,
    );
    let mut handle = harness.start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("sleep")).await.unwrap();
    loop {
        let ev = next_event(&mut handle).await;
        if let AgentEvent::PermissionRequest(req) = &ev {
            handle
                .send(SessionCommand::RespondPermission {
                    id: req.id.clone(),
                    decision: PermissionDecision::Allow {
                        updated_input: None,
                    },
                })
                .await
                .unwrap();
            break;
        }
    }
    handle.send(SessionCommand::Interrupt).await.unwrap();
    none_within(&mut handle, Duration::from_millis(700), |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    assert!(
        fake.sent_lines()
            .iter()
            .any(|v| v["method"] == "turn/interrupt")
    );
    std::fs::write(&gate, "").unwrap();
    let events = until_turn_ends(&mut handle, 1).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted
        })
    ));
    handle.send(SessionCommand::turn("now")).await.unwrap();
    let events = until_turn_ends(&mut handle, 1).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("now")))
    );
    let sent = fake.sent_lines();
    let interrupt = sent
        .iter()
        .position(|v| v["method"] == "turn/interrupt")
        .unwrap();
    let starts: Vec<usize> = sent
        .iter()
        .enumerate()
        .filter(|(_, v)| v["method"] == "turn/start")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(starts.len(), 2);
    assert!(starts[1] > interrupt);
}

/// pi ends an aborted turn (`agent_settled`) before it answers `abort`,
/// which it does once it is idle: the next prompt waits for that answer.
#[tokio::test]
async fn pi_sends_a_prompt_after_an_abort_only_once_it_is_answered() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let answer = "{\"id\":\"req-4\",\"type\":\"response\",\"command\":\"abort\",\"success\":true}";
    let next = ">> {\"id\": \"req-7\", \"type\": \"prompt\", \"message\": \"now\"}\n\
        {\"id\":\"req-7\",\"type\":\"response\",\"command\":\"prompt\",\"success\":true,\"data\":{\"disposition\":\"started\"}}\n\
        {\"type\":\"agent_settled\"}\n";
    let fixture = rewritten(
        &fake,
        "src/harness/pi/fixtures/gate_abort.jsonl",
        "abort.jsonl",
        &[(answer, &format!("# gate\n{answer}")), ("# exit=0\n", next)],
    );
    let gate = fake._tmp.path().join("gate");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.env
        .push(("UNHARNESS_FAKE_GATE".into(), gate.display().to_string()));
    let mut handle = pi_harness(&fake).start_session(cfg).unwrap();
    handle.send(SessionCommand::turn("echo hi")).await.unwrap();
    loop {
        if let AgentEvent::PermissionRequest(_) = next_event(&mut handle).await {
            handle.send(SessionCommand::Interrupt).await.unwrap();
            break;
        }
    }
    loop {
        if let AgentEvent::TurnCompleted { stop_reason } = next_event(&mut handle).await {
            assert_eq!(stop_reason, StopReason::Interrupted);
            break;
        }
    }
    handle.send(SessionCommand::turn("now")).await.unwrap();
    none_within(&mut handle, Duration::from_millis(500), |_| false).await;
    let prompts = |sent: &[Value]| {
        sent.iter()
            .filter(|v| v["type"] == "prompt")
            .map(|v| v["message"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        prompts(&fake.sent_lines()).len(),
        1,
        "sent before abort's answer"
    );
    std::fs::write(&gate, "").unwrap();
    let events = until_turn_ends(&mut handle, 1).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));
    assert_eq!(prompts(&fake.sent_lines())[1], "now");
}

/// An ACP agent's cancelled turn ends with its prompt's answer
/// (`stopReason: cancelled`), not with the `session/cancel`: vibe-acp
/// refuses a `session/prompt` while a turn runs. The next one goes after.
#[tokio::test]
async fn acp_ends_a_cancelled_turn_on_its_prompt_answer() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let answer = "{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"stopReason\":\"cancelled\"";
    let next = ">> {\"jsonrpc\": \"2.0\", \"id\": 5, \"method\": \"session/prompt\", \"params\": {}}\n\
        {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"stopReason\":\"end_turn\",\"usage\":{\"totalTokens\":8402,\"inputTokens\":8389,\"outputTokens\":13}}}\n";
    let fixture = rewritten(
        &fake,
        "src/harness/acp/fixtures/vibe_acp_interrupt.jsonl",
        "cancel.jsonl",
        &[
            (answer, &format!("# gate\n{answer}")),
            ("# exit=0 status=done\n", next),
        ],
    );
    let gate = fake._tmp.path().join("gate");
    let mut cfg = fake.config(&fixture, PermissionPolicy::Ask, true);
    cfg.model = None;
    cfg.env
        .push(("UNHARNESS_FAKE_GATE".into(), gate.display().to_string()));
    let mut handle = vibe_acp().start_session(cfg).unwrap();
    handle
        .send(SessionCommand::turn("create a.txt"))
        .await
        .unwrap();
    loop {
        if let AgentEvent::PermissionRequest(_) = next_event(&mut handle).await {
            handle.send(SessionCommand::Interrupt).await.unwrap();
            break;
        }
    }
    none_within(&mut handle, Duration::from_millis(700), |e| {
        matches!(e, AgentEvent::TurnCompleted { .. })
    })
    .await;
    assert!(
        fake.sent_lines()
            .iter()
            .any(|v| v["method"] == "session/cancel")
    );
    std::fs::write(&gate, "").unwrap();
    let events = until_turn_ends(&mut handle, 1).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted
        })
    ));
    handle.send(SessionCommand::turn("now")).await.unwrap();
    let events = until_turn_ends(&mut handle, 1).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done
        })
    ));
    let sent = fake.sent_lines();
    let cancel = sent
        .iter()
        .position(|v| v["method"] == "session/cancel")
        .unwrap();
    let prompts: Vec<usize> = sent
        .iter()
        .enumerate()
        .filter(|(_, v)| v["method"] == "session/prompt")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1] > cancel);
}

/// pi that never answers `abort` (a dialog it waits in, #89) does not hold
/// the next prompt for good: it goes after `ABORT_WAIT`, said to.
#[tokio::test]
async fn pi_sends_a_prompt_held_for_an_abort_pi_never_answers() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let answer =
        "{\"id\":\"req-4\",\"type\":\"response\",\"command\":\"abort\",\"success\":true}\n";
    let next = ">> {\"id\": \"req-7\", \"type\": \"prompt\", \"message\": \"now\"}\n\
        {\"id\":\"req-7\",\"type\":\"response\",\"command\":\"prompt\",\"success\":true,\"data\":{\"disposition\":\"started\"}}\n\
        {\"type\":\"agent_settled\"}\n";
    let fixture = rewritten(
        &fake,
        "src/harness/pi/fixtures/gate_abort.jsonl",
        "unanswered.jsonl",
        &[(answer, ""), ("# exit=0\n", next)],
    );
    let mut handle = pi_harness(&fake)
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
    handle.send(SessionCommand::turn("echo hi")).await.unwrap();
    loop {
        if let AgentEvent::PermissionRequest(_) = next_event(&mut handle).await {
            handle.send(SessionCommand::Interrupt).await.unwrap();
            break;
        }
    }
    while !matches!(
        next_event(&mut handle).await,
        AgentEvent::TurnCompleted { .. }
    ) {}
    handle.send(SessionCommand::turn("now")).await.unwrap();
    let events = until_turn_ends(&mut handle, 1).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Notice(n) if n.starts_with("pi did not answer the interrupt")
    )));
    let prompts: Vec<Value> = fake
        .sent_lines()
        .into_iter()
        .filter(|v| v["type"] == "prompt")
        .collect();
    assert_eq!(prompts.len(), 2);
}

/// A plain Esc on Claude: the turn it stops ends `error_during_execution`,
/// which the driver tells as the interrupt it was.
#[tokio::test]
async fn claude_tells_the_turn_an_interrupt_stopped_as_interrupted() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    // The recording up to the stopped turn's end, without the message
    // that was sent with its interrupt.
    let text =
        std::fs::read_to_string(repo().join("src/harness/claude/fixtures/send_now_plain.jsonl"))
            .unwrap();
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with(">> {\"type\": \"user\"") && line.contains("now-ok") {
            continue;
        }
        lines.push(line);
        if line.contains("\"subtype\":\"error_during_execution\"") {
            break;
        }
    }
    let fixture = fake._tmp.path().join("esc.jsonl");
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    let harness = unharness::harness::claude::ClaudeHarness::default();
    let mut handle = harness
        .start_session(fake.config(&fixture, PermissionPolicy::Ask, true))
        .unwrap();
    handle.send(SessionCommand::turn("sleep")).await.unwrap();
    loop {
        let ev = next_event(&mut handle).await;
        if let AgentEvent::PermissionRequest(req) = &ev {
            handle
                .send(SessionCommand::RespondPermission {
                    id: req.id.clone(),
                    decision: PermissionDecision::Allow {
                        updated_input: None,
                    },
                })
                .await
                .unwrap();
            break;
        }
    }
    handle.send(SessionCommand::Interrupt).await.unwrap();
    let events = until_turn_ends(&mut handle, 1).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            stop_reason: StopReason::Interrupted
        })
    ));
}

/// On a CLI whose interrupt does not take `cancel_queued`, Esc cannot take
/// the prompt back: the driver says so at `init`, sends a plain interrupt,
/// and still tells what becomes of the prompt.
#[tokio::test]
async fn claude_keeps_a_prompt_sent_now_it_cannot_take_back() {
    if !python_available() {
        return;
    }
    let fake = Fake::new();
    let fixture = rewritten(
        &fake,
        "src/harness/claude/fixtures/send_now_cancelled.jsonl",
        "no_cancel.jsonl",
        &[("\"interrupt_cancel_queued_v1\"", "\"x_cancel_queued\"")],
    );
    let events = claude_sent_now(&fake, &fixture, true).await;
    let sent = fake.sent_lines();
    let last = sent
        .iter()
        .rev()
        .find(|v| v["type"] == "control_request")
        .unwrap();
    assert_eq!(last["request"]["subtype"], "interrupt");
    assert!(last["request"].get("cancel_queued").is_none());
    // Told what becomes of it: here the CLI cancelled it, and its turn
    // fails once the stopped one has ended.
    let stop = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
        .unwrap();
    assert!(matches!(
        &events[stop + 1..],
        [
            AgentEvent::TurnAnchor { id },
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::Error(_)
            },
            ..
        ] if id.is_empty()
    ));
}
