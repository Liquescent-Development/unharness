//! The active harness, what it can do, and what the header and status rule show.

use std::path::PathBuf;
use std::sync::Arc;

use super::{Action, App};
use crate::core::registry::Registry;
use crate::core::{Capabilities, HarnessCommand};
use crate::harness::Harness;
use crate::tui::modal::Modal;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl App {
    /// The harnesses of this run: the controller's, the one its
    /// capabilities are read from.
    pub fn registry(&self) -> &Arc<Registry> {
        self.controller.registry()
    }

    pub fn harness(&self) -> &dyn Harness {
        self.controller.harness()
    }

    pub fn short_name(&self) -> &'static str {
        self.controller.short_name()
    }

    pub fn display_name(&self) -> &'static str {
        self.controller.display_name()
    }

    /// The commands of its own the active harness's session listed.
    pub fn harness_commands(&self) -> &[HarnessCommand] {
        self.controller.harness_commands(self.controller.active)
    }

    /// Declared capabilities of the active harness plus what its session
    /// reported (`Controller::caps`).
    pub fn caps(&self) -> Capabilities {
        self.controller.caps()
    }

    pub fn harness_binary(&self) -> Option<PathBuf> {
        self.controller.harness_binary()
    }

    /// What the status area warns about: a missing sandbox, a degraded policy.
    pub fn status_warning(&self) -> Option<String> {
        let sandbox = self.sandbox_level().1.map(|s| format!("{s} (/sandbox)"));
        let provider = self
            .controller
            .runs_on(self.controller.active)
            .map(|runs_on| {
                format!(
                    "runs on {runs_on}, not the chosen {} (/provider)",
                    self.current_provider()
                        .map(|p| p.as_str())
                        .unwrap_or("provider")
                )
            });
        let parts: Vec<String> = [sandbox, self.policy_warning(), provider]
            .into_iter()
            .flatten()
            .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    pub fn take_actions(&mut self) -> Vec<Action> {
        self.controller.take_actions()
    }

    pub fn is_thinking(&self) -> bool {
        self.controller.is_generating && self.controller.conversation.transcript.is_thinking()
    }

    pub fn elapsed_secs(&self) -> f32 {
        if self.controller.is_generating {
            self.controller
                .generation_start
                .map(|s| s.elapsed().as_secs_f32())
                .unwrap_or(0.0)
        } else {
            self.controller
                .conversation
                .generation_duration
                .map(|d| d.as_secs_f32())
                .unwrap_or(0.0)
        }
    }

    /// What the agent is doing right now, for the header.
    pub fn status_label(&self) -> String {
        if self.awaiting_answer() {
            return "Waiting for you".to_string();
        }
        if self.controller.compacting() {
            return "Compacting".to_string();
        }
        if self.controller.writing_handoff() {
            return "Writing a handoff summary".to_string();
        }
        if let Some(super::transcript::Block::Tool {
            name, done: false, ..
        }) = self
            .controller
            .conversation
            .transcript
            .blocks
            .iter()
            .rev()
            .find(|b| matches!(b, super::transcript::Block::Tool { .. }))
        {
            return format!("Running {name}");
        }
        if self.controller.conversation.transcript.is_thinking() {
            return "Thinking".to_string();
        }
        if matches!(
            self.controller.conversation.transcript.blocks.last(),
            Some(super::transcript::Block::Assistant { duration: None, .. })
        ) {
            return "Streaming".to_string();
        }
        "Working".to_string()
    }

    /// A turn is running, subagents whose end will be told are still at
    /// work after one (`awaited_subagents`), or a `!` command runs.
    pub fn is_busy(&self) -> bool {
        self.controller.is_generating
            || self.controller.awaited_subagents() > 0
            || self.controller.shell.is_some()
    }

    /// What herdr is told about this session: blocked while a harness
    /// request waits for an answer (open or queued behind another modal) or
    /// the policy picker holds the session, working while busy, otherwise
    /// idle.
    pub fn herdr_report(&self) -> super::herdr::Report {
        let waiting = match &self.modal {
            Some(m) if m.is_prompt() => m.waiting_on(),
            Some(Modal::Policy(_)) if self.effective_policy().is_none() => Some(format!(
                "choose a permission policy for {}",
                self.display_name()
            )),
            _ => None,
        }
        .or_else(|| {
            self.controller
                .tracking
                .pending_prompts
                .front()
                .map(|req| super::herdr::request_message(&req.kind))
        });
        super::herdr::Report::of(self.is_busy(), waiting)
    }

    pub fn tick_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
    }

    pub fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame]
    }
}
