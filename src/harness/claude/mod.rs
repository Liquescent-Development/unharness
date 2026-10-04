//! Claude Code harness.

pub mod parse;
pub mod transport;

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde_json::Value;

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};
use crate::core::{
    Capabilities, HarnessId, ModelRef, PermissionPolicy, PolicySupport, ProviderId, RewindSupport,
    SessionConfig, SessionHandle,
};

pub struct ClaudeHarness;

pub static DESCRIPTOR: HarnessDescriptor = HarnessDescriptor {
    id: HarnessId::CLAUDE,
    display_name: "Claude Code (claude)",
    short_name: "Claude",
    binary_names: &["claude"],
    providers: ProviderSource::Static(&[("anthropic", "Anthropic")]),
};

pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Static catalog, checked 2026-10-02. Aliases resolve to the latest release.
const MODELS: &[(&str, &str, &str)] = &[
    ("opus", "Opus (latest)", "Alias for the latest Opus release"),
    (
        "sonnet",
        "Sonnet (latest)",
        "Alias for the latest Sonnet release",
    ),
    (
        "fable",
        "Fable (latest)",
        "Alias for the latest Fable release",
    ),
    (
        "haiku",
        "Haiku (latest)",
        "Alias for the latest Haiku release",
    ),
    (
        "claude-fable-5-1",
        "Claude Fable 5.1",
        "Most capable, deepest reasoning",
    ),
    (
        "claude-opus-5",
        "Claude Opus 5",
        "Deep reasoning, strongest coding",
    ),
    (
        "claude-sonnet-5",
        "Claude Sonnet 5",
        "Fast, capable workhorse",
    ),
    (
        "claude-haiku-4-5-20251001",
        "Claude Haiku 4.5",
        "Fastest, lowest cost",
    ),
];

impl Harness for ClaudeHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        &DESCRIPTOR
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming_input: true,
            text_deltas: true,
            thinking: true,
            tool_events: true,
            interactive_permissions: true,
            permission_policies: PermissionPolicy::ALL
                .iter()
                .copied()
                .map(PolicySupport::full)
                .collect(),
            effort_levels: EFFORT_LEVELS.iter().map(|s| s.to_string()).collect(),
            resume_by_id: true,
            live_model_list: false,
            multi_provider: false,
            ask_user_question: true,
            interrupt: true,
            usage_reporting: true,
            image_input: true,
            plan_updates: true,
            subagents: true,
            steer: true,
            compaction: true,
            context_usage: true,
            rate_limits: true,
            rewind: RewindSupport {
                conversation: true,
                anchors_survive_fork: false,
            },
            fork: true,
        }
    }

    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        let binary = resolve_binary(&DESCRIPTOR, binary_override);
        let version = binary.as_deref().and_then(probe_version);
        let auth = binary.as_deref().map(auth_status).unwrap_or_default();
        Probe {
            binary,
            version,
            auth,
        }
    }

    fn quick_auth(&self, binary: &Path) -> Option<bool> {
        Some(auth_status(binary).authenticated)
    }

    fn list_models(&self, _binary: &Path, provider: &ProviderId) -> Result<Vec<ModelInfo>> {
        if provider.as_str() != "anthropic" {
            return Ok(Vec::new());
        }
        Ok(MODELS
            .iter()
            .map(|(id, name, desc)| ModelInfo {
                model_ref: ModelRef::new(HarnessId::CLAUDE, "anthropic", *id),
                display_name: name.to_string(),
                description: Some(desc.to_string()),
                effort_levels: None,
            })
            .collect())
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        transport::start(cfg)
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<Command> {
        let mut cmd = Command::new(&cfg.binary);
        cmd.current_dir(&cfg.cwd);
        if let Some(policy) = cfg.policy {
            cmd.args(transport::policy_args(policy));
        }
        if let Some(m) = &cfg.model {
            cmd.arg("--model").arg(&m.model);
        }
        if let Some(e) = &cfg.effort {
            cmd.arg("--effort").arg(e);
        }
        if let Some(id) = &cfg.resume {
            cmd.arg("--resume").arg(id);
        }
        if let Some(fmt) = &cfg.format {
            cmd.arg("--output-format").arg(fmt);
        }
        cmd.args(&cfg.extra_args);
        if cfg.print_mode {
            cmd.arg("-p");
        }
        if let Some(p) = &cfg.prompt {
            cmd.arg(p);
        }
        Ok(cmd)
    }
}

fn auth_status(binary: &Path) -> AuthInfo {
    let Ok(out) = Command::new(binary).args(["auth", "status"]).output() else {
        return AuthInfo {
            authenticated: false,
            details: Some("could not run `claude auth status`".into()),
        };
    };
    let Ok(val) = serde_json::from_slice::<Value>(&out.stdout) else {
        return AuthInfo {
            authenticated: false,
            details: Some("Not authenticated with claude.ai".into()),
        };
    };
    let logged_in = val
        .get("loggedIn")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut details = Vec::new();
    if let Some(o) = val.get("orgName").and_then(Value::as_str) {
        details.push(format!("org: {o}"));
    }
    if let Some(p) = val.get("subscriptionType").and_then(Value::as_str) {
        details.push(format!("plan: {p}"));
    }
    if let Some(e) = val.get("email").and_then(Value::as_str) {
        details.push(format!("user: {e}"));
    }
    AuthInfo {
        authenticated: logged_in,
        details: (!details.is_empty()).then(|| details.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn print_command_shape() {
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/claude"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("run tests".into()),
            print_mode: true,
            model: Some(ModelRef::new(HarnessId::CLAUDE, "anthropic", "sonnet")),
            effort: Some("low".into()),
            policy: Some(PermissionPolicy::Bypass),
            format: Some("stream-json".into()),
            resume: None,
            extra_args: vec![],
        };
        let a = args(&ClaudeHarness.build_print_command(&cfg).unwrap());
        assert_eq!(a.last().unwrap(), "run tests");
        assert!(a.contains(&"-p".to_string()));
        assert!(a.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(a.windows(2).any(|w| w == ["--model", "sonnet"]));
        assert!(
            a.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
    }

    #[test]
    fn passthrough_has_no_print_flag() {
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/claude"),
            cwd: PathBuf::from("/tmp"),
            print_mode: false,
            ..Default::default()
        };
        let a = args(&ClaudeHarness.build_print_command(&cfg).unwrap());
        assert!(a.is_empty());
    }

    #[test]
    fn capabilities_and_models() {
        let caps = ClaudeHarness.capabilities();
        assert!(caps.interactive_permissions);
        assert_eq!(caps.permission_policies.len(), 4);
        assert!(caps.supports_effort("xhigh"));
        let models = ClaudeHarness
            .list_models(Path::new("claude"), &ProviderId::from("anthropic"))
            .unwrap();
        assert!(models.iter().any(|m| m.model_ref.model == "opus"));
        assert!(
            ClaudeHarness
                .list_models(Path::new("claude"), &ProviderId::from("openai"))
                .unwrap()
                .is_empty()
        );
    }
}
