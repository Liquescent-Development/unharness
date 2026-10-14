//! Subagents at work: what they hold back, stopping one, and dropping
//! them with their session.

use super::{Action, Controller, Front, subagents_refusal};
use crate::core::SessionCommand;

impl Controller {
    /// The subagents at work whose end the harness reports. Only these
    /// keep the status rule and herdr at work, hold back what would end
    /// the session (`refused_for_subagents`) and a handoff summary. One
    /// whose end may never be told (Mistral Vibe's, until its agent waits
    /// for it; `SubagentStarted::unreported_end`) would hold them until
    /// the session ends: it stays listed at work with its note, and goes
    /// with the session.
    pub fn awaited_subagents(&self) -> usize {
        self.subagents.iter().filter(|a| !a.unreported_end).count()
    }

    /// Whether subagents at work keep the session from being ended for
    /// `doing` (they would end with it); if so the user is told, and
    /// pointed at stopping them only where the harness can.
    pub(super) fn refused_for_subagents(&mut self, doing: &str) -> bool {
        let n = self.awaited_subagents();
        if n == 0 {
            return false;
        }
        self.conversation.transcript.push_error(subagents_refusal(
            n,
            self.caps().subagents.stop,
            doing,
        ));
        true
    }

    /// Stop the subagent that the tool call `id` spawned.
    pub fn stop_subagent(&mut self, id: String, front: &mut dyn Front) {
        if !self.subagents.iter().any(|a| a.id == id) {
            front.flash("This subagent is not running".into());
        } else if !self.caps().subagents.stop || !self.session_alive {
            front.flash(format!(
                "{} cannot stop a subagent from here",
                self.short_name()
            ));
        } else {
            front.flash("Stopping…".into());
            self.actions
                .push_back(Action::Command(SessionCommand::StopSubagent { id }));
        }
    }

    /// The session is going away, and whatever its subagents were doing.
    pub(super) fn drop_subagents(&mut self) {
        self.subagents.clear();
        self.conversation.transcript.end_running_agents();
    }
}
