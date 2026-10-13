use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::core::PermissionPolicy;
use crate::core::checkpoints::project_key;
use crate::core::mcp::{self, McpServer, McpServerSettings};
use crate::core::rules::{Scope, write_atomic};
use crate::core::sandbox;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Config {
    /// Preferred default harness (agy, claude, codex, pi, vibe).
    pub default_harness: Option<String>,

    /// Default permission policy (plan, ask, accept-edits, auto, bypass).
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
    /// `deny_read` one. In a workspace's settings, also a path the global
    /// `deny_read` names, which it lifts for that workspace.
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

/// A `[harnesses.<name>]` table: the keys every harness takes, and in
/// `rest` the ones only its own harness takes (Codex's `transport`, an ACP
/// agent's `modes`, …), which the harness's builder parses
/// ([`HarnessSettings::own`], `core::registry`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HarnessSettings {
    pub binary: Option<PathBuf>,
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
    pub default_policy: Option<String>,
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
    /// Paths this harness may write inside the sandbox, besides the ones
    /// unharness knows it needs (an ACP agent's state, an MCP server's data).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sandbox_writable: Vec<PathBuf>,
    /// Every other key of the table, for its harness to parse.
    #[serde(flatten)]
    pub rest: toml::Table,
}

impl HarnessSettings {
    /// What this table sets that only harness `name` takes: `T`, a struct
    /// deriving `Deserialize` with `deny_unknown_fields`, read from
    /// [`rest`](Self::rest). A key `T` does not take is an error naming the
    /// table and the key, as a value it cannot take is.
    pub fn own<T: DeserializeOwned>(&self, name: &str) -> Result<T> {
        let takes = keys_of::<T>();
        if let Some(key) = self.rest.keys().find(|k| !takes.contains(&k.as_str())) {
            let own = match takes {
                [] => format!("{name} takes only the keys every harness takes"),
                keys => format!(
                    "besides the keys every harness takes, {name} takes {}",
                    keys.join(", ")
                ),
            };
            anyhow::bail!("[harnesses.{name}] {key}: unknown key; {own}");
        }
        T::deserialize(toml::Value::Table(self.rest.clone())).map_err(|e| {
            // toml says which key on a line of its own.
            let what = e.to_string();
            anyhow::anyhow!("[harnesses.{name}]: {}", what.trim_end().replace('\n', " "))
        })
    }
}

/// What a harness that takes only the keys every harness takes has of its
/// own: nothing.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoOwnKeys {}

/// Everything wrong with a harness table that a builder could check
/// independently (an ACP agent's missing command and its mode map), each
/// naming the table, in the order found. The registry reports each one
/// (`core::registry`); anywhere else they read as one, joined.
#[derive(Debug)]
pub struct TableErrors(pub Vec<String>);

impl std::fmt::Display for TableErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join("; "))
    }
}

impl std::error::Error for TableErrors {}

/// The keys a struct deriving `Deserialize` takes: the field names it hands
/// `deserialize_struct`, read by a deserializer that has nothing else.
fn keys_of<T: DeserializeOwned>() -> &'static [&'static str] {
    use serde::de::{self, Visitor};
    struct Keys<'a>(&'a mut &'static [&'static str]);
    impl<'de> de::Deserializer<'de> for Keys<'_> {
        type Error = de::value::Error;
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = fields;
            Err(de::Error::custom("read for its keys only"))
        }
        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not a struct"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }
    let mut keys: &'static [&'static str] = &[];
    let _ = T::deserialize(Keys(&mut keys));
    keys
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
    pub(crate) fn load_file(path: &Path) -> Result<Option<Self>> {
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

    /// The settings file of `scope` under `dir` (`<config dir>/unharness`);
    /// `None` for a workspace's when there is no workspace.
    pub fn scoped_path(dir: &Path, scope: Scope, root: Option<&Path>) -> Option<PathBuf> {
        match scope {
            Scope::Global => Some(dir.join("config.toml")),
            Scope::Workspace => root.map(|r| Self::workspace_path_in(&dir.join("workspaces"), r)),
        }
    }

    /// Set `[harnesses.<harness>] default_policy` in the file at `path`,
    /// leaving the rest of it as it was, comments included. A file that
    /// is a link stays one.
    pub fn set_harness_default_policy(
        path: &Path,
        harness: &str,
        policy: PermissionPolicy,
    ) -> Result<()> {
        // A link to a file not there yet (a dotfiles checkout) would be
        // replaced by a file.
        if path.is_symlink() && !path.exists() {
            anyhow::bail!("{} is a link to a file that does not exist", path.display());
        }
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        let mut doc: toml_edit::DocumentMut = content
            .parse()
            .with_context(|| format!("{} is not valid TOML", path.display()))?;
        let inline = doc
            .get("harnesses")
            .is_some_and(toml_edit::Item::is_inline_table);
        let harnesses = doc
            .entry("harnesses")
            .or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml_edit::Item::Table(t)
            })
            .as_table_like_mut()
            .with_context(|| format!("harnesses in {} is not a table", path.display()))?;
        let created = !harnesses.contains_key(harness);
        // In the form the tables around it have: one written inline would
        // otherwise be left out of an inline table.
        let new = if inline {
            toml_edit::Item::Value(toml_edit::InlineTable::new().into())
        } else {
            toml_edit::Item::Table(toml_edit::Table::new())
        };
        let settings = harnesses
            .entry(harness)
            .or_insert(new)
            .as_table_like_mut()
            .with_context(|| format!("harnesses.{harness} in {} is not a table", path.display()))?;
        settings.insert("default_policy", toml_edit::value(policy.as_str()));
        // Comments at the end of the file would follow a new table: it goes
        // after them instead.
        let trailing = doc.trailing().as_str().unwrap_or_default().to_string();
        if created && !inline && !trailing.trim().is_empty() {
            doc.set_trailing("");
            if let Some(t) = doc["harnesses"][harness].as_table_mut() {
                t.decor_mut().set_prefix(format!("{trailing}\n"));
            }
        }
        let content = doc.to_string();
        // What is written has to read back as a config, with the setting.
        let back: Config = toml::from_str(&content)
            .with_context(|| format!("{} is not a valid unharness config", path.display()))?;
        if back
            .harness(harness)
            .and_then(|h| h.default_policy.as_deref())
            != Some(policy.as_str())
        {
            anyhow::bail!(
                "could not add the setting to {} as it is laid out; add [harnesses.{harness}] \
                 default_policy = \"{policy}\" by hand",
                path.display()
            );
        }
        let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        write_atomic(&target, &content)
            .with_context(|| format!("could not write {}", path.display()))
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
            sandbox: Self::merge_sandbox(
                global.sandbox,
                local.sandbox,
                dirs::home_dir().as_deref(),
            ),
            harnesses,
            mcp_servers,
        }
    }

    /// The lists are joined, except that a global `deny_read` path the
    /// workspace names in `readable` is lifted for that workspace. Only
    /// that way round, and only for the same path as written (`~`
    /// expanded): any other overlap is refused when the sandbox is set up.
    fn merge_sandbox(
        global: SandboxSettings,
        local: SandboxSettings,
        home: Option<&Path>,
    ) -> SandboxSettings {
        let expand = |p: &PathBuf| sandbox::expand_home(p, home);
        let lifted: Vec<PathBuf> = local.readable.iter().map(expand).collect();
        let global_deny = global
            .deny_read
            .into_iter()
            .filter(|d| !lifted.contains(&expand(d)));
        SandboxSettings {
            level: local.level.or(global.level),
            writable: [global.writable, local.writable].concat(),
            readable: [global.readable, local.readable].concat(),
            deny_read: global_deny.chain(local.deny_read).collect(),
        }
    }

    fn merge_settings(global: HarnessSettings, local: HarnessSettings) -> HarnessSettings {
        HarnessSettings {
            binary: local.binary.or(global.binary),
            default_provider: local.default_provider.or(global.default_provider),
            default_model: local.default_model.or(global.default_model),
            default_effort: local.default_effort.or(global.default_effort),
            default_policy: local.default_policy.or(global.default_policy),
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
            sandbox_writable: [global.sandbox_writable, local.sandbox_writable].concat(),
            rest: Self::merge_rest(global.rest, local.rest),
        }
    }

    /// A harness's own keys: a workspace's replaces the global one's,
    /// except a table of tables (vibe's `providers`), whose entries are
    /// merged by name, a workspace's replacing the global one of that name.
    /// So a mode map, whose entries are not tables, is replaced whole: it
    /// is one statement about the agent. An empty table keeps the global
    /// one's, as an empty list does.
    fn merge_rest(global: toml::Table, local: toml::Table) -> toml::Table {
        let mut merged = global;
        for (key, value) in local {
            match (merged.get_mut(&key), value) {
                (Some(toml::Value::Table(global)), toml::Value::Table(local))
                    if local.values().all(toml::Value::is_table) =>
                {
                    global.extend(local)
                }
                (_, value) => {
                    merged.insert(key, value);
                }
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace's provider of the same name replaces the global one;
    /// the others are kept.
    #[test]
    fn providers_merge_by_name() {
        let global: Config = toml::from_str(
            "[harnesses.vibe.providers.lan]\napi_base = \"http://a/v1\"\nmodels = [\"m\"]\n\
             [harnesses.vibe.providers.box]\napi_base = \"http://b/v1\"\nmodels = [\"n\"]\n",
        )
        .unwrap();
        let local: Config = toml::from_str(
            "[harnesses.vibe.providers.lan]\napi_base = \"http://c/v1\"\nmodels = [\"o\"]\n",
        )
        .unwrap();
        let merged = Config::merge(global, local);
        let providers = &merged.harnesses["vibe"].rest["providers"];
        assert_eq!(providers["lan"]["api_base"].as_str(), Some("http://c/v1"));
        assert_eq!(providers["lan"]["models"], toml::Value::from(vec!["o"]));
        assert_eq!(providers["box"]["api_base"].as_str(), Some("http://b/v1"));
    }

    #[test]
    fn a_default_policy_is_set_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("unharness");

        // No file yet: one with just the setting.
        let global = Config::scoped_path(&dir, Scope::Global, None).unwrap();
        Config::set_harness_default_policy(&global, "agy", PermissionPolicy::AcceptEdits).unwrap();
        let written = std::fs::read_to_string(&global).unwrap();
        assert_eq!(
            written,
            "[harnesses.agy]\ndefault_policy = \"accept-edits\"\n"
        );

        // An existing one keeps its comments, order and other settings.
        let before = "# mine\ndefault_policy = \"ask\" # everywhere\n\n\
                      [harnesses.claude]\nbinary = \"/opt/claude\" # pinned\n\
                      default_policy = \"ask\"\n";
        std::fs::write(&global, before).unwrap();
        Config::set_harness_default_policy(&global, "claude", PermissionPolicy::Auto).unwrap();
        Config::set_harness_default_policy(&global, "pi", PermissionPolicy::Bypass).unwrap();
        let after = std::fs::read_to_string(&global).unwrap();
        assert!(after.starts_with("# mine\ndefault_policy = \"ask\" # everywhere\n"));
        assert!(after.contains("binary = \"/opt/claude\" # pinned"));
        let cfg: Config = toml::from_str(&after).unwrap();
        assert_eq!(cfg.default_policy.as_deref(), Some("ask"));
        assert_eq!(
            cfg.harnesses["claude"].default_policy.as_deref(),
            Some("auto")
        );
        assert_eq!(
            cfg.harnesses["pi"].default_policy.as_deref(),
            Some("bypass")
        );

        // A comment at the end stays with the table it follows.
        std::fs::write(
            &global,
            "[harnesses.claude]\nbinary = \"x\"\n# extra_args = []\n",
        )
        .unwrap();
        Config::set_harness_default_policy(&global, "pi", PermissionPolicy::Ask).unwrap();
        assert_eq!(
            std::fs::read_to_string(&global).unwrap(),
            "[harnesses.claude]\nbinary = \"x\"\n# extra_args = []\n\n\
             [harnesses.pi]\ndefault_policy = \"ask\"\n"
        );

        // A workspace's file is under the config directory, never in it.
        let root = tmp.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        let local = Config::scoped_path(&dir, Scope::Workspace, Some(&root)).unwrap();
        assert!(local.starts_with(dir.join("workspaces")));
        assert_eq!(Config::scoped_path(&dir, Scope::Workspace, None), None);
        Config::set_harness_default_policy(&local, "claude", PermissionPolicy::Ask).unwrap();
        let ws = Config::load_workspace_in(&dir.join("workspaces"), &root)
            .unwrap()
            .unwrap();
        assert_eq!(
            ws.harnesses["claude"].default_policy.as_deref(),
            Some("ask")
        );
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_default_policy_is_written_through_a_link_keeping_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("dotfiles/config.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "# secrets below\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = tmp.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        Config::set_harness_default_policy(&link, "pi", PermissionPolicy::Ask).unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        let content = std::fs::read_to_string(&real).unwrap();
        assert_eq!(
            content,
            "# secrets below\n\n[harnesses.pi]\ndefault_policy = \"ask\"\n"
        );
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_default_policy_goes_into_an_inline_table_inline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "harnesses = { claude = { binary = \"/opt/claude\" } }\n# tail\n",
        )
        .unwrap();
        Config::set_harness_default_policy(&path, "pi", PermissionPolicy::Bypass).unwrap();
        Config::set_harness_default_policy(&path, "claude", PermissionPolicy::Auto).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.ends_with("# tail\n"), "{after}");
        let cfg: Config = toml::from_str(&after).unwrap();
        assert_eq!(
            cfg.harnesses["pi"].default_policy.as_deref(),
            Some("bypass")
        );
        assert_eq!(
            cfg.harnesses["claude"].default_policy.as_deref(),
            Some("auto")
        );
        assert_eq!(
            cfg.harnesses["claude"].binary.as_deref(),
            Some(Path::new("/opt/claude"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_missing_file_is_not_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("config.toml");
        std::os::unix::fs::symlink(tmp.path().join("dotfiles/config.toml"), &link).unwrap();
        let e = Config::set_harness_default_policy(&link, "pi", PermissionPolicy::Ask).unwrap_err();
        assert!(e.to_string().contains("does not exist"), "{e}");
        assert!(link.is_symlink());
    }

    #[test]
    fn a_config_that_cannot_take_the_setting_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        for content in ["harnesses = 3\n", "[harnesses]\npi = \"x\"\n", "not toml ["] {
            std::fs::write(&path, content).unwrap();
            assert!(
                Config::set_harness_default_policy(&path, "pi", PermissionPolicy::Ask).is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }

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
    fn a_workspace_readable_lifts_only_a_global_deny_it_names() {
        let home = Path::new("/home/u");
        let lists = |readable: &[&str], deny_read: &[&str]| SandboxSettings {
            readable: readable.iter().map(PathBuf::from).collect(),
            deny_read: deny_read.iter().map(PathBuf::from).collect(),
            ..Default::default()
        };
        let merged = Config::merge_sandbox(
            lists(
                &[],
                &["~/.config/demo/push.token", "~/.config/demo/other.token"],
            ),
            lists(&["/home/u/.config/demo/push.token"], &[]),
            Some(home),
        );
        assert_eq!(
            merged.deny_read,
            [PathBuf::from("~/.config/demo/other.token")]
        );
        assert_eq!(
            merged.readable,
            [PathBuf::from("/home/u/.config/demo/push.token")]
        );

        // Not a directory around it, not a global readable over a
        // workspace deny, not both in one file: those stay for the
        // sandbox to refuse.
        for (global, local) in [
            (
                lists(&[], &["~/.config/demo/push.token"]),
                lists(&["~/.config/demo"], &[]),
            ),
            (lists(&["~/.config/gh"], &[]), lists(&[], &["~/.config/gh"])),
            (lists(&[], &[]), lists(&["~/.config/gh"], &["~/.config/gh"])),
        ] {
            let denied = [global.deny_read.clone(), local.deny_read.clone()].concat();
            assert_eq!(
                Config::merge_sandbox(global, local, Some(home)).deny_read,
                denied
            );
        }
    }

    /// `modes` is a table of policy → agent mode, and a workspace's
    /// replaces the global one whole.
    #[test]
    fn an_acp_agent_s_modes_are_one_table() {
        let global: Config = toml::from_str(
            "[harnesses.vibe]\nprotocol = \"acp\"\ncommand = [\"vibe-acp\"]\n\
             modes = { ask = \"ask\", accept-edits = \"ask\", bypass = \"auto-approve\" }\n",
        )
        .unwrap();
        let modes = |c: &Config| {
            c.harnesses["vibe"].rest["modes"]
                .as_table()
                .unwrap()
                .clone()
        };
        assert_eq!(modes(&global).len(), 3);
        assert_eq!(modes(&global)["accept-edits"].as_str(), Some("ask"));
        let local: Config =
            toml::from_str("[harnesses.vibe]\nmodes = { ask = \"ask\" }\n").unwrap();
        let merged = Config::merge(global.clone(), local);
        assert_eq!(modes(&merged).len(), 1);
        let merged = Config::merge(global, Config::default());
        assert_eq!(modes(&merged).len(), 3);
    }

    /// What only one harness takes is kept in `rest`, written back as it
    /// was read, and read by that harness: a key it does not take, or a
    /// value it cannot, is an error naming the table and the key (#241).
    #[test]
    fn a_harness_s_own_keys_are_its_to_read() {
        #[derive(Debug, Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Own {
            transport: Option<String>,
            #[serde(default)]
            modes: BTreeMap<String, String>,
        }
        let text = "[harnesses.x]\ndefault_model = \"m\"\ntransport = \"exec\"\n\
                    modes = { ask = \"ask\" }\n[harnesses.x.providers.lan]\nmodels = [\"a\"]\n";
        let cfg: Config = toml::from_str(text).unwrap();
        let x = &cfg.harnesses["x"];
        assert_eq!(x.default_model.as_deref(), Some("m"));
        assert_eq!(
            x.rest.keys().collect::<Vec<_>>(),
            ["modes", "providers", "transport"]
        );
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);

        let own = |text: &str| {
            let cfg: Config = toml::from_str(text).unwrap();
            cfg.harnesses["x"].own::<Own>("x")
        };
        let read = own("[harnesses.x]\ntransport = \"exec\"\nmodes = { ask = \"ask\" }\n").unwrap();
        assert_eq!(read.transport.as_deref(), Some("exec"));
        assert_eq!(read.modes["ask"], "ask");
        assert_eq!(
            own("[harnesses.x]\ntransprt = \"exec\"\n")
                .unwrap_err()
                .to_string(),
            "[harnesses.x] transprt: unknown key; besides the keys every harness takes, \
             x takes transport, modes"
        );
        let e = own("[harnesses.x]\ntransport = 3\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("[harnesses.x]: invalid type: integer `3`") && e.contains("`transport`"),
            "{e}"
        );
        let e = cfg.harnesses["x"].own::<NoOwnKeys>("x").unwrap_err();
        assert_eq!(
            e.to_string(),
            "[harnesses.x] modes: unknown key; x takes only the keys every harness takes"
        );
        assert!(HarnessSettings::default().own::<NoOwnKeys>("x").is_ok());
    }

    /// A workspace's table of the same name merges its harness's own keys
    /// as the rest: one by one, a table of tables by name, and nothing
    /// dropped on the way, so the harness reads what either file says.
    #[test]
    fn a_harness_s_own_keys_merge_key_by_key() {
        let global: Config = toml::from_str(
            "[harnesses.x]\ntransport = \"exec\"\nmodes = { ask = \"a\", bypass = \"b\" }\n",
        )
        .unwrap();
        let local: Config =
            toml::from_str("[harnesses.x]\ntransprt = \"auto\"\nmodes = {}\n").unwrap();
        let merged = Config::merge(global, local);
        let x = &merged.harnesses["x"].rest;
        assert_eq!(
            x.keys().collect::<Vec<_>>(),
            ["modes", "transport", "transprt"]
        );
        assert_eq!(x["modes"].as_table().unwrap().len(), 2);
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
                rest: toml::toml! { transport = "exec" },
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
            merged.harness("codex").unwrap().rest["transport"].as_str(),
            Some("exec")
        ); // global only
        assert_eq!(
            merged.harness("pi").unwrap().default_provider.as_deref(),
            Some("openai")
        ); // local only
    }
}
