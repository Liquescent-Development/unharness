//! Switching harnesses: the bridge, its budget, and the handoff summary.

use super::{Action, App, Handoff};
use crate::config::BridgeSummary;
use crate::core::conversations::{CONTEXT_WINDOW_MAX, CONTEXT_WINDOW_MIN, save_context_windows};
use crate::core::{ContextUsage, HarnessId, ModelRef, Usage};
use crate::tui::modal::{ListPicker, Modal};
use crate::tui::transcript::DEFAULT_BRIDGE_MAX_CHARS;

impl App {
    /// How long the bridge to `harness` may be: `bridge_max_chars` when
    /// set, else a quarter of the context window its model last reported
    /// here at about four characters a token, else the default.
    pub fn bridge_budget(&self, harness: HarnessId) -> usize {
        self.bridge_max_chars
            .or_else(|| {
                self.context_windows
                    .get(&harness)?
                    .get(&self.model_key(harness))
                    .map(|w| {
                        usize::try_from(*w).unwrap_or(usize::MAX) / BRIDGE_WINDOW_SHARE
                            * CHARS_PER_TOKEN
                    })
            })
            .unwrap_or(DEFAULT_BRIDGE_MAX_CHARS)
    }

    /// The model `harness` runs, as the context windows are kept by.
    fn model_key(&self, harness: HarnessId) -> String {
        self.models
            .get(&harness)
            .map_or_else(|| "default".to_string(), ModelRef::label)
    }

    /// Keep the window the active harness's model reported, for the next
    /// bridge to it.
    pub(super) fn learn_context_window(&mut self, window: u64) {
        let window = window.clamp(CONTEXT_WINDOW_MIN, CONTEXT_WINDOW_MAX);
        let model = self.model_key(self.active);
        let known = self.context_windows.entry(self.active).or_default();
        if known.get(&model) == Some(&window) {
            return;
        }
        known.insert(model, window);
        // Only a measure: a write that fails costs a default budget later.
        let _ = save_context_windows(&self.context_windows_path, &self.context_windows);
    }

    /// Where the bridge to `harness` starts: where it was last active,
    /// nothing when its live session saw it all, everything on its first
    /// visit.
    pub(super) fn bridge_start(&self, harness: HarnessId) -> usize {
        let visited = self.session_ids.contains_key(&harness)
            || (harness == self.active && self.session_seen());
        if visited {
            self.last_active_index
                .get(&harness)
                .copied()
                .unwrap_or(self.transcript.blocks.len())
        } else {
            0
        }
    }

    pub fn switch_harness(&mut self, next: HarnessId) {
        if next == self.active {
            return;
        }
        if self.registry.get(next).is_none() {
            self.transcript
                .push_error(format!("harness '{next}' is not available"));
            return;
        }
        if self.is_generating {
            self.transcript
                .push_error("finish or interrupt the current turn before switching harness");
            return;
        }
        if self.refused_while_start_held() {
            return;
        }
        if self.handoff_wanted_for(self.bridge_start(next), self.bridge_budget(next)) {
            self.ask_handoff(next, false);
            return;
        }
        self.switch_now(next);
    }

    /// Whether the conversation will not fit in the bridge to `next`, and
    /// the harness being left can say what it was about: it has a session,
    /// live or one it can resume (a fresh one would summarize nothing), and
    /// that session saw the whole transcript. Not while its subagents are
    /// at work: what they ask or report would land in the summary's turn
    /// (one whose end may never be told does not count: it would hold the
    /// summary until the session ends).
    ///
    /// `from` is where the bridge to the next session starts and `budget`
    /// its length.
    pub(super) fn handoff_wanted_for(&self, from: usize, budget: usize) -> bool {
        self.bridge_summary == BridgeSummary::Auto
            && (self.session_seen()
                || (self.session_ids.contains_key(&self.active) && self.caps().resume_by_id))
            && self.awaited_subagents() == 0
            && self.bridge_start(self.active) >= self.transcript.blocks.len()
            && self.shell.is_none()
            && self.effective_policy().is_some()
            && super::bridge::needs_summary(
                self.transcript.blocks.get(from..).unwrap_or_default(),
                budget,
            )
    }

    /// Ask the active harness for a summary for `next`, switching when its
    /// turn ends (Esc switches without it). It is told to use no tools,
    /// and what it asks permission for is denied.
    /// `renew`: the next session is a new one of the active harness
    /// (`renew_session`), not `next`'s.
    pub(super) fn ask_handoff(&mut self, next: HarnessId, renew: bool) {
        let to = self.handoff_target(next, renew);
        self.transcript.push_system(format!(
            "The conversation is longer than the bridge to {to}: asking {} for a handoff summary first (Esc {} without one)",
            self.short_name(),
            if renew { "goes on" } else { "switches" }
        ));
        self.handoff = Some(Handoff {
            to: next,
            start: self.transcript.blocks.len(),
            held: !self.queued.is_empty(),
            renew,
            blind: false,
        });
        self.start_generation();
        self.ensure_session();
        self.mark_prompted();
        let budget = self.bridge_budget(next);
        self.actions.push_back(Action::SendTurn {
            text: if renew {
                renew_handoff_prompt(budget)
            } else {
                handoff_prompt(&to, budget)
            },
            attachments: Vec::new(),
        });
    }

    /// What the next session is called in a handoff's messages.
    fn handoff_target(&self, next: HarnessId, renew: bool) -> String {
        let name = self
            .registry
            .get(next)
            .map_or("another agent", |h| h.descriptor().short_name);
        if renew {
            format!("a new {name} session")
        } else {
            name.to_string()
        }
    }

    /// The handoff turn is over, however it ended: the switch goes ahead.
    pub(super) fn finish_handoff(&mut self, done: bool) {
        let Some(h) = self.handoff.take() else {
            return;
        };
        let to = self.handoff_target(h.to, h.renew);
        if !(done && !h.blind && self.transcript.mark_handoff(h.start, &to)) {
            self.transcript.push_notice(format!(
                "no handoff summary; {to} gets the conversation as it is"
            ));
        }
        let left = self.active;
        if h.renew {
            self.renew_now();
        } else {
            self.switch_now(h.to);
            // The session left was new and saw only the request for a
            // summary: coming back starts afresh, with the conversation.
            if h.blind {
                self.forget_session(left);
            }
        }
    }

    pub(super) fn switch_now(&mut self, next: HarnessId) {
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session();
        }
        self.idle_start_paused = false;
        // What this harness saw: everything but the `!` commands run since
        // its last prompt, which it is told about when it comes back.
        let seen = self
            .transcript
            .blocks
            .iter()
            .position(|b| matches!(b, super::transcript::Block::Shell { sent: false, .. }))
            .unwrap_or(self.transcript.blocks.len());
        // Context held back for this harness (by a command sent alone) is
        // still to be told.
        let seen = self
            .last_active_index
            .get(&self.active)
            .map_or(seen, |held| seen.min(*held));
        self.last_active_index.insert(self.active, seen);
        self.sync_conversation();
        self.active = next;
        self.session_usage = self
            .conversation
            .usage
            .get(&next)
            .cloned()
            .unwrap_or_default();
        self.turn_usage = Usage::default();
        self.context = ContextUsage::default();
        self.anchor_pending.clear();
        self.persist();
        self.transcript.push_system(format!(
            "Switched to {} (model: {}, effort: {}, policy: {}, sandbox: {})",
            self.display_name(),
            self.model_label(),
            self.current_effort().unwrap_or("default"),
            self.policy_label(),
            self.sandbox_level().0
        ));
        if let Some(w) = self.policy_warning() {
            self.transcript.push_notice(w);
        }
        self.require_policy();
    }

    pub fn open_harness_picker(&mut self) {
        let idx = self
            .harness_options
            .iter()
            .position(|o| o.id == self.active);
        self.modal = Some(Modal::Harness(
            ListPicker::new(self.harness_options.clone())
                .with_disabled(|o| (!o.installed).then(|| "not found on PATH".to_string()))
                .with_selected(idx),
        ));
    }

    /// Whether `id` was found when the harnesses were probed. One that was
    /// not probed is taken as there.
    pub(super) fn harness_installed(&self, id: HarnessId) -> bool {
        self.harness_options
            .iter()
            .find(|o| o.id == id)
            .is_none_or(|o| o.installed)
    }
}

/// The bridge takes at most this fraction (1/n) of the window it goes to.
const BRIDGE_WINDOW_SHARE: usize = 4;

/// About what a token is in English prose and code.
const CHARS_PER_TOKEN: usize = 4;

/// However large the window, a summary longer than this is no longer one.
const HANDOFF_MAX_WORDS: usize = 1_500;

/// What the harness being left is asked for. It stays in that harness's
/// session, so what it forbids is said to hold for this reply only.
/// `budget`: the characters the bridge to `to` may take, half of which
/// the summary is asked to stay within (in words, at about six characters
/// a word), the rest being for the first prompt and what follows it.
pub(super) fn handoff_prompt(to: &str, budget: usize) -> String {
    let words = (budget / 2 / 6).clamp(50, HANDOFF_MAX_WORDS);
    format!(
        "[unharness] The user is switching this conversation to {to}, another coding agent \
         that will see only part of it. Write a handoff summary for that agent, in at most \
         {words} words: the user's goal and constraints, the decisions made and why, what has \
         been done (files changed, commands run and what came of them), what is unfinished or \
         failing, and the next steps. Be specific (paths, names, error messages). Leave out \
         your own setup (tools, connectors, MCP servers) unless the task depends on it. For \
         this reply only, use no tools and change nothing: answer from what you already know. \
         The conversation may come back to you later, with your tools as before."
    )
}

/// What a session left for a new one of the same harness is asked for
/// (`App::renew_session`), as [`handoff_prompt`] is.
fn renew_handoff_prompt(budget: usize) -> String {
    let words = (budget / 2 / 6).clamp(50, HANDOFF_MAX_WORDS);
    format!(
        "[unharness] The user is moving this conversation to a new session of yours, with \
         another model or reasoning effort, that will see only part of it. Write a handoff \
         summary for that session, in at most {words} words: the user's goal and constraints, \
         the decisions made and why, what has been done (files changed, commands run and what \
         came of them), what is unfinished or failing, and the next steps. Be specific (paths, \
         names, error messages). For this reply only, use no tools and change nothing: answer \
         from what you already know."
    )
}
