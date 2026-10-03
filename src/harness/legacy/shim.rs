//! Exposes the not-yet-ported v1 Antigravity adapter through the v2 `Harness`
//! trait using the spawn-per-turn driver. Behaviour matches the v1 TUI (one
//! child per turn, `--continue` between turns). Antigravity has not been
//! verified against a live account; its stdin stream-json shape is unknown.

use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::process::Command;

use super::{HarnessKind, RunOptions, get_adapter};
use crate::core::per_turn::{self, PerTurnProtocol, TurnParser, TurnSpec, TurnState};
use crate::core::{
    AgentEvent, Capabilities, HarnessId, ModelRef, PermissionPolicy, PolicySupport, ProviderId,
    SessionConfig, SessionHandle,
};
use crate::harness::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};

pub struct LegacyHarness {
    id: HarnessId,
}

pub static AGY_DESCRIPTOR: HarnessDescriptor = HarnessDescriptor {
    id: HarnessId::Agy,
    display_name: "Antigravity (agy)",
    binary_names: &["agy"],
    providers: ProviderSource::Static(&[("google", "Google")]),
};

impl LegacyHarness {
    pub fn agy() -> Self {
        LegacyHarness { id: HarnessId::Agy }
    }

    fn kind(&self) -> HarnessKind {
        match self.id {
            HarnessId::Agy => HarnessKind::Agy,
            HarnessId::Codex => HarnessKind::Codex,
            HarnessId::Claude => HarnessKind::Claude,
            HarnessId::Pi => unreachable!("pi has no legacy adapter"),
        }
    }

    fn provider(&self) -> &'static str {
        match self.id {
            HarnessId::Agy => "google",
            HarnessId::Codex => "openai",
            HarnessId::Claude => "anthropic",
            HarnessId::Pi => "",
        }
    }
}

impl Harness for LegacyHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        &AGY_DESCRIPTOR
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming_input: false,
            text_deltas: true,
            thinking: true,
            tool_events: true,
            interactive_permissions: false,
            permission_policies: vec![
                PolicySupport::degraded(
                    PermissionPolicy::Ask,
                    "headless agy auto-denies tools that need a prompt",
                ),
                PolicySupport::full(PermissionPolicy::AcceptEdits),
                PolicySupport::full(PermissionPolicy::Bypass),
            ],
            effort_levels: ["low", "medium", "high"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            resume_by_id: false,
            live_model_list: true,
            multi_provider: false,
            ask_user_question: false,
            interrupt: true,
            usage_reporting: false,
        }
    }

    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        let adapter = get_adapter(self.kind());
        let binary = resolve_binary(self.descriptor(), binary_override);
        let version = binary.as_deref().and_then(probe_version);
        let auth = binary
            .as_deref()
            .map(|b| {
                let a = adapter.auth_status(b);
                AuthInfo {
                    authenticated: a.authenticated,
                    details: a.details,
                }
            })
            .unwrap_or_default();
        Probe {
            binary,
            version,
            auth,
        }
    }

    fn list_models(&self, binary: &Path, provider: &ProviderId) -> Result<Vec<ModelInfo>> {
        if provider.as_str() != self.provider() {
            return Ok(Vec::new());
        }
        let adapter = get_adapter(self.kind());
        Ok(adapter
            .available_models(Some(binary))
            .into_iter()
            .map(|m| ModelInfo {
                model_ref: ModelRef::new(self.id, self.provider(), m.id),
                display_name: m.display_name,
                description: m.description,
                effort_levels: None,
            })
            .collect())
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        let protocol: Arc<dyn PerTurnProtocol> = Arc::new(AgyPerTurn);
        per_turn::start(cfg, protocol)
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<StdCommand> {
        let adapter = get_adapter(self.kind());
        let opts = RunOptions {
            prompt: cfg.prompt.clone(),
            print_mode: cfg.print_mode,
            auto_approve: cfg.policy == Some(PermissionPolicy::Bypass),
            model: cfg.model.as_ref().map(|m| m.model.clone()),
            effort: cfg.effort.clone(),
            format: cfg.format.clone(),
            cwd: Some(cfg.cwd.clone()),
            extra_args: cfg.extra_args.clone(),
        };
        adapter.build_command(&cfg.binary, &opts)
    }
}

// ---------------------------------------------------------------------------
// Antigravity: `agy -p --output-format stream-json [--continue] <prompt>`

struct AgyPerTurn;

pub fn agy_policy_args(policy: PermissionPolicy) -> Vec<&'static str> {
    match policy {
        PermissionPolicy::Ask => vec![],
        PermissionPolicy::AcceptEdits | PermissionPolicy::Auto => vec!["--mode", "accept-edits"],
        PermissionPolicy::Bypass => vec!["--dangerously-skip-permissions"],
    }
}

impl PerTurnProtocol for AgyPerTurn {
    fn harness(&self) -> HarnessId {
        HarnessId::Agy
    }

    fn build_turn(&self, state: &TurnState, text: &str) -> Result<TurnSpec> {
        let mut command = Command::new(&state.binary);
        command.current_dir(&state.cwd);
        command.arg("--output-format").arg("stream-json");
        command.args(agy_policy_args(state.policy));
        if let Some(m) = &state.model {
            command.arg("--model").arg(&m.model);
        }
        if let Some(e) = &state.effort {
            command.arg("--effort").arg(e);
        }
        if state.turn_index > 0 {
            command.arg("--continue");
        }
        command.args(&state.extra_args);
        command.arg("--print").arg(text);
        for (k, v) in &state.env {
            command.env(k, v);
        }
        Ok(TurnSpec {
            command,
            stdin: None,
        })
    }

    fn new_parser(&self) -> Box<dyn TurnParser> {
        Box::new(AgyLineParser::default())
    }
}

#[derive(Default)]
pub struct AgyLineParser {
    tool_seq: usize,
}

impl TurnParser for AgyLineParser {
    fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            return vec![AgentEvent::Notice(line.to_string())];
        };
        let event = val.get("event").and_then(Value::as_str).unwrap_or("");
        match event {
            "step_update" => {
                let Some(step) = val.get("step_update") else {
                    return vec![];
                };
                let step_type = step.get("step_type").and_then(Value::as_str).unwrap_or("");
                let delta = step.get("text_delta").and_then(Value::as_str).unwrap_or("");
                match step_type {
                    "agent_response" if !delta.is_empty() => {
                        vec![AgentEvent::TextDelta(delta.to_string())]
                    }
                    "thought" if !delta.is_empty() => {
                        vec![AgentEvent::ThinkingDelta(delta.to_string())]
                    }
                    "tool_use" => {
                        self.tool_seq += 1;
                        vec![AgentEvent::ToolCallStarted {
                            id: format!("agy-{}", self.tool_seq),
                            name: step
                                .get("tool_name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string(),
                            input: step.get("input").cloned().unwrap_or(Value::Null),
                        }]
                    }
                    _ => vec![],
                }
            }
            _ => vec![],
        }
    }

    fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        let t = line.trim();
        if t.is_empty() || t.starts_with("Debugger") {
            return vec![];
        }
        if let Some(json) = t.strip_prefix("AGY_ERROR:") {
            let msg = serde_json::from_str::<Value>(json.trim())
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .or_else(|| v.get("error"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| json.trim().to_string());
            return vec![AgentEvent::Error(msg)];
        }
        vec![AgentEvent::Error(t.to_string())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state(policy: PermissionPolicy, turn_index: usize) -> TurnState {
        TurnState {
            binary: PathBuf::from("/bin/x"),
            cwd: PathBuf::from("/tmp"),
            model: Some(ModelRef::new(HarnessId::Agy, "google", "gemini-x")),
            effort: Some("high".into()),
            policy,
            session_id: None,
            extra_args: vec![],
            env: vec![],
            turn_index,
        }
    }

    fn argv(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn agy_turn_args() {
        let spec = AgyPerTurn
            .build_turn(&state(PermissionPolicy::Bypass, 1), "hi")
            .unwrap();
        let a = argv(&spec.command).join(" ");
        assert_eq!(
            a,
            "--output-format stream-json --dangerously-skip-permissions --model gemini-x --effort high --continue --print hi"
        );
        let first = AgyPerTurn
            .build_turn(&state(PermissionPolicy::AcceptEdits, 0), "hi")
            .unwrap();
        let a = argv(&first.command).join(" ");
        assert!(a.contains("--mode accept-edits"));
        assert!(!a.contains("--continue"));
    }

    #[test]
    fn agy_parser_maps_steps_and_errors() {
        let mut p = AgyLineParser::default();
        assert_eq!(
            p.feed(r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"hi"}}"#),
            vec![AgentEvent::TextDelta("hi".into())]
        );
        assert_eq!(
            p.feed(r#"{"event":"step_update","step_update":{"step_type":"thought","text_delta":"hmm"}}"#),
            vec![AgentEvent::ThinkingDelta("hmm".into())]
        );
        let ev = p.feed(r#"{"event":"step_update","step_update":{"step_type":"tool_use","tool_name":"run","input":{"cmd":"ls"}}}"#);
        assert!(
            matches!(&ev[0], AgentEvent::ToolCallStarted { id, name, .. } if id == "agy-1" && name == "run")
        );
        assert_eq!(
            p.feed_stderr(r#"AGY_ERROR: {"message":"quota exceeded"}"#),
            vec![AgentEvent::Error("quota exceeded".into())]
        );
        assert!(p.feed_stderr("Debugger attached").is_empty());
    }

    #[test]
    fn shim_capabilities_and_print_command() {
        let agy = LegacyHarness::agy();
        assert!(!agy.capabilities().interactive_permissions);
        assert_eq!(agy.descriptor().id, HarnessId::Agy);
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/agy"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("p".into()),
            print_mode: true,
            policy: Some(PermissionPolicy::Bypass),
            ..Default::default()
        };
        let cmd = agy.build_print_command(&cfg).unwrap();
        let a: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert!(a.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(a.contains(&"-p".to_string()));
    }
}
