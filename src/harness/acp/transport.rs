//! Long-lived ACP session over stdio: `initialize` → `session/new` (or
//! `session/resume` / `session/load`) → one `session/prompt` per turn →
//! `session/close`, where the agent takes it, answered before stdin closes.
//!
//! unharness declares no `fs` or `terminal` client capabilities, so the
//! agent uses its own file and terminal tools and reports them as tool
//! calls. It declares form elicitation, so that the agent can ask the user
//! questions. The agent → client requests served are
//! `session/request_permission` and `elicitation/create` in form mode
//! (`elicitation.rs`); the rest are refused. The agent may withdraw one that
//! waits for the user (`$/cancel_request`).
//!
//! An agent with session modes is held to the one its policy maps to
//! (`modes.rs`): after every session answer (new, load, resume) and every
//! policy change the mode is set and read back before a prompt goes, and a
//! policy whose mode does not take is refused for the session
//! (`CapsUpdate::refused_policies`), its prompt failed rather than sent.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::elicitation::{self, Form, Refusal};
use super::modes::{AgentModes, ModeMap, ModeStep, SetVia, mode_step, read_back};
use super::parse::{
    AcpParser, SessionOptions, can_close, can_load, can_resume, error_text, mcp_http,
};
use super::spawn::{SharedModels, SpawnChoice, SpawnEnv, SpawnPlan};
use crate::core::jsonrpc::{self, RpcMessage};
use crate::core::mcp;
use crate::core::process::{LineProcess, RawLine};
use crate::core::session::ProcessSlot;
use crate::core::{
    AgentEvent, Attachment, CapsUpdate, HarnessId, McpServer, McpTransport, PermissionDecision,
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

/// Feed a `session/request_permission` line to the parser, and say whether
/// `policy` answers it without asking, with the events to pass on (the
/// question left out when it is answered: the tool call is still shown).
///
/// The kind is the tool call's as the parser has merged it: vibe-acp 2.26.1
/// sends only `toolCallId` in the request, the kind, input and locations
/// having come on the `tool_call` before it. A kind the request carries
/// replaces the earlier one, as an update's does; a request without an id
/// has only its own.
fn screen_permission(
    parser: &mut AcpParser,
    policy: PermissionPolicy,
    line: &str,
    params: &Value,
) -> (bool, Vec<AgentEvent>) {
    let mut evs = parser.feed(line);
    let call = params.get("toolCall").unwrap_or(&Value::Null);
    let kind = call
        .get("toolCallId")
        .and_then(Value::as_str)
        .and_then(|id| parser.tool_kind(id))
        .or_else(|| call.get("kind").and_then(Value::as_str))
        .unwrap_or("");
    let allowed = auto_allows(policy, kind);
    if allowed {
        evs.retain(|ev| !matches!(ev, AgentEvent::PermissionRequest(_)));
    }
    (allowed, evs)
}

/// The JSON-RPC error that answers a request cancelled by its sender
/// (`$/cancel_request`).
const REQUEST_CANCELLED: i64 = -32800;

/// Our id of an agent → client request: the TUI's request ids are strings.
fn request_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The client capabilities of `initialize`: form elicitation, and no
/// `fs` or `terminal`. Without it vibe-acp 2.26.1 does not offer its
/// `ask_user_question` tool, claude-agent-acp turns off AskUserQuestion,
/// and codex-acp answers its own questions empty (#157).
pub fn client_capabilities() -> Value {
    json!({"elicitation": {"form": {}}})
}

/// An agent → client request that waits for the user.
#[derive(Debug)]
enum Pending {
    /// `session/request_permission`, with the options it offered.
    Permission { rpc_id: Value, options: Value },
    /// `elicitation/create` in form mode.
    Elicitation { rpc_id: Value, form: Form },
}

impl Pending {
    fn rpc_id(&self) -> &Value {
        match self {
            Pending::Permission { rpc_id, .. } | Pending::Elicitation { rpc_id, .. } => rpc_id,
        }
    }

    /// The answer once the turn it belongs to is cancelled.
    fn cancelled(&self) -> Value {
        match self {
            Pending::Permission { .. } => json!({"outcome": {"outcome": "cancelled"}}),
            Pending::Elicitation { .. } => elicitation::cancelled(),
        }
    }
}

/// How long a shutdown waits for the answer to `session/close` before it
/// closes stdin. The agent's [`END_GRACE`](crate::core::process::END_GRACE)
/// starts then: the whole stays within what the TUI waits for an agent to
/// end before it kills it.
pub const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// What a harness knows about its agent that the protocol does not say.
#[derive(Clone, Default)]
pub struct AgentSpec {
    /// The config option id that is the agent's reasoning effort
    /// (`AcpHarness::declaring_effort_option`).
    pub effort_option: Option<String>,
    /// Which of the agent's session modes backs each policy
    /// (`AcpHarness::with_modes`).
    pub modes: ModeMap,
    /// The agent's model and effort are chosen by its environment when it
    /// starts, never by a session option (`AcpHarness::choosing_at_spawn`).
    pub spawn_env: Option<SpawnEnv>,
    /// What its sessions said about its models, for the builder.
    pub known: SharedModels,
}

/// The agent's command line, in `cfg.cwd`, with `extra` in its environment.
fn agent_command(args: &[String], cfg: &SessionConfig, extra: &[(String, String)]) -> Command {
    let mut command = Command::new(&cfg.binary);
    command
        .args(args)
        .args(&cfg.extra_args)
        .current_dir(&cfg.cwd);
    // An agent started from inside another agent's session may behave differently.
    for (k, _) in std::env::vars() {
        if k.starts_with("CLAUDECODE") || k == "CLAUDE_CODE_ENTRYPOINT" {
            command.env_remove(k);
        }
    }
    for (k, v) in extra.iter().chain(&cfg.env) {
        command.env(k, v);
    }
    command
}

/// What the agent's environment says for the chosen model and effort.
fn spawn_plan(spec: &AgentSpec, cfg: &SessionConfig) -> Option<SpawnPlan> {
    let build = spec.spawn_env.as_ref()?;
    let known = spec.known.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Some(build(&SpawnChoice {
        model: cfg.model.as_ref().map(|m| m.model.as_str()),
        effort: cfg.effort.as_deref(),
        known: &known,
        env: &cfg.env,
    }))
}

pub fn start(
    harness: HarnessId,
    args: Vec<String>,
    spec: AgentSpec,
    cfg: SessionConfig,
) -> Result<SessionHandle> {
    let plan = spawn_plan(&spec, &cfg);
    let extra = plan.as_ref().map(|p| p.env.as_slice()).unwrap_or_default();
    let proc = LineProcess::spawn(agent_command(&args, &cfg, extra), &cfg.sandbox)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness,
        process_model: ProcessModel::LongLived,
    });
    let slot = handle.process_slot();
    slot.set(&proc);
    let parser = AcpParser::new(harness).declaring_effort(spec.effort_option.clone());
    tokio::spawn(drive(
        harness,
        proc,
        parser,
        Spawning {
            args,
            spec,
            plan,
            slot,
            restart: None,
        },
        cfg,
        events_tx,
        cmd_rx,
    ));
    Ok(handle)
}

/// How the driver starts the agent's process again, for an agent whose
/// model and effort are chosen at spawn ([`AgentSpec::spawn_env`]).
struct Spawning {
    args: Vec<String>,
    spec: AgentSpec,
    /// What the running process was started with; `None` for an agent
    /// that does not choose at spawn.
    plan: Option<SpawnPlan>,
    slot: ProcessSlot,
    /// The process is ending to be started again with this plan.
    restart: Option<SpawnPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outstanding {
    Initialize,
    /// `session/new`, or a reattach to an existing session.
    Session {
        reattach: bool,
    },
    Prompt,
    /// Setting the agent's mode for `policy`; `seq` tells the newest.
    Mode {
        seq: u64,
        policy: PermissionPolicy,
        mode: String,
        via: SetVia,
    },
    /// Config changes: only an error matters, and the parser reports it.
    Other,
    /// `session/close` at shutdown: its answer, `{}` or an error, says
    /// nothing anyone is waiting for.
    Close,
}

/// Where the agent's mode stands for the driver's policy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModeState {
    /// Prompts go: the agent is in `held`, the mode its policy maps to, or
    /// nothing is mapped (`None`).
    Ready { held: Option<String> },
    /// A mode was asked for and has not answered: prompts wait.
    Setting,
    /// The policy cannot be held in this session: prompts fail with this.
    Refused(String),
}

struct Driver {
    harness: HarnessId,
    proc: LineProcess,
    spawning: Spawning,
    /// The process was started again once already (only one restart is
    /// made to bring in what a session said).
    restarted: bool,
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
    /// Permission requests and elicitations awaiting the user, by our id.
    pending: HashMap<String, Pending>,
    /// Requests the agent withdrew while they waited for the user: an
    /// answer the user gave meanwhile is dropped.
    withdrawn: HashSet<String>,
    /// A turn sent before the session and its mode were ready.
    queued_turn: Option<(String, Vec<Attachment>)>,
    last_stderr: Option<String>,
    mode_map: ModeMap,
    /// What the session said about its modes.
    agent_modes: AgentModes,
    mode: ModeState,
    /// Numbers the mode requests; only the newest one's answer counts.
    mode_seq: u64,
    /// Policies this session could not hold, as reported.
    refused: Vec<PermissionPolicy>,
    /// The configured model and effort are still to be sent to the
    /// session, once its mode is set.
    setup_pending: bool,
    /// The agent takes `session/close` (`sessionCapabilities.close`).
    can_close: bool,
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

    /// Send the waiting turn if the session and its mode are ready, or
    /// fail it if its policy was refused.
    async fn flush_turn(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        match &self.mode {
            ModeState::Setting => Ok(()),
            ModeState::Refused(why) => {
                if self.queued_turn.take().is_some() {
                    let _ = events
                        .send(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Error(why.clone()),
                        })
                        .await;
                }
                Ok(())
            }
            ModeState::Ready { .. } => {
                let Some((text, attachments)) = self.queued_turn.take() else {
                    return Ok(());
                };
                let _ = events.send(AgentEvent::TurnStarted).await;
                let sent = match prompt_blocks(&text, &attachments) {
                    Ok(prompt) => {
                        self.request(
                            "session/prompt",
                            json!({"sessionId": session_id, "prompt": prompt}),
                            Outstanding::Prompt,
                        )
                        .await
                    }
                    Err(e) => Err(e),
                };
                if let Err(e) = sent {
                    let _ = events
                        .send(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Error(e.to_string()),
                        })
                        .await;
                }
                Ok(())
            }
        }
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

    /// The session exists (created or reattached): hold it to the mode its
    /// policy maps to, then send the configured model and effort and the
    /// turn that was waiting for it.
    async fn session_ready(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        self.setup_pending = true;
        self.apply_mode(events).await
    }

    /// Bring the agent's mode in line with the policy; once it is, go on
    /// with what waited for it.
    async fn apply_mode(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        let policy = self.policy;
        match mode_step(
            self.harness.as_str(),
            policy,
            &self.mode_map,
            &self.agent_modes,
        ) {
            ModeStep::Leave => {
                self.mode = ModeState::Ready { held: None };
                self.mode_ready(events).await
            }
            ModeStep::Keep(mode) => {
                self.mode = ModeState::Ready { held: Some(mode) };
                self.mode_ready(events).await
            }
            ModeStep::Set { mode, via } => {
                self.mode_seq += 1;
                let (method, params) = match &via {
                    SetVia::Option(id) => (
                        "session/set_config_option",
                        json!({"sessionId": session_id, "configId": id, "value": mode}),
                    ),
                    SetVia::SetMode => (
                        "session/set_mode",
                        json!({"sessionId": session_id, "modeId": mode}),
                    ),
                };
                self.mode = ModeState::Setting;
                let seq = self.mode_seq;
                self.request(
                    method,
                    params,
                    Outstanding::Mode {
                        seq,
                        policy,
                        mode,
                        via,
                    },
                )
                .await
            }
            ModeStep::Refuse(why) => self.refuse(why, events).await,
        }
    }

    /// The mode is what the policy needs: send what waited for it.
    async fn mode_ready(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        // Chosen at spawn: nothing is sent for them.
        if std::mem::take(&mut self.setup_pending) && self.spawning.plan.is_none() {
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
        }
        self.flush_turn(events).await
    }

    /// The policy cannot be held in this session: say so, take it out of
    /// what is offered, fail the turn that waited for it and stop one that
    /// is running.
    async fn refuse(&mut self, why: String, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        self.mode = ModeState::Refused(why.clone());
        if !self.refused.contains(&self.policy) {
            self.refused.push(self.policy);
            self.refused.sort();
            let _ = events
                .send(AgentEvent::CapabilitiesChanged(CapsUpdate {
                    refused_policies: Some(self.refused.clone()),
                    ..Default::default()
                }))
                .await;
        }
        if self.queued_turn.is_none() {
            let _ = events.send(AgentEvent::Error(why)).await;
        }
        // A turn running when the policy changed goes on in whatever mode
        // the agent is in, which may run what the policy asks about.
        if self.outstanding.values().any(|o| *o == Outstanding::Prompt) {
            self.cancel_turn().await?;
        }
        self.flush_turn(events).await
    }

    /// The answer to a mode request: the mode took, or the policy is refused.
    async fn on_mode_answer(
        &mut self,
        (seq, policy, mode, via): (u64, PermissionPolicy, String, SetVia),
        result: &Value,
        error: Option<&Value>,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        let options = self.parser.session_options(result.get("configOptions"));
        if let Some(o) = &options {
            self.agent_modes.take_options(o);
            self.options = o.clone();
        }
        // A later policy change asked for another mode since.
        if seq != self.mode_seq {
            return Ok(());
        }
        let options = options.unwrap_or_default();
        let answer = match error {
            Some(e) => Err(error_text(e)),
            None => Ok(&options),
        };
        match read_back(self.harness.as_str(), policy, &mode, &via, answer) {
            Ok(()) => {
                self.agent_modes.current = Some(mode.clone());
                self.mode = ModeState::Ready { held: Some(mode) };
                self.mode_ready(events).await
            }
            Err(why) => self.refuse(why, events).await,
        }
    }

    /// The agent reported its mode (`current_mode_update`, or the mode
    /// option of a `config_option_update`). One it went to by itself, away
    /// from the mode the policy maps to, is set back; if being in it could
    /// run what the policy asks about, the running turn is cancelled first.
    async fn on_mode_report(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let ModeState::Ready { held: Some(held) } = &self.mode else {
            return Ok(());
        };
        let Some(now) = self.agent_modes.current.clone() else {
            return Ok(());
        };
        if now == *held {
            return Ok(());
        }
        let name = self.harness.as_str();
        let policy = self.policy;
        let turn_running = self.outstanding.values().any(|o| *o == Outstanding::Prompt);
        if turn_running && !self.mode_map.within(policy, &now) {
            let _ = events
                .send(AgentEvent::Notice(format!(
                    "{name} went into its mode '{now}', which can run what {policy} asks about; the turn is stopped and mode '{held}' set again"
                )))
                .await;
            self.cancel_turn().await?;
        } else {
            let _ = events
                .send(AgentEvent::Notice(format!(
                    "{name} went into its mode '{now}'; mode '{held}' ({policy}) is set again"
                )))
                .await;
        }
        self.apply_mode(events).await
    }

    /// `$/cancel_request`: the agent withdraws a request it sent. One that
    /// waits for the user is dropped, so that the user's answer is not sent,
    /// the TUI is told (`PermissionWithdrawn` closes its modal), and it is
    /// answered with the error the protocol gives a cancelled request. An
    /// id that waits for nothing (unknown, already answered, or answered by
    /// the policy) is ignored: its answer has gone.
    ///
    /// An elicitation that waits for the user is withdrawn the same way:
    /// it is kept under its id with the permission requests.
    async fn withdraw(&mut self, params: &Value, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let Some(id) = params.get("requestId").map(request_key) else {
            return Ok(());
        };
        let Some(pending) = self.pending.remove(&id) else {
            return Ok(());
        };
        let rpc_id = pending.rpc_id();
        self.withdrawn.insert(id.clone());
        let _ = events.send(AgentEvent::PermissionWithdrawn { id }).await;
        self.proc
            .write_line(&jsonrpc::error_response(
                rpc_id,
                REQUEST_CANCELLED,
                "Request cancelled",
            ))
            .await
    }

    /// `session/close` for the session, if the agent takes it, at a
    /// shutdown between turns, so that an agent that outlives its stdin, or
    /// serves more than one session, lets go of it. Whether it was sent:
    /// stdin is then closed once it is answered, or after [`CLOSE_WAIT`]
    /// (vibe-acp 2.26.1 left it unanswered when stdin closed right behind
    /// it).
    async fn close_session(&mut self) -> bool {
        let Some(session_id) = self.session_id.clone() else {
            return false;
        };
        self.can_close
            && self
                .request(
                    "session/close",
                    json!({"sessionId": session_id}),
                    Outstanding::Close,
                )
                .await
                .is_ok()
    }

    /// `elicitation/create`: a form waits for the user, whatever the
    /// policy (a question is never answered for them). A URL elicitation,
    /// never declared, is refused with an error; a form with a field no
    /// question stands for is declined. Either is told to the user.
    async fn on_elicitation(
        &mut self,
        line: &str,
        id: &Value,
        params: &Value,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        let name = self.harness.as_str();
        match Form::from_request(line, params) {
            Ok(form) => {
                let rid = request_key(id);
                self.withdrawn.remove(&rid);
                self.pending.insert(
                    rid,
                    Pending::Elicitation {
                        rpc_id: id.clone(),
                        form,
                    },
                );
                forward(self, line, events).await;
                Ok(())
            }
            Err(refusal) => {
                let _ = events
                    .send(AgentEvent::Notice(format!(
                        "{name} asked for {}; it was refused",
                        match &refusal {
                            Refusal::NotForm(what) => what.clone(),
                            Refusal::Unsupported(why) => format!("a form, but {why}"),
                        }
                    )))
                    .await;
                let line = match refusal {
                    Refusal::NotForm(_) => {
                        jsonrpc::error_response(id, -32602, "unsupported by unharness")
                    }
                    Refusal::Unsupported(_) => jsonrpc::response(id, elicitation::declined()),
                };
                self.proc.write_line(&line).await
            }
        }
    }

    /// For an agent that chooses its model and effort at spawn, a session
    /// answer: keep what it says about the models, and decide whether the
    /// process has to start again (its environment lacked what this
    /// session told). `Some` is the plan to start it again with.
    fn learn_from_session(&mut self) -> Option<SpawnPlan> {
        let plan = self.spawning.plan.as_ref()?;
        self.spawning
            .spec
            .known
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .learn(
                &self.options,
                plan.overrides.as_deref(),
                // A plan that replaces a model's entry pins that model too.
                self.cfg.model.is_some() || plan.overrides.is_some(),
            );
        if plan.unmet.is_none() || self.restarted {
            return None;
        }
        spawn_plan(&self.spawning.spec, &self.cfg).filter(|next| next.unmet.is_none())
    }

    /// End the process to start it again with `plan`, before anything
    /// went to its session. The session answer that led here is not
    /// passed on: the next process answers again.
    fn restart_with(&mut self, plan: SpawnPlan) {
        self.restarted = true;
        self.spawning.restart = Some(plan);
        self.session_id = None;
        self.mode = ModeState::Setting;
        self.outstanding.clear();
        self.parser.set_replaying(false);
        self.proc.end();
    }

    /// The process that was ending for a restart has exited: start the
    /// next one and begin its handshake.
    async fn respawn(&mut self, plan: SpawnPlan) -> Result<()> {
        let command = agent_command(&self.spawning.args, &self.cfg, &plan.env);
        self.proc = LineProcess::spawn(command, &self.cfg.sandbox)?;
        self.spawning.slot.set(&self.proc);
        self.spawning.plan = Some(plan);
        self.last_stderr = None;
        // A new connection: its requests are numbered from the start.
        self.next_id = 0;
        self.request("initialize", initialize_params(), Outstanding::Initialize)
            .await
    }

    /// What the session runs, against what was chosen for an agent that
    /// chooses at spawn: an agent may fall back to another model, or
    /// reset a level the model does not offer, without an error.
    async fn check_choice(&self, events: &mpsc::Sender<AgentEvent>) {
        let Some(plan) = &self.spawning.plan else {
            return;
        };
        let name = self.harness.as_str();
        if let Some(model) = &self.cfg.model
            && let Some(current) = &self.options.current_model
            && *current != model.model
        {
            let _ = events
                .send(AgentEvent::Notice(format!(
                    "{name} runs model '{current}', not '{}' as chosen (it may not offer that model)",
                    model.model
                )))
                .await;
        }
        if let Some(effort) = &self.cfg.effort
            && let Some(current) = &self.options.current_effort
            && current != effort
        {
            let why = match &plan.unmet {
                Some(why) => why.clone(),
                None => format!("the model offers {}", self.options.efforts.join(", ")),
            };
            let _ = events
                .send(AgentEvent::Notice(format!(
                    "{name} runs effort '{current}', not '{effort}' as chosen ({why})"
                )))
                .await;
        }
    }

    fn closing(&self) -> bool {
        self.outstanding.values().any(|o| *o == Outstanding::Close)
    }

    /// `session/cancel`, and every open permission request answered
    /// `cancelled`, as the spec requires once a turn is cancelled; an open
    /// elicitation is answered `cancel`.
    async fn cancel_turn(&mut self) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        let mut res = self
            .proc
            .write_line(&jsonrpc::notification(
                "session/cancel",
                json!({"sessionId": session_id}),
            ))
            .await;
        for (_, pending) in self.pending.drain() {
            let line = jsonrpc::response(pending.rpc_id(), pending.cancelled());
            res = res.and(self.proc.write_line(&line).await);
        }
        res
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": client_capabilities(),
        "clientInfo": {"name": "unharness", "title": "unharness", "version": env!("CARGO_PKG_VERSION")},
    })
}

async fn drive(
    harness: HarnessId,
    proc: LineProcess,
    parser: AcpParser,
    spawning: Spawning,
    cfg: SessionConfig,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
) {
    let mode_map = spawning.spec.modes.clone();
    let mut d = Driver {
        harness,
        proc,
        spawning,
        restarted: false,
        parser,
        policy: cfg.policy,
        cfg,
        next_id: 0,
        outstanding: HashMap::new(),
        session_id: None,
        mcp_servers: json!([]),
        options: SessionOptions::default(),
        pending: HashMap::new(),
        withdrawn: HashSet::new(),
        queued_turn: None,
        last_stderr: None,
        mode_map,
        agent_modes: AgentModes::default(),
        mode: ModeState::Setting,
        mode_seq: 0,
        refused: Vec::new(),
        setup_pending: false,
        can_close: false,
    };
    if let Err(e) = d
        .request("initialize", initialize_params(), Outstanding::Initialize)
        .await
    {
        let _ = events
            .send(AgentEvent::Error(format!("{harness}: {e}")))
            .await;
    }
    let mut shutting_down = false;
    // Until when stdin stays open for the answer to `session/close`.
    let mut close_by: Option<Instant> = None;

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    if close_by.is_some() {
                        // Its grace starts now rather than with the answer.
                        d.proc.end();
                    } else if !shutting_down {
                        d.proc.kill().await;
                    }
                    return;
                };
                // The session is ending: nothing more goes to it.
                if shutting_down {
                    continue;
                }
                let res: Result<()> = match cmd {
                    SessionCommand::SendTurn { text, attachments } => {
                        d.queued_turn = Some((text, attachments));
                        d.flush_turn(&events).await
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
                    SessionCommand::Interrupt => {
                        // Waiting for the session or its mode: it is never sent.
                        if d.queued_turn.take().is_some() {
                            let _ = events.send(AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Interrupted,
                            }).await;
                            Ok(())
                        } else {
                            d.cancel_turn().await
                        }
                    }
                    SessionCommand::RespondPermission { id, decision } => match d.pending.remove(&id) {
                        Some(Pending::Elicitation { rpc_id, form }) => {
                            d.proc.write_line(&jsonrpc::response(&rpc_id, form.answer(&decision))).await
                        }
                        Some(Pending::Permission { rpc_id, options }) => {
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
                        // Withdrawn by the agent while the user answered.
                        None if d.withdrawn.remove(&id) => Ok(()),
                        None => {
                            let _ = events.send(AgentEvent::Notice(format!("no pending permission request {id}"))).await;
                            Ok(())
                        }
                    },
                    // Chosen at spawn: the TUI starts the session again
                    // (`Capabilities::model_per_process`).
                    SessionCommand::SetModel(m) if d.spawning.plan.is_some() => {
                        d.cfg.model = Some(m);
                        let _ = events.send(AgentEvent::Notice(format!(
                            "{harness}'s model is chosen when it starts: it applies from its next start"
                        ))).await;
                        Ok(())
                    }
                    SessionCommand::SetEffort(e) if d.spawning.plan.is_some() => {
                        d.cfg.effort = e;
                        let _ = events.send(AgentEvent::Notice(format!(
                            "{harness}'s effort is chosen when it starts: it applies from its next start"
                        ))).await;
                        Ok(())
                    }
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
                        d.apply_mode(&events).await
                    }
                    SessionCommand::Shutdown => {
                        shutting_down = true;
                        let turn_open = d.queued_turn.is_some()
                            || d.outstanding.values().any(|o| *o == Outstanding::Prompt);
                        // Mid-turn the agent is killed, with nothing sent first.
                        if !turn_open && d.close_session().await {
                            close_by = Some(Instant::now() + CLOSE_WAIT);
                        } else {
                            d.proc.end_or_kill(turn_open).await;
                        }
                        Ok(())
                    }
                };
                if let Err(e) = res {
                    let _ = events.send(AgentEvent::Error(format!("{harness}: {e}"))).await;
                }
            }
            _ = tokio::time::sleep_until(close_by.unwrap_or_else(Instant::now)), if close_by.is_some() => {
                close_by = None;
                d.proc.end();
            }
            raw = d.proc.lines.recv() => {
                match raw {
                    // A process ending to be started again says nothing more.
                    Some(RawLine::Stdout(_) | RawLine::Stderr(_)) if d.spawning.restart.is_some() => {}
                    Some(RawLine::Exited(_)) if d.spawning.restart.is_some() && !shutting_down => {
                        let plan = d.spawning.restart.take().unwrap_or_default();
                        if let Err(e) = d.respawn(plan).await {
                            let _ = events.send(AgentEvent::Error(format!("{harness}: {e}"))).await;
                            let _ = events.send(AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Error(format!("could not start {harness} again: {e}")),
                            }).await;
                            let _ = events.send(AgentEvent::ProcessExited { code: None }).await;
                            return;
                        }
                    }
                    Some(RawLine::Stdout(line)) => {
                        if let Err(e) = on_line(&mut d, &line, &events).await {
                            let _ = events.send(AgentEvent::Error(format!("{harness}: {e}"))).await;
                        }
                        if close_by.is_some() && !d.closing() {
                            close_by = None;
                            d.proc.end();
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
                    d.can_close = can_close(caps);
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
                    d.options = d
                        .parser
                        .session_options(result.get("configOptions"))
                        .unwrap_or_default();
                    if let Some(plan) = d.learn_from_session() {
                        d.restart_with(plan);
                        return Ok(());
                    }
                    d.parser.set_replaying(false);
                    if reattach {
                        d.parser.mark_reattached();
                    }
                    d.agent_modes = AgentModes::from_session(&result, &d.options, reattach);
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
                    d.check_choice(events).await;
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
                (
                    Some(Outstanding::Mode {
                        seq,
                        policy,
                        mode,
                        via,
                    }),
                    error,
                ) => {
                    // An error is the driver's to report: the policy is refused.
                    if error.is_none() {
                        forward(d, line, events).await;
                    }
                    return d
                        .on_mode_answer((seq, policy, mode, via), &result, error.as_ref(), events)
                        .await;
                }
                (Some(Outstanding::Prompt), Some(e)) => {
                    let _ = events
                        .send(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Error(error_text(e)),
                        })
                        .await;
                    return Ok(());
                }
                // The session is over; an error closing it is not news.
                (Some(Outstanding::Close), _) => return Ok(()),
                // A config change answers with the option list as it now stands.
                (Some(Outstanding::Other), None) => {
                    if let Some(o) = d.parser.session_options(result.get("configOptions")) {
                        d.options = o;
                    }
                }
                _ => {}
            }
        }
        Some(RpcMessage::Notification { method, params }) if method == "session/update" => {
            match params
                .pointer("/update/sessionUpdate")
                .and_then(Value::as_str)
            {
                // Keep the option ids current so model and effort changes go
                // to the right one, and the mode known.
                Some("config_option_update") => {
                    if let Some(o) = d
                        .parser
                        .session_options(params.pointer("/update/configOptions"))
                    {
                        d.agent_modes.take_options(&o);
                        d.options = o;
                        forward(d, line, events).await;
                        return d.on_mode_report(events).await;
                    }
                }
                Some("current_mode_update") => {
                    if let Some(mode) = params
                        .pointer("/update/currentModeId")
                        .and_then(Value::as_str)
                    {
                        d.agent_modes.current = Some(mode.to_string());
                        forward(d, line, events).await;
                        return d.on_mode_report(events).await;
                    }
                }
                _ => {}
            }
        }
        Some(RpcMessage::Notification { method, params }) if method == "$/cancel_request" => {
            return d.withdraw(params, events).await;
        }
        Some(RpcMessage::Request { id, method, params }) if method == "elicitation/create" => {
            return d.on_elicitation(line, id, params, events).await;
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
            let (allowed, evs) = screen_permission(&mut d.parser, d.policy, line, params);
            if allowed {
                let allow = PermissionDecision::Allow {
                    updated_input: None,
                };
                d.proc
                    .write_line(&jsonrpc::response(id, permission_outcome(&options, &allow)))
                    .await?;
            } else {
                let rid = request_key(id);
                d.withdrawn.remove(&rid);
                d.pending.insert(
                    rid,
                    Pending::Permission {
                        rpc_id: id.clone(),
                        options,
                    },
                );
            }
            for ev in evs {
                let _ = events.send(ev).await;
            }
            return Ok(());
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

    /// vibe-acp 2.26.1's shapes (`fixtures/vibe_acp_ask.jsonl`): the
    /// request names the call by its id alone.
    #[test]
    fn the_policy_reads_the_kind_of_the_merged_tool_call() {
        use PermissionPolicy::*;
        let tool_call = |id: &str, kind: &str| {
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {
                "sessionUpdate": "tool_call", "toolCallId": id, "kind": kind,
                "status": "in_progress", "title": "t"}}})
            .to_string()
        };
        let request = |call: Value| {
            let line = json!({"jsonrpc": "2.0", "id": 0, "method": "session/request_permission",
                "params": {"toolCall": call, "options": options()}});
            (line.to_string(), line["params"].clone())
        };
        // Whether `policy` answers a request for `call` after a tool_call
        // with id `t` of `kind`, and whether the question is passed on.
        let screen = |policy, kind: &str, call: Value| {
            let mut parser = AcpParser::new(HarnessId::intern("acp-test"));
            parser.feed(&tool_call("t", kind));
            let (line, params) = request(call);
            let (allowed, evs) = screen_permission(&mut parser, policy, &line, &params);
            let asked = evs
                .iter()
                .any(|e| matches!(e, AgentEvent::PermissionRequest(_)));
            assert_ne!(allowed, asked, "asked exactly when not answered");
            allowed
        };
        let id_only = || json!({"toolCallId": "t"});
        assert!(screen(AcceptEdits, "edit", id_only()));
        assert!(screen(Bypass, "edit", id_only()));
        assert!(!screen(Ask, "edit", id_only()));
        assert!(!screen(AcceptEdits, "execute", id_only()));
        assert!(screen(Bypass, "execute", id_only()));
        // A kind in the request replaces the earlier one, as an update's does.
        assert!(!screen(
            AcceptEdits,
            "edit",
            json!({"toolCallId": "t", "kind": "execute"})
        ));
        assert!(screen(
            AcceptEdits,
            "execute",
            json!({"toolCallId": "t", "kind": "edit"})
        ));
        // A request for a call never announced has only its own fields.
        assert!(!screen(AcceptEdits, "edit", json!({"toolCallId": "u"})));
        assert!(screen(
            AcceptEdits,
            "execute",
            json!({"toolCallId": "u", "kind": "edit"})
        ));
        // One without an id does not take what an earlier one without an id said.
        let mut parser = AcpParser::new(HarnessId::intern("acp-test"));
        let (line, params) = request(json!({"kind": "edit"}));
        assert!(screen_permission(&mut parser, AcceptEdits, &line, &params).0);
        let (line, params) = request(json!({}));
        assert!(!screen_permission(&mut parser, AcceptEdits, &line, &params).0);
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
