//! Mistral Vibe (`vibe-acp`, PyPI `mistral-vibe`), driven over the Agent
//! Client Protocol by [`AcpHarness`] with what unharness knows about it:
//! the session modes that back each policy, its reasoning level as the
//! option `thinking`, and its model and level chosen by environment when
//! it starts.
//!
//! Checked on vibe 2.26.1. `session/set_config_option` for `model` or
//! `thinking` writes the choice into the user's `config.toml`
//! (`config.update({"active_model": …})` and `set_thinking` in
//! `vibe/acp/agent.py`), so unharness never sends either: the process is
//! started with `VIBE_ACTIVE_MODEL=<alias>` and, for a level, a whole model
//! entry in `VIBE_MODELS`, which vibe reads as a configuration layer above
//! both TOML files and below an agent profile and admin config, and never
//! writes (`vibe/core/config/layers/environment.py`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;

use super::acp::{AcpHarness, ModeMap, SpawnChoice, SpawnPlan};
use super::{Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe};
use crate::config::HarnessSettings;
use crate::core::guard::Guarded;
use crate::core::sandbox::{Sandbox, SandboxLevel, SandboxPaths};
use crate::core::{
    Capabilities, HarnessCommand, HarnessId, PolicySupport, ProviderId, SessionConfig,
    SessionHandle,
};

/// The command unharness runs when the config names none.
pub const COMMAND: &str = "vibe-acp";
const DISPLAY_NAME: &str = "Mistral Vibe";

/// The option that is vibe's reasoning level (category `thinking`, which
/// the spec does not name).
pub const EFFORT_OPTION: &str = "thinking";

/// vibe's levels (`ThinkingLevel` in `vibe/core/config/models.py`). A model
/// offers some of them; one outside the set fails the environment layer,
/// and with it the session, so nothing else is put in `VIBE_MODELS`.
pub const THINKING_LEVELS: [&str; 5] = ["off", "low", "medium", "high", "max"];

/// The provider of a model that the user's `config.toml` does not define:
/// vibe's built-in models and the ones its experiments add are all
/// `mistral` (`DEFAULT_MODELS`, `vibe/core/config/vibe_schema.py`).
const DEFAULT_PROVIDER: &str = "mistral";

/// The most read of vibe's `config.toml`.
const CONFIG_LIMIT: u64 = 1 << 20;

/// `ask` is vibe's mode that asks before every write and command; under
/// `accept-edits` the agent stays in it and unharness answers the edits.
/// `bypass` is `auto-approve`. (`fixtures/vibe_acp_mode*.jsonl`.)
fn mode_map() -> ModeMap {
    ModeMap::new([
        (crate::core::PermissionPolicy::Ask, "ask"),
        (crate::core::PermissionPolicy::Bypass, "auto-approve"),
    ])
}

pub struct VibeHarness {
    acp: AcpHarness,
}

impl Default for VibeHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl VibeHarness {
    pub fn new() -> Self {
        Self::build(&[COMMAND.to_string()], None, true, mode_map())
    }

    /// The built-in harness with what a `[harnesses.vibe]` table sets in
    /// its place: the command, the name shown, `asks_permission` and the
    /// mode map. The model and effort are chosen at spawn whatever it says.
    pub fn from_settings(settings: &HarnessSettings) -> Result<Self> {
        let command = if settings.command.is_empty() {
            vec![COMMAND.to_string()]
        } else {
            settings.command.clone()
        };
        let modes = if settings.modes.is_empty() {
            mode_map()
        } else {
            ModeMap::from_config(&settings.modes).map_err(anyhow::Error::msg)?
        };
        let vibe = Self::build(
            &command,
            settings.display_name.as_deref(),
            settings.asks_permission.unwrap_or(true),
            modes,
        );
        // `sign_in` in the table replaces vibe's own command.
        Ok(match settings.sign_in.as_deref() {
            Some(command) => VibeHarness {
                acp: vibe.acp.signing_in_with(Some(command)),
            },
            None => vibe,
        })
    }

    fn build(command: &[String], display: Option<&str>, asks: bool, modes: ModeMap) -> Self {
        let acp = AcpHarness::new(
            HarnessId::VIBE.as_str(),
            Some(display.unwrap_or(DISPLAY_NAME)),
            command,
        )
        // The command is never empty here.
        .expect("a command")
        .asking_permission(asks)
        .with_modes(modes)
        .declaring_effort_option(EFFORT_OPTION)
        // Told when vibe is not signed in, never run (`acp/auth.rs`).
        .signing_in_with(Some("vibe --setup"))
        .choosing_at_spawn(Arc::new(|choice: &SpawnChoice<'_>| {
            spawn_env(&vibe_home(choice.env), choice)
        }));
        VibeHarness { acp }
    }
}

/// `$VIBE_HOME` (from `env`, the session's additions, else unharness's
/// own environment), else `~/.vibe`: where vibe keeps its configuration,
/// sessions and logs (`vibe/utils/vibe_home.py`).
pub fn vibe_home(env: &[(String, String)]) -> PathBuf {
    let set = env
        .iter()
        .rev()
        .find(|(k, _)| k == "VIBE_HOME")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("VIBE_HOME").ok())
        .filter(|v| !v.is_empty());
    let home = dirs::home_dir();
    match set {
        Some(dir) => crate::core::sandbox::expand_home(Path::new(&dir), home.as_deref()),
        None => home.unwrap_or_default().join(".vibe"),
    }
}

/// A model the user defined in vibe's `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct UserModel {
    name: Option<String>,
    provider: Option<String>,
}

/// The entry for `alias` in `<home>/config.toml`, read only: `[[models]]`
/// tables, whose alias defaults to their name, or `[models.<alias>]`
/// (vibe reads both, `normalize_model_configs`). Nothing when the file is
/// missing, not a regular file, too large or not TOML.
fn user_model(home: &Path, alias: &str) -> UserModel {
    let Some(text) = crate::sync::open_regular(&home.join("config.toml"), true)
        .and_then(|f| crate::sync::read_at_most(&f, CONFIG_LIMIT))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
    else {
        return UserModel::default();
    };
    let Ok(config) = text.parse::<toml::Table>() else {
        return UserModel::default();
    };
    let field = |t: &toml::Table, key: &str| t.get(key).and_then(|v| v.as_str()).map(String::from);
    let entry = match config.get("models") {
        Some(toml::Value::Array(list)) => list
            .iter()
            .filter_map(|v| v.as_table())
            .find(|t| field(t, "alias").or_else(|| field(t, "name")).as_deref() == Some(alias)),
        Some(toml::Value::Table(map)) => map.get(alias).and_then(|v| v.as_table()),
        _ => None,
    };
    entry
        .map(|t| UserModel {
            name: field(t, "name"),
            provider: field(t, "provider"),
        })
        .unwrap_or_default()
}

/// vibe's environment for a model and thinking level, with `home` its
/// configuration directory. The model goes in `VIBE_ACTIVE_MODEL`. A
/// level goes in a whole entry for the model in `VIBE_MODELS`, the only
/// form vibe 2.26.1 took: its environment layer validates each entry as a
/// full model (`name`, `provider`, `alias` required; one without fails
/// the session), and merges only the fields given over the model's own.
/// So the entry needs the model's name, which a session gives as the
/// model option's description, and its provider, which only the user's
/// `config.toml` can say (else `mistral`). Only the chosen model is
/// listed, and the JSON is built, never pasted together. With a level and
/// no model chosen, the model the last session ran is kept, since the
/// level goes with it.
pub fn spawn_env(home: &Path, choice: &SpawnChoice<'_>) -> SpawnPlan {
    let mut plan = SpawnPlan::default();
    if let Some(model) = choice.model {
        plan.env
            .push(("VIBE_ACTIVE_MODEL".to_string(), model.to_string()));
    }
    let Some(level) = choice.effort else {
        return plan;
    };
    if !THINKING_LEVELS.contains(&level) {
        plan.unmet = Some(format!(
            "vibe has no thinking level '{level}'; it has {}",
            THINKING_LEVELS.join(", ")
        ));
        return plan;
    }
    let Some(alias) = choice.model.or(choice.known.default.as_deref()) else {
        plan.unmet = Some("vibe has not said yet which model it runs".to_string());
        return plan;
    };
    if choice.model.is_none() {
        // vibe's default can change between two of its processes (a
        // remote experiment's choice arrives with the first, 2.26.1): the
        // level is for the model it ran, which is kept.
        plan.env
            .push(("VIBE_ACTIVE_MODEL".to_string(), alias.to_string()));
    }
    let user = user_model(home, alias);
    let Some(name) = choice.known.descriptions.get(alias).cloned().or(user.name) else {
        plan.unmet = Some(format!("vibe has not said yet what model '{alias}' is"));
        return plan;
    };
    let provider = user
        .provider
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
    let entry = json!([{
        "alias": alias,
        "name": name,
        "provider": provider,
        "thinking": level,
    }]);
    plan.env
        .push(("VIBE_MODELS".to_string(), entry.to_string()));
    plan.overrides = Some(alias.to_string());
    plan
}

impl Harness for VibeHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        self.acp.descriptor()
    }

    fn capabilities(&self) -> Capabilities {
        self.acp.capabilities()
    }

    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        self.acp.probe(binary_override)
    }

    fn reports_version(&self) -> bool {
        self.acp.reports_version()
    }

    fn list_models(
        &self,
        binary: &Path,
        provider: &ProviderId,
        sandbox: &Sandbox,
    ) -> Result<Vec<ModelInfo>> {
        self.acp.list_models(binary, provider, sandbox)
    }

    fn list_commands(
        &self,
        binary: &Path,
        cwd: &Path,
        sandbox: &Sandbox,
    ) -> Result<Option<Vec<HarnessCommand>>> {
        self.acp.list_commands(binary, cwd, sandbox)
    }

    /// Everything vibe writes outside the workspace is under its home
    /// (logs, session journals, caches, locks: an strace of a turn on
    /// 2.26.1, from the discovery for #163).
    fn sandbox_paths(&self) -> SandboxPaths {
        let mut paths = self.acp.sandbox_paths();
        paths.writable.push(match std::env::var_os("VIBE_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => Path::new("~").join(".vibe"),
        });
        paths
    }

    fn guarded(&self, workspace: &Path) -> Vec<Guarded> {
        self.acp.guarded(workspace)
    }

    fn own_mcp_servers(&self) -> Vec<String> {
        self.acp.own_mcp_servers()
    }

    fn print_policies(&self, print_mode: bool) -> Vec<PolicySupport> {
        self.acp.print_policies(print_mode)
    }

    fn default_sandbox(&self) -> SandboxLevel {
        self.acp.default_sandbox()
    }

    fn file_reference(&self, path: &str) -> String {
        self.acp.file_reference(path)
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        self.acp.start_session(cfg)
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<std::process::Command> {
        self.acp.build_print_command(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::PermissionPolicy;
    use crate::harness::acp::KnownModels;

    fn choice<'a>(
        model: Option<&'a str>,
        effort: Option<&'a str>,
        known: &'a KnownModels,
    ) -> SpawnChoice<'a> {
        SpawnChoice {
            model,
            effort,
            known,
            env: &[],
        }
    }

    fn known() -> KnownModels {
        KnownModels {
            descriptions: [
                ("mistral-medium-3.5", "mistral-vibe-cli-latest"),
                ("glm-5-3", "zai-glm-5-3"),
            ]
            .into_iter()
            .map(|(a, n)| (a.to_string(), n.to_string()))
            .collect(),
            default: Some("glm-5-3".into()),
        }
    }

    fn var<'a>(plan: &'a SpawnPlan, key: &str) -> Option<&'a str> {
        plan.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Without a key vibe-acp 2.26.1 fails `session/new`
    /// (`acp/fixtures/vibe_acp_not_signed_in.jsonl`): unharness says to run
    /// `vibe --setup`, or what a `[harnesses.vibe] sign_in` names.
    #[test]
    fn its_sign_in_command_is_vibe_setup_unless_the_table_names_one() {
        let sign_in = |h: &VibeHarness| h.acp.sign_in().map(str::to_string);
        assert_eq!(
            sign_in(&VibeHarness::new()).as_deref(),
            Some("vibe --setup")
        );
        let h = VibeHarness::from_settings(&HarnessSettings::default()).unwrap();
        assert_eq!(sign_in(&h).as_deref(), Some("vibe --setup"));
        let table = HarnessSettings {
            sign_in: Some("vibe-acp --setup".into()),
            ..Default::default()
        };
        let h = VibeHarness::from_settings(&table).unwrap();
        assert_eq!(sign_in(&h).as_deref(), Some("vibe-acp --setup"));
    }

    #[test]
    fn a_model_alone_is_the_active_model() {
        let home = tempfile::tempdir().unwrap();
        let k = known();
        let plan = spawn_env(home.path(), &choice(Some("mistral-medium-3.5"), None, &k));
        assert_eq!(
            plan.env,
            [(
                "VIBE_ACTIVE_MODEL".to_string(),
                "mistral-medium-3.5".to_string()
            )]
        );
        assert_eq!(plan.overrides, None);
        assert_eq!(plan.unmet, None);
        // Nothing chosen, nothing set.
        assert_eq!(
            spawn_env(home.path(), &choice(None, None, &k)),
            SpawnPlan::default()
        );
    }

    #[test]
    fn a_level_is_a_whole_entry_for_the_model() {
        let home = tempfile::tempdir().unwrap();
        let k = known();
        let plan = spawn_env(
            home.path(),
            &choice(Some("mistral-medium-3.5"), Some("off"), &k),
        );
        assert_eq!(var(&plan, "VIBE_ACTIVE_MODEL"), Some("mistral-medium-3.5"));
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(
            models,
            json!([{"alias": "mistral-medium-3.5", "name": "mistral-vibe-cli-latest",
                    "provider": "mistral", "thinking": "off"}])
        );
        assert_eq!(plan.overrides.as_deref(), Some("mistral-medium-3.5"));
        assert_eq!(plan.unmet, None);

        // Without a model chosen, the entry is for the one vibe ran, which
        // is kept: vibe's default can move between processes (an
        // experiment's choice arriving), and the level is that model's.
        let plan = spawn_env(home.path(), &choice(None, Some("low"), &k));
        assert_eq!(var(&plan, "VIBE_ACTIVE_MODEL"), Some("glm-5-3"));
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(models[0]["alias"], "glm-5-3");
        assert_eq!(models[0]["name"], "zai-glm-5-3");
        assert_eq!(models[0]["thinking"], "low");
    }

    #[test]
    fn a_level_waits_for_what_only_a_session_says() {
        let home = tempfile::tempdir().unwrap();
        let none = KnownModels::default();
        let plan = spawn_env(home.path(), &choice(None, Some("low"), &none));
        assert!(plan.unmet.as_deref().unwrap().contains("which model"));
        assert_eq!(var(&plan, "VIBE_MODELS"), None);

        let plan = spawn_env(home.path(), &choice(Some("glm-5-3"), Some("low"), &none));
        assert!(plan.unmet.as_deref().unwrap().contains("'glm-5-3'"));
        // The model still goes.
        assert_eq!(var(&plan, "VIBE_ACTIVE_MODEL"), Some("glm-5-3"));
        assert_eq!(var(&plan, "VIBE_MODELS"), None);
    }

    #[test]
    fn a_level_vibe_does_not_have_is_never_sent() {
        let home = tempfile::tempdir().unwrap();
        let k = known();
        let plan = spawn_env(home.path(), &choice(None, Some("xhigh"), &k));
        assert!(plan.unmet.as_deref().unwrap().contains("'xhigh'"));
        assert_eq!(var(&plan, "VIBE_MODELS"), None);
        for level in THINKING_LEVELS {
            assert!(
                spawn_env(home.path(), &choice(None, Some(level), &k))
                    .unmet
                    .is_none()
            );
        }
    }

    #[test]
    fn the_provider_comes_from_the_users_config() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        std::fs::write(
            &config,
            r#"
active_model = "local"

[[models]]
name = "qwen3-coder"
provider = "llamacpp"
alias = "local"

[[models]]
name = "devstral"
provider = "mine"
"#,
        )
        .unwrap();
        let before = std::fs::read(&config).unwrap();
        let none = KnownModels::default();
        // Name and provider from the file: no session needed.
        let plan = spawn_env(home.path(), &choice(Some("local"), Some("high"), &none));
        assert_eq!(plan.unmet, None);
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(
            models,
            json!([{"alias": "local", "name": "qwen3-coder", "provider": "llamacpp",
                    "thinking": "high"}])
        );
        // An entry without an alias goes by its name.
        let plan = spawn_env(home.path(), &choice(Some("devstral"), Some("low"), &none));
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(models[0]["provider"], "mine");
        // What the session said of the name wins over the file.
        let mut k = KnownModels::default();
        k.descriptions
            .insert("local".into(), "qwen3-coder-30b".into());
        let plan = spawn_env(home.path(), &choice(Some("local"), Some("high"), &k));
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(models[0]["name"], "qwen3-coder-30b");
        assert_eq!(models[0]["provider"], "llamacpp");
        // A model the file does not define is mistral's.
        let plan = spawn_env(home.path(), &choice(Some("glm-5-3"), Some("low"), &known()));
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(models[0]["provider"], "mistral");
        // Read, never written.
        assert_eq!(std::fs::read(&config).unwrap(), before);
    }

    #[test]
    fn models_as_a_map_and_unreadable_configs() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        std::fs::write(
            &config,
            "[models.fast]\nname = \"m-small\"\nprovider = \"other\"\n",
        )
        .unwrap();
        let none = KnownModels::default();
        let plan = spawn_env(home.path(), &choice(Some("fast"), Some("off"), &none));
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(models[0]["name"], "m-small");
        assert_eq!(models[0]["provider"], "other");

        // Not TOML: as if there were no file.
        std::fs::write(&config, "models = [").unwrap();
        let plan = spawn_env(home.path(), &choice(Some("fast"), Some("off"), &none));
        assert!(plan.unmet.is_some());
        let plan = spawn_env(home.path(), &choice(Some("glm-5-3"), Some("off"), &known()));
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(models[0]["provider"], "mistral");
    }

    /// A FIFO in the config's place does not hang the builder.
    #[cfg(unix)]
    #[test]
    fn a_fifo_for_a_config_is_not_waited_on() {
        let home = tempfile::tempdir().unwrap();
        crate::sync::testing::mkfifo(&home.path().join("config.toml"));
        let none = KnownModels::default();
        let plan = spawn_env(home.path(), &choice(Some("x"), Some("off"), &none));
        assert!(plan.unmet.is_some());
    }

    /// JSON is built, so a name with quotes stays one string.
    #[test]
    fn names_are_escaped() {
        let home = tempfile::tempdir().unwrap();
        let mut k = KnownModels::default();
        k.descriptions.insert("a\"b".into(), "n\",\"x".into());
        let plan = spawn_env(home.path(), &choice(Some("a\"b"), Some("max"), &k));
        let models: serde_json::Value =
            serde_json::from_str(var(&plan, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(models.as_array().unwrap().len(), 1);
        assert_eq!(models[0]["name"], "n\",\"x");
    }

    #[test]
    fn vibe_home_is_the_variable_else_dot_vibe() {
        let set = [("VIBE_HOME".to_string(), "/tmp/vh".to_string())];
        assert_eq!(vibe_home(&set), PathBuf::from("/tmp/vh"));
        let tilde = [("VIBE_HOME".to_string(), "~/vh".to_string())];
        assert_eq!(vibe_home(&tilde), dirs::home_dir().unwrap().join("vh"));
    }

    #[test]
    fn declared_capabilities() {
        let h = VibeHarness::new();
        assert_eq!(h.descriptor().id, HarnessId::VIBE);
        assert_eq!(h.descriptor().binary_names, [COMMAND]);
        let caps = h.capabilities();
        assert!(caps.model_per_process);
        for p in [
            PermissionPolicy::Ask,
            PermissionPolicy::AcceptEdits,
            PermissionPolicy::Bypass,
        ] {
            assert!(caps.supports_policy(p).is_some(), "{p}");
        }
        assert!(caps.supports_policy(PermissionPolicy::Auto).is_none());
        assert!(!caps.plan_mode);

        // A `[harnesses.vibe]` table sets the command and the rest in place
        // of the built-in's; the choice at spawn stays.
        let settings = HarnessSettings {
            command: vec!["/opt/vibe/bin/vibe-acp".into()],
            display_name: Some("Vibe (mine)".into()),
            asks_permission: Some(false),
            ..Default::default()
        };
        let h = VibeHarness::from_settings(&settings).unwrap();
        assert_eq!(h.descriptor().binary_names, ["/opt/vibe/bin/vibe-acp"]);
        assert_eq!(h.descriptor().display_name, "Vibe (mine)");
        let caps = h.capabilities();
        assert!(caps.model_per_process);
        assert!(caps.supports_policy(PermissionPolicy::Ask).is_none());
        let bad = HarnessSettings {
            modes: [("sometimes".to_string(), "ask".to_string())].into(),
            ..Default::default()
        };
        assert!(VibeHarness::from_settings(&bad).is_err());
    }
}
