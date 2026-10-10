//! Antigravity transport: one `agy --print= --input-format stream-json
//! --output-format stream-json` process for the session, one NDJSON line on
//! stdin per turn (agy 1.2.17 names the `event` field when it is missing and
//! ignores every event but `user`).

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::brain;
use super::parse::AgyParser;
use crate::core::process::{LineProcess, RawLine};
use crate::core::session::ProcessSlot;
use crate::core::{
    AgentEvent, HarnessId, PermissionPolicy, ProcessModel, RemoteControl, SessionCommand,
    SessionConfig, SessionHandle, SessionInfo, StopReason, shutdown_queued,
};

pub fn policy_args(policy: PermissionPolicy) -> Vec<&'static str> {
    match policy {
        // Not declared (`--mode plan` does not wait); never sent one.
        PermissionPolicy::Plan | PermissionPolicy::Ask => vec![],
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => vec!["--mode", "accept-edits"],
        PermissionPolicy::Bypass => vec!["--dangerously-skip-permissions"],
    }
}

/// A model id ends in its effort (`gemini-3.8-flash-high`) and agy refuses
/// `--effort` beside one ("conflicts with --effort"), so no effort is sent:
/// the harness declares no effort levels.
pub fn model_args(model: Option<&str>) -> Vec<String> {
    match model {
        Some(m) => vec!["--model".into(), m.to_string()],
        None => vec![],
    }
}

pub fn stream_args(cfg: &SessionConfig) -> Vec<String> {
    let mut args: Vec<String> = [
        "--print=",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--print-timeout",
        "0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(policy_args(cfg.policy).iter().map(|s| s.to_string()));
    args.extend(model_args(cfg.model.as_ref().map(|m| m.model.as_str())));
    args.extend(cfg.extra_args.iter().cloned());
    if let Some(id) = &cfg.resume {
        args.push("--conversation".into());
        args.push(id.clone());
    }
    args
}

pub fn encode_turn(text: &str) -> String {
    json!({"event": "user", "message": {"content": text}}).to_string()
}

/// What agy's files say about the subagents before `line` is parsed: on
/// a `system_message` step (how a report is delivered) the reports filed
/// since, and before a `result` also how each that never reported ended.
/// Read once a subagent has started in this process, also after it ended:
/// one put back to work reports again.
fn subagent_ends(
    parser: &mut AgyParser,
    brain_dir: &Path,
    line: &Value,
    inbox: &mut brain::Inbox,
) -> Vec<AgentEvent> {
    let result = line.get("event").and_then(Value::as_str) == Some("result");
    let delivered = line
        .pointer("/step_update/step_type")
        .and_then(Value::as_str)
        == Some("system_message");
    if parser.subagents().is_empty() || !(result || delivered) {
        return Vec::new();
    }
    let Some(parent) = parser.session_id().map(str::to_string) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for message in brain::new_messages(brain_dir, &parent, parser.subagents(), inbox) {
        out.extend(parser.feed_report(&message));
    }
    if result {
        for id in parser.open_subagents().to_vec() {
            if let Some(step) = brain::last_step(brain_dir, &id) {
                out.extend(parser.feed_last_step(&id, &step));
            }
        }
    }
    out
}

pub fn start_stream(cfg: SessionConfig) -> Result<SessionHandle> {
    let proc = spawn_stream(&cfg)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness: HarnessId::AGY,
        process_model: ProcessModel::LongLived,
    });
    let slot = handle.process_slot();
    slot.set(&proc);
    tokio::spawn(drive(proc, slot, cfg, events_tx, cmd_rx));
    Ok(handle)
}

fn spawn_stream(cfg: &SessionConfig) -> Result<LineProcess> {
    let mut cmd = Command::new(&cfg.binary);
    cmd.args(stream_args(cfg)).current_dir(&cfg.cwd);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    LineProcess::spawn(cmd, &cfg.sandbox)
}

async fn drive(
    mut proc: LineProcess,
    slot: ProcessSlot,
    mut cfg: SessionConfig,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
) {
    let resume = cfg.resume.clone();
    let mut parser = AgyParser::new(resume.clone());
    let mut turn_open = false;
    let mut shutting_down = false;
    // Model/effort/policy are process flags; a change restarts the process
    // on the next turn, resuming the conversation by id.
    let mut restart_needed = false;
    let mut session_id = resume.clone();
    let default_brain = brain::default_dir();
    // Messages looked at, also in an earlier process of the conversation;
    // one from a subagent of that process is passed over all the same.
    let mut inbox = brain::Inbox::default();
    if let Some(id) = resume {
        let _ = events
            .send(AgentEvent::SessionStarted {
                session_id: id,
                model: None,
            })
            .await;
    }

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    // agy takes no event on stdin that would stop one.
                    SessionCommand::StopSubagent { .. } => {}
                    SessionCommand::RemoteControl { .. } => {
                        let _ = events.send(AgentEvent::RemoteControl(RemoteControl::Failed {
                            reason: "agy has no remote control".into(),
                        })).await;
                    }
                    SessionCommand::Steer { .. }
                    | SessionCommand::Compact { .. }
                    | SessionCommand::Rewind { .. } => {
                        let _ = events.send(AgentEvent::Error(
                            "agy cannot steer, compact or rewind a session".into(),
                        )).await;
                    }
                    SessionCommand::SendTurn { text, .. } => {
                        if restart_needed {
                            restart_needed = false;
                            proc.kill().await;
                            cfg.resume = session_id.clone();
                            match spawn_stream(&cfg) {
                                Ok(p) => {
                                    proc = p;
                                    slot.set(&proc);
                                    parser = AgyParser::new(session_id.clone());
                                    let _ = events.send(AgentEvent::Notice(
                                        "agy restarted with the new settings".into(),
                                    )).await;
                                }
                                Err(e) => {
                                    let _ = events.send(AgentEvent::TurnCompleted {
                                        stop_reason: StopReason::Error(format!("agy restart failed: {e}")),
                                    }).await;
                                    continue;
                                }
                            }
                        }
                        turn_open = true;
                        if let Err(e) = proc.write_line(&encode_turn(&text)).await {
                            turn_open = false;
                            let _ = events.send(AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Error(format!("agy: {e}")),
                            }).await;
                        }
                    }
                    SessionCommand::Interrupt => {
                        // There is no interrupt event; ending the process ends
                        // the turn, and the conversation resumes by id. agy
                        // leaves a command it started running after SIGINT;
                        // the kill takes it (checked on 1.2.17).
                        let _ = events.send(AgentEvent::Notice(
                            "agy has no interrupt: its process is stopped with what it started; the next turn resumes the conversation".into(),
                        )).await;
                        proc.kill().await;
                    }
                    SessionCommand::RespondPermission { .. } => {
                        let _ = events.send(AgentEvent::Error(
                            "agy cannot answer permission prompts in headless mode".into(),
                        )).await;
                    }
                    SessionCommand::SetModel(m) => {
                        cfg.model = Some(m);
                        restart_needed = true;
                    }
                    SessionCommand::SetEffort(e) => {
                        cfg.effort = e;
                        restart_needed = true;
                    }
                    SessionCommand::SetPolicy(p) => {
                        cfg.policy = p;
                        restart_needed = true;
                    }
                    SessionCommand::Shutdown => {
                        shutting_down = true;
                        proc.end_or_kill(turn_open).await;
                    }
                }
            }
            raw = proc.lines.recv() => {
                match raw {
                    Some(RawLine::Stdout(line)) => {
                        let mut evs = Vec::new();
                        let brain_dir = parser.brain_dir().map(Path::to_path_buf).or_else(|| default_brain.clone());
                        if let (Some(dir), Ok(v)) = (brain_dir, serde_json::from_str::<Value>(&line)) {
                            evs = subagent_ends(&mut parser, &dir, &v, &mut inbox);
                        }
                        evs.extend(parser.feed(&line));
                        for ev in evs {
                            match &ev {
                                AgentEvent::TurnCompleted { .. } => turn_open = false,
                                AgentEvent::SessionStarted { session_id: id, .. } => {
                                    session_id = Some(id.clone());
                                }
                                _ => {}
                            }
                            if events.send(ev).await.is_err() {
                                if !shutting_down {
                                    if shutdown_queued(&mut cmds) {
                                        proc.end_or_kill(turn_open).await;
                                    } else {
                                        proc.kill().await;
                                    }
                                }
                                return;
                            }
                        }
                    }
                    Some(RawLine::Stderr(line)) => {
                        for ev in parser.feed_stderr(&line) {
                            let _ = events.send(ev).await;
                        }
                    }
                    Some(RawLine::Exited(code)) => {
                        if turn_open {
                            let stop_reason = match code {
                                Some(0) | None => StopReason::Interrupted,
                                Some(c) => StopReason::Error(format!("agy exited with code {c}")),
                            };
                            let _ = events.send(AgentEvent::TurnCompleted { stop_reason }).await;
                        }
                        let _ = events.send(AgentEvent::ProcessExited { code }).await;
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    if !shutting_down {
        proc.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ModelRef;
    use std::path::PathBuf;

    fn cfg() -> SessionConfig {
        SessionConfig {
            binary: PathBuf::from("agy"),
            cwd: PathBuf::from("/tmp"),
            model: Some(ModelRef::new(
                HarnessId::AGY,
                "google",
                "gemini-3.8-flash-high",
            )),
            provider: None,
            effort: Some("high".into()),
            policy: PermissionPolicy::AcceptEdits,
            resume: Some("conv-1".into()),
            fork: false,
            extra_args: vec!["--add-dir".into(), "/x".into()],
            env: vec![],
            mcp_servers: Vec::new(),
            sandbox: crate::core::Sandbox::off(),
        }
    }

    #[test]
    fn stream_args_shape() {
        let mut cfg = cfg();
        assert_eq!(
            stream_args(&cfg).join(" "),
            "--print= --input-format stream-json --output-format stream-json --print-timeout 0 --mode accept-edits --model gemini-3.8-flash-high --add-dir /x --conversation conv-1"
        );
        cfg.model = None;
        cfg.resume = None;
        cfg.policy = PermissionPolicy::Bypass;
        assert_eq!(
            stream_args(&cfg).join(" "),
            "--print= --input-format stream-json --output-format stream-json --print-timeout 0 --dangerously-skip-permissions --add-dir /x"
        );
    }

    /// The `Subagent*` events of `case` as the transport sees it, with
    /// `brain` as agy's directory; `before_read(n)` runs before the `n`th
    /// `system_message` step or `result`, counted together, is read.
    fn subagent_events(
        case: &str,
        brain: &Path,
        mut before_read: impl FnMut(usize),
    ) -> Vec<String> {
        let dir = crate::core::testing::fixtures_dir(file!());
        let fixture = std::fs::read_to_string(dir.join(format!("{case}.jsonl"))).unwrap();
        let mut parser = AgyParser::new(None);
        let mut inbox = brain::Inbox::default();
        let mut deliveries = 0;
        let mut out = Vec::new();
        for line in fixture.lines().filter(|l| l.starts_with('{')) {
            let v: Value = serde_json::from_str(line).unwrap();
            if v.pointer("/step_update/step_type").and_then(Value::as_str) == Some("system_message")
                || v["event"] == "result"
            {
                before_read(deliveries);
                deliveries += 1;
            }
            let mut evs = subagent_ends(&mut parser, brain, &v, &mut inbox);
            evs.extend(parser.feed(line));
            out.extend(evs.iter().filter_map(|e| match e {
                AgentEvent::SubagentStarted { .. }
                | AgentEvent::SubagentEnded { .. }
                | AgentEvent::TurnCompleted { .. } => Some(e.summary()),
                _ => None,
            }));
        }
        out
    }

    /// The two messages agy filed in `subagent_followup`, cut down.
    const FOLLOWUP_REPORTS: [(&str, &str); 2] = [
        (
            "5ec40763-0e64-4726-809c-b8787f74daef",
            r#"{"id":"5ec40763-0e64-4726-809c-b8787f74daef","recipient":"d8a5df5b-cb3d-4358-b8ad-8d63750349a4","sender":"3e3112f0-bf16-4840-a3a8-0802c43769f4","priority":"MESSAGE_PRIORITY_HIGH","timestamp":"2026-10-07T20:21:34.546864664Z","renderDetails":{"messageTitle":"Message from Spec Reviewer (self)"},"content":"`calc.py` does not match `SPEC.md` because `add(a, b)` subtracts `b` from `a` instead of returning their sum."}"#,
        ),
        (
            "b06c478e-a62d-48b5-a2a2-257e88985ffe",
            r#"{"id":"b06c478e-a62d-48b5-a2a2-257e88985ffe","recipient":"d8a5df5b-cb3d-4358-b8ad-8d63750349a4","sender":"70c61b66-3d4e-4459-849c-4f61647eef76","priority":"MESSAGE_PRIORITY_HIGH","timestamp":"2026-10-07T20:21:49.425103896Z","renderDetails":{"messageTitle":"Message from Bug Reviewer (self)"},"content":"The `add` function in `calc.py` incorrectly subtracts `b` from `a` instead of adding them."}"#,
        ),
    ];

    fn write_report(brain: &Path, id: &str, body: &str) {
        let messages =
            brain.join("d8a5df5b-cb3d-4358-b8ad-8d63750349a4/.system_generated/messages");
        std::fs::create_dir_all(&messages).unwrap();
        std::fs::write(messages.join(format!("{id}.json")), body).unwrap();
    }

    #[test]
    fn a_late_report_and_a_second_one_are_read_at_a_result() {
        let brain = tempfile::tempdir().unwrap();
        let [first, second] = FOLLOWUP_REPORTS;
        let again = r#"{"recipient":"d8a5df5b-cb3d-4358-b8ad-8d63750349a4","sender":"70c61b66-3d4e-4459-849c-4f61647eef76","timestamp":"2026-10-07T20:23:02.1Z","content":"Looked again: still wrong."}"#;
        // The second report is filed after its delivery was read, and the
        // subagent reports once more during the next turn.
        let events = subagent_events("subagent_followup", brain.path(), |n| match n {
            0 => write_report(brain.path(), first.0, first.1),
            2 => write_report(brain.path(), second.0, second.1),
            3 => write_report(brain.path(), "0d0e0f", again),
            _ => {}
        });
        assert_eq!(
            events[2..],
            [
                r#"SubagentEnded id=3e3112f0-bf16-4840-a3a8-0802c43769f4 completed "`calc.py` does not match `SPEC.md` because `add(a, b)` su...""#,
                r#"SubagentEnded id=70c61b66-3d4e-4459-849c-4f61647eef76 completed "The `add` function in `calc.py` incorrectly subtracts `b`...""#,
                "TurnCompleted Done",
                r#"SubagentEnded id=70c61b66-3d4e-4459-849c-4f61647eef76 completed "Looked again: still wrong.""#,
                "TurnCompleted Done",
            ]
        );
    }

    #[test]
    fn a_report_ends_its_subagent_when_it_is_delivered() {
        // The two messages agy filed in the recording, cut down.
        let brain = tempfile::tempdir().unwrap();
        let events = subagent_events("subagent_followup", brain.path(), |n| {
            if let Some((id, body)) = FOLLOWUP_REPORTS.get(n) {
                write_report(brain.path(), id, body);
            }
        });
        assert_eq!(
            events,
            [
                r#"SubagentStarted id=70c61b66-3d4e-4459-849c-4f61647eef76 kind=self "Bug Reviewer""#,
                r#"SubagentStarted id=3e3112f0-bf16-4840-a3a8-0802c43769f4 kind=self "Spec Reviewer""#,
                r#"SubagentEnded id=3e3112f0-bf16-4840-a3a8-0802c43769f4 completed "`calc.py` does not match `SPEC.md` because `add(a, b)` su...""#,
                r#"SubagentEnded id=70c61b66-3d4e-4459-849c-4f61647eef76 completed "The `add` function in `calc.py` incorrectly subtracts `b`...""#,
                "TurnCompleted Done",
                "TurnCompleted Done",
            ]
        );
    }

    #[test]
    fn a_subagent_that_never_reported_ends_as_its_transcript_does() {
        let brain = tempfile::tempdir().unwrap();
        let transcript = |id: &str, text: &str| {
            let logs = brain.path().join(id).join(".system_generated/logs");
            std::fs::create_dir_all(&logs).unwrap();
            std::fs::write(logs.join("transcript.jsonl"), text).unwrap();
        };
        // How a refused subagent's transcript ended, cut down.
        transcript(
            "07b35f72-9201-45ef-a97c-7f3094885937",
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"ls -la /WORKSPACE\""}}]}
{"step_index":4,"source":"MODEL","type":"GENERIC","status":"ERROR","error":"permission check failed for unsandboxed \"ls -la /WORKSPACE\": user denied permission to run command"}
"#,
        );
        // One that answered without sending its answer.
        transcript(
            "b5abd3a7-fa61-4801-827d-f8215a6efb98",
            r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"calc.py matches SPEC.md."}
"#,
        );
        let events = subagent_events("subagent_refused", brain.path(), |_| {});
        assert!(
            events[2].starts_with(
                "SubagentEnded id=07b35f72-9201-45ef-a97c-7f3094885937 failed \"permission check failed"
            ),
            "{events:?}"
        );
        assert_eq!(
            events[3],
            r#"SubagentEnded id=b5abd3a7-fa61-4801-827d-f8215a6efb98 completed "calc.py matches SPEC.md.""#
        );
        // Without the files, the result ends them all the same.
        let none = tempfile::tempdir().unwrap();
        let events = subagent_events("subagent_refused", none.path(), |_| {});
        assert!(events[2].ends_with("completed -"), "{events:?}");
    }

    #[test]
    fn turn_encoding() {
        assert_eq!(
            encode_turn("hi"),
            r#"{"event":"user","message":{"content":"hi"}}"#
        );
    }
}
