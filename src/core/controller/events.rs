//! The events of the active harness's session: what each does to the
//! conversation, the session and the turn.

use super::{Controller, Front};
use crate::core::{AgentEvent, SendNow, StopReason, Usage, UsageScope};

impl Controller {
    /// The active harness's session reported `ev`. What it changes on
    /// screen part of the way through goes through `front`.
    pub fn on_event(&mut self, ev: AgentEvent, front: &mut dyn Front) {
        let sender = self.short_name().to_string();
        match ev {
            AgentEvent::SessionStarted { session_id, model } => {
                self.fill_context_window();
                if let Some(model) = &model {
                    self.model_reported(model);
                }
                // A session nobody has prompted is not saved, nor is the
                // conversation for it: not a new one, not a fork made at its
                // start, not one that replaced a session it could not resume.
                if self.session_blank {
                    self.blank_session = Some((session_id, model));
                } else {
                    self.commit_session(session_id, model);
                }
            }
            AgentEvent::SessionReplaced { reason } => self.session_replaced(&reason),
            AgentEvent::SessionRenewed { reason } => self.session_renewed(&reason, front),
            AgentEvent::Restarted => self.process_restarted(front),
            AgentEvent::TurnStarted => {
                if !self.is_generating {
                    self.start_generation(front);
                }
            }
            AgentEvent::TextDelta(t) => self.conversation.transcript.append_assistant(&sender, &t),
            AgentEvent::ThinkingDelta(t) => self.conversation.transcript.append_thought(&t),
            AgentEvent::ToolCallStarted { id, name, input } => {
                self.tool_inputs
                    .insert(id.clone(), (name.clone(), input.clone()));
                self.conversation.transcript.tool_started(&id, &name, input)
            }
            // Live output. Streaming args arrive before the block exists and
            // are dropped there; ToolCallStarted carries the final input.
            AgentEvent::ToolCallDelta { id, delta, .. } => {
                self.conversation.transcript.tool_delta(&id, &delta)
            }
            AgentEvent::ToolCallResult {
                id,
                output,
                is_error,
            } => {
                self.notice_blocked(&id, &output, is_error);
                self.conversation
                    .transcript
                    .tool_result(&id, &output, is_error)
            }
            AgentEvent::PermissionRequest(req) => self.on_permission_request(req, front),
            AgentEvent::PermissionWithdrawn { id } => self.withdraw_prompt(&id, front),
            AgentEvent::Usage(u) => self.usage_reported(u),
            AgentEvent::PlanUpdated { entries, .. } => self.conversation.plan = entries,
            // What a subagent does goes into its own transcript, not this
            // one: several at work would otherwise drown the main agent.
            AgentEvent::Sub { parent, event } => match *event {
                // These find their own place (ids are unique), or are the user's to answer.
                ev @ (AgentEvent::ToolCallDelta { .. }
                | AgentEvent::ToolCallResult { .. }
                | AgentEvent::PermissionRequest(_)
                | AgentEvent::Sub { .. }
                | AgentEvent::SubagentStarted { .. }
                | AgentEvent::SubagentProgress { .. }
                | AgentEvent::SubagentEnded { .. }) => self.on_event(ev, front),
                ev => {
                    if let AgentEvent::ToolCallStarted { id, name, input } = &ev {
                        self.tool_inputs
                            .insert(id.clone(), (name.clone(), input.clone()));
                        self.tracking
                            .subagent_tool_started(&parent, id, name, input);
                    }
                    let sender = self
                        .conversation
                        .transcript
                        .agent(&parent)
                        .map_or("subagent", |run| run.sender())
                        .to_string();
                    // Nothing known of the call that spawned it: nowhere to put it.
                    let Some(log) = self.conversation.transcript.agent_log(&parent) else {
                        return;
                    };
                    match ev {
                        AgentEvent::ToolCallStarted { id, name, input } => {
                            log.tool_started(&id, &name, input)
                        }
                        AgentEvent::TextDelta(t) => log.append_assistant(&sender, &t),
                        AgentEvent::ThinkingDelta(t) => log.append_thought(&t),
                        AgentEvent::Notice(n) => log.push_notice(n),
                        AgentEvent::Compacted(_) => log.push_notice("context compacted"),
                        AgentEvent::Error(e) => log.push_error(e),
                        AgentEvent::HookStarted { id, name } => log.hook_started(&id, &name),
                        AgentEvent::HookEnded {
                            id,
                            name,
                            outcome,
                            output,
                        } => log.hook_ended(&id, &name, outcome, &output),
                        _ => {}
                    }
                }
            },
            AgentEvent::SubagentStarted {
                id,
                description,
                kind,
                unreported_end,
            } => {
                self.conversation
                    .transcript
                    .agent_started(&id, &description, kind.as_deref());
                self.tracking
                    .subagent_started(id, description, kind, unreported_end);
            }
            AgentEvent::SubagentProgress { id, activity } => {
                self.tracking.subagent_progress(&id, activity)
            }
            AgentEvent::SubagentEnded { id, status, result } => {
                self.conversation
                    .transcript
                    .agent_ended(&id, status, result.as_deref());
                self.tracking.subagent_ended(&id, status);
                // Between turns nothing else would save its report.
                if !self.is_generating {
                    self.persist();
                }
            }
            AgentEvent::Context(c) => {
                if let Some(window) = c.window {
                    self.learn_context_window(window);
                    self.conversation.window_reported = true;
                }
                self.conversation.context.merge(c);
            }
            AgentEvent::ContextUnknown => self.conversation.context.used = None,
            AgentEvent::RateLimit(r) => self.rate_limit_reported(self.active, r),
            AgentEvent::Account(a) => self.account_reported(self.active, a),
            AgentEvent::SessionStats(s) => self.session_stats_reported(s),
            AgentEvent::RewindFailed { reason } => {
                let name = self.short_name();
                if self.is_generating {
                    // The edited prompt is already running in the old session.
                    self.conversation.transcript.push_error(format!(
                        "{name} could not rewind its session ({reason}): it still remembers the turns you removed"
                    ));
                } else {
                    self.conversation.transcript.push_notice(format!(
                        "{name} could not rewind its session ({reason}); it will start a fresh one with the conversation so far"
                    ));
                    self.forget_session(self.active, front);
                    self.persist();
                }
            }
            AgentEvent::TurnAnchor { id } => {
                // The anchor of a prompt sent at once that the CLI has, told
                // before the turn it was to stop has ended (no other turn's
                // anchor is owed): the CLI took it into that turn.
                if self.is_generating
                    && self.conversation.anchor_pending.is_empty()
                    && let Some(p) = self.sending_now.take_if(|p| p.with_cli.is_some())
                {
                    self.sent_now_taken_in(p);
                }
                self.turn_anchor(self.active, id)
            }
            AgentEvent::PolicyChanged(p) => self.on_policy_changed(p),
            AgentEvent::CapabilitiesChanged(update) => {
                let report = self.report(self.active, update, self.session_alive);
                for notice in report.notices {
                    self.conversation.transcript.push_notice(notice);
                }
                if report.commands {
                    front.commands_arrived();
                }
                if let Some(refused) = report.refused {
                    self.policy_refused(refused, front);
                }
            }
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::Error(e),
            } if self.session_blank && !self.is_generating => {
                // A session started early failed before anything was sent
                // to it (Codex's thread, an ACP agent's session/new): a
                // prompt would wait on it for good.
                self.conversation.transcript.push_error(e);
                self.shutdown_session(front);
                self.idle_start_paused = true;
            }
            AgentEvent::TurnCompleted { stop_reason } => {
                let done = stop_reason == StopReason::Done;
                let failed = matches!(stop_reason, StopReason::Error(_));
                let sending = self.sending_now.take();
                // A hook or a call of a turn cut short is not reported on.
                if !done {
                    self.conversation.transcript.end_turn_hooks();
                    self.conversation.transcript.end_turn_calls();
                }
                match stop_reason {
                    StopReason::Done => {}
                    StopReason::Interrupted => self
                        .conversation
                        .transcript
                        .push_system("Turn interrupted."),
                    StopReason::Error(e) => self.conversation.transcript.push_error(e),
                }
                self.check_guard();
                self.finish_generation();
                let held = self.handoff.is_some_and(|h| h.held);
                self.finish_handoff(done, front);
                self.persist();
                self.restart_for_grants(front);
                self.blocks_turn_ended();
                // A prompt sent at once is the next turn. A clean finish
                // moves on to the next queued prompt; after an interrupt or
                // error the user decides (Enter sends it), also when a
                // handoff summary came in between.
                if let Some(p) = sending {
                    self.sent_now_turn(p, !failed, front);
                } else if done && !held {
                    self.send_next_queued(front);
                } else {
                    self.say_held();
                }
                // What the sandbox kept the turn from, unless the next
                // turn is already on its way.
                self.show_next_prompt(front);
            }
            AgentEvent::HookStarted { id, name } => {
                self.conversation.transcript.hook_started(&id, &name)
            }
            AgentEvent::HookEnded {
                id,
                name,
                outcome,
                output,
            } => self
                .conversation
                .transcript
                .hook_ended(&id, &name, outcome, &output),
            AgentEvent::RemoteControl(r) => self.on_remote_control(r),
            AgentEvent::RemotePrompt { text } => self.remote_prompt(text),
            AgentEvent::Compacted(c) => self.compacted(c),
            AgentEvent::Notice(n) => self.conversation.transcript.push_notice(n),
            AgentEvent::Error(e) => self.conversation.transcript.push_error(e),
            AgentEvent::ProcessExited { code } => {
                let sending = self.sending_now.take();
                self.session_alive = false;
                // A choice it was sent will not be answered.
                self.choice_unanswered();
                self.forget_refusals(front);
                if self.session_blank {
                    self.conversation.transcript.push_notice(format!(
                        "{} exited{} before any prompt; your next prompt starts it again",
                        self.short_name(),
                        code.map(|c| format!(" with code {c}")).unwrap_or_default()
                    ));
                }
                self.session_blank = false;
                self.blank_session = None;
                // It is not started again until the user does something.
                self.idle_start_paused = true;
                if self.remote.is_some() {
                    self.conversation.transcript.push_notice(format!(
                        "Remote Control is paused until {} runs again (your next prompt)",
                        self.short_name()
                    ));
                }
                self.drop_subagents();
                self.check_guard();
                self.process_gone(front);
                self.conversation.transcript.end_running_tools();
                // The interrupt asked for, where it ends the process.
                let ended_by_interrupt = matches!(code, None | Some(0))
                    && sending.as_ref().is_some_and(|p| {
                        p.with_cli.is_none() && self.caps().send_now == SendNow::AfterExit
                    });
                if self.is_generating {
                    self.finish_generation();
                    if ended_by_interrupt {
                        self.conversation
                            .transcript
                            .push_system("Turn interrupted.");
                    } else {
                        self.conversation.transcript.push_error(format!(
                            "{} exited{} before the turn completed",
                            self.short_name(),
                            code.map(|c| format!(" with code {c}")).unwrap_or_default()
                        ));
                    }
                }
                self.finish_handoff(false, front);
                match sending {
                    // Its interrupt ended the process (agy): the prompt
                    // starts it again, resuming the session.
                    Some(p) if ended_by_interrupt => self.send_ahead(p.prompt, front),
                    // It went with the process.
                    Some(p) => self.give_back(p, front),
                    None => {}
                }
            }
        }
    }

    /// The main agent reported `u`. The session's totals take each report
    /// once. The turn's numbers stay what the last turn reported until
    /// this one reports: a whole turn's numbers replace them, the first of
    /// a turn's requests starts them again and each one after adds to
    /// them. A report with no tokens (Claude's `/compact` gives only its
    /// cost) replaces nothing.
    fn usage_reported(&mut self, u: Usage) {
        let c = &mut self.conversation;
        if u.cumulative {
            c.session_usage = u;
            return;
        }
        c.session_usage.add(&u);
        match u.scope {
            UsageScope::Request if c.turn_reported => c.turn_usage.add(&u),
            _ if u.total_tokens() == 0 => {}
            _ => {
                c.turn_usage = u;
                c.turn_reported = true;
            }
        }
    }
}
