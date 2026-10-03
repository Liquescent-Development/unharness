//! Session abstraction shared by long-lived and spawn-per-turn harnesses.
//!
//! A harness's `start_session` spawns a driver task that owns the child
//! process and returns a `SessionHandle`. The TUI only ever talks to the
//! handle: it reads `AgentEvent`s and sends `SessionCommand`s.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use super::caps::PermissionPolicy;
use super::event::{AgentEvent, PermissionDecision};
use super::ids::{HarnessId, ModelRef};

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub model: Option<ModelRef>,
    pub effort: Option<String>,
    /// Already resolved against the harness capabilities.
    pub policy: PermissionPolicy,
    /// Session id to resume, if any.
    pub resume: Option<String>,
    pub extra_args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    SendTurn {
        text: String,
    },
    Interrupt,
    RespondPermission {
        id: String,
        decision: PermissionDecision,
    },
    SetModel(ModelRef),
    SetEffort(Option<String>),
    SetPolicy(PermissionPolicy),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessModel {
    /// One child process for the whole session; turns go over stdin.
    LongLived,
    /// A fresh child per turn, resumed by id.
    PerTurn,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub harness: HarnessId,
    pub process_model: ProcessModel,
}

/// Capacity of the event channel from driver to TUI.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;
/// Capacity of the command channel from TUI to driver.
pub const COMMAND_CHANNEL_CAPACITY: usize = 64;

pub struct SessionHandle {
    pub info: SessionInfo,
    pub events: mpsc::Receiver<AgentEvent>,
    cmd_tx: mpsc::Sender<SessionCommand>,
}

impl SessionHandle {
    /// Create the channel pair for a new session. The driver keeps `events_tx`
    /// and `cmd_rx`; the handle goes to the TUI.
    pub fn channels(
        info: SessionInfo,
    ) -> (
        SessionHandle,
        mpsc::Sender<AgentEvent>,
        mpsc::Receiver<SessionCommand>,
    ) {
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        (
            SessionHandle {
                info,
                events: events_rx,
                cmd_tx,
            },
            events_tx,
            cmd_rx,
        )
    }

    pub async fn send(&self, cmd: SessionCommand) -> Result<()> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| anyhow!("session driver has exited"))
    }

    pub fn try_send(&self, cmd: SessionCommand) -> Result<()> {
        self.cmd_tx
            .try_send(cmd)
            .map_err(|e| anyhow!("could not send session command: {}", e))
    }

    /// True while the driver task is still alive.
    pub fn is_alive(&self) -> bool {
        !self.cmd_tx.is_closed()
    }
}
