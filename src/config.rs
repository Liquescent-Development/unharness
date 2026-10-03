use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Config {
    /// Preferred default harness (agy, claude, codex, pi).
    pub default_harness: Option<String>,

    /// Default permission policy (ask, accept-edits, auto, bypass).
    pub default_policy: Option<String>,

    /// Whether to synchronize rules symlinks before running.
    #[serde(default = "default_true")]
    pub auto_sync: bool,

    /// Maximum characters of transcript bridged into a new harness session on switch.
    pub bridge_max_chars: Option<usize>,

    /// Harness-specific settings keyed by harness id.
    #[serde(default)]
    pub harnesses: HashMap<String, HarnessSettings>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HarnessSettings {
    pub binary: Option<PathBuf>,
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    pub default_policy: Option<String>,
    /// Harness-specific transport selector (e.g. codex: auto | app-server | exec).
    pub transport: Option<String>,
    pub persist_sessions: Option<bool>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// `"acp"` defines a harness for an Agent Client Protocol agent under
    /// this table's name; `command` is then required.
    pub protocol: Option<String>,
    /// The agent's command line for an ACP harness, e.g. `["gemini", "--acp"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Name shown in the TUI for a config-defined harness.
    pub display_name: Option<String>,
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

    pub fn global_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("unharness").join("config.toml"))
    }

    pub fn load_global() -> Result<Self> {
        if let Some(path) = Self::global_path()
            && path.exists()
        {
            let content = std::fs::read_to_string(&path)?;
            return Ok(toml::from_str(&content)?);
        }
        Ok(Config::default())
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        for candidate in ["unharness.toml", ".unharness.toml"] {
            let path = dir.join(candidate);
            if path.exists() {
                let content = std::fs::read_to_string(&path)?;
                return Ok(toml::from_str(&content)?);
            }
        }
        anyhow::bail!("No unharness configuration found in {}", dir.display())
    }

    pub fn save_to_dir(&self, dir: &Path) -> Result<PathBuf> {
        let path = dir.join("unharness.toml");
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(path)
    }

    pub fn save_global(&self) -> Result<PathBuf> {
        let path = Self::global_path()
            .ok_or_else(|| anyhow::anyhow!("Could not determine user config directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(path)
    }

    /// Settings for one harness, if configured.
    pub fn harness(&self, id: &str) -> Option<&HarnessSettings> {
        self.harnesses.get(id)
    }

    pub fn binary_override(&self, id: &str) -> Option<&Path> {
        self.harness(id).and_then(|h| h.binary.as_deref())
    }

    pub fn default_model(&self, id: &str) -> Option<&str> {
        self.harness(id).and_then(|h| h.default_model.as_deref())
    }

    pub fn default_effort(&self, id: &str) -> Option<&str> {
        self.harness(id).and_then(|h| h.default_effort.as_deref())
    }

    pub fn extra_args(&self, id: &str) -> &[String] {
        self.harness(id)
            .map(|h| h.extra_args.as_slice())
            .unwrap_or(&[])
    }

    pub fn merge(global: Self, local: Self) -> Self {
        let mut harnesses = global.harnesses;
        for (id, local_settings) in local.harnesses {
            let merged = match harnesses.remove(&id) {
                Some(global_settings) => Self::merge_settings(global_settings, local_settings),
                None => local_settings,
            };
            harnesses.insert(id, merged);
        }
        Self {
            default_harness: local.default_harness.or(global.default_harness),
            default_policy: local.default_policy.or(global.default_policy),
            auto_sync: local.auto_sync,
            bridge_max_chars: local.bridge_max_chars.or(global.bridge_max_chars),
            harnesses,
        }
    }

    fn merge_settings(global: HarnessSettings, local: HarnessSettings) -> HarnessSettings {
        HarnessSettings {
            binary: local.binary.or(global.binary),
            default_provider: local.default_provider.or(global.default_provider),
            default_model: local.default_model.or(global.default_model),
            default_effort: local.default_effort.or(global.default_effort),
            default_policy: local.default_policy.or(global.default_policy),
            transport: local.transport.or(global.transport),
            persist_sessions: local.persist_sessions.or(global.persist_sessions),
            extra_args: if !local.extra_args.is_empty() {
                local.extra_args
            } else {
                global.extra_args
            },
            protocol: local.protocol.or(global.protocol),
            command: if !local.command.is_empty() {
                local.command
            } else {
                global.command
            },
            display_name: local.display_name.or(global.display_name),
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
        assert!(!cfg.auto_sync); // struct default is false; TOML default is true via default_true()
        assert!(cfg.harness("claude").is_none());
        assert!(cfg.extra_args("claude").is_empty());
    }

    #[test]
    fn test_legacy_toml_still_parses() {
        let toml_str = r#"
default_harness = "agy"
auto_sync = true

[harnesses.agy]
extra_args = []

[harnesses.claude]
default_model = "opus"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.default_harness.as_deref(), Some("agy"));
        assert!(cfg.auto_sync);
        assert_eq!(cfg.default_model("claude"), Some("opus"));
        assert!(cfg.harness("codex").is_none());
    }

    #[test]
    fn test_toml_roundtrip() {
        let toml_str = r#"
default_harness = "pi"
default_policy = "accept-edits"
bridge_max_chars = 1000

[harnesses.pi]
default_provider = "anthropic"
default_model = "claude-sonnet-4-5"
transport = "rpc"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg, deserialized);
        assert_eq!(
            cfg.harness("pi").unwrap().default_provider.as_deref(),
            Some("anthropic")
        );
    }

    #[test]
    fn test_config_merge() {
        let mut global = Config {
            default_harness: Some("claude".to_string()),
            default_policy: Some("ask".to_string()),
            auto_sync: true,
            ..Default::default()
        };
        global.harnesses.insert(
            "agy".into(),
            HarnessSettings {
                default_model: Some("global-model".to_string()),
                ..Default::default()
            },
        );
        global.harnesses.insert(
            "codex".into(),
            HarnessSettings {
                transport: Some("exec".to_string()),
                ..Default::default()
            },
        );

        let mut local = Config {
            default_harness: Some("agy".to_string()),
            auto_sync: true,
            ..Default::default()
        };
        local.harnesses.insert(
            "agy".into(),
            HarnessSettings {
                default_effort: Some("high".to_string()),
                ..Default::default()
            },
        );
        local.harnesses.insert(
            "pi".into(),
            HarnessSettings {
                default_provider: Some("openai".to_string()),
                ..Default::default()
            },
        );

        let merged = Config::merge(global, local);
        assert_eq!(merged.default_harness.as_deref(), Some("agy")); // local wins
        assert_eq!(merged.default_policy.as_deref(), Some("ask")); // inherited
        assert_eq!(merged.default_model("agy"), Some("global-model")); // inherited
        assert_eq!(merged.default_effort("agy"), Some("high")); // local
        assert_eq!(
            merged.harness("codex").unwrap().transport.as_deref(),
            Some("exec")
        ); // global only
        assert_eq!(
            merged.harness("pi").unwrap().default_provider.as_deref(),
            Some("openai")
        ); // local only
    }
}
