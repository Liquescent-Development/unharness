//! Codex `exec --json` transport: one child per turn, resumed by thread id.

use anyhow::{Result, bail};
use tokio::process::Command;

use super::OwnSandbox;
use super::exec_parse::CodexExecParser;
use crate::core::per_turn::{PerTurnProtocol, TurnParser, TurnSpec, TurnState};
use crate::core::sandbox::SandboxLevel;
use crate::core::{AgentEvent, Attachment, HarnessId, PermissionPolicy};

pub struct CodexExec;

/// Flags for a fresh `codex exec` (not accepted by `exec resume`).
///
/// `own` is what Codex's own sandbox holds to; the policy decides only the
/// approvals. `bypass` switches Codex's sandbox off only where there is
/// none to hold to. `auto`'s automatic review (`--approve-for-me`) exists
/// only with Codex's workspace-write sandbox: the CLI refuses it beside
/// `-s` (0.157.0), so at another level `auto` runs with that level and
/// Codex's default approvals, which is `accept-edits` there.
///
/// There is nothing for `ask`: `exec` cannot prompt, and the CLI takes no
/// `untrusted` approval policy (0.157.0), so it is not declared and never
/// resolved to.
pub fn policy_args(policy: PermissionPolicy, own: OwnSandbox) -> Result<Vec<&'static str>> {
    if policy == PermissionPolicy::Ask {
        bail!("codex exec cannot ask before acting");
    }
    Ok(match (policy, own) {
        _ if bypasses_sandbox(policy, own) => vec!["--dangerously-bypass-approvals-and-sandbox"],
        (PermissionPolicy::Auto, OwnSandbox::Level(SandboxLevel::WorkspaceWrite)) => {
            vec!["--approve-for-me"]
        }
        (PermissionPolicy::Bypass, _) => vec!["-s", own.mode(), "-c", "approval_policy=\"never\""],
        _ => vec!["-s", own.mode()],
    })
}

/// Equivalent `-c key=value` overrides, usable on `exec resume`.
pub fn policy_config_overrides(policy: PermissionPolicy, own: OwnSandbox) -> Result<Vec<String>> {
    if policy == PermissionPolicy::Ask {
        bail!("codex exec cannot ask before acting");
    }
    Ok(vec![
        format!("sandbox_mode=\"{}\"", own.mode()),
        "approval_policy=\"never\"".into(),
    ])
}

/// Whether `--dangerously-bypass-approvals-and-sandbox` is the form: the
/// policy is `bypass` and Codex's own sandbox has nothing to hold to.
pub fn bypasses_sandbox(policy: PermissionPolicy, own: OwnSandbox) -> bool {
    policy == PermissionPolicy::Bypass
        && matches!(
            own,
            OwnSandbox::External | OwnSandbox::Level(SandboxLevel::Off)
        )
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
                let own = OwnSandbox::for_session(&state.sandbox);
                for o in policy_config_overrides(state.policy, own)? {
                    command.arg("-c").arg(o);
                }
                if bypasses_sandbox(state.policy, own) {
                    command.arg("--dangerously-bypass-approvals-and-sandbox");
                }
            }
            None => {
                image_args(&mut command, attachments);
                command.arg("--json").arg("--skip-git-repo-check");
                command.arg("-C").arg(&state.cwd);
                command.args(policy_args(
                    state.policy,
                    OwnSandbox::for_session(&state.sandbox),
                )?);
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

    /// The sandbox was wanted at `level` and no backend could provide it.
    fn unconfined(level: SandboxLevel) -> crate::core::Sandbox {
        crate::core::Sandbox::Off {
            unavailable: Some("no kernel".into()),
            wanted: level,
        }
    }

    #[test]
    fn first_turn_uses_sandbox_flags() {
        let mut s = state(PermissionPolicy::Auto, None);
        s.sandbox = unconfined(SandboxLevel::WorkspaceWrite);
        let spec = CodexExec.build_turn(&s, "p", &[]).unwrap();
        assert_eq!(
            argv(&spec),
            "exec --json --skip-git-repo-check -C /work --approve-for-me -m gpt-5.5 -c model_reasoning_effort=\"high\" -"
        );
        assert_eq!(spec.stdin.as_deref(), Some("p"));
    }

    #[test]
    fn own_sandbox_is_off_inside_ours() {
        for policy in [PermissionPolicy::AcceptEdits, PermissionPolicy::Auto] {
            assert_eq!(
                policy_args(policy, OwnSandbox::External).unwrap(),
                ["-s", "danger-full-access"]
            );
            assert_eq!(
                policy_config_overrides(policy, OwnSandbox::External).unwrap()[0],
                "sandbox_mode=\"danger-full-access\""
            );
        }
        assert_eq!(
            policy_args(PermissionPolicy::Bypass, OwnSandbox::External).unwrap(),
            policy_args(
                PermissionPolicy::Bypass,
                OwnSandbox::Level(SandboxLevel::Off)
            )
            .unwrap()
        );
    }

    /// Where ours does not run, Codex's own sandbox holds to the level the
    /// user set, whatever the policy.
    #[test]
    fn own_sandbox_takes_the_level_not_the_policy() {
        use OwnSandbox::Level;
        use SandboxLevel::*;
        for policy in [PermissionPolicy::AcceptEdits, PermissionPolicy::Auto] {
            assert_eq!(
                policy_args(policy, Level(ReadOnly)).unwrap(),
                ["-s", "read-only"]
            );
            assert_eq!(
                policy_args(policy, Level(Off)).unwrap(),
                ["-s", "danger-full-access"]
            );
            assert_eq!(
                policy_config_overrides(policy, Level(ReadOnly)).unwrap(),
                ["sandbox_mode=\"read-only\"", "approval_policy=\"never\""]
            );
        }
        // Codex's automatic review comes only with its workspace-write sandbox.
        assert_eq!(
            policy_args(PermissionPolicy::AcceptEdits, Level(WorkspaceWrite)).unwrap(),
            ["-s", "workspace-write"]
        );
        assert_eq!(
            policy_args(PermissionPolicy::Auto, Level(WorkspaceWrite)).unwrap(),
            ["--approve-for-me"]
        );
        // `bypass` does not widen the sandbox.
        assert_eq!(
            policy_args(PermissionPolicy::Bypass, Level(ReadOnly)).unwrap(),
            ["-s", "read-only", "-c", "approval_policy=\"never\""]
        );
        assert!(!bypasses_sandbox(
            PermissionPolicy::Bypass,
            Level(WorkspaceWrite)
        ));
        assert!(bypasses_sandbox(PermissionPolicy::Bypass, Level(Off)));
        assert!(bypasses_sandbox(
            PermissionPolicy::Bypass,
            OwnSandbox::External
        ));
        assert!(!bypasses_sandbox(PermissionPolicy::Auto, Level(Off)));

        let mut s = state(PermissionPolicy::Bypass, Some("t1"));
        s.sandbox = unconfined(ReadOnly);
        let a = argv(&CodexExec.build_turn(&s, "p", &[]).unwrap());
        assert!(
            a.contains("-c sandbox_mode=\"read-only\" -c approval_policy=\"never\"")
                && !a.contains("--dangerously-bypass"),
            "{a}"
        );
    }

    #[test]
    fn ask_is_refused() {
        for own in [
            OwnSandbox::External,
            OwnSandbox::Level(SandboxLevel::WorkspaceWrite),
        ] {
            assert!(policy_args(PermissionPolicy::Ask, own).is_err());
            assert!(policy_config_overrides(PermissionPolicy::Ask, own).is_err());
        }
        for session in [None, Some("t1")] {
            assert!(
                CodexExec
                    .build_turn(&state(PermissionPolicy::Ask, session), "p", &[])
                    .is_err()
            );
        }
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
