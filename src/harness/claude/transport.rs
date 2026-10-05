//! Long-lived Claude Code session over `--input-format stream-json`.
//!
//! One `claude -p` process per session. Turns are written to stdin as
//! `{"type":"user",...}` lines; permission prompts arrive as
//! `control_request` lines (because we pass `--permission-prompt-tool stdio`)
//! and are answered with `control_response` lines.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::parse::ClaudeParser;
use crate::core::process::{LineProcess, RawLine};
use crate::core::{
    AgentEvent, Attachment, HarnessId, PermissionDecision, PermissionKind, PermissionPolicy,
    ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo, StopReason,
};

/// Flags for a permission policy. `Ask` maps to Claude's default mode with
/// prompts routed to us over stdio.
pub fn policy_args(policy: PermissionPolicy) -> Vec<&'static str> {
    match policy {
        PermissionPolicy::Ask => vec!["--permission-mode", "default"],
        PermissionPolicy::AcceptEdits => vec!["--permission-mode", "acceptEdits"],
        PermissionPolicy::Auto => vec!["--permission-mode", "auto"],
        PermissionPolicy::Bypass => vec![
            "--permission-mode",
            "bypassPermissions",
            "--dangerously-skip-permissions",
        ],
    }
}

/// Claude's `set_permission_mode` control request value for a policy.
pub fn policy_mode_name(policy: PermissionPolicy) -> &'static str {
    match policy {
        PermissionPolicy::Ask => "default",
        PermissionPolicy::AcceptEdits => "acceptEdits",
        PermissionPolicy::Auto => "auto",
        PermissionPolicy::Bypass => "bypassPermissions",
    }
}

/// The exact argv (after the binary) for an interactive session.
pub fn session_args(cfg: &SessionConfig) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--forward-subagent-text",
        "--permission-prompts",
        "host",
        "--permission-prompt-tool",
        "stdio",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    if let Some(m) = &cfg.model {
        args.push("--model".into());
        args.push(m.model.clone());
    }
    if let Some(e) = &cfg.effort {
        args.push("--effort".into());
        args.push(e.clone());
    }
    match &cfg.resume {
        Some(id) => {
            args.push("--resume".into());
            args.push(id.clone());
            if cfg.fork {
                args.push("--fork-session".into());
            }
        }
        None => {
            args.push("--session-id".into());
            args.push(uuid::Uuid::new_v4().to_string());
        }
    }
    args.extend(policy_args(cfg.policy).iter().map(|s| s.to_string()));
    args.extend(cfg.extra_args.iter().cloned());
    args
}

pub fn user_message(text: &str) -> String {
    json!({"type":"user","message":{"role":"user","content":text}}).to_string()
}

/// A user turn; images and PDFs ride along as base64 content blocks, text
/// files as text documents.
pub fn user_turn(text: &str, attachments: &[Attachment]) -> Result<String> {
    if attachments.is_empty() {
        return Ok(user_message(text));
    }
    let mut content = vec![json!({"type":"text","text":text})];
    for a in attachments {
        content.push(match a {
            Attachment::Image { mime, .. } => json!({
                "type": "image",
                "source": {"type":"base64","media_type": mime,"data": a.read_base64()?},
            }),
            Attachment::File { mime, .. } if mime == "text/plain" => json!({
                "type": "document",
                "source": {"type":"text","media_type": mime,"data": a.read_text()?},
                "title": a.label(),
            }),
            Attachment::File { mime, .. } => json!({
                "type": "document",
                "source": {"type":"base64","media_type": mime,"data": a.read_base64()?},
                "title": a.label(),
            }),
        });
    }
    Ok(json!({"type":"user","message":{"role":"user","content":content}}).to_string())
}

/// A user turn tagged with a fresh uuid: `(uuid, line)`. `rewind_conversation`
/// takes that uuid to drop the turn and everything after it.
pub fn anchored_turn(text: &str, attachments: &[Attachment]) -> Result<(String, String)> {
    let anchor = uuid::Uuid::new_v4().to_string();
    let mut v: Value = serde_json::from_str(&user_turn(text, attachments)?)?;
    v["uuid"] = json!(anchor);
    Ok((anchor, v.to_string()))
}

/// A user message merged into the running turn once its tool calls finish.
pub fn steer_turn(text: &str, attachments: &[Attachment]) -> Result<String> {
    let mut v: Value = serde_json::from_str(&user_turn(text, attachments)?)?;
    v["priority"] = json!("next");
    Ok(v.to_string())
}

pub fn control_request(subtype: &str, extra: Value) -> (String, String) {
    let id = uuid::Uuid::new_v4().to_string();
    let mut req = json!({"subtype": subtype});
    if let (Some(dst), Some(src)) = (req.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    (
        id.clone(),
        json!({"type":"control_request","request_id":id,"request":req}).to_string(),
    )
}

/// What we remember about an outstanding `can_use_tool` request so the
/// decision can be encoded.
#[derive(Debug, Clone)]
pub struct PendingPermission {
    input: Value,
    suggestions: Option<Value>,
    is_question: bool,
}

pub fn encode_decision(pending: &PendingPermission, decision: &PermissionDecision) -> Value {
    match decision {
        PermissionDecision::Allow { updated_input } => json!({
            "behavior": "allow",
            "updatedInput": updated_input.clone().unwrap_or_else(|| pending.input.clone()),
        }),
        PermissionDecision::AllowAlways => {
            let mut v = json!({"behavior":"allow","updatedInput": pending.input.clone()});
            if let Some(s) = &pending.suggestions {
                v["updatedPermissions"] = s.clone();
            }
            v
        }
        PermissionDecision::Deny { reason } => json!({"behavior":"deny","message": reason}),
        PermissionDecision::Answer(answer) => {
            if pending.is_question {
                if answer.is_null() {
                    json!({"behavior":"deny","message":"User dismissed the question"})
                } else {
                    let mut input = pending.input.clone();
                    input["answers"] = answer.clone();
                    json!({"behavior":"allow","updatedInput": input})
                }
            } else {
                json!({"behavior":"allow","updatedInput": pending.input.clone()})
            }
        }
    }
}

pub fn control_response(request_id: &str, response: Value) -> String {
    json!({
        "type":"control_response",
        "response":{"subtype":"success","request_id":request_id,"response":response}
    })
    .to_string()
}

pub fn start(cfg: SessionConfig) -> Result<SessionHandle> {
    let mut cmd = Command::new(&cfg.binary);
    cmd.args(session_args(&cfg)).current_dir(&cfg.cwd);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    // Never inherit a parent Claude Code session's identity.
    for var in [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
    ] {
        cmd.env_remove(var);
    }

    let proc = LineProcess::spawn(cmd, &cfg.sandbox)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness: HarnessId::CLAUDE,
        process_model: ProcessModel::LongLived,
    });

    tokio::spawn(drive(proc, events_tx, cmd_rx));
    Ok(handle)
}

async fn drive(
    mut proc: LineProcess,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
) {
    let mut parser = ClaudeParser::new();
    let mut pending: HashMap<String, PendingPermission> = HashMap::new();
    let mut shutting_down = false;

    // Handshake. The CLI answers with its command/model catalog; we only need
    // the ack. Not required for turns to work, so failure is a notice.
    let (init_id, init_line) = control_request("initialize", json!({}));
    if let Err(e) = proc.write_line(&init_line).await {
        let _ = events.send(AgentEvent::Error(format!("claude: {e}"))).await;
    }
    let mut awaiting_init = Some(init_id);
    let init_deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(init_deadline);

    loop {
        tokio::select! {
            _ = &mut init_deadline, if awaiting_init.is_some() => {
                awaiting_init = None;
                let _ = events.send(AgentEvent::Notice("claude: no initialize ack; continuing".into())).await;
            }
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    // TUI dropped the handle.
                    proc.kill().await;
                    break;
                };
                let line = match cmd {
                    // Each turn carries a uuid of ours, so it can be rewound to later.
                    SessionCommand::SendTurn { text, attachments } => match anchored_turn(&text, &attachments) {
                        Ok((anchor, line)) => {
                            let _ = events.send(AgentEvent::TurnAnchor { id: anchor }).await;
                            Some(line)
                        }
                        Err(e) => {
                            let _ = events.send(AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Error(e.to_string()),
                            }).await;
                            None
                        }
                    },
                    SessionCommand::Steer { text, attachments } => match steer_turn(&text, &attachments) {
                        Ok(line) => Some(line),
                        Err(e) => {
                            let _ = events.send(AgentEvent::Error(e.to_string())).await;
                            None
                        }
                    },
                    SessionCommand::Rewind { anchor } => Some(
                        control_request("rewind_conversation", json!({"target_message_uuid": anchor})).1,
                    ),
                    // `-p` mode accepts the slash command as a message; it ends
                    // with a `result`, like a turn.
                    SessionCommand::Compact { instructions } => Some(user_message(
                        format!("/compact {}", instructions.unwrap_or_default()).trim_end(),
                    )),
                    SessionCommand::Interrupt => Some(control_request("interrupt", json!({})).1),
                    // Claude then tells of the stop in a turn of its own.
                    SessionCommand::StopSubagent { id } => match parser.task_of(&id) {
                        Some(task) => Some(control_request("stop_task", json!({"task_id": task})).1),
                        None => {
                            let _ = events.send(AgentEvent::Notice("that subagent is no longer running".into())).await;
                            None
                        }
                    },
                    SessionCommand::RespondPermission { id, decision } => {
                        match pending.remove(&id) {
                            Some(p) => Some(control_response(&id, encode_decision(&p, &decision))),
                            None => {
                                let _ = events.send(AgentEvent::Notice(format!("no pending permission request {id}"))).await;
                                None
                            }
                        }
                    }
                    SessionCommand::SetModel(m) => Some(control_request("set_model", json!({"model": m.model})).1),
                    SessionCommand::SetEffort(e) => {
                        // `-p` mode accepts the slash command as a message.
                        Some(user_message(&format!("/effort {}", e.unwrap_or_else(|| "high".into()))))
                    }
                    SessionCommand::SetPolicy(p) => Some(
                        control_request("set_permission_mode", json!({"mode": policy_mode_name(p)})).1,
                    ),
                    SessionCommand::Shutdown => {
                        shutting_down = true;
                        proc.close_stdin();
                        None
                    }
                };
                if let Some(line) = line
                    && let Err(e) = proc.write_line(&line).await
                {
                    let _ = events.send(AgentEvent::Error(format!("claude: {e}"))).await;
                }
            }
            raw = proc.lines.recv() => {
                match raw {
                    Some(RawLine::Stdout(line)) => {
                        // Driver-level bookkeeping before the pure parser sees it.
                        if let Ok(v) = serde_json::from_str::<Value>(&line) {
                            match v.get("type").and_then(Value::as_str) {
                                Some("control_response") => {
                                    let rid = v.pointer("/response/request_id").and_then(Value::as_str);
                                    if rid.is_some() && rid == awaiting_init.as_deref() {
                                        awaiting_init = None;
                                    }
                                }
                                Some("control_cancel_request") => {
                                    if let Some(rid) = v.get("request_id").and_then(Value::as_str) {
                                        pending.remove(rid);
                                    }
                                }
                                _ => {}
                            }
                        }
                        for ev in parser.feed(&line) {
                            if let AgentEvent::PermissionRequest(req) = &ev {
                                let (input, suggestions, is_question) = match &req.kind {
                                    PermissionKind::ToolUse { input, suggestions, .. } => (input.clone(), suggestions.clone(), false),
                                    PermissionKind::Question { .. } => (Value::Null, None, true),
                                    _ => (Value::Null, None, false),
                                };
                                // For questions we need the original input to echo back.
                                let input = if is_question {
                                    serde_json::from_str::<Value>(&line)
                                        .ok()
                                        .and_then(|v| v.pointer("/request/input").cloned())
                                        .unwrap_or(Value::Null)
                                } else { input };
                                pending.insert(req.id.clone(), PendingPermission { input, suggestions, is_question });
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

    fn cfg(policy: PermissionPolicy) -> SessionConfig {
        SessionConfig {
            binary: PathBuf::from("claude"),
            cwd: PathBuf::from("/tmp"),
            model: Some(ModelRef::new(HarnessId::CLAUDE, "anthropic", "opus")),
            effort: Some("high".into()),
            policy,
            resume: None,
            fork: false,
            extra_args: vec!["--bare".into()],
            env: vec![],
            sandbox: crate::core::Sandbox::off(),
        }
    }

    #[test]
    fn session_args_shape() {
        let a = session_args(&cfg(PermissionPolicy::Ask));
        let s = a.join(" ");
        assert!(s.starts_with("-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --forward-subagent-text --permission-prompts host --permission-prompt-tool stdio"));
        assert!(s.contains("--model opus"));
        assert!(s.contains("--effort high"));
        assert!(s.contains("--session-id "));
        assert!(s.contains("--permission-mode default"));
        assert!(s.ends_with("--bare"));
        assert!(!s.contains("--resume"));
    }

    #[test]
    fn resume_replaces_session_id() {
        let mut c = cfg(PermissionPolicy::Bypass);
        c.resume = Some("abc".into());
        let s = session_args(&c).join(" ");
        assert!(s.contains("--resume abc"));
        assert!(!s.contains("--session-id"));
        assert!(s.contains("--permission-mode bypassPermissions --dangerously-skip-permissions"));
    }

    #[test]
    fn decision_encoding() {
        let p = PendingPermission {
            input: json!({"file_path":"/x"}),
            suggestions: Some(json!([{"type":"setMode","mode":"acceptEdits"}])),
            is_question: false,
        };
        let allow = encode_decision(
            &p,
            &PermissionDecision::Allow {
                updated_input: None,
            },
        );
        assert_eq!(
            allow,
            json!({"behavior":"allow","updatedInput":{"file_path":"/x"}})
        );
        let always = encode_decision(&p, &PermissionDecision::AllowAlways);
        assert_eq!(always["updatedPermissions"][0]["mode"], "acceptEdits");
        let deny = encode_decision(
            &p,
            &PermissionDecision::Deny {
                reason: "no".into(),
            },
        );
        assert_eq!(deny, json!({"behavior":"deny","message":"no"}));

        let q = PendingPermission {
            input: json!({"questions":[{"question":"Color?"}]}),
            suggestions: None,
            is_question: true,
        };
        let ans = encode_decision(&q, &PermissionDecision::Answer(json!({"Color?":"Red"})));
        assert_eq!(ans["behavior"], "allow");
        assert_eq!(ans["updatedInput"]["answers"]["Color?"], "Red");
        assert_eq!(ans["updatedInput"]["questions"][0]["question"], "Color?");
        let dismissed = encode_decision(&q, &PermissionDecision::Answer(Value::Null));
        assert_eq!(dismissed["behavior"], "deny");

        let line = control_response("r1", allow);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["subtype"], "success");
        assert_eq!(v["response"]["request_id"], "r1");
    }

    #[test]
    fn user_turn_inlines_images() {
        // Shape confirmed by fixtures/image_turn.jsonl.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        std::fs::write(&path, b"abc").unwrap();
        let a = Attachment::image(&path).unwrap();
        let v: Value = serde_json::from_str(&user_turn("hi", &[a]).unwrap()).unwrap();
        assert_eq!(
            v["message"]["content"],
            json!([
                {"type":"text","text":"hi"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"YWJj"}}
            ])
        );
        assert_eq!(user_turn("hi", &[]).unwrap(), user_message("hi"));
        let gone = Attachment::image(dir.path().join("gone.png")).unwrap();
        assert!(user_turn("hi", &[gone]).is_err());
    }

    #[test]
    fn user_turn_inlines_documents() {
        // Shapes confirmed by fixtures/pdf_turn.jsonl and fixtures/text_file_turn.jsonl.
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("memo.pdf");
        let notes = dir.path().join("notes.txt");
        std::fs::write(&pdf, b"abc").unwrap();
        std::fs::write(&notes, "héron\n").unwrap();
        let files = [
            Attachment::file(&pdf).unwrap(),
            Attachment::file(&notes).unwrap(),
        ];
        let v: Value = serde_json::from_str(&user_turn("hi", &files).unwrap()).unwrap();
        assert_eq!(
            v["message"]["content"],
            json!([
                {"type":"text","text":"hi"},
                {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"YWJj"},"title":"memo.pdf"},
                {"type":"document","source":{"type":"text","media_type":"text/plain","data":"héron\n"},"title":"notes.txt"}
            ])
        );
    }

    #[test]
    fn fork_branches_the_resumed_session() {
        // Checked live: the branch gets a new session id and keeps the history.
        let mut cfg = cfg(PermissionPolicy::Ask);
        cfg.resume = Some("abc".into());
        cfg.fork = true;
        let s = session_args(&cfg).join(" ");
        assert!(s.contains("--resume abc --fork-session"));
        cfg.fork = false;
        assert!(!session_args(&cfg).join(" ").contains("--fork-session"));
    }

    #[test]
    fn turns_carry_their_own_uuid() {
        // Shape confirmed by fixtures/rewind.jsonl.
        let (anchor, line) = anchored_turn("hi", &[]).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["uuid"], anchor.as_str());
        assert_eq!(v["message"]["content"], "hi");
        assert_ne!(anchor, anchored_turn("hi", &[]).unwrap().0);
    }

    #[test]
    fn steer_is_a_prioritised_user_message() {
        // Shape confirmed by fixtures/steer_and_compact.jsonl.
        let v: Value = serde_json::from_str(&steer_turn("more", &[]).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"type":"user","message":{"role":"user","content":"more"},"priority":"next"})
        );
    }

    #[test]
    fn user_message_shape() {
        let v: Value = serde_json::from_str(&user_message("hi")).unwrap();
        assert_eq!(
            v,
            json!({"type":"user","message":{"role":"user","content":"hi"}})
        );
    }
}
