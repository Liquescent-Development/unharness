use std::path::{Path, PathBuf};
use std::process::Command;
use anyhow::Result;

use super::{which, AuthInfo, HarnessAdapter, HarnessKind, ModelInfo, RunOptions};

pub struct ClaudeAdapter;

impl HarnessAdapter for ClaudeAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Claude
    }

    fn binary_name(&self) -> &'static str {
        "claude"
    }

    fn resolve_binary(&self, override_path: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = override_path {
            if p.exists() {
                return Some(p.to_path_buf());
            }
        }
        which("claude")
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
        if let Some(out) = output {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&s) {
                    let logged_in = val.get("loggedIn").and_then(|v| v.as_bool()).unwrap_or(false);
                    let org = val.get("orgName").and_then(|v| v.as_str());
                    let plan = val.get("subscriptionType").and_then(|v| v.as_str());
                    let email = val.get("email").and_then(|v| v.as_str());

                    let mut details = Vec::new();
                    if let Some(o) = org {
                        details.push(format!("org: {}", o));
                    }
                    if let Some(p) = plan {
                        details.push(format!("plan: {}", p));
                    }
                    if let Some(e) = email {
                        details.push(format!("user: {}", e));
                    }

                    return AuthInfo {
                        authenticated: logged_in,
                        details: if details.is_empty() {
                            None
                        } else {
                            Some(details.join(", "))
                        },
                    };
                }
            }
        }

        AuthInfo {
            authenticated: false,
            details: Some("Not authenticated with claude.ai".to_string()),
        }
    }

    fn available_models(&self, _binary: Option<&Path>) -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "claude-opus-5-5".to_string(),
                display_name: "Claude Opus 5.5".to_string(),
                description: Some("Deepest reasoning, most capable coding model (default)".to_string()),
            },
            ModelInfo {
                id: "claude-sonnet-5-5".to_string(),
                display_name: "Claude Sonnet 5.5".to_string(),
                description: Some("Fast, powerful coding workhorse".to_string()),
            },
            ModelInfo {
                id: "claude-fable-5-1".to_string(),
                display_name: "Claude Fable 5.1".to_string(),
                description: Some("Fast reasoning and architectural planning".to_string()),
            },
            ModelInfo {
                id: "claude-haiku-4-5".to_string(),
                display_name: "Claude Haiku 4.5".to_string(),
                description: Some("Fastest latency for routine changes".to_string()),
            },
            ModelInfo {
                id: "opus".to_string(),
                display_name: "opus (latest)".to_string(),
                description: Some("Alias for latest Opus release".to_string()),
            },
            ModelInfo {
                id: "sonnet".to_string(),
                display_name: "sonnet (latest)".to_string(),
                description: Some("Alias for latest Sonnet release".to_string()),
            },
            ModelInfo {
                id: "fable".to_string(),
                display_name: "fable (latest)".to_string(),
                description: Some("Alias for latest Fable release".to_string()),
            },
        ]
    }

    fn build_command(&self, binary: &Path, opts: &RunOptions) -> Result<Command> {
        let mut cmd = Command::new(binary);

        if let Some(ref cwd) = opts.cwd {
            cmd.current_dir(cwd);
        }

        // Auto permissions
        if opts.auto_approve {
            cmd.arg("--dangerously-skip-permissions");
        }

        // Model
        if let Some(ref model) = opts.model {
            cmd.arg("--model").arg(model);
        }

        // Reasoning effort / think level
        if let Some(ref effort) = opts.effort {
            cmd.arg("--effort").arg(effort);
        }

        // Format
        if let Some(ref fmt) = opts.format {
            cmd.arg("--output-format").arg(fmt);
        }

        for arg in &opts.extra_args {
            cmd.arg(arg);
        }

        if opts.print_mode {
            cmd.arg("-p");
            if let Some(ref prompt) = opts.prompt {
                cmd.arg(prompt);
            }
        } else if let Some(ref prompt) = opts.prompt {
            cmd.arg(prompt);
        }

        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_build_interactive() {
        let adapter = ClaudeAdapter;
        let opts = RunOptions {
            prompt: Some("refactor this".to_string()),
            auto_approve: true,
            model: Some("claude-3-7-sonnet-20250219".to_string()),
            ..Default::default()
        };
        let cmd = adapter.build_command(Path::new("/bin/claude"), &opts).unwrap();
        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert_eq!(args.last().unwrap(), "refactor this");
        assert!(!args.contains(&"-i".to_string()));
    }

    #[test]
    fn test_claude_build_print_mode() {
        let adapter = ClaudeAdapter;
        let opts = RunOptions {
            prompt: Some("run tests".to_string()),
            print_mode: true,
            format: Some("stream-json".to_string()),
            ..Default::default()
        };
        let cmd = adapter.build_command(Path::new("/bin/claude"), &opts).unwrap();
        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"run tests".to_string()));
    }

    #[test]
    fn test_claude_available_models() {
        let adapter = ClaudeAdapter;
        let models = adapter.available_models(None);
        assert!(models.iter().any(|m| m.id == "claude-opus-5-5"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-5-5"));
        assert!(models.iter().any(|m| m.id == "claude-fable-5-1"));
    }
}
