//! `--print`: one prompt through a session like the TUI's, its events
//! written to stdout as the assistant's text (`text`), one result object
//! (`json`), or one JSON object per line (`stream-json`, whose schema is
//! `docs/headless.md`). `--native` runs the vendor's own print command
//! instead (`runner.rs`).
//!
//! Nobody is there to answer a permission request: a tool call an allow
//! rule covers is allowed, every other one is denied, and a question is
//! dismissed.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use colored::*;
use serde_json::{Value, json};

use crate::core::{
    AgentEvent, CapsUpdate, HookOutcome, PermissionDecision, PermissionKind, PermissionRequest,
    Rule, Rules, SessionCommand, SessionHandle, StopReason, SubagentStatus, ToolAction, Usage,
};

/// The version of the `stream-json` and `json` output. Raised whenever a
/// line changes in a way a reader of the old one could misread; a new
/// line type or a new field does not raise it.
pub const SCHEMA_VERSION: u32 = 1;

/// How long a session has to end once it is told to.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    StreamJson,
}

impl Format {
    pub fn parse(s: Option<&str>) -> Result<Self> {
        Ok(match s {
            None | Some("text") => Format::Text,
            Some("json") => Format::Json,
            Some("stream-json") => Format::StreamJson,
            Some(other) => bail!("unknown format '{other}' (text, json, stream-json)"),
        })
    }
}

/// What the run is, for the first `stream-json` line.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub harness: String,
    pub policy: String,
    pub sandbox: String,
    pub cwd: PathBuf,
}

/// One headless run: what it has seen and where its output goes.
pub struct Headless<O: Write, E: Write> {
    format: Format,
    rules: Rules,
    cwd: PathBuf,
    /// The harness's short name, for stderr.
    name: String,
    /// `SubagentSupport::report_turn` of the harness.
    report_turn: bool,
    pub out: O,
    pub err: E,
    harness: String,
    session_id: Option<String>,
    model: Option<String>,
    /// The main agent's text so far.
    text: String,
    /// A tool call or a new turn came since the last text: the next text
    /// starts a paragraph.
    paragraph: bool,
    turn_running: bool,
    turns: u32,
    /// Turn ends still to come before the answer is complete: the
    /// prompt's, and one for each subagent the harness reports in a turn
    /// of its own (`report_turn`).
    owed: u32,
    /// Subagents that have started and not ended, at any depth.
    running: HashSet<String>,
    /// The tool call that spawned a subagent's subagent → the subagent
    /// that made it.
    spawned_by: HashMap<String, String>,
    /// Subagents whose end was counted in `owed` and whose spawning call
    /// has not returned: one that returns now was waited for (a blocking
    /// subagent), and its report is the call's result.
    owing: HashSet<String>,
    /// The session's work is over; what still comes in is written out
    /// but changes nothing.
    settled: bool,
    /// stdout and stderr are the same terminal, where a line on stderr
    /// would land in the middle of the answer's.
    shared_terminal: bool,
    /// How much of `text` was out when stderr last ended a line: the
    /// cursor is at a line start while nothing has been added since.
    text_at_err: usize,
    usage: Option<Usage>,
    denied: Vec<Value>,
    warnings: Vec<String>,
    /// How the run ends: an error or interrupt sticks, a later turn's
    /// `Done` does not hide it.
    stop: Option<StopReason>,
    /// The user pressed Ctrl+C.
    interrupted: bool,
    started: Instant,
}

impl<O: Write, E: Write> Headless<O, E> {
    pub fn new(
        format: Format,
        rules: Rules,
        cwd: PathBuf,
        name: &str,
        report_turn: bool,
        out: O,
        err: E,
    ) -> Self {
        Headless {
            format,
            rules,
            cwd,
            name: name.to_string(),
            report_turn,
            out,
            err,
            harness: String::new(),
            session_id: None,
            model: None,
            text: String::new(),
            paragraph: false,
            turn_running: false,
            turns: 0,
            owed: 0,
            running: HashSet::new(),
            spawned_by: HashMap::new(),
            owing: HashSet::new(),
            settled: false,
            shared_terminal: false,
            text_at_err: 0,
            usage: None,
            denied: Vec::new(),
            warnings: Vec::new(),
            stop: None,
            interrupted: false,
            started: Instant::now(),
        }
    }

    /// The run has begun: the prompt is on its way.
    pub fn begin(&mut self, info: &RunInfo) {
        self.harness = info.harness.clone();
        self.turn_running = true;
        self.owed = 1;
        self.line(&json!({
            "type": "start",
            "schema": SCHEMA_VERSION,
            "version": env!("CARGO_PKG_VERSION"),
            "harness": info.harness,
            "policy": info.policy,
            "sandbox": info.sandbox,
            "cwd": info.cwd,
        }));
    }

    /// Whether the answer is complete: the turn is over, and so is every
    /// subagent and every turn that reports one. An error or an interrupt
    /// ends the run at once.
    pub fn is_done(&self) -> bool {
        match &self.stop {
            None => false,
            Some(StopReason::Done) => self.owed == 0 && self.running.is_empty(),
            Some(_) => true,
        }
    }

    /// Nothing more the session sends counts: it is being shut down.
    pub fn settle(&mut self) {
        self.settled = true;
    }

    /// Something unharness itself has to say: kept for the result and
    /// written to stderr.
    pub fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.err_line(format!("{} {}", "[unharness]".yellow().bold(), message));
        self.warnings.push(message);
    }

    fn note(&mut self, message: &str) {
        self.err_line(format!("{} {}", "[unharness]".dimmed(), message));
    }

    /// Say that stdout and stderr are one terminal (`--format text` only
    /// streams to stdout as it goes).
    pub fn set_shared_terminal(&mut self, shared: bool) {
        self.shared_terminal = shared;
    }

    /// A line on stderr, on a line of its own where it shares the terminal
    /// with an answer that stopped mid-line. stdout is never changed.
    fn err_line(&mut self, line: String) {
        let mid_line = self.format == Format::Text
            && self.shared_terminal
            && self.text.len() != self.text_at_err
            && !self.text.ends_with('\n');
        if mid_line {
            let _ = writeln!(self.err);
        }
        let _ = writeln!(self.err, "{line}");
        self.text_at_err = self.text.len();
    }

    /// Ctrl+C: the process going away from here on is no error. Returns
    /// whether it is the first.
    pub fn interrupt(&mut self) -> bool {
        !std::mem::replace(&mut self.interrupted, true)
    }

    /// The run ends now, unfinished.
    pub fn abandon(&mut self) {
        self.stop = Some(StopReason::Interrupted);
    }

    pub fn turn_running(&self) -> bool {
        self.turn_running
    }

    fn record(&mut self, reason: StopReason) {
        if reason != StopReason::Done || self.stop.is_none() {
            self.stop = Some(reason);
        }
    }

    /// The events channel closed without the process saying it exited.
    pub fn lost(&mut self) {
        if !self.is_done() {
            self.record(StopReason::Error(format!(
                "the {} session ended before the turn completed",
                self.name
            )));
        }
    }

    /// Take one event from the session. Returns what to send back.
    pub fn on_event(&mut self, ev: AgentEvent) -> Vec<SessionCommand> {
        if self.format == Format::StreamJson {
            self.line(&event_json(&ev));
        }
        let mut commands = Vec::new();
        if !self.settled {
            self.track(&ev, None, &mut commands);
        }
        commands
    }

    /// `parent` is the subagent an event comes from, `None` for the main
    /// agent.
    fn track(&mut self, ev: &AgentEvent, parent: Option<&str>, commands: &mut Vec<SessionCommand>) {
        let main = parent.is_none();
        match ev {
            AgentEvent::Sub { parent, event } => self.track(event, Some(parent), commands),
            AgentEvent::PermissionRequest(req) => commands.push(self.answer(req)),
            AgentEvent::SubagentStarted { id, .. } => {
                self.running.insert(id.clone());
            }
            AgentEvent::SubagentEnded { id, status, .. } => {
                // Its report may come in a second end of the same id; only
                // the first one is news. Claude reports each end of a
                // background subagent in a turn of its own, wherever the
                // end falls (`fixtures/subagent_parallel.jsonl`), but not
                // one the agent stopped itself (`subagent_stopped.jsonl`).
                // A subagent's subagent was seen reported once its parent
                // had ended (`subagent_nested.jsonl`); while the parent
                // runs, it is taken to report to the parent.
                let parent_running = self
                    .spawned_by
                    .get(id)
                    .is_some_and(|p| self.running.contains(p));
                if self.running.remove(id)
                    && self.report_turn
                    && *status != SubagentStatus::Cancelled
                    && !parent_running
                {
                    self.owed += 1;
                    self.owing.insert(id.clone());
                }
            }
            AgentEvent::ToolCallStarted { id, .. } if !main => {
                if let Some(p) = parent {
                    self.spawned_by.insert(id.clone(), p.to_string());
                }
            }
            AgentEvent::ToolCallResult { id, .. } if self.owing.remove(id) => {
                // A blocking subagent: its report came back as the result.
                self.owed = self.owed.saturating_sub(1);
            }
            AgentEvent::ToolCallResult { .. } => {}
            // A hook that did its job quietly is not worth a line.
            AgentEvent::HookEnded {
                name,
                outcome,
                output,
                ..
            } if self.format != Format::StreamJson && *outcome != HookOutcome::Succeeded => {
                let line = format!(
                    "{}{}: hook {name} {}: {}",
                    self.name,
                    if main { "" } else { " subagent" },
                    outcome.label(),
                    output.trim()
                );
                self.note(line.trim_end_matches([':', ' ']));
            }
            _ if !main => {}
            AgentEvent::SessionStarted { session_id, model } => {
                self.session_id = Some(session_id.clone());
                if model.is_some() {
                    self.model = model.clone();
                }
            }
            AgentEvent::TurnStarted => {
                self.turn_running = true;
                self.paragraph = true;
            }
            AgentEvent::TextDelta(t) => self.text_delta(t),
            AgentEvent::ToolCallStarted { .. } => self.paragraph = true,
            AgentEvent::Usage(u) => match self.usage.as_mut() {
                Some(total) if !u.cumulative => total.add(u),
                _ => self.usage = Some(u.clone()),
            },
            AgentEvent::TurnCompleted { stop_reason } => {
                self.turn_running = false;
                self.turns += 1;
                self.owed = self.owed.saturating_sub(1);
                // However the harness words the end of an interrupted turn:
                // Claude as an error (`error_during_execution`), pi as done.
                self.record(if self.interrupted {
                    StopReason::Interrupted
                } else {
                    stop_reason.clone()
                });
                if !self.is_done() && self.format != Format::StreamJson {
                    let n = self.running.len();
                    self.note(&if n > 0 {
                        format!("waiting for {n} subagent(s)")
                    } else {
                        format!("waiting for {} to report its subagents", self.name)
                    });
                }
            }
            AgentEvent::Notice(n) if self.format != Format::StreamJson => {
                let line = format!("{}: {n}", self.name);
                self.note(&line);
            }
            AgentEvent::Error(e) if self.format != Format::StreamJson => {
                let line = format!("{} {}: {e}", "[unharness]".red().bold(), self.name);
                self.err_line(line);
            }
            AgentEvent::ProcessExited { code } => {
                self.turn_running = false;
                if self.is_done() {
                    return;
                }
                let reason = if self.interrupted {
                    StopReason::Interrupted
                } else {
                    StopReason::Error(format!(
                        "{} exited{} before the turn completed",
                        self.name,
                        code.map(|c| format!(" with code {c}")).unwrap_or_default()
                    ))
                };
                self.stop = Some(reason);
            }
            _ => {}
        }
    }

    fn text_delta(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        let mut piece = String::new();
        if self.paragraph && !self.text.is_empty() {
            if !self.text.ends_with('\n') {
                piece.push('\n');
            }
            if !self.text.ends_with("\n\n") {
                piece.push('\n');
            }
        }
        self.paragraph = false;
        piece.push_str(t);
        self.text.push_str(&piece);
        if self.format == Format::Text {
            let _ = self.out.write_all(piece.as_bytes());
            let _ = self.out.flush();
        }
    }

    /// Allow what an allow rule covers; deny or dismiss the rest.
    fn answer(&mut self, req: &PermissionRequest) -> SessionCommand {
        let (decision, line) = match &req.kind {
            PermissionKind::ToolUse { tool, action, .. } => {
                match self.rules.allows(tool, action, &self.cwd) {
                    Some(rules) => {
                        let rules: Vec<String> = rules.into_iter().map(Rule::describe).collect();
                        self.note(&format!(
                            "allowed {tool} by your rule for {}",
                            rules.join(", and for ")
                        ));
                        (
                            PermissionDecision::Allow {
                                updated_input: None,
                            },
                            json!({"decision": "allow", "rules": rules}),
                        )
                    }
                    None => {
                        self.warn(format!(
                            "denied {tool}{}: nobody can be asked without the TUI (an allow rule or --policy would let it run)",
                            action_text(action)
                        ));
                        self.denied
                            .push(json!({"tool": tool, "action": action_json(action)}));
                        (
                            PermissionDecision::Deny {
                                reason: "Denied: unharness is running without a user to ask. \
                                         Nothing that is not covered by an allow rule or the \
                                         permission policy can run."
                                    .into(),
                            },
                            json!({"decision": "deny"}),
                        )
                    }
                }
            }
            _ => {
                self.warn(format!(
                    "dismissed a question from {}: nobody can answer it without the TUI",
                    self.name
                ));
                (
                    PermissionDecision::Answer(Value::Null),
                    json!({"decision": "dismiss"}),
                )
            }
        };
        let mut line = line;
        line["type"] = json!("permission_answered");
        line["id"] = json!(req.id);
        self.line(&line);
        SessionCommand::RespondPermission {
            id: req.id.clone(),
            decision,
        }
    }

    fn line(&mut self, value: &Value) {
        if self.format == Format::StreamJson {
            let _ = writeln!(self.out, "{value}");
            let _ = self.out.flush();
        }
    }

    /// The `json` output, and the last `stream-json` line.
    pub fn result(&self) -> Value {
        let (status, error) = match &self.stop {
            Some(StopReason::Done) => ("done", None),
            Some(StopReason::Interrupted) => ("interrupted", None),
            Some(StopReason::Error(e)) => ("error", Some(e.clone())),
            None => (
                "error",
                Some("the run ended before the turn completed".into()),
            ),
        };
        json!({
            "type": "result",
            "schema": SCHEMA_VERSION,
            "harness": self.harness,
            "session_id": self.session_id,
            "model": self.model,
            "status": status,
            "error": error,
            "text": self.text,
            "turns": self.turns,
            "usage": self.usage.as_ref().map(usage_json),
            "denied": self.denied,
            "warnings": self.warnings,
            "duration_ms": self.started.elapsed().as_millis() as u64,
        })
    }

    /// Write what comes last and return the exit code: 0 when the run
    /// completed, 130 when it was interrupted, 1 otherwise.
    pub fn finish(&mut self) -> u8 {
        let result = self.result();
        match self.format {
            Format::Text => {
                if !self.text.is_empty() && !self.text.ends_with('\n') {
                    let _ = writeln!(self.out);
                }
                // The answer's own line is ended now.
                self.text_at_err = self.text.len();
            }
            Format::Json => {
                let _ = writeln!(
                    self.out,
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
            Format::StreamJson => self.line(&result),
        }
        let _ = self.out.flush();
        match &self.stop {
            Some(StopReason::Done) => 0,
            Some(StopReason::Interrupted) => {
                if self.format == Format::Text {
                    self.err_line(format!("{} interrupted", "[unharness]".yellow().bold()));
                }
                130
            }
            Some(StopReason::Error(e)) => {
                if self.format == Format::Text {
                    let line = format!("{} {e}", "[unharness]".red().bold());
                    self.err_line(line);
                }
                1
            }
            None => 1,
        }
    }
}

/// Send the prompt and take the session's events until the answer is
/// complete, then shut the session down. Ctrl+C interrupts the turn; a
/// second one, or one between turns, ends the run.
pub async fn drive<O: Write, E: Write>(
    mut handle: SessionHandle,
    prompt: String,
    run: &mut Headless<O, E>,
) {
    if handle.send(SessionCommand::turn(prompt)).await.is_err() {
        run.lost();
        return;
    }
    let mut interrupts = Interrupts::new();
    let mut ends = crate::core::process::EndSignals::listen();
    let mut job_stops = crate::core::process::JobStops::listen();
    loop {
        tokio::select! {
            ev = handle.events.recv() => match ev {
                Some(ev) => {
                    for cmd in run.on_event(ev) {
                        let _ = handle.send(cmd).await;
                    }
                    if run.is_done() {
                        break;
                    }
                }
                None => {
                    run.lost();
                    return;
                }
            },
            _ = interrupts.recv() => {
                if run.interrupt() && run.turn_running() {
                    let _ = handle.send(SessionCommand::Interrupt).await;
                } else {
                    run.abandon();
                    break;
                }
            }
            _ = ends.recv() => {
                run.abandon();
                break;
            }
            _ = job_stops.recv() => {
                #[cfg(unix)]
                crate::core::process::suspend();
            }
        }
    }
    run.settle();
    let _ = handle.send(SessionCommand::Shutdown).await;
    // Until the process is gone, the grace is up, or Ctrl+C. Whatever is
    // left is killed when the runtime goes, with what it started.
    let drain = async {
        while let Some(ev) = handle.events.recv().await {
            let exited = matches!(ev, AgentEvent::ProcessExited { .. });
            run.on_event(ev);
            if exited {
                break;
            }
        }
    };
    let drain = tokio::time::timeout(SHUTDOWN_GRACE, drain);
    tokio::pin!(drain);
    loop {
        tokio::select! {
            _ = &mut drain => break,
            _ = interrupts.recv() => break,
            _ = ends.recv() => break,
            // Its handler stays; the CLI is stopped with unharness here too.
            _ = job_stops.recv() => {
                #[cfg(unix)]
                crate::core::process::suspend();
            }
        }
    }
}

/// Ctrl+C, listened for once for the whole run: one pressed while an
/// event is being handled waits for the next poll instead of being lost.
struct Interrupts {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
}

impl Interrupts {
    fn new() -> Self {
        Interrupts {
            #[cfg(unix)]
            signal: crate::core::process::listen_unless_ignored(libc::SIGINT),
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        match self.signal.as_mut() {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending().await,
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input": u.input,
        "output": u.output,
        "cache_read": u.cache_read,
        "cache_write": u.cache_write,
        "cost_usd": u.cost_usd,
        "cumulative": u.cumulative,
    })
}

fn action_text(action: &ToolAction) -> String {
    match action {
        ToolAction::Other | ToolAction::Opaque => String::new(),
        a => format!(" ({})", a.summary()),
    }
}

fn action_json(action: &ToolAction) -> Value {
    match action {
        ToolAction::Shell { command, cwd } => {
            json!({"kind": "shell", "command": command, "cwd": cwd})
        }
        ToolAction::Edit { paths } => json!({"kind": "edit", "paths": paths}),
        ToolAction::Read { path } => json!({"kind": "read", "path": path}),
        ToolAction::Mcp { server, tool } => json!({"kind": "mcp", "server": server, "tool": tool}),
        ToolAction::Other => json!({"kind": "other"}),
        ToolAction::Opaque => json!({"kind": "opaque"}),
    }
}

fn caps_json(u: &CapsUpdate) -> Value {
    let mut v = json!({});
    if let Some(e) = &u.effort_levels {
        v["effort_levels"] = json!(e);
    }
    if let Some(i) = u.image_input {
        v["image_input"] = json!(i);
    }
    if let Some(r) = u.resume_by_id {
        v["resume_by_id"] = json!(r);
    }
    if let Some(h) = u.mcp_http {
        v["mcp_http"] = json!(h);
    }
    if let Some(p) = u.plan_mode {
        v["plan_mode"] = json!(p);
    }
    if let Some(s) = u.slash_commands {
        v["slash_commands"] = json!(s);
    }
    if let Some(p) = &u.provider {
        v["provider"] = json!(p.0);
    }
    if let Some(commands) = &u.commands {
        v["commands"] = commands
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "description": c.description,
                    "hint": c.hint,
                    "aliases": c.aliases,
                })
            })
            .collect();
    }
    if let Some(models) = &u.models {
        v["models"] = models
            .iter()
            .map(|m| {
                json!({
                    "id": m.model_ref.model,
                    "provider": m.model_ref.provider.0,
                    "name": m.display_name,
                    "description": m.description,
                    "effort_levels": m.effort_levels,
                })
            })
            .collect();
    }
    v
}

fn permission_json(req: &PermissionRequest) -> Value {
    let mut v = match &req.kind {
        PermissionKind::ToolUse {
            tool,
            input,
            action,
            description,
        } => json!({
            "kind": "tool_use",
            "tool": tool,
            "input": input,
            "action": action_json(action),
            "description": description,
        }),
        PermissionKind::Question { questions } => json!({
            "kind": "question",
            "questions": questions.iter().map(|q| json!({
                "id": q.id,
                "header": q.header,
                "text": q.text,
                "options": q.options.iter().map(|o| json!({
                    "label": o.label,
                    "description": o.description,
                    "preview": o.preview,
                })).collect::<Vec<_>>(),
                "allow_other": q.allow_other,
                "multi": q.multi,
            })).collect::<Vec<_>>(),
        }),
        PermissionKind::Confirm { title, message } => {
            json!({"kind": "confirm", "title": title, "message": message})
        }
        PermissionKind::Select { title, options } => {
            json!({"kind": "select", "title": title, "options": options})
        }
        PermissionKind::Input {
            title,
            placeholder,
            prefill,
            multiline,
        } => json!({
            "kind": "input",
            "title": title,
            "placeholder": placeholder,
            "prefill": prefill,
            "multiline": multiline,
        }),
    };
    v["type"] = json!("permission_request");
    v["id"] = json!(req.id);
    v["tool_call_id"] = json!(req.tool_call_id);
    v
}

/// One event as a `stream-json` line (`docs/headless.md`).
pub fn event_json(ev: &AgentEvent) -> Value {
    match ev {
        AgentEvent::SessionStarted { session_id, model } => {
            json!({"type": "session_started", "session_id": session_id, "model": model})
        }
        AgentEvent::TurnStarted => json!({"type": "turn_started"}),
        AgentEvent::TextDelta(t) => json!({"type": "text_delta", "text": t}),
        AgentEvent::ThinkingDelta(t) => json!({"type": "thinking_delta", "text": t}),
        AgentEvent::ToolCallStarted { id, name, input } => {
            json!({"type": "tool_call_started", "id": id, "name": name, "input": input})
        }
        AgentEvent::ToolCallDelta { id, name, delta } => {
            json!({"type": "tool_call_delta", "id": id, "name": name, "delta": delta})
        }
        AgentEvent::ToolCallResult {
            id,
            output,
            is_error,
        } => json!({"type": "tool_call_result", "id": id, "output": output, "is_error": is_error}),
        AgentEvent::PermissionRequest(req) => permission_json(req),
        AgentEvent::Usage(u) => {
            let mut v = usage_json(u);
            v["type"] = json!("usage");
            v
        }
        AgentEvent::PlanUpdated {
            entries,
            explanation,
        } => json!({"type": "plan_updated", "entries": entries, "explanation": explanation}),
        AgentEvent::Sub { parent, event } => {
            json!({"type": "sub", "parent": parent, "event": event_json(event)})
        }
        AgentEvent::SubagentStarted {
            id,
            description,
            kind,
        } => json!({
            "type": "subagent_started",
            "id": id,
            "description": description,
            "kind": kind,
        }),
        AgentEvent::SubagentProgress { id, activity } => {
            json!({"type": "subagent_progress", "id": id, "activity": activity})
        }
        AgentEvent::SubagentEnded { id, status, result } => {
            json!({"type": "subagent_ended", "id": id, "status": status, "result": result})
        }
        AgentEvent::HookStarted { id, name } => {
            json!({"type": "hook_started", "id": id, "name": name})
        }
        AgentEvent::HookEnded {
            id,
            name,
            outcome,
            output,
        } => json!({
            "type": "hook_ended",
            "id": id,
            "name": name,
            "outcome": outcome,
            "output": output,
        }),
        AgentEvent::Context(c) => json!({"type": "context", "used": c.used, "window": c.window}),
        AgentEvent::RateLimit(r) => json!({
            "type": "rate_limit",
            "status": r.status,
            "windows": r.windows.iter().map(|w| json!({
                "label": w.label,
                "used_percent": w.used_percent,
                "resets_at": w.resets_at,
            })).collect::<Vec<_>>(),
        }),
        AgentEvent::TurnAnchor { id } => json!({"type": "turn_anchor", "id": id}),
        AgentEvent::RewindFailed { reason } => json!({"type": "rewind_failed", "reason": reason}),
        AgentEvent::CapabilitiesChanged(u) => {
            let mut v = caps_json(u);
            v["type"] = json!("capabilities_changed");
            v
        }
        AgentEvent::TurnCompleted { stop_reason } => {
            let (status, error) = match stop_reason {
                StopReason::Done => ("done", None),
                StopReason::Interrupted => ("interrupted", None),
                StopReason::Error(e) => ("error", Some(e)),
            };
            json!({"type": "turn_completed", "status": status, "error": error})
        }
        AgentEvent::Notice(n) => json!({"type": "notice", "message": n}),
        AgentEvent::Error(e) => json!({"type": "error", "message": e}),
        AgentEvent::ProcessExited { code } => json!({"type": "process_exited", "code": code}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::rules::Scope;
    use crate::core::{Question, QuestionOption, SubagentStatus};

    type Run = Headless<Vec<u8>, Vec<u8>>;

    fn headless(format: Format, report_turn: bool) -> (tempfile::TempDir, Run) {
        let dir = tempfile::tempdir().unwrap();
        let rules = Rules::load_in(&dir.path().join("config"), Some(dir.path())).unwrap();
        let mut run = Headless::new(
            format,
            rules,
            dir.path().to_path_buf(),
            "claude",
            report_turn,
            Vec::new(),
            Vec::new(),
        );
        run.begin(&RunInfo {
            harness: "claude".into(),
            policy: "ask".into(),
            sandbox: "workspace-write".into(),
            cwd: dir.path().to_path_buf(),
        });
        (dir, run)
    }

    fn feed(run: &mut Run, events: Vec<AgentEvent>) -> Vec<SessionCommand> {
        events.into_iter().flat_map(|e| run.on_event(e)).collect()
    }

    fn done() -> AgentEvent {
        AgentEvent::TurnCompleted {
            stop_reason: StopReason::Done,
        }
    }

    fn tool(id: &str) -> AgentEvent {
        AgentEvent::ToolCallStarted {
            id: id.into(),
            name: "Bash".into(),
            input: json!({"command": "ls"}),
        }
    }

    fn text(t: &str) -> AgentEvent {
        AgentEvent::TextDelta(t.into())
    }

    fn shell_request(id: &str, command: &str) -> AgentEvent {
        AgentEvent::PermissionRequest(PermissionRequest {
            id: id.into(),
            kind: PermissionKind::ToolUse {
                tool: "Bash".into(),
                input: json!({"command": command}),
                action: ToolAction::Shell {
                    command: command.into(),
                    cwd: None,
                },
                description: None,
            },
            tool_call_id: None,
        })
    }

    fn lines(run: &Run) -> Vec<Value> {
        String::from_utf8_lossy(&run.out)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn formats_by_name() {
        assert_eq!(Format::parse(None).unwrap(), Format::Text);
        assert_eq!(
            Format::parse(Some("stream-json")).unwrap(),
            Format::StreamJson
        );
        assert!(Format::parse(Some("yaml")).is_err());
    }

    #[test]
    fn text_is_the_main_agents_text_in_paragraphs() {
        let (_dir, mut run) = headless(Format::Text, false);
        feed(
            &mut run,
            vec![
                AgentEvent::TurnStarted,
                text("Looking"),
                text(" now."),
                tool("t1"),
                AgentEvent::Sub {
                    parent: "t1".into(),
                    event: Box::new(text("not mine")),
                },
                text("Done."),
                done(),
            ],
        );
        assert!(run.is_done());
        assert_eq!(run.finish(), 0);
        assert_eq!(String::from_utf8_lossy(&run.out), "Looking now.\n\nDone.\n");
    }

    #[test]
    fn a_harness_s_commands_are_listed_with_their_hint() {
        let v = event_json(&AgentEvent::CapabilitiesChanged(CapsUpdate {
            commands: Some(vec![
                crate::core::HarnessCommand::new("review", Some("Review it"), Some("[pr]"))
                    .unwrap(),
            ]),
            ..Default::default()
        }));
        assert_eq!(v["type"], "capabilities_changed");
        assert_eq!(
            v["commands"],
            json!([{"name": "review", "description": "Review it", "hint": "[pr]", "aliases": []}])
        );
    }

    #[test]
    fn stream_json_is_one_object_per_line_between_start_and_result() {
        let (_dir, mut run) = headless(Format::StreamJson, false);
        feed(
            &mut run,
            vec![
                AgentEvent::SessionStarted {
                    session_id: "s1".into(),
                    model: Some("haiku".into()),
                },
                AgentEvent::TurnStarted,
                AgentEvent::Sub {
                    parent: "t1".into(),
                    event: Box::new(text("inner")),
                },
                text("hi"),
                AgentEvent::Usage(Usage {
                    input: 3,
                    output: 4,
                    cost_usd: Some(0.5),
                    ..Default::default()
                }),
                done(),
            ],
        );
        assert_eq!(run.finish(), 0);
        let lines = lines(&run);
        let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "start",
                "session_started",
                "turn_started",
                "sub",
                "text_delta",
                "usage",
                "turn_completed",
                "result"
            ]
        );
        assert_eq!(lines[0]["schema"], SCHEMA_VERSION);
        assert_eq!(lines[3]["event"]["text"], "inner");
        let result = &lines[7];
        assert_eq!(result["status"], "done");
        assert_eq!(result["session_id"], "s1");
        assert_eq!(result["model"], "haiku");
        assert_eq!(result["text"], "hi");
        assert_eq!(result["turns"], 1);
        assert_eq!(result["usage"]["output"], 4);
        // Nothing but the lines on stdout.
        assert!(run.err.is_empty());
    }

    #[test]
    fn a_rule_allows_and_everything_else_is_refused() {
        let (_dir, mut run) = headless(Format::Json, false);
        run.rules
            .append(Scope::Workspace, &[Rule::shell("cargo test")])
            .unwrap();
        let question = AgentEvent::PermissionRequest(PermissionRequest {
            id: "q".into(),
            kind: PermissionKind::Question {
                questions: vec![Question {
                    id: "color".into(),
                    header: "Color".into(),
                    text: "Red or blue?".into(),
                    options: vec![QuestionOption::new("Red", "")],
                    allow_other: false,
                    multi: false,
                }],
            },
            tool_call_id: None,
        });
        let sub = AgentEvent::Sub {
            parent: "t1".into(),
            event: Box::new(shell_request("inner", "cargo test")),
        };
        let commands = feed(
            &mut run,
            vec![
                shell_request("a", "cargo test"),
                shell_request("b", "rm -rf target"),
                question,
                sub,
                done(),
            ],
        );
        let decisions: Vec<(&str, &PermissionDecision)> = commands
            .iter()
            .map(|c| match c {
                SessionCommand::RespondPermission { id, decision } => (id.as_str(), decision),
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(matches!(
            decisions[0],
            ("a", PermissionDecision::Allow { .. })
        ));
        assert!(matches!(
            decisions[1],
            ("b", PermissionDecision::Deny { .. })
        ));
        assert_eq!(
            decisions[2],
            ("q", &PermissionDecision::Answer(Value::Null))
        );
        // A subagent's request is answered the same way.
        assert!(matches!(
            decisions[3],
            ("inner", PermissionDecision::Allow { .. })
        ));

        assert_eq!(run.finish(), 0);
        let result: Value = serde_json::from_slice(&run.out).unwrap();
        assert_eq!(
            result["denied"],
            json!([{"tool": "Bash", "action": {"kind": "shell", "command": "rm -rf target", "cwd": null}}])
        );
        assert_eq!(result["warnings"].as_array().unwrap().len(), 2);
        let err = String::from_utf8_lossy(&run.err);
        assert!(
            err.contains("denied Bash (shell \"rm -rf target\")"),
            "{err}"
        );
    }

    #[test]
    fn the_answer_waits_for_subagents_and_the_turn_that_reports_them() {
        let started = AgentEvent::SubagentStarted {
            id: "t1".into(),
            description: "read a file".into(),
            kind: None,
        };
        let ended = |result: Option<&str>| AgentEvent::SubagentEnded {
            id: "t1".into(),
            status: SubagentStatus::Completed,
            result: result.map(str::to_string),
        };

        // Claude: the end comes between turns and a turn of its own follows
        // (`fixtures/subagent.jsonl`).
        let (_dir, mut run) = headless(Format::Text, true);
        feed(
            &mut run,
            vec![
                AgentEvent::TurnStarted,
                started.clone(),
                text("Launched."),
                done(),
            ],
        );
        assert!(!run.is_done());
        feed(&mut run, vec![ended(None)]);
        assert!(!run.is_done());
        feed(&mut run, vec![ended(Some("alpha"))]);
        assert!(!run.is_done());
        feed(
            &mut run,
            vec![AgentEvent::TurnStarted, text("It says alpha.")],
        );
        assert!(!run.is_done());
        feed(&mut run, vec![done()]);
        assert!(run.is_done());
        assert_eq!(run.finish(), 0);
        assert_eq!(
            String::from_utf8_lossy(&run.out),
            "Launched.\n\nIt says alpha.\n"
        );

        // One that ends during the turn is reported in a turn of its own
        // too (`fixtures/subagent_parallel.jsonl`).
        let (_dir, mut run) = headless(Format::Text, true);
        feed(
            &mut run,
            vec![
                AgentEvent::TurnStarted,
                started.clone(),
                ended(None),
                done(),
            ],
        );
        assert!(!run.is_done());
        feed(&mut run, vec![done()]);
        assert!(run.is_done());

        // One the spawning call waited for reports as the call's result
        // (`fixtures/subagent_blocking.jsonl`).
        let (_dir, mut run) = headless(Format::Text, true);
        let returned = AgentEvent::ToolCallResult {
            id: "t1".into(),
            output: "alpha".into(),
            is_error: false,
        };
        feed(
            &mut run,
            vec![
                AgentEvent::TurnStarted,
                started.clone(),
                ended(Some("alpha")),
                returned,
                done(),
            ],
        );
        assert!(run.is_done());

        // One the agent stopped is not reported (`subagent_stopped.jsonl`).
        let (_dir, mut run) = headless(Format::Text, true);
        let stopped = AgentEvent::SubagentEnded {
            id: "t1".into(),
            status: SubagentStatus::Cancelled,
            result: None,
        };
        feed(
            &mut run,
            vec![AgentEvent::TurnStarted, started.clone(), stopped, done()],
        );
        assert!(run.is_done());

        // Codex: nothing follows the end.
        let (_dir, mut run) = headless(Format::Text, false);
        feed(&mut run, vec![AgentEvent::TurnStarted, started, done()]);
        assert!(!run.is_done());
        feed(&mut run, vec![ended(Some("alpha"))]);
        assert!(run.is_done());
    }

    /// Every Claude recording of one prompt, through the parser: the run is
    /// done at the last turn end and not before.
    #[test]
    fn claude_recordings_end_with_their_last_turn() {
        use crate::core::testing::{fixtures_dir, replay};
        use crate::harness::claude::parse::ClaudeParser;
        let dir = fixtures_dir("src/harness/claude/mod.rs");
        let mut checked = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let fixture = std::fs::read_to_string(&path).unwrap();
            let sent: Vec<Value> = fixture
                .lines()
                .filter_map(|l| l.strip_prefix(">>"))
                .map(|l| serde_json::from_str(l.trim()).unwrap())
                .collect();
            let prompts = sent.iter().filter(|v| v["type"] == "user").count();
            // Control requests a headless run never sends (`interrupt`,
            // `stop_task`, `rewind_conversation`).
            let other = sent
                .iter()
                .any(|v| v["type"] == "control_request" && v["request"]["subtype"] != "initialize");
            if prompts != 1 || other {
                continue;
            }
            let events = replay(&mut ClaudeParser::new(), &fixture);
            let last = events
                .iter()
                .rposition(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
                .unwrap();
            let (_dir, mut run) = headless(Format::Text, true);
            for (i, ev) in events.into_iter().enumerate() {
                run.on_event(ev);
                let name = path.file_name().unwrap().to_string_lossy();
                assert_eq!(
                    run.is_done(),
                    i >= last,
                    "{name}: event {i}, last turn end {last}"
                );
            }
            checked.push(path.file_stem().unwrap().to_string_lossy().into_owned());
        }
        checked.sort();
        for case in [
            "subagent",
            "subagent_blocking",
            "subagent_nested",
            "subagent_parallel",
            "subagent_resumed",
        ] {
            assert!(
                checked.iter().any(|c| c == case),
                "{case} not checked: {checked:?}"
            );
        }
    }

    #[test]
    fn a_note_starts_its_own_line_on_a_shared_terminal() {
        let notice = || AgentEvent::Notice("1 permission denial(s)".into());
        let (_dir, mut run) = headless(Format::Text, false);
        run.set_shared_terminal(true);
        feed(&mut run, vec![text("so I"), notice(), notice()]);
        feed(&mut run, vec![text(" don't know."), notice(), done()]);
        assert_eq!(run.finish(), 0);
        // stdout is the answer alone; stderr breaks the line once per
        // stretch of text.
        assert_eq!(String::from_utf8_lossy(&run.out), "so I don't know.\n");
        let err = String::from_utf8_lossy(&run.err);
        let lines: Vec<&str> = err.split('\n').collect();
        let blank: Vec<bool> = lines.iter().map(|l| l.is_empty()).collect();
        assert_eq!(blank, [true, false, false, true, false, true], "{err:?}");

        // Piped, or ending on a line end, nothing is added.
        for (shared, t) in [(false, "so I"), (true, "line\n")] {
            let (_dir, mut run) = headless(Format::Text, false);
            run.set_shared_terminal(shared);
            feed(&mut run, vec![text(t), notice()]);
            assert!(!run.err.starts_with(b"\n"));
        }
    }

    #[test]
    fn a_hook_is_noted_only_when_it_did_not_succeed() {
        let hook = |id: &str, outcome, output: &str| AgentEvent::HookEnded {
            id: id.into(),
            name: "PreToolUse:Bash".into(),
            outcome,
            output: output.into(),
        };
        let (_dir, mut run) = headless(Format::Text, false);
        feed(
            &mut run,
            vec![
                hook("1", HookOutcome::Succeeded, "checked"),
                hook("2", HookOutcome::Blocked, "no rm\n"),
                hook("3", HookOutcome::Failed, ""),
                AgentEvent::Sub {
                    parent: "spawn".into(),
                    event: Box::new(hook("4", HookOutcome::Blocked, "not here")),
                },
                done(),
            ],
        );
        assert_eq!(run.finish(), 0);
        let err = String::from_utf8_lossy(&run.err);
        assert!(!err.contains("checked"), "{err:?}");
        assert!(
            err.contains("claude subagent: hook PreToolUse:Bash blocked: not here\n"),
            "{err:?}"
        );
        assert!(
            err.contains("claude: hook PreToolUse:Bash blocked: no rm\n"),
            "{err:?}"
        );
        assert!(
            err.contains("claude: hook PreToolUse:Bash failed\n"),
            "{err:?}"
        );
    }

    #[test]
    fn what_comes_while_shutting_down_changes_nothing() {
        let (_dir, mut run) = headless(Format::StreamJson, false);
        feed(&mut run, vec![text("hi"), done()]);
        assert!(run.is_done());
        run.settle();
        let late = feed(
            &mut run,
            vec![
                text(" more"),
                shell_request("late", "rm -rf /"),
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Error("late".into()),
                },
                AgentEvent::ProcessExited { code: Some(1) },
            ],
        );
        assert!(late.is_empty());
        assert_eq!(run.finish(), 0);
        let lines = lines(&run);
        // Written out all the same.
        assert!(lines.iter().any(|l| l["type"] == "permission_request"));
        let result = lines.last().unwrap();
        assert_eq!(result["status"], "done");
        assert_eq!(result["text"], "hi");
        assert_eq!(result["turns"], 1);
        assert_eq!(result["denied"], json!([]));
    }

    #[test]
    fn an_early_exit_is_an_error_unless_interrupted() {
        let (_dir, mut run) = headless(Format::Json, false);
        feed(
            &mut run,
            vec![
                AgentEvent::TurnStarted,
                AgentEvent::ProcessExited { code: Some(2) },
            ],
        );
        assert!(run.is_done());
        assert_eq!(run.finish(), 1);
        let result: Value = serde_json::from_slice(&run.out).unwrap();
        assert_eq!(result["status"], "error");
        assert_eq!(
            result["error"],
            "claude exited with code 2 before the turn completed"
        );

        let (_dir, mut run) = headless(Format::Text, false);
        assert!(run.interrupt());
        assert!(!run.interrupt());
        feed(&mut run, vec![AgentEvent::ProcessExited { code: None }]);
        assert_eq!(run.finish(), 130);
        // However the harness words the end of an interrupted turn:
        // Claude as an error, pi as done.
        for stop_reason in [
            StopReason::Error("error_during_execution".into()),
            StopReason::Done,
        ] {
            let (_dir, mut run) = headless(Format::Text, false);
            run.interrupt();
            feed(&mut run, vec![AgentEvent::TurnCompleted { stop_reason }]);
            assert!(run.is_done());
            assert_eq!(run.finish(), 130);
        }

        // An error sticks through a later turn's success.
        let (_dir, mut run) = headless(Format::Text, true);
        feed(
            &mut run,
            vec![
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Error("overloaded".into()),
                },
                done(),
            ],
        );
        assert_eq!(run.finish(), 1);
        // Exiting after the answer is complete is how a run ends.
        let (_dir, mut run) = headless(Format::Text, false);
        feed(
            &mut run,
            vec![done(), AgentEvent::ProcessExited { code: Some(0) }],
        );
        assert_eq!(run.finish(), 0);
    }
}
