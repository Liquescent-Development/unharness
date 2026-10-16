//! Turns as the keys reach them: sending a prompt, queueing, steering and
//! sending one at once, compacting, interrupting. What they do is the controller's
//! (`core::controller`, `turn.rs`).

use super::App;

impl App {
    /// Send the prompt box contents as a turn.
    pub fn submit_prompt(&mut self, text: String) {
        self.with(|c, screen| c.submit_prompt(text, screen));
    }

    /// Hold a prompt until the running turn finishes.
    pub fn queue_prompt(&mut self, text: String) {
        self.with(|c, screen| c.queue_prompt(text, screen));
    }

    /// Send the oldest queued prompt, if idle. Returns whether one was sent.
    pub fn send_next_queued(&mut self) -> bool {
        self.with(|c, screen| c.send_next_queued(screen))
    }

    /// Put the newest queued prompt back in the prompt box for editing.
    pub fn unqueue_last(&mut self) {
        if !self.input.is_empty() {
            return;
        }
        if let Some(q) = self.controller.unqueue_last() {
            self.cursor = q.text.chars().count();
            self.input = q.text;
            self.attachments.extend(q.attachments);
        }
    }

    /// Inject a message into the running turn, where the harness can; queue it otherwise.
    pub fn steer(&mut self, text: String) {
        self.with(|c, screen| c.steer(text, screen));
    }

    /// Send the prompt at once in place of the running turn (Ctrl+Enter,
    /// `/now`); with `text` empty, the oldest queued prompt. Idle, what
    /// Enter does.
    pub fn send_now(&mut self, text: String) {
        self.with(|c, screen| c.send_now(text, screen));
    }

    /// Ask the harness to summarise its context now.
    pub fn compact(&mut self, instructions: Option<String>) {
        self.with(|c, screen| c.compact(instructions, screen));
    }

    /// Ask the running turn to stop; asked again once `STOP_GRACE` has
    /// passed without the turn ending, end the CLI's process instead.
    pub fn interrupt(&mut self) {
        self.with(|c, screen| c.interrupt(screen));
    }
}
