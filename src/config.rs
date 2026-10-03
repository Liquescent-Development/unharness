use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Config {
    /// Preferred default harness (agy, claude, codex, pi)
    pub default_harness: Option<String>,

    /// Whether to automatically sync skills and rules before running
    #[serde(default = "default_true")]
    pub auto_sync: bool,

    /// Harness-specific settings
    #[serde(default)]
    pub harnesses: HarnessConfigs,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HarnessConfigs {
    #[serde(default)]
    pub agy: HarnessSettings,
    #[serde(default)]
    pub claude: HarnessSettings,
    #[serde(default)]
    pub codex: HarnessSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HarnessSettings {
    pub binary: Option<PathBuf>,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

impl Config {
    pub fn load_effective(workspace_root: Option<&Path>) -> Self {
        let global_config = Self::load_global().unwrap_or_default();
        let workspace_config = workspace_root.and_then(|root| Self::load_from_dir(root).ok());

        match workspace_config {
            Some(local) => Self::merge(global_config, local),
            None => global_config,
        }
    }

    pub fn load_global() -> Result<Self> {
        if let Some(config_dir) = dirs::config_dir() {
            let path = config_dir.join("unharness").join("config.toml");
            if path.exists() {
                let content = std::fs::read_to_string(&path)?;
                let cfg: Config = toml::from_str(&content)?;
                return Ok(cfg);
            }
        }
        Ok(Config::default())
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let candidates = ["unharness.toml", ".unharness.toml"];
        for candidate in candidates {
            let path = dir.join(candidate);
            if path.exists() {
                let content = std::fs::read_to_string(&path)?;
                let cfg: Config = toml::from_str(&content)?;
                return Ok(cfg);
            }
        }
        anyhow::bail!("No unharness configuration found in {}", dir.display())
    }

    pub fn save_to_dir(&self, dir: &Path) -> Result<PathBuf> {
        let path = dir.join("unharness.toml");
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(path)
    }

    pub fn save_global(&self) -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine user config directory"))?
            .join("unharness");
        std::fs::create_dir_all(&config_dir)?;
        let path = config_dir.join("config.toml");
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(path)
    }

    pub fn merge(global: Self, local: Self) -> Self {
        Self {
            default_harness: local.default_harness.or(global.default_harness),
            auto_sync: local.auto_sync,
            harnesses: HarnessConfigs {
                agy: Self::merge_settings(global.harnesses.agy, local.harnesses.agy),
                claude: Self::merge_settings(global.harnesses.claude, local.harnesses.claude),
                codex: Self::merge_settings(global.harnesses.codex, local.harnesses.codex),
            },
        }
    }

    fn merge_settings(global: HarnessSettings, local: HarnessSettings) -> HarnessSettings {
        HarnessSettings {
            binary: local.binary.or(global.binary),
            default_model: local.default_model.or(global.default_model),
            default_effort: local.default_effort.or(global.default_effort),
            extra_args: if !local.extra_args.is_empty() {
                local.extra_args
            } else {
                global.extra_args
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let cfg = Config::default();
        assert!(cfg.default_harness.is_none());
        assert!(!cfg.auto_sync); // struct default is false, toml default via fn default_true()
    }

    #[test]
    fn test_toml_roundtrip() {
        let toml_str = r#"
default_harness = "agy"
auto_sync = true

[harnesses.agy]
default_model = "gemini-3.8-flash-high"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.default_harness.as_deref(), Some("agy"));
        assert!(cfg.auto_sync);
        assert_eq!(cfg.harnesses.agy.default_model.as_deref(), Some("gemini-3.8-flash-high"));

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg, deserialized);
    }

    #[test]
    fn test_config_merge() {
        let global = Config {
            default_harness: Some("claude".to_string()),
            auto_sync: true,
            harnesses: HarnessConfigs {
                agy: HarnessSettings {
                    default_model: Some("global-model".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        };

        let local = Config {
            default_harness: Some("agy".to_string()),
            auto_sync: true,
            harnesses: HarnessConfigs {
                agy: HarnessSettings {
                    default_effort: Some("high".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        };

        let merged = Config::merge(global, local);
        assert_eq!(merged.default_harness.as_deref(), Some("agy")); // local overrides global
        assert_eq!(merged.harnesses.agy.default_model.as_deref(), Some("global-model")); // inherited from global
        assert_eq!(merged.harnesses.agy.default_effort.as_deref(), Some("high")); // local
    }
}
