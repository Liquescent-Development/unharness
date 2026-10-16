//! The active harness's session: started, committed, replaced and ended.

use super::{Action, Controller, Front};
use crate::core::conversations::ShellStatus;
use crate::core::guard::{self, Watch};
use crate::core::mcp::{self, McpServer};
use crate::core::{AgentEvent, HarnessId, StopReason};

impl Controller {
    /// The configured MCP servers the active harness takes for a session.
    /// The ones it does not take are named once per harness.
    pub fn session_mcp_servers(&mut self) -> Vec<McpServer> {
        let (servers, warnings) = mcp::for_harness(
            &self.config.mcp_servers().0,
            self.caps().mcp,
            self.short_name(),
            &self.harness().own_mcp_servers(),
        );
        if !warnings.is_empty() && self.mcp_warned.insert(self.active) {
            for w in warnings {
                self.conversation.transcript.push_notice(w);
            }
        }
        servers
    }

    /// Show what the active harness says about running in this folder
    /// ([`Harness::session_notice`]), once per harness in a run: a session
    /// starts again on every model, policy or sandbox change.
    ///
    /// [`Harness::session_notice`]: crate::harness::Harness::session_notice
    pub fn show_session_notice(&mut self) {
        if self.session_noticed.contains(&self.active) {
            return;
        }
        if let Some(notice) = self.harness().session_notice(&self.cwd) {
            self.session_noticed.insert(self.active);
            self.conversation.transcript.push_notice(notice);
        }
    }

    /// Start watching the active harness's configuration files, before its
    /// process starts.
    pub fn arm_guard(&mut self) {
        self.guard = Some(Watch::begin(
            &self
                .harness()
                .guarded(self.workspace_root.as_deref().unwrap_or(&self.cwd)),
            dirs::home_dir().as_deref(),
        ));
    }

    /// Tell the user about configuration files that changed since the last
    /// check.
    pub(super) fn check_guard(&mut self) {
        let name = self.display_name();
        let Some(watch) = self.guard.as_mut() else {
            return;
        };
        for change in watch.changes(&guard::default_keep_dir()) {
            self.conversation
                .transcript
                .push_error(change.describe(name));
        }
    }

    /// A session for the active harness, unless it has one or one is on
    /// its way.
    pub(super) fn ensure_session(&mut self) {
        if !self.session_alive && !self.start_pending() {
            let resume = self.conversation.session_ids.get(&self.active).cloned();
            self.actions.push_back(Action::StartSession { resume });
        }
    }

    /// Start the active harness's session before anything is sent to it,
    /// so that what it reports (its commands above all) is there before the
    /// first prompt. The event loop calls this before it runs the actions;
    /// nothing starts while a policy is still to be chosen, during a turn,
    /// or after a session ended by itself or failed to start. Returns
    /// whether the harness, which starts no session before the first
    /// prompt, is to be asked for its commands instead.
    pub fn start_when_idle(&mut self) -> bool {
        if self.should_quit
            || self.session_alive
            || self.start_pending()
            || self.is_generating
            || self.handoff.is_some()
            || self.idle_start_paused
            || self.effective_policy(self.active).is_none()
        {
            return false;
        }
        if !self.caps().start_unprompted {
            return true;
        }
        let resume = self.conversation.session_ids.get(&self.active).cloned();
        self.actions.push_back(Action::StartSession { resume });
        self.session_blank = true;
        false
    }

    /// The session could not be started. The next prompt tries again.
    pub fn start_failed(&mut self, why: String, front: &mut dyn Front) {
        self.session_blank = false;
        self.blank_session = None;
        self.idle_start_paused = true;
        if self.is_generating {
            self.on_event(
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Error(why),
                },
                front,
            );
        } else {
            self.conversation.transcript.push_error(why);
        }
    }

    /// The session's driver stopped taking commands.
    pub fn driver_gone(&mut self, front: &mut dyn Front) {
        self.conversation
            .transcript
            .push_error("session driver is gone");
        if let Some(p) = self.sending_now.take() {
            self.give_back(p, front);
        }
        self.session_alive = false;
        self.choice_unanswered();
        self.forget_refusals(front);
        self.session_blank = false;
        self.blank_session = None;
        self.idle_start_paused = true;
        self.forget_reported_model(self.active);
    }

    /// A start is queued, or held until the CLI before it is gone.
    fn start_pending(&self) -> bool {
        self.start_held
            || self
                .actions
                .iter()
                .any(|a| matches!(a, Action::StartSession { .. }))
    }

    /// Whether the live session has been sent something: one started
    /// early has seen nothing of the conversation.
    pub fn session_seen(&self) -> bool {
        self.session_alive && !self.session_blank
    }

    /// Something goes to the session: from here on it is the
    /// conversation's.
    pub(super) fn mark_prompted(&mut self) {
        self.session_blank = false;
        if let Some((id, model)) = self.blank_session.take() {
            self.commit_session(id, model);
        }
    }

    /// `id` is the active harness's session in this conversation.
    pub(super) fn commit_session(&mut self, session_id: String, model: Option<String>) {
        let is_new = self.conversation.session_ids.get(&self.active) != Some(&session_id);
        // A branch of the forked-from session now exists under its own id.
        if is_new {
            self.conversation.fork_pending.remove(&self.active);
        }
        self.conversation
            .session_ids
            .insert(self.active, session_id.clone());
        self.persist();
        if is_new {
            self.conversation.transcript.push_notice(format!(
                "session {} ({})",
                session_id,
                model.unwrap_or_else(|| "default model".into())
            ));
        }
    }

    /// The active harness could not resume or fork its session and runs a
    /// new one, which knows nothing of the conversation
    /// (`AgentEvent::SessionReplaced`). It is taken as a session started
    /// for a harness switched to: the next prompt tells it the
    /// conversation so far (within the bridge's budget), and the
    /// conversation keeps its id once a prompt has gone to it. What
    /// pointed into the old session (rewind anchors, a pending fork) goes.
    pub(super) fn session_replaced(&mut self, reason: &str) {
        let harness = self.active;
        let name = self.short_name().to_string();
        self.conversation.fork_pending.remove(&harness);
        self.conversation.anchors.retain(|a| a.harness != harness);
        self.conversation.context.used = None;
        if self.session_blank {
            // The new session's id waits in `blank_session` for the first
            // prompt; the old one is not this conversation's any more.
            self.conversation.session_ids.remove(&harness);
            self.conversation.last_active_index.remove(&harness);
            self.conversation.transcript.push_notice(format!(
                "{reason}: {name} starts a new session, and your next prompt tells it the conversation so far"
            ));
            return;
        }
        // A prompt went with the start, without the conversation: the
        // next one tells it all of it. The session's id is committed by
        // its `SessionStarted`, before or after this.
        self.conversation.last_active_index.insert(harness, 0);
        if let Some(handoff) = self.handoff.as_mut() {
            handoff.blind = true;
            self.conversation.transcript.push_notice(format!(
                "{reason}: {name} runs a new session, which has no conversation to summarize"
            ));
        } else {
            self.conversation.transcript.push_notice(format!(
                "{reason}: {name} runs a new session without the conversation so far; your next prompt tells it"
            ));
        }
    }

    /// Stop using `harness`'s vendor session: its next turn starts a fresh
    /// one and gets the conversation so far as context.
    pub(super) fn forget_session(&mut self, harness: HarnessId, front: &mut dyn Front) {
        if harness == self.active {
            self.end_remote_control();
        }
        if harness == self.active && self.session_alive {
            self.shutdown_session(front);
        }
        self.forget_ids(harness);
    }

    /// True, with an error shown, while the session is settling
    /// (`Controller::settling`).
    pub(super) fn refused_while_settling(&mut self) -> bool {
        match self.settling(self.active, self.start_held) {
            Some(why) => {
                self.conversation.transcript.push_error(why);
                true
            }
            None => false,
        }
    }

    /// The session's process is gone, or going: what it was running and
    /// would have reported on will not be.
    pub(super) fn process_gone(&mut self, front: &mut dyn Front) {
        self.conversation.transcript.end_running_hooks();
        // The next process takes the messages so far as its baseline, so
        // the anchors still owed would land on later turns (pi).
        self.conversation.anchor_pending.clear();
        self.drop_prompts(front);
        self.forget_reported_model(self.active);
    }

    /// End the active harness's session. It is not alive from here on: a
    /// prompt handled before the loop runs the shutdown starts the next.
    pub(super) fn shutdown_session(&mut self, front: &mut dyn Front) {
        // What it changed since the last turn ended, or since it started
        // when it never had one, is told before the next process is watched.
        if self.session_alive {
            self.check_guard();
        }
        self.session_alive = false;
        self.session_blank = false;
        self.blank_session = None;
        self.idle_start_paused = false;
        self.drop_subagents();
        self.drop_prompts(front);
        // A prompt sent at once goes with the session it was for.
        if let Some(p) = self.sending_now.take() {
            self.give_back(p, front);
        }
        // The next session reports where it runs.
        self.process_ended(self.active);
        self.forget_refusals(front);
        self.actions.push_back(Action::Shutdown);
        // A choice it was sent will not be answered now.
        self.choice_unanswered();
    }

    /// The active harness's session cannot take a choice, for `reason`
    /// (`AgentEvent::SessionRenewed`, or its stored session keeps what it
    /// ran with none live to say so, `Route::Renew`): the conversation goes
    /// on in a new session, told it with the next prompt as a harness
    /// switched to is, and the one left writes a handoff summary first when
    /// the conversation does not fit in that bridge. What was typed after
    /// the choice goes after that, as if typed then.
    pub(super) fn session_renewed(&mut self, reason: &str, front: &mut dyn Front) {
        let pending = self.choice_answered(super::choices::provider_line);
        // A guard, not a path the TUI takes: no second choice goes to the
        // session before it has answered the first (`Controller::settling`)
        // nor during the summary's turn, so only a driver answering one
        // choice twice comes here with a renewal under way. That answer
        // takes only its own pending choice; a second summary or a
        // renewal in the middle of the first is not started.
        if self.handoff.is_some_and(|h| h.renew) {
            return;
        }
        let name = self.display_name();
        self.conversation.transcript.push_notice(format!(
            "{name} starts a new session for {reason}. Your next prompt tells it the conversation so far."
        ));
        if self.handoff_wanted_for(0, self.bridge_budget(self.active)) {
            self.ask_handoff(self.active, true, front);
            if let (Some(pending), Some(handoff)) = (&pending, self.handoff.as_mut()) {
                handoff.held = pending.queued > 0;
            }
        } else {
            self.renew_now(front);
            if let Some(pending) = &pending {
                self.send_typed_after(pending, front);
            }
        }
    }

    /// The session's CLI was started again by its driver, for a choice it
    /// takes only that way (`AgentEvent::Restarted`): what went with the
    /// process before goes here too, as when the TUI ended a session and
    /// started it again, and the session goes on, resumed, in a process
    /// nothing was sent to yet. Its driver puts it back on Remote Control.
    pub(super) fn process_restarted(&mut self, front: &mut dyn Front) {
        let name = self.display_name();
        let pending = self.choice_answered(|provider| super::choices::restart_line(provider, name));
        // A turn the process before had started by itself cannot end now.
        if self.is_generating {
            self.conversation.transcript.end_running_tools();
            self.finish_generation();
            self.conversation
                .transcript
                .push_system("Turn interrupted.");
        }
        self.drop_subagents();
        self.drop_prompts(front);
        self.process_ended(self.active);
        self.forget_refusals(front);
        self.session_blank = true;
        self.blank_session = None;
        let policy = self.effective_policy(self.active);
        self.policy.started_under(policy);
        if let Some(remote) = self.remote.as_mut() {
            remote.url = None;
            remote.state = None;
        }
        if let Some(pending) = &pending {
            self.send_typed_after(pending, front);
        }
    }

    /// Forget the active harness's session: the next one starts new, with
    /// the conversation in its first prompt's bridge.
    pub(super) fn renew_now(&mut self, front: &mut dyn Front) {
        self.forget_session(self.active, front);
        // What the session left reported in its handoff turn may have been
        // on the model chosen before.
        self.fresh_context();
        self.idle_start_paused = false;
        self.persist();
    }

    pub fn quit(&mut self, front: &mut dyn Front) {
        self.end_remote_control();
        if self.session_alive {
            self.shutdown_session(front);
        }
        // The event loop kills it on the way out.
        if self.shell.take().is_some() {
            self.conversation
                .transcript
                .shell_ended(ShellStatus::Killed);
        }
        self.persist();
        self.should_quit = true;
    }
}
