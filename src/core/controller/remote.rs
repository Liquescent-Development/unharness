//! The vendor's remote control (`/remote-control`).

use super::{Action, Controller, RemoteSession};
use crate::core::{RemoteControl, SessionCommand};

impl Controller {
    /// `/remote-control [on [name] | off | status]`, `args` being what
    /// follows the command, on a harness that has it.
    pub fn remote_control_command(&mut self, args: &str) {
        let (word, name) = match args.split_once(char::is_whitespace) {
            Some((w, n)) => (w, Some(n.trim().to_string()).filter(|n| !n.is_empty())),
            None => (args, None),
        };
        match word {
            "" | "on" => self.remote_control_on(name),
            "off" if name.is_none() => self.remote_control_off(),
            "status" if name.is_none() => {
                let says = match &self.remote {
                    None => "Remote Control is off".to_string(),
                    Some(RemoteSession {
                        url: Some(url),
                        state,
                        ..
                    }) => format!(
                        "Remote Control is on ({}): {url}",
                        state.as_deref().unwrap_or("starting")
                    ),
                    Some(_) => "Remote Control is starting".to_string(),
                };
                self.conversation.transcript.push_notice(says);
            }
            _ => self
                .conversation
                .transcript
                .push_notice("usage: /remote-control [on [name] | off | status]"),
        }
    }

    fn remote_control_on(&mut self, name: Option<String>) {
        if let Some(remote) = &self.remote {
            let says = match &remote.url {
                Some(url) => format!("Remote Control is already on: {url}"),
                None => "Remote Control is starting".to_string(),
            };
            self.conversation.transcript.push_notice(says);
            return;
        }
        self.remote = Some(RemoteSession {
            name: name.clone(),
            ..Default::default()
        });
        if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::RemoteControl {
                    enabled: true,
                    name,
                }));
            self.conversation.transcript.push_notice(format!(
                "turning on Remote Control for {}",
                self.short_name()
            ));
        } else {
            // Sent once the session is up (`remote_control_resend`).
            self.conversation.transcript.push_notice(format!(
                "Remote Control goes on once {}'s session has started",
                self.short_name()
            ));
        }
    }

    fn remote_control_off(&mut self) {
        if self.remote.take().is_some() && self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::RemoteControl {
                    enabled: false,
                    name: None,
                }));
        }
        self.conversation
            .transcript
            .push_notice("Remote Control is off");
    }

    /// Remote Control ends with the conversation's tie to this session (a
    /// switch, a fork, a resume, `/clear`, quit): taken off before the session is
    /// shut down, so that the remote side is told.
    pub(super) fn end_remote_control(&mut self) {
        if self.remote.take().is_none() {
            return;
        }
        if self.session_alive {
            self.actions
                .push_back(Action::Command(SessionCommand::RemoteControl {
                    enabled: false,
                    name: None,
                }));
        }
        self.conversation
            .transcript
            .push_notice("Remote Control ended");
    }

    /// For a session that just started: the command that puts it on Remote
    /// Control again, if the user had it on.
    pub fn remote_control_resend(&mut self) -> Option<SessionCommand> {
        let remote = self.remote.as_mut()?;
        remote.url = None;
        remote.state = None;
        Some(SessionCommand::RemoteControl {
            enabled: true,
            name: remote.name.clone(),
        })
    }

    /// A prompt typed on Remote Control, which the session runs as a turn
    /// of its own: shown, checkpointed and titled as one typed here.
    pub(super) fn remote_prompt(&mut self, text: String) {
        // What this harness has not been told yet is still owed: it goes
        // with the next prompt typed here, as after a command.
        let from = self.bridge_start(self.active);
        if from < self.conversation.transcript.blocks.len() {
            self.conversation
                .last_active_index
                .insert(self.active, from);
        }
        // The session is the conversation's from here on.
        if self.session_blank {
            self.mark_prompted();
        }
        if self.conversation.first_prompt.is_none() {
            self.conversation.first_prompt = Some(text.clone());
        }
        self.conversation
            .transcript
            .push_notice("from Remote Control:");
        self.conversation.transcript.push_user(text);
        let block = self.conversation.transcript.blocks.len() - 1;
        self.checkpoint_files(block);
    }

    /// What the status bar says of Remote Control.
    pub fn remote_status(&self) -> Option<String> {
        let remote = self.remote.as_ref()?;
        Some(match (&remote.url, remote.state.as_deref()) {
            // Taken up again by the next process.
            _ if !self.session_alive => "remote: paused".to_string(),
            (None, _) => "remote: starting".to_string(),
            (Some(_), None | Some("connected")) => "remote".to_string(),
            (Some(_), Some(state)) => format!("remote: {state}"),
        })
    }

    pub(super) fn on_remote_control(&mut self, event: RemoteControl) {
        match event {
            // Taken off meanwhile, which follows: nothing to open.
            RemoteControl::On { .. } | RemoteControl::State { .. } if self.remote.is_none() => {}
            RemoteControl::On { url } => {
                if let Some(remote) = self.remote.as_mut() {
                    remote.url = Some(url.clone());
                }
                self.conversation.transcript.push_system(format!(
                    "Remote Control is on: open {url} in a browser or the Claude app"
                ));
            }
            RemoteControl::State { state, .. } => {
                if let Some(remote) = self.remote.as_mut() {
                    remote.state = Some(state);
                }
            }
            // Said when it was asked for (`remote_control_off`).
            RemoteControl::Off => {}
            RemoteControl::Failed { reason } => {
                self.remote = None;
                self.conversation
                    .transcript
                    .push_error(format!("Remote Control: {reason}"));
            }
        }
    }
}
