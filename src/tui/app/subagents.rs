//! Subagents: the list under the prompt and their own transcripts. Those
//! at work are the controller's (`core::controller`, `subagents.rs`).

use super::{App, SubagentOption, SubagentRow};
use crate::core::transcript::Transcript;
use crate::tui::modal::{ListPicker, Modal};

impl App {
    /// Any subagent is listed at work, whether or not its end will be
    /// told: its row's spinner and time go on.
    pub fn has_running_subagents(&self) -> bool {
        !self.controller.subagents.is_empty()
    }

    /// "2 subagents running", when any are whose end will be told.
    pub fn subagents_label(&self) -> Option<String> {
        match self.controller.awaited_subagents() {
            0 => None,
            1 => Some("1 subagent running".to_string()),
            n => Some(format!("{n} subagents running")),
        }
    }

    /// How long the longest-running subagent whose end will be told has
    /// been at work.
    pub fn subagents_elapsed_secs(&self) -> f32 {
        self.controller
            .subagents
            .iter()
            .filter(|a| !a.unreported_end)
            .map(|a| a.started.elapsed().as_secs_f32())
            .fold(0.0, f32::max)
    }

    /// The list under the prompt: the subagents at work, then those that
    /// have ended, latest first. An ended one stays until the user takes
    /// it off, so that what it did can still be read.
    pub fn subagent_rows(&self) -> Vec<SubagentRow> {
        let mut rows: Vec<SubagentRow> = self
            .controller
            .conversation
            .transcript
            .listed_agents()
            .into_iter()
            .map(|(id, run, depth)| {
                let live = self.controller.subagents.iter().find(|a| a.id == id);
                SubagentRow {
                    id: id.to_string(),
                    description: run.description.clone(),
                    kind: run.kind.clone(),
                    status: run.status,
                    depth,
                    activity: live.and_then(|a| a.activity.clone()),
                    tools: live.map_or(0, |a| a.tools),
                    secs: run
                        .duration
                        .unwrap_or_else(|| run.started.elapsed())
                        .as_secs_f32(),
                }
            })
            .collect();
        let ended = rows.iter().filter(|r| r.status.is_some()).count();
        // (Stable: the running keep their order; the ended are then reversed.)
        rows.sort_by_key(|r| r.status.is_some());
        let n = rows.len();
        rows[n - ended..].reverse();
        rows
    }

    /// Where the keyboard is in that list, if it is there (and the
    /// subagent it was on is still listed).
    pub fn subagent_focus_index(&self) -> Option<usize> {
        let id = self.subagent_focus.as_ref()?;
        self.subagent_rows().iter().position(|r| &r.id == id)
    }

    /// Down: from the prompt into the list, then along it. False when
    /// there is no list to go into.
    pub fn subagent_list_down(&mut self) -> bool {
        let rows = self.subagent_rows();
        let next = match self.subagent_focus_index() {
            Some(i) => (i + 1).min(rows.len().saturating_sub(1)),
            None => 0,
        };
        self.subagent_focus = rows.get(next).map(|r| r.id.clone());
        self.subagent_focus.is_some()
    }

    /// Up: along the list, and from its first row back to the prompt.
    pub fn subagent_list_up(&mut self) {
        let rows = self.subagent_rows();
        self.subagent_focus = match self.subagent_focus_index() {
            Some(i) if i > 0 => rows.get(i - 1).map(|r| r.id.clone()),
            _ => None,
        };
    }

    /// Take the chosen subagent off the list, if it has ended. The
    /// keyboard moves to the row that takes its place, or back to the
    /// prompt when the list is empty.
    pub fn dismiss_focused_subagent(&mut self) {
        let rows = self.subagent_rows();
        let Some(at) = self.subagent_focus_index() else {
            return;
        };
        if rows[at].status.is_none() {
            if self.caps().subagents.stop {
                self.flash("Still running: open it and press s to stop it");
            } else {
                self.flash(format!(
                    "Still running; {} cannot stop a subagent from here",
                    self.short_name()
                ));
            }
            return;
        }
        if let Some(run) = self
            .controller
            .conversation
            .transcript
            .agent_mut(&rows[at].id)
        {
            run.dismissed = true;
        }
        self.controller.persist();
        let rows = self.subagent_rows();
        self.subagent_focus = rows
            .get(at.min(rows.len().saturating_sub(1)))
            .map(|r| r.id.clone());
    }

    /// Enter on the list: that subagent's own transcript.
    pub fn open_focused_subagent(&mut self) {
        match self.subagent_focus_index() {
            Some(_) => {
                let id = self.subagent_focus.clone().unwrap_or_default();
                self.open_subagent(&id);
            }
            None => self.subagent_focus = None,
        }
    }

    /// Every subagent of the conversation, to choose one to look at.
    pub fn open_subagent_picker(&mut self) {
        let items: Vec<SubagentOption> = self
            .controller
            .conversation
            .transcript
            .agents()
            .into_iter()
            .map(|(id, run, depth)| SubagentOption {
                id: id.to_string(),
                description: run.description.clone(),
                kind: run.kind.clone(),
                status: run.status,
                depth,
            })
            .collect();
        if items.is_empty() {
            self.controller
                .conversation
                .transcript
                .push_notice("no subagents in this conversation");
            return;
        }
        // Start on the one in view, else the first still at work, else the latest.
        let at = items
            .iter()
            .position(|a| Some(&a.id) == self.viewing.as_ref())
            .or_else(|| items.iter().position(|a| a.status.is_none()))
            .unwrap_or(items.len() - 1);
        self.modal = Some(Modal::Subagents(
            ListPicker::new(items).with_selected(Some(at)),
        ));
    }

    /// Show the transcript of the subagent that the tool call `id` spawned
    /// in place of the main one.
    pub fn open_subagent(&mut self, id: &str) {
        if self.controller.conversation.transcript.agent(id).is_none() {
            return;
        }
        if self.viewing.is_none() {
            self.main_scroll = (self.scroll, self.auto_scroll);
        }
        self.viewing = Some(id.to_string());
        // Back from it, the keyboard is on its row of the list, if it has one.
        self.subagent_focus = Some(id.to_string());
        self.show_from_the_end();
    }

    /// Back to the main transcript, where it was.
    pub fn close_subagent_view(&mut self) {
        self.split().1.close_subagent_view();
    }

    /// Another transcript is to be drawn: nothing kept from the last one applies.
    pub(super) fn show_from_the_end(&mut self) {
        self.split().1.show_from_the_end();
    }

    /// The subagent in view, if it still exists (a rewind may have removed it).
    pub fn viewed(&self) -> Option<&super::transcript::AgentRun> {
        self.controller
            .conversation
            .transcript
            .agent(self.viewing.as_deref()?)
    }

    /// The next (or previous) subagent's transcript, round and round.
    pub fn view_next_subagent(&mut self, back: bool) {
        let ids: Vec<String> = self
            .controller
            .conversation
            .transcript
            .agents()
            .into_iter()
            .map(|(id, _, _)| id.to_string())
            .collect();
        let Some(at) = ids.iter().position(|i| Some(i) == self.viewing.as_ref()) else {
            return;
        };
        let step = if back { ids.len() - 1 } else { 1 };
        let next = ids[(at + step) % ids.len()].clone();
        self.open_subagent(&next);
    }

    /// The blocks on screen: the main transcript's, or those of the subagent in view.
    pub fn shown_blocks(&self) -> &[super::transcript::Block] {
        match self.viewed() {
            Some(run) => &run.log.blocks,
            None => &self.controller.conversation.transcript.blocks,
        }
    }

    /// The transcript on screen, for expanding and collapsing its tool calls.
    pub fn shown_transcript_mut(&mut self) -> &mut Transcript {
        let id = self.viewing.clone().unwrap_or_default();
        if self.controller.conversation.transcript.agent(&id).is_some() {
            return &mut self
                .controller
                .conversation
                .transcript
                .agent_mut(&id)
                .unwrap()
                .log;
        }
        &mut self.controller.conversation.transcript
    }

    /// Stop the subagent in view.
    pub fn stop_viewed_subagent(&mut self) {
        let Some(id) = self.viewing.clone() else {
            return;
        };
        self.with(|c, screen| c.stop_subagent(id, screen));
    }
}
