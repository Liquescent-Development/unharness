//! `--print`: one prompt through a session like the TUI's, its events
//! written to stdout as the assistant's text (`text`), one result object
//! (`json`), or one JSON object per line (`stream-json`, whose schema is
//! `docs/headless.md`). `--native` runs the vendor's own print command
//! instead (`runner.rs`).
//!
//! Nobody is there to answer a permission request: a tool call an allow
//! rule covers is allowed, every other one is denied, and a question is
//! dismissed.

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use colored::*;
use serde_json::{Value, json};

use crate::core::{
    AgentEvent, CapsUpdate, PermissionDecision, PermissionKind, PermissionRequest, Rule, Rules,
    SessionCommand, SessionHandle, StopReason, ToolAction, Usage,
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
    /// Subagents that have started and not ended, at any depth.
    running: HashSet<String>,
    /// A subagent ended between turns and the harness will report it in a
    /// turn of its own, which is part of the answer.
    report_due: bool,
    /// The session's work is over; what still comes in is written out
    /// but changes nothing.
    settled: bool,
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
            running: HashSet::new(),
            report_due: false,
            settled: false,
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
    /// subagent and the turn that reports one. An error or an interrupt
    /// ends the run at once.
    pub fn is_done(&self) -> bool {
        match &self.stop {
            None => false,
            Some(StopReason::Done) => {
                !self.turn_running && self.running.is_empty() && !self.report_due
            }
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
        let _ = writeln!(self.err, "{} {}", "[unharness]".yellow().bold(), message);
        self.warnings.push(message);
    }

    fn note(&mut self, message: &str) {
        let _ = writeln!(self.err, "{} {}", "[unharness]".dimmed(), message);
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
            self.track(&ev, 0, &mut commands);
        }
        commands
    }

    fn track(&mut self, ev: &AgentEvent, depth: usize, commands: &mut Vec<SessionCommand>) {
        let main = depth == 0;
        match ev {
            AgentEvent::Sub { event, .. } => self.track(event, depth + 1, commands),
            AgentEvent::PermissionRequest(req) => commands.push(self.answer(req)),
            AgentEvent::SubagentStarted { id, .. } => {
                self.running.insert(id.clone());
            }
            AgentEvent::SubagentEnded { id, .. } => {
                // Its report may come in a second end of the same id; only
                // the first one is news.
                if self.running.remove(id) && main && self.report_turn && !self.turn_running {
                    self.report_due = true;
                }
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
                self.report_due = false;
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
                // However the harness words the end of an interrupted turn:
                // Claude as an error (`error_during_execution`), pi as done.
                self.record(if self.interrupted {
                    StopReason::Interrupted
                } else {
                    stop_reason.clone()
                });
                if *stop_reason == StopReason::Done
                    && !self.running.is_empty()
                    && self.format != Format::StreamJson
                {
                    let n = self.running.len();
                    self.note(&format!("waiting for {n} subagent(s)"));
                }
            }
            AgentEvent::Notice(n) if self.format != Format::StreamJson => {
                let line = format!("{}: {n}", self.name);
                self.note(&line);
            }
            AgentEvent::Error(e) if self.format != Format::StreamJson => {
                let _ = writeln!(
                    self.err,
                    "{} {}: {e}",
                    "[unharness]".red().bold(),
                    self.name
                );
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
                    let _ = writeln!(self.err, "{} interrupted", "[unharness]".yellow().bold());
                }
                130
            }
            Some(StopReason::Error(e)) => {
                if self.format == Format::Text {
                    let _ = writeln!(self.err, "{} {e}", "[unharness]".red().bold());
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
        }
    }
    run.settle();
    let _ = handle.send(SessionCommand::Shutdown).await;
    // Until the process is gone, the grace is up, or Ctrl+C. Whatever is
    // left is killed when the runtime goes (`kill_on_drop`).
    let drain = async {
        while let Some(ev) = handle.events.recv().await {
            let exited = matches!(ev, AgentEvent::ProcessExited { .. });
            run.on_event(ev);
            if exited {
                break;
            }
        }
    };
    tokio::select! {
        _ = tokio::time::timeout(SHUTDOWN_GRACE, drain) => {}
        _ = interrupts.recv() => {}
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
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok(),
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

        // One that ends during the turn is reported in it.
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
        assert!(run.is_done());

        // Codex: nothing follows the end.
        let (_dir, mut run) = headless(Format::Text, false);
        feed(&mut run, vec![AgentEvent::TurnStarted, started, done()]);
        assert!(!run.is_done());
        feed(&mut run, vec![ended(Some("alpha"))]);
        assert!(run.is_done());
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
