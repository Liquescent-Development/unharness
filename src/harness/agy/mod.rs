//! Antigravity (`agy`) harness. Best effort: flag parsing and the `result`
//! event were verified against agy 1.2.15 without an account; the live
//! `init`/`step_update` stream has not been recorded yet.

pub mod parse;
pub mod transport;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};
use crate::core::sandbox::SandboxPaths;
use crate::core::{
    Capabilities, HarnessId, McpSupport, ModelRef, PermissionPolicy, PolicySupport, ProviderId,
    RewindSupport, SessionConfig, SessionHandle, SubagentSupport,
};
pub use transport::AgyTransport;

pub struct AgyHarness {
    pub transport: AgyTransport,
}

impl Default for AgyHarness {
    fn default() -> Self {
        AgyHarness {
            transport: AgyTransport::Stream,
        }
    }
}

impl AgyHarness {
    pub fn new(transport: AgyTransport) -> Self {
        AgyHarness { transport }
    }
}

pub static DESCRIPTOR: HarnessDescriptor = HarnessDescriptor {
    id: HarnessId::AGY,
    display_name: "Antigravity (agy)",
    short_name: "Antigravity",
    binary_names: &["agy"],
    providers: ProviderSource::Static(&[("google", "Google")]),
};

pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high"];

/// Static fallback, from the v1 adapter (unverified against a live account).
const FALLBACK_MODELS: &[(&str, &str, &str)] = &[
    (
        "gemini-3.8-flash-high",
        "Gemini 3.8 Flash (High)",
        "Fast, high reasoning effort",
    ),
    (
        "gemini-3.8-flash-medium",
        "Gemini 3.8 Flash (Medium)",
        "Fast, balanced reasoning",
    ),
    (
        "gemini-3.7-flash-high",
        "Gemini 3.7 Flash (High)",
        "Gemini 3.7 high reasoning tier",
    ),
    (
        "gemini-3.1-pro-high",
        "Gemini 3.1 Pro (High)",
        "Pro model with high reasoning",
    ),
];

const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

impl Harness for AgyHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        &DESCRIPTOR
    }

    /// Seen on agy 1.2.15 without an account: `~/.gemini/antigravity-cli`.
    fn sandbox_paths(&self) -> SandboxPaths {
        SandboxPaths {
            writable: vec![PathBuf::from("~/.gemini")],
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming_input: self.transport != AgyTransport::PerTurn,
            text_deltas: true,
            thinking: true,
            tool_events: true,
            interactive_permissions: false,
            permission_policies: vec![
                PolicySupport::degraded(
                    PermissionPolicy::Ask,
                    "headless agy soft-denies tools that would prompt",
                ),
                PolicySupport::full(PermissionPolicy::AcceptEdits),
                PolicySupport::full(PermissionPolicy::Bypass),
            ],
            effort_levels: EFFORT_LEVELS.iter().map(|s| s.to_string()).collect(),
            resume_by_id: true,
            live_model_list: true,
            multi_provider: false,
            ask_user_question: false,
            interrupt: true,
            usage_reporting: true,
            image_input: false,
            file_input: false,
            plan_updates: false,
            subagents: SubagentSupport::default(),
            steer: false,
            compaction: false,
            context_usage: false,
            rate_limits: false,
            rewind: RewindSupport::default(),
            fork: false,
            // Servers are only read from agy's own config (`agy mcp add`
            // writes it); there is no flag for one session.
            mcp: McpSupport::NONE,
            // `--mode plan`, from `--help` on 1.2.16.
            plan_mode: true,
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

    fn list_models(&self, binary: &Path, provider: &ProviderId) -> Result<Vec<ModelInfo>> {
        if provider.as_str() != "google" {
            return Ok(Vec::new());
        }
        let live = query_models(binary).unwrap_or_default();
        if !live.is_empty() {
            return Ok(live
                .iter()
                .filter_map(|m| {
                    let id = m
                        .get("id")
                        .or_else(|| m.get("name"))
                        .or_else(|| m.get("model"))
                        .and_then(Value::as_str)?
                        .to_string();
                    Some(ModelInfo {
                        model_ref: ModelRef::new(HarnessId::AGY, "google", id.clone()),
                        display_name: m
                            .get("display_name")
                            .or_else(|| m.get("displayName"))
                            .and_then(Value::as_str)
                            .unwrap_or(&id)
                            .to_string(),
                        description: m
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        effort_levels: None,
                    })
                })
                .collect());
        }
        Ok(FALLBACK_MODELS
            .iter()
            .map(|(id, name, desc)| ModelInfo {
                model_ref: ModelRef::new(HarnessId::AGY, "google", *id),
                display_name: name.to_string(),
                description: Some(desc.to_string()),
                effort_levels: None,
            })
            .collect())
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        match self.transport {
            AgyTransport::PerTurn => {
                crate::core::per_turn::start(cfg, Arc::new(transport::AgyPerTurn))
            }
            t => transport::start_stream(cfg, t),
        }
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<Command> {
        let mut cmd = Command::new(&cfg.binary);
        cmd.current_dir(&cfg.cwd);
        if let Some(p) = cfg.policy {
            cmd.args(transport::policy_args(p));
        }
        if let Some(m) = &cfg.model {
            cmd.arg("--model").arg(&m.model);
        }
        if let Some(e) = &cfg.effort {
            cmd.arg("--effort").arg(e);
        }
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

/// Best-effort without spawning: agy keeps its login state next to its
/// settings. A `GEMINI_API_KEY` also works for headless runs.
fn auth_status() -> AuthInfo {
    if std::env::var("GEMINI_API_KEY").is_ok_and(|k| !k.is_empty()) {
        return AuthInfo {
            authenticated: true,
            details: Some("GEMINI_API_KEY set".into()),
        };
    }
    let Some(home) = dirs::home_dir() else {
        return AuthInfo::default();
    };
    let dir = home.join(".gemini").join("antigravity-cli");
    let settings = std::fs::read_to_string(dir.join("settings.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok());
    if let Some(s) = &settings {
        if let Some(project) = s.pointer("/gcp/project").and_then(Value::as_str) {
            return AuthInfo {
                authenticated: true,
                details: Some(format!("GCP project: {project}")),
            };
        }
        if s.get("account").is_some() || s.get("auth").is_some() || s.get("user").is_some() {
            return AuthInfo {
                authenticated: true,
                details: Some("account configured".into()),
            };
        }
    }
    AuthInfo {
        authenticated: false,
        details: Some("not logged in (run `agy` to log in)".into()),
    }
}

/// `agy --output-format json models`, bounded so an OAuth prompt cannot hang us.
pub fn query_models(binary: &Path) -> Result<Vec<Value>> {
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
                let v: Value = serde_json::from_str(text.trim())
                    .with_context(|| "agy models did not return JSON")?;
                let list = v
                    .as_array()
                    .cloned()
                    .or_else(|| v.get("models").and_then(Value::as_array).cloned())
                    .unwrap_or_default();
                return Ok(list);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
            effort: Some("low".into()),
            policy: Some(PermissionPolicy::Bypass),
            format: Some("stream-json".into()),
            resume: Some("c1".into()),
            extra_args: vec![],
            sandbox: crate::core::Sandbox::off(),
        };
        let h = AgyHarness::default();
        assert_eq!(
            args(&h.build_print_command(&base).unwrap()),
            "--dangerously-skip-permissions --model gemini-x --effort low --conversation c1 --output-format stream-json --print=fix it"
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
        let c = AgyHarness::default().capabilities();
        assert!(!c.interactive_permissions && c.streaming_input && c.resume_by_id);
        assert!(c.supports_policy(PermissionPolicy::Auto).is_none());
        assert!(
            !AgyHarness::new(AgyTransport::PerTurn)
                .capabilities()
                .streaming_input
        );
    }
}
