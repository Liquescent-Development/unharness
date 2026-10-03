//! OpenAI Codex harness with two transports: `app-server` (long-lived JSON-RPC,
//! interactive approvals) and `exec --json` (one child per turn, no prompts).

pub mod app_server;
pub mod app_server_parse;
pub mod exec;
pub mod exec_parse;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};
use crate::core::jsonrpc;
use crate::core::{
    Capabilities, HarnessId, ModelRef, PermissionPolicy, PolicySupport, ProviderId, RewindSupport,
    SessionConfig, SessionHandle,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexTransport {
    /// app-server when available, else exec.
    #[default]
    Auto,
    AppServer,
    Exec,
}

impl CodexTransport {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "auto" => Some(CodexTransport::Auto),
            "app-server" | "appserver" | "app_server" => Some(CodexTransport::AppServer),
            "exec" => Some(CodexTransport::Exec),
            _ => None,
        }
    }
}

pub struct CodexHarness {
    pub transport: CodexTransport,
}

impl Default for CodexHarness {
    fn default() -> Self {
        CodexHarness {
            transport: CodexTransport::Auto,
        }
    }
}

pub static DESCRIPTOR: HarnessDescriptor = HarnessDescriptor {
    id: HarnessId::Codex,
    display_name: "Codex (codex)",
    binary_names: &["codex"],
    providers: ProviderSource::Static(&[("openai", "OpenAI")]),
};

pub const EFFORT_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

/// Static fallback, checked 2026-10-02 against `model/list`.
const FALLBACK_MODELS: &[(&str, &str, &str)] = &[
    (
        "gpt-6-astra",
        "GPT-6-Astra",
        "Frontier intelligence for the most demanding work",
    ),
    ("gpt-5.5", "GPT-5.5", "Strong general coding model"),
    (
        "gpt-5.3-codex-spark",
        "GPT-5.3 Codex Spark",
        "Fast coding model",
    ),
];

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

impl CodexHarness {
    pub fn new(transport: CodexTransport) -> Self {
        CodexHarness { transport }
    }

    /// Resolve `Auto` against the installed binary.
    fn effective_transport(&self, binary: &Path) -> CodexTransport {
        match self.transport {
            CodexTransport::Auto => {
                let ok = Command::new(binary)
                    .args(["app-server", "--help"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if ok {
                    CodexTransport::AppServer
                } else {
                    CodexTransport::Exec
                }
            }
            t => t,
        }
    }
}

impl Harness for CodexHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        &DESCRIPTOR
    }

    fn capabilities(&self) -> Capabilities {
        let app_server = self.transport != CodexTransport::Exec;
        Capabilities {
            streaming_input: app_server,
            text_deltas: app_server,
            thinking: true,
            tool_events: true,
            interactive_permissions: app_server,
            permission_policies: if app_server {
                PermissionPolicy::ALL
                    .iter()
                    .copied()
                    .map(PolicySupport::full)
                    .collect()
            } else {
                vec![
                    PolicySupport::degraded(
                        PermissionPolicy::Ask,
                        "codex exec cannot prompt; running read-only",
                    ),
                    PolicySupport::full(PermissionPolicy::AcceptEdits),
                    PolicySupport::full(PermissionPolicy::Auto),
                    PolicySupport::full(PermissionPolicy::Bypass),
                ]
            },
            effort_levels: EFFORT_LEVELS.iter().map(|s| s.to_string()).collect(),
            resume_by_id: true,
            live_model_list: app_server,
            multi_provider: false,
            ask_user_question: app_server,
            interrupt: true,
            usage_reporting: true,
            image_input: true,
            plan_updates: true,
            subagents: false,
            steer: app_server,
            compaction: app_server,
            context_usage: app_server,
            rate_limits: app_server,
            rewind: RewindSupport::default(),
            fork: false,
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

    fn list_models(&self, binary: &Path, provider: &ProviderId) -> Result<Vec<ModelInfo>> {
        if provider.as_str() != "openai" {
            return Ok(Vec::new());
        }
        let live = match self.effective_transport(binary) {
            CodexTransport::Exec => Vec::new(),
            _ => query_models(binary).unwrap_or_default(),
        };
        if !live.is_empty() {
            return Ok(live
                .iter()
                .filter(|m| !m.get("hidden").and_then(Value::as_bool).unwrap_or(false))
                .map(|m| {
                    let id = m
                        .get("model")
                        .or_else(|| m.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let efforts: Vec<String> = m
                        .get("supportedReasoningEfforts")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| e.get("reasoningEffort").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    ModelInfo {
                        model_ref: ModelRef::new(HarnessId::Codex, "openai", id),
                        display_name: m
                            .get("displayName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        description: m
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        effort_levels: (!efforts.is_empty()).then_some(efforts),
                    }
                })
                .collect());
        }
        Ok(FALLBACK_MODELS
            .iter()
            .map(|(id, name, desc)| ModelInfo {
                model_ref: ModelRef::new(HarnessId::Codex, "openai", *id),
                display_name: name.to_string(),
                description: Some(desc.to_string()),
                effort_levels: None,
            })
            .collect())
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        match self.effective_transport(&cfg.binary) {
            CodexTransport::Exec => crate::core::per_turn::start(cfg, Arc::new(exec::CodexExec)),
            _ => app_server::start(cfg),
        }
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<Command> {
        let mut cmd = Command::new(&cfg.binary);
        cmd.current_dir(&cfg.cwd);
        if cfg.print_mode {
            cmd.arg("exec");
            if let Some(id) = &cfg.resume {
                cmd.arg("resume").arg(id);
            }
            cmd.arg("--skip-git-repo-check");
            if cfg.format.as_deref().is_some_and(|f| f != "text") {
                cmd.arg("--json");
            }
            if let Some(p) = cfg.policy {
                if cfg.resume.is_some() {
                    for o in exec::policy_config_overrides(p) {
                        cmd.arg("-c").arg(o);
                    }
                } else {
                    cmd.args(exec::policy_args(p));
                }
            }
        } else {
            if let Some(id) = &cfg.resume {
                cmd.arg("resume").arg(id);
            }
            if cfg.policy == Some(PermissionPolicy::Bypass) {
                cmd.arg("--dangerously-bypass-approvals-and-sandbox");
            } else if let Some(p) = cfg.policy {
                cmd.args(exec::policy_args(p));
            }
        }
        if let Some(m) = &cfg.model {
            cmd.arg("-m").arg(&m.model);
        }
        if let Some(e) = &cfg.effort {
            cmd.arg("-c").arg(format!("model_reasoning_effort=\"{e}\""));
        }
        cmd.args(&cfg.extra_args);
        if let Some(p) = &cfg.prompt {
            cmd.arg(p);
        }
        Ok(cmd)
    }
}

fn auth_status(binary: &Path) -> AuthInfo {
    let out = Command::new(binary).args(["login", "status"]).output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let s = if s.is_empty() {
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            } else {
                s
            };
            AuthInfo {
                authenticated: !s.to_lowercase().contains("not logged in"),
                details: (!s.is_empty()).then_some(s),
            }
        }
        _ => AuthInfo {
            authenticated: false,
            details: Some("not logged in (run `codex login`)".into()),
        },
    }
}

/// Ask a throwaway `codex app-server` for `model/list`.
pub fn query_models(binary: &Path) -> Result<Vec<Value>> {
    let mut child = Command::new(binary)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn codex app-server")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    writeln!(
        stdin,
        "{}",
        jsonrpc::request(
            1,
            "initialize",
            json!({"clientInfo": {"name": "unharness", "version": env!("CARGO_PKG_VERSION")}})
        )
    )?;
    stdin.flush()?;

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut models = Vec::new();
    let mut found = false;
    for line in BufReader::new(stdout).lines() {
        if Instant::now() > deadline {
            break;
        }
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v.get("id").and_then(Value::as_u64) {
            Some(1) if v.get("result").is_some() => {
                writeln!(
                    stdin,
                    "{}",
                    jsonrpc::notification("initialized", Value::Null)
                )?;
                writeln!(stdin, "{}", jsonrpc::request(2, "model/list", json!({})))?;
                stdin.flush()?;
            }
            Some(2) => {
                if let Some(list) = v.pointer("/result/data").and_then(Value::as_array) {
                    models = list.clone();
                }
                found = true;
                break;
            }
            _ => {}
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    if !found {
        anyhow::bail!("codex app-server did not answer model/list");
    }
    Ok(models)
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
            binary: PathBuf::from("/bin/codex"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("fix it".into()),
            print_mode: true,
            model: Some(ModelRef::new(HarnessId::Codex, "openai", "gpt-5.5")),
            effort: Some("low".into()),
            policy: Some(PermissionPolicy::AcceptEdits),
            format: None,
            resume: None,
            extra_args: vec![],
        };
        let h = CodexHarness::default();
        assert_eq!(
            args(&h.build_print_command(&base).unwrap()),
            "exec --skip-git-repo-check -s workspace-write -m gpt-5.5 -c model_reasoning_effort=\"low\" fix it"
        );
        let resumed = PrintConfig {
            resume: Some("t1".into()),
            format: Some("stream-json".into()),
            ..base.clone()
        };
        let a = args(&h.build_print_command(&resumed).unwrap());
        assert!(a.starts_with("exec resume t1 --skip-git-repo-check --json -c sandbox_mode="));
        let tui = PrintConfig {
            print_mode: false,
            prompt: None,
            policy: Some(PermissionPolicy::Bypass),
            ..base
        };
        assert_eq!(
            args(&h.build_print_command(&tui).unwrap()),
            "--dangerously-bypass-approvals-and-sandbox -m gpt-5.5 -c model_reasoning_effort=\"low\""
        );
    }

    #[test]
    fn capabilities_by_transport() {
        let app = CodexHarness::new(CodexTransport::AppServer).capabilities();
        assert!(app.interactive_permissions && app.ask_user_question);
        assert!(
            app.supports_policy(PermissionPolicy::Ask)
                .unwrap()
                .degraded
                .is_none()
        );
        let exec = CodexHarness::new(CodexTransport::Exec).capabilities();
        assert!(!exec.interactive_permissions);
        assert!(
            exec.supports_policy(PermissionPolicy::Ask)
                .unwrap()
                .degraded
                .is_some()
        );
        assert_eq!(
            CodexTransport::parse("app_server"),
            Some(CodexTransport::AppServer)
        );
        assert_eq!(CodexTransport::parse("x"), None);
    }
}
