//! Claude Code harness.

pub mod parse;
pub mod transport;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};
use crate::core::guard::Guarded;
use crate::core::process::ProbeProcess;
use crate::core::sandbox::{Sandbox, SandboxPaths};
use crate::core::{
    Capabilities, HarnessId, McpChannel, McpSupport, ModelRef, PermissionPolicy, PolicySupport,
    ProviderId, RewindSupport, SessionConfig, SessionHandle, SubagentSupport,
};

#[derive(Debug, Default)]
pub struct ClaudeHarness {
    /// Run Claude with `CLAUDE_CONFIG_DIR` set to its state directory, so
    /// that `.claude.json` lives inside it. Claude rewrites that file through
    /// a lock directory and a temp file beside it; in the home directory the
    /// sandbox denies both, and cannot allow them without opening the whole
    /// of it. On unless `[harnesses.claude] relocate_config = false`; off in
    /// `default()`, which tests use.
    pub relocate_config: bool,
    /// Where `--mcp-config` files are written instead of
    /// `<state dir>/unharness/mcp`; tests set it.
    pub mcp_config_dir: Option<PathBuf>,
}

/// Claude's state directory when `CLAUDE_CONFIG_DIR` does not name another.
const STATE_DIR: &str = ".claude";
const CONFIG_FILE: &str = ".claude.json";

/// Copy `~/.claude.json` to `<dir>/.claude.json` unless that exists, so a
/// relocated Claude starts from the user's account, trust and MCP settings.
/// The original is left alone. Returns whether a copy was made.
pub fn seed_relocated_config(home: &Path, dir: &Path) -> Result<bool> {
    let (from, to) = (home.join(CONFIG_FILE), dir.join(CONFIG_FILE));
    if to.exists() || !from.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    // Copy then rename, so Claude never sees half a file.
    let partial = dir.join(format!("{CONFIG_FILE}.unharness-import"));
    std::fs::copy(&from, &partial)?;
    std::fs::rename(&partial, &to)?;
    Ok(true)
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn state_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new("~").join(STATE_DIR))
}

/// The files of `skills` that Claude's sync of organisation skills rewrites
/// with the same bytes on a timer, `synced/<org>/manifest.json` and
/// `synced/<org>/.last-complete-round` (2.1.296, #114). The skills it syncs
/// were not rewritten.
fn synced_bookkeeping(path: &Path) -> bool {
    let parts: Vec<_> = path.iter().collect();
    matches!(
        parts.as_slice(),
        [synced, _org, name]
            if *synced == "synced" && (*name == "manifest.json" || *name == ".last-complete-round")
    )
}

/// The part of `.claude.json` that decides what Claude runs and allows: MCP
/// servers, and each project's allowed tools, MCP approvals and trust. The
/// rest is counters and caches that change on every run.
fn config_that_matters(config: &Value) -> Value {
    const PER_PROJECT: [&str; 5] = [
        "allowedTools",
        "mcpServers",
        "enabledMcpjsonServers",
        "disabledMcpjsonServers",
        "hasTrustDialogAccepted",
    ];
    let projects: serde_json::Map<String, Value> = config
        .get("projects")
        .and_then(Value::as_object)
        .map(|projects| {
            projects
                .iter()
                .map(|(dir, project)| {
                    let kept: serde_json::Map<String, Value> = PER_PROJECT
                        .iter()
                        .filter_map(|key| Some((key.to_string(), project.get(*key)?.clone())))
                        .collect();
                    (dir.clone(), Value::Object(kept))
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "mcpServers": config.get("mcpServers").cloned().unwrap_or(Value::Null),
        "projects": projects,
    })
}

impl ClaudeHarness {
    /// Home and the directory the config is relocated to. Nothing when
    /// relocation is off or the user already set `CLAUDE_CONFIG_DIR` (the
    /// file is then inside that directory anyway).
    fn relocation(&self) -> Option<(PathBuf, PathBuf)> {
        if !self.relocate_config || std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
            return None;
        }
        let home = dirs::home_dir()?;
        let dir = home.join(STATE_DIR);
        Some((home, dir))
    }

    /// The directory for `--mcp-config` files: unharness's own state, which
    /// the sandbox lets a harness read and not write.
    fn mcp_dir(&self) -> PathBuf {
        self.mcp_config_dir.clone().unwrap_or_else(|| {
            dirs::state_dir()
                .or_else(dirs::data_local_dir)
                .unwrap_or_else(std::env::temp_dir)
                .join("unharness")
                .join("mcp")
        })
    }

    /// The relocated config for a probe, once `prepare` has seeded it: a
    /// probe seeds nothing, or `prepare`'s notice would never be shown.
    fn probe_config_env(&self) -> Option<(String, String)> {
        let (_, dir) = self.relocation()?;
        dir.join(CONFIG_FILE).exists().then(|| {
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                dir.to_string_lossy().into_owned(),
            )
        })
    }

    /// The environment that relocates the config, seeding it if `prepare`
    /// has not.
    fn config_env(&self) -> Result<Option<(String, String)>> {
        let Some((home, dir)) = self.relocation() else {
            return Ok(None);
        };
        seed_relocated_config(&home, &dir)?;
        Ok(Some((
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        )))
    }
}

pub static DESCRIPTOR: HarnessDescriptor = HarnessDescriptor {
    id: HarnessId::CLAUDE,
    display_name: "Claude Code (claude)",
    short_name: "Claude",
    binary_names: &["claude"],
    providers: ProviderSource::Static(&[
        ("anthropic", "Anthropic"),
        ("bedrock", "Amazon Bedrock"),
        ("vertex", "Google Vertex AI"),
        ("foundry", "Microsoft Foundry"),
    ]),
};

/// The variables by which Claude Code chooses the API it calls, with its
/// name for each (`apiProvider`, 2.1.292). With two of them set, whatever
/// their values and whether in the environment or in its settings' `env`,
/// it calls Anthropic's.
const PROVIDER_SWITCHES: [(&str, &str); 6] = [
    ("CLAUDE_CODE_USE_BEDROCK", "bedrock"),
    ("CLAUDE_CODE_USE_FOUNDRY", "foundry"),
    ("CLAUDE_CODE_USE_ANTHROPIC_AWS", "anthropicAws"),
    (
        "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
        "anthropicGoogleCloud",
    ),
    ("CLAUDE_CODE_USE_MANTLE", "mantle"),
    ("CLAUDE_CODE_USE_VERTEX", "vertex"),
];

/// Whether Claude reads a switch's value as on (`0`, `2`, `no` are off).
fn switch_on(value: &str) -> bool {
    ["1", "true", "yes", "on"]
        .iter()
        .any(|on| value.trim().eq_ignore_ascii_case(on))
}

/// The provider Claude calls with the switches `var` finds.
fn provider_from(var: impl Fn(&str) -> Option<String>) -> ProviderId {
    let set: Vec<(&str, String)> = PROVIDER_SWITCHES
        .iter()
        .filter_map(|(name, provider)| Some((*provider, var(name)?)))
        .collect();
    match set.as_slice() {
        [(provider, value)] if switch_on(value) => parse::provider_id(provider),
        _ => ProviderId::from("anthropic"),
    }
}

/// The environment in which Claude calls `provider`: its switch on and
/// every other one taken away (one set to `0` would still count as set).
/// Nothing for a provider of Claude's that unharness does not offer.
pub fn provider_env(provider: &ProviderId) -> Result<Vec<(&'static str, Option<&'static str>)>> {
    let own = match provider.as_str() {
        "anthropic" => None,
        "bedrock" | "vertex" | "foundry" => PROVIDER_SWITCHES
            .iter()
            .find(|(_, name)| *name == provider.as_str())
            .map(|(var, _)| *var),
        // Claude's names for APIs unharness does not offer, which
        // `default_provider` can report: the environment as it is.
        other if PROVIDER_SWITCHES.iter().any(|(_, name)| *name == other) => {
            return Ok(Vec::new());
        }
        other => {
            bail!("Claude Code has no provider '{other}' (anthropic, bedrock, vertex or foundry)")
        }
    };
    Ok(PROVIDER_SWITCHES
        .iter()
        .map(|(var, _)| (*var, (Some(*var) == own).then_some("1")))
        .collect())
}

/// Why the provider in the answer to `initialize` is not the one that was
/// chosen, if it is not.
pub fn provider_mismatch(chosen: Option<&ProviderId>, answer: &Value) -> Option<String> {
    let chosen = chosen?;
    let runs_on = parse::provider_id(answer.pointer("/account/apiProvider")?.as_str()?);
    (runs_on != *chosen).then(|| {
        format!(
            "Claude Code runs on {runs_on}, not {chosen}: a CLAUDE_CODE_USE_* variable in the \
             env of its settings decides, and unharness cannot override it"
        )
    })
}

pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// When Claude cannot be asked (`initialize_answer`). Checked 2026-10-02;
/// aliases resolve to the latest release.
const MODELS: &[(&str, &str, &str)] = &[
    ("opus", "Opus (latest)", "Alias for the latest Opus release"),
    (
        "sonnet",
        "Sonnet (latest)",
        "Alias for the latest Sonnet release",
    ),
    (
        "fable",
        "Fable (latest)",
        "Alias for the latest Fable release",
    ),
    (
        "haiku",
        "Haiku (latest)",
        "Alias for the latest Haiku release",
    ),
    (
        "claude-fable-5-1",
        "Claude Fable 5.1",
        "Most capable, deepest reasoning",
    ),
    (
        "claude-opus-5",
        "Claude Opus 5",
        "Deep reasoning, strongest coding",
    ),
    (
        "claude-sonnet-5",
        "Claude Sonnet 5",
        "Fast, capable workhorse",
    ),
    (
        "claude-haiku-4-5-20251001",
        "Claude Haiku 4.5",
        "Fastest, lowest cost",
    ),
];

impl Harness for ClaudeHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        &DESCRIPTOR
    }

    fn prepare(&self) -> Result<Option<String>> {
        let Some((home, dir)) = self.relocation() else {
            return Ok(None);
        };
        Ok(seed_relocated_config(&home, &dir)?.then(|| {
            format!(
                "Claude Code now keeps its config in {dir}/{CONFIG_FILE} when run by unharness \
                 (copied from ~/{CONFIG_FILE}, which is unchanged), so that it can be updated \
                 inside the sandbox. Export CLAUDE_CONFIG_DIR={dir} to make plain `claude` \
                 share it, or set relocate_config = false under [harnesses.claude].",
                dir = dir.display()
            )
        }))
    }

    /// Not `plugins`: Claude rewrites its plugin cache on every start.
    fn guarded(&self, _workspace: &Path) -> Vec<Guarded> {
        let state = state_dir();
        let config = if self.relocate_config || std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
            state.join(CONFIG_FILE)
        } else {
            Path::new("~").join(CONFIG_FILE)
        };
        let mut guarded = vec![
            Guarded::File(state.join("settings.json")),
            Guarded::File(state.join("CLAUDE.md")),
            Guarded::json(config, config_that_matters),
        ];
        for tree in ["agents", "commands", "hooks"] {
            guarded.push(Guarded::Tree(state.join(tree)));
        }
        guarded.push(Guarded::TreeWithBookkeeping {
            path: state.join("skills"),
            rewritten: synced_bookkeeping,
        });
        guarded
    }

    fn sandbox_paths(&self) -> SandboxPaths {
        // `~/.claude.json` is not here: Claude never writes it in place, so
        // the grant would only let an agent edit it (see `relocate_config`).
        // Not `~/.local/share/claude`, `~/.local/state/claude` or
        // `~/.cache/claude`: the installed binaries and the updater's
        // staging. A confined Claude does not update itself.
        let mut writable = vec![state_dir(), PathBuf::from("~/.cache/claude-cli-nodejs")];
        // The messaging socket of each process.
        if let Some(run) = dirs::runtime_dir() {
            writable.push(run.join("cc-socks"));
        }
        SandboxPaths { writable }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming_input: true,
            text_deltas: true,
            thinking: true,
            tool_events: true,
            interactive_permissions: true,
            permission_policies: PermissionPolicy::ALL
                .iter()
                .copied()
                .map(PolicySupport::full)
                .collect(),
            effort_levels: EFFORT_LEVELS.iter().map(|s| s.to_string()).collect(),
            resume_by_id: true,
            live_model_list: true,
            // `CLAUDE_CODE_USE_*` in its environment.
            multi_provider: true,
            provider_per_process: true,
            ask_user_question: true,
            interrupt: true,
            usage_reporting: true,
            image_input: true,
            file_input: true,
            plan_updates: true,
            // Tasks are described and their progress worded; an interrupt
            // between turns stops them.
            subagents: SubagentSupport {
                reported: true,
                described: true,
                stop: true,
                // `fixtures/subagent.jsonl`: a turn follows the end.
                report_turn: true,
            },
            steer: true,
            compaction: true,
            context_usage: true,
            rate_limits: true,
            rewind: RewindSupport {
                conversation: true,
                anchors_survive_fork: false,
            },
            fork: true,
            // `--mcp-config`, which takes stdio and http servers.
            mcp: McpSupport::via(McpChannel::CommandLine, true),
            // `--permission-mode plan`.
            plan_mode: true,
            // A stream-json prompt runs its built-ins, custom commands and
            // skills, and an unknown `/name` reaches the model as text
            // (2.1.292); `initialize` answers with the list.
            slash_commands: true,
            // Checked on 2.1.295: quit unprompted, no session was written.
            start_unprompted: true,
            // The `remote_control` control request (2.1.296); `/rc` itself
            // is refused over stream-json.
            remote_control: true,
        }
    }

    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        let binary = resolve_binary(&DESCRIPTOR, binary_override);
        let version = binary.as_deref().and_then(probe_version);
        let auth = binary.as_deref().map(auth_status).unwrap_or_default();
        Probe {
            binary,
            version,
            auth,
        }
    }

    fn quick_auth(&self, binary: &Path) -> Option<bool> {
        Some(auth_status(binary).authenticated)
    }

    /// What the environment and the user's settings choose; a session
    /// reports what it runs on (the workspace's settings may differ).
    fn default_provider(&self) -> Option<ProviderId> {
        let settings = expand_home(&state_dir().join("settings.json"));
        let env = std::fs::read_to_string(settings)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v.get("env").cloned())
            .unwrap_or(Value::Null);
        Some(provider_from(|var| {
            match env.get(var) {
                Some(Value::String(s)) => Some(s.clone()),
                Some(v) if !v.is_null() => Some(v.to_string()),
                _ => None,
            }
            .or_else(|| std::env::var(var).ok())
        }))
    }

    /// What Claude answers to `initialize` on that provider. The table
    /// only when Anthropic's API cannot be asked.
    fn list_models(
        &self,
        binary: &Path,
        provider: &ProviderId,
        sandbox: &Sandbox,
    ) -> Result<Vec<ModelInfo>> {
        let env = provider_env(provider)?;
        let answer = initialize_answer(binary, self.probe_config_env(), &env, sandbox);
        if let Ok(answer) = &answer {
            if let Some(why) = provider_mismatch(Some(provider), answer) {
                bail!(why);
            }
            if let Some(models) = parse::initialize_models(answer) {
                return Ok(models);
            }
        }
        if provider.as_str() != "anthropic" {
            return answer.and(Ok(Vec::new()));
        }
        Ok(MODELS
            .iter()
            .map(|(id, name, desc)| ModelInfo {
                model_ref: ModelRef::new(HarnessId::CLAUDE, "anthropic", *id),
                display_name: name.to_string(),
                description: Some(desc.to_string()),
                effort_levels: None,
            })
            .collect())
    }

    /// Claude Code puts the content of an `@path` in a user message into
    /// the turn itself, wherever in the text it stands; a path with spaces
    /// goes in double quotes. A path with a quote in it has no such form.
    fn file_reference(&self, path: &str) -> String {
        if path.contains('"') {
            path.to_string()
        } else if path.contains(char::is_whitespace) {
            format!("@\"{path}\"")
        } else {
            format!("@{path}")
        }
    }

    fn start_session(&self, mut cfg: SessionConfig) -> Result<SessionHandle> {
        cfg.env.extend(self.config_env()?);
        let mcp = transport::mcp_config_file(&cfg.mcp_servers, &self.mcp_dir())?;
        transport::start(cfg, mcp)
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<Command> {
        let mut cmd = Command::new(&cfg.binary);
        cmd.current_dir(&cfg.cwd);
        if let Some((key, value)) = self.config_env()? {
            cmd.env(key, value);
        }
        if let Some(provider) = &cfg.provider {
            apply_env(&mut cmd, &provider_env(provider)?);
        }
        let mcp = transport::mcp_config_file(&cfg.mcp_servers, &self.mcp_dir())?;
        if let Some(path) = &mcp {
            cmd.arg("--mcp-config").arg(path);
        }
        if let Some(policy) = cfg.policy {
            cmd.args(transport::policy_args(policy));
        }
        if let Some(m) = &cfg.model {
            cmd.arg("--model").arg(&m.model);
        }
        if let Some(e) = &cfg.effort {
            cmd.arg("--effort").arg(e);
        }
        if let Some(id) = &cfg.resume {
            cmd.arg("--resume").arg(id);
        }
        if let Some(fmt) = &cfg.format {
            cmd.arg("--output-format").arg(fmt);
        }
        cmd.args(&cfg.extra_args);
        if cfg.print_mode {
            cmd.arg("-p");
        }
        if let Some(p) = &cfg.prompt {
            // `--mcp-config` takes every word up to the next flag, and in
            // passthrough there may be none before the prompt.
            if mcp.is_some() {
                cmd.arg("--");
            }
            cmd.arg(p);
        }
        Ok(cmd)
    }
}

/// Set the variables with a value and take away the others.
fn apply_env(cmd: &mut Command, vars: &[(&str, Option<&str>)]) {
    for (key, value) in vars {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

fn no_answer() -> anyhow::Error {
    anyhow!(
        "Claude Code did not list its models within {} s",
        QUERY_TIMEOUT.as_secs()
    )
}

/// How long a model list may take. On Bedrock without credentials Claude
/// answered after 60 s (2.1.292), on Vertex after 3 s.
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Start Claude, send `initialize` and return its answer, which lists the
/// models (Claude has no command that prints them). `--safe-mode` turns
/// off hooks, plugins and MCP servers and keeps the settings' `env`, which
/// chooses the provider; it starts in an empty directory and reads only
/// the user's settings. Those still name commands Claude runs at startup
/// (`apiKeyHelper` ran, checked on 2.1.292), and a sandboxed session can
/// write them, hence `sandbox`.
fn initialize_answer(
    binary: &Path,
    config: Option<(String, String)>,
    provider: &[(&str, Option<&str>)],
    sandbox: &Sandbox,
) -> Result<Value> {
    let dir = std::env::temp_dir().join(format!("unharness-claude-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir)?;
    let answer = ask_initialize(binary, config, provider, sandbox, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    answer
}

fn ask_initialize(
    binary: &Path,
    config: Option<(String, String)>,
    provider: &[(&str, Option<&str>)],
    sandbox: &Sandbox,
    dir: &Path,
) -> Result<Value> {
    let mut cmd = Command::new(binary);
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--safe-mode",
        "--setting-sources",
        "user",
    ])
    .current_dir(dir);
    cmd.envs(config);
    apply_env(&mut cmd, provider);
    for var in transport::PARENT_SESSION_VARS {
        cmd.env_remove(var);
    }
    let mut probe = ProbeProcess::spawn(cmd, sandbox).context("start claude")?;
    let (id, line) = transport::control_request("initialize", serde_json::json!({}));
    probe.write_line(&line)?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    loop {
        let line = match probe.next_line(deadline) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => return Err(no_answer()),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("Claude Code exited before it listed its models")
            }
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("control_response")
            && v.pointer("/response/request_id").and_then(Value::as_str) == Some(id.as_str())
        {
            return v
                .pointer("/response/response")
                .cloned()
                .context("initialize had no answer");
        }
    }
}

fn auth_status(binary: &Path) -> AuthInfo {
    let Ok(out) = Command::new(binary).args(["auth", "status"]).output() else {
        return AuthInfo {
            authenticated: false,
            details: Some("could not run `claude auth status`".into()),
        };
    };
    let Ok(val) = serde_json::from_slice::<Value>(&out.stdout) else {
        return AuthInfo {
            authenticated: false,
            details: Some("Not authenticated with claude.ai".into()),
        };
    };
    let logged_in = val
        .get("loggedIn")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut details = Vec::new();
    if let Some(o) = val.get("orgName").and_then(Value::as_str) {
        details.push(format!("org: {o}"));
    }
    if let Some(p) = val.get("subscriptionType").and_then(Value::as_str) {
        details.push(format!("plan: {p}"));
    }
    if let Some(e) = val.get("email").and_then(Value::as_str) {
        details.push(format!("user: {e}"));
    }
    AuthInfo {
        authenticated: logged_in,
        details: (!details.is_empty()).then(|| details.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    /// Only the sync's own two files are compared by content; a synced
    /// skill is compared as any other file.
    #[test]
    fn only_the_skill_sync_s_bookkeeping_is_compared_by_content() {
        for path in [
            "synced/org/manifest.json",
            "synced/org/.last-complete-round",
        ] {
            assert!(synced_bookkeeping(Path::new(path)), "{path}");
        }
        for path in [
            "synced/org/pdf/SKILL.md",
            "synced/org/pdf/manifest.json",
            "synced/manifest.json",
            "mine/manifest.json",
            "synced/org",
        ] {
            assert!(!synced_bookkeeping(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn print_command_shape() {
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/claude"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("run tests".into()),
            print_mode: true,
            model: Some(ModelRef::new(HarnessId::CLAUDE, "anthropic", "sonnet")),
            provider: None,
            effort: Some("low".into()),
            policy: Some(PermissionPolicy::Bypass),
            format: Some("stream-json".into()),
            resume: None,
            extra_args: vec![],
            mcp_servers: Vec::new(),
            sandbox: crate::core::Sandbox::off(),
        };
        let a = args(&ClaudeHarness::default().build_print_command(&cfg).unwrap());
        assert_eq!(a.last().unwrap(), "run tests");
        assert!(a.contains(&"-p".to_string()));
        assert!(a.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(a.windows(2).any(|w| w == ["--model", "sonnet"]));
        assert!(
            a.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
    }

    #[test]
    fn passthrough_has_no_print_flag() {
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/claude"),
            cwd: PathBuf::from("/tmp"),
            print_mode: false,
            ..Default::default()
        };
        let a = args(&ClaudeHarness::default().build_print_command(&cfg).unwrap());
        assert!(a.is_empty());
    }

    #[test]
    fn files_are_referenced_with_an_at() {
        let claude = ClaudeHarness::default();
        assert_eq!(claude.file_reference("src/main.rs"), "@src/main.rs");
        assert_eq!(
            claude.file_reference("my notes/a.txt"),
            "@\"my notes/a.txt\""
        );
        assert_eq!(claude.file_reference("odd\".txt"), "odd\".txt");
    }

    #[test]
    fn capabilities_and_models() {
        let caps = ClaudeHarness::default().capabilities();
        assert!(caps.interactive_permissions);
        assert_eq!(caps.permission_policies.len(), 5);
        assert!(caps.supports_effort("xhigh"));
        let models = ClaudeHarness::default()
            .list_models(
                Path::new("/nonexistent/claude"),
                &ProviderId::from("anthropic"),
                &Sandbox::off(),
            )
            .unwrap();
        assert!(models.iter().any(|m| m.model_ref.model == "opus"));
        // The table is Anthropic's; another provider's models come from
        // Claude or not at all.
        for provider in ["bedrock", "openai"] {
            assert!(
                ClaudeHarness::default()
                    .list_models(
                        Path::new("/nonexistent/claude"),
                        &ProviderId::from(provider),
                        &Sandbox::off(),
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn the_provider_is_the_one_switch_that_is_on() {
        let with = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            provider_from(|name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())).0
        };
        assert_eq!(with(&[]), "anthropic");
        assert_eq!(with(&[("CLAUDE_CODE_USE_VERTEX", "1")]), "vertex");
        assert_eq!(with(&[("CLAUDE_CODE_USE_BEDROCK", "True")]), "bedrock");
        assert_eq!(with(&[("CLAUDE_CODE_USE_FOUNDRY", "yes")]), "foundry");
        assert_eq!(with(&[("CLAUDE_CODE_USE_MANTLE", "on")]), "mantle");
        // Off, or two of them: Anthropic's API (checked on 2.1.292).
        assert_eq!(with(&[("CLAUDE_CODE_USE_VERTEX", "0")]), "anthropic");
        assert_eq!(with(&[("CLAUDE_CODE_USE_VERTEX", "2")]), "anthropic");
        assert_eq!(
            with(&[
                ("CLAUDE_CODE_USE_BEDROCK", "0"),
                ("CLAUDE_CODE_USE_FOUNDRY", "1")
            ]),
            "anthropic"
        );
    }

    #[test]
    fn choosing_a_provider_takes_the_other_switches_away() {
        let env = provider_env(&ProviderId::from("vertex")).unwrap();
        assert_eq!(env.len(), PROVIDER_SWITCHES.len());
        for (var, value) in &env {
            assert_eq!(*value, (*var == "CLAUDE_CODE_USE_VERTEX").then_some("1"));
        }
        let anthropic = provider_env(&ProviderId::from("anthropic")).unwrap();
        assert!(anthropic.iter().all(|(_, value)| value.is_none()));
        // A provider of Claude's that unharness does not offer is left as
        // the environment has it; an unknown one is refused.
        assert!(
            provider_env(&ProviderId::from("mantle"))
                .unwrap()
                .is_empty()
        );
        assert!(provider_env(&ProviderId::from("openai")).is_err());

        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/claude"),
            cwd: PathBuf::from("/tmp"),
            provider: Some(ProviderId::from("bedrock")),
            ..Default::default()
        };
        let cmd = ClaudeHarness::default().build_print_command(&cfg).unwrap();
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&("CLAUDE_CODE_USE_BEDROCK".as_ref(), Some("1".as_ref()))));
        assert!(envs.contains(&("CLAUDE_CODE_USE_VERTEX".as_ref(), None)));
    }

    #[test]
    fn a_session_on_another_provider_is_reported() {
        let answer = serde_json::json!({"account": {"apiProvider": "firstParty"}});
        let bedrock = ProviderId::from("bedrock");
        let why = provider_mismatch(Some(&bedrock), &answer).unwrap();
        assert!(why.contains("runs on anthropic, not bedrock"));
        assert_eq!(
            provider_mismatch(Some(&ProviderId::from("anthropic")), &answer),
            None
        );
        // Nothing was chosen, or nothing reported.
        assert_eq!(provider_mismatch(None, &answer), None);
        assert_eq!(
            provider_mismatch(Some(&bedrock), &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn relocated_config_is_seeded_once_and_the_original_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, dir) = (tmp.path(), tmp.path().join(".claude"));
        // Nothing to copy yet.
        assert!(!seed_relocated_config(home, &dir).unwrap());

        std::fs::write(home.join(".claude.json"), "{\"a\":1}").unwrap();
        assert!(seed_relocated_config(home, &dir).unwrap());
        let read = |p: &Path| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(&dir.join(".claude.json")), "{\"a\":1}");
        assert_eq!(read(&home.join(".claude.json")), "{\"a\":1}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // What Claude wrote since is not overwritten.
        std::fs::write(dir.join(".claude.json"), "{\"a\":2}").unwrap();
        assert!(!seed_relocated_config(home, &dir).unwrap());
        assert_eq!(read(&dir.join(".claude.json")), "{\"a\":2}");
    }

    #[test]
    fn config_stays_put_unless_asked() {
        assert_eq!(ClaudeHarness::default().config_env().unwrap(), None);
        let paths = ClaudeHarness::default().sandbox_paths().writable;
        assert!(!paths.iter().any(|p| p.ends_with(".claude.json")));
    }

    #[test]
    fn only_the_deciding_part_of_the_config_is_watched() {
        let before = serde_json::json!({
            "numStartups": 1,
            "mcpServers": {},
            "projects": {"/w": {"allowedTools": [], "lastCost": 0.1}},
        });
        let mut after = before.clone();
        after["numStartups"] = 2.into();
        after["projects"]["/w"]["lastCost"] = 0.2.into();
        assert_eq!(config_that_matters(&before), config_that_matters(&after));

        after["projects"]["/w"]["allowedTools"] = serde_json::json!(["Bash(rm:*)"]);
        assert_ne!(config_that_matters(&before), config_that_matters(&after));
        let mut server = before.clone();
        server["mcpServers"] = serde_json::json!({"x": {"command": "sh"}});
        assert_ne!(config_that_matters(&before), config_that_matters(&server));
    }

    #[test]
    fn mcp_config_does_not_swallow_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let harness = ClaudeHarness {
            mcp_config_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/claude"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("run tests".into()),
            mcp_servers: crate::core::testing::sample_mcp_servers(),
            ..Default::default()
        };
        let a = args(&harness.build_print_command(&cfg).unwrap());
        assert_eq!(a[0], "--mcp-config");
        // A file: the servers' secrets stay off the command line.
        assert!(Path::new(&a[1]).starts_with(dir.path()) && Path::new(&a[1]).is_file());
        assert_eq!(a[2..], ["--", "run tests"]);
    }
}
