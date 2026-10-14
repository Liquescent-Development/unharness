//! Turns: sending a prompt, queueing and steering, compacting, interrupting.

use std::time::{Duration, Instant};

use super::{Action, Controller, Front, QueuedPrompt};
use crate::core::transcript::Block;
use crate::core::{Attachment, SessionCommand, Usage, shell};

/// How long a turn has to end after an interrupt before another one ends
/// the CLI's process instead. Every harness stopped well within it in the
/// runs checked; pi waiting in a dialog never did (#89).
pub const STOP_GRACE: Duration = Duration::from_secs(5);

impl Controller {
    pub(crate) fn start_generation(&mut self, front: &mut dyn Front) {
        self.is_generating = true;
        self.interrupted = None;
        self.conversation.transcript.begin_turn();
        self.generation_start = Some(Instant::now());
        self.conversation.generation_duration = None;
        self.conversation.turn_usage = Usage::default();
        front.follow();
    }

    pub(super) fn finish_generation(&mut self) {
        if !self.is_generating {
            return;
        }
        self.is_generating = false;
        self.interrupted = None;
        self.compacting = false;
        let dur = self
            .generation_start
            .take()
            .map(|s| s.elapsed())
            .unwrap_or_default();
        self.conversation.generation_duration = Some(dur);
        self.conversation.transcript.finish_turn(dur);
    }

    /// Send the prompt box contents as a turn.
    pub fn submit_prompt(&mut self, text: String, front: &mut dyn Front) {
        let text = text.trim().to_string();
        if text.is_empty() || self.is_generating {
            return;
        }
        front.prompt_submitted();
        if self.shell.is_some() || self.choice_pending() || !self.require_policy(front) {
            // Kept until the `!` command has ended, the session has taken
            // the choice it was sent, or a policy is chosen.
            self.queued.push_back(QueuedPrompt {
                text,
                attachments: std::mem::take(front.attachments()),
                harness: self.active,
            });
            return;
        }
        if self.conversation.first_prompt.is_none() {
            self.conversation.first_prompt = Some(text.clone());
        }

        // A command the harness runs has to start the message, so it goes
        // alone: what would have gone in front of it waits for the next
        // prompt.
        let alone = text.starts_with('/') && self.caps().slash_commands;
        // Bridge context from other harnesses: everything since this harness
        // was last active, or everything on its first visit. Nothing when the
        // live session already saw the whole transcript.
        let from = self.bridge_start(self.active);
        let bridge = if from >= self.conversation.transcript.blocks.len() {
            None
        } else {
            self.conversation
                .transcript
                .bridge_text(from, self.bridge_budget(self.active))
        };
        if alone && bridge.is_some() {
            self.conversation
                .last_active_index
                .insert(self.active, from);
        } else {
            self.conversation.last_active_index.remove(&self.active);
        }
        let unsent_shell = self
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Shell { sent: false, .. }));
        if alone && (bridge.is_some() || unsent_shell) {
            self.conversation.transcript.push_notice(format!(
                "{} is told what it has not seen yet with your next prompt: a command has to start its message",
                self.short_name()
            ));
        }
        let bridge = bridge.filter(|_| !alone);
        // `!` commands no agent has been told about yet (the bridge has
        // only those one has).
        let mut shells = Vec::new();
        for b in self
            .conversation
            .transcript
            .blocks
            .iter_mut()
            .filter(|_| !alone)
        {
            if let Block::Shell {
                command,
                output,
                dropped,
                status,
                sent: sent @ false,
                ..
            } = b
            {
                shells.push(shell::context_entry(
                    command,
                    output,
                    *dropped,
                    status,
                    shell::CONTEXT_MAX_CHARS,
                ));
                *sent = true;
            }
        }

        let mut shown = text.clone();
        let attachments = self.take_attachments(&mut shown, front);
        self.conversation.transcript.push_user(shown);
        let block = self.conversation.transcript.blocks.len() - 1;
        if self.caps().rewind.conversation {
            self.conversation.anchor_pending.push_back(block);
        }
        self.checkpoint_files(block);
        self.start_generation(front);

        let context: Vec<String> = bridge
            .map(|ctx| {
                format!(
                    "[Context: earlier conversation in this unharness session, possibly with other agents]\n{ctx}"
                )
            })
            .into_iter()
            .chain(shell::context(&shells))
            .collect();
        let outgoing = if context.is_empty() {
            text
        } else {
            format!(
                "{}\n\n[Current task for {}]:\n{text}",
                context.join("\n\n"),
                self.short_name()
            )
        };

        self.ensure_session();
        self.mark_prompted();
        self.actions.push_back(Action::SendTurn {
            text: outgoing,
            attachments,
        });
    }

    /// Hold a prompt until the running turn finishes.
    pub fn queue_prompt(&mut self, text: String, front: &mut dyn Front) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.is_generating && self.shell.is_none() {
            self.submit_prompt(text, front);
            return;
        }
        self.queued.push_back(QueuedPrompt {
            text,
            attachments: std::mem::take(front.attachments()),
            harness: self.active,
        });
    }

    /// Send the oldest queued prompt, if idle. Returns whether one was sent.
    pub fn send_next_queued(&mut self, front: &mut dyn Front) -> bool {
        self.send_queued(0, front)
    }

    /// Send the queued prompt at `at`, if idle; the others keep their
    /// places. Returns whether one was sent.
    pub(super) fn send_queued(&mut self, at: usize, front: &mut dyn Front) -> bool {
        if self.is_generating
            || self.shell.is_some()
            || self.choice_pending()
            || self.effective_policy(self.active).is_none()
        {
            return false;
        }
        let Some(q) = self.queued.remove(at) else {
            return false;
        };
        // Whether it runs as a command depends on the harness: one typed
        // for another goes back to the prompt box instead.
        if q.text.starts_with('/') && q.harness != self.active {
            self.conversation.transcript.push_notice(format!(
                "'{}' was typed for {}; it is back in the prompt box",
                q.text.lines().next().unwrap_or_default(),
                self.registry()
                    .get(q.harness)
                    .map_or(q.harness.as_str(), |h| h.descriptor().short_name)
            ));
            // In front of what is being typed, if anything.
            front.put_back(q.text, q.attachments);
            return self.send_queued(at, front);
        }
        let pending = std::mem::replace(front.attachments(), q.attachments);
        self.submit_prompt(q.text, front);
        *front.attachments() = pending;
        true
    }

    /// Inject a message into the running turn, where the harness can; queue it otherwise.
    /// A handoff summary's turn is not steered: the answer to the message
    /// would be taken for the summary. The message goes to the next harness.
    pub fn steer(&mut self, text: String, front: &mut dyn Front) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.is_generating || self.compacting || self.handoff.is_some() {
            self.queue_prompt(text, front);
            return;
        }
        if !self.caps().steer || !self.session_alive {
            self.conversation.transcript.push_notice(format!(
                "{} cannot be steered mid-turn; queued for when it finishes",
                self.short_name()
            ));
            self.queue_prompt(text, front);
            return;
        }
        let mut shown = text.clone();
        let attachments = self.take_attachments(&mut shown, front);
        self.conversation.transcript.push_user(shown);
        self.actions
            .push_back(Action::Command(SessionCommand::Steer { text, attachments }));
    }

    /// Ask the harness to summarise its context now.
    pub fn compact(&mut self, instructions: Option<String>, front: &mut dyn Front) {
        if self.is_generating {
            self.conversation
                .transcript
                .push_error("finish or interrupt the current turn before compacting");
            return;
        }
        if !self.caps().compaction {
            self.conversation
                .transcript
                .push_error(format!("{} cannot compact on request", self.short_name()));
            return;
        }
        if !self.session_seen() {
            self.conversation
                .transcript
                .push_notice("no live session to compact; send a prompt first");
            return;
        }
        self.start_generation(front);
        self.compacting = true;
        self.actions
            .push_back(Action::Command(SessionCommand::Compact { instructions }));
    }

    /// Ask the running turn to stop. Asked again once `STOP_GRACE` has
    /// passed without the turn ending, end the CLI's process instead.
    pub fn interrupt(&mut self, front: &mut dyn Front) {
        if !self.is_generating {
            return;
        }
        match self.interrupted {
            None => {
                self.actions
                    .push_back(Action::Command(SessionCommand::Interrupt));
                self.conversation.transcript.push_system("Interrupting…");
                self.interrupted = Some((Instant::now(), false));
            }
            // Said before the process is ended, however long the wait.
            Some((at, false)) => {
                let notice = if at.elapsed() < STOP_GRACE {
                    format!(
                        "waiting for {} to stop; asked again after {}s, its process is ended",
                        self.short_name(),
                        STOP_GRACE.as_secs()
                    )
                } else {
                    format!(
                        "{} has not stopped; asked again, its process is ended",
                        self.short_name()
                    )
                };
                self.conversation.transcript.push_notice(notice);
                self.interrupted = Some((at, true));
            }
            Some((at, true)) if at.elapsed() < STOP_GRACE => {}
            Some(_) => self.end_stuck_turn(front),
        }
    }

    /// The CLI did not stop when asked: end its process and the turn. The
    /// next prompt starts it again on the same session, unless the turn
    /// was a handoff summary: the switch it was for goes ahead, or the new
    /// session it was for starts, told the conversation (#226).
    fn end_stuck_turn(&mut self, front: &mut dyn Front) {
        let name = self.short_name();
        self.conversation.transcript.push_notice(match self.handoff {
            None => format!(
                "{name} did not stop, so its process was ended; the next prompt resumes the session"
            ),
            Some(h) if h.renew => format!(
                "{name} did not stop, so its process was ended; a new session starts with the change, and your next prompt tells it the conversation so far"
            ),
            Some(_) => format!("{name} did not stop, so its process was ended"),
        });
        // What the turn changed is told before the next process is watched.
        self.check_guard();
        self.shutdown_session(front);
        // Not started again before the next prompt.
        self.idle_start_paused = true;
        self.process_gone(front);
        self.conversation.transcript.end_running_tools();
        self.finish_generation();
        self.conversation
            .transcript
            .push_system("Turn interrupted.");
        self.finish_handoff(false, front);
        self.persist();
        if !self.queued.is_empty() {
            self.conversation.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    /// Why the active harness cannot take this attachment.
    pub fn refusal(&self, a: &Attachment) -> String {
        match a {
            Attachment::Image { .. } => format!(
                "{} does not accept images with the current model",
                self.short_name()
            ),
            Attachment::File { .. } => format!(
                "{} does not accept file attachments, only images",
                self.short_name()
            ),
        }
    }

    /// The pending attachments the active harness accepts, with their
    /// transcript markers appended to `shown`. The others are dropped, and
    /// said to be: one queued for another harness can end up here.
    pub(super) fn take_attachments(
        &mut self,
        shown: &mut String,
        front: &mut dyn Front,
    ) -> Vec<Attachment> {
        let caps = self.caps();
        let (sent, dropped): (Vec<_>, Vec<_>) = std::mem::take(front.attachments())
            .into_iter()
            .partition(|a| caps.accepts(a));
        for a in &dropped {
            let why = self.refusal(a);
            self.conversation
                .transcript
                .push_error(format!("{} not sent: {why}", a.label()));
        }
        for a in &sent {
            shown.push_str(&format!("\n{}", a.marker()));
        }
        sent
    }
}
