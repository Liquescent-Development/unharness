//! Turns: sending a prompt, queueing and steering, compacting, interrupting.

use std::time::{Duration, Instant};

use super::{Action, Controller, Front, QueuedPrompt, SendingNow};
use crate::core::transcript::Block;
use crate::core::{Attachment, SendNow, SessionCommand, shell};

/// How long a turn has to end after an interrupt before another one ends
/// the CLI's process instead. Every harness stopped well within it in the
/// runs checked; pi waiting in a dialog never did (#89).
pub const STOP_GRACE: Duration = Duration::from_secs(5);

impl Controller {
    pub(super) fn start_generation(&mut self, front: &mut dyn Front) {
        self.is_generating = true;
        self.interrupted = None;
        self.conversation.transcript.begin_turn();
        self.generation_start = Some(Instant::now());
        self.conversation.generation_duration = None;
        // The last turn's numbers stay until this one reports its own.
        self.conversation.turn_reported = false;
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
        let outgoing = self.outgoing(&text);
        let mut shown = text.clone();
        let attachments = self.take_attachments(&mut shown, front);
        self.begin_prompted_turn(&text, shown, front);

        self.ensure_session();
        self.mark_prompted();
        self.actions.push_back(Action::SendTurn {
            text: outgoing,
            attachments,
        });
    }

    /// What goes to the harness for the prompt `text`: in front of it, what
    /// the live session has not seen of the conversation, and the `!`
    /// commands no agent was told of yet, which count as told from here.
    pub(super) fn outgoing(&mut self, text: &str) -> String {
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

        let context: Vec<String> = bridge
            .map(|ctx| {
                format!(
                    "[Context: earlier conversation in this unharness session, possibly with other agents]\n{ctx}"
                )
            })
            .into_iter()
            .chain(shell::context(&shells))
            .collect();
        if context.is_empty() {
            text.to_string()
        } else {
            format!(
                "{}\n\n[Current task for {}]:\n{text}",
                context.join("\n\n"),
                self.short_name()
            )
        }
    }

    /// The turn of the prompt `text` begins, shown as `shown`: it is the
    /// conversation's first prompt if none was, the transcript shows it,
    /// its anchor is owed, and the files are checkpointed before it runs.
    pub(super) fn begin_prompted_turn(&mut self, text: &str, shown: String, front: &mut dyn Front) {
        self.prompt_shown(text, shown);
        self.start_generation(front);
    }

    /// The prompt `text`, shown as `shown`, is in the transcript as the
    /// user's, with its anchor owed and the files checkpointed.
    fn prompt_shown(&mut self, text: &str, shown: String) {
        if self.conversation.first_prompt.is_none() {
            self.conversation.first_prompt = Some(text.to_string());
        }
        self.conversation.transcript.push_user(shown);
        let block = self.conversation.transcript.blocks.len() - 1;
        if self.caps().rewind.conversation {
            self.conversation.anchor_pending.push_back(block);
        }
        self.checkpoint_files(block);
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

    /// Take the newest queued prompt back, to be edited.
    pub fn unqueue_last(&mut self) -> Option<QueuedPrompt> {
        self.queued.pop_back()
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

    /// Send `text`, with the pending attachments, at once in place of the
    /// running turn (`/now`, Ctrl+Enter): the turn is interrupted and the
    /// prompt is its successor, where every interrupt works, or the CLI
    /// takes it with the interrupt (`SendNow::Native`). With `text` empty,
    /// the oldest queued prompt; the others keep their places, and go as
    /// usual after the new turn ends. Idle, it is what Enter does.
    ///
    /// What cannot go now (a handoff summary's turn, a compaction, a
    /// choice the session was sent, no policy, a harness that cannot be
    /// interrupted) is queued for when the turn ends, and said to be.
    /// Asked again while one waits, the turn is pressed to end as a second
    /// interrupt presses it ([`Controller::interrupt`]), and what was typed
    /// queues behind the prompt.
    pub fn send_now(&mut self, text: String, front: &mut dyn Front) {
        let text = text.trim().to_string();
        if !self.is_generating {
            if !text.is_empty() {
                self.queue_prompt(text, front);
            } else if !self.send_next_queued(front) && !self.queued.is_empty() {
                self.require_policy(front);
            }
            return;
        }
        if self.sending_now.is_some() {
            if !text.is_empty() {
                self.queued.push_front(QueuedPrompt {
                    text,
                    attachments: std::mem::take(front.attachments()),
                    harness: self.active,
                });
                self.conversation
                    .transcript
                    .push_notice("queued behind the prompt being sent now");
            }
            self.press_interrupt(front);
            return;
        }
        let (prompt, from_queue) = if text.is_empty() {
            match self.queued.pop_front() {
                Some(q) => (q, true),
                None => {
                    self.conversation
                        .transcript
                        .push_notice("nothing to send now: type a prompt, or queue one first");
                    return;
                }
            }
        } else {
            let q = QueuedPrompt {
                text,
                attachments: std::mem::take(front.attachments()),
                harness: self.active,
            };
            (q, false)
        };
        if let Some(why) = self.cannot_send_now() {
            self.conversation.transcript.push_notice(format!(
                "{why}: '{}' is queued for when the turn ends",
                first_line(&prompt.text)
            ));
            if from_queue {
                self.queued.push_front(prompt);
            } else {
                self.queued.push_back(prompt);
            }
            return;
        }
        // A command typed for another harness waits for the turn's end,
        // where it goes back to the prompt box (`send_queued`).
        let typed_elsewhere = prompt.text.starts_with('/') && prompt.harness != self.active;
        let with_cli = (self.caps().send_now == SendNow::Native
            && !typed_elsewhere
            && !self.start_held)
            .then(|| {
                let outgoing = self.outgoing(&prompt.text);
                let mut shown = prompt.text.clone();
                let attachments = self.accepted(&mut shown, prompt.attachments.clone());
                self.mark_prompted();
                self.actions
                    .push_back(Action::Command(SessionCommand::SendNow {
                        text: outgoing,
                        attachments,
                    }));
                shown
            });
        match &with_cli {
            Some(_) => self.conversation.transcript.push_system("Sending now…"),
            None if self.interrupted.is_none() => {
                self.actions
                    .push_back(Action::Command(SessionCommand::Interrupt));
                self.conversation.transcript.push_system("Interrupting…");
            }
            // Asked to stop already: the prompt follows when it has.
            None => {}
        }
        if self.interrupted.is_none() {
            self.interrupted = Some((Instant::now(), false));
        }
        self.sending_now = Some(SendingNow {
            prompt,
            from_queue,
            with_cli,
        });
    }

    /// Why a prompt cannot be sent at once now, if it cannot.
    fn cannot_send_now(&self) -> Option<String> {
        let name = self.short_name();
        if self.handoff.is_some() {
            Some("a handoff summary is being written".into())
        } else if self.compacting {
            Some(format!("{name} is compacting its context"))
        } else if self.shell.is_some() {
            Some("a ! command is running".into())
        } else if self.choice_pending() {
            Some(format!(
                "{name} has not said yet how it took the last choice"
            ))
        } else if self.effective_policy(self.active).is_none() {
            Some("no permission policy is chosen".into())
        } else if !self.caps().interrupt || !self.session_alive {
            Some(format!("{name} cannot be interrupted now"))
        } else {
            None
        }
    }

    /// The turn a prompt sent at once was to stop has ended (`p`), as
    /// `interrupted` says (otherwise it failed): the prompt's own turn
    /// begins. The CLI that has it started it by itself; one whose
    /// interrupt ends its process is started again once it has exited;
    /// otherwise it is sent now, ahead of the queue. After a turn that
    /// failed, what was not sent yet goes back to the user.
    pub(super) fn sent_now_turn(
        &mut self,
        p: SendingNow,
        interrupted: bool,
        front: &mut dyn Front,
    ) {
        match p.with_cli {
            Some(shown) => self.begin_prompted_turn(&p.prompt.text, shown, front),
            None if !interrupted => {
                self.give_back(p, front);
                self.say_held();
            }
            None if self.caps().send_now == SendNow::AfterExit && self.session_alive => {
                self.sending_now = Some(SendingNow {
                    with_cli: None,
                    ..p
                });
            }
            None => self.send_ahead(p.prompt, front),
        }
    }

    /// Send `prompt`, the one sent at once, and nothing else: one that
    /// cannot go now waits at the queue's head, and a command typed for
    /// another harness goes back to the prompt box; the queue is held
    /// either way.
    pub(super) fn send_ahead(&mut self, prompt: QueuedPrompt, front: &mut dyn Front) {
        if prompt.text.starts_with('/') && prompt.harness != self.active {
            let first = first_line(&prompt.text).to_string();
            self.conversation.transcript.push_notice(format!(
                "'{first}' was typed for {}; it is back in the prompt box",
                self.registry()
                    .get(prompt.harness)
                    .map_or(prompt.harness.as_str(), |h| h.descriptor().short_name)
            ));
            front.put_back(prompt.text, prompt.attachments);
            self.say_held();
            return;
        }
        self.queued.push_front(prompt);
        if !self.send_queued(0, front) {
            self.say_held();
        }
    }

    /// Say that the queued prompts wait for the user.
    pub(super) fn say_held(&mut self) {
        if !self.queued.is_empty() {
            self.conversation.transcript.push_notice(format!(
                "{} queued prompt(s) held; press Enter to send the next",
                self.queued.len()
            ));
        }
    }

    /// The CLI took the prompt sent at once into the running turn (Claude
    /// moved what the turn waited on to the background): it is shown
    /// there, as a steer is, and the turn goes on, to be interrupted anew.
    pub(super) fn sent_now_taken_in(&mut self, p: SendingNow) {
        if let Some(shown) = p.with_cli {
            self.prompt_shown(&p.prompt.text, shown);
        }
        self.interrupted = None;
    }

    /// The prompt sent at once was not, and goes back where it came from.
    pub(super) fn give_back(&mut self, p: SendingNow, front: &mut dyn Front) {
        let first = first_line(&p.prompt.text).to_string();
        if p.from_queue {
            self.queued.push_front(p.prompt);
            self.conversation.transcript.push_notice(format!(
                "'{first}' was not sent; it is back at the head of the queue"
            ));
        } else {
            front.put_back(p.prompt.text, p.prompt.attachments);
            self.conversation.transcript.push_notice(format!(
                "'{first}' was not sent; it is back in the prompt box"
            ));
        }
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
    /// Asked while a prompt waits to be sent at once, that prompt is not
    /// sent: it goes back where it came from, and the turn still ends.
    pub fn interrupt(&mut self, front: &mut dyn Front) {
        if !self.is_generating {
            return;
        }
        match self.sending_now.take() {
            // A CLI that cannot be made to drop it runs it all the same:
            // it stays, and the turn is pressed as without it.
            Some(p) if p.with_cli.is_some() && !self.caps().take_back_sent_now => {
                self.conversation.transcript.push_notice(format!(
                    "{} already has '{}' and runs it once the turn has stopped",
                    self.short_name(),
                    first_line(&p.prompt.text)
                ));
                self.sending_now = Some(p);
            }
            Some(p) => {
                // The CLI that has it is asked to drop it with the turn.
                if p.with_cli.is_some() {
                    self.actions
                        .push_back(Action::Command(SessionCommand::Interrupt));
                }
                self.give_back(p, front);
                return;
            }
            None => {}
        }
        self.press_interrupt(front);
    }

    /// Ask the running turn to stop, or press it: see
    /// [`Controller::interrupt`].
    fn press_interrupt(&mut self, front: &mut dyn Front) {
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
        // Sent as the next session's first prompt, below.
        let sending = self.sending_now.take();
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
        match sending {
            Some(p) => self.send_ahead(p.prompt, front),
            None => self.say_held(),
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
        let attachments = std::mem::take(front.attachments());
        self.accepted(shown, attachments)
    }

    /// Of `attachments`, the ones the active harness accepts, as
    /// [`Controller::take_attachments`] takes them.
    fn accepted(&mut self, shown: &mut String, attachments: Vec<Attachment>) -> Vec<Attachment> {
        let caps = self.caps();
        let (sent, dropped): (Vec<_>, Vec<_>) =
            attachments.into_iter().partition(|a| caps.accepts(a));
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

/// The first line of `text`, for saying which prompt is meant.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}
