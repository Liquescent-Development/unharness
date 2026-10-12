//! Long-lived ACP session over stdio: `initialize` → `session/new` (or
//! `session/resume` / `session/load`) → one `session/prompt` per turn →
//! `session/close`, where the agent takes it, answered before stdin closes.
//! A fork (`SessionConfig::fork`) starts with `session/fork` of the stored
//! session instead (see [`Driver::fork`]).
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
//!
//! An interrupt answers the turn's open requests `cancelled` first and
//! sends `session/cancel` once their tool calls have ended (see
//! [`Driver::cancel_turn`]).
//!
//! An agent that wants the user signed in (the spec's auth-required error
//! at a session request or a prompt) gets no `authenticate` and no retry:
//! the turn fails with what the agent offers and what to run (`auth.rs`),
//! and the process is ended, so that the next prompt starts one that reads
//! the sign-in afresh.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::auth::{self, AuthMethod};
use super::elicitation::{self, Form, Refusal};
use super::modes::{AgentModes, ModeMap, ModeStep, SetVia, mode_step, read_back};
use super::parse::{
    AcpParser, RequestEnd, SessionOptions, can_close, can_fork, can_load, can_resume, error_text,
    mcp_http,
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
/// and codex-acp answers its own questions empty (#157). No terminal
/// sign-in either (`auth.terminal`, vibe-acp's `_meta["terminal-auth"]`):
/// unharness runs no command an agent names to sign in (`auth.rs`).
pub fn client_capabilities() -> Value {
    json!({"elicitation": {"form": {}}})
}

/// An agent → client request that waits for the user, with the tool call
/// it is for as the parser tied it (`PermissionRequest::tool_call_id`).
#[derive(Debug)]
enum Pending {
    /// `session/request_permission`, with the options it offered.
    Permission {
        rpc_id: Value,
        options: Value,
        call: Option<String>,
    },
    /// `elicitation/create` in form mode.
    Elicitation {
        rpc_id: Value,
        form: Form,
        call: Option<String>,
    },
}

impl Pending {
    fn rpc_id(&self) -> &Value {
        match self {
            Pending::Permission { rpc_id, .. } | Pending::Elicitation { rpc_id, .. } => rpc_id,
        }
    }

    fn call(&self) -> Option<&String> {
        match self {
            Pending::Permission { call, .. } | Pending::Elicitation { call, .. } => call.as_ref(),
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

/// How long an interrupt's `session/cancel` waits for the tool calls of the
/// requests it answered `cancelled` to end (see [`Driver::cancel_turn`]).
/// vibe-acp 2.26.1 reported its call failed within milliseconds.
pub const SETTLE_WAIT: Duration = Duration::from_secs(2);

/// The tool call a request event is for, as the parser tied it.
fn requested_call(evs: &[AgentEvent]) -> Option<String> {
    evs.iter().find_map(|ev| match ev {
        AgentEvent::PermissionRequest(req) => req.tool_call_id.clone(),
        _ => None,
    })
}

/// A request for `call` was answered or withdrawn: a call only the
/// request named ends here, since the agent reports nothing about it
/// ([`AcpParser::answered`]).
async fn settle(
    parser: &AcpParser,
    call: Option<&String>,
    end: RequestEnd,
    events: &mpsc::Sender<AgentEvent>,
) {
    if let Some(ev) = call.and_then(|c| parser.answered(c, end)) {
        let _ = events.send(ev).await;
    }
}

/// A turn being cancelled: its requests are answered `cancelled`, and the
/// `session/cancel` waits for their calls to end.
struct Cancelling {
    session_id: String,
    /// Calls of answered requests not yet reported `completed` or `failed`.
    calls: HashSet<String>,
    /// When the `session/cancel` goes whatever is left; `None` once sent.
    by: Option<Instant>,
}

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
    /// The command that signs the agent in, told to the user when it is
    /// not (`AcpHarness::signing_in_with`). Never run by unharness.
    pub sign_in: Option<String>,
    /// A relative path in an edit's `diff` is in the session's directory
    /// (`AcpHarness::resolving_relative_edits`).
    pub relative_edits: bool,
    /// Reads what the agent says in fields of its own
    /// (`AcpHarness::reading_meta_with`).
    pub meta_reader: Option<super::parse::MetaReaderFactory>,
    /// The extension method that rewinds the session to a user message
    /// (`AcpHarness::rewinding_with`), or none.
    pub rewind: Option<&'static str>,
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
        provider: cfg.provider.as_ref().map(|p| p.as_str()),
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
    let parser = AcpParser::new(harness)
        .declaring_effort(spec.effort_option.clone())
        .resolving_relative_edits(spec.relative_edits.then(|| cfg.cwd.clone()))
        .reading_meta(spec.meta_reader.map(|make| make()))
        .anchoring_turns(spec.rewind.is_some());
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
    /// `session/fork` of the stored session; `retried` once it was
    /// reattached for the agent to fork it.
    Fork {
        retried: bool,
    },
    /// The stored session reattached so that the agent can fork it.
    ForkSource,
    Prompt,
    /// The extension request that rewinds the session; turns wait for it.
    Rewind,
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
    /// How the agent reattaches a stored session: `session/resume`, else
    /// `session/load`, or neither.
    reattach_via: Option<&'static str>,
    /// The sign-in methods the agent offered at `initialize`.
    auth_methods: Vec<AuthMethod>,
    /// The agent wants the user signed in: the process is to end.
    signed_out: bool,
    /// The running turn is being cancelled, until its prompt answers.
    cancelling: Option<Cancelling>,
    /// A rewind to this message, to go before the next turn.
    queued_rewind: Option<String>,
    /// A turn was sent and its `TurnAnchor` has not been reported (only
    /// for an agent that rewinds): one goes for every turn, empty for one
    /// that never reached the agent or whose message was not echoed.
    anchor_owed: bool,
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
        // A rewind goes first, and the turn waits for its answer: the
        // agent refuses one while a turn runs, and a turn sent beside it
        // would land in the history it cuts.
        if let (Some(anchor), Some(method)) = (self.queued_rewind.take(), self.spawning.spec.rewind)
        {
            // Files are unharness's to restore, from its checkpoints.
            let params =
                json!({"sessionId": session_id, "messageId": anchor, "restoreFiles": false});
            return self.request(method, params, Outstanding::Rewind).await;
        }
        if self.outstanding.values().any(|o| *o == Outstanding::Rewind) {
            return Ok(());
        }
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
            self.cancel_turn(events).await?;
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
            self.cancel_turn(events).await?;
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
        settle(&self.parser, pending.call(), RequestEnd::Withdrawn, events).await;
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
                let evs = self.parser.feed(line);
                let pending = Pending::Elicitation {
                    rpc_id: id.clone(),
                    form,
                    call: requested_call(&evs),
                };
                if self.cancelling.is_some() {
                    return self.refuse_late(pending, events).await;
                }
                let rid = request_key(id);
                self.withdrawn.remove(&rid);
                self.pending.insert(rid, pending);
                for ev in evs {
                    let _ = events.send(ev).await;
                }
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
        // On a provider of the harness's, with none of its models chosen
        // (an agent profile or admin configuration can name another).
        if self.cfg.model.is_none()
            && let Some(p) = &plan.provider
            && let Some(current) = &self.options.current_model
            && !p.models.contains(current)
        {
            let _ = events
                .send(AgentEvent::Notice(format!(
                    "{name} runs model '{current}', which is not one of provider {}'s ({})",
                    p.provider,
                    p.models.join(", ")
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

    /// Branch the stored session `source` into a new one (`session/fork`,
    /// unstable in ACP v1). Agents differ in what they need around it:
    /// vibe-acp 2.26.1 forks only a session it has open ("Session not
    /// found" otherwise), so on an error the source is reattached and
    /// forked again; codex-acp 2.1.1 leaves the fork closed (its prompt was
    /// never answered) and claude-agent-acp 0.85.1 forks the stored files,
    /// so an agent that resumes sessions has the fork resumed before
    /// anything goes to it. The source is never sent a prompt, a mode or a
    /// config change.
    async fn fork(&mut self, source: &str, retried: bool) -> Result<()> {
        let params =
            json!({"sessionId": source, "cwd": self.cfg.cwd, "mcpServers": self.mcp_servers});
        self.request("session/fork", params, Outstanding::Fork { retried })
            .await
    }

    /// The answer to `session/fork`.
    async fn on_fork_answer(
        &mut self,
        line: &str,
        result: &Value,
        error: Option<&Value>,
        retried: bool,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        let source = self.cfg.resume.clone().unwrap_or_default();
        let fork = result.get("sessionId").and_then(Value::as_str);
        let Some(fork) = fork.filter(|_| error.is_none()) else {
            let why = error.map_or_else(|| "it named no session".to_string(), error_text);
            return match self.reattach_via {
                Some(method) if !retried => {
                    if method == "session/load" {
                        self.parser.set_replaying(true);
                    }
                    let params = json!({
                        "sessionId": source, "cwd": self.cfg.cwd, "mcpServers": self.mcp_servers,
                    });
                    self.request(method, params, Outstanding::ForkSource).await
                }
                _ => self.fork_failed(&why, events).await,
            };
        };
        if self.reattach_via == Some("session/resume") {
            // The session reattached from here on is the fork: its id is
            // announced with the answer.
            self.cfg.resume = Some(fork.to_string());
            let params =
                json!({"sessionId": fork, "cwd": self.cfg.cwd, "mcpServers": self.mcp_servers});
            return self
                .request(
                    "session/resume",
                    params,
                    Outstanding::Session { reattach: true },
                )
                .await;
        }
        // The fork is open, and its answer is a new session's.
        self.on_session_answer(line, result, false, events).await
    }

    /// The agent answered a session request (new, load, resume, fork, or
    /// the reattach before a fork) or a prompt with the spec's
    /// auth-required error. Nothing is retried and no new session is
    /// started in its place (one for a resume or a fork would not know the
    /// conversation, and would fail the same way): the turn fails with
    /// what the agent offers and what to run, and the process is ended
    /// (`signed_out`), so that the next prompt starts one that reads the
    /// sign-in afresh.
    async fn not_signed_in(&mut self, error: &Value, events: &mpsc::Sender<AgentEvent>) {
        self.parser.set_replaying(false);
        self.queued_turn = None;
        let why = auth::not_signed_in(
            self.harness.as_str(),
            error,
            &self.auth_methods,
            self.spawning.spec.sign_in.as_deref(),
        );
        let _ = events
            .send(AgentEvent::TurnCompleted {
                stop_reason: StopReason::Error(why),
            })
            .await;
        self.signed_out = true;
    }

    /// The source could not be forked: a new session rather than the
    /// source itself, reported as one (`replaced`).
    async fn fork_failed(&mut self, why: &str, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        self.replaced(format!("could not fork the session ({why})"), events)
            .await
    }

    /// The session could not be resumed or forked: a new one, which knows
    /// nothing of the conversation, is started in its place, and the TUI
    /// is told so (`SessionReplaced`), so that it sends the conversation
    /// with the next prompt as it does to any new session. An agent not
    /// signed in gets none (`not_signed_in`).
    async fn replaced(&mut self, reason: String, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        self.parser.set_replaying(false);
        // Nothing is resumed from here on, also by a process started again
        // for its spawn plan (`restart_with`).
        self.cfg.resume = None;
        self.cfg.fork = false;
        let _ = events.send(AgentEvent::SessionReplaced { reason }).await;
        self.new_session().await
    }

    /// A session answer (new, reattached, or a fork's): take its options
    /// and modes, then hold it to the policy's mode (`session_ready`).
    async fn on_session_answer(
        &mut self,
        line: &str,
        result: &Value,
        reattach: bool,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        self.options = self
            .parser
            .session_options(result.get("configOptions"))
            .unwrap_or_default();
        if let Some(plan) = self.learn_from_session() {
            self.restart_with(plan);
            return Ok(());
        }
        self.parser.set_replaying(false);
        if reattach {
            self.parser.mark_reattached();
        }
        self.agent_modes = AgentModes::from_session(result, &self.options, reattach);
        self.session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        if reattach && self.session_id.is_none() {
            // The spec's reattach result does not repeat the
            // session id (some agents add it): announce it here.
            let session_id = self.cfg.resume.clone().unwrap_or_default();
            self.session_id = Some(session_id.clone());
            let _ = events
                .send(AgentEvent::SessionStarted {
                    session_id,
                    model: self.options.current_model.clone(),
                })
                .await;
            let mut update = self.options.caps_update(self.harness);
            if let Some(plan) = &self.spawning.plan {
                plan.restrict(&mut update);
            }
            let _ = events.send(AgentEvent::CapabilitiesChanged(update)).await;
        }
        forward(self, line, events).await;
        self.check_choice(events).await;
        self.session_ready(events).await
    }

    fn closing(&self) -> bool {
        self.outstanding.values().any(|o| *o == Outstanding::Close)
    }

    /// Cancel the running turn: every open permission request answered
    /// `cancelled`, as the spec requires of a cancelled turn, and an open
    /// elicitation `cancel` (the TUI is told each was withdrawn), then
    /// `session/cancel` once the tool calls they were for are reported
    /// `completed` or `failed`, or after [`SETTLE_WAIT`]. Until the prompt
    /// answers, a request that comes is answered the same way.
    ///
    /// The other order breaks vibe-acp 2.26.1: its interrupt and the
    /// answer each resolve the request's callback in its session store,
    /// whose next read then fails ("callback result has no pending
    /// registration") and the prompt is never answered; the answer sent
    /// just before the cancel races the same way. Answered first, the call
    /// fails at once, and the cancel then ends the turn `cancelled`
    /// (`fixtures/vibe_acp_interrupt.jsonl`); claude-agent-acp 0.85.1 and
    /// codex-acp 2.1.1 did the same.
    async fn cancel_turn(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        // Asked again while it waits: the cancel goes now.
        if self.cancelling.as_ref().is_some_and(|c| c.by.is_some()) {
            return self.send_cancel().await;
        }
        let mut res = Ok(());
        let mut calls = HashSet::new();
        let mut untied = false;
        for (id, pending) in self.pending.drain() {
            match pending.call() {
                Some(call) => {
                    calls.insert(call.clone());
                }
                None => untied = true,
            }
            let line = jsonrpc::response(pending.rpc_id(), pending.cancelled());
            res = res.and(self.proc.write_line(&line).await);
            let _ = events.send(AgentEvent::PermissionWithdrawn { id }).await;
            settle(&self.parser, pending.call(), RequestEnd::Cancelled, events).await;
        }
        let turn_running = self.outstanding.values().any(|o| *o == Outstanding::Prompt);
        if !turn_running {
            return res.and(self.cancel_now(&session_id).await);
        }
        let waits = untied || !calls.is_empty();
        self.cancelling = Some(Cancelling {
            session_id,
            calls,
            by: Some(Instant::now() + SETTLE_WAIT),
        });
        if !waits {
            return res.and(self.send_cancel().await);
        }
        res
    }

    async fn cancel_now(&mut self, session_id: &str) -> Result<()> {
        self.proc
            .write_line(&jsonrpc::notification(
                "session/cancel",
                json!({"sessionId": session_id}),
            ))
            .await
    }

    /// The `session/cancel` a cancelled turn owes, once.
    async fn send_cancel(&mut self) -> Result<()> {
        let Some(c) = self.cancelling.as_mut() else {
            return Ok(());
        };
        if c.by.take().is_none() {
            return Ok(());
        }
        c.calls.clear();
        let session_id = c.session_id.clone();
        self.cancel_now(&session_id).await
    }

    /// When the owed `session/cancel` goes whatever is left.
    fn cancel_by(&self) -> Option<Instant> {
        self.cancelling.as_ref().and_then(|c| c.by)
    }

    /// A tool call was reported: one the owed `session/cancel` waits for
    /// has ended.
    async fn call_reported(&mut self, update: &Value) -> Result<()> {
        let Some(c) = self.cancelling.as_mut() else {
            return Ok(());
        };
        let ended = matches!(
            update.get("status").and_then(Value::as_str),
            Some("completed" | "failed")
        );
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            return Ok(());
        };
        if ended && c.calls.remove(id) && c.calls.is_empty() {
            return self.send_cancel().await;
        }
        Ok(())
    }

    /// Report the anchor still owed for a turn that is over, or that will
    /// not be sent, as empty (`AgentEvent::TurnAnchor`).
    async fn settle_anchor(&mut self, events: &mpsc::Sender<AgentEvent>) {
        let turn_open = self.queued_turn.is_some()
            || self.outstanding.values().any(|o| *o == Outstanding::Prompt);
        if self.anchor_owed && !turn_open {
            self.anchor_owed = false;
            let _ = events
                .send(AgentEvent::TurnAnchor { id: String::new() })
                .await;
        }
    }

    /// A request that comes while the turn is being cancelled is answered
    /// at once, as the open ones were, and its call waited for.
    async fn refuse_late(
        &mut self,
        pending: Pending,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        if let Some(c) = self.cancelling.as_mut()
            && c.by.is_some()
            && let Some(call) = pending.call()
        {
            c.calls.insert(call.clone());
        }
        let res = self
            .proc
            .write_line(&jsonrpc::response(pending.rpc_id(), pending.cancelled()))
            .await;
        settle(&self.parser, pending.call(), RequestEnd::Cancelled, events).await;
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
        reattach_via: None,
        auth_methods: Vec::new(),
        signed_out: false,
        cancelling: None,
        queued_rewind: None,
        anchor_owed: false,
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
                        d.anchor_owed = d.spawning.spec.rewind.is_some();
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
                    // Sent once the session is there, before the next turn.
                    SessionCommand::Rewind { anchor } if d.spawning.spec.rewind.is_some() => {
                        d.queued_rewind = Some(anchor);
                        d.flush_turn(&events).await
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
                            d.cancel_turn(&events).await
                        }
                    }
                    SessionCommand::RespondPermission { id, decision } => match d.pending.remove(&id) {
                        Some(Pending::Elicitation { rpc_id, form, .. }) => {
                            d.proc.write_line(&jsonrpc::response(&rpc_id, form.answer(&decision))).await
                        }
                        Some(Pending::Permission { rpc_id, options, call }) => {
                            let outcome = permission_outcome(&options, &decision);
                            if matches!(decision, PermissionDecision::Allow { .. })
                                && outcome.pointer("/outcome/outcome") == Some(&json!("cancelled"))
                            {
                                let _ = events.send(AgentEvent::Notice(
                                    "the agent offered no way to allow this just once; the request was cancelled".into(),
                                )).await;
                            }
                            let end = RequestEnd::of(&outcome, &options);
                            let res = d.proc.write_line(&jsonrpc::response(&rpc_id, outcome)).await;
                            settle(&d.parser, call.as_ref(), end, &events).await;
                            res
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
            _ = tokio::time::sleep_until(d.cancel_by().unwrap_or_else(Instant::now)), if d.cancel_by().is_some() => {
                if let Err(e) = d.send_cancel().await {
                    let _ = events.send(AgentEvent::Error(format!("{harness}: {e}"))).await;
                }
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
                        // Not signed in: nothing more goes to this process,
                        // and what it says on the way out is not news.
                        if d.signed_out && !shutting_down {
                            shutting_down = true;
                            close_by = None;
                            d.proc.end();
                            continue;
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
        d.settle_anchor(&events).await;
    }
}

/// Handle one stdout line: driver bookkeeping first, then the parser's events.
async fn on_line(d: &mut Driver, line: &str, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
    let msg = RpcMessage::parse(line);
    match &msg {
        Some(RpcMessage::Response { id, result, error }) => {
            let kind = id.as_u64().and_then(|i| d.outstanding.remove(&i));
            if kind == Some(Outstanding::Prompt) {
                d.cancelling = None;
            }
            let result = result.clone().unwrap_or(Value::Null);
            if let Some(e) = error
                && auth::is_auth_required(e)
                && matches!(
                    kind,
                    Some(
                        Outstanding::Session { .. }
                            | Outstanding::Fork { .. }
                            | Outstanding::ForkSource
                            | Outstanding::Prompt
                    )
                )
            {
                d.not_signed_in(e, events).await;
                return Ok(());
            }
            match (kind, error) {
                (Some(Outstanding::Initialize), None) => {
                    d.auth_methods = auth::auth_methods(&result);
                    let caps = result.get("agentCapabilities").unwrap_or(&Value::Null);
                    d.can_close = can_close(caps);
                    d.reattach_via = if can_resume(caps) {
                        Some("session/resume")
                    } else if can_load(caps) {
                        Some("session/load")
                    } else {
                        None
                    };
                    let (servers, dropped) =
                        mcp_servers_param(&d.cfg.mcp_servers, mcp_http(caps), &d.cfg.cwd);
                    d.mcp_servers = servers;
                    if !dropped.is_empty() {
                        let warning = mcp::not_http(d.harness.as_str(), &dropped.join(", "));
                        let _ = events.send(AgentEvent::Notice(warning)).await;
                    }
                    match d.cfg.resume.clone() {
                        // The stored session is the original's: it is forked,
                        // never reattached and written to.
                        Some(id) if d.cfg.fork && can_fork(caps) => d.fork(&id, false).await?,
                        Some(_) if d.cfg.fork => {
                            d.replaced("this agent cannot fork sessions".into(), events)
                                .await?;
                        }
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
                            d.replaced("this agent cannot resume sessions".into(), events)
                                .await?;
                        }
                        None => d.new_session().await?,
                    }
                }
                (Some(Outstanding::Session { reattach }), None) => {
                    return d.on_session_answer(line, &result, reattach, events).await;
                }
                (Some(Outstanding::Fork { retried }), error) => {
                    return d
                        .on_fork_answer(line, &result, error.as_ref(), retried, events)
                        .await;
                }
                // The source is open now: fork it again. Its answer (its
                // modes, its options) is not this session's.
                (Some(Outstanding::ForkSource), None) => {
                    d.parser.set_replaying(false);
                    let source = d.cfg.resume.clone().unwrap_or_default();
                    return d.fork(&source, true).await;
                }
                (Some(Outstanding::ForkSource), Some(e)) => {
                    return d.fork_failed(&error_text(e), events).await;
                }
                (Some(Outstanding::Session { reattach: true }), Some(e)) => {
                    let reason = format!("could not resume the session ({})", error_text(e));
                    return d.replaced(reason, events).await;
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
                // Either way the turn that waited goes now: after a failed
                // rewind into the session as it was, which the user is told.
                (Some(Outstanding::Rewind), error) => {
                    if let Some(e) = error {
                        let _ = events
                            .send(AgentEvent::RewindFailed {
                                reason: error_text(e),
                            })
                            .await;
                    }
                    return d.flush_turn(events).await;
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
                // A call an owed `session/cancel` waits for may have ended.
                Some("tool_call" | "tool_call_update") => {
                    forward(d, line, events).await;
                    let update = params.get("update").unwrap_or(&Value::Null);
                    return d.call_reported(update).await;
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
            if d.cancelling.is_some() {
                let pending = Pending::Permission {
                    rpc_id: id.clone(),
                    call: requested_call(&evs),
                    options,
                };
                for ev in evs {
                    if !matches!(ev, AgentEvent::PermissionRequest(_)) {
                        let _ = events.send(ev).await;
                    }
                }
                return d.refuse_late(pending, events).await;
            }
            if allowed {
                let allow = PermissionDecision::Allow {
                    updated_input: None,
                };
                let answer = permission_outcome(&options, &allow);
                d.proc
                    .write_line(&jsonrpc::response(id, answer.clone()))
                    .await?;
                for ev in evs {
                    let _ = events.send(ev).await;
                }
                // The question was left out of `evs`: the call is the request's.
                let call = params
                    .pointer("/toolCall/toolCallId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                settle(
                    &d.parser,
                    call.as_ref(),
                    RequestEnd::of(&answer, &options),
                    events,
                )
                .await;
                return Ok(());
            }
            let rid = request_key(id);
            d.withdrawn.remove(&rid);
            d.pending.insert(
                rid,
                Pending::Permission {
                    rpc_id: id.clone(),
                    call: requested_call(&evs),
                    options,
                },
            );
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
    for mut ev in d.parser.feed(line) {
        // One anchor per turn sent: a message the agent echoes outside
        // one is not a turn of ours.
        if matches!(ev, AgentEvent::TurnAnchor { .. }) && !std::mem::take(&mut d.anchor_owed) {
            continue;
        }
        // On a provider in the agent's environment, only its models.
        if let (AgentEvent::CapabilitiesChanged(update), Some(plan)) = (&mut ev, &d.spawning.plan) {
            plan.restrict(update);
        }
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
