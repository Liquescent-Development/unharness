//! Mistral Vibe's subagents, read from its own fields of a tool call
//! (`_meta`), which the protocol does not define.
//!
//! Recorded on vibe-acp 2.26.1 (`acp/fixtures/vibe_acp_subagent*.jsonl`).
//! The model starts one with its runtime's `subagent.spawn` and may wait
//! for it with `subagent.wait`; the child runs in the background either
//! way. vibe sends the spawn as one call, `_meta.tool_name:
//! "subagent.spawn"`, titled `Starting <name>` (the name the model gave
//! it, `_effect_detail` in `mistralai_vibe_local_harness/vibe/
//! _projection.py`) with the profile in `agent` and the task in `task`,
//! and completes it with `_meta.child_session_id` once the child exists.
//! A wait is a call `subagent.wait` with `rawInput.agentName`, completed
//! with the child's answer in `rawOutput.structured_content.value` (`type:
//! "success"`). That is all the client hears of a child: its own tool
//! calls are never sent, and its end only when a wait for it completes.

use std::collections::HashMap;

use serde_json::Value;

use crate::core::{AgentEvent, SubagentStatus};
use crate::harness::acp::parse::{MetaReader, ToolView};

const SPAWN: &str = "subagent.spawn";
const WAIT: &str = "subagent.wait";
/// How vibe titles a spawn: `Starting <name>`.
const TITLE: &str = "Starting ";

/// What a subagent's row says while it runs, since nothing else comes.
pub const UNSEEN_END: &str = "its end shows only when the agent waits for it";

/// The subagents of one session.
#[derive(Debug, Default)]
pub struct Subagents {
    /// The call that spawned each subagent, by the name the model gave it.
    by_name: HashMap<String, String>,
    /// Each announced subagent's last report, `None` while it has none.
    reports: HashMap<String, Option<String>>,
}

/// A reader for a new session (`AgentHooks::meta_reader`).
pub fn reader() -> Box<dyn MetaReader> {
    Box::<Subagents>::default()
}

impl MetaReader for Subagents {
    fn tool_update(&mut self, call: ToolView<'_>, update: &Value) -> Vec<AgentEvent> {
        let completed = update.get("status").and_then(Value::as_str) == Some("completed");
        let meta = update.get("_meta").unwrap_or(&Value::Null);
        match meta.get("tool_name").and_then(Value::as_str) {
            Some(SPAWN) if completed => self.spawned(call, meta),
            Some(WAIT) if completed => self.waited(call, update),
            _ => Vec::new(),
        }
    }
}

impl Subagents {
    /// A spawn that made a child. One that did not (a second spawn of a
    /// name, which vibe 2.26.1 completed as `Started <name>` without a
    /// child) is only a call.
    fn spawned(&mut self, call: ToolView<'_>, meta: &Value) -> Vec<AgentEvent> {
        let field = |key: &str| {
            meta.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        if field("child_session_id").is_none() || self.reports.contains_key(call.id) {
            return Vec::new();
        }
        let name = call
            .title
            .strip_prefix(TITLE)
            .map(str::trim)
            .filter(|n| !n.is_empty());
        if let Some(name) = name {
            self.by_name.insert(name.to_string(), call.id.to_string());
        }
        self.reports.insert(call.id.to_string(), None);
        let task = field("task");
        let mut out = vec![
            AgentEvent::SubagentStarted {
                id: call.id.to_string(),
                description: name.or(task).unwrap_or("subagent").to_string(),
                kind: field("agent").map(str::to_string),
            },
            AgentEvent::SubagentProgress {
                id: call.id.to_string(),
                activity: UNSEEN_END.to_string(),
            },
        ];
        if let Some(task) = task {
            out.push(AgentEvent::Sub {
                parent: call.id.to_string(),
                event: Box::new(AgentEvent::Notice(format!("task: {task}"))),
            });
        }
        out
    }

    /// A wait that came back with the child's answer ends it. One that
    /// failed (vibe's `subagent_wait_timeout`, an interrupt, a child's
    /// failed turn: read in `_subagents/_controller.py`, not recorded but
    /// for the interrupt) ends nothing: the child may run on.
    fn waited(&mut self, call: ToolView<'_>, update: &Value) -> Vec<AgentEvent> {
        let Some(id) = call
            .input
            .get("agentName")
            .and_then(Value::as_str)
            .and_then(|name| self.by_name.get(name.trim()))
            .cloned()
        else {
            return Vec::new();
        };
        let Some(result) = update.pointer("/rawOutput/structured_content") else {
            return Vec::new();
        };
        let report = match (
            result.get("type").and_then(Value::as_str),
            result.get("value").and_then(Value::as_str),
        ) {
            (Some("success"), Some(value)) => value.to_string(),
            _ => return Vec::new(),
        };
        // Waited for again, it answers the same; once sent more work
        // (`subagent.send_message`, not recorded) it has a new report.
        let last = self.reports.entry(id.clone()).or_default();
        if last.as_deref() == Some(report.as_str()) {
            return Vec::new();
        }
        *last = Some(report.clone());
        vec![AgentEvent::SubagentEnded {
            id,
            status: SubagentStatus::Completed,
            result: Some(report),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(id: &str, status: &str, meta: Value, extra: Value) -> Value {
        let mut u = json!({"toolCallId": id, "status": status, "_meta": meta});
        for (k, v) in extra.as_object().unwrap() {
            u[k] = v.clone();
        }
        u
    }

    fn feed(s: &mut Subagents, id: &str, title: &str, input: Value, u: Value) -> Vec<String> {
        let call = ToolView {
            id,
            title,
            input: &input,
        };
        s.tool_update(call, &u)
            .iter()
            .map(|e| e.summary())
            .collect()
    }

    fn spawn_meta(child: Option<&str>) -> Value {
        let mut m = json!({"tool_name": "subagent.spawn", "agent": "generic", "task": "list"});
        if let Some(c) = child {
            m["child_session_id"] = json!(c);
        }
        m
    }

    #[test]
    fn a_spawn_starts_once_it_has_a_child_and_a_wait_ends_it() {
        let mut s = Subagents::default();
        let title = "Starting lister";
        // Running: no child yet.
        assert!(
            feed(
                &mut s,
                "e1",
                title,
                json!({}),
                update("e1", "in_progress", spawn_meta(None), json!({}))
            )
            .is_empty()
        );
        let started = feed(
            &mut s,
            "e1",
            title,
            json!({}),
            update("e1", "completed", spawn_meta(Some("child-1")), json!({})),
        );
        assert_eq!(
            started,
            [
                r#"SubagentStarted id=e1 kind=generic "lister""#.to_string(),
                format!("SubagentProgress id=e1 {UNSEEN_END:?}"),
                r#"Sub parent=e1 Notice "task: list""#.to_string(),
            ]
        );
        // The same update again starts nothing.
        assert!(
            feed(
                &mut s,
                "e1",
                title,
                json!({}),
                update("e1", "completed", spawn_meta(Some("child-1")), json!({}))
            )
            .is_empty()
        );

        let wait = |status: &str, output: Value| {
            update(
                "w1",
                status,
                json!({"tool_name": "subagent.wait"}),
                json!({"rawOutput": output}),
            )
        };
        let input = json!({"agentName": "lister", "timeoutMs": 1000});
        let timeout = json!({"type": "success", "structured_content":
            {"type": "error", "error": "subagent_wait_timeout: Timed out waiting for lister"}});
        assert!(feed(&mut s, "w1", "", input.clone(), wait("completed", timeout)).is_empty());
        assert!(
            feed(
                &mut s,
                "w1",
                "",
                input.clone(),
                wait("failed", json!("client interrupt"))
            )
            .is_empty()
        );
        let done =
            json!({"type": "success", "structured_content": {"type": "success", "value": "a, b"}});
        assert_eq!(
            feed(
                &mut s,
                "w2",
                "",
                input.clone(),
                wait("completed", done.clone())
            ),
            [r#"SubagentEnded id=e1 completed "a, b""#]
        );
        // Waited for again: the same answer is no news.
        assert!(feed(&mut s, "w3", "", input, wait("completed", done)).is_empty());
        // A wait for a name never spawned here.
        let other = json!({"agentName": "nobody"});
        let done = json!({"structured_content": {"type": "success", "value": "x"}});
        assert!(feed(&mut s, "w4", "", other, wait("completed", done)).is_empty());
    }

    #[test]
    fn a_spawn_without_a_child_or_a_name() {
        let mut s = Subagents::default();
        // A second spawn of a name, completed without a child.
        assert!(
            feed(
                &mut s,
                "e2",
                "Starting lister",
                json!({}),
                update("e2", "completed", spawn_meta(None), json!({}))
            )
            .is_empty()
        );
        // A title not in vibe's shape: the task stands for the name.
        let out = feed(
            &mut s,
            "e3",
            "something else",
            json!({}),
            update("e3", "completed", spawn_meta(Some("child-3")), json!({})),
        );
        assert_eq!(out[0], r#"SubagentStarted id=e3 kind=generic "list""#);
    }
}
