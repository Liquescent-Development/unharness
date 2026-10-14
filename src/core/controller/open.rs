//! A run's start: the controller it opens with, on the conversation
//! `--resume` named or a new one, and what it says once its front end is
//! up.

use std::path::PathBuf;
use std::sync::Arc;

use super::{Controller, Front, Keeping, Launch};
use crate::config::Config;
use crate::core::checkpoints::Checkpoints;
use crate::core::conversations::{Conversation, ConversationStore, default_context_windows_path};
use crate::core::registry::Registry;

/// What a run opens its controller with.
pub struct Opening {
    pub registry: Arc<Registry>,
    pub config: Config,
    pub launch: Launch,
    /// Conversation id or prefix; empty string = most recent.
    pub resume: Option<String>,
    /// The harness came from the CLI, so it overrides a resumed conversation's.
    pub harness_explicit: bool,
    /// Where file-checkpoint shadow repositories go; `None` = the user's
    /// state directory.
    pub checkpoint_store: Option<PathBuf>,
    /// Where the context windows harnesses reported are kept; `None` = the
    /// user's state directory.
    pub context_windows: Option<PathBuf>,
}

/// What a run says at its start, once its front end is up
/// (`Controller::greet`).
pub struct Greeting {
    /// A conversation was resumed.
    resumed: bool,
    /// The conversation `--resume` named could not be.
    resume_error: Option<String>,
}

impl Controller {
    /// The controller a run opens with: on the conversation `--resume`
    /// names, on its harness unless the command line named one, or on a
    /// new one. What it says of that waits for its front end (`greet`).
    pub fn open(o: Opening) -> (Self, Greeting) {
        let Opening {
            registry,
            config,
            launch,
            resume,
            harness_explicit,
            checkpoint_store,
            context_windows,
        } = o;
        let store = ConversationStore::open(launch.workspace_root.as_deref(), &launch.cwd);
        let mut resume_error: Option<String> = None;
        let mut loaded: Option<Conversation> = None;
        if let Some(r) = resume.as_deref() {
            let target = if r.is_empty() {
                store.last().map(|c| c.id)
            } else {
                Some(r.to_string())
            };
            match target {
                None => {
                    resume_error = Some("no saved conversation to resume in this workspace".into())
                }
                Some(id) => match store.load(&id) {
                    Ok(c) => loaded = Some(c),
                    Err(e) => resume_error = Some(format!("could not resume: {e}")),
                },
            }
        }
        let checkpoints = if config.file_checkpoints.unwrap_or(true) {
            let store = checkpoint_store.unwrap_or_else(Checkpoints::default_store);
            launch
                .workspace_root
                .as_deref()
                .and_then(|root| Checkpoints::open_in(root, &store))
        } else {
            None
        };
        let harness = launch.harness;
        let mut controller = Controller::new(
            registry,
            config,
            launch,
            Keeping {
                store,
                checkpoints,
                context_windows: context_windows.unwrap_or_else(default_context_windows_path),
            },
        );
        let resumed = loaded.is_some();
        if let Some(conversation) = loaded {
            let active = if !harness_explicit
                && controller
                    .registry()
                    .get(conversation.active_harness)
                    .is_some()
            {
                conversation.active_harness
            } else {
                harness
            };
            controller.install(conversation, active);
        }
        (
            controller,
            Greeting {
                resumed,
                resume_error,
            },
        )
    }

    /// What the run says at its start: the choices a resumed conversation
    /// was run with taken back, the welcome, a policy that is not what was
    /// asked for (and the picker where none is in effect), a sandbox that is
    /// off, MCP servers that are not right, a conversation that could not
    /// be resumed, and the one that was.
    pub fn greet(&mut self, g: Greeting, front: &mut dyn Front) {
        if g.resumed {
            self.take_conversation_choices();
        }
        let welcome = format!(
            "Welcome to unharness. Harness: {}  Policy: {}  Sandbox: {}. Ctrl+H harness, Ctrl+M model, Ctrl+E effort, Ctrl+P policy, /sandbox, /help.",
            self.display_name(),
            self.policy_label(),
            self.sandbox_level().0
        );
        self.conversation.transcript.push_system(welcome);
        if let Some(w) = self.policy_warning() {
            self.conversation.transcript.push_notice(w);
        }
        self.require_policy(front);
        if let Some(w) = self.sandbox_level().1 {
            self.conversation.transcript.push_notice(w);
        }
        for problem in self.config.mcp_servers().1 {
            self.conversation.transcript.push_notice(problem);
        }
        if let Some(e) = g.resume_error {
            self.conversation.transcript.push_error(e);
        }
        if g.resumed {
            self.announce_resume(self.active, true);
        }
    }
}
