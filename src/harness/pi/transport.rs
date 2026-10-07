//! Long-lived pi session over `--mode rpc`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::gate;
use super::parse::PiParser;
use crate::core::process::{LineProcess, RawLine};
use crate::core::{
    AgentEvent, Attachment, HarnessId, PermissionDecision, PermissionKind, PermissionPolicy,
    ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo, StopReason,
    shutdown_queued,
};

/// argv after the binary for an interactive session; returns the session id used.
///
/// `gate` is the installed gate extension. It is loaded under every policy,
/// since the policy can change while pi runs; who answers it is decided here.
pub fn session_args(cfg: &SessionConfig, gate: &Path) -> (Vec<String>, String) {
    let session_id = cfg
        .resume
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut args = vec![
        "--mode".to_string(),
        "rpc".to_string(),
        // `--fork` branches the named session into a new one, whose id pi
        // reports in `get_state`.
        if cfg.fork && cfg.resume.is_some() {
            "--fork".to_string()
        } else {
            "--session-id".to_string()
        },
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
    args.push("-e".into());
    args.push(gate.to_string_lossy().into_owned());
    args.extend(cfg.extra_args.iter().cloned());
    (args, session_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Confirm,
    Select,
    Input,
    /// The gate extension asking about a tool call.
    Gate,
}

/// Whether `line` is pi's response to `command`.
fn is_response_to(line: &str, command: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|v| {
        v.get("type").and_then(Value::as_str) == Some("response")
            && v.get("command").and_then(Value::as_str) == Some(command)
    })
}

/// A `prompt` or `steer` command; images are inlined as base64.
pub fn encode_message(
    kind: &str,
    id: &str,
    text: &str,
    attachments: &[Attachment],
) -> Result<String> {
    let mut cmd = json!({"id": id, "type": kind, "message": text});
    if attachments.iter().any(Attachment::is_image) {
        let images = attachments
            .iter()
            .filter(|a| a.is_image())
            .map(|a| Ok(json!({"type":"image","data": a.read_base64()?,"mimeType": a.mime()})))
            .collect::<Result<Vec<Value>>>()?;
        cmd["images"] = Value::Array(images);
    }
    Ok(cmd.to_string())
}

pub fn encode_ui_response(id: &str, kind: PendingKind, decision: &PermissionDecision) -> String {
    let mut v = json!({"type":"extension_ui_response","id":id});
    match (kind, decision) {
        (PendingKind::Gate, PermissionDecision::Allow { .. }) => v["value"] = json!(gate::ALLOW),
        // Anything but an allow keeps the tool from running.
        (PendingKind::Gate, _) => v["value"] = json!(gate::DENY),
        (PendingKind::Confirm, PermissionDecision::Allow { .. }) => v["confirmed"] = json!(true),
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

pub fn start(cfg: SessionConfig, gate: &Path) -> Result<SessionHandle> {
    let (args, session_id) = session_args(&cfg, gate);
    let mut cmd = Command::new(&cfg.binary);
    cmd.args(args).current_dir(&cfg.cwd);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let proc = LineProcess::spawn(cmd, &cfg.sandbox)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness: HarnessId::PI,
        process_model: ProcessModel::LongLived,
    });
    let resumed = cfg.resume.is_some();
    tokio::spawn(drive(
        proc, session_id, resumed, cfg.policy, events_tx, cmd_rx,
    ));
    Ok(handle)
}

async fn drive(
    mut proc: LineProcess,
    session_id: String,
    resumed: bool,
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
    // A rewind (`fork`) is in flight; a turn sent meanwhile is held back.
    let mut forking = false;
    let mut held_turn: Option<String> = None;

    let _ = proc
        .write_line(&json!({"id": next_id(), "type":"get_state"}).to_string())
        .await;
    if resumed {
        // Learn which user messages predate this run, so later listings
        // can tell which one is new.
        parser.expect_fork_baseline();
        let _ = proc
            .write_line(&json!({"id": next_id(), "type":"get_fork_messages"}).to_string())
            .await;
    }

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { break };
                let is_turn = matches!(cmd, SessionCommand::SendTurn { .. });
                let line = match cmd {
                    SessionCommand::SendTurn { text, attachments } => match encode_message("prompt", &next_id(), &text, &attachments) {
                        Ok(line) => Some(line),
                        Err(e) => {
                            let _ = events.send(AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Error(e.to_string()),
                            }).await;
                            None
                        }
                    },
                    SessionCommand::Steer { text, attachments } => match encode_message("steer", &next_id(), &text, &attachments) {
                        Ok(line) => Some(line),
                        Err(e) => {
                            let _ = events.send(AgentEvent::Error(e.to_string())).await;
                            None
                        }
                    },
                    // `fork` branches a new session from before that message. It
                    // is only in place once it answers, so turns wait for that.
                    SessionCommand::Rewind { anchor } => {
                        forking = true;
                        Some(json!({"id": next_id(), "type":"fork","entryId": anchor}).to_string())
                    }
                    SessionCommand::Compact { instructions } => {
                        let mut cmd = json!({"id": next_id(), "type":"compact"});
                        if let Some(i) = instructions {
                            cmd["customInstructions"] = json!(i);
                        }
                        Some(cmd.to_string())
                    }
                    SessionCommand::Interrupt => Some(json!({"id": next_id(), "type":"abort"}).to_string()),
                    // No subagents are reported here, so none can be running.
                    SessionCommand::StopSubagent { .. } => None,
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
                        // The new model may differ in thinking levels, context
                        // window and image support.
                        let _ = proc.write_line(&json!({"id": next_id(), "type":"get_state"}).to_string()).await;
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
                        proc.end();
                        None
                    }
                };
                if forking && is_turn {
                    held_turn = line;
                } else if let Some(line) = line
                    && let Err(e) = proc.write_line(&line).await
                {
                    let _ = events.send(AgentEvent::Error(format!("pi: {e}"))).await;
                }
            }
            raw = proc.lines.recv() => {
                match raw {
                    Some(RawLine::Stdout(line)) => {
                        if forking && is_response_to(&line, "fork") {
                            // The fork is a new session: re-read its id and
                            // message list, then send what was waiting.
                            forking = false;
                            let _ = proc.write_line(&json!({"id": next_id(), "type":"get_state"}).to_string()).await;
                            let _ = proc.write_line(&json!({"id": next_id(), "type":"get_fork_messages"}).to_string()).await;
                            if let Some(turn) = held_turn.take() {
                                let _ = proc.write_line(&turn).await;
                            }
                        }
                        for ev in parser.feed(&line) {
                            if let AgentEvent::PermissionRequest(req) = &ev {
                                let kind = match &req.kind {
                                    PermissionKind::ToolUse { .. } => PendingKind::Gate,
                                    PermissionKind::Confirm { .. } => PendingKind::Confirm,
                                    PermissionKind::Select { .. } => PendingKind::Select,
                                    _ => PendingKind::Input,
                                };
                                if policy == PermissionPolicy::Bypass && kind == PendingKind::Gate {
                                    let allow = PermissionDecision::Allow { updated_input: None };
                                    let _ = proc.write_line(&encode_ui_response(&req.id, kind, &allow)).await;
                                    continue;
                                }
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
                            // Context usage is only available on request.
                            if matches!(ev, AgentEvent::TurnCompleted { .. }) {
                                let _ = proc.write_line(&json!({"id": next_id(), "type":"get_session_stats"}).to_string()).await;
                                // The turn's user message now has an entry id to rewind to.
                                let _ = proc.write_line(&json!({"id": next_id(), "type":"get_fork_messages"}).to_string()).await;
                            }
                            if events.send(ev).await.is_err() {
                                if !shutting_down {
                                    if shutdown_queued(&mut cmds) {
                                        proc.end();
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
    fn prompt_inlines_images() {
        // Shape confirmed by fixtures/image_turn.jsonl.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        std::fs::write(&path, b"abc").unwrap();
        let a = Attachment::image(&path).unwrap();
        let v: Value =
            serde_json::from_str(&encode_message("prompt", "u1", "hi", &[a]).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"id":"u1","type":"prompt","message":"hi",
                   "images":[{"type":"image","data":"YWJj","mimeType":"image/jpeg"}]})
        );
        let v: Value =
            serde_json::from_str(&encode_message("steer", "u2", "hi", &[]).unwrap()).unwrap();
        assert!(v.get("images").is_none());
        assert_eq!(v["type"], "steer");
    }

    #[test]
    fn session_args_shape() {
        let cfg = SessionConfig {
            binary: PathBuf::from("pi"),
            cwd: PathBuf::from("/tmp"),
            model: Some(ModelRef::new(
                HarnessId::PI,
                "anthropic",
                "claude-sonnet-4-5",
            )),
            provider: None,
            effort: Some("high".into()),
            policy: PermissionPolicy::Ask,
            resume: None,
            fork: false,
            extra_args: vec!["--no-extensions".into()],
            env: vec![],
            mcp_servers: Vec::new(),
            sandbox: crate::core::Sandbox::off(),
        };
        let gate = Path::new("/state/gate.ts");
        let (args, sid) = session_args(&cfg, gate);
        let s = args.join(" ");
        assert!(s.starts_with(&format!("--mode rpc --session-id {sid}")));
        assert!(s.contains("--provider anthropic --model claude-sonnet-4-5"));
        assert!(s.contains("--thinking high"));
        // The gate is loaded whatever the user's own arguments say.
        assert!(s.ends_with("-e /state/gate.ts --no-extensions"));

        // Checked live: `--fork` branches the session into a new one whose
        // id pi reports in get_state; entry ids are kept.
        let fork = SessionConfig {
            resume: Some("abc".into()),
            fork: true,
            ..cfg.clone()
        };
        assert!(
            session_args(&fork, gate)
                .0
                .join(" ")
                .starts_with("--mode rpc --fork abc")
        );

        let mut r = cfg.clone();
        r.resume = Some("abc".into());
        let (args, sid) = session_args(&r, gate);
        assert_eq!(sid, "abc");
        assert!(args.join(" ").contains("--session-id abc"));
    }

    #[test]
    fn gate_answers() {
        let value = |decision: PermissionDecision| {
            let line = encode_ui_response("g1", PendingKind::Gate, &decision);
            serde_json::from_str::<Value>(&line).unwrap()["value"].clone()
        };
        assert_eq!(
            value(PermissionDecision::Allow {
                updated_input: None
            }),
            "Allow"
        );
        // Whatever is not an allow keeps the tool from running.
        assert_eq!(
            value(PermissionDecision::Deny {
                reason: "no".into()
            }),
            "Deny"
        );
        assert_eq!(value(PermissionDecision::Answer(json!("Allow"))), "Deny");
        assert_eq!(value(PermissionDecision::Answer(Value::Null)), "Deny");
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
