//! Long-lived pi session over `--mode rpc`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;

use super::gate;
use super::parse::PiParser;
use crate::core::driver::{self, Encoded, Gone, Io, Shutdown, StreamProtocol};
use crate::core::{
    AgentEvent, Attachment, Capabilities, HarnessId, Parser, PermissionDecision, PermissionKind,
    PermissionPolicy, SessionCommand, SessionConfig, SessionHandle, StopReason,
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

/// The gate dialogs still open, taken out of `pending`. A turn ends with
/// one open only when it was aborted: the gate passes the turn's signal to
/// its dialog, which closes with the turn, and pi says nothing about that
/// (`fixtures/gate_abort.jsonl`). Other extensions' dialogs may outlive a
/// turn, so they stay.
fn take_gate_dialogs(pending: &mut HashMap<String, PendingKind>) -> Vec<String> {
    let mut ids: Vec<String> = pending
        .iter()
        .filter(|(_, kind)| **kind == PendingKind::Gate)
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    for id in &ids {
        pending.remove(id);
    }
    ids
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

/// `caps` is what the harness declares.
pub fn start(cfg: SessionConfig, gate: &Path, caps: &Capabilities) -> Result<SessionHandle> {
    let (args, session_id) = session_args(&cfg, gate);
    let mut cmd = Command::new(&cfg.binary);
    cmd.args(args).current_dir(&cfg.cwd);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let resumed = cfg.resume.is_some();
    driver::start(
        HarnessId::PI,
        caps,
        cmd,
        &cfg.sandbox,
        Pi::new(session_id, resumed, cfg.policy),
    )
}

/// What the driver does with pi's RPC.
struct Pi {
    parser: PiParser,
    pending: HashMap<String, PendingKind>,
    /// The last of our request ids (`u<n>`).
    seq: u64,
    policy: PermissionPolicy,
    resumed: bool,
    turn_open: bool,
    /// `abort` was sent during the open turn. pi ends an aborted turn the way
    /// it ends any other (`agent_settled`), so this is what tells them apart.
    aborting: bool,
    /// A rewind (`fork`) is in flight; a turn sent meanwhile is held back.
    forking: bool,
    /// A resumed session's first listing has not answered: a turn sent
    /// meanwhile is held back too, since pi saves a prompt under the id
    /// it was given (`SessionManager._persist`, 1.1.0) and the listing may
    /// show that the session is gone (`SessionReplaced`). Sent there, the
    /// prompt would make the empty session pi made under the id pass as
    /// the one to resume next time (#214).
    baseline: bool,
    held_turn: Option<String>,
}

impl Pi {
    fn new(session_id: String, resumed: bool, policy: PermissionPolicy) -> Self {
        Pi {
            parser: PiParser::new(Some(session_id)),
            pending: HashMap::new(),
            seq: 0,
            policy,
            resumed,
            turn_open: false,
            aborting: false,
            forking: false,
            baseline: resumed,
            held_turn: None,
        }
    }

    fn next_id(&mut self) -> String {
        self.seq += 1;
        format!("u{}", self.seq)
    }

    /// A request of ours that takes nothing but its id.
    fn request(&mut self, kind: &str) -> String {
        json!({"id": self.next_id(), "type": kind}).to_string()
    }
}

impl StreamProtocol for Pi {
    async fn start(&mut self, io: &mut Io<'_>) {
        let _ = io.write(&self.request("get_state")).await;
        let _ = io.write(&self.request("get_commands")).await;
        if self.resumed {
            // Learn which user messages predate this run, so later listings
            // can tell which one is new.
            self.parser.expect_fork_baseline();
            let _ = io.write(&self.request("get_fork_messages")).await;
        }
    }

    async fn encode(&mut self, cmd: SessionCommand, io: &mut Io<'_>) -> Encoded {
        let is_turn = matches!(cmd, SessionCommand::SendTurn { .. });
        let line = match cmd {
            SessionCommand::SendTurn { text, attachments } => {
                match encode_message("prompt", &self.next_id(), &text, &attachments) {
                    Ok(line) => Some(line),
                    Err(e) => {
                        io.emit(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Error(e.to_string()),
                        })
                        .await;
                        None
                    }
                }
            }
            SessionCommand::Steer { text, attachments } => {
                match encode_message("steer", &self.next_id(), &text, &attachments) {
                    Ok(line) => Some(line),
                    Err(e) => {
                        io.emit(AgentEvent::Error(e.to_string())).await;
                        None
                    }
                }
            }
            // `fork` branches a new session from before that message. It
            // is only in place once it answers, so turns wait for that.
            SessionCommand::Rewind { anchor } => {
                self.forking = true;
                Some(json!({"id": self.next_id(), "type":"fork","entryId": anchor}).to_string())
            }
            SessionCommand::Compact { instructions } => {
                let mut cmd = json!({"id": self.next_id(), "type":"compact"});
                if let Some(i) = instructions {
                    cmd["customInstructions"] = json!(i);
                }
                Some(cmd.to_string())
            }
            SessionCommand::Interrupt => {
                if self.held_turn.take().is_some() {
                    // A turn held for a fork has not reached pi: it
                    // is dropped, and ends here.
                    self.turn_open = false;
                    io.emit(AgentEvent::TurnCompleted {
                        stop_reason: StopReason::Interrupted,
                    })
                    .await;
                    None
                } else {
                    self.aborting |= self.turn_open;
                    Some(self.request("abort"))
                }
            }
            // pi reports no subagents, so none can be running, and has no
            // remote control.
            SessionCommand::StopSubagent { .. } | SessionCommand::RemoteControl { .. } => {
                return Encoded::Unsupported;
            }
            SessionCommand::RespondPermission { id, decision } => match self.pending.remove(&id) {
                Some(kind) => Some(encode_ui_response(&id, kind, &decision)),
                None => {
                    io.emit(AgentEvent::Notice(format!("no pending pi dialog {id}")))
                        .await;
                    None
                }
            },
            SessionCommand::SetModel(m) => {
                let set = json!({
                    "id": self.next_id(), "type":"set_model",
                    "provider": m.provider.as_str(), "modelId": m.model
                });
                let _ = io.write(&set.to_string()).await;
                // The new model may differ in thinking levels, context
                // window and image support.
                let _ = io.write(&self.request("get_state")).await;
                Some(self.request("get_available_thinking_levels"))
            }
            SessionCommand::SetEffort(e) => Some(
                json!({
                    "id": self.next_id(), "type":"set_thinking_level",
                    "level": e.unwrap_or_else(|| "medium".into())
                })
                .to_string(),
            ),
            SessionCommand::SetPolicy(p) => {
                self.policy = p;
                None
            }
            // The driver's (`shutdown`).
            SessionCommand::Shutdown => None,
        };
        self.turn_open |= is_turn && line.is_some();
        if (self.forking || self.baseline) && is_turn {
            self.held_turn = line;
            Encoded::Done
        } else {
            line.map_or(Encoded::Done, Encoded::Write)
        }
    }

    fn write_failed(&mut self, error: anyhow::Error) -> Vec<AgentEvent> {
        vec![AgentEvent::Error(format!("pi: {error}"))]
    }

    fn parser(&mut self) -> &mut dyn Parser {
        &mut self.parser
    }

    async fn on_stdout(&mut self, line: &str, io: &mut Io<'_>) -> Result<(), Gone> {
        if self.forking && is_response_to(line, "fork") {
            // The fork is a new session: re-read its id and
            // message list, then send what was waiting.
            self.forking = false;
            let _ = io.write(&self.request("get_state")).await;
            let _ = io.write(&self.request("get_fork_messages")).await;
            if !self.baseline
                && let Some(turn) = self.held_turn.take()
            {
                let _ = io.write(&turn).await;
            }
        }
        let baseline_answer = self.baseline && is_response_to(line, "get_fork_messages");
        let mut replaced = false;
        for mut ev in self.parser.feed(line) {
            replaced |= matches!(ev, AgentEvent::SessionReplaced { .. });
            if let AgentEvent::PermissionRequest(req) = &ev {
                let kind = match &req.kind {
                    PermissionKind::ToolUse { .. } => PendingKind::Gate,
                    PermissionKind::Confirm { .. } => PendingKind::Confirm,
                    PermissionKind::Select { .. } => PendingKind::Select,
                    _ => PendingKind::Input,
                };
                if self.policy == PermissionPolicy::Bypass && kind == PendingKind::Gate {
                    let allow = PermissionDecision::Allow {
                        updated_input: None,
                    };
                    let _ = io.write(&encode_ui_response(&req.id, kind, &allow)).await;
                    continue;
                }
                // Under Bypass, confirm dialogs are auto-accepted and
                // selects take the first option; text input still asks.
                if self.policy == PermissionPolicy::Bypass && kind != PendingKind::Input {
                    let answer = match &req.kind {
                        PermissionKind::Select { options, .. } => PermissionDecision::Answer(
                            options
                                .first()
                                .cloned()
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        ),
                        _ => PermissionDecision::Answer(Value::Bool(true)),
                    };
                    let _ = io.write(&encode_ui_response(&req.id, kind, &answer)).await;
                    io.emit(AgentEvent::Notice(
                        "auto-accepted an extension dialog (bypass)".into(),
                    ))
                    .await;
                    continue;
                }
                self.pending.insert(req.id.clone(), kind);
            }
            // Context usage is only available on request.
            if let AgentEvent::TurnCompleted { stop_reason } = &mut ev {
                self.turn_open = false;
                if std::mem::take(&mut self.aborting) && *stop_reason == StopReason::Done {
                    *stop_reason = StopReason::Interrupted;
                }
                for id in take_gate_dialogs(&mut self.pending) {
                    io.emit(AgentEvent::PermissionWithdrawn { id }).await;
                }
                let _ = io.write(&self.request("get_session_stats")).await;
                // The turn's user message now has an entry id to rewind to.
                let _ = io.write(&self.request("get_fork_messages")).await;
            }
            io.forward(ev).await?;
        }
        if baseline_answer {
            self.baseline = false;
            match self.held_turn.take() {
                // The session to resume is gone: the prompt
                // is not sent to the one pi made in its place.
                Some(_) if replaced => {
                    self.turn_open = false;
                    io.emit(AgentEvent::TurnCompleted {
                        stop_reason: StopReason::Error(
                            "pi: the session to resume is gone, so this prompt was not sent; send it again to have the new session told the conversation".into(),
                        ),
                    })
                    .await;
                }
                Some(turn) if !self.forking => {
                    let _ = io.write(&turn).await;
                }
                held => self.held_turn = held,
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Shutdown {
        Shutdown::EndOrKill {
            turn_open: self.turn_open,
        }
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
            workspace_root: None,
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
    fn only_gate_dialogs_close_with_the_turn() {
        let mut pending = HashMap::from([
            ("g2".to_string(), PendingKind::Gate),
            ("s1".to_string(), PendingKind::Select),
            ("g1".to_string(), PendingKind::Gate),
        ]);
        assert_eq!(take_gate_dialogs(&mut pending), ["g1", "g2"]);
        assert_eq!(pending.keys().collect::<Vec<_>>(), ["s1"]);
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
