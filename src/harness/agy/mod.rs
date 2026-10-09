//! Antigravity (`agy`) harness, checked against agy 1.2.17 with an account
//! (see the status notes in `AGENTS.md`).

mod brain;
pub mod parse;
pub mod transport;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};
use crate::core::guard::Guarded;
use crate::core::sandbox::{Sandbox, SandboxPaths};
use crate::core::{
    Capabilities, HarnessId, McpSupport, ModelRef, PermissionPolicy, PolicySupport, ProviderId,
    RewindSupport, SessionConfig, SessionHandle, SubagentSupport,
};

#[derive(Default)]
pub struct AgyHarness;

pub static DESCRIPTOR: HarnessDescriptor = HarnessDescriptor {
    id: HarnessId::AGY,
    display_name: "Antigravity (agy)",
    short_name: "Antigravity",
    binary_names: &["agy"],
    providers: ProviderSource::Static(&[("google", "Google")]),
};

/// What `agy models` listed on 2026-10-05 (1.2.17), for when it cannot be
/// asked.
const FALLBACK_MODELS: &[(&str, &str)] = &[
    ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
    ("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
    ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
    ("gemini-3.7-flash-high", "Gemini 3.7 Flash (High)"),
    ("gemini-3.7-flash-medium", "Gemini 3.7 Flash (Medium)"),
    ("gemini-3.7-flash-low", "Gemini 3.7 Flash (Low)"),
    ("gemini-3.6-flash-high", "Gemini 3.6 Flash (High)"),
    ("gemini-3.6-flash-medium", "Gemini 3.6 Flash (Medium)"),
    ("gemini-3.6-flash-low", "Gemini 3.6 Flash (Low)"),
    ("gemini-3.1-pro-high", "Gemini 3.1 Pro (High)"),
    ("gemini-3.1-pro-low", "Gemini 3.1 Pro (Low)"),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6 (Thinking)"),
    ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

impl Harness for AgyHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        &DESCRIPTOR
    }

    /// Headless runs left all three as they were; agy's own interface
    /// adds the workspace to `trustedWorkspaces` in `settings.json` when the
    /// user trusts it there.
    fn guarded(&self, _workspace: &Path) -> Vec<Guarded> {
        let state = PathBuf::from("~/.gemini");
        vec![
            Guarded::File(state.join("antigravity-cli/settings.json")),
            Guarded::File(state.join("config/config.json")),
            Guarded::File(state.join("config/mcp_config.json")),
        ]
    }

    /// Conversations, plans (`brain/`), logs and the updater's files are all
    /// under this directory; `~/.gemini/config` is only read.
    fn sandbox_paths(&self) -> SandboxPaths {
        SandboxPaths {
            writable: vec![PathBuf::from("~/.gemini/antigravity-cli")],
        }
    }

    /// agy's own interface asks before a write and before a command (seen
    /// on 1.2.17: nothing happened until answered); `--print` is headless
    /// like a session.
    fn print_policies(&self, print_mode: bool) -> Vec<PolicySupport> {
        let mut policies = self.capabilities().permission_policies;
        if !print_mode {
            policies.insert(0, PolicySupport::full(PermissionPolicy::Ask));
        }
        policies
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming_input: true,
            text_deltas: true,
            // Counted in the usage, never sent.
            thinking: false,
            tool_events: true,
            interactive_permissions: false,
            // No `ask`: headless agy cannot prompt, and refuses by itself
            // what it would have asked about.
            permission_policies: vec![
                PolicySupport::full(PermissionPolicy::AcceptEdits),
                PolicySupport::full(PermissionPolicy::Bypass),
            ],
            // The effort is the end of the model id, and agy refuses
            // `--effort` beside `--model`.
            effort_levels: Vec::new(),
            resume_by_id: true,
            live_model_list: true,
            multi_provider: false,
            provider_per_process: false,
            ask_user_question: false,
            interrupt: true,
            usage_reporting: true,
            image_input: false,
            file_input: false,
            plan_updates: false,
            // From its stream and its files (`brain.rs`, agy 1.3.1). No
            // event on stdin stops one, and the turn waits for them, so
            // none reports between turns.
            subagents: SubagentSupport {
                reported: true,
                described: false,
                stop: false,
                report_turn: false,
            },
            steer: false,
            compaction: false,
            context_usage: false,
            rate_limits: false,
            rewind: RewindSupport::default(),
            fork: false,
            // Servers are only read from agy's own config (`agy mcp add`
            // writes it); there is no flag for one session.
            mcp: McpSupport::NONE,
            // `--mode plan` makes agy write a plan file under its state
            // directory and then act in the same turn: nothing waits.
            plan_mode: false,
            // Not seen to read a `/name` prompt as a command (unverified),
            // and it lists none.
            slash_commands: false,
            // 1.3.2 writes a conversation for every process as it starts
            // (`init` comes then, not after the first message): one
            // started early would leave an empty one behind.
            start_unprompted: false,
        }
    }

    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        let binary = resolve_binary(&DESCRIPTOR, binary_override);
        let version = binary.as_deref().and_then(probe_version);
        Probe {
            binary,
            version,
            auth: auth_status(),
        }
    }

    fn quick_auth(&self, _binary: &Path) -> Option<bool> {
        Some(auth_status().authenticated)
    }

    fn list_models(
        &self,
        binary: &Path,
        provider: &ProviderId,
        _sandbox: &Sandbox,
    ) -> Result<Vec<ModelInfo>> {
        if provider.as_str() != "google" {
            return Ok(Vec::new());
        }
        let mut models = query_models(binary).unwrap_or_default();
        if models.is_empty() {
            models = FALLBACK_MODELS
                .iter()
                .map(|(id, label)| (id.to_string(), label.to_string()))
                .collect();
        }
        Ok(models
            .into_iter()
            .map(|(id, label)| ModelInfo {
                model_ref: ModelRef::new(HarnessId::AGY, "google", id),
                display_name: label,
                description: None,
                effort_levels: None,
            })
            .collect())
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        transport::start_stream(cfg)
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<Command> {
        let mut cmd = Command::new(&cfg.binary);
        cmd.current_dir(&cfg.cwd);
        if let Some(p) = cfg.policy {
            cmd.args(transport::policy_args(p));
        }
        cmd.args(transport::model_args(
            cfg.model.as_ref().map(|m| m.model.as_str()),
        ));
        if let Some(id) = &cfg.resume {
            cmd.arg("--conversation").arg(id);
        }
        if let Some(fmt) = &cfg.format {
            cmd.arg("--output-format").arg(fmt);
        }
        cmd.args(&cfg.extra_args);
        match (&cfg.prompt, cfg.print_mode) {
            (Some(p), true) => {
                cmd.arg(format!("--print={p}"));
            }
            (Some(p), false) => {
                cmd.arg("--prompt-interactive").arg(p);
            }
            (None, true) => {
                cmd.arg("--print=");
            }
            (None, false) => {}
        }
        Ok(cmd)
    }
}

/// Without spawning: signing in leaves a token file beside the settings.
/// Only its presence is looked at.
fn auth_status() -> AuthInfo {
    let token = dirs::home_dir()
        .map(|home| home.join(".gemini/antigravity-cli/antigravity-oauth-token"))
        .filter(|path| path.metadata().is_ok_and(|m| m.len() > 0));
    match token {
        Some(_) => AuthInfo {
            authenticated: true,
            details: Some("signed in".into()),
        },
        None => AuthInfo {
            authenticated: false,
            details: Some("not logged in (run `agy` to log in)".into()),
        },
    }
}

/// `agy --output-format json models` as `(id, label)`, bounded so an OAuth
/// prompt cannot hang us.
pub fn query_models(binary: &Path) -> Result<Vec<(String, String)>> {
    let mut child = Command::new(binary)
        .args(["--output-format", "json", "models"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn agy")?;
    let mut stdout = child.stdout.take().context("stdout")?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut s = String::new();
        let _ = Read::by_ref(&mut stdout)
            .take(1 << 20)
            .read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + QUERY_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let text = reader.join().unwrap_or_default();
                if !status.success() {
                    anyhow::bail!("agy models exited with {status}");
                }
                return parse_models(&text);
            }
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("agy models timed out (not logged in?)");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// One object on stdout (a progress line goes to stderr), with the list at
/// `command.data.models`.
fn parse_models(stdout: &str) -> Result<Vec<(String, String)>> {
    let v: Value = serde_json::from_str(stdout.trim()).context("agy models did not return JSON")?;
    let models = v
        .pointer("/command/data/models")
        .and_then(Value::as_array)
        .context("agy models: no command.data.models")?;
    Ok(models
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            let label = m.get("label").and_then(Value::as_str).unwrap_or(id);
            Some((id.to_string(), label.to_string()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn models_output() {
        // Cut down from agy 1.2.17.
        let out = "{\"conversation_id\":\"\",\"status\":\"SUCCESS\",\"response\":\"gemini-3.8-flash-high\\tGemini 3.8 Flash (High)\\n\",\"duration_seconds\":0,\"num_turns\":0,\"command\":{\"name\":\"models\",\"data\":{\"models\":[{\"id\":\"gemini-3.8-flash-high\",\"label\":\"Gemini 3.8 Flash (High)\"},{\"id\":\"claude-sonnet-4-6\",\"label\":\"Claude Sonnet 4.6 (Thinking)\"}]}}}\n";
        assert_eq!(
            parse_models(out).unwrap(),
            vec![
                (
                    "gemini-3.8-flash-high".to_string(),
                    "Gemini 3.8 Flash (High)".to_string()
                ),
                (
                    "claude-sonnet-4-6".to_string(),
                    "Claude Sonnet 4.6 (Thinking)".to_string()
                ),
            ]
        );
        assert!(parse_models("").is_err());
    }

    fn args(cmd: &Command) -> String {
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn print_and_passthrough_commands() {
        let base = PrintConfig {
            binary: PathBuf::from("/bin/agy"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("fix it".into()),
            print_mode: true,
            model: Some(ModelRef::new(HarnessId::AGY, "google", "gemini-x")),
            provider: None,
            effort: Some("low".into()),
            policy: Some(PermissionPolicy::Bypass),
            format: Some("stream-json".into()),
            resume: Some("c1".into()),
            extra_args: vec![],
            mcp_servers: Vec::new(),
            sandbox: crate::core::Sandbox::off(),
        };
        let h = AgyHarness;
        assert_eq!(
            args(&h.build_print_command(&base).unwrap()),
            "--dangerously-skip-permissions --model gemini-x --conversation c1 --output-format stream-json --print=fix it"
        );
        let tui = PrintConfig {
            print_mode: false,
            format: None,
            resume: None,
            ..base
        };
        assert!(
            args(&h.build_print_command(&tui).unwrap()).ends_with("--prompt-interactive fix it")
        );
    }

    #[test]
    fn capabilities() {
        let c = AgyHarness.capabilities();
        assert!(!c.interactive_permissions && c.streaming_input && c.resume_by_id);
        assert!(c.supports_policy(PermissionPolicy::Auto).is_none());
        assert!(c.supports_policy(PermissionPolicy::Ask).is_none());
        let h = AgyHarness;
        let asks = |print_mode| {
            h.print_policies(print_mode)
                .iter()
                .any(|p| p.policy == PermissionPolicy::Ask)
        };
        assert!(asks(false) && !asks(true));
    }
}
