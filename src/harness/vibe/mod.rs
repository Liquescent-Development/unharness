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
//!
//! A provider defined in unharness's config (`[harnesses.vibe.providers]`)
//! goes the same way when it is chosen: `VIBE_PROVIDERS`, its models in
//! `VIBE_MODELS` and the one to run in `VIBE_ACTIVE_MODEL`.

pub mod subagents;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::json;

pub mod trust;

use super::acp::{AcpHarness, KnownModels, ModeMap, ProviderModels, SpawnChoice, SpawnPlan};
use super::{
    AuthInfo, DoctorNote, Harness, HarnessDescriptor, MinVersion, ModelInfo, PrintConfig, Probe,
    probe_version_of, resolve_binary,
};
use crate::config::{HarnessSettings, ProviderSettings};
use crate::core::guard::Guarded;
use crate::core::sandbox::{Sandbox, SandboxLevel, SandboxPaths};
use crate::core::{
    Capabilities, HarnessCommand, HarnessId, PermissionPolicy, PolicySupport, ProviderId,
    SessionConfig, SessionHandle, SubagentSupport,
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

/// vibe-acp's extension that rewinds a session in place to before a user
/// message (`_rewind_extension`, `vibe/acp/agent.py` of 2.26.1; not
/// advertised at `initialize`). Its `restoreFiles` is sent `false`:
/// unharness restores files from its own checkpoints
/// (`fixtures/vibe_acp_rewind*.jsonl`).
pub const REWIND_METHOD: &str = "_rewind/to";

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

/// vibe's own CLI (`--print --native`, `--no-tui`), installed beside
/// `vibe-acp` by the `mistral-vibe` package.
const CLI: &str = "vibe";

/// The agent (`--agent`) that backs each policy in vibe's own CLI: its
/// built-in profiles (`vibe/core/agents/models.py`, 2.26.1). `ask` asks
/// before every write and command, and in print mode (`-p`) denies them
/// ("Tool execution denied by approval callback"); `accept-edits` runs
/// `write_file` and `edit` unasked; `auto-approve` runs everything. `auto`
/// (its `smart-approve`, #174) and `plan` (#166) are not offered.
fn cli_agent(policy: PermissionPolicy) -> Option<&'static str> {
    match policy {
        PermissionPolicy::Ask => Some("ask"),
        PermissionPolicy::AcceptEdits => Some("accept-edits"),
        PermissionPolicy::Bypass => Some("auto-approve"),
        PermissionPolicy::Auto | PermissionPolicy::Plan => None,
    }
}

/// vibe's `--output` for unharness's `--format` (`text`, `json`,
/// `stream-json`), or vibe's own name for one.
fn cli_output(format: &str) -> Result<&'static str> {
    Ok(match format {
        "text" => "text",
        "json" => "json",
        "stream-json" | "streaming" => "streaming",
        other => anyhow::bail!(
            "vibe's print mode has no format '{other}'; --format takes text, json or stream-json (vibe's streaming)"
        ),
    })
}

/// `vibe` beside `acp` (the `vibe-acp` sessions run), else on `PATH`.
fn cli_binary(acp: &Path) -> Option<PathBuf> {
    let beside = acp
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(CLI))
        .filter(|path| is_executable(path));
    beside.or_else(|| super::which(CLI))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

/// The model and level of a run of vibe's own CLI, by environment as for a
/// session, with `home` vibe's: on a provider of `providers` chosen with
/// `--provider`, that provider's ([`provider_env`]). No session has said
/// what vibe's own models are, so a level is only given for one of those
/// or for a model the user's `config.toml` defines.
fn cli_plan(cfg: &PrintConfig, home: &Path, providers: &[Provider]) -> SpawnPlan {
    let choice = SpawnChoice {
        model: cfg.model.as_ref().map(|m| m.model.as_str()),
        effort: cfg.effort.as_deref(),
        known: &KnownModels::default(),
        env: &[],
        provider: cfg.provider.as_ref().map(|p| p.as_str()),
        policy: cfg.policy.unwrap_or(PermissionPolicy::Ask),
    };
    let mut plan = match choice
        .provider
        .and_then(|id| providers.iter().find(|p| p.id == id))
    {
        Some(provider) => provider_env(provider, &choice),
        None => spawn_env(home, &choice),
    };
    // vibe's CLI is given no `auto` (`cli_agent`), so it is not offered
    // the mode behind it either.
    plan.env
        .retain(|(key, _)| key != "VIBE_SMART_APPROVE_AVAILABLE");
    plan
}

/// `vibe -p` (`--print --native`) or `vibe` in its own interface
/// (`--no-tui`), in the session's directory, with the policy's agent and
/// the model and level by environment ([`cli_plan`]). Never `--trust`:
/// whether vibe reads the project's configuration is the user's to decide
/// in vibe. vibe's exit status is passed on (0 finished, 1 usage or
/// configuration error, 2 infrastructure failure, 3 stopped: a limit, a
/// refusal, SIGINT or SIGTERM; `HeadlessRun`, `vibe/cli/headless_run.py`).
fn cli_command(
    cfg: &PrintConfig,
    home: &Path,
    providers: &[Provider],
) -> Result<std::process::Command> {
    let Some(binary) = cli_binary(&cfg.binary) else {
        anyhow::bail!(
            "vibe's own CLI (`{CLI}`) is neither beside {} nor on PATH",
            cfg.binary.display()
        );
    };
    let policy = cfg.policy.unwrap_or(PermissionPolicy::Ask);
    let Some(agent) = cli_agent(policy) else {
        anyhow::bail!("vibe's own CLI has no agent for the policy '{policy}'");
    };
    let mut cmd = std::process::Command::new(binary);
    cmd.current_dir(&cfg.cwd);
    for (key, value) in cli_plan(cfg, home, providers).env {
        cmd.env(key, value);
    }
    cmd.arg("--workdir").arg(&cfg.cwd);
    cmd.arg("--agent").arg(agent);
    if cfg.print_mode
        && let Some(format) = &cfg.format
    {
        cmd.arg("--output").arg(cli_output(format)?);
    }
    if let Some(id) = &cfg.resume {
        cmd.arg("--resume").arg(id);
    }
    cmd.args(&cfg.extra_args);
    match (&cfg.prompt, cfg.print_mode) {
        // One word, so that a prompt that starts with `-` is not taken for
        // a flag; empty, vibe reads the prompt from stdin.
        (prompt, true) => {
            cmd.arg(format!("--prompt={}", prompt.as_deref().unwrap_or("")));
        }
        (Some(prompt), false) => {
            cmd.arg("--").arg(prompt);
        }
        (None, false) => {}
    }
    Ok(cmd)
}

/// What [`cli_command`] leaves out: a level it cannot give (see
/// [`cli_plan`]) and the MCP servers, which reach vibe only over ACP.
fn cli_warnings(cfg: &PrintConfig, home: &Path, providers: &[Provider]) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(why) = cli_plan(cfg, home, providers).unmet {
        warnings.push(format!(
            "vibe runs without the thinking level asked for: {why} (only a vibe-acp session tells a model's name; one defined in vibe's config.toml needs none)"
        ));
    }
    if !cfg.mcp_servers.is_empty() {
        let names: Vec<&str> = cfg.mcp_servers.iter().map(|s| s.name.as_str()).collect();
        warnings.push(format!(
            "vibe's own CLI takes no MCP servers from unharness, only from its own configuration; not passed on: {}",
            names.join(", ")
        ));
    }
    warnings
}

/// vibe's mode that backs `auto` (#174): a model classifies each call
/// that would ask, runs what it judges safe and escalates the rest
/// (`fixtures/vibe_acp_auto*.jsonl`).
pub const SMART_APPROVE: &str = "smart-approve";

/// `ask` is vibe's mode that asks before every write and command; under
/// `accept-edits` the agent stays in it and unharness answers the edits.
/// `auto` is smart-approve, `bypass` is `auto-approve`.
/// (`fixtures/vibe_acp_mode*.jsonl`, `fixtures/vibe_acp_auto*.jsonl`.)
fn mode_map() -> ModeMap {
    ModeMap::new([
        (crate::core::PermissionPolicy::Ask, "ask"),
        (crate::core::PermissionPolicy::Auto, SMART_APPROVE),
        (crate::core::PermissionPolicy::Bypass, "auto-approve"),
    ])
}

pub struct VibeHarness {
    acp: AcpHarness,
    /// The providers `[harnesses.vibe.providers]` defines.
    providers: Arc<Vec<Provider>>,
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
        let vibe = match settings.sign_in.as_deref() {
            Some(command) => VibeHarness {
                acp: vibe.acp.signing_in_with(Some(command)),
                ..vibe
            },
            None => vibe,
        };
        Ok(vibe.with_providers(providers(&settings.providers)?))
    }

    /// Offer `providers` beside vibe's own configuration: one chosen for a
    /// session is handed to vibe in its environment ([`provider_env`]).
    fn with_providers(self, providers: Vec<Provider>) -> Self {
        let providers = Arc::new(providers);
        let defined = providers.clone();
        let acp = self
            .acp
            .choosing_at_spawn(Arc::new(move |choice: &SpawnChoice<'_>| {
                plan_for(&defined, choice)
            }));
        VibeHarness { acp, providers }
    }

    /// The provider a session is to run on, if it is one of
    /// [`Self::providers`]; an error for one vibe does not have, or a
    /// model that provider does not list.
    fn session_provider(
        &self,
        provider: Option<&ProviderId>,
        model: Option<&str>,
    ) -> Result<Option<&Provider>> {
        let Some(id) = provider.map(ProviderId::as_str) else {
            return Ok(None);
        };
        if id == OWN_PROVIDER {
            return Ok(None);
        }
        let Some(p) = self.providers.iter().find(|p| p.id == id) else {
            let known: Vec<&str> = std::iter::once(OWN_PROVIDER)
                .chain(self.providers.iter().map(|p| p.id.as_str()))
                .collect();
            bail!(
                "{DISPLAY_NAME} has no provider '{id}' ({}; others are defined in [harnesses.vibe.providers])",
                known.join(", ")
            );
        };
        if let Some(m) = model
            && !p.models.iter().any(|x| x == m)
        {
            bail!(
                "provider {id} has no model '{m}' ({}; [harnesses.vibe.providers.{id}] models)",
                p.models.join(", ")
            );
        }
        Ok(Some(p))
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
        .reading_meta_with(subagents::reader)
        .rewinding_with(REWIND_METHOD)
        .choosing_at_spawn(Arc::new(|choice: &SpawnChoice<'_>| {
            spawn_env(&vibe_home(choice.env), choice)
        }));
        VibeHarness {
            acp,
            providers: Arc::default(),
        }
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

/// The smart-approve part of vibe's environment, on vibe's own
/// configuration and on a provider alike: the mode offered in every
/// session, and under `auto` new sessions started in it (see
/// [`spawn_env`]).
fn smart_approve_env(plan: &mut SpawnPlan, choice: &SpawnChoice<'_>) {
    plan.env.push((
        "VIBE_SMART_APPROVE_AVAILABLE".to_string(),
        "true".to_string(),
    ));
    if choice.policy == crate::core::PermissionPolicy::Auto {
        plan.env
            .push(("VIBE_DEFAULT_AGENT".to_string(), SMART_APPROVE.to_string()));
    }
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
///
/// smart-approve, the mode behind `auto`, is listed by vibe 2.26.1 only
/// for the cohort of a remote experiment, and `session/set_mode` to a mode
/// it does not list answers `{}` and changes nothing.
/// `VIBE_SMART_APPROVE_AVAILABLE` lists it in every session of the
/// process, a loaded one too, so that a policy changed to auto sets it and
/// reads it back with no restart; it does not make it the default, and a
/// user's `disabled_agents` still hides it. Under `auto`,
/// `VIBE_DEFAULT_AGENT` starts each new session in it, which also has vibe
/// look for its classifier model as the session opens
/// (`vibe/core/agents/manager.py`, `vibe/app_server/_runtime.py`).
pub fn spawn_env(home: &Path, choice: &SpawnChoice<'_>) -> SpawnPlan {
    let mut plan = SpawnPlan::default();
    smart_approve_env(&mut plan, choice);
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

/// The id of vibe's own configuration among its providers: the models its
/// sessions list (Mistral's, its experiments', those of the user's
/// `config.toml`), under the provider of any ACP agent's.
pub const OWN_PROVIDER: &str = super::acp::parse::PROVIDER;

/// What vibe is told a provider of unharness's is called: vibe merges its
/// providers by name across its configuration layers (`WithUnionMerge` in
/// `vibe/core/config/vibe_schema.py`), so one of the user's own with the
/// same name would be replaced for the process, and its models with it.
const PROVIDER_PREFIX: &str = "unharness-";

/// The API styles of vibe 2.26.1's generic backend (`_ADAPTERS`,
/// `mistralai_vibe_local_harness/vibe/adapters/generic/_execute.py`) but
/// `vertex-anthropic`, which needs a project and region unharness does not
/// pass. The first is vibe's default.
pub const API_STYLES: [&str; 4] = ["openai", "reasoning", "openai-responses", "anthropic"];

/// vibe's backends (`Backend`, `vibe/core/types.py`); its default for a
/// provider is `generic`.
pub const BACKENDS: [&str; 2] = ["generic", "mistral"];

/// A provider of `[harnesses.vibe.providers.<id>]`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub id: String,
    pub api_base: String,
    pub api_style: String,
    pub backend: String,
    /// The variable that holds the key; never its value.
    pub api_key_env_var: Option<String>,
    /// Never empty; the first is run when none is chosen.
    pub models: Vec<String>,
}

/// The providers of a `[harnesses.vibe]` table, checked; an error names
/// the first table that is wrong. unharness's config is the user's, outside
/// the workspace: nothing a session writes can add one.
pub fn providers(table: &BTreeMap<String, ProviderSettings>) -> Result<Vec<Provider>> {
    table
        .iter()
        .map(|(id, settings)| {
            provider(id, settings).with_context(|| format!("[harnesses.vibe.providers.{id}]"))
        })
        .collect()
}

fn provider(id: &str, s: &ProviderSettings) -> Result<Provider> {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if id.is_empty() || !id.chars().all(plain) {
        bail!("a provider's name has only letters, digits, '-', '_' and '.'");
    }
    if id == OWN_PROVIDER {
        bail!("'{OWN_PROVIDER}' is vibe's own configuration; choose another name");
    }
    let api_base = s.api_base.trim();
    let scheme = ["http://", "https://"]
        .iter()
        .find_map(|p| api_base.strip_prefix(p));
    if scheme.is_none_or(str::is_empty)
        || api_base
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("api_base must be an http:// or https:// URL");
    }
    let api_style = s.api_style.as_deref().unwrap_or(API_STYLES[0]);
    if api_style == "vertex-anthropic" {
        bail!(
            "api_style 'vertex-anthropic' needs a project and region, which unharness does not pass"
        );
    }
    if !API_STYLES.contains(&api_style) {
        bail!(
            "api_style '{api_style}' is not one of {}",
            API_STYLES.join(", ")
        );
    }
    let backend = s.backend.as_deref().unwrap_or(BACKENDS[0]);
    if !BACKENDS.contains(&backend) {
        bail!("backend '{backend}' is not one of {}", BACKENDS.join(", "));
    }
    // Only ever a name: what is there is not repeated, in case it is a key.
    let api_key_env_var = s.api_key_env_var.as_deref().filter(|v| !v.is_empty());
    if let Some(var) = api_key_env_var {
        let mut chars = var.chars();
        let first = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if !first || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!(
                "api_key_env_var is not an environment variable's name: it names the variable that holds the key, never the key"
            );
        }
    }
    if s.models.is_empty() {
        bail!("models lists none; name the models the server has");
    }
    for (i, m) in s.models.iter().enumerate() {
        if m.trim().is_empty() || m.trim() != m || m.chars().any(char::is_control) {
            bail!("models: '{}' is not a model name", m.escape_debug());
        }
        if s.models[..i].contains(m) {
            bail!("models: '{m}' is listed twice");
        }
    }
    Ok(Provider {
        id: id.to_string(),
        api_base: api_base.to_string(),
        api_style: api_style.to_string(),
        backend: backend.to_string(),
        api_key_env_var: api_key_env_var.map(str::to_string),
        models: s.models.clone(),
    })
}

/// vibe's environment for a choice: on one of `providers`, that
/// provider's ([`provider_env`]); on vibe's own configuration, the model
/// and level alone ([`spawn_env`]).
fn plan_for(providers: &[Provider], choice: &SpawnChoice<'_>) -> SpawnPlan {
    match choice
        .provider
        .and_then(|id| providers.iter().find(|p| p.id == id))
    {
        Some(provider) => provider_env(provider, choice),
        None => spawn_env(&vibe_home(choice.env), choice),
    }
}

/// vibe's environment for a session on `provider`: the provider in
/// `VIBE_PROVIDERS`, each of its models as a whole entry in `VIBE_MODELS`
/// (its alias the model's name), and the chosen one, else the first, in
/// `VIBE_ACTIVE_MODEL`. A level goes in the entry of the model that runs,
/// so that one `VIBE_MODELS` says both; the rest get vibe's default (`off`).
/// Built as JSON, never pasted together: vibe 2.26.1 fails its whole
/// environment layer on a value that does not parse. No key is passed, only
/// the name of the variable vibe reads it from.
pub fn provider_env(provider: &Provider, choice: &SpawnChoice<'_>) -> SpawnPlan {
    let mut plan = SpawnPlan::default();
    smart_approve_env(&mut plan, choice);
    let name = format!("{PROVIDER_PREFIX}{}", provider.id);
    let active = choice.model.unwrap_or(&provider.models[0]).to_string();
    let level = match choice.effort {
        Some(level) if THINKING_LEVELS.contains(&level) => Some(level),
        Some(level) => {
            plan.unmet = Some(format!(
                "vibe has no thinking level '{level}'; it has {}",
                THINKING_LEVELS.join(", ")
            ));
            None
        }
        None => None,
    };
    let providers = json!([{
        "name": name,
        "api_base": provider.api_base,
        "api_style": provider.api_style,
        "backend": provider.backend,
        "api_key_env_var": provider.api_key_env_var.as_deref().unwrap_or_default(),
    }]);
    let models: Vec<serde_json::Value> = provider
        .models
        .iter()
        .map(|m| {
            let mut entry = json!({"alias": m, "name": m, "provider": name});
            if let Some(level) = level.filter(|_| *m == active) {
                entry["thinking"] = json!(level);
            }
            entry
        })
        .collect();
    plan.env
        .push(("VIBE_PROVIDERS".to_string(), providers.to_string()));
    plan.env.push((
        "VIBE_MODELS".to_string(),
        serde_json::Value::Array(models).to_string(),
    ));
    plan.env
        .push(("VIBE_ACTIVE_MODEL".to_string(), active.clone()));
    plan.overrides = Some(active);
    plan.provider = Some(ProviderModels {
        provider: provider.id.clone(),
        models: provider.models.clone(),
    });
    plan
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

    /// Those of any ACP agent chosen at spawn, and with providers in
    /// `[harnesses.vibe.providers]` more than one, chosen when vibe starts.
    ///
    /// The ACP agent's, with its subagents (`subagents.rs`): a start and,
    /// when the agent waits for one, its end and report. Nothing stops one
    /// from outside (no client method; `session/cancel` leaves it running),
    /// and nothing reports one between turns.
    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities {
            subagents: SubagentSupport {
                reported: true,
                described: false,
                stop: false,
                report_turn: false,
                unreported_ends: true,
            },
            ..self.acp.capabilities()
        };
        if !self.providers.is_empty() {
            caps.multi_provider = true;
            caps.provider_per_process = true;
        }
        caps
    }

    /// vibe's own configuration, then the providers of
    /// `[harnesses.vibe.providers]`.
    fn list_providers(
        &self,
        _binary: &Path,
        _sandbox: &Sandbox,
    ) -> Result<Vec<(ProviderId, String)>> {
        Ok(std::iter::once((
            ProviderId::from(OWN_PROVIDER),
            "vibe's own configuration".to_string(),
        ))
        .chain(
            self.providers
                .iter()
                .map(|p| (ProviderId::new(p.id.clone()), p.api_base.clone())),
        )
        .collect())
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

    /// A provider of `[harnesses.vibe.providers]` has the models it lists;
    /// vibe's own are what a session lists.
    fn list_models(
        &self,
        binary: &Path,
        provider: &ProviderId,
        sandbox: &Sandbox,
    ) -> Result<Vec<ModelInfo>> {
        match self.providers.iter().find(|p| p.id == provider.as_str()) {
            Some(p) => Ok(p
                .models
                .iter()
                .map(|m| ModelInfo {
                    model_ref: crate::core::ModelRef::new(HarnessId::VIBE, provider.clone(), m),
                    display_name: m.clone(),
                    description: Some(format!("on {}", p.api_base)),
                    effort_levels: None,
                })
                .collect()),
            None => self.acp.list_models(binary, provider, sandbox),
        }
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

    /// vibe's own CLI holds to each policy it has an agent for
    /// ([`cli_agent`]): its interface asks under `ask`, and its print mode
    /// denies what it would have asked about and goes on (2.26.1, live).
    /// `ask` stays tied to `asks_permission`, as for a session.
    fn print_policies(&self, _print_mode: bool) -> Vec<PolicySupport> {
        self.capabilities()
            .permission_policies
            .into_iter()
            .filter(|p| cli_agent(p.policy).is_some())
            .collect()
    }

    fn default_sandbox(&self) -> SandboxLevel {
        self.acp.default_sandbox()
    }

    fn file_reference(&self, path: &str) -> String {
        self.acp.file_reference(path)
    }

    /// A provider chosen must be vibe's own or one of
    /// `[harnesses.vibe.providers]`, and a model chosen with one of those
    /// among its models.
    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        self.session_provider(
            cfg.provider.as_ref(),
            cfg.model.as_ref().map(|m| m.model.as_str()),
        )?;
        self.acp.start_session(cfg)
    }

    /// `vibe -p` (`--print --native`) or `vibe` in its own interface
    /// (`--no-tui`): see [`cli_command`].
    fn build_print_command(&self, cfg: &PrintConfig) -> Result<std::process::Command> {
        self.session_provider(
            cfg.provider.as_ref(),
            cfg.model.as_ref().map(|m| m.model.as_str()),
        )?;
        cli_command(cfg, &vibe_home(&[]), &self.providers)
    }

    fn print_warnings(&self, cfg: &PrintConfig) -> Vec<String> {
        cli_warnings(cfg, &vibe_home(&[]), &self.providers)
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
            policy: PermissionPolicy::Ask,
            known,
            env: &[],
            provider: None,
        }
    }

    /// What every vibe process is started with: smart-approve offered.
    fn available() -> (String, String) {
        (
            "VIBE_SMART_APPROVE_AVAILABLE".to_string(),
            "true".to_string(),
        )
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
            [
                available(),
                (
                    "VIBE_ACTIVE_MODEL".to_string(),
                    "mistral-medium-3.5".to_string()
                )
            ]
        );
        assert_eq!(plan.overrides, None);
        assert_eq!(plan.unmet, None);
        // Nothing chosen, nothing set but smart-approve offered.
        assert_eq!(
            spawn_env(home.path(), &choice(None, None, &k)),
            SpawnPlan {
                env: vec![available()],
                ..Default::default()
            }
        );
    }

    /// Under auto a new session starts in smart-approve
    /// (`acp/fixtures/vibe_acp_auto.jsonl`: `currentModeId:
    /// "smart-approve"`, nothing sent to set it). Under every policy the
    /// mode is offered, so that a change to auto, or a session loaded
    /// under it, can set it: vibe 2.26.1 outside the experiment's cohort
    /// listed it only with `VIBE_SMART_APPROVE_AVAILABLE` (checked live).
    #[test]
    fn auto_starts_in_smart_approve() {
        let home = tempfile::tempdir().unwrap();
        let k = known();
        for policy in [
            PermissionPolicy::Plan,
            PermissionPolicy::Ask,
            PermissionPolicy::AcceptEdits,
            PermissionPolicy::Auto,
            PermissionPolicy::Bypass,
        ] {
            let plan = spawn_env(
                home.path(),
                &SpawnChoice {
                    policy,
                    ..choice(None, None, &k)
                },
            );
            assert_eq!(var(&plan, "VIBE_SMART_APPROVE_AVAILABLE"), Some("true"));
            let default = var(&plan, "VIBE_DEFAULT_AGENT");
            if policy == PermissionPolicy::Auto {
                assert_eq!(default, Some(SMART_APPROVE));
            } else {
                assert_eq!(default, None, "{policy}");
            }
        }
        // With a model and level as before.
        let plan = spawn_env(
            home.path(),
            &SpawnChoice {
                policy: PermissionPolicy::Auto,
                ..choice(Some("glm-5-3"), Some("low"), &k)
            },
        );
        assert_eq!(var(&plan, "VIBE_DEFAULT_AGENT"), Some(SMART_APPROVE));
        assert_eq!(var(&plan, "VIBE_ACTIVE_MODEL"), Some("glm-5-3"));
        assert!(var(&plan, "VIBE_MODELS").is_some());
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
            PermissionPolicy::Auto,
            PermissionPolicy::Bypass,
        ] {
            assert!(caps.supports_policy(p).is_some(), "{p}");
        }
        // auto is smart-approve (#174).
        assert_eq!(
            mode_map().for_policy(PermissionPolicy::Auto),
            Some((PermissionPolicy::Auto, SMART_APPROVE))
        );
        assert!(caps.supports_policy(PermissionPolicy::Plan).is_none());
        assert!(!caps.plan_mode);
        // Through its `_rewind/to`; a fork's messages have ids of its own.
        assert_eq!(
            caps.rewind,
            crate::core::RewindSupport {
                conversation: true,
                anchors_survive_fork: false,
            }
        );
        // Stdio servers in `session/new`; vibe-acp 2.26.1 names no
        // `mcpCapabilities`, so no http ones (a session would say).
        assert_eq!(
            caps.mcp,
            crate::core::McpSupport::via(crate::core::McpChannel::Protocol, false)
        );
        // Started and, when waited for, ended (`subagents.rs`); never
        // stopped from here, and nothing waits for an end that may not come.
        let subagents = caps.subagents;
        assert!(subagents.reported && subagents.unreported_ends);
        assert!(!subagents.stop && !subagents.report_turn);
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

    /// A directory with `vibe-acp` and `vibe` in it, and a print run of
    /// the first.
    fn cli_setup() -> (tempfile::TempDir, PrintConfig) {
        let dir = tempfile::tempdir().unwrap();
        for name in ["vibe-acp", "vibe"] {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let cfg = PrintConfig {
            binary: dir.path().join("vibe-acp"),
            cwd: PathBuf::from("/ws"),
            prompt: Some("fix it".into()),
            print_mode: true,
            policy: Some(PermissionPolicy::Ask),
            ..Default::default()
        };
        (dir, cfg)
    }

    fn cli_args(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    fn cli_env(cmd: &std::process::Command, key: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().to_string()))
    }

    /// `vibe -p` with the policy's agent (2.26.1's `--help` and built-in
    /// profiles), the session's directory, and the prompt as one word.
    #[test]
    fn the_print_command_per_policy() {
        let (dir, cfg) = cli_setup();
        let home = dir.path();
        for (policy, agent) in [
            (PermissionPolicy::Ask, "ask"),
            (PermissionPolicy::AcceptEdits, "accept-edits"),
            (PermissionPolicy::Bypass, "auto-approve"),
        ] {
            let cfg = PrintConfig {
                policy: Some(policy),
                ..cfg.clone()
            };
            let cmd = cli_command(&cfg, home, &[]).unwrap();
            assert_eq!(cmd.get_program(), dir.path().join("vibe"));
            assert_eq!(cmd.get_current_dir(), Some(Path::new("/ws")));
            assert_eq!(
                cli_args(&cmd),
                ["--workdir", "/ws", "--agent", agent, "--prompt=fix it"]
            );
        }
        // No agent stands for these; none more permissive is taken.
        for policy in [PermissionPolicy::Auto, PermissionPolicy::Plan] {
            let cfg = PrintConfig {
                policy: Some(policy),
                ..cfg.clone()
            };
            assert!(cli_command(&cfg, home, &[]).is_err(), "{policy}");
        }
        // Never `--trust`.
        let a = cli_args(&cli_command(&cfg, home, &[]).unwrap());
        assert!(!a.iter().any(|a| a == "--trust"));
    }

    #[test]
    fn the_print_command_formats_resume_and_stdin() {
        let (dir, cfg) = cli_setup();
        let home = dir.path();
        let with = |format: &str| PrintConfig {
            format: Some(format.into()),
            ..cfg.clone()
        };
        for (format, output) in [
            ("text", "text"),
            ("json", "json"),
            ("stream-json", "streaming"),
            ("streaming", "streaming"),
        ] {
            let a = cli_args(&cli_command(&with(format), home, &[]).unwrap());
            assert!(a.windows(2).any(|w| w == ["--output", output]), "{a:?}");
        }
        assert!(cli_command(&with("yaml"), home, &[]).is_err());

        let cfg = PrintConfig {
            resume: Some("s-1".into()),
            extra_args: vec!["--max-turns".into(), "3".into()],
            prompt: Some("-n is a flag".into()),
            ..cfg
        };
        let a = cli_args(&cli_command(&cfg, home, &[]).unwrap());
        assert!(a.windows(2).any(|w| w == ["--resume", "s-1"]));
        assert!(a.windows(2).any(|w| w == ["--max-turns", "3"]));
        assert_eq!(a.last().unwrap(), "--prompt=-n is a flag");
        // No prompt: vibe reads it from stdin.
        let cfg = PrintConfig {
            prompt: None,
            ..cfg
        };
        let a = cli_args(&cli_command(&cfg, home, &[]).unwrap());
        assert_eq!(a.last().unwrap(), "--prompt=");
    }

    /// `--no-tui`: vibe's own interface with the agent and the prompt as
    /// its first message, after `--`; no print-mode flags.
    #[test]
    fn the_interface_command() {
        let (dir, cfg) = cli_setup();
        let cfg = PrintConfig {
            print_mode: false,
            format: Some("json".into()),
            policy: Some(PermissionPolicy::AcceptEdits),
            ..cfg
        };
        let a = cli_args(&cli_command(&cfg, dir.path(), &[]).unwrap());
        assert_eq!(
            a,
            [
                "--workdir",
                "/ws",
                "--agent",
                "accept-edits",
                "--",
                "fix it"
            ]
        );
        let cfg = PrintConfig {
            prompt: None,
            ..cfg
        };
        let a = cli_args(&cli_command(&cfg, dir.path(), &[]).unwrap());
        assert_eq!(a, ["--workdir", "/ws", "--agent", "accept-edits"]);
    }

    /// The model and level go in the environment as for a session; a level
    /// only for a model the user's config defines, since no session has
    /// said what the others are.
    #[test]
    fn the_cli_gets_the_model_and_level_by_environment() {
        let (dir, cfg) = cli_setup();
        let home = dir.path();
        std::fs::write(
            home.join("config.toml"),
            "[[models]]\nname = \"devstral-2\"\nprovider = \"mistral\"\nalias = \"dev\"\n",
        )
        .unwrap();
        let model = |m: &str| Some(crate::core::ModelRef::new(HarnessId::VIBE, "acp", m));
        let cfg = PrintConfig {
            model: model("dev"),
            effort: Some("high".into()),
            ..cfg
        };
        let cmd = cli_command(&cfg, home, &[]).unwrap();
        assert_eq!(cli_env(&cmd, "VIBE_ACTIVE_MODEL").as_deref(), Some("dev"));
        let models: serde_json::Value =
            serde_json::from_str(&cli_env(&cmd, "VIBE_MODELS").unwrap()).unwrap();
        assert_eq!(
            models,
            json!([{"alias": "dev", "name": "devstral-2", "provider": "mistral",
                    "thinking": "high"}])
        );
        assert!(cli_warnings(&cfg, home, &[]).is_empty());
        // Nothing of it on the command line.
        assert!(!cli_args(&cmd).iter().any(|a| a.contains("dev")));

        // A built-in model: its name is not known here, so the model goes
        // and the level is left out, with a warning.
        let cfg = PrintConfig {
            model: model("glm-5-3"),
            ..cfg
        };
        let cmd = cli_command(&cfg, home, &[]).unwrap();
        assert_eq!(
            cli_env(&cmd, "VIBE_ACTIVE_MODEL").as_deref(),
            Some("glm-5-3")
        );
        assert_eq!(cli_env(&cmd, "VIBE_MODELS"), None);
        let warnings = cli_warnings(&cfg, home, &[]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("without the thinking level"));

        // Nothing chosen, nothing set.
        let (dir, cfg) = cli_setup();
        let cmd = cli_command(&cfg, dir.path(), &[]).unwrap();
        assert_eq!(cmd.get_envs().count(), 0);
        assert!(cli_warnings(&cfg, dir.path(), &[]).is_empty());
    }

    /// `--print --native --provider`: vibe's CLI gets the provider in its
    /// environment as a session does, and an unknown one is refused.
    #[test]
    fn the_cli_runs_on_a_configured_provider() {
        let (dir, cfg) = cli_setup();
        let settings = HarnessSettings {
            providers: [(
                "lan".to_string(),
                ProviderSettings {
                    api_base: "http://127.0.0.1:8080/v1".into(),
                    models: vec!["qwen3-coder".into()],
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let vibe = VibeHarness::from_settings(&settings).unwrap();
        let cfg = PrintConfig {
            provider: Some(ProviderId::from("lan")),
            ..cfg
        };
        let cmd = cli_command(&cfg, dir.path(), &vibe.providers).unwrap();
        assert_eq!(
            cli_env(&cmd, "VIBE_ACTIVE_MODEL").as_deref(),
            Some("qwen3-coder")
        );
        let providers: serde_json::Value =
            serde_json::from_str(&cli_env(&cmd, "VIBE_PROVIDERS").unwrap()).unwrap();
        assert_eq!(providers[0]["name"], "unharness-lan");
        assert!(cli_env(&cmd, "VIBE_MODELS").is_some());
        let unknown = PrintConfig {
            provider: Some(ProviderId::from("nowhere")),
            ..cfg
        };
        assert!(vibe.build_print_command(&unknown).is_err());
    }

    #[test]
    fn mcp_servers_are_not_passed_to_the_cli() {
        let (dir, cfg) = cli_setup();
        let cfg = PrintConfig {
            mcp_servers: vec![crate::core::McpServer {
                name: "tiny".into(),
                transport: crate::core::McpTransport::Stdio {
                    command: "tiny".into(),
                    args: Vec::new(),
                    env: Default::default(),
                },
            }],
            ..cfg
        };
        let warnings = cli_warnings(&cfg, dir.path(), &[]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].ends_with("not passed on: tiny"), "{warnings:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_cli_is_the_vibe_beside_vibe_acp() {
        let (dir, _) = cli_setup();
        let beside = dir.path().join("vibe");
        assert_eq!(
            cli_binary(&dir.path().join("vibe-acp")),
            Some(beside.clone())
        );
        // One that cannot be run is not taken (PATH's, if any, is).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&beside, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_ne!(cli_binary(&dir.path().join("vibe-acp")), Some(beside));
    }

    /// `ask`, `accept-edits` and `bypass` in vibe's print mode and in its
    /// own interface; `ask` only while `asks_permission` holds.
    #[test]
    fn print_policies_are_the_agents_vibe_has() {
        let policies = |h: &VibeHarness, print_mode| -> Vec<PermissionPolicy> {
            h.print_policies(print_mode)
                .iter()
                .map(|p| p.policy)
                .collect()
        };
        let h = VibeHarness::new();
        for print_mode in [true, false] {
            assert_eq!(
                policies(&h, print_mode),
                [
                    PermissionPolicy::Ask,
                    PermissionPolicy::AcceptEdits,
                    PermissionPolicy::Bypass
                ]
            );
        }
        let h = VibeHarness::from_settings(&HarnessSettings {
            asks_permission: Some(false),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            policies(&h, true),
            [PermissionPolicy::AcceptEdits, PermissionPolicy::Bypass]
        );
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

    /// `[harnesses.vibe.providers.lan]` as the docs show it, read from TOML.
    const LAN: &str = r#"
[harnesses.vibe.providers.lan]
api_base = "https://LOCAL-LLM/v1"
models = ["qwen3.8-q5kxl-x2", "gemma-4"]
"#;

    fn lan() -> Provider {
        let cfg: crate::config::Config = toml::from_str(LAN).unwrap();
        providers(&cfg.harnesses["vibe"].providers)
            .unwrap()
            .remove(0)
    }

    fn json_var(plan: &SpawnPlan, key: &str) -> serde_json::Value {
        let raw = var(plan, key).unwrap_or_else(|| panic!("no {key}"));
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("{key} is not JSON ({e}): {raw}"))
    }

    fn on<'a>(
        provider: &'a str,
        model: Option<&'a str>,
        effort: Option<&'a str>,
        known: &'a KnownModels,
    ) -> SpawnChoice<'a> {
        SpawnChoice {
            provider: Some(provider),
            ..choice(model, effort, known)
        }
    }

    /// The provider, its models as whole entries and the one to run, all
    /// in valid JSON (vibe 2.26.1 fails its environment layer on one that
    /// is not), with no key: only the variable's name, empty for none.
    #[test]
    fn a_provider_is_handed_over_by_environment() {
        let known = KnownModels::default();
        let lan = lan();
        assert_eq!(lan.api_style, "openai");
        assert_eq!(lan.backend, "generic");
        let plan = provider_env(&lan, &on("lan", None, None, &known));
        assert_eq!(
            json_var(&plan, "VIBE_PROVIDERS"),
            json!([{"name": "unharness-lan", "api_base": "https://LOCAL-LLM/v1",
                    "api_style": "openai", "backend": "generic", "api_key_env_var": ""}])
        );
        assert_eq!(
            json_var(&plan, "VIBE_MODELS"),
            json!([
                {"alias": "qwen3.8-q5kxl-x2", "name": "qwen3.8-q5kxl-x2", "provider": "unharness-lan"},
                {"alias": "gemma-4", "name": "gemma-4", "provider": "unharness-lan"},
            ])
        );
        // None chosen: the first.
        assert_eq!(var(&plan, "VIBE_ACTIVE_MODEL"), Some("qwen3.8-q5kxl-x2"));
        assert_eq!(plan.env.len(), 4, "one VIBE_MODELS, nothing else");
        // smart-approve is offered on a provider too, and under auto the
        // session starts in it (#174).
        assert_eq!(var(&plan, "VIBE_SMART_APPROVE_AVAILABLE"), Some("true"));
        assert_eq!(var(&plan, "VIBE_DEFAULT_AGENT"), None);
        let auto = provider_env(
            &lan,
            &SpawnChoice {
                policy: PermissionPolicy::Auto,
                ..on("lan", None, None, &known)
            },
        );
        assert_eq!(var(&auto, "VIBE_DEFAULT_AGENT"), Some(SMART_APPROVE));
        assert_eq!(plan.overrides.as_deref(), Some("qwen3.8-q5kxl-x2"));
        assert_eq!(plan.unmet, None);
        assert_eq!(
            plan.provider,
            Some(ProviderModels {
                provider: "lan".into(),
                models: vec!["qwen3.8-q5kxl-x2".into(), "gemma-4".into()],
            })
        );

        // A key's variable is named, and a style and backend passed on.
        let keyed = Provider {
            api_key_env_var: Some("LAN_API_KEY".into()),
            api_style: "anthropic".into(),
            ..lan.clone()
        };
        let plan = provider_env(&keyed, &on("lan", None, None, &known));
        let providers = json_var(&plan, "VIBE_PROVIDERS");
        assert_eq!(providers[0]["api_key_env_var"], "LAN_API_KEY");
        assert_eq!(providers[0]["api_style"], "anthropic");
    }

    /// A model and a level chosen on a provider make one `VIBE_MODELS`:
    /// the level in the entry of the model that runs, the others whole
    /// and without one (vibe's default, `off`).
    #[test]
    fn a_level_goes_in_the_running_models_entry() {
        let known = known();
        let lan = lan();
        let plan = provider_env(&lan, &on("lan", Some("gemma-4"), Some("high"), &known));
        assert_eq!(var(&plan, "VIBE_ACTIVE_MODEL"), Some("gemma-4"));
        assert_eq!(
            plan.env.iter().filter(|(k, _)| k == "VIBE_MODELS").count(),
            1
        );
        assert_eq!(
            json_var(&plan, "VIBE_MODELS"),
            json!([
                {"alias": "qwen3.8-q5kxl-x2", "name": "qwen3.8-q5kxl-x2", "provider": "unharness-lan"},
                {"alias": "gemma-4", "name": "gemma-4", "provider": "unharness-lan", "thinking": "high"},
            ])
        );
        assert_eq!(plan.overrides.as_deref(), Some("gemma-4"));

        // A level alone: on the first model.
        let plan = provider_env(&lan, &on("lan", None, Some("low"), &known));
        assert_eq!(json_var(&plan, "VIBE_MODELS")[0]["thinking"], "low");

        // A level vibe does not have is not sent, and said.
        let plan = provider_env(&lan, &on("lan", None, Some("extreme"), &known));
        let models = json_var(&plan, "VIBE_MODELS");
        assert!(
            models
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m.get("thinking").is_none())
        );
        assert!(plan.unmet.unwrap().contains("'extreme'"));
    }

    /// The spawn builder: a configured provider's environment for one of
    /// its ids; vibe's own configuration (no provider, `agent`, or one
    /// not defined) gets the model and level alone, a model of the user's
    /// `config.toml` with its provider as before (#164).
    #[test]
    fn the_builder_picks_the_providers_environment() {
        let known = known();
        let lan = [lan()];
        let plan = plan_for(&lan, &on("lan", None, None, &known));
        assert!(var(&plan, "VIBE_PROVIDERS").is_some());

        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[[models]]\nalias = \"local\"\nname = \"qwen3-coder\"\nprovider = \"llamacpp\"\n",
        )
        .unwrap();
        let env = [(
            "VIBE_HOME".to_string(),
            home.path().to_string_lossy().into_owned(),
        )];
        for provider in [None, Some(OWN_PROVIDER)] {
            let choice = SpawnChoice {
                env: &env,
                provider,
                ..choice(Some("local"), Some("high"), &known)
            };
            let plan = plan_for(&lan, &choice);
            assert_eq!(var(&plan, "VIBE_PROVIDERS"), None);
            assert_eq!(plan.provider, None);
            assert_eq!(var(&plan, "VIBE_ACTIVE_MODEL"), Some("local"));
            assert_eq!(
                json_var(&plan, "VIBE_MODELS"),
                json!([{"alias": "local", "name": "qwen3-coder", "provider": "llamacpp",
                        "thinking": "high"}])
            );
        }
    }

    /// A session names a provider vibe has, and a model of it.
    #[test]
    fn a_session_on_an_unknown_provider_or_model_is_refused() {
        let cfg: crate::config::Config = toml::from_str(LAN).unwrap();
        let vibe = VibeHarness::from_settings(&cfg.harnesses["vibe"]).unwrap();
        let lan = ProviderId::from("lan");
        let own = ProviderId::from(OWN_PROVIDER);
        assert!(
            vibe.session_provider(None, Some("glm-5-3"))
                .unwrap()
                .is_none()
        );
        assert!(
            vibe.session_provider(Some(&own), Some("glm-5-3"))
                .unwrap()
                .is_none()
        );
        let p = vibe.session_provider(Some(&lan), Some("gemma-4")).unwrap();
        assert_eq!(p.map(|p| p.id.as_str()), Some("lan"));
        assert!(vibe.session_provider(Some(&lan), None).unwrap().is_some());
        // A model of vibe's own (or of its config.toml) is not the provider's.
        let e = vibe
            .session_provider(Some(&lan), Some("glm-5-3"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no model 'glm-5-3'") && e.contains("gemma-4"),
            "{e}"
        );
        let e = vibe
            .session_provider(Some(&ProviderId::from("cloud")), None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no provider 'cloud'") && e.contains("agent, lan"),
            "{e}"
        );
    }

    /// Listed beside vibe's own, with their models before any session;
    /// more than one provider only with some defined.
    #[test]
    fn providers_are_listed_with_their_models() {
        let none = VibeHarness::new();
        assert!(!none.capabilities().multi_provider);
        let sandbox = Sandbox::default();
        let bin = Path::new("vibe-acp");
        assert_eq!(none.list_providers(bin, &sandbox).unwrap().len(), 1);

        let cfg: crate::config::Config = toml::from_str(LAN).unwrap();
        let vibe = VibeHarness::from_settings(&cfg.harnesses["vibe"]).unwrap();
        let caps = vibe.capabilities();
        assert!(caps.multi_provider && caps.provider_per_process && caps.model_per_process);
        let listed: Vec<String> = vibe
            .list_providers(bin, &sandbox)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect();
        assert_eq!(listed, ["agent", "lan"]);
        let models = vibe
            .list_models(bin, &ProviderId::from("lan"), &sandbox)
            .unwrap();
        let names: Vec<&str> = models.iter().map(|m| m.model_ref.model.as_str()).collect();
        assert_eq!(names, ["qwen3.8-q5kxl-x2", "gemma-4"]);
        assert_eq!(models[0].model_ref.provider, ProviderId::from("lan"));
        // vibe's own come from a session.
        assert!(
            vibe.list_models(bin, &ProviderId::from(OWN_PROVIDER), &sandbox)
                .unwrap()
                .is_empty()
        );
    }

    /// A provider table that is wrong is an error naming it, and never
    /// repeats what `api_key_env_var` holds (it may be a key).
    #[test]
    fn provider_tables_that_are_wrong() {
        let parse = |toml_text: &str| -> Result<Vec<Provider>> {
            let cfg: crate::config::Config = toml::from_str(toml_text)?;
            providers(&cfg.harnesses["vibe"].providers)
        };
        let table = |body: &str| format!("[harnesses.vibe.providers.lan]\n{body}\n");
        let err = |body: &str| format!("{:#}", parse(&table(body)).unwrap_err());

        assert!(
            parse(&table(
                "api_base = \"http://127.0.0.1:8080/v1\"\nmodels = [\"m\"]"
            ))
            .is_ok()
        );
        let full = parse(&table(
            "api_base = \"https://h/v1\"\napi_style = \"reasoning\"\nbackend = \"mistral\"\n\
             api_key_env_var = \"LAN_KEY\"\nmodels = [\"m\"]",
        ))
        .unwrap();
        assert_eq!(full[0].api_key_env_var.as_deref(), Some("LAN_KEY"));
        assert_eq!(full[0].backend, "mistral");

        // Unknown keys and a missing api_base do not parse.
        assert!(
            parse(&table(
                "api_base = \"http://h\"\nmodels = [\"m\"]\napi_key = \"x\""
            ))
            .is_err()
        );
        assert!(parse(&table("models = [\"m\"]")).is_err());

        let e = err("api_base = \"http://h\"");
        assert!(
            e.starts_with("[harnesses.vibe.providers.lan]: models lists none"),
            "{e}"
        );
        for base in ["\"h/v1\"", "\"ftp://h\"", "\"http://\"", "\"http://h /v1\""] {
            let e = err(&format!("api_base = {base}\nmodels = [\"m\"]"));
            assert!(e.contains("api_base must be"), "{base}: {e}");
        }
        let e = err("api_base = \"http://h\"\napi_style = \"grpc\"\nmodels = [\"m\"]");
        assert!(e.contains("api_style 'grpc'"), "{e}");
        let e = err("api_base = \"http://h\"\napi_style = \"vertex-anthropic\"\nmodels = [\"m\"]");
        assert!(e.contains("project and region"), "{e}");
        let e = err("api_base = \"http://h\"\nbackend = \"local\"\nmodels = [\"m\"]");
        assert!(e.contains("backend 'local'"), "{e}");
        let e = err("api_base = \"http://h\"\nmodels = [\"m\", \"m\"]");
        assert!(e.contains("listed twice"), "{e}");
        let e = err("api_base = \"http://h\"\nmodels = [\" m\"]");
        assert!(e.contains("not a model name"), "{e}");

        let secret = "sk-not-a-name-0123";
        let e = err(&format!(
            "api_base = \"http://h\"\napi_key_env_var = \"{secret}\"\nmodels = [\"m\"]"
        ));
        assert!(e.contains("api_key_env_var is not"), "{e}");
        assert!(!e.contains(secret), "the value is never repeated: {e}");

        for name in ["agent", "a b", "lan/x"] {
            let cfg = crate::config::Config {
                harnesses: [(
                    "vibe".to_string(),
                    HarnessSettings {
                        providers: [(
                            name.to_string(),
                            ProviderSettings {
                                api_base: "http://h".into(),
                                models: vec!["m".into()],
                                ..Default::default()
                            },
                        )]
                        .into(),
                        ..Default::default()
                    },
                )]
                .into(),
                ..Default::default()
            };
            assert!(
                providers(&cfg.harnesses["vibe"].providers).is_err(),
                "{name}"
            );
        }
        // From the table, the harness is not built either.
        let cfg: crate::config::Config = toml::from_str(&table("api_base = \"http://h\"")).unwrap();
        assert!(VibeHarness::from_settings(&cfg.harnesses["vibe"]).is_err());
    }
}
