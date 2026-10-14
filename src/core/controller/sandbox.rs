//! The sandbox level the active harness's processes run at.

use super::{Controller, Front};
use crate::core::sandbox::{Sandbox, SandboxLevel};

impl Controller {
    /// The sandbox level the active harness runs at, and why it is off if
    /// that was not asked for.
    pub fn sandbox_level(&self) -> (SandboxLevel, Option<String>) {
        self.sandbox.level(self.harness().default_sandbox())
    }

    /// What confines the next process of the active harness.
    pub fn session_sandbox(&self) -> anyhow::Result<Sandbox> {
        crate::runner::session_sandbox(
            self.harness(),
            &self.sandbox,
            &self.config,
            self.workspace_root.as_deref().unwrap_or(&self.cwd),
        )
    }

    /// Choose the sandbox level for the run. Returns whether it could be
    /// set. The sandbox is applied when a process is spawned, so a live
    /// session is shut down and comes back under the new level, resumed
    /// where the harness can.
    pub fn set_sandbox(&mut self, level: SandboxLevel, front: &mut dyn Front) -> bool {
        if self.is_generating {
            self.conversation
                .transcript
                .push_error("finish or interrupt the current turn before changing the sandbox");
            return false;
        }
        if let Err(why) = &self.sandbox.backend
            && level != SandboxLevel::Off
        {
            self.conversation
                .transcript
                .push_error(format!("sandbox '{level}' is unavailable: {why}"));
            return false;
        }
        self.sandbox.explicit = Some(level);
        self.idle_start_paused = false;
        let restart = self.session_alive && self.session_sandbox_level != Some(level);
        self.conversation.transcript.push_system(if restart {
            format!(
                "Sandbox: {level} ({} restarts under it)",
                self.display_name()
            )
        } else {
            format!("Sandbox: {level}")
        });
        if restart {
            self.shutdown_session(front);
        }
        true
    }

    /// The sandbox levels there is a backend for.
    pub fn available_sandbox_levels(&self) -> Vec<SandboxLevel> {
        SandboxLevel::ALL
            .into_iter()
            .filter(|l| self.sandbox.backend.is_ok() || *l == SandboxLevel::Off)
            .collect()
    }
}
