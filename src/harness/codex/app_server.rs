//! Long-lived Codex session over `codex app-server` (JSON-RPC 2.0 on stdio).

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::app_server_parse::CodexAppServerParser;
use crate::core::jsonrpc::{self, RpcMessage};
use crate::core::process::{LineProcess, RawLine};
use crate::core::{
    AgentEvent, HarnessId, ModelRef, PermissionDecision, PermissionKind, PermissionPolicy,
    ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo, StopReason,
};

/// `thread/start` parameters for a policy.
pub fn policy_params(policy: PermissionPolicy) -> (&'static str, &'static str) {
    match policy {
        PermissionPolicy::Ask => ("untrusted", "read-only"),
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => ("on-request", "workspace-write"),
        PermissionPolicy::Bypass => ("never", "danger-full-access"),
    }
}

pub fn encode_decision(kind: &PermissionKind, decision: &PermissionDecision) -> Value {
    match kind {
        PermissionKind::Question { .. } => match decision {
            PermissionDecision::Answer(Value::Object(map)) => {
                let answers: serde_json::Map<String, Value> = map
                    .iter()
                    .map(|(q, a)| {
                        let list = match a {
                            Value::Array(items) => items.clone(),
                            Value::Null => vec![],
                            other => vec![other.clone()],
                        };
                        (q.clone(), json!({"answers": list}))
                    })
                    .collect();
                json!({"answers": answers})
            }
            _ => json!({"answers": {}}),
        },
        _ => {
            let decision = match decision {
                PermissionDecision::Allow { .. } => "accept",
                PermissionDecision::AllowAlways => "acceptForSession",
                PermissionDecision::Deny { .. } => "decline",
                PermissionDecision::Answer(Value::Bool(true)) => "accept",
                PermissionDecision::Answer(_) => "cancel",
            };
            json!({"decision": decision})
        }
    }
}

pub fn start(cfg: SessionConfig) -> Result<SessionHandle> {
    let mut cmd = Command::new(&cfg.binary);
    cmd.arg("app-server").current_dir(&cfg.cwd);
    cmd.args(&cfg.extra_args);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let proc = LineProcess::spawn(cmd)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness: HarnessId::Codex,
        process_model: ProcessModel::LongLived,
    });
    tokio::spawn(drive(proc, cfg, events_tx, cmd_rx));
    Ok(handle)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outstanding {
    Initialize,
    ThreadStart,
    TurnStart,
    Interrupt,
    Other,
}

struct Driver {
    proc: LineProcess,
    parser: CodexAppServerParser,
    next_id: u64,
    outstanding: HashMap<u64, Outstanding>,
    /// Server requests awaiting a decision: our string id → (rpc id, kind).
    pending: HashMap<String, (Value, PermissionKind)>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    model: Option<ModelRef>,
    effort: Option<String>,
    /// A turn requested before the thread was ready.
    queued_turn: Option<String>,
}

impl Driver {
    async fn request(&mut self, method: &str, params: Value, kind: Outstanding) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        self.outstanding.insert(id, kind);
        self.proc
            .write_line(&jsonrpc::request(id, method, params))
            .await?;
        Ok(id)
    }

    async fn start_turn(&mut self, text: String) -> Result<()> {
        let Some(thread_id) = self.thread_id.clone() else {
            self.queued_turn = Some(text);
            return Ok(());
        };
        let mut params = json!({
            "threadId": thread_id,
            "input": [{"type":"text","text": text}],
        });
        if let Some(m) = &self.model {
            params["model"] = json!(m.model);
        }
        if let Some(e) = &self.effort {
            params["effort"] = json!(e);
        }
        self.request("turn/start", params, Outstanding::TurnStart)
            .await?;
        Ok(())
    }
}

async fn drive(
    proc: LineProcess,
    cfg: SessionConfig,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
) {
    let mut d = Driver {
        proc,
        parser: CodexAppServerParser::new(),
        next_id: 0,
        outstanding: HashMap::new(),
        pending: HashMap::new(),
        thread_id: None,
        turn_id: None,
        model: cfg.model.clone(),
        effort: cfg.effort.clone(),
        queued_turn: None,
    };
    let mut shutting_down = false;

    if let Err(e) = d
        .request(
            "initialize",
            json!({"clientInfo": {"name": "unharness", "version": env!("CARGO_PKG_VERSION")}}),
            Outstanding::Initialize,
        )
        .await
    {
        let _ = events.send(AgentEvent::Error(format!("codex: {e}"))).await;
    }

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    d.proc.kill().await;
                    return;
                };
                let res: Result<()> = match cmd {
                    SessionCommand::SendTurn { text } => d.start_turn(text).await,
                    SessionCommand::Interrupt => {
                        match (&d.thread_id, &d.turn_id) {
                            (Some(t), Some(turn)) => {
                                let params = json!({"threadId": t, "turnId": turn});
                                d.request("turn/interrupt", params, Outstanding::Interrupt).await.map(|_| ())
                            }
                            _ => Ok(()),
                        }
                    }
                    SessionCommand::RespondPermission { id, decision } => match d.pending.remove(&id) {
                        Some((rpc_id, kind)) => d
                            .proc
                            .write_line(&jsonrpc::response(&rpc_id, encode_decision(&kind, &decision)))
                            .await,
                        None => {
                            let _ = events.send(AgentEvent::Notice(format!("no pending codex request {id}"))).await;
                            Ok(())
                        }
                    },
                    SessionCommand::SetModel(m) => {
                        d.model = Some(m);
                        Ok(())
                    }
                    SessionCommand::SetEffort(e) => {
                        d.effort = e;
                        Ok(())
                    }
                    SessionCommand::SetPolicy(_) => {
                        let _ = events.send(AgentEvent::Notice(
                            "codex: the permission policy applies when the next session starts".into(),
                        )).await;
                        Ok(())
                    }
                    SessionCommand::Shutdown => {
                        shutting_down = true;
                        d.proc.close_stdin();
                        Ok(())
                    }
                };
                if let Err(e) = res {
                    let _ = events.send(AgentEvent::Error(format!("codex: {e}"))).await;
                }
            }
            raw = d.proc.lines.recv() => {
                match raw {
                    Some(RawLine::Stdout(line)) => {
                        let msg = RpcMessage::parse(&line);
                        match &msg {
                            Some(RpcMessage::Response { id, result, error }) => {
                                let kind = id.as_u64().and_then(|i| d.outstanding.remove(&i));
                                match kind {
                                    Some(Outstanding::Initialize) => {
                                        let _ = d.proc.write_line(&jsonrpc::notification("initialized", Value::Null)).await;
                                        let (approval, sandbox) = policy_params(cfg.policy);
                                        let r = match &cfg.resume {
                                            Some(id) => d.request("thread/resume", json!({"threadId": id}), Outstanding::ThreadStart).await,
                                            None => {
                                                let mut params = json!({
                                                    "cwd": cfg.cwd,
                                                    "approvalPolicy": approval,
                                                    "sandbox": sandbox,
                                                });
                                                if let Some(m) = &cfg.model {
                                                    params["model"] = json!(m.model);
                                                }
                                                d.request("thread/start", params, Outstanding::ThreadStart).await
                                            }
                                        };
                                        if let Err(e) = r {
                                            let _ = events.send(AgentEvent::Error(format!("codex: {e}"))).await;
                                        }
                                    }
                                    Some(Outstanding::ThreadStart) => {
                                        if let Some(err) = error {
                                            let msg = err.get("message").and_then(Value::as_str).unwrap_or("thread start failed");
                                            let _ = events.send(AgentEvent::TurnCompleted { stop_reason: StopReason::Error(format!("codex: {msg}")) }).await;
                                        } else {
                                            d.thread_id = result
                                                .as_ref()
                                                .and_then(|r| r.pointer("/thread/id").or_else(|| r.get("threadId")))
                                                .and_then(Value::as_str)
                                                .map(str::to_string);
                                            if let Some(t) = d.queued_turn.take()
                                                && let Err(e) = d.start_turn(t).await
                                            {
                                                let _ = events.send(AgentEvent::Error(format!("codex: {e}"))).await;
                                            }
                                        }
                                    }
                                    Some(Outstanding::TurnStart) => {
                                        if let Some(err) = error {
                                            let msg = err.get("message").and_then(Value::as_str).unwrap_or("turn start failed");
                                            let _ = events.send(AgentEvent::TurnCompleted { stop_reason: StopReason::Error(format!("codex: {msg}")) }).await;
                                        } else {
                                            d.turn_id = result
                                                .as_ref()
                                                .and_then(|r| r.pointer("/turn/id"))
                                                .and_then(Value::as_str)
                                                .map(str::to_string);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some(RpcMessage::Request { id, method, .. }) => {
                                // Remember the rpc id so the decision can be routed back.
                                let rid = match id {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                let supported = method.ends_with("requestApproval") || method == "item/tool/requestUserInput";
                                if !supported {
                                    let _ = d.proc.write_line(&jsonrpc::error_response(id, -32601, "unsupported by unharness")).await;
                                } else {
                                    d.pending.insert(rid, (id.clone(), PermissionKind::Confirm { title: String::new(), message: None }));
                                }
                            }
                            _ => {}
                        }
                        for ev in d.parser.feed(&line) {
                            if let AgentEvent::PermissionRequest(req) = &ev
                                && let Some((rpc_id, _)) = d.pending.remove(&req.id)
                            {
                                d.pending.insert(req.id.clone(), (rpc_id, req.kind.clone()));
                            }
                            if events.send(ev).await.is_err() {
                                d.proc.kill().await;
                                return;
                            }
                        }
                    }
                    Some(RawLine::Stderr(line)) => {
                        for ev in d.parser.feed_stderr(&line) {
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
        d.proc.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions() {
        let tool = PermissionKind::ToolUse {
            tool: "shell".into(),
            input: Value::Null,
            suggestions: None,
            description: None,
        };
        assert_eq!(
            encode_decision(
                &tool,
                &PermissionDecision::Allow {
                    updated_input: None
                }
            ),
            json!({"decision":"accept"})
        );
        assert_eq!(
            encode_decision(&tool, &PermissionDecision::AllowAlways),
            json!({"decision":"acceptForSession"})
        );
        assert_eq!(
            encode_decision(&tool, &PermissionDecision::Deny { reason: "x".into() }),
            json!({"decision":"decline"})
        );
        let q = PermissionKind::Question { questions: vec![] };
        assert_eq!(
            encode_decision(
                &q,
                &PermissionDecision::Answer(json!({"q1":"dev","q2":["a","b"]}))
            ),
            json!({"answers":{"q1":{"answers":["dev"]},"q2":{"answers":["a","b"]}}})
        );
        assert_eq!(
            policy_params(PermissionPolicy::Bypass),
            ("never", "danger-full-access")
        );
    }
}
