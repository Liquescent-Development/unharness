//! What the event loop does on the controller's behalf: its [`Action`]s
//! carried out against the session, the CLIs asked to end ([`Ending`]),
//! and the `!` command's process.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use anyhow::{Context, Result};

use super::{Action, Controller, Front};
use crate::core::process::LineProcess;
use crate::core::{
    AgentEvent, HarnessId, McpServer, SessionCommand, SessionConfig, SessionHandle, StopReason,
    shell,
};
use crate::harness::resolve_binary;

/// Sessions asked to end whose CLI may still be running, and the actions
/// held until one of them is gone.
pub struct Ending {
    running: HashMap<HarnessId, usize>,
    gone_tx: tokio::sync::mpsc::UnboundedSender<HarnessId>,
    gone_rx: tokio::sync::mpsc::UnboundedReceiver<HarnessId>,
    pub held: VecDeque<Action>,
    /// How long a CLI asked to end may take before it is killed.
    patience: Duration,
}

impl Default for Ending {
    fn default() -> Self {
        Self::new()
    }
}

impl Ending {
    pub fn new() -> Self {
        let (gone_tx, gone_rx) = tokio::sync::mpsc::unbounded_channel();
        Ending {
            running: HashMap::new(),
            gone_tx,
            gone_rx,
            held: VecDeque::new(),
            // Its grace, its kill and its drain are bounded; this is in
            // case a driver never gets that far (stuck on a write).
            patience: crate::core::process::END_GRACE
                + crate::core::process::DRAIN_TIMEOUT
                + Duration::from_secs(1),
        }
    }

    /// Follow `session`, which was asked to end, until its CLI is gone.
    /// Its events are read, so that the driver sees it to the end as it
    /// would with the handle kept, and dropped.
    pub fn follow(&mut self, session: SessionHandle) {
        let harness = session.info.harness;
        *self.running.entry(harness).or_default() += 1;
        let gone = self.gone_tx.clone();
        let patience = self.patience;
        tokio::spawn(async move {
            // All of it: its command channel closed, the driver would
            // stop reading the CLI and go.
            let mut session = session;
            let process = session.process_slot();
            let exited = async {
                while let Some(ev) = session.events.recv().await {
                    if matches!(ev, AgentEvent::ProcessExited { .. }) {
                        break;
                    }
                }
            };
            tokio::pin!(exited);
            if tokio::time::timeout(patience, &mut exited).await.is_err() {
                // Not to be resumed beside it, whatever its driver does.
                process.kill();
                let _ = tokio::time::timeout(
                    crate::core::process::DRAIN_TIMEOUT + Duration::from_secs(1),
                    exited,
                )
                .await;
            }
            let _ = gone.send(harness);
        });
    }

    /// Whether a CLI of `harness` asked to end may still be running.
    pub fn running(&self, harness: HarnessId) -> bool {
        self.running.get(&harness).is_some_and(|n| *n > 0)
    }

    /// The next of them to be gone.
    pub async fn recv(&mut self) -> HarnessId {
        match self.gone_rx.recv().await {
            Some(harness) => harness,
            // A sender is kept here.
            None => std::future::pending().await,
        }
    }

    pub fn gone(&mut self, harness: HarnessId) {
        if let Some(n) = self.running.get_mut(&harness) {
            *n = n.saturating_sub(1);
        }
    }
}

/// Carry out what the controller asked for (`Controller::take_actions`),
/// and what was held until the CLI before was gone: start, prompt, command
/// and end `session`, and run or stop the `!` command (`shell`).
pub async fn run_actions(
    c: &mut Controller,
    front: &mut dyn Front,
    session: &mut Option<SessionHandle>,
    ending: &mut Ending,
    shell: &mut Option<LineProcess>,
) {
    let mut actions = std::mem::take(&mut ending.held);
    actions.extend(c.take_actions());
    c.start_held = false;
    while let Some(action) = actions.pop_front() {
        match action {
            Action::RunShell { command } => {
                // The sandbox the active harness's next process gets: a
                // command typed here is confined no less than the agent.
                let started = c
                    .session_sandbox()
                    .and_then(|sandbox| shell::spawn(&command, &c.cwd, &sandbox));
                match started {
                    Ok(p) => *shell = Some(p),
                    Err(e) => c.shell_failed(format!("{e:#}"), front),
                }
            }
            Action::StopShell => {
                if let Some(p) = shell.as_mut() {
                    p.kill().await;
                }
            }
            Action::StartSession { resume } => {
                if session.is_some() {
                    continue;
                }
                // The CLI that ended its session last (a sandbox or
                // provider change, a switch away, a resume) may still be
                // writing it, or hold its lock: it is resumed, or forked,
                // once that one is gone. What follows waits with it.
                if resume.is_some() && ending.running(c.active) {
                    // An interrupt can only be for the turn that waits:
                    // it is not sent at all.
                    if actions
                        .iter()
                        .any(|a| matches!(a, Action::Command(SessionCommand::Interrupt)))
                    {
                        actions.retain(|a| {
                            !matches!(
                                a,
                                Action::SendTurn { .. }
                                    | Action::Command(
                                        SessionCommand::Interrupt | SessionCommand::Steer { .. }
                                    )
                            )
                        });
                        c.on_event(
                            AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Interrupted,
                            },
                            front,
                        );
                    }
                    front.flash(format!(
                        "Waiting for the previous {} to exit",
                        c.short_name()
                    ));
                    actions.push_front(Action::StartSession { resume });
                    ending.held = actions;
                    c.start_held = true;
                    return;
                }
                let prepared = c.harness().prepare();
                // After `prepare`, which may itself create a watched file.
                c.arm_guard();
                if let Ok(Some(note)) = &prepared {
                    c.conversation.transcript.push_notice(note.clone());
                }
                c.show_session_notice();
                let mcp_servers = c.session_mcp_servers();
                match prepared.and_then(|_| start_session(c, resume, mcp_servers)) {
                    Ok(handle) => {
                        *session = Some(handle);
                        c.session_alive = true;
                        c.session_sandbox_level = Some(c.sandbox_level().0);
                        c.sandbox_restart_due = false;
                        c.sandbox_started();
                        let policy = c.effective_policy(c.active);
                        c.policy.started_under(policy);
                        // Remote Control the user had on, on the process before.
                        if let Some(cmd) = c.remote_control_resend() {
                            actions.push_front(Action::Command(cmd));
                        }
                    }
                    Err(e) => {
                        let why = format!("could not start {}: {e:#}", c.short_name());
                        c.start_failed(why, front);
                    }
                }
            }
            Action::SendTurn { text, attachments } => {
                if let Some(s) = session.as_ref() {
                    if let Err(e) = s.send(SessionCommand::SendTurn { text, attachments }).await {
                        c.on_event(
                            AgentEvent::TurnCompleted {
                                stop_reason: StopReason::Error(e.to_string()),
                            },
                            front,
                        );
                    }
                } else if c.is_generating {
                    // StartSession failed; the error was already reported.
                }
            }
            Action::Command(cmd) => {
                if let Some(s) = session.as_ref()
                    && s.send(cmd).await.is_err()
                {
                    c.driver_gone(front);
                    *session = None;
                }
            }
            Action::Shutdown => {
                if let Some(s) = session.take() {
                    let _ = s.send(SessionCommand::Shutdown).await;
                    ending.follow(s);
                }
                c.session_alive = false;
            }
        }
    }
}

fn start_session(
    c: &Controller,
    resume: Option<String>,
    mcp_servers: Vec<McpServer>,
) -> Result<SessionHandle> {
    let harness = c.harness();
    let desc = harness.descriptor();
    let binary = resolve_binary(desc, c.config.binary_override(desc.id.as_str()))
        .with_context(|| format!("{} not found on PATH", desc.binary_names.join("/")))?;
    let cfg = SessionConfig {
        binary,
        cwd: c.cwd.clone(),
        workspace_root: c.workspace_root.clone(),
        model: c.model(c.active).cloned(),
        provider: c.chosen_provider(c.active).cloned(),
        effort: c.effort(c.active).map(str::to_string),
        policy: c
            .effective_policy(c.active)
            .context("no permission policy is chosen")?,
        fork: resume.is_some() && c.fork_pending(c.active),
        resume: if c.caps().resume_by_id { resume } else { None },
        extra_args: c.config.extra_args(desc.id.as_str()).to_vec(),
        env: Vec::new(),
        mcp_servers,
        sandbox: c.session_sandbox()?,
    };
    harness.start_session(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::process::RawLine;
    use crate::core::{SessionInfo, session::ProcessModel};

    // A driver stuck before its CLI ends, holding it: the hold does not
    // lift with the CLI still there.
    #[tokio::test]
    async fn a_cli_that_outlasts_the_hold_is_killed_before_it_lifts() {
        let (handle, _events, _cmds) = SessionHandle::channels(SessionInfo {
            harness: HarnessId::CLAUDE,
            process_model: ProcessModel::LongLived,
        });
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("exec sleep 30");
        let mut cli = LineProcess::spawn(cmd, &crate::core::sandbox::Sandbox::off()).unwrap();
        handle.process_slot().set(&cli);
        let mut ending = Ending::new();
        ending.patience = Duration::from_millis(200);
        ending.follow(handle);
        let gone = tokio::time::timeout(Duration::from_secs(10), ending.recv())
            .await
            .expect("the hold never lifted");
        assert_eq!(gone, HarnessId::CLAUDE);
        let exited = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(line) = cli.lines.recv().await {
                if matches!(line, RawLine::Exited(_)) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(exited, Ok(true), "the CLI was left running");
    }
}
