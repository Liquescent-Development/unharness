//! Antigravity transports.
//!
//! `stream`: one `agy --print= --input-format stream-json --output-format
//! stream-json` process for the session; each turn is one NDJSON line on
//! stdin ("runs one turn per message in a single conversation", agy
//! changelog). The stdin message shape is undocumented; the Claude-compatible
//! shape is the default and `{"prompt": …}` is selectable.
//!
//! `per-turn`: one `agy --print=<text>` child per turn, resumed with
//! `--conversation <id>` once the id is known (else `--continue`).

use anyhow::Result;
use serde_json::json;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::parse::AgyParser;
use crate::core::per_turn::{PerTurnProtocol, TurnParser, TurnSpec, TurnState};
use crate::core::process::{LineProcess, RawLine};
use crate::core::{
    AgentEvent, HarnessId, PermissionPolicy, ProcessModel, SessionCommand, SessionConfig,
    SessionHandle, SessionInfo, StopReason,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgyTransport {
    /// Long-lived stream-json session, Claude-shaped stdin messages.
    #[default]
    Stream,
    /// Long-lived stream-json session, `{"prompt": …}` stdin messages.
    StreamPrompt,
    /// One child per turn.
    PerTurn,
}

impl AgyTransport {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "stream" | "stream-json" | "auto" => Some(AgyTransport::Stream),
            "stream-prompt" => Some(AgyTransport::StreamPrompt),
            "per-turn" | "perturn" | "exec" => Some(AgyTransport::PerTurn),
            _ => None,
        }
    }
}

pub fn policy_args(policy: PermissionPolicy) -> Vec<&'static str> {
    match policy {
        PermissionPolicy::Ask => vec![],
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => vec!["--mode", "accept-edits"],
        PermissionPolicy::Bypass => vec!["--dangerously-skip-permissions"],
    }
}

/// Common flags shared by both transports.
fn common_args(
    model: Option<&str>,
    effort: Option<&str>,
    policy: PermissionPolicy,
    extra: &[String],
) -> Vec<String> {
    let mut args = vec![
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--print-timeout".to_string(),
        "0".to_string(),
    ];
    args.extend(policy_args(policy).iter().map(|s| s.to_string()));
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        args.push("--effort".into());
        args.push(e.to_string());
    }
    args.extend(extra.iter().cloned());
    args
}

pub fn stream_args(cfg: &SessionConfig) -> Vec<String> {
    let mut args = vec![
        "--print=".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
    ];
    args.extend(common_args(
        cfg.model.as_ref().map(|m| m.model.as_str()),
        cfg.effort.as_deref(),
        cfg.policy,
        &cfg.extra_args,
    ));
    if let Some(id) = &cfg.resume {
        args.push("--conversation".into());
        args.push(id.clone());
    }
    args
}

pub fn encode_turn(transport: AgyTransport, text: &str) -> String {
    match transport {
        AgyTransport::StreamPrompt => json!({"prompt": text}).to_string(),
        _ => json!({"type":"user","message":{"role":"user","content":text}}).to_string(),
    }
}

pub fn start_stream(cfg: SessionConfig, transport: AgyTransport) -> Result<SessionHandle> {
    let mut cmd = Command::new(&cfg.binary);
    cmd.args(stream_args(&cfg)).current_dir(&cfg.cwd);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let proc = LineProcess::spawn(cmd)?;
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness: HarnessId::Agy,
        process_model: ProcessModel::LongLived,
    });
    tokio::spawn(drive(proc, cfg, transport, events_tx, cmd_rx));
    Ok(handle)
}

fn spawn_stream(cfg: &SessionConfig) -> Result<LineProcess> {
    let mut cmd = Command::new(&cfg.binary);
    cmd.args(stream_args(cfg)).current_dir(&cfg.cwd);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    LineProcess::spawn(cmd)
}

async fn drive(
    mut proc: LineProcess,
    mut cfg: SessionConfig,
    transport: AgyTransport,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
) {
    let resume = cfg.resume.clone();
    let mut parser = AgyParser::new(resume.clone());
    let mut turn_open = false;
    let mut shutting_down = false;
    // Model/effort/policy are process flags; a change restarts the process
    // on the next turn, resuming the conversation by id.
    let mut restart_needed = false;
    let mut session_id = resume.clone();
    if let Some(id) = resume {
        let _ = events
            .send(AgentEvent::SessionStarted {
                session_id: id,
                model: None,
            })
            .await;
    }

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    proc.kill().await;
                    return;
                };
                match cmd {
                    SessionCommand::SendTurn { text } => {
                        if restart_needed {
                            restart_needed = false;
                            proc.kill().await;
                            cfg.resume = session_id.clone();
                            match spawn_stream(&cfg) {
                                Ok(p) => {
                                    proc = p;
                                    parser = AgyParser::new(session_id.clone());
                                    let _ = events.send(AgentEvent::Notice(
                                        "agy restarted with the new settings".into(),
                                    )).await;
                                }
                                Err(e) => {
                                    let _ = events.send(AgentEvent::TurnCompleted {
                                        stop_reason: StopReason::Error(format!("agy restart failed: {e}")),
                                    }).await;
                                    continue;
                                }
                            }
                        }
                        turn_open = true;
                        if let Err(e) = proc.write_line(&encode_turn(transport, &text)).await {
                            turn_open = false;
                            let _ = events.send(AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Error(format!("agy: {e}")),
                            }).await;
                        }
                    }
                    SessionCommand::Interrupt => {
                        // No interrupt message exists in the protocol; ending the
                        // process ends the turn. The session can be resumed by id.
                        let _ = events.send(AgentEvent::Notice(
                            "agy has no interrupt; stopping the process (resume with --conversation)".into(),
                        )).await;
                        proc.kill().await;
                    }
                    SessionCommand::RespondPermission { .. } => {
                        let _ = events.send(AgentEvent::Error(
                            "agy cannot answer permission prompts in headless mode".into(),
                        )).await;
                    }
                    SessionCommand::SetModel(m) => {
                        cfg.model = Some(m);
                        restart_needed = true;
                    }
                    SessionCommand::SetEffort(e) => {
                        cfg.effort = e;
                        restart_needed = true;
                    }
                    SessionCommand::SetPolicy(p) => {
                        cfg.policy = p;
                        restart_needed = true;
                    }
                    SessionCommand::Shutdown => {
                        shutting_down = true;
                        proc.close_stdin();
                    }
                }
            }
            raw = proc.lines.recv() => {
                match raw {
                    Some(RawLine::Stdout(line)) => {
                        for ev in parser.feed(&line) {
                            match &ev {
                                AgentEvent::TurnCompleted { .. } => turn_open = false,
                                AgentEvent::SessionStarted { session_id: id, .. } => {
                                    session_id = Some(id.clone());
                                }
                                _ => {}
                            }
                            if events.send(ev).await.is_err() {
                                proc.kill().await;
                                return;
                            }
                        }
                    }
                    Some(RawLine::Stderr(line)) => {
                        for ev in parser.feed_stderr(&line) {
                            let _ = events.send(ev).await;
                        }
                    }
                    Some(RawLine::Exited(code)) => {
                        if turn_open {
                            let stop_reason = match code {
                                Some(0) | None => StopReason::Interrupted,
                                Some(c) => StopReason::Error(format!("agy exited with code {c}")),
                            };
                            let _ = events.send(AgentEvent::TurnCompleted { stop_reason }).await;
                        }
                        let _ = events.send(AgentEvent::ProcessExited { code }).await;
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    if !shutting_down {
        proc.kill().await;
    }
}

// ---------------------------------------------------------------------------

pub struct AgyPerTurn;

impl PerTurnProtocol for AgyPerTurn {
    fn harness(&self) -> HarnessId {
        HarnessId::Agy
    }

    fn build_turn(&self, state: &TurnState, text: &str) -> Result<TurnSpec> {
        let mut command = Command::new(&state.binary);
        command.current_dir(&state.cwd);
        command.arg(format!("--print={text}"));
        command.args(common_args(
            state.model.as_ref().map(|m| m.model.as_str()),
            state.effort.as_deref(),
            state.policy,
            &state.extra_args,
        ));
        match &state.session_id {
            Some(id) => {
                command.arg("--conversation").arg(id);
            }
            None if state.turn_index > 0 => {
                command.arg("--continue");
            }
            None => {}
        }
        for (k, v) in &state.env {
            command.env(k, v);
        }
        Ok(TurnSpec {
            command,
            stdin: None,
        })
    }

    fn new_parser(&self) -> Box<dyn TurnParser> {
        Box::new(PerTurnParser(AgyParser::new(None)))
    }
}

struct PerTurnParser(AgyParser);

impl TurnParser for PerTurnParser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        self.0.feed(line)
    }
    fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        self.0.feed_stderr(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ModelRef;
    use std::path::PathBuf;

    fn cfg() -> SessionConfig {
        SessionConfig {
            binary: PathBuf::from("agy"),
            cwd: PathBuf::from("/tmp"),
            model: Some(ModelRef::new(
                HarnessId::Agy,
                "google",
                "gemini-3.8-flash-high",
            )),
            effort: Some("high".into()),
            policy: PermissionPolicy::AcceptEdits,
            resume: Some("conv-1".into()),
            extra_args: vec!["--add-dir".into(), "/x".into()],
            env: vec![],
        }
    }

    #[test]
    fn stream_args_shape() {
        let a = stream_args(&cfg()).join(" ");
        assert_eq!(
            a,
            "--print= --input-format stream-json --output-format stream-json --print-timeout 0 --mode accept-edits --model gemini-3.8-flash-high --effort high --add-dir /x --conversation conv-1"
        );
    }

    #[test]
    fn turn_encodings() {
        assert_eq!(
            encode_turn(AgyTransport::Stream, "hi"),
            r#"{"message":{"content":"hi","role":"user"},"type":"user"}"#
        );
        assert_eq!(
            encode_turn(AgyTransport::StreamPrompt, "hi"),
            r#"{"prompt":"hi"}"#
        );
        assert_eq!(AgyTransport::parse("per_turn"), Some(AgyTransport::PerTurn));
        assert_eq!(
            AgyTransport::parse("stream-prompt"),
            Some(AgyTransport::StreamPrompt)
        );
    }

    #[test]
    fn per_turn_args() {
        let argv = |state: &TurnState| -> String {
            let spec = AgyPerTurn.build_turn(state, "do it").unwrap();
            spec.command
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut state = TurnState {
            binary: PathBuf::from("agy"),
            cwd: PathBuf::from("/tmp"),
            model: None,
            effort: None,
            policy: PermissionPolicy::Bypass,
            session_id: None,
            extra_args: vec![],
            env: vec![],
            turn_index: 0,
        };
        assert_eq!(
            argv(&state),
            "--print=do it --output-format stream-json --print-timeout 0 --dangerously-skip-permissions"
        );
        state.turn_index = 1;
        assert!(argv(&state).ends_with("--continue"));
        state.session_id = Some("c9".into());
        assert!(argv(&state).ends_with("--conversation c9"));
    }
}
