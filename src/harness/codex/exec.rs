//! Codex `exec --json` transport: one child per turn, resumed by thread id.

use anyhow::Result;
use tokio::process::Command;

use super::exec_parse::CodexExecParser;
use crate::core::per_turn::{PerTurnProtocol, TurnParser, TurnSpec, TurnState};
use crate::core::{AgentEvent, Attachment, HarnessId, PermissionPolicy};

pub struct CodexExec;

/// Flags for a fresh `codex exec` (not accepted by `exec resume`).
///
/// `confined` says unharness's sandbox is around the process. Codex's own
/// (bubblewrap) cannot start inside it, so it is switched off and ours is
/// the one that holds.
pub fn policy_args(policy: PermissionPolicy, confined: bool) -> Vec<&'static str> {
    if confined && policy != PermissionPolicy::Bypass {
        return vec!["-s", "danger-full-access"];
    }
    match policy {
        PermissionPolicy::Ask => vec!["-s", "read-only"],
        PermissionPolicy::AcceptEdits => vec!["-s", "workspace-write"],
        PermissionPolicy::Auto => vec!["--approve-for-me"],
        PermissionPolicy::Bypass => vec!["--dangerously-bypass-approvals-and-sandbox"],
    }
}

/// Equivalent `-c key=value` overrides, usable on `exec resume`.
pub fn policy_config_overrides(policy: PermissionPolicy, confined: bool) -> Vec<String> {
    if confined {
        return vec![
            "sandbox_mode=\"danger-full-access\"".into(),
            "approval_policy=\"never\"".into(),
        ];
    }
    match policy {
        PermissionPolicy::Ask => vec![
            "sandbox_mode=\"read-only\"".into(),
            "approval_policy=\"never\"".into(),
        ],
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => vec![
            "sandbox_mode=\"workspace-write\"".into(),
            "approval_policy=\"never\"".into(),
        ],
        PermissionPolicy::Bypass => vec![
            "sandbox_mode=\"danger-full-access\"".into(),
            "approval_policy=\"never\"".into(),
        ],
    }
}

/// `-i <FILE>...` accepts several values, so callers must put a flag after it.
fn image_args(command: &mut Command, attachments: &[Attachment]) {
    for a in attachments.iter().filter(|a| a.is_image()) {
        command.arg("-i").arg(a.path());
    }
}

impl PerTurnProtocol for CodexExec {
    fn harness(&self) -> HarnessId {
        HarnessId::CODEX
    }

    fn build_turn(
        &self,
        state: &TurnState,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<TurnSpec> {
        let mut command = Command::new(&state.binary);
        command.current_dir(&state.cwd);
        command.arg("exec");
        match &state.session_id {
            Some(id) => {
                command.arg("resume").arg(id);
                image_args(&mut command, attachments);
                command.arg("--json").arg("--skip-git-repo-check");
                for o in policy_config_overrides(state.policy, state.sandbox.is_active()) {
                    command.arg("-c").arg(o);
                }
                if state.policy == PermissionPolicy::Bypass {
                    command.arg("--dangerously-bypass-approvals-and-sandbox");
                }
            }
            None => {
                image_args(&mut command, attachments);
                command.arg("--json").arg("--skip-git-repo-check");
                command.arg("-C").arg(&state.cwd);
                command.args(policy_args(state.policy, state.sandbox.is_active()));
            }
        }
        if let Some(m) = &state.model {
            command.arg("-m").arg(&m.model);
        }
        if let Some(e) = &state.effort {
            command
                .arg("-c")
                .arg(format!("model_reasoning_effort=\"{e}\""));
        }
        let mcp = super::mcp_args(&state.mcp_servers);
        for o in &mcp.overrides {
            command.arg("-c").arg(o);
        }
        command.envs(mcp.env);
        command.args(&state.extra_args);
        command.arg("-");
        for (k, v) in &state.env {
            command.env(k, v);
        }
        Ok(TurnSpec {
            command,
            stdin: Some(text.to_string()),
        })
    }

    fn new_parser(&self) -> Box<dyn TurnParser> {
        Box::new(ExecTurnParser(CodexExecParser::new()))
    }
}

struct ExecTurnParser(CodexExecParser);

impl TurnParser for ExecTurnParser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        self.0.feed(line)
    }
    fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        self.0.feed_stderr(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ModelRef;
    use std::path::PathBuf;

    fn state(policy: PermissionPolicy, session_id: Option<&str>) -> TurnState {
        TurnState {
            binary: PathBuf::from("/bin/codex"),
            cwd: PathBuf::from("/work"),
            model: Some(ModelRef::new(HarnessId::CODEX, "openai", "gpt-5.5")),
            effort: Some("high".into()),
            policy,
            session_id: session_id.map(str::to_string),
            extra_args: vec![],
            env: vec![],
            mcp_servers: Vec::new(),
            sandbox: crate::core::Sandbox::off(),
            turn_index: 0,
        }
    }

    fn argv(spec: &TurnSpec) -> String {
        spec.command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn images_are_passed_before_a_flag() {
        let a = Attachment::image("/w/a.png").unwrap();
        let spec = CodexExec
            .build_turn(&state(PermissionPolicy::Auto, None), "p", &[a])
            .unwrap();
        let args: Vec<String> = spec
            .command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "/w/a.png");
        assert!(args[i + 2].starts_with("--"));
    }

    #[test]
    fn first_turn_uses_sandbox_flags() {
        let spec = CodexExec
            .build_turn(&state(PermissionPolicy::Auto, None), "p", &[])
            .unwrap();
        assert_eq!(
            argv(&spec),
            "exec --json --skip-git-repo-check -C /work --approve-for-me -m gpt-5.5 -c model_reasoning_effort=\"high\" -"
        );
        assert_eq!(spec.stdin.as_deref(), Some("p"));
    }

    #[test]
    fn own_sandbox_is_off_inside_ours() {
        for policy in [
            PermissionPolicy::Ask,
            PermissionPolicy::AcceptEdits,
            PermissionPolicy::Auto,
        ] {
            assert_eq!(policy_args(policy, true), ["-s", "danger-full-access"]);
            assert_eq!(
                policy_config_overrides(policy, true)[0],
                "sandbox_mode=\"danger-full-access\""
            );
        }
        assert_eq!(
            policy_args(PermissionPolicy::Bypass, true),
            policy_args(PermissionPolicy::Bypass, false)
        );
        assert_eq!(
            policy_args(PermissionPolicy::Ask, false),
            ["-s", "read-only"]
        );
    }

    #[test]
    fn resume_uses_config_overrides() {
        let spec = CodexExec
            .build_turn(&state(PermissionPolicy::Bypass, Some("t1")), "p", &[])
            .unwrap();
        let a = argv(&spec);
        assert!(a.starts_with("exec resume t1 --json --skip-git-repo-check -c sandbox_mode=\"danger-full-access\" -c approval_policy=\"never\" --dangerously-bypass-approvals-and-sandbox"));
        assert!(!a.contains("-s "));
        assert!(!a.contains("--effort"));
    }

    #[test]
    fn mcp_servers_are_passed_on_every_turn() {
        let servers = crate::core::testing::sample_mcp_servers();
        for session in [None, Some("t1")] {
            let mut s = state(PermissionPolicy::Auto, session);
            s.mcp_servers = servers.clone();
            let a = argv(&CodexExec.build_turn(&s, "p", &[]).unwrap());
            assert!(
                a.contains(" -c mcp_servers.files.command=\"/usr/bin/files-mcp\" -c ")
                    && a.contains(" -c mcp_servers.docs.url="),
                "{a}"
            );
        }
    }
}
