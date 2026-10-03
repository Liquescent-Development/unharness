//! Long-lived Codex session over `codex app-server` (JSON-RPC 2.0 on stdio).

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::app_server_parse::CodexAppServerParser;
use super::app_server_parse::ELICITATION_PREFIX;
use crate::core::jsonrpc::{self, RpcMessage};
use crate::core::process::{LineProcess, RawLine};
use crate::core::{
    AgentEvent, Attachment, HarnessId, ModelRef, PermissionDecision, PermissionKind,
    PermissionPolicy, ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo,
    StopReason,
};

/// `turn/start` input items: the text, then each image by path (codex reads the file).
pub fn turn_input(text: &str, attachments: &[Attachment]) -> Value {
    let mut items = vec![json!({"type":"text","text": text})];
    for a in attachments {
        items.push(json!({"type":"localImage","path": a.path()}));
    }
    Value::Array(items)
}

/// `turn/start` sandbox policy object for a policy.
pub fn sandbox_policy(policy: PermissionPolicy) -> Value {
    match policy {
        PermissionPolicy::Ask => json!({"type": "readOnly"}),
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => json!({"type": "workspaceWrite"}),
        PermissionPolicy::Bypass => json!({"type": "dangerFullAccess"}),
    }
}

/// `thread/start` parameters for a policy.
pub fn policy_params(policy: PermissionPolicy) -> (&'static str, &'static str) {
    match policy {
        PermissionPolicy::Ask => ("untrusted", "read-only"),
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => ("on-request", "workspace-write"),
        PermissionPolicy::Bypass => ("never", "danger-full-access"),
    }
}

pub fn encode_decision(kind: &PermissionKind, decision: &PermissionDecision) -> Value {
    if let PermissionKind::Input { title, .. } = kind
        && title.starts_with(ELICITATION_PREFIX)
    {
        // MCP elicitation: {action, content}
        return match decision {
            PermissionDecision::Answer(Value::String(text)) => {
                json!({"action": "accept", "content": {"value": text}})
            }
            PermissionDecision::Answer(Value::Bool(b)) => {
                json!({"action": "accept", "content": {"value": b}})
            }
            PermissionDecision::Allow { .. } | PermissionDecision::AllowAlways => {
                json!({"action": "accept", "content": {}})
            }
            PermissionDecision::Deny { .. } => json!({"action": "decline"}),
            PermissionDecision::Answer(_) => json!({"action": "cancel"}),
        };
    }
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
        harness: HarnessId::CODEX,
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
    /// Steer, compact: only an error response matters, and the parser reports it.
    Other,
    Revert,
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
    policy: PermissionPolicy,
    /// A turn requested before the thread was ready.
    queued_turn: Option<(String, Vec<Attachment>)>,
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

    async fn start_turn(&mut self, text: String, attachments: Vec<Attachment>) -> Result<()> {
        let Some(thread_id) = self.thread_id.clone() else {
            self.queued_turn = Some((text, attachments));
            return Ok(());
        };
        let mut params = json!({
            "threadId": thread_id,
            "input": turn_input(&text, &attachments),
        });
        if let Some(m) = &self.model {
            params["model"] = json!(m.model);
        }
        if let Some(e) = &self.effort {
            params["effort"] = json!(e);
        }
        let (approval, _) = policy_params(self.policy);
        params["approvalPolicy"] = json!(approval);
        params["sandboxPolicy"] = sandbox_policy(self.policy);
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
        policy: cfg.policy,
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
                    SessionCommand::SendTurn { text, attachments } => d.start_turn(text, attachments).await,
                    SessionCommand::Steer { text, attachments } => {
                        match (d.thread_id.clone(), d.turn_id.clone()) {
                            (Some(t), Some(turn)) => {
                                let params = json!({
                                    "threadId": t,
                                    "expectedTurnId": turn,
                                    "input": turn_input(&text, &attachments),
                                });
                                d.request("turn/steer", params, Outstanding::Other).await.map(|_| ())
                            }
                            // Nothing is running: it is simply the next turn.
                            _ => d.start_turn(text, attachments).await,
                        }
                    }
                    SessionCommand::Rewind { anchor } => match d.thread_id.clone() {
                        Some(t) => d
                            .request("thread/revert", json!({"threadId": t, "beforeTurnId": anchor}), Outstanding::Revert)
                            .await
                            .map(|_| ()),
                        None => Ok(()),
                    },
                    SessionCommand::Compact { .. } => match d.thread_id.clone() {
                        // Runs as a turn of its own (turn/started … turn/completed).
                        Some(t) => d
                            .request("thread/compact/start", json!({"threadId": t}), Outstanding::Other)
                            .await
                            .map(|_| ()),
                        None => {
                            let _ = events.send(AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Error("codex: no thread to compact yet".into()),
                            }).await;
                            Ok(())
                        }
                    },
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
                    SessionCommand::SetPolicy(p) => {
                        // Applied on the next turn/start (approvalPolicy + sandboxPolicy).
                        d.policy = p;
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
                                            // A fork answers like a start: with the new thread.
                                            Some(id) => {
                                                // (`excludeTurns`: codex deprecates returning the whole history here.)
                                                if cfg.fork {
                                                    d.request("thread/fork", json!({"threadId": id, "excludeTurns": true}), Outstanding::ThreadStart).await
                                                } else {
                                                    d.request("thread/resume", json!({"threadId": id}), Outstanding::ThreadStart).await
                                                }
                                            }
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
                                            // No turn id to rewind to: keep the anchor sequence aligned.
                                            let _ = events.send(AgentEvent::TurnAnchor { id: String::new() }).await;
                                            let _ = events.send(AgentEvent::TurnCompleted { stop_reason: StopReason::Error(format!("codex: {msg}")) }).await;
                                        } else {
                                            d.thread_id = result
                                                .as_ref()
                                                .and_then(|r| r.pointer("/thread/id").or_else(|| r.get("threadId")))
                                                .and_then(Value::as_str)
                                                .map(str::to_string);
                                            if let Some((t, a)) = d.queued_turn.take()
                                                && let Err(e) = d.start_turn(t, a).await
                                            {
                                                let _ = events.send(AgentEvent::Error(format!("codex: {e}"))).await;
                                            }
                                        }
                                    }
                                    Some(Outstanding::Revert) => {
                                        if let Some(err) = error {
                                            let reason = err.get("message").and_then(Value::as_str).unwrap_or("revert failed").to_string();
                                            let _ = events.send(AgentEvent::RewindFailed { reason }).await;
                                        }
                                    }
                                    Some(Outstanding::TurnStart) => {
                                        if let Some(err) = error {
                                            let msg = err.get("message").and_then(Value::as_str).unwrap_or("turn start failed");
                                            // No turn id to rewind to: keep the anchor sequence aligned.
                                            let _ = events.send(AgentEvent::TurnAnchor { id: String::new() }).await;
                                            let _ = events.send(AgentEvent::TurnCompleted { stop_reason: StopReason::Error(format!("codex: {msg}")) }).await;
                                        } else {
                                            d.turn_id = result
                                                .as_ref()
                                                .and_then(|r| r.pointer("/turn/id"))
                                                .and_then(Value::as_str)
                                                .map(str::to_string);
                                            // `thread/revert` takes this id to drop the turn later.
                                            if let Some(id) = d.turn_id.clone() {
                                                let _ = events.send(AgentEvent::TurnAnchor { id }).await;
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            // Track the running turn: steer and interrupt need its id.
                            // Sub-agent threads share the stream; only the main thread's turn counts.
                            Some(RpcMessage::Notification { method, params })
                                if params.get("threadId").and_then(Value::as_str) == d.thread_id.as_deref() =>
                            {
                                match method.as_str() {
                                    "turn/started" => {
                                        d.turn_id = params.pointer("/turn/id").and_then(Value::as_str).map(str::to_string);
                                    }
                                    "turn/completed" => d.turn_id = None,
                                    _ => {}
                                }
                            }
                            Some(RpcMessage::Request { id, method, .. }) => {
                                // Remember the rpc id so the decision can be routed back.
                                let rid = match id {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                let supported = method.ends_with("requestApproval")
                                    || method == "item/tool/requestUserInput"
                                    || method == "mcpServer/elicitation/request";
                                if !supported {
                                    let _ = d.proc.write_line(&jsonrpc::error_response(id, -32601, "unsupported by unharness")).await;
                                } else {
                                    d.pending.insert(rid, (id.clone(), PermissionKind::Confirm { title: String::new(), message: None }));
                                }
                            }
                            _ => {}
                        }
                        for ev in d.parser.feed(&line) {
                            // A sub-agent's request arrives wrapped in `Sub`.
                            let mut inner = &ev;
                            while let AgentEvent::Sub { event, .. } = inner {
                                inner = event;
                            }
                            if let AgentEvent::PermissionRequest(req) = inner
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
    fn turn_input_adds_local_images() {
        // Shape confirmed by fixtures/app_server_image_turn.jsonl.
        let a = Attachment::image("/w/a.png").unwrap();
        assert_eq!(
            turn_input("hi", &[a]),
            json!([{"type":"text","text":"hi"},{"type":"localImage","path":"/w/a.png"}])
        );
    }

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
