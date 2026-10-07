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
use super::ids::{HarnessId, ModelRef, ProviderId};
use super::mcp::McpServer;
use super::sandbox::Sandbox;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub model: Option<ModelRef>,
    /// The provider the user chose (`--provider`, `default_provider`, the
    /// picker); `None` leaves it to the harness's own configuration.
    pub provider: Option<ProviderId>,
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
    /// MCP servers for this session, already limited to what the harness
    /// declares it takes (`mcp::for_harness`).
    pub mcp_servers: Vec<McpServer>,
    /// What confines the harness process, every time it is spawned.
    pub sandbox: Sandbox,
}

/// Something sent along with a turn's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    Image {
        path: PathBuf,
        mime: String,
    },
    /// A document: a PDF (`application/pdf`) or a text file (`text/plain`).
    File {
        path: PathBuf,
        mime: String,
    },
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

    /// A document attachment: a PDF by extension, or any file whose content
    /// is text. Reads the file to tell.
    pub fn file(path: impl Into<PathBuf>) -> Option<Self> {
        let path = path.into();
        let pdf = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let mime = if pdf {
            "application/pdf"
        } else {
            std::str::from_utf8(&std::fs::read(&path).ok()?).ok()?;
            "text/plain"
        };
        Some(Attachment::File {
            path,
            mime: mime.to_string(),
        })
    }

    /// An image by its extension, a document otherwise.
    pub fn from_path(path: impl Into<PathBuf>) -> Option<Self> {
        let path = path.into();
        Self::image(&path).or_else(|| Self::file(path))
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Attachment::Image { .. })
    }

    pub fn path(&self) -> &std::path::Path {
        match self {
            Attachment::Image { path, .. } | Attachment::File { path, .. } => path,
        }
    }

    pub fn mime(&self) -> &str {
        match self {
            Attachment::Image { mime, .. } | Attachment::File { mime, .. } => mime,
        }
    }

    /// File name for display.
    pub fn label(&self) -> String {
        self.path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path().display().to_string())
    }

    /// How the transcript marks it under the prompt.
    pub fn marker(&self) -> String {
        let kind = if self.is_image() { "image" } else { "file" };
        format!("[{kind}: {}]", self.label())
    }

    /// The file's content, for protocols that inline text documents.
    pub fn read_text(&self) -> Result<String> {
        std::fs::read_to_string(self.path())
            .map_err(|e| anyhow!("could not read {}: {e}", self.path().display()))
    }

    /// The file's bytes, base64-encoded, for protocols that inline them.
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

/// Whether `Shutdown` is among the commands still queued, taking them.
/// A driver that can no longer send events (the handle was dropped) asks
/// before killing: the TUI sends `Shutdown` and then drops the handle, and
/// the driver may have been sending an event in between.
pub fn shutdown_queued(cmds: &mut mpsc::Receiver<SessionCommand>) -> bool {
    std::iter::from_fn(|| cmds.try_recv().ok()).any(|c| c == SessionCommand::Shutdown)
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

    #[test]
    fn file_attachment_is_a_pdf_or_text() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let pdf = write("memo.PDF", b"%PDF-1.4");
        let text = write("Makefile", b"all:\n");
        let binary = write("a.out", &[0x7f, b'E', b'L', b'F', 0xff, 0xfe]);
        let image = write("shot.png", b"png");

        let a = Attachment::from_path(&pdf).unwrap();
        assert_eq!(
            (a.mime(), a.marker().as_str()),
            ("application/pdf", "[file: memo.PDF]")
        );
        assert_eq!(Attachment::from_path(&text).unwrap().mime(), "text/plain");
        assert_eq!(Attachment::from_path(&binary), None);
        assert_eq!(Attachment::from_path(dir.path().join("gone.txt")), None);
        let a = Attachment::from_path(&image).unwrap();
        assert!(a.is_image());
        assert_eq!(a.marker(), "[image: shot.png]");
    }
}
