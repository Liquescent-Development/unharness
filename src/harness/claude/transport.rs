//! Long-lived Claude Code session over `--input-format stream-json`.
//!
//! One `claude -p` process per session. Turns are written to stdin as
//! `{"type":"user",...}` lines; permission prompts arrive as
//! `control_request` lines (because we pass `--permission-prompt-tool stdio`)
//! and are answered with `control_response` lines.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::Instant;

use super::parse::ClaudeParser;
use crate::core::driver::{self, Encoded, Gone, Io, Shutdown, StreamProtocol};
use crate::core::{
    AgentEvent, Attachment, Capabilities, CapsUpdate, HarnessId, McpServer, McpTransport, Parser,
    PermissionDecision, PermissionKind, PermissionPolicy, ProviderId, SessionCommand,
    SessionConfig, SessionHandle, StopReason,
};

/// Flags for a permission policy. `Ask` maps to Claude's default mode with
/// prompts routed to us over stdio.
pub fn policy_args(policy: PermissionPolicy) -> Vec<&'static str> {
    match policy {
        PermissionPolicy::Plan => vec!["--permission-mode", "plan"],
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
        PermissionPolicy::Plan => "plan",
        PermissionPolicy::Ask => "default",
        PermissionPolicy::AcceptEdits => "acceptEdits",
        PermissionPolicy::Auto => "auto",
        PermissionPolicy::Bypass => "bypassPermissions",
    }
}

/// Write the servers to a file only the user can read, under `dir`, and
/// return its path for `--mcp-config`. A file, not the JSON itself, because
/// `env` and `headers` hold secrets and a command line is readable by every
/// local user. The name follows the content, so sessions with the same
/// servers share a file and none is rewritten under a running Claude.
pub fn mcp_config_file(servers: &[McpServer], dir: &Path) -> Result<Option<PathBuf>> {
    let Some(config) = mcp_config(servers) else {
        return Ok(None);
    };
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.hash(&mut hasher);
    let path = dir.join(format!("claude-{:016x}.json", hasher.finish()));
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == config) {
        return Ok(Some(path));
    }
    let partial = dir.join(format!("claude-{}.partial", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    std::io::Write::write_all(&mut options.open(&partial)?, config.as_bytes())?;
    std::fs::rename(&partial, &path)?;
    Ok(Some(path))
}

/// The servers in Claude's own JSON shape (`--mcp-config`). They are added
/// to the servers Claude already has from its configuration.
pub fn mcp_config(servers: &[McpServer]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let servers: serde_json::Map<String, Value> = servers
        .iter()
        .map(|s| {
            let definition = match &s.transport {
                McpTransport::Stdio { command, args, env } => {
                    json!({"type": "stdio", "command": command, "args": args, "env": env})
                }
                McpTransport::Http { url, headers } => {
                    json!({"type": "http", "url": url, "headers": headers})
                }
            };
            (s.name.clone(), definition)
        })
        .collect();
    Some(json!({"mcpServers": servers}).to_string())
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
        "--include-hook-events",
        // Shows a prompt typed on Remote Control; ours are echoed too.
        "--replay-user-messages",
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
    kind: PendingKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Tool,
    Question,
    /// `ExitPlanMode`: only an `Allow` approves the plan.
    Plan,
}

/// A plan the user did not approve, worded as Claude's own interface words
/// it (2.1.296): with what the user said, or told to wait without.
const PLAN_REJECTED: &str = "The user doesn't want to proceed with this tool use. The tool use \
     was rejected (eg. if it was a file edit, the new_string was NOT written to the file).";

fn plan_rejected(feedback: &str) -> String {
    if feedback.trim().is_empty() {
        format!(
            "{PLAN_REJECTED} STOP what you are doing and wait for the user to tell you how to proceed."
        )
    } else {
        format!("{PLAN_REJECTED} To tell you how to proceed, the user said:\n{feedback}")
    }
}

pub fn encode_decision(pending: &PendingPermission, decision: &PermissionDecision) -> Value {
    match decision {
        PermissionDecision::Allow { updated_input } => json!({
            "behavior": "allow",
            "updatedInput": updated_input.clone().unwrap_or_else(|| pending.input.clone()),
        }),
        PermissionDecision::Deny { reason } if pending.kind == PendingKind::Plan => {
            json!({"behavior":"deny","message": plan_rejected(reason)})
        }
        PermissionDecision::Deny { reason } => json!({"behavior":"deny","message": reason}),
        PermissionDecision::Answer(_) if pending.kind == PendingKind::Plan => {
            json!({"behavior":"deny","message": plan_rejected("")})
        }
        PermissionDecision::Answer(answer) => {
            if pending.kind == PendingKind::Question {
                if answer.is_null() {
                    json!({"behavior":"deny","message":"User dismissed the question"})
                } else {
                    let mut input = pending.input.clone();
                    input["answers"] = claude_answers(answer);
                    json!({"behavior":"allow","updatedInput": input})
                }
            } else {
                json!({"behavior":"allow","updatedInput": pending.input.clone()})
            }
        }
    }
}

/// Claude's `answers` take a string per question. A multi-select array
/// sent as it is reaches the model as its labels joined by a bare comma
/// (2.1.293 writes the answer with a template literal), so it is joined the
/// way Claude joins one itself: by `", "`, a label containing `", "` or `"`
/// written as a JSON string.
fn claude_answers(answer: &Value) -> Value {
    let Some(map) = answer.as_object() else {
        return answer.clone();
    };
    let joined = map.iter().map(|(question, v)| {
        let v = match v.as_array() {
            Some(labels) => Value::String(
                labels
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|l| {
                        if l.contains(", ") || l.contains('"') {
                            Value::from(l).to_string()
                        } else {
                            l.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            None => v.clone(),
        };
        (question.clone(), v)
    });
    Value::Object(joined.collect())
}

pub fn control_response(request_id: &str, response: Value) -> String {
    json!({
        "type":"control_response",
        "response":{"subtype":"success","request_id":request_id,"response":response}
    })
    .to_string()
}

/// A parent Claude Code session's identity, never inherited.
pub const PARENT_SESSION_VARS: [&str; 4] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
];

/// `mcp_config` is the file written by [`mcp_config_file`], if any; `caps`
/// is what the harness declares.
pub fn start(
    cfg: SessionConfig,
    mcp_config: Option<PathBuf>,
    caps: &Capabilities,
) -> Result<SessionHandle> {
    let cmd = session_command(&cfg, mcp_config.as_deref())?;
    let sandbox = cfg.sandbox.clone();
    driver::start(
        HarnessId::CLAUDE,
        caps,
        cmd,
        &sandbox,
        Claude::new(cfg, mcp_config),
    )
}

/// The process of a session for `cfg`, with the servers in `mcp_config`.
fn session_command(cfg: &SessionConfig, mcp_config: Option<&Path>) -> Result<Command> {
    let mut cmd = Command::new(&cfg.binary);
    cmd.args(session_args(cfg)).current_dir(&cfg.cwd);
    // Last: the flag takes every word up to the next flag.
    if let Some(path) = mcp_config {
        cmd.arg("--mcp-config").arg(path);
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    for var in PARENT_SESSION_VARS {
        cmd.env_remove(var);
    }
    if let Some(provider) = &cfg.provider {
        for (key, value) in super::provider_env(provider)? {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
    }
    Ok(cmd)
}

/// The control request that puts the session on Remote Control, or takes
/// it off: `(request id, line)`.
fn remote_control_request(enabled: bool, name: Option<String>) -> (String, String) {
    let mut extra = json!({"enabled": enabled});
    if let Some(name) = name.filter(|_| enabled) {
        extra["name"] = json!(name);
    }
    control_request("remote_control", extra)
}

/// A prompt sent at once (`SessionCommand::SendNow`) that the CLI has not
/// started on yet. Its `TurnAnchor` is told when it does: where that
/// falls tells the controller whether the prompt went into the running
/// turn (before that turn's end) or starts a turn of its own (after it).
#[derive(Debug)]
struct SentNow {
    /// Its uuid, which the CLI's `command_lifecycle` lines name.
    uuid: String,
    /// The CLI says when it starts on a queued message
    /// (`msg_lifecycle_v1`): the anchor waits for that. Otherwise it is
    /// told when the turn the prompt stops has ended, since a plain
    /// interrupt always ends that turn.
    lifecycle: bool,
    /// The turn it stops has ended (its `result` came).
    stopped: bool,
    /// It will not run, for this reason: its turn is told failed once the
    /// turn it was to stop has ended.
    failed: Option<String>,
}

impl SentNow {
    /// What the controller is told of a prompt that will not run: its
    /// turn, begun when the one before ended, failed.
    fn failed(why: String) -> Vec<AgentEvent> {
        vec![
            AgentEvent::TurnAnchor { id: String::new() },
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::Error(why),
            },
        ]
    }
}

/// How a turn's error starts that Claude ends when it is interrupted.
const STOPPED: &str = "error_during_execution";

/// How long the answer to `initialize` is waited for before the session
/// goes on without it.
const INIT_WAIT: Duration = Duration::from_secs(5);

/// What the driver does with Claude's stream-json.
struct Claude {
    parser: ClaudeParser,
    pending: HashMap<String, PendingPermission>,
    /// Our `remote_control` requests: whether each turns it on.
    remote_requests: HashMap<String, bool>,
    /// Our `get_usage` requests (`/status`) not answered yet.
    usage_requests: HashSet<String>,
    /// Our `initialize` request while it is not answered, and until when
    /// that is waited for.
    awaiting_init: Option<(String, Instant)>,
    /// What the process is started again with for a new provider: the
    /// session's configuration, kept as the session changed it since
    /// (model, effort, policy, the session to resume).
    cfg: SessionConfig,
    mcp_config: Option<PathBuf>,
    /// The session the process runs, as it last said.
    session_id: Option<String>,
    /// On Remote Control, under this name: the process started again is
    /// put back on it.
    remote: Option<Option<String>>,
    /// Policies set and not yet reported back: a report of one of them is
    /// not a change of Claude's own.
    policies_sent: VecDeque<PermissionPolicy>,
    /// The prompt sent at once that the CLI has not started on yet.
    sent_now: Option<SentNow>,
    /// An interrupt went since the last turn's end: the turn it stops ends
    /// with an `error_during_execution` result, which is that interrupt.
    interrupting: bool,
    /// Whether the TUI was told that a prompt sent at once cannot be taken
    /// back on this CLI (`take_back_sent_now`).
    told_take_back: bool,
}

impl Claude {
    fn new(cfg: SessionConfig, mcp_config: Option<PathBuf>) -> Self {
        Claude {
            parser: ClaudeParser::new(),
            pending: HashMap::new(),
            remote_requests: HashMap::new(),
            usage_requests: HashSet::new(),
            awaiting_init: None,
            session_id: cfg.resume.clone().filter(|_| !cfg.fork),
            cfg,
            mcp_config,
            remote: None,
            policies_sent: VecDeque::new(),
            sent_now: None,
            interrupting: false,
            told_take_back: false,
        }
    }

    /// Send `text` at once. Where the CLI advertises both, the message
    /// goes first and then `interrupt` with `send_now` and its uuid: the
    /// CLI stops the running turn for it, or moves what the turn waits on
    /// to the background and takes it into the turn, and says which with
    /// `command_lifecycle` (2.1.301; the issue's research found `stopped`
    /// for a foreground command). Otherwise a plain interrupt, which ends
    /// the turn, and the message right after it, which is the next turn.
    /// Not `"priority":"now"`: that waits for a foreground tool to finish.
    async fn send_now(&mut self, text: &str, attachments: &[Attachment], io: &mut Io<'_>) {
        let (uuid, message) = match anchored_turn(text, attachments) {
            Ok(turn) => turn,
            // Nothing of it reaches the CLI: the turn is stopped all the
            // same, and the prompt's own turn fails once that one ended.
            Err(e) => {
                self.sent_now = Some(SentNow {
                    uuid: String::new(),
                    lifecycle: false,
                    stopped: false,
                    failed: Some(e.to_string()),
                });
                self.write(&control_request("interrupt", json!({})).1, io)
                    .await;
                return;
            }
        };
        let lifecycle = self.parser.advertises("msg_lifecycle_v1");
        let lines = if lifecycle && self.parser.advertises("interrupt_send_now_v1") {
            let interrupt = json!({"send_now": true, "message_uuid": uuid});
            [message, control_request("interrupt", interrupt).1]
        } else {
            [control_request("interrupt", json!({})).1, message]
        };
        self.sent_now = Some(SentNow {
            uuid,
            lifecycle,
            stopped: false,
            failed: None,
        });
        for line in lines {
            if !self.write(&line, io).await {
                break;
            }
        }
    }

    /// Write `line`; a failure is told as `write_failed` words it.
    async fn write(&mut self, line: &str, io: &mut Io<'_>) -> bool {
        match io.write(line).await {
            Ok(()) => true,
            Err(e) => {
                for ev in self.write_failed(e) {
                    io.emit(ev).await;
                }
                false
            }
        }
    }

    /// What a `command_lifecycle` line says of the prompt sent at once:
    /// the CLI started on it (its anchor), or will not run it.
    fn lifecycle(&mut self, line: &Value) -> Vec<AgentEvent> {
        let Some(sent) = self.sent_now.as_mut() else {
            return Vec::new();
        };
        if line.get("command_uuid").and_then(Value::as_str) != Some(sent.uuid.as_str()) {
            return Vec::new();
        }
        match line.get("state").and_then(Value::as_str) {
            Some("started") => {
                let id = std::mem::take(&mut sent.uuid);
                self.sent_now = None;
                vec![AgentEvent::TurnAnchor { id }]
            }
            Some(state @ ("refused" | "discarded" | "cancelled")) => {
                let why = format!("claude did not run the prompt sent at once ({state})");
                if sent.stopped {
                    self.sent_now = None;
                    SentNow::failed(why)
                } else {
                    sent.failed = Some(why);
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// The running turn ended: what follows of the prompt sent at once.
    /// Without `command_lifecycle` the next turn is the prompt's, and so
    /// is a turn that ends before the CLI said it started on the prompt
    /// (the turn it was to stop had ended by itself).
    fn turn_ended(&mut self) -> Vec<AgentEvent> {
        let Some(sent) = self.sent_now.as_mut() else {
            return Vec::new();
        };
        if let Some(why) = sent.failed.take() {
            self.sent_now = None;
            return SentNow::failed(why);
        }
        if sent.lifecycle && !sent.stopped {
            sent.stopped = true;
            return Vec::new();
        }
        let id = std::mem::take(&mut sent.uuid);
        self.sent_now = None;
        vec![AgentEvent::TurnAnchor { id }]
    }

    /// What the session said that a process started again for it needs.
    fn keep(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::SessionStarted { session_id, .. } => {
                self.session_id = Some(session_id.clone());
            }
            AgentEvent::PolicyChanged(p) => match self.policies_sent.iter().position(|s| s == p) {
                Some(at) => {
                    self.policies_sent.drain(..=at);
                }
                // Its own (plan mode entered or left).
                None => self.cfg.policy = *p,
            },
            AgentEvent::RemoteControl(crate::core::RemoteControl::Failed { .. }) => {
                self.remote = None;
            }
            _ => {}
        }
    }

    /// The provider is an environment variable of Claude's process: it is
    /// started again on `provider`, resuming the session, which keeps its
    /// conversation (2.1.296). Ended as a shutdown ends it (`interrupt`,
    /// then its input closed), as the TUI did before this was the driver's.
    fn restart_on(&mut self, provider: ProviderId) -> Result<Command> {
        self.cfg.provider = Some(provider);
        // A model is one of its provider's.
        self.cfg.model = None;
        if let Some(id) = &self.session_id {
            self.cfg.resume = Some(id.clone());
            self.cfg.fork = false;
        }
        session_command(&self.cfg, self.mcp_config.as_deref())
    }
}

impl StreamProtocol for Claude {
    /// Handshake. The CLI answers with its command/model catalog; we only
    /// need the ack. Not required for turns to work, so failure is a notice.
    async fn start(&mut self, io: &mut Io<'_>) {
        let (init_id, init_line) = control_request("initialize", json!({}));
        if let Err(e) = io.write(&init_line).await {
            io.emit(AgentEvent::Error(format!("claude: {e}"))).await;
        }
        self.awaiting_init = Some((init_id, Instant::now() + INIT_WAIT));
    }

    fn deadline(&self) -> Option<Instant> {
        self.awaiting_init.as_ref().map(|(_, by)| *by)
    }

    async fn on_deadline(&mut self, io: &mut Io<'_>) {
        self.awaiting_init = None;
        io.emit(AgentEvent::Notice(
            "claude: no initialize ack; continuing".into(),
        ))
        .await;
    }

    async fn encode(&mut self, cmd: SessionCommand, io: &mut Io<'_>) -> Encoded {
        match cmd {
            // Each turn carries a uuid of ours, so it can be rewound to later.
            SessionCommand::SendTurn { text, attachments } => {
                match anchored_turn(&text, &attachments) {
                    Ok((anchor, line)) => {
                        io.emit(AgentEvent::TurnAnchor { id: anchor }).await;
                        Encoded::Write(line)
                    }
                    Err(e) => {
                        io.emit(AgentEvent::TurnCompleted {
                            stop_reason: StopReason::Error(e.to_string()),
                        })
                        .await;
                        Encoded::Done
                    }
                }
            }
            SessionCommand::Steer { text, attachments } => match steer_turn(&text, &attachments) {
                Ok(line) => Encoded::Write(line),
                Err(e) => {
                    io.emit(AgentEvent::Error(e.to_string())).await;
                    Encoded::Done
                }
            },
            SessionCommand::Rewind { anchor } => Encoded::Write(
                control_request(
                    "rewind_conversation",
                    json!({"target_message_uuid": anchor}),
                )
                .1,
            ),
            // `-p` mode accepts the slash command as a message; it ends
            // with a `result`, like a turn.
            SessionCommand::Compact { instructions } => Encoded::Write(user_message(
                format!("/compact {}", instructions.unwrap_or_default()).trim_end(),
            )),
            SessionCommand::SendNow { text, attachments } => {
                self.interrupting = true;
                self.send_now(&text, &attachments, io).await;
                Encoded::Done
            }
            // A prompt sent at once that has not gone into a turn yet is
            // taken back with it (the user's Esc right after it), where
            // the CLI can (`interrupt_cancel_queued_v1`; it then answers
            // `cancelled` with the uuid). Otherwise it runs after the
            // interrupt, as anything queued does, and is still told
            // (`take_back_sent_now`: the TUI keeps it).
            SessionCommand::Interrupt => {
                self.interrupting = true;
                let take_back = self.parser.advertises("interrupt_cancel_queued_v1")
                    && self
                        .sent_now
                        .take_if(|s| !s.stopped && s.failed.is_none())
                        .is_some();
                let extra = if take_back {
                    json!({"cancel_queued": true})
                } else {
                    json!({})
                };
                Encoded::Write(control_request("interrupt", extra).1)
            }
            // Claude then tells of the stop in a turn of its own.
            SessionCommand::StopSubagent { id } => {
                match self.parser.task_of(&id).map(str::to_string) {
                    Some(task) => {
                        Encoded::Write(control_request("stop_task", json!({"task_id": task})).1)
                    }
                    None => {
                        io.emit(AgentEvent::Notice(
                            "that subagent is no longer running".into(),
                        ))
                        .await;
                        Encoded::Done
                    }
                }
            }
            SessionCommand::RespondPermission { id, decision } => match self.pending.remove(&id) {
                Some(p) => Encoded::Write(control_response(&id, encode_decision(&p, &decision))),
                None => {
                    io.emit(AgentEvent::Notice(format!(
                        "no pending permission request {id}"
                    )))
                    .await;
                    Encoded::Done
                }
            },
            SessionCommand::SetModel(m) => {
                let line = control_request("set_model", json!({"model": m.model})).1;
                self.cfg.model = Some(m);
                Encoded::Write(line)
            }
            // `-p` mode accepts the slash command as a message.
            SessionCommand::SetEffort(e) => {
                self.cfg.effort = e.clone();
                Encoded::Write(user_message(&format!(
                    "/effort {}",
                    e.unwrap_or_else(|| "high".into())
                )))
            }
            SessionCommand::SetPolicy(p) => {
                self.cfg.policy = p;
                self.policies_sent.push_back(p);
                Encoded::Write(
                    control_request("set_permission_mode", json!({"mode": policy_mode_name(p)})).1,
                )
            }
            SessionCommand::SetProvider(p) => match self.restart_on(p) {
                Ok(command) => Encoded::Restart {
                    command: Box::new(command),
                    then: None,
                },
                // No process can be started on it (a provider Claude does not
                // have): the session ends as a shutdown ends it, as it did
                // when the TUI ended it to start one.
                Err(e) => {
                    io.emit(AgentEvent::Error(format!(
                        "could not start claude again: {e:#}"
                    )))
                    .await;
                    let _ = io.write(&control_request("interrupt", json!({})).1).await;
                    io.end();
                    Encoded::Done
                }
            },
            SessionCommand::RemoteControl { enabled, name } => {
                self.remote = enabled.then(|| name.clone());
                let (id, line) = remote_control_request(enabled, name);
                self.remote_requests.insert(id, enabled);
                Encoded::Write(line)
            }
            // Answered with the account's windows, which the parser reads.
            SessionCommand::QueryStatus => {
                let (id, line) = control_request("get_usage", json!({"skip_behaviors": true}));
                self.usage_requests.insert(id);
                Encoded::Write(line)
            }
            // The driver's (`shutdown`).
            SessionCommand::Shutdown => Encoded::Done,
        }
    }

    fn write_failed(&mut self, error: anyhow::Error) -> Vec<AgentEvent> {
        vec![AgentEvent::Error(format!("claude: {error}"))]
    }

    fn parser(&mut self) -> &mut dyn Parser {
        &mut self.parser
    }

    /// An API error held for a result that never came is shown as text.
    fn on_exit(&mut self, _code: Option<i32>) -> Vec<AgentEvent> {
        self.parser.flush()
    }

    async fn on_stdout(&mut self, line: &str, io: &mut Io<'_>) -> Result<(), Gone> {
        // The answer to one of our `remote_control` requests, which the
        // parser would take for a generic one.
        let mut remote_answer = None;
        let mut init = false;
        // Driver-level bookkeeping before the pure parser sees it.
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            init = v.get("type").and_then(Value::as_str) == Some("system")
                && v.get("subtype").and_then(Value::as_str) == Some("init");
            match v.get("type").and_then(Value::as_str) {
                Some("control_response") => {
                    let rid = v.pointer("/response/request_id").and_then(Value::as_str);
                    if rid.is_some()
                        && rid == self.awaiting_init.as_ref().map(|(id, _)| id.as_str())
                    {
                        self.awaiting_init = None;
                    }
                    // A CLI that cannot say (an older one, or "not
                    // supported in this context") is not asked again, and
                    // its refusal is no error of the session's.
                    if rid.is_some_and(|r| self.usage_requests.remove(r))
                        && v.pointer("/response/subtype").and_then(Value::as_str) == Some("error")
                    {
                        remote_answer = Some(AgentEvent::CapabilitiesChanged(CapsUpdate {
                            status_query: Some(false),
                            ..Default::default()
                        }));
                    }
                    if let Some(enabled) = rid.and_then(|r| self.remote_requests.remove(r)) {
                        remote_answer = Some(AgentEvent::RemoteControl(
                            ClaudeParser::remote_control_answer(
                                enabled,
                                v.get("response").unwrap_or(&Value::Null),
                            ),
                        ));
                    }
                    // Only the answer to `initialize` names the account.
                    if let Some(why) = v.pointer("/response/response").and_then(|answer| {
                        super::provider_mismatch(self.cfg.provider.as_ref(), answer)
                    }) {
                        io.emit(AgentEvent::Error(why)).await;
                    }
                }
                Some("control_cancel_request") => {
                    if let Some(rid) = v.get("request_id").and_then(Value::as_str) {
                        self.pending.remove(rid);
                    }
                }
                Some("command_lifecycle") => {
                    for ev in self.lifecycle(&v) {
                        io.forward(ev).await?;
                    }
                }
                _ => {}
            }
        }
        let evs = match remote_answer {
            Some(ev) => vec![ev],
            None => self.parser.feed(line),
        };
        for mut ev in evs {
            if let AgentEvent::PermissionRequest(req) = &ev {
                let (input, kind) = match &req.kind {
                    PermissionKind::ToolUse { input, .. } => (input.clone(), PendingKind::Tool),
                    PermissionKind::Question { .. } => (Value::Null, PendingKind::Question),
                    PermissionKind::PlanApproval { .. } => (Value::Null, PendingKind::Plan),
                    _ => (Value::Null, PendingKind::Tool),
                };
                // For questions and plans we need the original input to echo back.
                let input = if kind == PendingKind::Tool {
                    input
                } else {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .and_then(|v| v.pointer("/request/input").cloned())
                        .unwrap_or(Value::Null)
                };
                self.pending
                    .insert(req.id.clone(), PendingPermission { input, kind });
            }
            self.keep(&ev);
            let ended = matches!(ev, AgentEvent::TurnCompleted { .. });
            if let AgentEvent::TurnCompleted { stop_reason } = &mut ev
                && std::mem::take(&mut self.interrupting)
                && matches!(stop_reason, StopReason::Error(e) if e.starts_with(STOPPED))
            {
                // What Claude makes of the interrupt it was sent (it ends
                // the turn `error_during_execution`, `aborted_tools` or
                // `aborted_streaming`), told as one.
                *stop_reason = StopReason::Interrupted;
            }
            io.forward(ev).await?;
            if ended {
                for ev in self.turn_ended() {
                    io.forward(ev).await?;
                }
            }
        }
        // A CLI whose interrupt does not take `cancel_queued` cannot be
        // made to drop a prompt sent at once: said once, at its first
        // `init`.
        if init && !self.told_take_back && !self.parser.advertises("interrupt_cancel_queued_v1") {
            self.told_take_back = true;
            io.forward(AgentEvent::CapabilitiesChanged(CapsUpdate {
                take_back_sent_now: Some(false),
                ..Default::default()
            }))
            .await?;
        }
        Ok(())
    }

    /// Started again on a new provider ([`Claude::restart_on`]): the TUI is
    /// told, and the new process is greeted as a session's is and put back
    /// on Remote Control if it was on, which the TUI did before this was
    /// the driver's (the same lines, in the same order).
    async fn respawned(&mut self, started: Result<()>, io: &mut Io<'_>) -> bool {
        if let Err(e) = started {
            io.emit(AgentEvent::Error(format!(
                "could not start claude again: {e:#}"
            )))
            .await;
            return false;
        }
        self.parser = ClaudeParser::new();
        self.pending.clear();
        self.remote_requests.clear();
        self.usage_requests.clear();
        self.policies_sent.clear();
        self.sent_now = None;
        self.interrupting = false;
        self.told_take_back = false;
        io.emit(AgentEvent::Restarted).await;
        self.start(io).await;
        if let Some(name) = self.remote.clone() {
            let (id, line) = remote_control_request(true, name);
            self.remote_requests.insert(id, true);
            if let Err(e) = io.write(&line).await {
                io.emit(AgentEvent::Error(format!("claude: {e}"))).await;
            }
        }
        true
    }

    /// With only its stdin closed Claude waits for its background tasks (a
    /// Monitor for as long as it was given); `interrupt` stops them, also
    /// between turns, and it then exits (2.1.292).
    fn shutdown(&mut self) -> Shutdown {
        Shutdown::End {
            first: vec![control_request("interrupt", json!({})).1],
        }
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
            workspace_root: None,
            model: Some(ModelRef::new(HarnessId::CLAUDE, "anthropic", "opus")),
            provider: None,
            effort: Some("high".into()),
            policy,
            resume: None,
            fork: false,
            extra_args: vec!["--bare".into()],
            env: vec![],
            mcp_servers: Vec::new(),
            sandbox: crate::core::Sandbox::off(),
        }
    }

    #[test]
    fn session_args_shape() {
        let a = session_args(&cfg(PermissionPolicy::Ask));
        let s = a.join(" ");
        assert!(s.starts_with("-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --forward-subagent-text --include-hook-events --replay-user-messages --permission-prompts host --permission-prompt-tool stdio"));
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
            kind: PendingKind::Tool,
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
        // Claude's own "always" (`updatedPermissions`) is never sent: it can
        // write to Claude's settings. Allow rules are unharness's.
        assert!(allow.get("updatedPermissions").is_none());
        let deny = encode_decision(
            &p,
            &PermissionDecision::Deny {
                reason: "no".into(),
            },
        );
        assert_eq!(deny, json!({"behavior":"deny","message":"no"}));

        let q = PendingPermission {
            input: json!({"questions":[{"question":"Color?"}]}),
            kind: PendingKind::Question,
        };
        let ans = encode_decision(&q, &PermissionDecision::Answer(json!({"Color?":"Red"})));
        assert_eq!(ans["behavior"], "allow");
        assert_eq!(ans["updatedInput"]["answers"]["Color?"], "Red");
        assert_eq!(ans["updatedInput"]["questions"][0]["question"], "Color?");
        // A bare `,` stays as it is (Claude splits only on `", "`); a quoted
        // label is escaped as JSON.stringify does; free text on a
        // multi-select question is a string and goes as it is.
        let many = encode_decision(
            &q,
            &PermissionDecision::Answer(json!({
                "Color?": ["Red", "a,b", "Salt, pepper", r#"C:\ "x""#],
                "Size?": "Big, or bigger",
            })),
        );
        assert_eq!(
            many["updatedInput"]["answers"],
            json!({
                "Color?": r#"Red, a,b, "Salt, pepper", "C:\\ \"x\"""#,
                "Size?": "Big, or bigger",
            })
        );
        let dismissed = encode_decision(&q, &PermissionDecision::Answer(Value::Null));
        assert_eq!(dismissed["behavior"], "deny");

        // A plan is approved with its input as it came
        // (`fixtures/plan_approved.jsonl`), and only by `Allow`.
        let plan = PendingPermission {
            input: json!({"plan": "# Plan", "planFilePath": "/p.md"}),
            kind: PendingKind::Plan,
        };
        let approved = encode_decision(
            &plan,
            &PermissionDecision::Allow {
                updated_input: None,
            },
        );
        assert_eq!(
            approved,
            json!({"behavior":"allow","updatedInput":{"plan": "# Plan", "planFilePath": "/p.md"}})
        );
        for answer in [Value::Null, Value::Bool(true)] {
            let kept = encode_decision(&plan, &PermissionDecision::Answer(answer));
            assert_eq!(kept["behavior"], "deny");
            assert!(
                kept["message"]
                    .as_str()
                    .unwrap()
                    .ends_with("how to proceed.")
            );
        }
        // Feedback goes as Claude's own interface sends it, which haiku
        // followed where the bare text was not (`fixtures/plan_rejected.jsonl`).
        let feedback = encode_decision(
            &plan,
            &PermissionDecision::Deny {
                reason: "Use Howdy".into(),
            },
        );
        assert!(
            feedback["message"]
                .as_str()
                .unwrap()
                .ends_with("To tell you how to proceed, the user said:\nUse Howdy")
        );

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

    #[test]
    fn mcp_servers_go_in_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("mcp");
        assert_eq!(mcp_config_file(&[], &dir).unwrap(), None);
        assert!(!dir.exists());

        let servers = crate::core::testing::sample_mcp_servers();
        let path = mcp_config_file(&servers, &dir).unwrap().unwrap();
        let config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            config,
            json!({"mcpServers": {
                "files": {
                    "type": "stdio",
                    "command": "/usr/bin/files-mcp",
                    "args": ["--root", "/my work"],
                    "env": {"TOKEN": "t\"1"},
                },
                "docs": {
                    "type": "http",
                    "url": "https://example.com/mcp",
                    "headers": {"Authorization": "Bearer x"},
                },
            }})
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| p.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&path), mode(&dir)), (0o600, 0o700));
        }

        // The same servers share the file; other servers get another.
        assert_eq!(mcp_config_file(&servers, &dir).unwrap().unwrap(), path);
        assert_ne!(mcp_config_file(&servers[..1], &dir).unwrap().unwrap(), path);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        // Nothing of it is on the command line.
        let mut c = cfg(PermissionPolicy::Ask);
        c.mcp_servers = servers;
        assert!(!session_args(&c).join(" ").contains("mcp"));
    }
}
