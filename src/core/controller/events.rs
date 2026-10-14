//! The events of the active harness's session: what each does to the
//! conversation, the session and the turn.

use std::time::Instant;

use super::{Controller, Front, RunningSubagent};
use crate::core::transcript::tool_summary;
use crate::core::{AgentEvent, RateLimitInfo, StopReason};

impl Controller {
    pub fn on_event(&mut self, ev: AgentEvent, front: &mut dyn Front) {
        let sender = self.short_name().to_string();
        match ev {
            AgentEvent::SessionStarted { session_id, model } => {
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
            } => self
                .conversation
                .transcript
                .tool_result(&id, &output, is_error),
            AgentEvent::PermissionRequest(req) => self.on_permission_request(req, front),
            AgentEvent::PermissionWithdrawn { id } => self.withdraw_prompt(&id, front),
            AgentEvent::Usage(u) => {
                if u.cumulative {
                    self.conversation.session_usage = u;
                } else {
                    self.conversation.session_usage.add(&u);
                    self.conversation.turn_usage = u;
                }
            }
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
                    if let AgentEvent::ToolCallStarted { name, input, .. } = &ev
                        && let Some(a) = self.subagents.iter_mut().find(|a| a.id == parent)
                    {
                        a.tools += 1;
                        if !a.described {
                            a.activity = Some(
                                format!("{name} {}", tool_summary(name, input))
                                    .trim_end()
                                    .to_string(),
                            );
                        }
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
                self.subagents.retain(|a| a.id != id);
                self.subagents.push(RunningSubagent {
                    id,
                    description,
                    kind,
                    activity: None,
                    described: false,
                    tools: 0,
                    started: Instant::now(),
                    unreported_end,
                });
            }
            AgentEvent::SubagentProgress { id, activity } => {
                if let Some(a) = self.subagents.iter_mut().find(|a| a.id == id) {
                    a.activity = Some(activity);
                    a.described = true;
                }
            }
            AgentEvent::SubagentEnded { id, status, result } => {
                self.conversation
                    .transcript
                    .agent_ended(&id, status, result.as_deref());
                self.subagents.retain(|a| a.id != id);
                // Between turns nothing else would save its report.
                if !self.is_generating {
                    self.persist();
                }
            }
            AgentEvent::Context(c) => {
                if let Some(window) = c.window {
                    self.learn_context_window(window);
                }
                self.conversation.context.merge(c);
            }
            AgentEvent::RateLimit(r) => {
                // Warn once each time a window crosses the threshold.
                let was_high = self.rate_limit.as_ref().is_some_and(rate_limit_high);
                if !was_high && rate_limit_high(&r) {
                    self.conversation
                        .transcript
                        .push_notice(format!("rate limit: {}", rate_limit_summary(&r)));
                }
                self.rate_limit = Some(r);
            }
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
            AgentEvent::TurnAnchor { id } => self.turn_anchor(self.active, id),
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
                // A clean finish moves on to the next queued prompt; after an
                // interrupt or error the user decides (Enter sends it), also
                // when a handoff summary came in between.
                if done && !held {
                    self.send_next_queued(front);
                } else if !self.queued.is_empty() {
                    self.conversation.transcript.push_notice(format!(
                        "{} queued prompt(s) held; press Enter to send the next",
                        self.queued.len()
                    ));
                }
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
            AgentEvent::Notice(n) => self.conversation.transcript.push_notice(n),
            AgentEvent::Error(e) => self.conversation.transcript.push_error(e),
            AgentEvent::ProcessExited { code } => {
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
                if self.is_generating {
                    self.finish_generation();
                    self.conversation.transcript.push_error(format!(
                        "{} exited{} before the turn completed",
                        self.short_name(),
                        code.map(|c| format!(" with code {c}")).unwrap_or_default()
                    ));
                }
                self.finish_handoff(false, front);
            }
        }
    }
}

/// Percentage of a rate-limit window at which the user is warned.
const RATE_LIMIT_WARN_PERCENT: f32 = 80.0;

fn rate_limit_high(r: &RateLimitInfo) -> bool {
    r.windows
        .iter()
        .any(|w| w.used_percent.is_some_and(|p| p >= RATE_LIMIT_WARN_PERCENT))
}

pub fn rate_limit_summary(r: &RateLimitInfo) -> String {
    let windows: Vec<String> = r
        .windows
        .iter()
        .map(|w| match w.used_percent {
            Some(p) => format!("{} {p:.0}% used", w.label),
            None => w.label.clone(),
        })
        .collect();
    match (&r.status, windows.is_empty()) {
        (Some(s), true) => s.clone(),
        (Some(s), false) if s != "allowed" => format!("{s} ({})", windows.join(", ")),
        _ => windows.join(", "),
    }
}
