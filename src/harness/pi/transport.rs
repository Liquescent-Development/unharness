//! Long-lived pi session over `--mode rpc`.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::parse::PiParser;
use crate::core::process::{LineProcess, RawLine};
use crate::core::{
    AgentEvent, HarnessId, PermissionDecision, PermissionKind, PermissionPolicy, ProcessModel,
    SessionCommand, SessionConfig, SessionHandle, SessionInfo,
};

/// argv after the binary for an interactive session; returns the session id used.
pub fn session_args(cfg: &SessionConfig) -> (Vec<String>, String) {
    let session_id = cfg
        .resume
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut args = vec![
        "--mode".to_string(),
        "rpc".to_string(),
        "--session-id".to_string(),
        session_id.clone(),
    ];
    if let Some(m) = &cfg.model {
        if !m.provider.as_str().is_empty() && m.provider.as_str() != "default" {
            args.push("--provider".into());
            args.push(m.provider.as_str().to_string());
        }
        args.push("--model".into());
        args.push(m.model.clone());
    }
    if let Some(e) = &cfg.effort {
        args.push("--thinking".into());
        args.push(e.clone());
    }
    args.extend(cfg.extra_args.iter().cloned());
    (args, session_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Confirm,
    Select,
    Input,
}

pub fn encode_ui_response(id: &str, kind: PendingKind, decision: &PermissionDecision) -> String {
    let mut v = json!({"type":"extension_ui_response","id":id});
    match (kind, decision) {
        (PendingKind::Confirm, PermissionDecision::Allow { .. })
        | (PendingKind::Confirm, PermissionDecision::AllowAlways) => v["confirmed"] = json!(true),
        (PendingKind::Confirm, PermissionDecision::Answer(Value::Bool(b))) => {
            v["confirmed"] = json!(b)
        }
        (
            PendingKind::Select | PendingKind::Input,
            PermissionDecision::Answer(Value::String(s)),
        ) => v["value"] = json!(s),
        _ => v["cancelled"] = json!(true),
    }
    v.to_string()
}

pub fn start(cfg: SessionConfig) -> Result<SessionHandle> {
    let (args, session_id) = session_args(&cfg);
    let mut cmd = Command::new(&cfg.binary);
    cmd.args(args).current_dir(&cfg.cwd);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let proc = LineProcess::spawn(cmd)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness: HarnessId::Pi,
        process_model: ProcessModel::LongLived,
    });
    tokio::spawn(drive(proc, session_id, cfg.policy, events_tx, cmd_rx));
    Ok(handle)
}

async fn drive(
    mut proc: LineProcess,
    session_id: String,
    mut policy: PermissionPolicy,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
) {
    let mut parser = PiParser::new(Some(session_id));
    let mut pending: HashMap<String, PendingKind> = HashMap::new();
    let mut seq = 0u64;
    let mut next_id = move || {
        seq += 1;
        format!("u{seq}")
    };
    let mut shutting_down = false;

    let _ = proc
        .write_line(&json!({"id": next_id(), "type":"get_state"}).to_string())
        .await;

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    proc.kill().await;
                    break;
                };
                let line = match cmd {
                    SessionCommand::SendTurn { text } => {
                        Some(json!({"id": next_id(), "type":"prompt","message": text}).to_string())
                    }
                    SessionCommand::Interrupt => Some(json!({"id": next_id(), "type":"abort"}).to_string()),
                    SessionCommand::RespondPermission { id, decision } => match pending.remove(&id) {
                        Some(kind) => Some(encode_ui_response(&id, kind, &decision)),
                        None => {
                            let _ = events.send(AgentEvent::Notice(format!("no pending pi dialog {id}"))).await;
                            None
                        }
                    },
                    SessionCommand::SetModel(m) => {
                        let _ = proc.write_line(&json!({
                            "id": next_id(), "type":"set_model",
                            "provider": m.provider.as_str(), "modelId": m.model
                        }).to_string()).await;
                        Some(json!({"id": next_id(), "type":"get_available_thinking_levels"}).to_string())
                    }
                    SessionCommand::SetEffort(e) => Some(json!({
                        "id": next_id(), "type":"set_thinking_level",
                        "level": e.unwrap_or_else(|| "medium".into())
                    }).to_string()),
                    SessionCommand::SetPolicy(p) => {
                        policy = p;
                        None
                    }
                    SessionCommand::Shutdown => {
                        shutting_down = true;
                        proc.close_stdin();
                        None
                    }
                };
                if let Some(line) = line
                    && let Err(e) = proc.write_line(&line).await
                {
                    let _ = events.send(AgentEvent::Error(format!("pi: {e}"))).await;
                }
            }
            raw = proc.lines.recv() => {
                match raw {
                    Some(RawLine::Stdout(line)) => {
                        for ev in parser.feed(&line) {
                            if let AgentEvent::PermissionRequest(req) = &ev {
                                let kind = match &req.kind {
                                    PermissionKind::Confirm { .. } => PendingKind::Confirm,
                                    PermissionKind::Select { .. } => PendingKind::Select,
                                    _ => PendingKind::Input,
                                };
                                // Under Bypass, confirm dialogs are auto-accepted and
                                // selects take the first option; text input still asks.
                                if policy == PermissionPolicy::Bypass && kind != PendingKind::Input {
                                    let answer = match &req.kind {
                                        PermissionKind::Select { options, .. } => PermissionDecision::Answer(
                                            options.first().cloned().map(Value::String).unwrap_or(Value::Null),
                                        ),
                                        _ => PermissionDecision::Answer(Value::Bool(true)),
                                    };
                                    let _ = proc.write_line(&encode_ui_response(&req.id, kind, &answer)).await;
                                    let _ = events.send(AgentEvent::Notice("auto-accepted an extension dialog (bypass)".into())).await;
                                    continue;
                                }
                                pending.insert(req.id.clone(), kind);
                            }
                            if events.send(ev).await.is_err() {
                                proc.kill().await;
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

    #[test]
    fn session_args_shape() {
        let cfg = SessionConfig {
            binary: PathBuf::from("pi"),
            cwd: PathBuf::from("/tmp"),
            model: Some(ModelRef::new(
                HarnessId::Pi,
                "anthropic",
                "claude-sonnet-4-5",
            )),
            effort: Some("high".into()),
            policy: PermissionPolicy::Ask,
            resume: None,
            extra_args: vec!["--no-extensions".into()],
            env: vec![],
        };
        let (args, sid) = session_args(&cfg);
        let s = args.join(" ");
        assert!(s.starts_with(&format!("--mode rpc --session-id {sid}")));
        assert!(s.contains("--provider anthropic --model claude-sonnet-4-5"));
        assert!(s.contains("--thinking high"));
        assert!(s.ends_with("--no-extensions"));

        let mut r = cfg.clone();
        r.resume = Some("abc".into());
        let (args, sid) = session_args(&r);
        assert_eq!(sid, "abc");
        assert!(args.join(" ").contains("--session-id abc"));
    }

    #[test]
    fn ui_response_encoding() {
        let v: Value = serde_json::from_str(&encode_ui_response(
            "u1",
            PendingKind::Confirm,
            &PermissionDecision::Answer(Value::Bool(false)),
        ))
        .unwrap();
        assert_eq!(
            v,
            json!({"type":"extension_ui_response","id":"u1","confirmed":false})
        );
        let v: Value = serde_json::from_str(&encode_ui_response(
            "u2",
            PendingKind::Select,
            &PermissionDecision::Answer(json!("b")),
        ))
        .unwrap();
        assert_eq!(v["value"], "b");
        let v: Value = serde_json::from_str(&encode_ui_response(
            "u3",
            PendingKind::Input,
            &PermissionDecision::Answer(Value::Null),
        ))
        .unwrap();
        assert_eq!(v["cancelled"], true);
        let v: Value = serde_json::from_str(&encode_ui_response(
            "u4",
            PendingKind::Confirm,
            &PermissionDecision::Deny { reason: "x".into() },
        ))
        .unwrap();
        assert_eq!(v["cancelled"], true);
    }
}
