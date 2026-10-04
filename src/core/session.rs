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
    /// Branch a new session off `resume` instead of reattaching to it
    /// (`Capabilities::fork`); the original session is left untouched.
    pub fork: bool,
    pub extra_args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Something sent along with a turn's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    Image { path: PathBuf, mime: String },
}

impl Attachment {
    /// An image attachment, if the extension is one the vendors accept.
    pub fn image(path: impl Into<PathBuf>) -> Option<Self> {
        let path = path.into();
        let mime = match path.extension()?.to_str()?.to_lowercase().as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return None,
        };
        Some(Attachment::Image {
            path,
            mime: mime.to_string(),
        })
    }

    pub fn path(&self) -> &std::path::Path {
        match self {
            Attachment::Image { path, .. } => path,
        }
    }

    pub fn mime(&self) -> &str {
        match self {
            Attachment::Image { mime, .. } => mime,
        }
    }

    /// File name for display.
    pub fn label(&self) -> String {
        self.path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path().display().to_string())
    }

    /// The file's bytes, base64-encoded, for protocols that inline images.
    pub fn read_base64(&self) -> Result<String> {
        use base64::Engine;
        let bytes = std::fs::read(self.path())
            .map_err(|e| anyhow!("could not read {}: {e}", self.path().display()))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    SendTurn {
        text: String,
        attachments: Vec<Attachment>,
    },
    /// Inject a message into the running turn (`Capabilities::steer`).
    Steer {
        text: String,
        attachments: Vec<Attachment>,
    },
    /// Drop the user turn `anchor` (a `TurnAnchor` id) and everything after
    /// it from the session (`Capabilities::rewind`). Files are not touched.
    Rewind {
        anchor: String,
    },
    /// Summarise the context now (`Capabilities::compaction`).
    Compact {
        instructions: Option<String>,
    },
    Interrupt,
    /// Stop one running subagent, named by the tool call that spawned it
    /// (`Capabilities::subagents.stop`).
    StopSubagent {
        id: String,
    },
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

impl SessionCommand {
    /// A text-only turn.
    pub fn turn(text: impl Into<String>) -> Self {
        SessionCommand::SendTurn {
            text: text.into(),
            attachments: Vec::new(),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_attachment_by_extension() {
        assert_eq!(
            Attachment::image("/x/shot.PNG"),
            Some(Attachment::Image {
                path: "/x/shot.PNG".into(),
                mime: "image/png".into()
            })
        );
        assert_eq!(Attachment::image("/x/notes.txt"), None);
        assert_eq!(Attachment::image("/x/noext"), None);
    }
}
