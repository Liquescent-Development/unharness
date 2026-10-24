//! The per-turn flavour of the driver.
//!
//! For harnesses whose CLI has no long-lived stdin protocol (codex
//! `exec`), every turn is a fresh child process resumed by id. The driver still presents the same `SessionHandle`
//! as the long-lived transports, and answers a command it cannot carry out
//! as the long-lived loop does: one event, worded from the harness's
//! [`Capabilities`], which ends what the TUI waits on after such a
//! command (a compaction's turn, a rewind).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{asks, exited};
use crate::core::caps::{Capabilities, PermissionPolicy};
use crate::core::event::{AgentEvent, CapsUpdate, StopReason};
use crate::core::ids::{HarnessId, ModelRef, ProviderId};
use crate::core::mcp::McpServer;
use crate::core::parser::Parser;
use crate::core::process::{LineProcess, RawLine};
use crate::core::sandbox::Sandbox;
use crate::core::session::{
    Attachment, ProcessModel, ProcessSlot, SessionCommand, SessionConfig, SessionHandle,
    SessionInfo,
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

pub trait PerTurnProtocol: Send + Sync {
    fn harness(&self) -> HarnessId;
    fn build_turn(
        &self,
        state: &TurnState,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<TurnSpec>;
    fn new_parser(&self) -> Box<dyn Parser>;

    /// What the session reports of its capabilities before its first
    /// turn, on top of what its harness declares (Codex under `transport
    /// = "auto"` that came to `exec`: what it lacks of app-server's). Told
    /// again after `Restarted`, with which the TUI forgets the policies a
    /// session refused, as for a CLI started anew.
    fn reported_caps(&self) -> Option<CapsUpdate> {
        None
    }
}

/// Drive `protocol` for the session whose handle this returns. `caps` is
/// what its harness declares.
///
/// A CLI run once per turn branches no session: one asked to fork
/// (`SessionConfig::fork`) starts new, and says so (`SessionReplaced`),
/// rather than going on in the session it was to branch (Codex `exec`,
/// where `transport = "auto"` declared app-server's fork).
pub fn start(
    cfg: SessionConfig,
    caps: &Capabilities,
    protocol: Arc<dyn PerTurnProtocol>,
) -> Result<SessionHandle> {
    let unforked = (cfg.fork && cfg.resume.is_some()).then(|| {
        format!(
            "could not fork the session ({} cannot fork one)",
            protocol.harness()
        )
    });
    let state = TurnState {
        binary: cfg.binary,
        cwd: cfg.cwd,
        model: cfg.model,
        provider: cfg.provider,
        effort: cfg.effort,
        policy: cfg.policy,
        session_id: cfg.resume.filter(|_| unforked.is_none()),
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
    let slot = handle.process_slot();
    tokio::spawn(drive(
        state,
        caps.clone(),
        protocol,
        slot,
        events_tx,
        cmd_rx,
        unforked,
    ));
    Ok(handle)
}

/// `unforked`: why the session runs new instead of the fork it was asked
/// for.
async fn drive(
    mut state: TurnState,
    caps: Capabilities,
    protocol: Arc<dyn PerTurnProtocol>,
    slot: ProcessSlot,
    events: mpsc::Sender<AgentEvent>,
    mut cmds: mpsc::Receiver<SessionCommand>,
    unforked: Option<String>,
) {
    let reported = protocol.reported_caps();
    if let Some(update) = &reported {
        let _ = events
            .send(AgentEvent::CapabilitiesChanged(update.clone()))
            .await;
    }
    if let Some(reason) = unforked {
        let _ = events.send(AgentEvent::SessionReplaced { reason }).await;
    }
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
    let mut parser: Option<Box<dyn Parser>> = None;
    let mut turn_completed = false;

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    if let Some(mut p) = current.take() { p.kill().await; }
                    return;
                };
                let (refusal, offered) = asks(&cmd, &caps);
                if !offered {
                    let _ = events.send(refusal.event(protocol.harness())).await;
                    continue;
                }
                match cmd {
                    // What a CLI run once per turn cannot do, whatever its
                    // harness declares.
                    SessionCommand::StopSubagent { .. }
                    | SessionCommand::RemoteControl { .. }
                    | SessionCommand::QueryStatus
                    | SessionCommand::Steer { .. }
                    | SessionCommand::SendNow { .. }
                    | SessionCommand::Compact { .. }
                    | SessionCommand::Rewind { .. }
                    | SessionCommand::RespondPermission { .. } => {
                        let _ = events.send(refusal.event(protocol.harness())).await;
                    }
                    SessionCommand::SendTurn { text, attachments } => {
                        if current.is_some() {
                            if turn_completed {
                                // The parser saw the turn end but the child has
                                // not been reaped yet; finish it off, and
                                // start the next once it is gone (#286).
                                if let Some(mut p) = current.take() {
                                    p.kill().await;
                                    exited(&mut p).await;
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
                                    // Before the prompt is written, which
                                    // a CLI that does not read can hold up.
                                    slot.set(&proc);
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
                    SessionCommand::SetModel(m) => state.model = Some(m),
                    SessionCommand::SetEffort(e) => state.effort = e,
                    SessionCommand::SetPolicy(p) => state.policy = p,
                    // The next turn's CLI starts on it: the session goes on
                    // in a new process, as it does every turn.
                    SessionCommand::SetProvider(p) => {
                        state.provider = Some(p);
                        state.model = None;
                        let _ = events.send(AgentEvent::Restarted).await;
                        if let Some(update) = &reported {
                            let _ = events
                                .send(AgentEvent::CapabilitiesChanged(update.clone()))
                                .await;
                        }
                    }
                    SessionCommand::Shutdown => {
                        if let Some(mut p) = current.take() {
                            p.kill().await;
                            // Gone before it is said to be: the next
                            // session resumes the thread it was writing.
                            exited(&mut p).await;
                        }
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
                    RawLine::Stderr(l) => {
                        parser_ref.feed_stderr(crate::core::process::stderr_line(l))
                    }
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
    impl Parser for EchoParser {
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
        fn new_parser(&self) -> Box<dyn Parser> {
            Box::new(EchoParser)
        }
    }

    /// Each turn runs a CLI that never reads its stdin.
    struct Deaf;
    impl PerTurnProtocol for Deaf {
        fn harness(&self) -> HarnessId {
            HarnessId::CODEX
        }
        fn build_turn(
            &self,
            _state: &TurnState,
            text: &str,
            _attachments: &[Attachment],
        ) -> Result<TurnSpec> {
            let mut command = Command::new("sleep");
            command.arg("30");
            Ok(TurnSpec {
                command,
                stdin: Some(text.to_string()),
            })
        }
        fn new_parser(&self) -> Box<dyn Parser> {
            Box::new(EchoParser)
        }
    }

    // A prompt larger than a pipe holds the driver on writing it to a CLI
    // that does not read; the session's process can still be killed.
    #[tokio::test]
    async fn a_turn_stuck_on_its_prompt_can_be_killed_through_the_session() {
        let mut h = start(cfg(), &caps(), Arc::new(Deaf)).unwrap();
        h.send(SessionCommand::turn("x".repeat(1 << 20)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.process_slot().kill();
        // The write fails, and the turn ends with its process.
        collect_turn(&mut h).await;
    }

    /// What a harness run once per turn can offer: an interrupt.
    fn caps() -> Capabilities {
        Capabilities {
            interrupt: true,
            ..Default::default()
        }
    }

    fn cfg() -> SessionConfig {
        SessionConfig {
            binary: PathBuf::from("sh"),
            cwd: std::env::temp_dir(),
            workspace_root: None,
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

    async fn next(h: &mut SessionHandle) -> AgentEvent {
        tokio::time::timeout(Duration::from_secs(5), h.events.recv())
            .await
            .unwrap()
            .unwrap()
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

    // What a CLI run once per turn cannot do is answered as the long-lived
    // loop answers it, also where the harness declares it, and so is what
    // the harness does not declare: one event each, and nothing runs. A
    // compaction ends the turn the TUI runs it as, and a rewind fails
    // (#288: Codex under `auto` come to `exec` left the TUI compacting).
    #[tokio::test]
    async fn a_command_it_cannot_carry_out_is_answered_once() {
        let mut every_but_interrupt = Capabilities {
            steer: true,
            compaction: true,
            interactive_permissions: true,
            remote_control: true,
            ..Default::default()
        };
        every_but_interrupt.rewind.conversation = true;
        every_but_interrupt.subagents.stop = true;
        let mut h = start(cfg(), &every_but_interrupt, Arc::new(Echo)).unwrap();
        for cmd in [
            SessionCommand::Steer {
                text: "x".into(),
                attachments: Vec::new(),
            },
            SessionCommand::Rewind { anchor: "a".into() },
            SessionCommand::Interrupt,
            SessionCommand::StopSubagent { id: "s".into() },
            SessionCommand::RespondPermission {
                id: "r".into(),
                decision: crate::core::PermissionDecision::Deny {
                    reason: "no".into(),
                },
            },
            SessionCommand::RemoteControl {
                enabled: true,
                name: None,
            },
            SessionCommand::Compact { instructions: None },
            SessionCommand::turn("after"),
        ] {
            h.send(cmd).await.unwrap();
        }
        let mut said = Vec::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), h.events.recv())
                .await
                .unwrap()
                .unwrap();
            if ev == AgentEvent::TurnStarted {
                break;
            }
            said.push(ev);
        }
        let notice = |what: &str| AgentEvent::Notice(format!("codex cannot {what}"));
        assert_eq!(
            said,
            [
                notice("steer a running turn"),
                AgentEvent::RewindFailed {
                    reason: "not supported".into()
                },
                notice("interrupt a turn"),
                notice("stop a subagent"),
                notice("take an answer to a permission request"),
                notice("go on remote control"),
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Error(
                        "codex cannot compact its context on request".into()
                    )
                },
            ]
        );
        let turn = collect_turn(&mut h).await;
        // Nothing reached a CLI before the turn: the first is the turn's.
        assert!(
            turn.contains(&AgentEvent::SessionStarted {
                session_id: "s1".into(),
                model: None
            }),
            "{turn:?}"
        );
        assert!(turn.contains(&AgentEvent::TextDelta("after".into())));
    }

    #[tokio::test]
    async fn two_turns_resume_by_captured_session_id() {
        let mut h = start(cfg(), &caps(), Arc::new(Echo)).unwrap();
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_turn_ended_with_its_session_is_gone_when_the_exit_is_reported() {
        struct Pid;
        impl PerTurnProtocol for Pid {
            fn harness(&self) -> HarnessId {
                HarnessId::CODEX
            }
            fn build_turn(&self, _s: &TurnState, _t: &str, _a: &[Attachment]) -> Result<TurnSpec> {
                let mut command = Command::new("sh");
                command.arg("-c").arg("echo SID:$$; exec sleep 30");
                Ok(TurnSpec {
                    command,
                    stdin: None,
                })
            }
            fn new_parser(&self) -> Box<dyn Parser> {
                Box::new(EchoParser)
            }
        }
        let mut h = start(cfg(), &caps(), Arc::new(Pid)).unwrap();
        h.send(SessionCommand::turn("x")).await.unwrap();
        let pid = loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), h.events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AgentEvent::SessionStarted { session_id, .. } = ev {
                break session_id.parse::<u32>().unwrap();
            }
        };
        h.send(SessionCommand::Shutdown).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(10), h.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, AgentEvent::ProcessExited { .. }), "{ev:?}");
        assert!(!crate::core::process::running(pid));
    }

    // The next turn's CLI starts only once the one before it is gone, also
    // when the parser saw that turn end before its CLI exited (#286: it was
    // killed, and the next started beside it).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_next_turn_starts_once_the_cli_before_it_is_gone() {
        struct Ends;
        impl Parser for Ends {
            fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
                match line {
                    "done" => vec![AgentEvent::TurnCompleted {
                        stop_reason: StopReason::Done,
                    }],
                    pid => vec![AgentEvent::TextDelta(pid.to_string())],
                }
            }
        }
        struct Lingers;
        impl PerTurnProtocol for Lingers {
            fn harness(&self) -> HarnessId {
                HarnessId::CODEX
            }
            fn build_turn(&self, _s: &TurnState, _t: &str, _a: &[Attachment]) -> Result<TurnSpec> {
                // Ends its turn, and has not exited when the next is asked for.
                let mut command = Command::new("sh");
                command.arg("-c").arg("echo $$; echo done; exec sleep 30");
                Ok(TurnSpec {
                    command,
                    stdin: None,
                })
            }
            fn new_parser(&self) -> Box<dyn Parser> {
                Box::new(Ends)
            }
        }
        let mut h = start(cfg(), &caps(), Arc::new(Lingers)).unwrap();
        h.send(SessionCommand::turn("one")).await.unwrap();
        let first = collect_turn(&mut h).await;
        let pid: u32 = first
            .iter()
            .find_map(|e| match e {
                AgentEvent::TextDelta(p) => p.parse().ok(),
                _ => None,
            })
            .unwrap();
        h.send(SessionCommand::turn("two")).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), h.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev, AgentEvent::TurnStarted);
        assert!(!crate::core::process::running(pid), "started beside it");
        h.send(SessionCommand::Shutdown).await.unwrap();
        while !matches!(
            tokio::time::timeout(Duration::from_secs(10), h.events.recv())
                .await
                .unwrap()
                .unwrap(),
            AgentEvent::ProcessExited { .. }
        ) {}
    }

    // A provider goes on the next turn's CLI, with no model of the one
    // before: the session goes on in a new process, as it does every turn,
    // which the TUI is told (`Restarted`) as for a CLI started again.
    #[tokio::test]
    async fn a_provider_goes_to_the_next_turn() {
        struct Says;
        impl PerTurnProtocol for Says {
            fn harness(&self) -> HarnessId {
                HarnessId::CODEX
            }
            fn build_turn(&self, s: &TurnState, _t: &str, _a: &[Attachment]) -> Result<TurnSpec> {
                let mut command = Command::new("sh");
                command.arg("-c").arg(format!(
                    "echo {} {}",
                    s.provider.as_ref().map_or("-", |p| p.as_str()),
                    s.model.as_ref().map_or("-", |m| m.model.as_str())
                ));
                Ok(TurnSpec {
                    command,
                    stdin: None,
                })
            }
            fn new_parser(&self) -> Box<dyn Parser> {
                Box::new(EchoParser)
            }
        }
        let mut cfg = cfg();
        cfg.model = Some(ModelRef::new(HarnessId::CODEX, "openai", "gpt-5"));
        let mut h = start(cfg, &caps(), Arc::new(Says)).unwrap();
        h.send(SessionCommand::SetProvider("llama".into()))
            .await
            .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), h.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev, AgentEvent::Restarted);
        h.send(SessionCommand::turn("x")).await.unwrap();
        let turn = collect_turn(&mut h).await;
        assert!(
            turn.contains(&AgentEvent::TextDelta("llama -".into())),
            "{turn:?}"
        );
    }

    // What the session reports of its capabilities comes before anything
    // else, and again after it says it was restarted on a provider: the
    // TUI forgets then what the session before refused (#288).
    #[tokio::test]
    async fn what_the_session_reports_of_its_capabilities_comes_first_and_after_a_restart() {
        struct Lacking;
        impl PerTurnProtocol for Lacking {
            fn harness(&self) -> HarnessId {
                HarnessId::CODEX
            }
            fn build_turn(&self, s: &TurnState, t: &str, a: &[Attachment]) -> Result<TurnSpec> {
                Echo.build_turn(s, t, a)
            }
            fn new_parser(&self) -> Box<dyn Parser> {
                Box::new(EchoParser)
            }
            fn reported_caps(&self) -> Option<CapsUpdate> {
                Some(CapsUpdate {
                    refused_policies: Some(vec![PermissionPolicy::Ask]),
                    ..Default::default()
                })
            }
        }
        let reported = AgentEvent::CapabilitiesChanged(Lacking.reported_caps().unwrap());
        let mut resumed = cfg();
        resumed.resume = Some("t1".into());
        let mut h = start(resumed, &caps(), Arc::new(Lacking)).unwrap();
        assert_eq!(next(&mut h).await, reported);
        assert!(matches!(
            next(&mut h).await,
            AgentEvent::SessionStarted { .. }
        ));
        h.send(SessionCommand::SetProvider("llama".into()))
            .await
            .unwrap();
        assert_eq!(next(&mut h).await, AgentEvent::Restarted);
        assert_eq!(next(&mut h).await, reported);

        // A protocol that reports nothing says nothing.
        let mut h = start(cfg(), &caps(), Arc::new(Echo)).unwrap();
        h.send(SessionCommand::SetProvider("llama".into()))
            .await
            .unwrap();
        assert_eq!(next(&mut h).await, AgentEvent::Restarted);
        h.send(SessionCommand::Shutdown).await.unwrap();
        assert!(matches!(
            next(&mut h).await,
            AgentEvent::ProcessExited { .. }
        ));
    }

    // A session asked to fork starts new, and says so: its first turn
    // resumes nothing, rather than going on in the session it was to
    // branch (Codex `exec`, where `auto` declared app-server's fork).
    #[tokio::test]
    async fn a_session_asked_to_fork_runs_new_instead() {
        let mut forked = cfg();
        forked.resume = Some("s0".into());
        forked.fork = true;
        let mut h = start(forked, &caps(), Arc::new(Echo)).unwrap();
        assert_eq!(
            next(&mut h).await,
            AgentEvent::SessionReplaced {
                reason: "could not fork the session (codex cannot fork one)".into()
            }
        );
        h.send(SessionCommand::turn("one")).await.unwrap();
        let turn = collect_turn(&mut h).await;
        assert_eq!(turn[0], AgentEvent::TurnStarted);
        assert!(
            turn.contains(&AgentEvent::SessionStarted {
                session_id: "s1".into(),
                model: None
            }),
            "{turn:?}"
        );
        assert!(
            !turn
                .iter()
                .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.starts_with("resumed"))),
            "{turn:?}"
        );
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
            fn new_parser(&self) -> Box<dyn Parser> {
                Box::new(EchoParser)
            }
        }
        let mut h = start(cfg(), &caps(), Arc::new(Sleeper)).unwrap();
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
