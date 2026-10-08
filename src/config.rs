use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::checkpoints::project_key;
use crate::core::mcp::{self, McpServer, McpServerSettings};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Config {
    /// Preferred default harness (agy, claude, codex, pi).
    pub default_harness: Option<String>,

    /// Default permission policy (ask, accept-edits, auto, bypass).
    pub default_policy: Option<String>,

    /// Whether to synchronize rules symlinks before running. Default: on.
    pub auto_sync: Option<bool>,

    /// Maximum characters of transcript bridged into a new harness session on switch.
    pub bridge_max_chars: Option<usize>,

    /// Ask the harness the user switches away from for a handoff summary
    /// when the conversation will not fit in the bridge. Default: auto.
    pub bridge_summary: Option<BridgeSummary>,

    /// Checkpoint the working tree before each prompt (git repositories
    /// only) so `/rewind` can restore files. Default: on.
    pub file_checkpoints: Option<bool>,

    /// Take the mouse in the TUI: the wheel scrolls the transcript and
    /// dragging selects and copies text. Off leaves the mouse to the
    /// terminal. Default: on.
    pub mouse: Option<bool>,

    /// Report the session's state to herdr when running in one of its
    /// panes (`HERDR_ENV=1`). Default: on.
    pub herdr: Option<bool>,

    /// The OS-level sandbox around every harness process.
    #[serde(default, skip_serializing_if = "SandboxSettings::is_empty")]
    pub sandbox: SandboxSettings,

    /// Harness-specific settings keyed by harness id.
    #[serde(default)]
    pub harnesses: HashMap<String, HarnessSettings>,

    /// MCP servers by name, passed for the session to every harness that
    /// can take them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerSettings>,
}

/// When a harness is asked for a handoff summary on a switch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BridgeSummary {
    /// When the bridge to the next harness would be over its budget.
    #[default]
    Auto,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SandboxSettings {
    /// read-only, workspace-write or off. Default: workspace-write wherever
    /// the platform has a sandbox.
    pub level: Option<String>,
    /// Paths a harness may write besides the workspace and its own state.
    /// `~` is the home directory; a relative path is under the workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<PathBuf>,
    /// Credential paths a harness may read although they are denied by
    /// default (e.g. `~/.config/gh`), and paths that stay readable inside a
    /// `deny_read` one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readable: Vec<PathBuf>,
    /// More paths no harness may read, besides the built-in credential
    /// locations. The workspace and the harness's own state stay readable
    /// inside them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_read: Vec<PathBuf>,
}

impl SandboxSettings {
    fn is_empty(&self) -> bool {
        *self == SandboxSettings::default()
    }
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
    /// ACP only: this agent asks permission before it writes, runs a
    /// command or reaches out, so the `ask` policy can be offered for it.
    /// unharness cannot check that; set it for an agent seen to do so.
    pub asks_permission: Option<bool>,
    /// Claude Code only: keep `.claude.json` inside `~/.claude` (by setting
    /// `CLAUDE_CONFIG_DIR`), where the sandbox lets Claude update it. The
    /// existing `~/.claude.json` is copied there once. Default: on.
    pub relocate_config: Option<bool>,
    /// Paths this harness may write inside the sandbox, besides the ones
    /// unharness knows it needs (an ACP agent's state, an MCP server's data).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sandbox_writable: Vec<PathBuf>,
}

impl Config {
    /// The global config with the workspace's overrides merged over it.
    ///
    /// Both live under the user's config directory. Nothing is read from the
    /// workspace itself: an agent can write there, and a config it edited
    /// would decide its own sandbox and permissions on the next run.
    ///
    /// A file that does not parse is an error, not an empty config: the
    /// defaults it would silently fall back to include the sandbox level
    /// and the permission policy.
    pub fn load_effective(workspace_root: Option<&Path>) -> Result<Self> {
        let global_config = Self::load_global()?;
        let workspace_config = match workspace_root.zip(Self::workspace_store()) {
            Some((root, store)) => Self::load_workspace_in(&store, root)?,
            None => None,
        };
        Ok(match workspace_config {
            Some(local) => Self::merge(global_config, local),
            None => global_config,
        })
    }

    /// Read and parse a config file; `None` when there is none.
    fn load_file(path: &Path) -> Result<Option<Self>> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        toml::from_str(&content)
            .map(Some)
            .with_context(|| format!("{} is not a valid unharness config", path.display()))
    }

    /// Where workspace overrides are kept: `<config dir>/unharness/workspaces`.
    pub fn workspace_store() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("unharness").join("workspaces"))
    }

    /// The overrides file for the workspace at `root`, under `store`.
    pub fn workspace_path_in(store: &Path, root: &Path) -> PathBuf {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        store.join(format!("{}.toml", project_key(&root)))
    }

    pub fn load_workspace_in(store: &Path, root: &Path) -> Result<Option<Self>> {
        Self::load_file(&Self::workspace_path_in(store, root))
    }

    pub fn save_workspace_in(&self, store: &Path, root: &Path) -> Result<PathBuf> {
        let path = Self::workspace_path_in(store, root);
        std::fs::create_dir_all(store)?;
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(path)
    }

    pub fn global_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("unharness").join("config.toml"))
    }

    pub fn load_global() -> Result<Self> {
        match Self::global_path() {
            Some(path) => Ok(Self::load_file(&path)?.unwrap_or_default()),
            None => Ok(Config::default()),
        }
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

    pub fn auto_sync(&self) -> bool {
        self.auto_sync.unwrap_or(true)
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

    /// The enabled MCP servers, and what is wrong with the entries left out.
    pub fn mcp_servers(&self) -> (Vec<McpServer>, Vec<String>) {
        mcp::resolve(&self.mcp_servers)
    }

    pub fn merge(global: Self, local: Self) -> Self {
        // A workspace's table of the same name replaces the global one.
        let mut mcp_servers = global.mcp_servers;
        mcp_servers.extend(local.mcp_servers);
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
            auto_sync: local.auto_sync.or(global.auto_sync),
            bridge_max_chars: local.bridge_max_chars.or(global.bridge_max_chars),
            bridge_summary: local.bridge_summary.or(global.bridge_summary),
            file_checkpoints: local.file_checkpoints.or(global.file_checkpoints),
            mouse: local.mouse.or(global.mouse),
            herdr: local.herdr.or(global.herdr),
            sandbox: SandboxSettings {
                level: local.sandbox.level.or(global.sandbox.level),
                writable: [global.sandbox.writable, local.sandbox.writable].concat(),
                readable: [global.sandbox.readable, local.sandbox.readable].concat(),
                deny_read: [global.sandbox.deny_read, local.sandbox.deny_read].concat(),
            },
            harnesses,
            mcp_servers,
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
            asks_permission: local.asks_permission.or(global.asks_permission),
            relocate_config: local.relocate_config.or(global.relocate_config),
            sandbox_writable: [global.sandbox_writable, local.sandbox_writable].concat(),
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
        assert!(cfg.auto_sync());
        assert!(cfg.harness("claude").is_none());
        assert!(cfg.extra_args("claude").is_empty());
    }

    #[test]
    fn test_toml_roundtrip() {
        let toml_str = r#"
default_harness = "pi"
default_policy = "accept-edits"
bridge_max_chars = 1000
bridge_summary = "never"

[sandbox]
level = "workspace-write"
readable = ["~/.ssh"]

[harnesses.pi]
default_provider = "anthropic"
default_model = "claude-sonnet-4-5"
transport = "rpc"

[mcp_servers.files]
command = "npx"
args = ["-y", "server-filesystem"]

[mcp_servers.files.env]
TOKEN = "t"

[mcp_servers.docs]
url = "https://example.com/mcp"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg, deserialized);
        assert_eq!(cfg.bridge_summary, Some(BridgeSummary::Never));
        assert_eq!(
            cfg.harness("pi").unwrap().default_provider.as_deref(),
            Some("anthropic")
        );
    }

    #[test]
    fn workspace_overrides_live_outside_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, root) = (tmp.path().join("store"), tmp.path().join("proj"));
        std::fs::create_dir_all(&root).unwrap();
        assert!(Config::load_workspace_in(&store, &root).unwrap().is_none());

        let cfg = Config {
            default_harness: Some("pi".into()),
            ..Default::default()
        };
        let path = cfg.save_workspace_in(&store, &root).unwrap();
        assert!(path.starts_with(&store));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("proj-") && name.ends_with(".toml"),
            "{name}"
        );
        assert_eq!(
            Config::load_workspace_in(&store, &root)
                .unwrap()
                .unwrap()
                .default_harness
                .as_deref(),
            Some("pi")
        );

        // A file in the workspace is never loaded.
        std::fs::write(root.join("unharness.toml"), "default_harness = \"codex\"\n").unwrap();
        assert_eq!(
            Config::load_workspace_in(&store, &root)
                .unwrap()
                .unwrap()
                .default_harness
                .as_deref(),
            Some("pi")
        );
    }

    #[test]
    fn a_config_that_does_not_parse_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, root) = (tmp.path().join("store"), tmp.path().join("proj"));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let path = Config::workspace_path_in(&store, &root);
        // A number where a string belongs, in a table that is easy to get wrong.
        std::fs::write(
            &path,
            "[sandbox]\nlevel = \"read-only\"\n[mcp_servers.x]\ncommand = \"x\"\nenv = { PORT = 8080 }\n",
        )
        .unwrap();
        let error = format!(
            "{:#}",
            Config::load_workspace_in(&store, &root).unwrap_err()
        );
        assert!(
            error.contains(&path.display().to_string()) && error.contains("PORT"),
            "{error}"
        );
    }

    #[test]
    fn auto_sync_is_on_unless_a_file_says_otherwise() {
        let parse = |text: &str| toml::from_str::<Config>(text).unwrap();
        assert!(Config::default().auto_sync());
        assert!(parse("").auto_sync());
        // A workspace file that does not mention it keeps the global value.
        let off = || parse("auto_sync = false\n");
        assert!(!Config::merge(off(), parse("[sandbox]\n")).auto_sync());
        assert!(Config::merge(off(), parse("auto_sync = true\n")).auto_sync());
        assert!(!Config::merge(parse(""), off()).auto_sync());
        // Saving a config that never set it does not write it.
        let saved = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!saved.contains("auto_sync"), "{saved}");
    }

    #[test]
    fn test_config_merge() {
        let mut global = Config {
            default_harness: Some("claude".to_string()),
            default_policy: Some("ask".to_string()),
            auto_sync: Some(false),
            sandbox: SandboxSettings {
                level: Some("off".to_string()),
                writable: vec![PathBuf::from("~/.cache/a")],
                readable: vec![],
                deny_read: vec![],
            },
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
            sandbox: SandboxSettings {
                level: Some("read-only".to_string()),
                writable: vec![PathBuf::from("target-shared")],
                readable: vec![],
                deny_read: vec![],
            },
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

        let server = |command: Option<&str>, enabled: Option<bool>| McpServerSettings {
            command: command.map(str::to_string),
            enabled,
            ..Default::default()
        };
        for name in ["docs", "files"] {
            let old = server(Some("old"), None);
            global.mcp_servers.insert(name.into(), old);
        }
        let off = server(Some("old"), Some(false));
        local.mcp_servers.insert("docs".into(), off);
        for name in ["files", "local"] {
            let new = server(Some("new"), None);
            local.mcp_servers.insert(name.into(), new);
        }

        let merged = Config::merge(global, local);
        let names: Vec<&String> = merged.mcp_servers.keys().collect();
        assert_eq!(names, ["docs", "files", "local"]);
        // A workspace can switch a global server off, or redefine it.
        assert_eq!(merged.mcp_servers["docs"].enabled, Some(false));
        assert_eq!(merged.mcp_servers["files"].command.as_deref(), Some("new"));
        let (servers, problems) = merged.mcp_servers();
        assert_eq!(servers.len(), 2);
        assert!(problems.is_empty());
        assert_eq!(merged.sandbox.level.as_deref(), Some("read-only")); // local wins
        assert_eq!(
            merged.sandbox.writable,
            vec![PathBuf::from("~/.cache/a"), PathBuf::from("target-shared")]
        ); // both
        assert_eq!(merged.default_harness.as_deref(), Some("agy")); // local wins
        assert_eq!(merged.default_policy.as_deref(), Some("ask")); // inherited
        assert!(!merged.auto_sync()); // inherited
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
