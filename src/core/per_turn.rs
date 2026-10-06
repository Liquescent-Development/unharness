//! Spawn-per-turn session driver.
//!
//! For harnesses whose CLI has no long-lived stdin protocol (codex
//! `exec`), every turn is a fresh child process resumed by id. The driver still presents the same `SessionHandle`
//! as the long-lived transports.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::caps::PermissionPolicy;
use super::event::{AgentEvent, StopReason};
use super::ids::{HarnessId, ModelRef, ProviderId};
use super::mcp::McpServer;
use super::process::{LineProcess, RawLine};
use super::sandbox::Sandbox;
use super::session::{
    Attachment, ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo,
};

/// Mutable per-session state a protocol needs to build the next turn.
#[derive(Debug, Clone)]
pub struct TurnState {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub model: Option<ModelRef>,
    /// As `SessionConfig::provider`.
    pub provider: Option<ProviderId>,
    pub effort: Option<String>,
    pub policy: PermissionPolicy,
    /// Session/thread id to resume, once known.
    pub session_id: Option<String>,
    pub extra_args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mcp_servers: Vec<McpServer>,
    pub sandbox: Sandbox,
    pub turn_index: usize,
}

/// What one turn's child process looks like.
pub struct TurnSpec {
    pub command: Command,
    /// Written to the child's stdin, which is then closed.
    pub stdin: Option<String>,
}

/// Parses one child's output lines into events.
pub trait TurnParser: Send {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent>;
    fn feed_stderr(&mut self, _line: &str) -> Vec<AgentEvent> {
        Vec::new()
    }
}

pub trait PerTurnProtocol: Send + Sync {
    fn harness(&self) -> HarnessId;
    fn build_turn(
        &self,
        state: &TurnState,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<TurnSpec>;
    fn new_parser(&self) -> Box<dyn TurnParser>;
}

pub fn start(cfg: SessionConfig, protocol: Arc<dyn PerTurnProtocol>) -> Result<SessionHandle> {
    let state = TurnState {
        binary: cfg.binary,
        cwd: cfg.cwd,
        model: cfg.model,
        provider: cfg.provider,
        effort: cfg.effort,
        policy: cfg.policy,
        session_id: cfg.resume,
        extra_args: cfg.extra_args,
        env: cfg.env,
        mcp_servers: cfg.mcp_servers,
        sandbox: cfg.sandbox,
        turn_index: 0,
    };
    let (handle, events_tx, cmd_rx) = SessionHandle::channels(SessionInfo {
        harness: protocol.harness(),
        process_model: ProcessModel::PerTurn,
    });
    tokio::spawn(drive(state, protocol, events_tx, cmd_rx));
    Ok(handle)
}

async fn drive(
    mut state: TurnState,
    protocol: Arc<dyn PerTurnProtocol>,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
) {
    // Announce the resumed session immediately so the TUI can label it.
    if let Some(id) = &state.session_id {
        let _ = events
            .send(AgentEvent::SessionStarted {
                session_id: id.clone(),
                model: state.model.as_ref().map(|m| m.model.clone()),
            })
            .await;
    }

    let mut current: Option<LineProcess> = None;
    let mut parser: Option<Box<dyn TurnParser>> = None;
    let mut turn_completed = false;

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    if let Some(mut p) = current.take() { p.kill().await; }
                    return;
                };
                match cmd {
                    // No subagents are reported here, so none can be running.
                    SessionCommand::StopSubagent { .. } => {}
                    SessionCommand::Steer { .. }
                    | SessionCommand::Compact { .. }
                    | SessionCommand::Rewind { .. } => {
                        let _ = events.send(AgentEvent::Error(
                            "this harness cannot steer, compact or rewind a session".into(),
                        )).await;
                    }
                    SessionCommand::SendTurn { text, attachments } => {
                        if current.is_some() {
                            if turn_completed {
                                // The parser saw the turn end but the child has
                                // not been reaped yet; finish it off.
                                if let Some(mut p) = current.take() {
                                    p.kill().await;
                                }
                            } else {
                                let _ = events
                                    .send(AgentEvent::Error("a turn is already running".into()))
                                    .await;
                                continue;
                            }
                        }
                        match protocol.build_turn(&state, &text, &attachments) {
                            Ok(spec) => match LineProcess::spawn(spec.command, &state.sandbox) {
                                Ok(mut proc) => {
                                    if let Some(input) = spec.stdin
                                        && let Err(e) = proc.write_line(&input).await
                                    {
                                            let _ = events.send(AgentEvent::Error(format!("stdin: {e}"))).await;
                                    }
                                    proc.close_stdin();
                                    state.turn_index += 1;
                                    turn_completed = false;
                                    parser = Some(protocol.new_parser());
                                    current = Some(proc);
                                    let _ = events.send(AgentEvent::TurnStarted).await;
                                }
                                Err(e) => {
                                    let _ = events.send(AgentEvent::TurnCompleted { stop_reason: StopReason::Error(e.to_string()) }).await;
                                }
                            },
                            Err(e) => {
                                let _ = events.send(AgentEvent::TurnCompleted { stop_reason: StopReason::Error(e.to_string()) }).await;
                            }
                        }
                    }
                    SessionCommand::Interrupt => {
                        if let Some(p) = current.as_mut() {
                            p.kill().await;
                            // Exited arrives on the line channel and finishes the turn.
                        }
                    }
                    SessionCommand::RespondPermission { .. } => {
                        let _ = events.send(AgentEvent::Error(
                            "this harness cannot answer permission prompts interactively".into(),
                        )).await;
                    }
                    SessionCommand::SetModel(m) => state.model = Some(m),
                    SessionCommand::SetEffort(e) => state.effort = e,
                    SessionCommand::SetPolicy(p) => state.policy = p,
                    SessionCommand::Shutdown => {
                        if let Some(mut p) = current.take() { p.kill().await; }
                        let _ = events.send(AgentEvent::ProcessExited { code: Some(0) }).await;
                        return;
                    }
                }
            }
            raw = async {
                match current.as_mut() {
                    Some(p) => p.lines.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(raw) = raw else {
                    current = None;
                    continue;
                };
                let Some(parser_ref) = parser.as_mut() else { continue };
                let evs = match &raw {
                    RawLine::Stdout(l) => parser_ref.feed(l),
                    RawLine::Stderr(l) => parser_ref.feed_stderr(l),
                    RawLine::Exited(code) => {
                        current = None;
                        let mut evs = Vec::new();
                        if !turn_completed {
                            let stop_reason = match code {
                                Some(0) => StopReason::Done,
                                None => StopReason::Interrupted,
                                Some(c) => StopReason::Error(format!("exit code {c}")),
                            };
                            evs.push(AgentEvent::TurnCompleted { stop_reason });
                        }
                        evs
                    }
                };
                for ev in evs {
                    match &ev {
                        AgentEvent::SessionStarted { session_id, .. } => {
                            state.session_id = Some(session_id.clone());
                        }
                        AgentEvent::TurnCompleted { .. } => turn_completed = true,
                        // The driver already announced the turn when it spawned the child.
                        AgentEvent::TurnStarted => continue,
                        _ => {}
                    }
                    if events.send(ev).await.is_err() {
                        if let Some(mut p) = current.take() { p.kill().await; }
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Echo protocol: each turn runs `sh -c 'cat'` and every stdout line is a
    /// TextDelta; a line starting with `SID:` is a SessionStarted.
    struct Echo;
    struct EchoParser;
    impl TurnParser for EchoParser {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            if let Some(id) = line.strip_prefix("SID:") {
                vec![AgentEvent::SessionStarted {
                    session_id: id.to_string(),
                    model: None,
                }]
            } else {
                vec![AgentEvent::TextDelta(line.to_string())]
            }
        }
    }
    impl PerTurnProtocol for Echo {
        fn harness(&self) -> HarnessId {
            HarnessId::CODEX
        }
        fn build_turn(
            &self,
            state: &TurnState,
            text: &str,
            _attachments: &[Attachment],
        ) -> Result<TurnSpec> {
            let mut command = Command::new("sh");
            // Prints the resume id (if any), then echoes stdin.
            command.arg("-c").arg(format!(
                "echo 'SID:s{}'; [ -n \"$RESUME\" ] && echo \"resumed $RESUME\"; cat",
                state.turn_index + 1
            ));
            if let Some(id) = &state.session_id {
                command.env("RESUME", id);
            }
            Ok(TurnSpec {
                command,
                stdin: Some(text.to_string()),
            })
        }
        fn new_parser(&self) -> Box<dyn TurnParser> {
            Box::new(EchoParser)
        }
    }

    fn cfg() -> SessionConfig {
        SessionConfig {
            binary: PathBuf::from("sh"),
            cwd: std::env::temp_dir(),
            model: None,
            provider: None,
            effort: None,
            policy: PermissionPolicy::Ask,
            resume: None,
            fork: false,
            extra_args: vec![],
            env: vec![],
            mcp_servers: Vec::new(),
            sandbox: Sandbox::off(),
        }
    }

    async fn collect_turn(h: &mut SessionHandle) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), h.events.recv())
                .await
                .unwrap()
                .unwrap();
            let done = matches!(ev, AgentEvent::TurnCompleted { .. });
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn two_turns_resume_by_captured_session_id() {
        let mut h = start(cfg(), Arc::new(Echo)).unwrap();
        h.send(SessionCommand::turn("one")).await.unwrap();
        let t1 = collect_turn(&mut h).await;
        assert_eq!(t1[0], AgentEvent::TurnStarted);
        assert!(t1.contains(&AgentEvent::SessionStarted {
            session_id: "s1".into(),
            model: None
        }));
        assert!(t1.contains(&AgentEvent::TextDelta("one".into())));
        assert!(matches!(
            t1.last(),
            Some(AgentEvent::TurnCompleted {
                stop_reason: StopReason::Done
            })
        ));

        h.send(SessionCommand::turn("two")).await.unwrap();
        let t2 = collect_turn(&mut h).await;
        assert!(t2.contains(&AgentEvent::TextDelta("resumed s1".into())));

        h.send(SessionCommand::Shutdown).await.unwrap();
        let ev = h.events.recv().await.unwrap();
        assert!(matches!(ev, AgentEvent::ProcessExited { .. }));
    }

    #[tokio::test]
    async fn interrupt_kills_running_turn() {
        struct Sleeper;
        impl PerTurnProtocol for Sleeper {
            fn harness(&self) -> HarnessId {
                HarnessId::CODEX
            }
            fn build_turn(&self, _s: &TurnState, _t: &str, _a: &[Attachment]) -> Result<TurnSpec> {
                let mut command = Command::new("sh");
                command.arg("-c").arg("sleep 30");
                Ok(TurnSpec {
                    command,
                    stdin: None,
                })
            }
            fn new_parser(&self) -> Box<dyn TurnParser> {
                Box::new(EchoParser)
            }
        }
        let mut h = start(cfg(), Arc::new(Sleeper)).unwrap();
        h.send(SessionCommand::turn("x")).await.unwrap();
        assert_eq!(h.events.recv().await.unwrap(), AgentEvent::TurnStarted);
        h.send(SessionCommand::Interrupt).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), h.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                ev,
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Interrupted
                }
            ),
            "{ev:?}"
        );
    }
}
