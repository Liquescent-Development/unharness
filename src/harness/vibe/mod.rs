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

pub mod trust;

use super::acp::{AcpHarness, ModeMap, SpawnChoice, SpawnPlan};
use super::{
    AuthInfo, DoctorNote, Harness, HarnessDescriptor, MinVersion, ModelInfo, PrintConfig, Probe,
    probe_version_of, resolve_binary,
};
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

/// The release unharness was checked against: the session modes behind
/// each policy, the `thinking` option, the environment that chooses the
/// model and level, `session/close` and the files below were all read or
/// recorded on it, and older releases were not tried.
pub const MIN_VERSION: MinVersion = MinVersion {
    version: [2, 26, 1],
    needs: "the session modes, options and environment unharness was checked against",
    upgrade: "uv tool upgrade mistral-vibe",
};

/// Where vibe's default provider takes its key from: the environment, or
/// a line of `<home>/.env`, which vibe loads into it (`load_dotenv_values`,
/// `vibe/core/config/vibe_schema.py`), or the OS keyring
/// (`vibe/setup/auth/auth_state.py`).
const API_KEY_VAR: &str = "MISTRAL_API_KEY";

/// The most read of vibe's `.env` when looking for the key.
const ENV_FILE_LIMIT: u64 = 64 * 1024;

/// The files under vibe's home that decide how it runs (read in
/// `vibe/core/config/harness_files/_harness_manager.py`, `_paths.py` and
/// `vibe/core/paths/_vibe_home.py` of 2.26.1): its configuration (models,
/// providers, MCP servers, `vibe mcp add` writes there too), the folders
/// it trusts with project configuration, hooks, the user's instructions
/// and the skills it installed from its registry.
const GUARDED_FILES: [&str; 5] = [
    "config.toml",
    "trusted_folders.toml",
    "hooks.toml",
    "AGENTS.md",
    "skills.toml",
];

/// Its agent profiles (one can set the mode and the model), tools,
/// skills, plugins and prompts.
const GUARDED_TREES: [&str; 5] = ["agents", "tools", "skills", "plugins", "prompts"];

/// Caches vibe writes by itself, each entry a timestamp and what a server
/// answered: the experiments it is in (the default model, the system
/// prompt, smart-approve), the account (whose `api_base` vibe copies into
/// `config.toml` when they differ, `vibe/app_server/_account.py`) and the
/// connectors. Only the answers are compared.
const GUARDED_CACHES: [&str; 3] = [
    "experiment_eval_cache.json",
    "whoami_cache.json",
    "connector_bootstrap_cache.json",
];

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
        .resolving_relative_edits()
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

/// Where the key for vibe's default provider was found, as far as can be
/// told without a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAt {
    Environment,
    EnvFile,
}

/// Whether `MISTRAL_API_KEY` has a value in `env_value` (unharness's own
/// environment, which vibe inherits) or in `<home>/.env`. Presence only:
/// the value is never kept, copied or shown. `.env` is read only when it
/// is a regular file (vibe also reads a FIFO, which could block), and only
/// its first 64 KiB.
fn api_key_at(env_value: Option<&std::ffi::OsStr>, home: &Path) -> Option<KeyAt> {
    if env_value.is_some_and(|v| !v.is_empty()) {
        return Some(KeyAt::Environment);
    }
    let file = crate::sync::open_regular(&home.join(".env"), true).ok()?;
    let bytes = crate::sync::read_at_most(&file, ENV_FILE_LIMIT).ok()?;
    dotenv_has_value(&String::from_utf8_lossy(&bytes), API_KEY_VAR).then_some(KeyAt::EnvFile)
}

/// Whether the last assignment of `key` in a dotenv file gives it a
/// value, read as python-dotenv does for vibe: `[export ]KEY = value`,
/// a value in single or double quotes, an unquoted one up to ` #`; empty
/// is no value (vibe's `_has_value`).
fn dotenv_has_value(text: &str, key: &str) -> bool {
    let mut found = false;
    for line in text.lines() {
        let line = line.trim_start();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim_end() != key {
            continue;
        }
        let value = value.trim_start();
        found = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().is_some_and(|v| !v.is_empty()),
            _ => !value
                .split(" #")
                .next()
                .unwrap_or_default()
                .trim()
                .is_empty(),
        };
    }
    found
}

/// Whether vibe is signed in as far as a file read can tell, for a key
/// in `env_value` (unharness's environment) or `<home>/.env`; with neither
/// and no home, vibe has not been set up.
fn auth_info(env_value: Option<&std::ffi::OsStr>, home: &Path, sign_in: &str) -> AuthInfo {
    match api_key_at(env_value, home) {
        Some(KeyAt::Environment) => AuthInfo {
            authenticated: true,
            details: Some(format!("{API_KEY_VAR} is set in the environment")),
        },
        Some(KeyAt::EnvFile) => AuthInfo {
            authenticated: true,
            details: Some(format!(
                "{API_KEY_VAR} is in {}",
                home.join(".env").display()
            )),
        },
        None if std::fs::metadata(home).is_err() => AuthInfo {
            authenticated: false,
            details: Some(format!(
                "vibe has not been set up: {} is not there and {API_KEY_VAR} is not set (to set it up, run `{sign_in}`)",
                home.display(),
            )),
        },
        None => AuthInfo {
            authenticated: false,
            details: Some(format!(
                "no {API_KEY_VAR} in the environment or {}; a key in the OS keyring or another provider's is checked when a session starts (to sign in, run `{sign_in}`)",
                home.join(".env").display(),
            )),
        },
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

/// What `binary --version` prints, run with vibe's home a new empty
/// directory of its own, removed afterwards. vibe-acp 2.26.1 sets up its
/// home before it reads its arguments, also for `--version`: it creates
/// the directory when missing (`logs/vibe.log`, `trusted_folders.toml`,
/// `vibehistory`), tightens its mode, appends to its log, and empties a
/// `trusted_folders.toml` it cannot read. An strace of `--version` with
/// `VIBE_HOME` elsewhere showed no write outside that directory. The
/// probe runs outside the sandbox (doctor, and every TUI start), so it is
/// kept out of the user's home: on a machine where vibe was never run it
/// would otherwise create `~/.vibe`.
fn version_aside(binary: &Path) -> Option<String> {
    let scratch = std::env::temp_dir().join(format!(
        "unharness-vibe-version-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let mut dir = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
    dir.create(&scratch).ok()?;
    let mut command = std::process::Command::new(binary);
    command.env("VIBE_HOME", &scratch);
    let version = probe_version_of(command);
    let _ = std::fs::remove_dir_all(&scratch);
    version
}

/// [`VibeHarness::guarded`] for a vibe home at `home`.
fn guarded_in(home: &Path) -> Vec<Guarded> {
    let mut guarded: Vec<Guarded> = GUARDED_FILES
        .iter()
        .map(|f| Guarded::File(home.join(f)))
        .collect();
    guarded.push(Guarded::Secret(home.join(".env")));
    guarded.extend(GUARDED_TREES.iter().map(|t| Guarded::Tree(home.join(t))));
    guarded.extend(
        GUARDED_CACHES
            .iter()
            .map(|c| Guarded::json(home.join(c), cache_answers)),
    );
    guarded
}

/// A vibe cache without the time each entry was stored
/// (`stored_at_timestamp`, `stored_at`): vibe rewrites the time whenever
/// it fetches again, the answer only when the server's changed.
fn cache_answers(cache: &serde_json::Value) -> serde_json::Value {
    match cache.as_object() {
        Some(entries) => entries
            .iter()
            .map(|(key, entry)| {
                let mut entry = entry.clone();
                if let Some(fields) = entry.as_object_mut() {
                    fields.remove("stored_at_timestamp");
                    fields.remove("stored_at");
                }
                (key.clone(), entry)
            })
            .collect(),
        None => cache.clone(),
    }
}

impl Harness for VibeHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        self.acp.descriptor()
    }

    fn capabilities(&self) -> Capabilities {
        self.acp.capabilities()
    }

    /// The binary and its version (see [`version_aside`]); signed in when
    /// vibe's default provider has its key where a file read can tell, and
    /// not set up when vibe's home is not there.
    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        let binary = resolve_binary(self.descriptor(), binary_override);
        let version = binary.as_deref().and_then(version_aside);
        let auth = auth_info(
            std::env::var_os(API_KEY_VAR).as_deref(),
            &vibe_home(&[]),
            self.acp.sign_in().unwrap_or("vibe --setup"),
        );
        Probe {
            binary,
            version,
            auth,
        }
    }

    fn min_version(&self) -> Option<MinVersion> {
        Some(MIN_VERSION)
    }

    /// `vibe-acp --version` prints `vibe-acp 2.26.1`.
    fn reports_version(&self) -> bool {
        true
    }

    /// Whether vibe trusts the folder with its project configuration
    /// ([`trust::describe`]).
    fn doctor_notes(&self, cwd: &Path) -> Vec<DoctorNote> {
        let (text, warning) = trust::describe(&vibe_home(&[]), cwd);
        vec![DoctorNote {
            label: "Trust",
            text,
            warning,
        }]
    }

    /// That vibe ignores the folder's project configuration, when it does
    /// ([`trust::notice`]).
    fn session_notice(&self, cwd: &Path) -> Option<String> {
        trust::notice(&vibe_home(&[]), cwd)
    }

    /// Signed in when the key is in the environment or vibe's `.env`;
    /// otherwise not known: it may be in the OS keyring, whose check is
    /// not quick, or the user's configuration may choose a provider with
    /// another key or none.
    fn quick_auth(&self, _binary: &Path) -> Option<bool> {
        api_key_at(std::env::var_os(API_KEY_VAR).as_deref(), &vibe_home(&[])).map(|_| true)
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

    /// What decides how vibe runs, under its home (see [`GUARDED_FILES`]
    /// and the rest), with its `.env` (the API key, and any `VIBE_*`
    /// setting) compared by digest so that the key is never copied. Not
    /// watched: `logs/` (each session's journal and the model and level it
    /// is pinned to, rewritten every turn), the skills vibe writes out of
    /// its own binary (`builtin-skills/`, rewritten when they differ), its
    /// other caches and its history.
    fn guarded(&self, _workspace: &Path) -> Vec<Guarded> {
        guarded_in(&vibe_home(&[]))
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
        assert!(caps.supports_policy(PermissionPolicy::Plan).is_none());
        assert!(!caps.plan_mode);
        // Stdio servers in `session/new`; vibe-acp 2.26.1 names no
        // `mcpCapabilities`, so no http ones (a session would say).
        assert_eq!(
            caps.mcp,
            crate::core::McpSupport::via(crate::core::McpChannel::Protocol, false)
        );
        assert_eq!(h.min_version(), Some(MIN_VERSION));
        assert_eq!(MIN_VERSION.to_string(), "2.26.1");
        assert!(h.reports_version());

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

    /// What `vibe-acp --version` and `vibe --version` print (2.26.1).
    #[test]
    fn the_version_is_read_from_what_vibe_prints() {
        use crate::harness::parse_version;
        assert_eq!(parse_version("vibe-acp 2.26.1\n"), Some([2, 26, 1]));
        assert_eq!(parse_version("vibe 2.26.1"), Some([2, 26, 1]));
        assert!(parse_version("vibe-acp 2.26.1").unwrap() >= MIN_VERSION.version);
        assert!(parse_version("vibe 2.25.9").unwrap() < MIN_VERSION.version);
    }

    #[test]
    fn a_key_in_the_environment_or_dotenv_signs_in() {
        let home = tempfile::tempdir().unwrap();
        let env = home.path().join(".env");
        // Nothing anywhere: not known (the keyring may hold it).
        assert_eq!(api_key_at(None, home.path()), None);
        assert_eq!(
            api_key_at(Some(std::ffi::OsStr::new("sk")), home.path()),
            Some(KeyAt::Environment)
        );
        // Set but empty is no key.
        assert_eq!(
            api_key_at(Some(std::ffi::OsStr::new("")), home.path()),
            None
        );
        std::fs::write(&env, "OTHER=1\nMISTRAL_API_KEY=abc\n").unwrap();
        assert_eq!(api_key_at(None, home.path()), Some(KeyAt::EnvFile));
        std::fs::write(&env, "MISTRAL_API_KEY=\n").unwrap();
        assert_eq!(api_key_at(None, home.path()), None);
        // Read only, and never more than the limit.
        let before = std::fs::read(&env).unwrap();
        let _ = api_key_at(None, home.path());
        assert_eq!(std::fs::read(&env).unwrap(), before);
        let mut big = "MISTRAL_API_KEY=abc\n".to_string();
        big.push_str(&"#".repeat(ENV_FILE_LIMIT as usize));
        std::fs::write(&env, big).unwrap();
        assert_eq!(api_key_at(None, home.path()), None);
    }

    /// What doctor says of sign-in (#172), against tempfile homes: a key
    /// in the environment or `.env` signs in, neither is not known, and
    /// without the home vibe has not been set up. Nothing is created.
    #[test]
    fn sign_in_and_set_up_as_doctor_says_them() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("vibehome");
        let missing = dir.path().join("missing");
        std::fs::create_dir(&home).unwrap();
        let key = Some(std::ffi::OsStr::new("sk"));

        let auth = auth_info(None, &home, "vibe --setup");
        assert!(!auth.authenticated);
        let text = auth.details.unwrap();
        assert!(
            text.starts_with("no MISTRAL_API_KEY in the environment or "),
            "{text}"
        );
        assert!(text.ends_with("(to sign in, run `vibe --setup`)"), "{text}");

        std::fs::write(home.join(".env"), "MISTRAL_API_KEY=abc\n").unwrap();
        let auth = auth_info(None, &home, "vibe --setup");
        assert!(auth.authenticated);
        assert_eq!(
            auth.details.unwrap(),
            format!("MISTRAL_API_KEY is in {}", home.join(".env").display())
        );
        // The value is never shown.
        let auth = auth_info(key, &home, "vibe --setup");
        assert_eq!(
            auth.details.as_deref(),
            Some("MISTRAL_API_KEY is set in the environment")
        );

        let auth = auth_info(None, &missing, "my-setup");
        assert!(!auth.authenticated);
        assert_eq!(
            auth.details.unwrap(),
            format!(
                "vibe has not been set up: {} is not there and MISTRAL_API_KEY is not set \
                 (to set it up, run `my-setup`)",
                missing.display()
            )
        );
        // With the key in the environment vibe runs without being set up.
        assert!(auth_info(key, &missing, "vibe --setup").authenticated);
        assert!(!missing.exists());
    }

    /// The version probe gives vibe a home of its own, removed afterwards,
    /// so that `--version` (which sets up the home it is given) never
    /// creates or touches the user's (#172).
    #[cfg(unix)]
    #[test]
    fn the_version_is_probed_with_a_home_of_its_own() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let stub = dir.path().join("vibe-acp");
        // What vibe-acp 2.26.1 does with its home on `--version`.
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] || exit 2\necho \"$VIBE_HOME\" > '{}'\n\
                 mkdir -p \"$VIBE_HOME/logs\" && touch \"$VIBE_HOME/trusted_folders.toml\" \
                 \"$VIBE_HOME/vibehistory\" \"$VIBE_HOME/logs/vibe.log\"\necho 'vibe-acp 2.26.1'\n",
                seen.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(version_aside(&stub).as_deref(), Some("vibe-acp 2.26.1"));
        let used = std::fs::read_to_string(&seen).unwrap();
        let used = Path::new(used.trim());
        assert!(
            used.starts_with(std::env::temp_dir())
                && used
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("unharness-vibe-version-"),
            "{}",
            used.display()
        );
        assert!(!used.exists(), "{} left behind", used.display());
        assert_ne!(used, vibe_home(&[]));
    }

    /// vibe also reads a FIFO for `.env` (1Password's local env files);
    /// unharness does not wait on one.
    #[cfg(unix)]
    #[test]
    fn a_fifo_for_dotenv_is_not_waited_on() {
        let home = tempfile::tempdir().unwrap();
        crate::sync::testing::mkfifo(&home.path().join(".env"));
        assert_eq!(api_key_at(None, home.path()), None);
    }

    #[test]
    fn dotenv_lines_as_python_dotenv_reads_them() {
        let has = |text: &str| dotenv_has_value(text, "MISTRAL_API_KEY");
        assert!(has("MISTRAL_API_KEY=abc"));
        assert!(has("export MISTRAL_API_KEY=abc"));
        assert!(has("  MISTRAL_API_KEY = abc  "));
        assert!(has("MISTRAL_API_KEY=\"abc\""));
        assert!(has("MISTRAL_API_KEY='abc' # comment"));
        assert!(has("MISTRAL_API_KEY=abc#not-a-comment"));
        assert!(!has("MISTRAL_API_KEY="));
        assert!(!has("MISTRAL_API_KEY=\"\""));
        assert!(!has("MISTRAL_API_KEY=''"));
        // python-dotenv 1.x (vibe's) keeps this as the value `# nothing`.
        assert!(has("MISTRAL_API_KEY= # nothing"));
        assert!(!has("MISTRAL_API_KEY=   "));
        assert!(!has("MISTRAL_API_KEY"));
        assert!(!has("# MISTRAL_API_KEY=abc"));
        assert!(!has("MISTRAL_API_KEY_2=abc"));
        assert!(!has("OTHER_MISTRAL_API_KEY=abc"));
        // The last assignment wins, as in python-dotenv.
        assert!(!has("MISTRAL_API_KEY=abc\nMISTRAL_API_KEY="));
        assert!(has("MISTRAL_API_KEY=\nMISTRAL_API_KEY=abc"));
    }

    #[test]
    fn guarded_files_are_under_vibes_home() {
        let home = Path::new("/vh");
        let guarded = guarded_in(home);
        let names: Vec<String> = guarded.iter().map(|g| format!("{g:?}")).collect();
        for want in [
            "File(/vh/config.toml)",
            "File(/vh/trusted_folders.toml)",
            "File(/vh/hooks.toml)",
            "File(/vh/AGENTS.md)",
            "File(/vh/skills.toml)",
            "Secret(/vh/.env)",
            "Tree(/vh/agents)",
            "Tree(/vh/tools)",
            "Tree(/vh/skills)",
            "Tree(/vh/plugins)",
            "Tree(/vh/prompts)",
            "Projected(/vh/experiment_eval_cache.json)",
            "Projected(/vh/whoami_cache.json)",
            "Projected(/vh/connector_bootstrap_cache.json)",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
        }
        // Each session's journal and pins: per session, rewritten every turn.
        assert!(!names.iter().any(|n| n.contains("logs")));
    }

    /// vibe stores a time with each cache entry and rewrites it whenever it
    /// fetches again; the answers are what decide how it runs.
    #[test]
    fn a_cache_is_compared_by_its_answers() {
        use crate::core::guard::Watch;
        let home = tempfile::tempdir().unwrap();
        let keep = home.path().join("keep");
        let cache = home.path().join("whoami_cache.json");
        let write = |stored: u64, base: &str| {
            std::fs::write(
                &cache,
                json!({"k": {"stored_at_timestamp": stored,
                             "payload": {"plan_type": "x", "api_base": base}}})
                .to_string(),
            )
            .unwrap();
        };
        write(1, "https://api.mistral.ai");
        let connectors = home.path().join("connector_bootstrap_cache.json");
        std::fs::write(
            &connectors,
            json!({"k": {"format": 1, "stored_at": 5, "payload": {"connectors": []}}}).to_string(),
        )
        .unwrap();
        let mut watch = Watch::begin(&guarded_in(home.path()), None);
        write(2, "https://api.mistral.ai");
        std::fs::write(
            &connectors,
            json!({"k": {"format": 1, "stored_at": 9, "payload": {"connectors": []}}}).to_string(),
        )
        .unwrap();
        assert!(watch.changes(&keep).is_empty());
        write(3, "https://example.invalid");
        let changes = watch.changes(&keep);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, cache);
    }
}
