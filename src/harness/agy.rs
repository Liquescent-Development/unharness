use std::path::{Path, PathBuf};
use std::process::Command;
use anyhow::Result;

use super::{which, AuthInfo, HarnessAdapter, HarnessKind, ModelInfo, RunOptions};

pub struct AgyAdapter;

impl HarnessAdapter for AgyAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Agy
    }

    fn binary_name(&self) -> &'static str {
        "agy"
    }

    fn resolve_binary(&self, override_path: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = override_path {
            if p.exists() {
                return Some(p.to_path_buf());
            }
        }
        which("agy")
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

    fn auth_status(&self, _binary: &Path) -> AuthInfo {
        if let Some(home) = dirs::home_dir() {
            let settings_path = home.join(".gemini/antigravity-cli/settings.json");
            if settings_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&settings_path) {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(gcp) = val.get("gcp") {
                            let project = gcp.get("project").and_then(|p| p.as_str()).unwrap_or("configured");
                            return AuthInfo {
                                authenticated: true,
                                details: Some(format!("GCP project: {}", project)),
                            };
                        }
                    }
                }
            }
        }

        AuthInfo {
            authenticated: false,
            details: Some("No GCP project found in ~/.gemini/antigravity-cli/settings.json".to_string()),
        }
    }

    fn available_models(&self, binary: Option<&Path>) -> Vec<ModelInfo> {
        // Try live discovery via `agy models`
        if let Some(bin) = binary {
            if let Ok(output) = Command::new(bin).arg("models").output() {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    let mut models = Vec::new();
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.is_empty() || trimmed.starts_with("Fetching") {
                            continue;
                        }
                        let parts: Vec<&str> = trimmed.split('\t').collect();
                        if parts.len() >= 2 {
                            models.push(ModelInfo {
                                id: parts[0].trim().to_string(),
                                display_name: parts[1].trim().to_string(),
                                description: None,
                            });
                        } else if !trimmed.is_empty() {
                            models.push(ModelInfo {
                                id: trimmed.to_string(),
                                display_name: trimmed.to_string(),
                                description: None,
                            });
                        }
                    }
                    if !models.is_empty() {
                        return models;
                    }
                }
            }
        }

        // Fallback default Gemini catalog
        vec![
            ModelInfo {
                id: "gemini-3.8-flash-high".to_string(),
                display_name: "Gemini 3.8 Flash (High)".to_string(),
                description: Some("Fast & high reasoning effort (default)".to_string()),
            },
            ModelInfo {
                id: "gemini-3.8-flash-medium".to_string(),
                display_name: "Gemini 3.8 Flash (Medium)".to_string(),
                description: Some("Fast & balanced reasoning".to_string()),
            },
            ModelInfo {
                id: "gemini-3.7-flash-high".to_string(),
                display_name: "Gemini 3.7 Flash (High)".to_string(),
                description: Some("Gemini 3.7 high reasoning tier".to_string()),
            },
            ModelInfo {
                id: "gemini-3.1-pro-high".to_string(),
                display_name: "Gemini 3.1 Pro (High)".to_string(),
                description: Some("Pro model with high reasoning".to_string()),
            },
        ]
    }

    fn build_command(&self, binary: &Path, opts: &RunOptions) -> Result<Command> {
        let mut cmd = Command::new(binary);

        if let Some(ref cwd) = opts.cwd {
            cmd.current_dir(cwd);
        }

        if opts.auto_approve {
            cmd.arg("--dangerously-skip-permissions");
        }

        if let Some(ref model) = opts.model {
            cmd.arg("--model").arg(model);
        }

        if let Some(ref effort) = opts.effort {
            cmd.arg("--effort").arg(effort);
        }

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
            cmd.arg("-i").arg(prompt);
        }

        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agy_build_interactive_prompt() {
        let adapter = AgyAdapter;
        let opts = RunOptions {
            prompt: Some("hello agy".to_string()),
            auto_approve: true,
            model: Some("gemini-3.8-flash-high".to_string()),
            ..Default::default()
        };
        let cmd = adapter.build_command(Path::new("/bin/agy"), &opts).unwrap();
        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"gemini-3.8-flash-high".to_string()));
        assert_eq!(args.last().unwrap(), "hello agy");
        assert!(args.contains(&"-i".to_string()));
    }

    #[test]
    fn test_agy_build_print_mode() {
        let adapter = AgyAdapter;
        let opts = RunOptions {
            prompt: Some("analyze this".to_string()),
            print_mode: true,
            format: Some("json".to_string()),
            ..Default::default()
        };
        let cmd = adapter.build_command(Path::new("/bin/agy"), &opts).unwrap();
        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"json".to_string()));
        assert!(args.contains(&"analyze this".to_string()));
        assert!(!args.contains(&"-i".to_string()));
    }

    #[test]
    fn test_agy_available_models_fallback() {
        let adapter = AgyAdapter;
        let models = adapter.available_models(None);
        assert!(!models.is_empty());
        assert!(models.iter().any(|m| m.id == "gemini-3.8-flash-high"));
    }
}
