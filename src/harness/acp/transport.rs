//! Long-lived ACP session over stdio: `initialize` → `session/new` (or
//! `session/resume` / `session/load`) → one `session/prompt` per turn.
//!
//! unharness declares no client capabilities, so the agent uses its own file
//! and terminal tools and reports them as tool calls. The only agent → client
//! request served is `session/request_permission`; the rest are refused.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::parse::{AcpParser, SessionOptions, can_load, can_resume, error_text, mcp_http};
use crate::core::jsonrpc::{self, RpcMessage};
use crate::core::mcp;
use crate::core::process::{LineProcess, RawLine};
use crate::core::{
    AgentEvent, Attachment, HarnessId, McpServer, McpTransport, PermissionDecision,
    PermissionPolicy, ProcessModel, RemoteControl, SessionCommand, SessionConfig, SessionHandle,
    SessionInfo, StopReason,
};
use crate::harness::which;

/// `session/prompt` content: the text, then each image inline and each
/// file as a `resource_link` (the block every agent must accept; the agent
/// reads the file itself).
pub fn prompt_blocks(text: &str, attachments: &[Attachment]) -> Result<Value> {
    let mut blocks = vec![json!({"type":"text","text": text})];
    for a in attachments {
        blocks.push(match a {
            Attachment::Image { mime, .. } => {
                json!({"type":"image","data": a.read_base64()?,"mimeType": mime})
            }
            Attachment::File { path, mime } => json!({
                "type": "resource_link",
                "uri": file_uri(path),
                "name": a.label(),
                "mimeType": mime,
            }),
        });
    }
    Ok(Value::Array(blocks))
}

/// `mcpServers` of a session request, and the names of the http servers
/// left out because the agent does not take any (`http`, from its
/// `mcpCapabilities`). Every agent must take stdio servers.
pub fn mcp_servers_param(servers: &[McpServer], http: bool, cwd: &Path) -> (Value, Vec<String>) {
    let pairs = |map: &std::collections::BTreeMap<String, String>| -> Vec<Value> {
        map.iter()
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect()
    };
    let (mut list, mut dropped) = (Vec::new(), Vec::new());
    for s in servers {
        match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                // The protocol asks for an absolute path: a bare name is
                // looked up, a relative path is one in the session's directory.
                let path = Path::new(command);
                let command = if path.is_absolute() {
                    command.clone()
                } else if path.components().count() > 1 {
                    cwd.join(path).to_string_lossy().into_owned()
                } else {
                    which(command)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| command.clone())
                };
                list.push(json!({
                    "name": s.name, "command": command, "args": args, "env": pairs(env),
                }));
            }
            McpTransport::Http { url, headers } if http => list.push(json!({
                "type": "http", "name": s.name, "url": url, "headers": pairs(headers),
            })),
            McpTransport::Http { .. } => dropped.push(s.name.clone()),
        }
    }
    (Value::Array(list), dropped)
}

/// `file://` URI of an absolute path.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for b in path.as_os_str().as_encoded_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                uri.push(*b as char)
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

/// Map a decision onto the options the agent offered. ACP has no free-form
/// denial, so a deny reason is not passed on. Allowing never picks the
/// agent's `allow_always`: what is allowed from now on is unharness's to
/// remember (allow rules), and an agent that offers nothing else is not
/// answered for.
pub fn permission_outcome(options: &Value, decision: &PermissionDecision) -> Value {
    let wanted: &[&str] = match decision {
        PermissionDecision::Allow { .. } => &["allow_once"],
        PermissionDecision::Deny { .. } => &["reject_once", "reject_always"],
        PermissionDecision::Answer(_) => &[],
    };
    let pick = wanted.iter().find_map(|kind| {
        options
            .as_array()?
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
    });
    match pick.and_then(|o| o.get("optionId")) {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// Whether `policy` answers a request for a tool of this `kind` without asking.
/// The policy is applied here, on the client, so it means the same for every
/// ACP agent regardless of the modes it offers.
pub fn auto_allows(policy: PermissionPolicy, kind: &str) -> bool {
    match policy {
        // Not declared for ACP agents; never sent one.
        PermissionPolicy::Plan | PermissionPolicy::Ask => false,
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => kind == "edit",
        PermissionPolicy::Bypass => true,
    }
}

pub fn start(harness: HarnessId, args: Vec<String>, cfg: SessionConfig) -> Result<SessionHandle> {
    let mut command = Command::new(&cfg.binary);
    command
        .args(&args)
        .args(&cfg.extra_args)
        .current_dir(&cfg.cwd);
    // An agent started from inside another agent's session may behave differently.
    for (k, _) in std::env::vars() {
        if k.starts_with("CLAUDECODE") || k == "CLAUDE_CODE_ENTRYPOINT" {
            command.env_remove(k);
        }
    }
    for (k, v) in &cfg.env {
        command.env(k, v);
    }
    let proc = LineProcess::spawn(command, &cfg.sandbox)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness,
        process_model: ProcessModel::LongLived,
    });
    handle.process_slot().set(&proc);
    tokio::spawn(drive(harness, proc, cfg, events_tx, cmd_rx));
    Ok(handle)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outstanding {
    Initialize,
    /// `session/new`, or a reattach to an existing session.
    Session {
        reattach: bool,
    },
    Prompt,
    /// Config changes: only an error matters, and the parser reports it.
    Other,
}

struct Driver {
    harness: HarnessId,
    proc: LineProcess,
    parser: AcpParser,
    cfg: SessionConfig,
    next_id: u64,
    outstanding: HashMap<u64, Outstanding>,
    session_id: Option<String>,
    /// `mcpServers` for every session request, once the agent has said
    /// which kinds it takes.
    mcp_servers: Value,
    options: SessionOptions,
    policy: PermissionPolicy,
    /// Permission requests awaiting the user: our id → (rpc id, offered options).
    pending: HashMap<String, (Value, Value)>,
    /// A turn sent before the session was ready.
    queued_turn: Option<(String, Vec<Attachment>)>,
    last_stderr: Option<String>,
}

impl Driver {
    async fn request(&mut self, method: &str, params: Value, kind: Outstanding) -> Result<()> {
        self.next_id += 1;
        self.outstanding.insert(self.next_id, kind);
        self.proc
            .write_line(&jsonrpc::request(self.next_id, method, params))
            .await
    }

    async fn new_session(&mut self) -> Result<()> {
        let params = json!({"cwd": self.cfg.cwd, "mcpServers": self.mcp_servers});
        self.request(
            "session/new",
            params,
            Outstanding::Session { reattach: false },
        )
        .await
    }

    async fn send_turn(&mut self, text: String, attachments: Vec<Attachment>) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            self.queued_turn = Some((text, attachments));
            return Ok(());
        };
        let prompt = prompt_blocks(&text, &attachments)?;
        self.request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": prompt}),
            Outstanding::Prompt,
        )
        .await
    }

    async fn set_option(&mut self, option: Option<String>, value: &str) -> Result<bool> {
        let (Some(session_id), Some(option)) = (self.session_id.clone(), option) else {
            return Ok(false);
        };
        self.request(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": option, "value": value}),
            Outstanding::Other,
        )
        .await?;
        Ok(true)
    }

    /// The session exists: apply the configured model and effort, then send
    /// the turn that was waiting for it.
    async fn session_ready(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        if let Some(model) = self.cfg.model.clone()
            && self.options.current_model.as_deref() != Some(&model.model)
        {
            self.set_option(self.options.model_option.clone(), &model.model)
                .await?;
        }
        if let Some(effort) = self.cfg.effort.clone() {
            self.set_option(self.options.effort_option.clone(), &effort)
                .await?;
        }
        if let Some((text, attachments)) = self.queued_turn.take() {
            let _ = events.send(AgentEvent::TurnStarted).await;
            self.send_turn(text, attachments).await?;
        }
        Ok(())
    }
}

async fn drive(
    harness: HarnessId,
    proc: LineProcess,
    cfg: SessionConfig,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
) {
    let mut d = Driver {
        harness,
        proc,
        parser: AcpParser::new(harness),
        policy: cfg.policy,
        cfg,
        next_id: 0,
        outstanding: HashMap::new(),
        session_id: None,
        mcp_servers: json!([]),
        options: SessionOptions::default(),
        pending: HashMap::new(),
        queued_turn: None,
        last_stderr: None,
    };
    let init = json!({
        "protocolVersion": 1,
        "clientCapabilities": {},
        "clientInfo": {"name": "unharness", "title": "unharness", "version": env!("CARGO_PKG_VERSION")},
    });
    if let Err(e) = d.request("initialize", init, Outstanding::Initialize).await {
        let _ = events
            .send(AgentEvent::Error(format!("{harness}: {e}")))
            .await;
    }
    let mut shutting_down = false;

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    if !shutting_down {
                        d.proc.kill().await;
                    }
                    return;
                };
                let res: Result<()> = match cmd {
                    SessionCommand::SendTurn { text, attachments } => {
                        if d.session_id.is_some() {
                            let _ = events.send(AgentEvent::TurnStarted).await;
                        }
                        match d.send_turn(text, attachments).await {
                            Ok(()) => Ok(()),
                            Err(e) => {
                                let _ = events.send(AgentEvent::TurnCompleted {
                                    stop_reason: StopReason::Error(e.to_string()),
                                }).await;
                                Ok(())
                            }
                        }
                    }
                    // No subagents are reported here, so none can be running.
                    SessionCommand::StopSubagent { .. } => Ok(()),
                    SessionCommand::RemoteControl { .. } => {
                        let _ = events.send(AgentEvent::RemoteControl(RemoteControl::Failed {
                            reason: "this agent has no remote control".into(),
                        })).await;
                        Ok(())
                    }
                    SessionCommand::Steer { .. } | SessionCommand::Rewind { .. } => {
                        let _ = events.send(AgentEvent::Error(
                            "this agent cannot be steered or rewound".into(),
                        )).await;
                        Ok(())
                    }
                    SessionCommand::Compact { .. } => {
                        let _ = events.send(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Error("this agent cannot compact on request".into()),
                        }).await;
                        Ok(())
                    }
                    SessionCommand::Interrupt => match d.session_id.clone() {
                        Some(session_id) => {
                            // The spec requires every open permission request to be
                            // answered `cancelled` once the turn is cancelled.
                            let mut res = d
                                .proc
                                .write_line(&jsonrpc::notification("session/cancel", json!({"sessionId": session_id})))
                                .await;
                            for (_, (rpc_id, _)) in d.pending.drain() {
                                let line = jsonrpc::response(&rpc_id, json!({"outcome": {"outcome": "cancelled"}}));
                                res = res.and(d.proc.write_line(&line).await);
                            }
                            res
                        }
                        // Waiting for the session: it is never sent.
                        None => {
                            if d.queued_turn.take().is_some() {
                                let _ = events.send(AgentEvent::TurnCompleted {
                                    stop_reason: StopReason::Interrupted,
                                }).await;
                            }
                            Ok(())
                        }
                    },
                    SessionCommand::RespondPermission { id, decision } => match d.pending.remove(&id) {
                        Some((rpc_id, options)) => {
                            let outcome = permission_outcome(&options, &decision);
                            if matches!(decision, PermissionDecision::Allow { .. })
                                && outcome.pointer("/outcome/outcome") == Some(&json!("cancelled"))
                            {
                                let _ = events.send(AgentEvent::Notice(
                                    "the agent offered no way to allow this just once; the request was cancelled".into(),
                                )).await;
                            }
                            d.proc.write_line(&jsonrpc::response(&rpc_id, outcome)).await
                        }
                        None => {
                            let _ = events.send(AgentEvent::Notice(format!("no pending permission request {id}"))).await;
                            Ok(())
                        }
                    },
                    SessionCommand::SetModel(m) => {
                        d.cfg.model = Some(m.clone());
                        match d.set_option(d.options.model_option.clone(), &m.model).await {
                            Ok(false) if d.session_id.is_some() => {
                                let _ = events.send(AgentEvent::Notice(
                                    "this agent does not offer a model choice".into(),
                                )).await;
                                Ok(())
                            }
                            other => other.map(|_| ()),
                        }
                    }
                    SessionCommand::SetEffort(e) => {
                        d.cfg.effort = e.clone();
                        match e {
                            Some(e) => d.set_option(d.options.effort_option.clone(), &e).await.map(|_| ()),
                            None => Ok(()),
                        }
                    }
                    SessionCommand::SetPolicy(p) => {
                        d.policy = p;
                        Ok(())
                    }
                    SessionCommand::Shutdown => {
                        shutting_down = true;
                        let turn_open = d.queued_turn.is_some()
                            || d.outstanding.values().any(|o| *o == Outstanding::Prompt);
                        d.proc.end_or_kill(turn_open).await;
                        Ok(())
                    }
                };
                if let Err(e) = res {
                    let _ = events.send(AgentEvent::Error(format!("{harness}: {e}"))).await;
                }
            }
            raw = d.proc.lines.recv() => {
                match raw {
                    Some(RawLine::Stdout(line)) => {
                        if let Err(e) = on_line(&mut d, &line, &events).await {
                            let _ = events.send(AgentEvent::Error(format!("{harness}: {e}"))).await;
                        }
                    }
                    // Agents log freely to stderr; keep the last line for a crash report.
                    Some(RawLine::Stderr(line)) => {
                        if !line.trim().is_empty() {
                            d.last_stderr = Some(line.trim().to_string());
                        }
                    }
                    Some(RawLine::Exited(code)) => {
                        if !shutting_down && code != Some(0)
                            && let Some(err) = d.last_stderr.take()
                        {
                            let _ = events.send(AgentEvent::Error(err)).await;
                        }
                        let _ = events.send(AgentEvent::ProcessExited { code }).await;
                        return;
                    }
                    None => return,
                }
            }
        }
    }
}

/// Handle one stdout line: driver bookkeeping first, then the parser's events.
async fn on_line(d: &mut Driver, line: &str, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
    let msg = RpcMessage::parse(line);
    match &msg {
        Some(RpcMessage::Response { id, result, error }) => {
            let kind = id.as_u64().and_then(|i| d.outstanding.remove(&i));
            let result = result.clone().unwrap_or(Value::Null);
            match (kind, error) {
                (Some(Outstanding::Initialize), None) => {
                    let caps = result.get("agentCapabilities").unwrap_or(&Value::Null);
                    let (servers, dropped) =
                        mcp_servers_param(&d.cfg.mcp_servers, mcp_http(caps), &d.cfg.cwd);
                    d.mcp_servers = servers;
                    if !dropped.is_empty() {
                        let warning = mcp::not_http(d.harness.as_str(), &dropped.join(", "));
                        let _ = events.send(AgentEvent::Notice(warning)).await;
                    }
                    match d.cfg.resume.clone() {
                        Some(id) if can_resume(caps) || can_load(caps) => {
                            // `resume` reattaches silently; `load` replays the
                            // history, which the transcript already has.
                            let method = if can_resume(caps) {
                                "session/resume"
                            } else {
                                d.parser.set_replaying(true);
                                "session/load"
                            };
                            let params = json!({
                                "sessionId": id, "cwd": d.cfg.cwd, "mcpServers": d.mcp_servers,
                            });
                            d.request(method, params, Outstanding::Session { reattach: true })
                                .await?;
                        }
                        Some(_) => {
                            let _ = events
                                .send(AgentEvent::Notice(
                                    "this agent cannot resume sessions; starting a new one".into(),
                                ))
                                .await;
                            d.new_session().await?;
                        }
                        None => d.new_session().await?,
                    }
                }
                (Some(Outstanding::Session { reattach }), None) => {
                    d.parser.set_replaying(false);
                    if reattach {
                        d.parser.mark_reattached();
                    }
                    d.options = SessionOptions::from_config_options(result.get("configOptions"))
                        .unwrap_or_default();
                    d.session_id = result
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if reattach && d.session_id.is_none() {
                        // The spec's reattach result does not repeat the
                        // session id (some agents add it): announce it here.
                        let session_id = d.cfg.resume.clone().unwrap_or_default();
                        d.session_id = Some(session_id.clone());
                        let _ = events
                            .send(AgentEvent::SessionStarted {
                                session_id,
                                model: d.options.current_model.clone(),
                            })
                            .await;
                        let _ = events
                            .send(AgentEvent::CapabilitiesChanged(
                                d.options.caps_update(d.harness),
                            ))
                            .await;
                    }
                    forward(d, line, events).await;
                    return d.session_ready(events).await;
                }
                (Some(Outstanding::Session { reattach: true }), Some(e)) => {
                    d.parser.set_replaying(false);
                    let _ = events
                        .send(AgentEvent::Notice(format!(
                            "could not resume the session ({}); starting a new one",
                            error_text(e)
                        )))
                        .await;
                    return d.new_session().await;
                }
                // Without a session nothing can run: fail the waiting turn.
                (Some(Outstanding::Initialize | Outstanding::Session { .. }), Some(e)) => {
                    d.queued_turn = None;
                    let _ = events
                        .send(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Error(format!(
                                "{} (if this is an authentication error, log in with the agent's own CLI first)",
                                error_text(e)
                            )),
                        })
                        .await;
                    return Ok(());
                }
                (Some(Outstanding::Prompt), Some(e)) => {
                    let _ = events
                        .send(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Error(error_text(e)),
                        })
                        .await;
                    return Ok(());
                }
                // A config change answers with the option list as it now stands.
                (Some(Outstanding::Other), None) => {
                    if let Some(o) =
                        SessionOptions::from_config_options(result.get("configOptions"))
                    {
                        d.options = o;
                    }
                }
                _ => {}
            }
        }
        Some(RpcMessage::Notification { method, params }) if method == "session/update" => {
            // Keep the option ids current so model and effort changes go to the right one.
            if params
                .pointer("/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("config_option_update")
                && let Some(o) =
                    SessionOptions::from_config_options(params.pointer("/update/configOptions"))
            {
                d.options = o;
            }
        }
        Some(RpcMessage::Request { id, method, params }) => {
            if method != "session/request_permission" {
                return d
                    .proc
                    .write_line(&jsonrpc::error_response(
                        id,
                        -32601,
                        "unsupported by unharness",
                    ))
                    .await;
            }
            let options = params.get("options").cloned().unwrap_or(Value::Null);
            let kind = params
                .pointer("/toolCall/kind")
                .and_then(Value::as_str)
                .unwrap_or("");
            if auto_allows(d.policy, kind) {
                let allow = PermissionDecision::Allow {
                    updated_input: None,
                };
                d.proc
                    .write_line(&jsonrpc::response(id, permission_outcome(&options, &allow)))
                    .await?;
                // Still announce the tool call, just not the question.
                for ev in d.parser.feed(line) {
                    if !matches!(ev, AgentEvent::PermissionRequest(_)) {
                        let _ = events.send(ev).await;
                    }
                }
                return Ok(());
            }
            let rid = match id {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            d.pending.insert(rid, (id.clone(), options));
        }
        _ => {}
    }
    forward(d, line, events).await;
    Ok(())
}

async fn forward(d: &mut Driver, line: &str, events: &mpsc::Sender<AgentEvent>) {
    for ev in d.parser.feed(line) {
        let _ = events.send(ev).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Value {
        json!([
            {"optionId":"allow-once","name":"Yes","kind":"allow_once"},
            {"optionId":"allow-all","name":"Always","kind":"allow_always"},
            {"optionId":"reject","name":"No","kind":"reject_once"}
        ])
    }

    #[test]
    fn decisions_map_onto_offered_options() {
        let pick = |d: PermissionDecision| permission_outcome(&options(), &d);
        assert_eq!(
            pick(PermissionDecision::Allow {
                updated_input: None
            }),
            json!({"outcome":{"outcome":"selected","optionId":"allow-once"}})
        );
        assert_eq!(
            pick(PermissionDecision::Deny {
                reason: "no".into()
            })["outcome"]["optionId"],
            "reject"
        );
        assert_eq!(
            pick(PermissionDecision::Answer(Value::Null)),
            json!({"outcome":{"outcome":"cancelled"}})
        );
        // An agent that offers no way to allow once gets a cancel, never a
        // guess, and never its own "always".
        let only_reject = json!([
            {"optionId":"r","kind":"reject_once"},
            {"optionId":"a","kind":"allow_always"}
        ]);
        assert_eq!(
            permission_outcome(
                &only_reject,
                &PermissionDecision::Allow {
                    updated_input: None
                }
            ),
            json!({"outcome":{"outcome":"cancelled"}})
        );
    }

    #[test]
    fn policy_is_applied_client_side() {
        use PermissionPolicy::*;
        assert!(!auto_allows(Ask, "edit"));
        assert!(auto_allows(AcceptEdits, "edit"));
        assert!(!auto_allows(AcceptEdits, "execute"));
        assert!(auto_allows(Bypass, "execute"));
    }

    #[test]
    fn prompt_blocks_inline_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        std::fs::write(&path, b"abc").unwrap();
        let a = Attachment::image(&path).unwrap();
        assert_eq!(
            prompt_blocks("hi", &[a]).unwrap(),
            json!([{"type":"text","text":"hi"},{"type":"image","data":"YWJj","mimeType":"image/png"}])
        );
    }

    #[test]
    fn prompt_blocks_link_files() {
        // Shape confirmed by fixtures/claude_agent_acp_file.jsonl and
        // fixtures/codex_acp_file.jsonl: both agents read the linked file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my notes.txt");
        std::fs::write(&path, b"abc").unwrap();
        let a = Attachment::file(&path).unwrap();
        let uri = format!("file://{}/my%20notes.txt", dir.path().display());
        assert_eq!(
            prompt_blocks("hi", &[a]).unwrap(),
            json!([
                {"type":"text","text":"hi"},
                {"type":"resource_link","uri":uri,"name":"my notes.txt","mimeType":"text/plain"}
            ])
        );
    }

    #[test]
    fn mcp_servers_in_the_session_request() {
        let servers = crate::core::testing::sample_mcp_servers();
        let files = json!({
            "name": "files",
            "command": "/usr/bin/files-mcp",
            "args": ["--root", "/my work"],
            "env": [{"name": "TOKEN", "value": "t\"1"}],
        });
        let cwd = Path::new("/work");
        let (param, dropped) = mcp_servers_param(&servers, true, cwd);
        assert!(dropped.is_empty());
        assert_eq!(param[0], files);
        assert_eq!(
            param[1],
            json!({
                "type": "http",
                "name": "docs",
                "url": "https://example.com/mcp",
                "headers": [{"name": "Authorization", "value": "Bearer x"}],
            })
        );

        // An agent without the http capability gets the stdio ones only.
        let (param, dropped) = mcp_servers_param(&servers, false, cwd);
        assert_eq!(param, json!([files]));
        assert_eq!(dropped, ["docs"]);

        // The protocol wants a path: a bare command is looked up, a
        // relative one is in the session's directory.
        let with_command = |c: &str| {
            let mut server = servers[0].clone();
            if let McpTransport::Stdio { command, .. } = &mut server.transport {
                *command = c.into();
            }
            mcp_servers_param(&[server], false, cwd).0[0]["command"].clone()
        };
        assert!(with_command("sh").as_str().unwrap().ends_with("/sh"));
        assert_eq!(with_command("./tools/server"), "/work/./tools/server");
    }
}
