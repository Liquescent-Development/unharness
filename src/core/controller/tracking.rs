//! What a front end tracks of its session's events, whoever it shows them
//! to: the session's id and model and its usage, whether the agent went
//! into plan mode, the subagents at work and the turn ends still owed for
//! them, and who answers a permission request, including when nobody is
//! there to ask. None of it starts, ends or prompts a session: that
//! lifecycle is the TUI's controller's alone. The controller keeps one
//! ([`Controller::tracking`]); a run with no one to ask (`--print`) can
//! keep one without it.
//!
//! [`Controller::tracking`]: super::Controller::tracking

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;

use serde_json::Value;

use crate::core::permissions::{HandoffTo, Route, Router};
use crate::core::rules::{Rules, ToolAction};
use crate::core::transcript::tool_summary;
use crate::core::{
    PermissionDecision, PermissionKind, PermissionPolicy, PermissionRequest, SubagentStatus, Usage,
};

/// A subagent that is at work.
#[derive(Debug, Clone)]
pub struct RunningSubagent {
    /// The tool call that spawned it.
    pub id: String,
    pub description: String,
    pub kind: Option<String>,
    /// What it is doing now.
    pub activity: Option<String>,
    /// The harness words its activity itself, so its tool calls need not.
    described: bool,
    /// Tool calls it has made.
    pub tools: usize,
    pub started: Instant,
    /// Its end may never be told (`AgentEvent::SubagentStarted`): it holds
    /// nothing back (`Tracking::awaited`).
    pub unreported_end: bool,
}

/// What became of a request that only a user could answer, with nobody
/// there to ask ([`Tracking::nobody_to_ask`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unasked<'a> {
    /// A tool call, denied.
    Denied {
        tool: &'a str,
        action: &'a ToolAction,
    },
    /// A plan's approval, refused: the agent goes on planning.
    KeptPlanning,
    /// A question or another request for an answer, dismissed.
    Dismissed,
}

/// What a front end tracks of its session's events. See the module.
#[derive(Debug, Clone, Default)]
pub struct Tracking {
    /// The session's id, as it last reported it.
    pub session_id: Option<String>,
    /// Its model, as it last named one.
    pub model: Option<String>,
    /// Its usage so far, as the main agent reported it.
    pub usage: Option<Usage>,
    /// The main agent went into plan mode by itself: only a read is
    /// answered by a rule meanwhile.
    pub planning: bool,
    /// Subagents at work, at any depth, oldest first. They can outlive the
    /// turn that started them.
    pub subagents: Vec<RunningSubagent>,
    /// The tool call that spawned a subagent's subagent → the subagent that
    /// made it (kept where the harness reports subagents in turns of their
    /// own, `report_turn`).
    spawned_by: HashMap<String, String>,
    /// Subagents whose end was counted in `owed` and whose spawning call
    /// has not returned: one that returns now was waited for (a blocking
    /// subagent), and its report is the call's result.
    owing: HashSet<String>,
    /// Turn ends still to come before an answer is complete: the prompt's
    /// (`turn_expected`), and one for each subagent the harness reports in
    /// a turn of its own (`report_turn`).
    pub owed: u32,
    /// `SubagentSupport::report_turn` of the harness.
    report_turn: bool,
    /// Requests these allow are answered without asking.
    pub rules: Rules,
    /// Permission requests waiting behind another dialog for the user.
    pub pending_prompts: VecDeque<PermissionRequest>,
    /// Requests in `pending_prompts` the agent made while it planned: they
    /// are judged so also when they come out after the plan's approval.
    pub(crate) asked_planning: HashSet<String>,
}

impl Tracking {
    /// Requests are answered against `rules`.
    pub fn new(rules: Rules) -> Self {
        Tracking {
            rules,
            ..Default::default()
        }
    }

    /// The harness reports each subagent's end in a turn of its own
    /// (`SubagentSupport::report_turn`): those turns are owed too.
    pub fn report_turns(&mut self, report_turn: bool) {
        self.report_turn = report_turn;
    }

    /// The session started, as `session_id`, on `model` if it said.
    pub fn session_started(&mut self, session_id: &str, model: Option<&str>) {
        self.session_id = Some(session_id.to_string());
        if let Some(model) = model {
            self.model = Some(model.to_string());
        }
    }

    /// The main agent reported `u`: a total, or one turn's to add.
    pub fn usage_reported(&mut self, u: &Usage) {
        match self.usage.as_mut() {
            Some(total) if !u.cumulative => total.add(u),
            _ => self.usage = Some(u.clone()),
        }
    }

    /// The main agent runs under `p` now.
    pub fn policy_changed(&mut self, p: PermissionPolicy) {
        self.planning = p == PermissionPolicy::Plan;
    }

    /// A turn is to end before the answer is complete (the prompt's).
    pub fn turn_expected(&mut self) {
        self.owed += 1;
    }

    /// The main agent's turn ended.
    pub fn turn_ended(&mut self) {
        self.owed = self.owed.saturating_sub(1);
    }

    /// The subagents at work whose end the harness reports. One whose end
    /// may never be told (`SubagentStarted::unreported_end`) would hold
    /// back whatever waits for it until the session ends.
    pub fn awaited(&self) -> usize {
        self.subagents.iter().filter(|a| !a.unreported_end).count()
    }

    /// The subagents at work whose end may never be told.
    pub fn unawaited(&self) -> usize {
        self.subagents.iter().filter(|a| a.unreported_end).count()
    }

    /// The tool call `id` spawned a subagent, at work from now on.
    pub fn subagent_started(
        &mut self,
        id: String,
        description: String,
        kind: Option<String>,
        unreported_end: bool,
    ) {
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

    /// The subagent `id` says what it is doing.
    pub fn subagent_progress(&mut self, id: &str, activity: String) {
        if let Some(a) = self.subagents.iter_mut().find(|a| a.id == id) {
            a.activity = Some(activity);
            a.described = true;
        }
    }

    /// The subagent `parent` started the tool call `id`.
    pub fn subagent_tool_started(&mut self, parent: &str, id: &str, name: &str, input: &Value) {
        if let Some(a) = self.subagents.iter_mut().find(|a| a.id == parent) {
            a.tools += 1;
            if !a.described {
                a.activity = Some(
                    format!("{name} {}", tool_summary(name, input))
                        .trim_end()
                        .to_string(),
                );
            }
        }
        if self.report_turn {
            self.spawned_by.insert(id.to_string(), parent.to_string());
        }
    }

    /// The subagent `id` ended. Where the harness reports it in a turn of
    /// its own, that turn is owed: Claude reports each end of a background
    /// subagent in a turn of its own, wherever the end falls
    /// (`fixtures/subagent_parallel.jsonl`), but not one the agent stopped
    /// itself (`subagent_stopped.jsonl`). A subagent's subagent was seen
    /// reported once its parent had ended (`subagent_nested.jsonl`); while
    /// the parent runs, it is taken to report to the parent. Its report may
    /// come in a second end of the same id; only the first is news.
    pub fn subagent_ended(&mut self, id: &str, status: SubagentStatus) {
        let parent_running = self
            .spawned_by
            .get(id)
            .is_some_and(|p| self.subagents.iter().any(|a| a.id == *p));
        let was_running = self.subagents.iter().any(|a| a.id == id);
        self.subagents.retain(|a| a.id != id);
        if was_running && self.report_turn && status != SubagentStatus::Cancelled && !parent_running
        {
            self.owed += 1;
            self.owing.insert(id.to_string());
        }
    }

    /// The tool call `id` returned. A blocking subagent's report came back
    /// as its result: the turn owed for it is not.
    pub fn tool_result(&mut self, id: &str) {
        if self.owing.remove(id) {
            self.owed = self.owed.saturating_sub(1);
        }
    }

    /// The session is going away, and whatever its subagents were doing.
    pub fn clear_subagents(&mut self) {
        self.subagents.clear();
    }

    /// Who answers `req`: a rule, a handoff summary being written, or the
    /// user (`core::permissions::Router`). It was made while the agent
    /// planned if `planning`, or if it waited behind a dialog made so.
    /// Returns the route, and whether it was made while planning.
    pub fn route(
        &mut self,
        req: &PermissionRequest,
        cwd: &Path,
        planning: bool,
        handoff: Option<HandoffTo>,
    ) -> (Route, bool) {
        let planning = self.asked_planning.remove(&req.id) || planning;
        let router = Router {
            rules: &self.rules,
            cwd,
            planning,
            handoff,
        };
        (router.route(req), planning)
    }

    /// `req` waits behind another dialog for the user; it was made while
    /// the agent planned if `planning`.
    pub fn wait(&mut self, req: PermissionRequest, planning: bool) {
        if planning {
            self.asked_planning.insert(req.id.clone());
        }
        self.pending_prompts.push_back(req);
    }

    /// The oldest request waiting for the user.
    pub fn next_waiting(&mut self) -> Option<PermissionRequest> {
        self.pending_prompts.pop_front()
    }

    /// The harness stopped waiting for request `id`.
    pub fn withdraw(&mut self, id: &str) {
        self.pending_prompts.retain(|req| req.id != id);
        self.asked_planning.remove(id);
    }

    /// Nobody can answer the requests waiting: their session is gone.
    pub fn drop_waiting(&mut self) {
        self.pending_prompts.clear();
    }

    /// Deny or dismiss `req`, which only a user could answer, with nobody
    /// there to ask (`--print`): a tool call is denied, a plan is not
    /// approved (the agent ends the turn with it), and anything else is
    /// dismissed.
    pub fn nobody_to_ask(req: &PermissionRequest) -> (PermissionDecision, Unasked<'_>) {
        match &req.kind {
            PermissionKind::ToolUse { tool, action, .. } => (
                PermissionDecision::Deny {
                    reason: "Denied: unharness is running without a user to ask. \
                             Nothing that is not covered by an allow rule or the \
                             permission policy can run."
                        .into(),
                },
                Unasked::Denied { tool, action },
            ),
            // The agent can enter plan mode by itself (Claude's
            // `EnterPlanMode`); its plan is never approved here.
            PermissionKind::PlanApproval { .. } => (
                PermissionDecision::Deny {
                    reason: "unharness is running without a user to approve the plan. \
                             Do not carry it out; end the turn with the plan."
                        .into(),
                },
                Unasked::KeptPlanning,
            ),
            _ => (PermissionDecision::Answer(Value::Null), Unasked::Dismissed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Rule;
    use crate::core::permissions::Why;
    use crate::core::rules::Scope;

    fn shell(id: &str, command: &str) -> PermissionRequest {
        PermissionRequest {
            id: id.into(),
            kind: PermissionKind::ToolUse {
                tool: "Bash".into(),
                input: Value::Null,
                action: ToolAction::Shell {
                    command: command.into(),
                    cwd: None,
                },
                description: None,
            },
            tool_call_id: None,
        }
    }

    fn started(t: &mut Tracking, id: &str, unreported_end: bool) {
        t.subagent_started(id.into(), format!("task {id}"), None, unreported_end);
    }

    /// The session's id and model, its usage and whether it plans, as the
    /// main agent reports them.
    #[test]
    fn the_session_is_what_it_last_reported() {
        let mut t = Tracking::default();
        t.session_started("s1", Some("opus"));
        t.session_started("s2", None);
        assert_eq!(t.session_id.as_deref(), Some("s2"));
        assert_eq!(t.model.as_deref(), Some("opus"));

        let turn = |input| Usage {
            input,
            ..Default::default()
        };
        t.usage_reported(&turn(3));
        t.usage_reported(&turn(4));
        assert_eq!(t.usage.as_ref().unwrap().input, 7);
        t.usage_reported(&Usage {
            input: 100,
            cumulative: true,
            ..Default::default()
        });
        assert_eq!(t.usage.as_ref().unwrap().input, 100);

        t.policy_changed(PermissionPolicy::Plan);
        assert!(t.planning);
        t.policy_changed(PermissionPolicy::AcceptEdits);
        assert!(!t.planning);
    }

    /// A subagent is at work from its start to its end; one whose end may
    /// never be told holds nothing back. Its tool calls say what it does
    /// until it says so itself.
    #[test]
    fn subagents_at_work_and_what_they_do() {
        let mut t = Tracking::default();
        started(&mut t, "a", false);
        started(&mut t, "b", true);
        assert_eq!((t.awaited(), t.unawaited()), (1, 1));
        t.subagent_tool_started("a", "t1", "Read", &serde_json::json!({"file_path": "/x"}));
        assert_eq!(t.subagents[0].tools, 1);
        assert!(
            t.subagents[0]
                .activity
                .as_deref()
                .unwrap()
                .starts_with("Read")
        );
        t.subagent_progress("a", "thinking".into());
        t.subagent_tool_started("a", "t2", "Bash", &serde_json::json!({"command": "ls"}));
        assert_eq!(t.subagents[0].activity.as_deref(), Some("thinking"));
        assert_eq!(t.subagents[0].tools, 2);
        // Started again under the same call: anew.
        started(&mut t, "a", false);
        assert_eq!((t.subagents.len(), t.subagents[1].tools), (2, 0));
        t.subagent_ended("a", SubagentStatus::Completed);
        assert_eq!((t.awaited(), t.unawaited()), (0, 1));
        // Nothing owed where the harness reports no turn for it.
        assert_eq!(t.owed, 0);
        t.clear_subagents();
        assert!(t.subagents.is_empty());
    }

    /// Where the harness reports each subagent's end in a turn of its own,
    /// that turn is owed, unless the subagent was stopped, reported to its
    /// running parent, or blocked its call (whose result is the report).
    #[test]
    fn the_turns_owed_for_subagents_that_report() {
        let mut t = Tracking::default();
        t.report_turns(true);
        t.turn_expected();
        started(&mut t, "bg", false);
        t.turn_ended();
        assert_eq!(t.owed, 0);
        t.subagent_ended("bg", SubagentStatus::Completed);
        assert_eq!(t.owed, 1);
        // Its second end is no news.
        t.subagent_ended("bg", SubagentStatus::Completed);
        assert_eq!(t.owed, 1);
        t.turn_ended();
        assert_eq!(t.owed, 0);

        // Stopped by the agent: no turn of its own.
        started(&mut t, "stopped", false);
        t.subagent_ended("stopped", SubagentStatus::Cancelled);
        assert_eq!(t.owed, 0);

        // A subagent's subagent, ending while its parent runs, reports to
        // the parent; after the parent ended, in a turn of its own.
        started(&mut t, "parent", false);
        t.subagent_tool_started("parent", "child", "Agent", &Value::Null);
        started(&mut t, "child", false);
        t.subagent_ended("child", SubagentStatus::Completed);
        assert_eq!(t.owed, 0);
        started(&mut t, "child", false);
        t.subagent_ended("parent", SubagentStatus::Completed);
        t.subagent_ended("child", SubagentStatus::Failed);
        assert_eq!(t.owed, 2);

        // A blocking one: its call returns with the report.
        t.tool_result("child");
        assert_eq!(t.owed, 1);
        t.tool_result("child");
        assert_eq!(t.owed, 1);
    }

    /// A request made while planning is judged so, also when it waited
    /// behind a dialog until the plan was approved; a rule answers what it
    /// covers, and only a read while planning.
    #[test]
    fn requests_are_routed_and_wait_behind_a_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let mut rules = Rules::load_in(&dir.path().join("config"), Some(dir.path())).unwrap();
        rules
            .append(Scope::Workspace, &[Rule::shell("cargo test")])
            .unwrap();
        let mut t = Tracking::new(rules);
        let cwd = dir.path();
        let (route, planning) = t.route(&shell("r1", "cargo test"), cwd, false, None);
        assert!(!planning);
        assert!(matches!(
            route,
            Route::Answered {
                why: Why::Rules(_),
                ..
            }
        ));
        let (route, planning) = t.route(&shell("r2", "cargo test"), cwd, true, None);
        assert_eq!((route, planning), (Route::Ask, true));

        t.wait(shell("r2", "cargo test"), true);
        t.wait(shell("r3", "rm -rf x"), false);
        let next = t.next_waiting().unwrap();
        // After the plan's approval, still judged as made while planning.
        assert_eq!(t.route(&next, cwd, false, None), (Route::Ask, true));
        t.withdraw("r3");
        assert!(t.next_waiting().is_none());

        t.wait(shell("r4", "ls"), false);
        t.drop_waiting();
        assert!(t.pending_prompts.is_empty());
    }

    /// With nobody to ask, a tool call is denied, a plan is not approved,
    /// and anything else is dismissed.
    #[test]
    fn nobody_to_ask_denies_and_dismisses() {
        let req = shell("r1", "rm -rf x");
        let (decision, unasked) = Tracking::nobody_to_ask(&req);
        assert!(matches!(decision, PermissionDecision::Deny { reason }
            if reason == "Denied: unharness is running without a user to ask. Nothing that is not covered by an allow rule or the permission policy can run."));
        assert!(matches!(unasked, Unasked::Denied { tool: "Bash", .. }));

        let plan = PermissionRequest {
            id: "p".into(),
            kind: PermissionKind::PlanApproval {
                plan: "# Plan".into(),
                plan_file: None,
            },
            tool_call_id: None,
        };
        let (decision, unasked) = Tracking::nobody_to_ask(&plan);
        assert!(matches!(decision, PermissionDecision::Deny { reason }
            if reason == "unharness is running without a user to approve the plan. Do not carry it out; end the turn with the plan."));
        assert_eq!(unasked, Unasked::KeptPlanning);

        let question = PermissionRequest {
            id: "q".into(),
            kind: PermissionKind::Confirm {
                title: "Go on?".into(),
                message: None,
            },
            tool_call_id: None,
        };
        assert_eq!(
            Tracking::nobody_to_ask(&question),
            (PermissionDecision::Answer(Value::Null), Unasked::Dismissed)
        );
    }
}
