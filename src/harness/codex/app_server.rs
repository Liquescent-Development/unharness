//! Long-lived Codex session over `codex app-server` (JSON-RPC 2.0 on stdio).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;

use super::OwnSandbox;
use super::app_server_parse::CodexAppServerParser;
use super::app_server_parse::ELICITATION_PREFIX;
use crate::core::driver::{self, Answer, Encoded, Gone, Io, Shutdown, StreamProtocol};
use crate::core::jsonrpc::{self, RequestId, RpcMessage};
use crate::core::sandbox::SandboxLevel;
use crate::core::{
    AgentEvent, Attachment, Capabilities, CapsUpdate, HarnessCommand, HarnessId, ModelRef, Parser,
    PermissionDecision, PermissionKind, PermissionPolicy, SessionCommand, SessionConfig,
    SessionHandle, StopReason,
};

/// `turn/start` input items: the text, then each image by path (codex reads the file).
pub fn turn_input(text: &str, attachments: &[Attachment]) -> Value {
    let mut items = vec![json!({"type":"text","text": text})];
    for a in attachments.iter().filter(|a| a.is_image()) {
        items.push(json!({"type":"localImage","path": a.path()}));
    }
    Value::Array(items)
}

/// The id of a `skills/list` request. A string, so that the numbered
/// requests keep the ids they had before it was sent.
const SKILLS_ID: &str = "skills";

/// The enabled skills in an answer to `skills/list` (0.157.0: `data[]`,
/// one entry per `cwd`, each with `skills[]` of `name`, `description`,
/// `path`, `enabled`), with the path a turn names each by. The first of a
/// name wins.
pub fn listed_skills(answer: &Value) -> Vec<(HarnessCommand, String)> {
    let mut skills: Vec<(HarnessCommand, String)> = Vec::new();
    let listed = answer
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("skills")?.as_array())
        .flatten();
    for skill in listed {
        if skill.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let description = skill
            .pointer("/interface/shortDescription")
            .or_else(|| skill.get("shortDescription"))
            .and_then(Value::as_str)
            .filter(|d| !d.trim().is_empty())
            .or_else(|| skill.get("description").and_then(Value::as_str));
        let (Some(name), Some(path)) = (
            skill.get("name").and_then(Value::as_str),
            skill.get("path").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Some(command) = HarnessCommand::new(name, description, None) else {
            continue;
        };
        if !skills.iter().any(|(c, _)| c.name == command.name) {
            skills.push((command, path.to_string()));
        }
    }
    skills
}

/// A prompt that starts with `/name` for one of `skills`: the text Codex
/// is sent, with `$name` (how its own interface names a skill), and the
/// skill's name and path for the `skill` input item.
pub fn skill_call<'a>(
    text: &str,
    skills: &'a [(HarnessCommand, String)],
) -> Option<(String, &'a str, &'a str)> {
    let rest = text.strip_prefix('/')?;
    let name = rest.split_whitespace().next()?;
    let (command, path) = skills.iter().find(|(c, _)| c.name == name)?;
    Some((format!("${rest}"), command.name.as_str(), path.as_str()))
}

/// `turn/start` sandbox policy object.
///
/// Inside unharness's sandbox Codex is told the sandbox is someone else's:
/// its own (bubblewrap) cannot start there. Otherwise Codex's own holds to
/// the level the user set. The policy has no say: what is asked about is
/// `approvalPolicy` alone.
pub fn sandbox_policy(own: OwnSandbox) -> Value {
    match own {
        OwnSandbox::External => json!({"type": "externalSandbox", "networkAccess": "enabled"}),
        OwnSandbox::Level(SandboxLevel::ReadOnly) => json!({"type": "readOnly"}),
        OwnSandbox::Level(SandboxLevel::WorkspaceWrite) => json!({"type": "workspaceWrite"}),
        OwnSandbox::Level(SandboxLevel::Off) => json!({"type": "dangerFullAccess"}),
    }
}

/// `thread/start` parameters: the approval policy for a permission policy,
/// and Codex's own sandbox mode.
pub fn policy_params(policy: PermissionPolicy, own: OwnSandbox) -> (&'static str, &'static str) {
    let approval = match policy {
        // Checked on 0.157.0: with `workspace-write`, `untrusted` asks before
        // every command and file change.
        // Not declared for Codex yet; never sent one.
        PermissionPolicy::Plan | PermissionPolicy::Ask => "untrusted",
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => "on-request",
        PermissionPolicy::Bypass => "never",
    };
    (approval, own.mode())
}

/// The answer to an MCP tool-call approval, which Codex asks for as an
/// elicitation with nothing to fill in.
pub fn encode_mcp_approval(decision: &PermissionDecision) -> Value {
    match decision {
        PermissionDecision::Allow { .. } => {
            json!({"action": "accept", "content": {}})
        }
        PermissionDecision::Deny { .. } => json!({"action": "decline"}),
        PermissionDecision::Answer(_) => json!({"action": "cancel"}),
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
            PermissionDecision::Allow { .. } => {
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
                PermissionDecision::Deny { .. } => "decline",
                PermissionDecision::Answer(Value::Bool(true)) => "accept",
                PermissionDecision::Answer(_) => "cancel",
            };
            json!({"decision": decision})
        }
    }
}

/// `caps` is what the harness declares for app-server.
pub fn start(cfg: SessionConfig, caps: &Capabilities) -> Result<SessionHandle> {
    let mut cmd = Command::new(&cfg.binary);
    cmd.arg("app-server").current_dir(&cfg.cwd);
    if cfg.sandbox.is_active() {
        // Without this the server probes bubblewrap at startup and warns
        // that it cannot create user namespaces.
        cmd.args(["-c", "sandbox_mode=\"danger-full-access\""]);
    }
    if let Some(p) = &cfg.provider {
        cmd.arg("-c").arg(super::provider_override(p));
    }
    let mcp = super::mcp_args(&cfg.mcp_servers);
    for o in &mcp.overrides {
        cmd.arg("-c").arg(o);
    }
    cmd.envs(mcp.env);
    cmd.args(&cfg.extra_args);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let sandbox = cfg.sandbox.clone();
    driver::start(HarnessId::CODEX, caps, cmd, &sandbox, AppServer::new(cfg))
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

/// What the driver does with Codex's app-server protocol. Its requests go
/// through the loop ([`Io::request`]), which numbers them and brings their
/// answers to [`StreamProtocol::on_answer`]; what each was for is kept
/// here, in `outstanding`.
struct AppServer {
    cfg: SessionConfig,
    parser: CodexAppServerParser,
    outstanding: HashMap<u64, Outstanding>,
    /// Server requests awaiting a decision: our string id → (rpc id, kind).
    pending: HashMap<String, (Value, PermissionKind)>,
    /// The pending requests that are elicitations (by our string id).
    elicitations: HashSet<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    /// Sub-agent thread id → its running turn.
    child_turns: HashMap<String, String>,
    model: Option<ModelRef>,
    effort: Option<String>,
    policy: PermissionPolicy,
    /// What Codex's own sandbox holds to.
    own_sandbox: OwnSandbox,
    /// A turn requested before the thread was ready.
    queued_turn: Option<(String, Vec<Attachment>)>,
    /// Interrupt the turn asked for as soon as it has an id.
    interrupt_wanted: bool,
    /// The skills the last `skills/list` answered with.
    skills: Vec<(HarnessCommand, String)>,
    /// A `skills/list` is unanswered: a `/` prompt waits for it, since it
    /// may name one.
    skills_pending: bool,
}

/// A request that could not be written is told as an error.
async fn report<T>(io: &Io<'_>, res: Result<T>) {
    if let Err(e) = res {
        io.emit(AgentEvent::Error(format!("codex: {e}"))).await;
    }
}

impl AppServer {
    fn new(cfg: SessionConfig) -> Self {
        AppServer {
            parser: CodexAppServerParser::new(),
            outstanding: HashMap::new(),
            pending: HashMap::new(),
            elicitations: HashSet::new(),
            thread_id: None,
            turn_id: None,
            child_turns: HashMap::new(),
            model: cfg.model.clone(),
            effort: cfg.effort.clone(),
            policy: cfg.policy,
            own_sandbox: OwnSandbox::for_session(&cfg.sandbox),
            queued_turn: None,
            interrupt_wanted: false,
            skills: Vec::new(),
            skills_pending: false,
            cfg,
        }
    }

    async fn request(
        &mut self,
        io: &mut Io<'_>,
        method: &str,
        params: Value,
        kind: Outstanding,
    ) -> Result<u64> {
        let (id, sent) = io.request(method, params).await;
        self.outstanding.insert(id, kind);
        sent.map(|()| id)
    }

    async fn list_skills(&mut self, io: &mut Io<'_>) -> Result<()> {
        self.skills_pending = true;
        let params = json!({"cwds": [self.cfg.cwd]});
        io.request_named(SKILLS_ID, "skills/list", params).await
    }

    /// A turn's input items; `/name` for a skill goes as Codex's own
    /// interface sends it, `$name` and the skill.
    fn input(&self, text: &str, attachments: &[Attachment]) -> Value {
        let Some((text, name, path)) = skill_call(text, &self.skills) else {
            return turn_input(text, attachments);
        };
        let mut items = turn_input(&text, attachments);
        if let Some(items) = items.as_array_mut() {
            items.push(json!({"type": "skill", "name": name, "path": path}));
        }
        items
    }

    async fn start_turn(
        &mut self,
        io: &mut Io<'_>,
        text: String,
        attachments: Vec<Attachment>,
    ) -> Result<()> {
        let thread_id = match self.thread_id.clone() {
            Some(t) if !(self.skills_pending && text.starts_with('/')) => t,
            _ => {
                self.queued_turn = Some((text, attachments));
                return Ok(());
            }
        };
        let mut params = json!({
            "threadId": thread_id,
            "input": self.input(&text, &attachments),
        });
        if let Some(m) = &self.model {
            params["model"] = json!(m.model);
        }
        if let Some(e) = &self.effort {
            params["effort"] = json!(e);
        }
        let (approval, _) = policy_params(self.policy, self.own_sandbox);
        params["approvalPolicy"] = json!(approval);
        params["sandboxPolicy"] = sandbox_policy(self.own_sandbox);
        self.request(io, "turn/start", params, Outstanding::TurnStart)
            .await?;
        Ok(())
    }

    /// The main thread's turn has an id: interrupt it if that was asked
    /// for before it had one.
    async fn turn_known(&mut self, io: &mut Io<'_>) -> Result<()> {
        if !std::mem::take(&mut self.interrupt_wanted) {
            return Ok(());
        }
        let (Some(thread), Some(turn)) = (self.thread_id.clone(), self.turn_id.clone()) else {
            return Ok(());
        };
        let params = json!({"threadId": thread, "turnId": turn});
        self.request(io, "turn/interrupt", params, Outstanding::Interrupt)
            .await
            .map(|_| ())
    }

    /// A sub-agent's turn counts as one: it runs on with the main turn over.
    fn turn_open(&self) -> bool {
        self.turn_id.is_some()
            || !self.child_turns.is_empty()
            || self.queued_turn.is_some()
            || self
                .outstanding
                .values()
                .any(|o| *o == Outstanding::TurnStart)
    }

    /// Carry out `cmd`; what could not be written is the error.
    async fn command(&mut self, cmd: SessionCommand, io: &mut Io<'_>) -> Result<()> {
        match cmd {
            SessionCommand::SendTurn { text, attachments } => {
                self.start_turn(io, text, attachments).await
            }
            SessionCommand::Steer { text, attachments } => {
                match (self.thread_id.clone(), self.turn_id.clone()) {
                    (Some(t), Some(turn)) => {
                        let params = json!({
                            "threadId": t,
                            "expectedTurnId": turn,
                            "input": self.input(&text, &attachments),
                        });
                        self.request(io, "turn/steer", params, Outstanding::Other)
                            .await
                            .map(|_| ())
                    }
                    // Nothing is running: it is simply the next turn.
                    _ => self.start_turn(io, text, attachments).await,
                }
            }
            SessionCommand::Rewind { anchor } => match self.thread_id.clone() {
                Some(t) => self
                    .request(
                        io,
                        "thread/revert",
                        json!({"threadId": t, "beforeTurnId": anchor}),
                        Outstanding::Revert,
                    )
                    .await
                    .map(|_| ()),
                None => Ok(()),
            },
            SessionCommand::Compact { .. } => match self.thread_id.clone() {
                // Runs as a turn of its own (turn/started … turn/completed).
                Some(t) => self
                    .request(
                        io,
                        "thread/compact/start",
                        json!({"threadId": t}),
                        Outstanding::Other,
                    )
                    .await
                    .map(|_| ()),
                None => {
                    io.emit(AgentEvent::TurnCompleted {
                        stop_reason: StopReason::Error("codex: no thread to compact yet".into()),
                    })
                    .await;
                    Ok(())
                }
            },
            SessionCommand::Interrupt => {
                match (&self.thread_id, &self.turn_id) {
                    (Some(t), Some(turn)) => {
                        let params = json!({"threadId": t, "turnId": turn});
                        self.request(io, "turn/interrupt", params, Outstanding::Interrupt)
                            .await
                            .map(|_| ())
                    }
                    // Waiting for the thread: it is never sent.
                    _ if self.queued_turn.take().is_some() => {
                        io.emit(AgentEvent::TurnAnchor { id: String::new() }).await;
                        io.emit(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Interrupted,
                        })
                        .await;
                        Ok(())
                    }
                    // Sent, with no id to interrupt it by yet.
                    _ if self
                        .outstanding
                        .values()
                        .any(|o| *o == Outstanding::TurnStart) =>
                    {
                        self.interrupt_wanted = true;
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            SessionCommand::StopSubagent { id } => {
                let running = self.parser.child_thread(&id).and_then(|thread| {
                    self.child_turns
                        .get(thread)
                        .map(|turn| (thread.to_string(), turn.clone()))
                });
                match running {
                    Some((thread, turn)) => {
                        let params = json!({"threadId": thread, "turnId": turn});
                        self.request(io, "turn/interrupt", params, Outstanding::Interrupt)
                            .await
                            .map(|_| ())
                    }
                    None => {
                        io.emit(AgentEvent::Notice(
                            "that sub-agent is no longer running".into(),
                        ))
                        .await;
                        Ok(())
                    }
                }
            }
            SessionCommand::RespondPermission { id, decision } => match self.pending.remove(&id) {
                Some((rpc_id, kind)) => {
                    let approval = self.elicitations.remove(&id)
                        && matches!(kind, PermissionKind::ToolUse { .. });
                    let answer = if approval {
                        encode_mcp_approval(&decision)
                    } else {
                        encode_decision(&kind, &decision)
                    };
                    io.write(&jsonrpc::response(&rpc_id, answer)).await
                }
                None => {
                    io.emit(AgentEvent::Notice(format!("no pending codex request {id}")))
                        .await;
                    Ok(())
                }
            },
            SessionCommand::SetModel(m) => {
                self.model = Some(m);
                Ok(())
            }
            SessionCommand::SetEffort(e) => {
                self.effort = e;
                Ok(())
            }
            SessionCommand::SetPolicy(p) => {
                // Applied on the next turn/start (approvalPolicy + sandboxPolicy).
                self.policy = p;
                Ok(())
            }
            // Not sent here: `encode`'s and the driver's (`shutdown`).
            SessionCommand::RemoteControl { .. } | SessionCommand::Shutdown => Ok(()),
        }
    }

    /// The answer to `skills/list`. An older Codex without it answers with
    /// an error, which is not the user's: no skills, and the parser does
    /// not see it.
    async fn skills_listed(&mut self, result: Option<&Value>, io: &mut Io<'_>) {
        self.skills_pending = false;
        self.skills = result.map(listed_skills).unwrap_or_default();
        let commands: Vec<HarnessCommand> = self.skills.iter().map(|(c, _)| c.clone()).collect();
        io.emit(AgentEvent::CapabilitiesChanged(CapsUpdate {
            slash_commands: Some(!commands.is_empty()),
            commands: Some(commands),
            ..Default::default()
        }))
        .await;
        if let Some((t, a)) = self.queued_turn.take() {
            let started = self.start_turn(io, t, a).await;
            report(io, started).await;
        }
    }

    /// The answer to a numbered request of ours, which was `kind`.
    async fn answered(
        &mut self,
        kind: Option<Outstanding>,
        result: Option<Value>,
        error: Option<Value>,
        io: &mut Io<'_>,
    ) {
        match kind {
            Some(Outstanding::Initialize) => {
                let _ = io
                    .write(&jsonrpc::notification("initialized", Value::Null))
                    .await;
                let (approval, sandbox) = policy_params(self.cfg.policy, self.own_sandbox);
                let r = match self.cfg.resume.clone() {
                    // A fork answers like a start: with the new thread.
                    Some(id) => {
                        // A thread keeps the provider it was started on
                        // unless told otherwise, and so does a fork of it.
                        let mut params = json!({"threadId": id});
                        if let Some(p) = &self.cfg.provider {
                            params["modelProvider"] = json!(p.as_str());
                        }
                        if self.cfg.fork {
                            // (`excludeTurns`: codex deprecates returning the whole history here.)
                            params["excludeTurns"] = json!(true);
                            self.request(io, "thread/fork", params, Outstanding::ThreadStart)
                                .await
                        } else {
                            self.request(io, "thread/resume", params, Outstanding::ThreadStart)
                                .await
                        }
                    }
                    None => {
                        let mut params = json!({
                            "cwd": self.cfg.cwd,
                            "approvalPolicy": approval,
                            "sandbox": sandbox,
                        });
                        if let Some(m) = &self.cfg.model {
                            params["model"] = json!(m.model);
                        }
                        self.request(io, "thread/start", params, Outstanding::ThreadStart)
                            .await
                    }
                };
                report(io, r).await;
            }
            Some(Outstanding::ThreadStart) => {
                if let Some(err) = error {
                    let msg = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("thread start failed");
                    // No turn id to rewind to: keep the anchor sequence aligned.
                    io.emit(AgentEvent::TurnAnchor { id: String::new() }).await;
                    io.emit(AgentEvent::TurnCompleted {
                        stop_reason: StopReason::Error(format!("codex: {msg}")),
                    })
                    .await;
                } else {
                    self.thread_id = result
                        .as_ref()
                        .and_then(|r| r.pointer("/thread/id").or_else(|| r.get("threadId")))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if let Some(reported) = result
                        .as_ref()
                        .and_then(|r| r.get("modelProvider"))
                        .and_then(Value::as_str)
                    {
                        if let Some(why) =
                            super::provider_mismatch(self.cfg.provider.as_ref(), reported)
                        {
                            io.emit(AgentEvent::Error(why)).await;
                        }
                        io.emit(AgentEvent::CapabilitiesChanged(CapsUpdate {
                            provider: Some(reported.into()),
                            ..Default::default()
                        }))
                        .await;
                    }
                    let listed = self.list_skills(io).await;
                    report(io, listed).await;
                    if let Some((t, a)) = self.queued_turn.take() {
                        let started = self.start_turn(io, t, a).await;
                        report(io, started).await;
                    }
                }
            }
            Some(Outstanding::Revert) => {
                if let Some(err) = error {
                    let reason = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("revert failed")
                        .to_string();
                    io.emit(AgentEvent::RewindFailed { reason }).await;
                }
            }
            Some(Outstanding::TurnStart) => {
                if let Some(err) = error {
                    self.interrupt_wanted = false;
                    let msg = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("turn start failed");
                    // No turn id to rewind to: keep the anchor sequence aligned.
                    io.emit(AgentEvent::TurnAnchor { id: String::new() }).await;
                    io.emit(AgentEvent::TurnCompleted {
                        stop_reason: StopReason::Error(format!("codex: {msg}")),
                    })
                    .await;
                } else {
                    self.turn_id = result
                        .as_ref()
                        .and_then(|r| r.pointer("/turn/id"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    // `thread/revert` takes this id to drop the turn later.
                    if let Some(id) = self.turn_id.clone() {
                        io.emit(AgentEvent::TurnAnchor { id }).await;
                    }
                    let interrupted = self.turn_known(io).await;
                    report(io, interrupted).await;
                }
            }
            _ => {}
        }
    }

    /// `line` through the parser, its events passed on.
    async fn feed(&mut self, line: &str, io: &mut Io<'_>) -> Result<(), Gone> {
        for ev in self.parser.feed(line) {
            // A sub-agent's request arrives wrapped in `Sub`.
            let mut inner = &ev;
            while let AgentEvent::Sub { event, .. } = inner {
                inner = event;
            }
            if let AgentEvent::PermissionRequest(req) = inner
                && let Some((rpc_id, _)) = self.pending.remove(&req.id)
            {
                self.pending
                    .insert(req.id.clone(), (rpc_id, req.kind.clone()));
            }
            io.forward(ev).await?;
        }
        Ok(())
    }
}

impl StreamProtocol for AppServer {
    async fn start(&mut self, io: &mut Io<'_>) {
        let params =
            json!({"clientInfo": {"name": "unharness", "version": env!("CARGO_PKG_VERSION")}});
        let sent = self
            .request(io, "initialize", params, Outstanding::Initialize)
            .await;
        report(io, sent).await;
    }

    async fn encode(&mut self, cmd: SessionCommand, io: &mut Io<'_>) -> Encoded {
        // Codex has none, and does not declare it.
        if matches!(cmd, SessionCommand::RemoteControl { .. }) {
            return Encoded::Unsupported;
        }
        let res = self.command(cmd, io).await;
        report(io, res).await;
        Encoded::Done
    }

    fn write_failed(&mut self, error: anyhow::Error) -> Vec<AgentEvent> {
        vec![AgentEvent::Error(format!("codex: {error}"))]
    }

    fn parser(&mut self) -> &mut dyn Parser {
        &mut self.parser
    }

    async fn on_answer(&mut self, answer: Answer<'_>, io: &mut Io<'_>) -> Result<(), Gone> {
        let kind = match &answer.id {
            RequestId::Name(name) if name == SKILLS_ID => {
                self.skills_listed(answer.result.as_ref(), io).await;
                return Ok(());
            }
            RequestId::Number(id) => self.outstanding.remove(id),
            RequestId::Name(_) => None,
        };
        self.answered(kind, answer.result, answer.error, io).await;
        self.feed(answer.line, io).await
    }

    async fn on_stdout(&mut self, line: &str, io: &mut Io<'_>) -> Result<(), Gone> {
        match RpcMessage::parse(line) {
            // Track the running turn: steer and interrupt need its id.
            // Sub-agent threads share the stream; only the main thread's turn counts.
            Some(RpcMessage::Notification { method, params })
                if params.get("threadId").and_then(Value::as_str) == self.thread_id.as_deref() =>
            {
                match method.as_str() {
                    "turn/started" => {
                        self.turn_id = params
                            .pointer("/turn/id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let interrupted = self.turn_known(io).await;
                        report(io, interrupted).await;
                    }
                    "turn/completed" => {
                        self.turn_id = None;
                        self.interrupt_wanted = false;
                    }
                    _ => {}
                }
            }
            // Its skill files changed: what it lists now.
            Some(RpcMessage::Notification { method, .. }) if method == "skills/changed" => {
                let listed = self.list_skills(io).await;
                report(io, listed).await;
            }
            // A sub-agent's turn is what stopping it interrupts.
            Some(RpcMessage::Notification { method, params }) => {
                if let Some(thread) = params.get("threadId").and_then(Value::as_str) {
                    match method.as_str() {
                        "turn/started" => {
                            if let Some(turn) = params.pointer("/turn/id").and_then(Value::as_str) {
                                self.child_turns
                                    .insert(thread.to_string(), turn.to_string());
                            }
                        }
                        "turn/completed" => {
                            self.child_turns.remove(thread);
                        }
                        _ => {}
                    }
                }
            }
            Some(RpcMessage::Request { id, method, .. }) => {
                // Remember the rpc id so the decision can be routed back.
                let rid = match &id {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let supported = method.ends_with("requestApproval")
                    || method == "item/tool/requestUserInput"
                    || method == "mcpServer/elicitation/request";
                if !supported {
                    let _ = io
                        .write(&jsonrpc::error_response(
                            &id,
                            -32601,
                            "unsupported by unharness",
                        ))
                        .await;
                } else {
                    if method == "mcpServer/elicitation/request" {
                        self.elicitations.insert(rid.clone());
                    }
                    self.pending.insert(
                        rid,
                        (
                            id,
                            PermissionKind::Confirm {
                                title: String::new(),
                                message: None,
                            },
                        ),
                    );
                }
            }
            // The answers to our requests come to `on_answer`: an answer
            // under any other id is the parser's alone.
            Some(RpcMessage::Response { .. }) | None => {}
        }
        self.feed(line, io).await
    }

    fn shutdown(&mut self) -> Shutdown {
        Shutdown::EndOrKill {
            turn_open: self.turn_open(),
        }
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

    /// The answer to `skills/list` in `fixtures/app_server_skill.jsonl`.
    fn recorded_skills() -> Vec<(HarnessCommand, String)> {
        let fixture = include_str!("fixtures/app_server_skill.jsonl");
        let line = fixture
            .lines()
            .find(|l| l.starts_with(r#"{"id":"skills""#))
            .unwrap();
        let answer: Value = serde_json::from_str(line).unwrap();
        listed_skills(&answer["result"])
    }

    #[test]
    fn skills_are_listed_with_their_paths() {
        let skills = recorded_skills();
        let names: Vec<&str> = skills.iter().map(|(c, _)| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "pineapple",
                "cq",
                "sites:sites-building",
                "superpowers:brainstorming",
                "imagegen",
                "openai-docs"
            ]
        );
        assert_eq!(skills[0].1, "/WORKSPACE/.agents/skills/pineapple/SKILL.md");
        assert!(
            skills[0]
                .0
                .description
                .starts_with("Answers with a fixed code word")
        );
        // The short description of its interface, where it has one.
        let docs = &skills[5].0;
        assert!(docs.description.len() < 120, "{}", docs.description);

        let disabled = json!({"data": [{"cwd": "/w", "errors": [], "skills": [
            {"name": "off", "description": "", "path": "/p", "scope": "user", "enabled": false},
            {"name": "on", "description": "d", "path": "/q", "scope": "user", "enabled": true},
            {"name": "on", "description": "again", "path": "/r", "scope": "repo", "enabled": true},
        ]}]});
        let skills = listed_skills(&disabled);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].1, "/q");
    }

    #[test]
    fn a_slash_for_a_skill_goes_as_codex_names_one() {
        let skills = recorded_skills();
        assert_eq!(
            skill_call("/pineapple hello there", &skills),
            Some((
                "$pineapple hello there".to_string(),
                "pineapple",
                "/WORKSPACE/.agents/skills/pineapple/SKILL.md"
            ))
        );
        assert_eq!(
            skill_call("/superpowers:brainstorming", &skills).map(|c| c.0),
            Some("$superpowers:brainstorming".to_string())
        );
        assert_eq!(skill_call("/nope x", &skills), None);
        assert_eq!(skill_call("pineapple", &skills), None);
        assert_eq!(skill_call("/", &skills), None);
    }

    #[test]
    fn decisions() {
        let tool = PermissionKind::ToolUse {
            tool: "shell".into(),
            input: Value::Null,
            action: crate::core::ToolAction::Opaque,
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
            policy_params(
                PermissionPolicy::Bypass,
                OwnSandbox::Level(SandboxLevel::Off)
            ),
            ("never", "danger-full-access")
        );
    }

    #[test]
    fn own_sandbox_is_off_inside_ours() {
        // Approvals are untouched; only the sandbox is handed over.
        for policy in PermissionPolicy::ALL {
            assert_eq!(
                sandbox_policy(OwnSandbox::External),
                json!({"type": "externalSandbox", "networkAccess": "enabled"})
            );
            assert_eq!(
                policy_params(policy, OwnSandbox::External).0,
                policy_params(policy, OwnSandbox::Level(SandboxLevel::WorkspaceWrite)).0
            );
            assert_eq!(
                policy_params(policy, OwnSandbox::External).1,
                "danger-full-access"
            );
        }
        assert_eq!(
            policy_params(PermissionPolicy::Ask, OwnSandbox::External).0,
            "untrusted"
        );
    }

    /// Where ours does not run, Codex's own sandbox holds to the level the
    /// user set, whatever the policy: `bypass` does not widen it.
    #[test]
    fn own_sandbox_takes_the_level_not_the_policy() {
        for (level, name, kind) in [
            (SandboxLevel::ReadOnly, "read-only", "readOnly"),
            (
                SandboxLevel::WorkspaceWrite,
                "workspace-write",
                "workspaceWrite",
            ),
            (SandboxLevel::Off, "danger-full-access", "dangerFullAccess"),
        ] {
            let own = OwnSandbox::Level(level);
            assert_eq!(sandbox_policy(own), json!({"type": kind}));
            for policy in PermissionPolicy::ALL {
                assert_eq!(policy_params(policy, own).1, name);
            }
        }
        assert_eq!(
            policy_params(
                PermissionPolicy::Bypass,
                OwnSandbox::Level(SandboxLevel::ReadOnly)
            ),
            ("never", "read-only")
        );
    }
}
