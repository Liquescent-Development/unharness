//! pi harness: a multi-provider coding agent with a JSONL RPC mode.

pub mod parse;
pub mod transport;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};
use crate::core::{
    Capabilities, HarnessId, ModelRef, PermissionPolicy, PolicySupport, ProviderId, RewindSupport,
    SessionConfig, SessionHandle,
};

pub struct PiHarness;

pub static DESCRIPTOR: HarnessDescriptor = HarnessDescriptor {
    id: HarnessId::PI,
    display_name: "pi",
    short_name: "pi",
    binary_names: &["pi"],
    providers: ProviderSource::Dynamic,
};

pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// How long to wait for `pi --mode rpc` to answer a catalog query.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

impl Harness for PiHarness {
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
            permission_policies: vec![
                PolicySupport::degraded(
                    PermissionPolicy::Ask,
                    "pi only prompts when an extension asks; built-in tools run freely",
                ),
                PolicySupport::degraded(
                    PermissionPolicy::Bypass,
                    "extension confirm/select dialogs are auto-accepted",
                ),
            ],
            effort_levels: THINKING_LEVELS.iter().map(|s| s.to_string()).collect(),
            resume_by_id: true,
            live_model_list: true,
            multi_provider: true,
            ask_user_question: false,
            interrupt: true,
            usage_reporting: true,
            image_input: false,
            plan_updates: false,
            subagents: false,
            steer: true,
            compaction: true,
            context_usage: true,
            rate_limits: false,
            rewind: RewindSupport {
                conversation: true,
                files: false,
            },
            fork: false,
        }
    }

    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        let binary = resolve_binary(&DESCRIPTOR, binary_override);
        let version = binary.as_deref().and_then(probe_version);
        let auth = binary
            .as_deref()
            .map(|b| match query_models(b) {
                Ok(models) if !models.is_empty() => AuthInfo {
                    authenticated: true,
                    details: Some(format!("{} models across providers", models.len())),
                },
                Ok(_) => AuthInfo {
                    authenticated: false,
                    details: Some("no models available (configure provider credentials)".into()),
                },
                Err(e) => AuthInfo {
                    authenticated: false,
                    details: Some(format!("could not query models: {e}")),
                },
            })
            .unwrap_or_default();
        Probe {
            binary,
            version,
            auth,
        }
    }

    fn list_providers(&self, binary: &Path) -> Result<Vec<(ProviderId, String)>> {
        let mut seen = Vec::new();
        for m in query_models(binary)? {
            let p = m
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !p.is_empty()
                && !seen
                    .iter()
                    .any(|(id, _): &(ProviderId, String)| id.as_str() == p)
            {
                seen.push((ProviderId::new(p.clone()), p));
            }
        }
        Ok(seen)
    }

    fn list_models(&self, binary: &Path, provider: &ProviderId) -> Result<Vec<ModelInfo>> {
        Ok(query_models(binary)?
            .into_iter()
            .filter(|m| m.get("provider").and_then(Value::as_str) == Some(provider.as_str()))
            .map(|m| {
                let id = m
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let reasoning = m.get("reasoning").and_then(Value::as_bool).unwrap_or(false);
                ModelInfo {
                    model_ref: ModelRef::new(HarnessId::PI, provider.clone(), id),
                    display_name: m
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    description: m.get("api").and_then(Value::as_str).map(str::to_string),
                    effort_levels: Some(if reasoning {
                        THINKING_LEVELS.iter().map(|s| s.to_string()).collect()
                    } else {
                        vec!["off".to_string()]
                    }),
                }
            })
            .collect())
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        transport::start(cfg)
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<Command> {
        let mut cmd = Command::new(&cfg.binary);
        cmd.current_dir(&cfg.cwd);
        if let Some(m) = &cfg.model {
            if m.provider.as_str() != "default" && !m.provider.as_str().is_empty() {
                cmd.arg("--provider").arg(m.provider.as_str());
            }
            cmd.arg("--model").arg(&m.model);
        }
        if let Some(e) = &cfg.effort {
            cmd.arg("--thinking").arg(e);
        }
        if let Some(id) = &cfg.resume {
            cmd.arg("--session-id").arg(id);
        }
        match cfg.format.as_deref() {
            Some("json") | Some("stream-json") => {
                cmd.arg("--mode").arg("json");
            }
            _ => {}
        }
        if cfg.print_mode {
            cmd.arg("--print");
        }
        cmd.args(&cfg.extra_args);
        if let Some(p) = &cfg.prompt {
            cmd.arg("--").arg(p);
        }
        Ok(cmd)
    }
}

/// Ask a throwaway `pi --mode rpc --no-session` for its model catalog.
pub fn query_models(binary: &Path) -> Result<Vec<Value>> {
    let mut child = Command::new(binary)
        .args(["--mode", "rpc", "--no-session", "--no-extensions"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn pi")?;
    let mut stdin = child.stdin.take().context("pi stdin")?;
    let stdout = child.stdout.take().context("pi stdout")?;
    writeln!(
        stdin,
        "{}",
        json!({"id":"models","type":"get_available_models"})
    )?;
    stdin.flush()?;

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut result = Vec::new();
    let mut found = false;
    for line in BufReader::new(stdout).lines() {
        if Instant::now() > deadline {
            break;
        }
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("id").and_then(Value::as_str) == Some("models") {
            if let Some(models) = v.pointer("/data/models").and_then(Value::as_array) {
                result = models.clone();
            }
            found = true;
            break;
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    if !found {
        anyhow::bail!("pi did not answer get_available_models");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn print_command_shape() {
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/pi"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("hi there".into()),
            print_mode: true,
            model: Some(ModelRef::new(HarnessId::PI, "openai", "gpt-5.5")),
            effort: Some("low".into()),
            policy: Some(PermissionPolicy::Ask),
            format: Some("json".into()),
            resume: Some("s1".into()),
            extra_args: vec![],
        };
        let cmd = PiHarness.build_print_command(&cfg).unwrap();
        let a: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            a.join(" "),
            "--provider openai --model gpt-5.5 --thinking low --session-id s1 --mode json --print -- hi there"
        );
    }

    #[test]
    fn capabilities() {
        let c = PiHarness.capabilities();
        assert!(c.multi_provider && c.live_model_list && c.interactive_permissions);
        assert!(c.supports_policy(PermissionPolicy::Auto).is_none());
        assert!(c.supports_effort("xhigh"));
    }
}
