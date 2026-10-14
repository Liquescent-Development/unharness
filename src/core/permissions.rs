//! Who answers a permission request that reaches unharness: an allow rule,
//! unharness itself while a handoff summary is written, or the user. The
//! TUI and `--print` both ask [`Router::route`], through their
//! `core::controller::Tracking`; the TUI opens a modal for [`Route::Ask`],
//! and `--print`, with nobody to ask, denies it (`Tracking::nobody_to_ask`,
//! said by `src/headless.rs`). What a transport answers by itself under the
//! policy (an ACP agent's edit inside the workspace under accept-edits,
//! `acp::transport::auto_allows`) never gets here.

use std::path::Path;

use serde_json::Value;

use super::event::{PermissionDecision, PermissionKind, PermissionRequest};
use super::rules::{Rule, Rules, rule_may_answer};

/// What a request is answered against.
#[derive(Debug, Clone, Copy)]
pub struct Router<'a> {
    /// The user's allow rules.
    pub rules: &'a Rules,
    /// The session's directory, which stands in for the workspace when
    /// there is none (`Rules::allows`).
    pub cwd: &'a Path,
    /// The agent plans, or did when it made the request: only a read is
    /// answered by a rule then (`rule_may_answer`).
    pub planning: bool,
    /// A handoff summary is being written, for whom.
    pub handoff: Option<HandoffTo>,
}

/// Whom a handoff summary is written for, which the agent is told when a
/// request of its is refused meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffTo {
    /// The agent the user is switching to.
    AnotherAgent,
    /// A new session of the same harness (another model or effort).
    NewSession,
}

/// How a request is answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Without asking anyone.
    Answered {
        decision: PermissionDecision,
        why: Why,
    },
    /// Only the user can answer it.
    Ask,
}

/// Why a request was answered without asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// The user's allow rules cover it: each as [`Rule::describe`] puts it.
    Rules(Vec<String>),
    /// A handoff summary is being written, which uses no tools and carries
    /// out no plan; a question is dismissed.
    Handoff,
}

impl Router<'_> {
    /// Who answers `req`. While a handoff summary is written unharness
    /// does, before any rule: nothing runs then. Otherwise a tool call the
    /// rules cover is allowed, and everything else is the user's.
    pub fn route(&self, req: &PermissionRequest) -> Route {
        if let Some(to) = self.handoff {
            return Route::Answered {
                decision: to.answer(&req.kind),
                why: Why::Handoff,
            };
        }
        match &req.kind {
            PermissionKind::ToolUse { tool, action, .. }
                if rule_may_answer(self.planning, action) =>
            {
                match self.rules.allows(tool, action, self.cwd) {
                    Some(rules) => Route::Answered {
                        decision: PermissionDecision::Allow {
                            updated_input: None,
                        },
                        why: Why::Rules(rules.into_iter().map(Rule::describe).collect()),
                    },
                    None => Route::Ask,
                }
            }
            // No rule answers a plan or a question.
            _ => Route::Ask,
        }
    }
}

impl HandoffTo {
    fn answer(self, kind: &PermissionKind) -> PermissionDecision {
        let moving = match self {
            HandoffTo::NewSession => "The user is moving this conversation to a new session.",
            HandoffTo::AnotherAgent => "The user is switching to another agent.",
        };
        match kind {
            PermissionKind::ToolUse { .. } => PermissionDecision::Deny {
                reason: format!(
                    "{moving} Write the handoff summary from what you already know, \
                     without tools."
                ),
            },
            // Never approved unseen.
            PermissionKind::PlanApproval { .. } => PermissionDecision::Deny {
                reason: format!("{moving} Do not carry out the plan; write the handoff summary."),
            },
            _ => PermissionDecision::Answer(Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::core::rules::{Scope, ToolAction};
    use crate::core::{Question, QuestionOption};

    fn tool_use(tool: &str, action: ToolAction) -> PermissionKind {
        PermissionKind::ToolUse {
            tool: tool.into(),
            input: Value::Null,
            action,
            description: None,
        }
    }

    fn shell(command: &str) -> PermissionKind {
        tool_use(
            "Bash",
            ToolAction::Shell {
                command: command.into(),
                cwd: None,
            },
        )
    }

    fn edit(path: PathBuf) -> PermissionKind {
        tool_use("Write", ToolAction::Edit { paths: vec![path] })
    }

    fn read(path: PathBuf) -> PermissionKind {
        tool_use("Read", ToolAction::Read { path })
    }

    fn plan() -> PermissionKind {
        PermissionKind::PlanApproval {
            plan: "# Plan".into(),
            plan_file: None,
        }
    }

    /// Every kind of request that is not a tool call.
    fn not_tool_calls() -> Vec<PermissionKind> {
        vec![
            PermissionKind::Question {
                questions: vec![Question {
                    id: "color".into(),
                    header: "Color".into(),
                    text: "Red or blue?".into(),
                    options: vec![QuestionOption::new("Red", "")],
                    allow_other: false,
                    multi: false,
                }],
            },
            PermissionKind::Confirm {
                title: "Go on?".into(),
                message: None,
            },
            PermissionKind::Select {
                title: "Which?".into(),
                options: vec!["a".into(), "b".into()],
            },
            PermissionKind::Input {
                title: "Name?".into(),
                placeholder: None,
                prefill: None,
                multiline: false,
            },
        ]
    }

    fn request(kind: PermissionKind) -> PermissionRequest {
        PermissionRequest {
            id: "r".into(),
            kind,
            tool_call_id: None,
        }
    }

    /// Rules for a shell command, every edit and read in the workspace, an
    /// MCP server and a tool by name. The workspace is `dir`, and
    /// unharness's config directory is `dir/config`, inside it.
    fn rules_in(dir: &Path) -> Rules {
        let mut rules = Rules::load_in(&dir.join("config"), Some(dir)).unwrap();
        rules
            .append(
                Scope::Workspace,
                &[
                    Rule::shell("cargo test"),
                    Rule::shell("git push"),
                    Rule::edit("**"),
                    Rule::read("**"),
                    Rule::mcp("docs/*"),
                    Rule::tool("WebFetch"),
                ],
            )
            .unwrap();
        rules
    }

    fn allowed(rules: &[&str]) -> Route {
        Route::Answered {
            decision: PermissionDecision::Allow {
                updated_input: None,
            },
            why: Why::Rules(rules.iter().map(|r| r.to_string()).collect()),
        }
    }

    /// Each kind of request, as it is answered with no handoff being
    /// written: (request, not planning, planning).
    #[test]
    fn a_rule_answers_what_it_covers_and_the_user_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let rules = rules_in(dir.path());
        let ws = dir.path();
        let own = ws.join("config").join("allow.toml");
        let cargo = "shell commands starting with `cargo test`";
        let push = "shell commands starting with `git push`";
        let mut table = vec![
            (shell("cargo test --all"), allowed(&[cargo]), Route::Ask),
            // Several commands, several rules: all of them are named.
            (
                shell("cargo test && git push"),
                allowed(&[cargo, push]),
                Route::Ask,
            ),
            // More than the rules cover.
            (shell("cargo test && rm -rf x"), Route::Ask, Route::Ask),
            (shell("rm -rf x"), Route::Ask, Route::Ask),
            (
                edit(ws.join("notes.txt")),
                allowed(&["edits to `**`"]),
                Route::Ask,
            ),
            (
                read(ws.join("notes.txt")),
                allowed(&["reads of `**`"]),
                allowed(&["reads of `**`"]),
            ),
            // No rule reaches unharness's own directories.
            (edit(own.clone()), Route::Ask, Route::Ask),
            (read(own), Route::Ask, Route::Ask),
            (
                tool_use(
                    "mcp__docs__search",
                    ToolAction::Mcp {
                        server: "docs".into(),
                        tool: "search".into(),
                    },
                ),
                allowed(&["every tool of the MCP server `docs`"]),
                Route::Ask,
            ),
            (
                tool_use("WebFetch", ToolAction::Other),
                allowed(&["every use of `WebFetch`"]),
                Route::Ask,
            ),
            (tool_use("Glob", ToolAction::Other), Route::Ask, Route::Ask),
            // What cannot be told with certainty is always asked about.
            (tool_use("Bash", ToolAction::Opaque), Route::Ask, Route::Ask),
            (
                tool_use("WebFetch", ToolAction::Opaque),
                Route::Ask,
                Route::Ask,
            ),
            (plan(), Route::Ask, Route::Ask),
        ];
        table.extend(
            not_tool_calls()
                .into_iter()
                .map(|kind| (kind, Route::Ask, Route::Ask)),
        );
        for (kind, idle, planning) in table {
            let req = request(kind);
            for (is_planning, want) in [(false, idle), (true, planning)] {
                let router = Router {
                    rules: &rules,
                    cwd: ws,
                    planning: is_planning,
                    handoff: None,
                };
                assert_eq!(
                    router.route(&req),
                    want,
                    "{:?} planning: {is_planning}",
                    req.kind
                );
            }
        }
    }

    /// While a handoff summary is written nothing runs, not even what a
    /// rule covers, no plan is approved and a question is dismissed.
    #[test]
    fn a_handoff_answers_every_request_itself() {
        let dir = tempfile::tempdir().unwrap();
        let rules = rules_in(dir.path());
        let ws = dir.path();
        let deny = |reason: &str| Route::Answered {
            decision: PermissionDecision::Deny {
                reason: reason.into(),
            },
            why: Why::Handoff,
        };
        let dismissed = Route::Answered {
            decision: PermissionDecision::Answer(Value::Null),
            why: Why::Handoff,
        };
        for (to, moving) in [
            (
                HandoffTo::AnotherAgent,
                "The user is switching to another agent.",
            ),
            (
                HandoffTo::NewSession,
                "The user is moving this conversation to a new session.",
            ),
        ] {
            let no_tools = deny(&format!(
                "{moving} Write the handoff summary from what you already know, without tools."
            ));
            let no_plan = deny(&format!(
                "{moving} Do not carry out the plan; write the handoff summary."
            ));
            let mut table = vec![
                (shell("cargo test"), no_tools.clone()),
                (shell("rm -rf x"), no_tools.clone()),
                (edit(ws.join("notes.txt")), no_tools.clone()),
                (read(ws.join("notes.txt")), no_tools.clone()),
                (tool_use("WebFetch", ToolAction::Other), no_tools.clone()),
                (tool_use("Bash", ToolAction::Opaque), no_tools.clone()),
                (plan(), no_plan),
            ];
            table.extend(
                not_tool_calls()
                    .into_iter()
                    .map(|kind| (kind, dismissed.clone())),
            );
            for (kind, want) in table {
                let req = request(kind);
                for planning in [false, true] {
                    let router = Router {
                        rules: &rules,
                        cwd: ws,
                        planning,
                        handoff: Some(to),
                    };
                    assert_eq!(
                        router.route(&req),
                        want,
                        "{:?} {to:?} planning: {planning}",
                        req.kind
                    );
                }
            }
        }
    }

    /// A command a rule covers, run outside the workspace, is another
    /// command.
    #[test]
    fn a_rule_answers_a_command_only_in_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let rules = rules_in(dir.path());
        let router = Router {
            rules: &rules,
            cwd: dir.path(),
            planning: false,
            handoff: None,
        };
        let in_dir = |cwd: &Path| {
            request(tool_use(
                "shell",
                ToolAction::Shell {
                    command: "cargo test".into(),
                    cwd: Some(cwd.to_path_buf()),
                },
            ))
        };
        assert_eq!(
            router.route(&in_dir(dir.path())),
            allowed(&["shell commands starting with `cargo test`"])
        );
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(router.route(&in_dir(elsewhere.path())), Route::Ask);
    }
}
