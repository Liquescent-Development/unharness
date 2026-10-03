use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{AuthInfo, HarnessAdapter, HarnessKind, ModelInfo, RunOptions, which};

pub struct CodexAdapter;

impl HarnessAdapter for CodexAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn binary_name(&self) -> &'static str {
        "codex"
    }

    fn resolve_binary(&self, override_path: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = override_path
            && p.exists()
        {
            return Some(p.to_path_buf());
        }
        which("codex")
    }

    fn version(&self, binary: &Path) -> Option<String> {
        let output = Command::new(binary).arg("--version").output().ok()?;
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
        None
    }

    fn auth_status(&self, binary: &Path) -> AuthInfo {
        let output = Command::new(binary).args(["auth", "status"]).output().ok();
        if let Some(out) = output
            && out.status.success()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return AuthInfo {
                authenticated: true,
                details: if s.is_empty() { None } else { Some(s) },
            };
        }

        if let Some(home) = dirs::home_dir() {
            let config_path = home.join(".codex/config.toml");
            if config_path.exists() {
                return AuthInfo {
                    authenticated: true,
                    details: Some("~/.codex/config.toml found".to_string()),
                };
            }
        }

        AuthInfo {
            authenticated: false,
            details: Some("codex CLI not configured".to_string()),
        }
    }

    fn available_models(&self, _binary: Option<&Path>) -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "o3-mini".to_string(),
                display_name: "OpenAI o3-mini".to_string(),
                description: Some("Fast high-reasoning coding model".to_string()),
            },
            ModelInfo {
                id: "o1".to_string(),
                display_name: "OpenAI o1".to_string(),
                description: Some("Deepest reasoning model".to_string()),
            },
            ModelInfo {
                id: "gpt-4o".to_string(),
                display_name: "OpenAI GPT-4o".to_string(),
                description: Some("Fast multimodal workhorse".to_string()),
            },
        ]
    }

    fn build_command(&self, binary: &Path, opts: &RunOptions) -> Result<Command> {
        let mut cmd = Command::new(binary);

        if let Some(ref cwd) = opts.cwd {
            cmd.current_dir(cwd);
        }

        if opts.auto_approve {
            cmd.arg("--full-auto");
        }

        if let Some(ref model) = opts.model {
            cmd.arg("--model").arg(model);
        }

        for arg in &opts.extra_args {
            cmd.arg(arg);
        }

        if opts.print_mode {
            cmd.arg("exec");
            if let Some(ref prompt) = opts.prompt {
                cmd.arg(prompt);
            }
        } else if let Some(ref prompt) = opts.prompt {
            cmd.arg(prompt);
        }

        Ok(cmd)
    }
}
