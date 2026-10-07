//! Antigravity transport: one `agy --print= --input-format stream-json
//! --output-format stream-json` process for the session, one NDJSON line on
//! stdin per turn (agy 1.2.17 names the `event` field when it is missing and
//! ignores every event but `user`).

use anyhow::Result;
use serde_json::json;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::parse::AgyParser;
use crate::core::process::{LineProcess, RawLine};
use crate::core::{
    AgentEvent, HarnessId, PermissionPolicy, ProcessModel, SessionCommand, SessionConfig,
    SessionHandle, SessionInfo, StopReason,
};

pub fn policy_args(policy: PermissionPolicy) -> Vec<&'static str> {
    match policy {
        PermissionPolicy::Ask => vec![],
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

pub fn start_stream(cfg: SessionConfig) -> Result<SessionHandle> {
    let proc = spawn_stream(&cfg)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness: HarnessId::AGY,
        process_model: ProcessModel::LongLived,
    });
    tokio::spawn(drive(proc, cfg, events_tx, cmd_rx));
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
                    // No subagents are reported here, so none can be running.
                    SessionCommand::StopSubagent { .. } => {}
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
                        // the turn, and the conversation resumes by id. A
                        // command agy started runs on (also after SIGINT,
                        // checked on 1.2.17).
                        let _ = events.send(AgentEvent::Notice(
                            "agy has no interrupt: its process is stopped (a command it started runs on); the next turn resumes the conversation".into(),
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
                        proc.end();
                    }
                }
            }
            raw = proc.lines.recv() => {
                match raw {
                    Some(RawLine::Stdout(line)) => {
                        for ev in parser.feed(&line) {
                            match &ev {
                                AgentEvent::TurnCompleted { .. } => turn_open = false,
                                AgentEvent::SessionStarted { session_id: id, .. } => {
                                    session_id = Some(id.clone());
                                }
                                _ => {}
                            }
                            if events.send(ev).await.is_err() {
                                if !shutting_down {
                                    proc.kill().await;
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

    #[test]
    fn turn_encoding() {
        assert_eq!(
            encode_turn("hi"),
            r#"{"event":"user","message":{"content":"hi"}}"#
        );
    }
}
