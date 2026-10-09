//! OpenAI Codex harness with two transports: `app-server` (long-lived JSON-RPC,
//! interactive approvals) and `exec --json` (one child per turn, no prompts).

pub mod app_server;
pub mod app_server_parse;
pub mod exec;
pub mod exec_parse;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};
use crate::core::guard::Guarded;
use crate::core::jsonrpc;
use crate::core::process::ProbeProcess;
use crate::core::sandbox::{Sandbox, SandboxLevel, SandboxPaths};
use crate::core::{
    Capabilities, HarnessId, McpChannel, McpServer, McpSupport, McpTransport, ModelRef,
    PermissionPolicy, PolicySupport, ProviderId, RewindSupport, SessionConfig, SessionHandle,
    SubagentSupport,
};

/// What Codex's own sandbox holds to. The permission policy has no say in
/// it: that decides only what is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnSandbox {
    /// unharness's sandbox is around the process. Codex's own (bubblewrap)
    /// cannot start inside it, so it is switched off and ours holds.
    External,
    /// Codex's own sandbox at the level the user set.
    Level(SandboxLevel),
}

impl OwnSandbox {
    pub fn for_session(sandbox: &Sandbox) -> Self {
        if sandbox.is_active() {
            OwnSandbox::External
        } else {
            OwnSandbox::Level(sandbox.wanted())
        }
    }

    /// Codex's name for it: `-s`, `sandbox_mode`, `thread/start`'s `sandbox`.
    pub fn mode(self) -> &'static str {
        match self {
            OwnSandbox::External | OwnSandbox::Level(SandboxLevel::Off) => "danger-full-access",
            OwnSandbox::Level(SandboxLevel::ReadOnly) => "read-only",
            OwnSandbox::Level(SandboxLevel::WorkspaceWrite) => "workspace-write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexTransport {
    /// app-server when available, else exec.
    #[default]
    Auto,
    AppServer,
    Exec,
}

impl CodexTransport {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "auto" => Some(CodexTransport::Auto),
            "app-server" | "appserver" | "app_server" => Some(CodexTransport::AppServer),
            "exec" => Some(CodexTransport::Exec),
            _ => None,
        }
    }
}

pub struct CodexHarness {
    pub transport: CodexTransport,
}

impl Default for CodexHarness {
    fn default() -> Self {
        CodexHarness {
            transport: CodexTransport::Auto,
        }
    }
}

pub static DESCRIPTOR: HarnessDescriptor = HarnessDescriptor {
    id: HarnessId::CODEX,
    display_name: "Codex (codex)",
    short_name: "Codex",
    binary_names: &["codex"],
    providers: ProviderSource::Static(BUILT_IN_PROVIDERS),
};

/// The providers Codex has without configuration, which a
/// `[model_providers.<id>]` of the same name may not replace (0.157.0,
/// "reserved built-in provider IDs"). Others come from its config.
const BUILT_IN_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI"),
    ("ollama", "Ollama"),
    ("lmstudio", "LM Studio"),
    ("amazon-bedrock", "Amazon Bedrock"),
];

/// The `-c` value that has Codex use `provider`.
pub fn provider_override(provider: &ProviderId) -> String {
    format!("model_provider={}", json!(provider.as_str()))
}

/// Why the provider Codex reports (`modelProvider` in the answer to
/// `thread/start` or `thread/resume`) is not the one chosen, if it is not.
pub fn provider_mismatch(chosen: Option<&ProviderId>, reported: &str) -> Option<String> {
    let chosen = chosen?;
    (reported != chosen.as_str())
        .then(|| format!("Codex runs on {reported}, not {chosen}, in this thread"))
}

/// The built-in providers, then the ones `model_providers` (as
/// `config/read` gives it) adds. A built-in one appears there too when its
/// few settable fields are set, with an empty name.
fn with_configured(configured: &serde_json::Map<String, Value>) -> Vec<(ProviderId, String)> {
    let mut providers: Vec<(ProviderId, String)> = BUILT_IN_PROVIDERS
        .iter()
        .map(|(id, name)| (ProviderId::from(*id), name.to_string()))
        .collect();
    for (id, provider) in configured {
        if providers.iter().any(|(known, _)| known.as_str() == id) {
            continue;
        }
        let name = provider
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(id)
            .to_string();
        providers.push((ProviderId::new(id.clone()), name));
    }
    providers
}

/// The `[model_providers.<id>]` tables of a Codex `config.toml`, in the
/// shape `config/read` gives them (only `name` is used).
fn providers_in_config(config: &str) -> serde_json::Map<String, Value> {
    config
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| t.get("model_providers")?.as_table().cloned())
        .map(|providers| {
            providers
                .into_iter()
                .map(|(id, p)| {
                    let name = p.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                    (id, json!({ "name": name }))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// `model_provider` in a Codex `config.toml`, or in the profile it selects.
fn configured_provider(config: &str) -> Option<String> {
    let table = config.parse::<toml::Table>().ok()?;
    let in_profile = table
        .get("profile")
        .and_then(|p| p.as_str())
        .and_then(|p| table.get("profiles")?.get(p)?.get("model_provider"));
    in_profile
        .or_else(|| table.get("model_provider"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
}

pub const EFFORT_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

/// Static fallback, checked 2026-10-02 against `model/list`.
const FALLBACK_MODELS: &[(&str, &str, &str)] = &[
    (
        "gpt-6-astra",
        "GPT-6-Astra",
        "Frontier intelligence for the most demanding work",
    ),
    ("gpt-5.5", "GPT-5.5", "Strong general coding model"),
    (
        "gpt-5.3-codex-spark",
        "GPT-5.3 Codex Spark",
        "Fast coding model",
    ),
];

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

impl CodexHarness {
    pub fn new(transport: CodexTransport) -> Self {
        CodexHarness { transport }
    }

    /// Resolve `Auto` against the installed binary.
    fn effective_transport(&self, binary: &Path) -> CodexTransport {
        match self.transport {
            CodexTransport::Auto => {
                let ok = Command::new(binary)
                    .args(["app-server", "--help"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if ok {
                    CodexTransport::AppServer
                } else {
                    CodexTransport::Exec
                }
            }
            t => t,
        }
    }
}

/// What `codex exec` can hold to: it cannot prompt, so there is no `ask`.
fn exec_policies() -> Vec<PolicySupport> {
    vec![
        PolicySupport::full(PermissionPolicy::AcceptEdits),
        PolicySupport::full(PermissionPolicy::Auto),
        PolicySupport::full(PermissionPolicy::Bypass),
    ]
}

impl Harness for CodexHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        &DESCRIPTOR
    }

    fn capabilities(&self) -> Capabilities {
        let app_server = self.transport != CodexTransport::Exec;
        Capabilities {
            streaming_input: app_server,
            text_deltas: app_server,
            thinking: true,
            tool_events: true,
            interactive_permissions: app_server,
            permission_policies: if app_server {
                PermissionPolicy::ALL
                    .iter()
                    .copied()
                    .map(PolicySupport::full)
                    .collect()
            } else {
                exec_policies()
            },
            effort_levels: EFFORT_LEVELS.iter().map(|s| s.to_string()).collect(),
            resume_by_id: true,
            live_model_list: app_server,
            // `-c model_provider=…` when the process starts.
            multi_provider: true,
            provider_per_process: true,
            ask_user_question: app_server,
            interrupt: true,
            usage_reporting: true,
            image_input: true,
            file_input: false,
            plan_updates: true,
            // A sub-agent thread has a name but no task description, and
            // says what it is doing only through its tool calls.
            subagents: SubagentSupport {
                reported: app_server,
                described: false,
                stop: app_server,
                // Nothing follows on the main thread when one ends.
                report_turn: false,
            },
            steer: app_server,
            compaction: app_server,
            context_usage: app_server,
            rate_limits: app_server,
            rewind: RewindSupport {
                conversation: app_server,
                anchors_survive_fork: true,
            },
            fork: app_server,
            // `-c mcp_servers.<name>.…` overrides, on either transport.
            mcp: McpSupport::via(McpChannel::CommandLine, true),
            // `collaborationMode` on `turn/start` (0.157.0 schema); `exec`
            // has no such mode.
            plan_mode: app_server,
            // Its commands are its own interface's. Its skills are, on
            // app-server, what `skills/list` answers, and the session
            // reports them; `/name` for one goes as `$name` with the skill
            // (`app_server::skill_call`). `exec` lists none.
            slash_commands: false,
            // Checked on 0.157.0 (app-server): quit unprompted, no thread
            // was written.
            start_unprompted: true,
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

    /// The built-in providers and the ones Codex's configuration adds
    /// (`config/read`), else the built-in ones alone.
    fn list_providers(
        &self,
        binary: &Path,
        sandbox: &Sandbox,
    ) -> Result<Vec<(ProviderId, String)>> {
        // Without an answer, the user's config.toml as it reads.
        let configured = query(binary, sandbox, "config/read", json!({}))
            .ok()
            .and_then(|r| r.pointer("/config/model_providers")?.as_object().cloned())
            .unwrap_or_else(|| {
                std::fs::read_to_string(expand_home(&codex_home()).join("config.toml"))
                    .map(|text| providers_in_config(&text))
                    .unwrap_or_default()
            });
        Ok(with_configured(&configured))
    }

    /// `model_provider` in Codex's `config.toml` (its profile's first),
    /// else OpenAI. A thread reports the one it runs on.
    fn default_provider(&self) -> Option<ProviderId> {
        let home = dirs::home_dir().unwrap_or_default();
        let dir = codex_home();
        let dir = dir.strip_prefix("~").map(|p| home.join(p)).unwrap_or(dir);
        let configured = std::fs::read_to_string(dir.join("config.toml"))
            .ok()
            .and_then(|text| configured_provider(&text));
        Some(ProviderId::new(
            configured.unwrap_or_else(|| "openai".into()),
        ))
    }

    /// `model/list` is OpenAI's catalog whichever provider is configured
    /// (0.157.0), so other providers list nothing.
    fn list_models(
        &self,
        binary: &Path,
        provider: &ProviderId,
        sandbox: &Sandbox,
    ) -> Result<Vec<ModelInfo>> {
        if provider.as_str() != "openai" {
            return Ok(Vec::new());
        }
        let live = match self.effective_transport(binary) {
            CodexTransport::Exec => Vec::new(),
            _ => query_models(binary, sandbox).unwrap_or_default(),
        };
        if !live.is_empty() {
            return Ok(live
                .iter()
                .filter(|m| !m.get("hidden").and_then(Value::as_bool).unwrap_or(false))
                .map(|m| {
                    let id = m
                        .get("model")
                        .or_else(|| m.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let efforts: Vec<String> = m
                        .get("supportedReasoningEfforts")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| e.get("reasoningEffort").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    ModelInfo {
                        model_ref: ModelRef::new(HarnessId::CODEX, "openai", id),
                        display_name: m
                            .get("displayName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        description: m
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        effort_levels: (!efforts.is_empty()).then_some(efforts),
                    }
                })
                .collect());
        }
        Ok(FALLBACK_MODELS
            .iter()
            .map(|(id, name, desc)| ModelInfo {
                model_ref: ModelRef::new(HarnessId::CODEX, "openai", *id),
                display_name: name.to_string(),
                description: Some(desc.to_string()),
                effort_levels: None,
            })
            .collect())
    }

    /// `packages` holds the standalone install's binary; `agents` the roles
    /// every project gets.
    fn guarded(&self, workspace: &Path) -> Vec<Guarded> {
        let home = codex_home();
        let workspace = workspace.to_path_buf();
        let mut guarded = vec![
            Guarded::Projected {
                path: home.join("config.toml"),
                project: Arc::new(move |bytes| config_without_own_trust(bytes, &workspace)),
            },
            Guarded::File(home.join("hooks.json")),
            Guarded::File(home.join("AGENTS.md")),
        ];
        for tree in ["prompts", "skills", "agents", "packages"] {
            guarded.push(Guarded::Tree(home.join(tree)));
        }
        guarded
    }

    /// Codex merges a `-c mcp_servers.<name>` override key by key into a
    /// server of that name in `config.toml` (checked on 0.157.0 with `codex
    /// mcp get`): a `command` over its `url` stops Codex from starting, and
    /// its `args` and `env` stay.
    fn own_mcp_servers(&self) -> Vec<String> {
        let home = dirs::home_dir().unwrap_or_default();
        let dir = codex_home();
        let dir = dir.strip_prefix("~").map(|p| home.join(p)).unwrap_or(dir);
        std::fs::read_to_string(dir.join("config.toml"))
            .map(|text| mcp_server_names(&text))
            .unwrap_or_default()
    }

    fn sandbox_paths(&self) -> SandboxPaths {
        SandboxPaths {
            writable: vec![codex_home()],
        }
    }

    /// `--print` is `codex exec` whatever the session transport. Codex's
    /// own interface asks when its model decides to (`on-request`); the CLI
    /// takes no `untrusted` there, so neither holds to `ask`.
    fn print_policies(&self, _print_mode: bool) -> Vec<PolicySupport> {
        exec_policies()
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        match self.effective_transport(&cfg.binary) {
            CodexTransport::Exec => crate::core::per_turn::start(cfg, Arc::new(exec::CodexExec)),
            _ => app_server::start(cfg),
        }
    }

    fn build_print_command(&self, cfg: &PrintConfig) -> Result<Command> {
        let mut cmd = Command::new(&cfg.binary);
        cmd.current_dir(&cfg.cwd);
        let own = OwnSandbox::for_session(&cfg.sandbox);
        if cfg.print_mode {
            cmd.arg("exec");
            if let Some(id) = &cfg.resume {
                cmd.arg("resume").arg(id);
            }
            cmd.arg("--skip-git-repo-check");
            if cfg.format.as_deref().is_some_and(|f| f != "text") {
                cmd.arg("--json");
            }
            if let Some(p) = cfg.policy {
                if cfg.resume.is_some() {
                    for o in exec::policy_config_overrides(p, own)? {
                        cmd.arg("-c").arg(o);
                    }
                } else {
                    cmd.args(exec::policy_args(p, own)?);
                }
            }
        } else {
            if let Some(id) = &cfg.resume {
                cmd.arg("resume").arg(id);
            }
            if let Some(p) = cfg.policy {
                cmd.args(exec::policy_args(p, own)?);
            }
        }
        if let Some(m) = &cfg.model {
            cmd.arg("-m").arg(&m.model);
        }
        if let Some(p) = &cfg.provider {
            cmd.arg("-c").arg(provider_override(p));
        }
        if let Some(e) = &cfg.effort {
            cmd.arg("-c").arg(format!("model_reasoning_effort=\"{e}\""));
        }
        mcp_args(&cfg.mcp_servers).apply(&mut cmd);
        cmd.args(&cfg.extra_args);
        if let Some(p) = &cfg.prompt {
            cmd.arg(p);
        }
        Ok(cmd)
    }
}

/// The `[mcp_servers.<name>]` tables of a Codex `config.toml`.
fn mcp_server_names(config: &str) -> Vec<String> {
    config
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| Some(t.get("mcp_servers")?.as_table()?.keys().cloned().collect()))
        .unwrap_or_default()
}

/// How a set of MCP servers is handed to one Codex process.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct McpArgs {
    /// `-c` values, in the shape of `[mcp_servers.<name>]` in Codex's
    /// `config.toml`, which is not written.
    pub overrides: Vec<String>,
    /// Environment for the Codex process: the header values, which the
    /// overrides only name (`env_http_headers`).
    pub env: Vec<(String, String)>,
}

impl McpArgs {
    pub fn apply(&self, cmd: &mut Command) {
        for o in &self.overrides {
            cmd.arg("-c").arg(o);
        }
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
    }
}

/// Define these servers for one run. Header values go through the
/// environment, since a command line is readable by every local user. A
/// command server's `env` has no such way: Codex can pass on variables of
/// its own environment (`env_vars`), but only under the same name, and
/// setting them on Codex would hand them to everything else it starts.
pub fn mcp_args(servers: &[McpServer]) -> McpArgs {
    let text = |s: &String| toml::Value::String(s.clone());
    let table = |map: &std::collections::BTreeMap<String, String>| {
        toml::Value::Table(map.iter().map(|(k, v)| (k.clone(), text(v))).collect())
    };
    let mut out = McpArgs::default();
    for s in servers {
        let mut set = |key: &str, value: toml::Value| {
            out.overrides
                .push(format!("mcp_servers.{}.{key}={value}", s.name));
        };
        match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                set("command", text(command));
                if !args.is_empty() {
                    set("args", toml::Value::Array(args.iter().map(text).collect()));
                }
                if !env.is_empty() {
                    set("env", table(env));
                }
            }
            McpTransport::Http { url, headers } => {
                set("url", text(url));
                let mut named = std::collections::BTreeMap::new();
                for (header, value) in headers {
                    let var = format!("UNHARNESS_MCP_HEADER_{}", out.env.len());
                    out.env.push((var.clone(), value.clone()));
                    named.insert(header.clone(), var);
                }
                if !named.is_empty() {
                    out.overrides.push(format!(
                        "mcp_servers.{}.env_http_headers={}",
                        s.name,
                        table(&named)
                    ));
                }
            }
        }
    }
    out
}

/// `config.toml` without the trust entry of `workspace`, which Codex adds by
/// itself the first time it runs there. Trust for any other directory, and
/// everything else in the file, still counts.
fn config_without_own_trust(bytes: &[u8], workspace: &Path) -> Option<String> {
    let mut config: toml::Table = toml::from_str(std::str::from_utf8(bytes).ok()?).ok()?;
    if let Some(toml::Value::Table(projects)) = config.get_mut("projects") {
        projects.remove(workspace.to_string_lossy().as_ref());
        if projects.is_empty() {
            config.remove("projects");
        }
    }
    Some(config.to_string())
}

/// Where Codex keeps sessions, credentials and logs.
fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~/.codex"))
}

fn auth_status(binary: &Path) -> AuthInfo {
    let out = Command::new(binary).args(["login", "status"]).output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let s = if s.is_empty() {
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            } else {
                s
            };
            AuthInfo {
                authenticated: !s.to_lowercase().contains("not logged in"),
                details: (!s.is_empty()).then_some(s),
            }
        }
        _ => AuthInfo {
            authenticated: false,
            details: Some("not logged in (run `codex login`)".into()),
        },
    }
}

/// Ask a throwaway `codex app-server` for `model/list`.
pub fn query_models(binary: &Path, sandbox: &Sandbox) -> Result<Vec<Value>> {
    let result = query(binary, sandbox, "model/list", json!({}))?;
    Ok(result
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// The result of one request to a throwaway `codex app-server`, in the
/// sandbox a session would get: Codex reads its own configuration, which
/// a session can write.
fn query(binary: &Path, sandbox: &Sandbox, method: &str, params: Value) -> Result<Value> {
    let mut cmd = Command::new(binary);
    cmd.arg("app-server");
    if sandbox.is_active() {
        // As for a session: its own sandbox cannot start inside ours.
        cmd.args(["-c", "sandbox_mode=\"danger-full-access\""]);
    }
    let mut probe = ProbeProcess::spawn(cmd, sandbox).context("start codex app-server")?;
    probe.write_line(&jsonrpc::request(
        1,
        "initialize",
        json!({"clientInfo": {"name": "unharness", "version": env!("CARGO_PKG_VERSION")}}),
    ))?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    loop {
        let line = probe
            .next_line(deadline)
            .map_err(|_| anyhow::anyhow!("codex app-server did not answer {method}"))?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v.get("id").and_then(Value::as_u64) {
            Some(1) if v.get("result").is_some() => {
                probe.write_line(&jsonrpc::notification("initialized", Value::Null))?;
                probe.write_line(&jsonrpc::request(2, method, params.clone()))?;
            }
            Some(2) => {
                if let Some(err) = v.get("error") {
                    anyhow::bail!("codex app-server: {method}: {err}");
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(cmd: &Command) -> String {
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A user-level role is loaded in every project: a change is reported.
    #[test]
    fn user_level_roles_are_guarded() {
        let h = CodexHarness::default();
        let trees: Vec<_> = h
            .guarded(Path::new("/w"))
            .into_iter()
            .filter_map(|g| match g {
                Guarded::Tree(p) => Some(p),
                _ => None,
            })
            .collect();
        assert!(trees.contains(&codex_home().join("agents")), "{trees:?}");
    }

    #[test]
    fn the_configured_provider_is_the_profiles_first() {
        assert_eq!(configured_provider(""), None);
        assert_eq!(
            configured_provider("model_provider = \"ollama\"").as_deref(),
            Some("ollama")
        );
        let profiled = r#"
model_provider = "ollama"
profile = "work"

[profiles.work]
model_provider = "azure"

[profiles.home]
model_provider = "lmstudio"
"#;
        assert_eq!(configured_provider(profiled).as_deref(), Some("azure"));
        let without = profiled.replace("profile = \"work\"", "profile = \"none\"");
        assert_eq!(configured_provider(&without).as_deref(), Some("ollama"));
    }

    #[test]
    fn configured_providers_are_read_from_the_file_without_codex() {
        let config = r#"
model = "x"

[model_providers.llama]
name = "llama-swap"
base_url = "http://x/v1"

[model_providers.amazon-bedrock.aws]
region = "us-east-1"
"#;
        let providers = with_configured(&providers_in_config(config));
        let ids: Vec<&str> = providers.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            ["openai", "ollama", "lmstudio", "amazon-bedrock", "llama"]
        );
        assert_eq!(providers[4].1, "llama-swap");
        assert!(providers_in_config("not toml [").is_empty());
    }

    #[test]
    fn configured_providers_follow_the_built_in_ones_once() {
        let configured = json!({
            "amazon-bedrock": {"name": "", "aws": {"region": "us-east-1"}},
            "llama": {"name": "llama-swap", "base_url": "http://x/v1"},
            "bare": {"name": " ", "base_url": "http://y/v1"},
        });
        let providers = with_configured(configured.as_object().unwrap());
        let names: Vec<String> = providers
            .iter()
            .map(|(id, name)| format!("{id}={name}"))
            .collect();
        assert_eq!(
            names,
            [
                "openai=OpenAI",
                "ollama=Ollama",
                "lmstudio=LM Studio",
                "amazon-bedrock=Amazon Bedrock",
                "bare=bare",
                "llama=llama-swap"
            ]
        );
    }

    #[test]
    fn a_chosen_provider_is_a_config_override() {
        assert_eq!(
            provider_override(&ProviderId::from("amazon-bedrock")),
            "model_provider=\"amazon-bedrock\""
        );
        // Quoted as a TOML string, whatever the id holds.
        assert_eq!(
            provider_override(&ProviderId::from("a\"b")),
            "model_provider=\"a\\\"b\""
        );
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/codex"),
            cwd: PathBuf::from("/tmp"),
            print_mode: true,
            provider: Some(ProviderId::from("ollama")),
            ..Default::default()
        };
        let a = args(&CodexHarness::default().build_print_command(&cfg).unwrap());
        assert!(a.contains("-c model_provider=\"ollama\""), "{a}");
        assert_eq!(
            provider_mismatch(Some(&ProviderId::from("ollama")), "openai").as_deref(),
            Some("Codex runs on openai, not ollama, in this thread")
        );
        assert_eq!(provider_mismatch(None, "openai"), None);
    }

    #[test]
    fn print_and_passthrough_commands() {
        let base = PrintConfig {
            binary: PathBuf::from("/bin/codex"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("fix it".into()),
            print_mode: true,
            model: Some(ModelRef::new(HarnessId::CODEX, "openai", "gpt-5.5")),
            provider: None,
            effort: Some("low".into()),
            policy: Some(PermissionPolicy::AcceptEdits),
            format: None,
            resume: None,
            extra_args: vec![],
            mcp_servers: Vec::new(),
            // Wanted at workspace-write, and no backend to provide it.
            sandbox: Sandbox::Off {
                unavailable: Some("no kernel".into()),
                wanted: SandboxLevel::WorkspaceWrite,
            },
        };
        let h = CodexHarness::default();
        assert_eq!(
            args(&h.build_print_command(&base).unwrap()),
            "exec --skip-git-repo-check -s workspace-write -m gpt-5.5 -c model_reasoning_effort=\"low\" fix it"
        );
        let off = PrintConfig {
            sandbox: Sandbox::off(),
            ..base.clone()
        };
        assert_eq!(
            args(&h.build_print_command(&off).unwrap()),
            "exec --skip-git-repo-check -s danger-full-access -m gpt-5.5 -c model_reasoning_effort=\"low\" fix it"
        );
        let resumed = PrintConfig {
            resume: Some("t1".into()),
            format: Some("stream-json".into()),
            ..base.clone()
        };
        let a = args(&h.build_print_command(&resumed).unwrap());
        assert!(a.starts_with("exec resume t1 --skip-git-repo-check --json -c sandbox_mode="));
        let tui = PrintConfig {
            print_mode: false,
            prompt: None,
            policy: Some(PermissionPolicy::Bypass),
            sandbox: Sandbox::off(),
            ..base.clone()
        };
        assert_eq!(
            args(&h.build_print_command(&tui).unwrap()),
            "--dangerously-bypass-approvals-and-sandbox -m gpt-5.5 -c model_reasoning_effort=\"low\""
        );
        // `bypass` keeps the level Codex's own sandbox holds to.
        let tui_sandboxed = PrintConfig {
            sandbox: base.sandbox.clone(),
            ..tui
        };
        assert_eq!(
            args(&h.build_print_command(&tui_sandboxed).unwrap()),
            "-s workspace-write -c approval_policy=\"never\" -m gpt-5.5 -c model_reasoning_effort=\"low\""
        );
        for print_mode in [true, false] {
            let ask = PrintConfig {
                print_mode,
                policy: Some(PermissionPolicy::Ask),
                ..resumed.clone()
            };
            assert!(h.build_print_command(&ask).is_err());
        }
    }

    #[test]
    fn capabilities_by_transport() {
        let app = CodexHarness::new(CodexTransport::AppServer).capabilities();
        assert!(app.interactive_permissions && app.ask_user_question);
        assert!(
            app.supports_policy(PermissionPolicy::Ask)
                .unwrap()
                .degraded
                .is_none()
        );
        let exec = CodexHarness::new(CodexTransport::Exec).capabilities();
        assert!(!exec.interactive_permissions);
        assert!(exec.supports_policy(PermissionPolicy::Ask).is_none());
        // `--print` runs `exec` on either transport.
        for transport in [CodexTransport::AppServer, CodexTransport::Exec] {
            for print_mode in [true, false] {
                let policies = CodexHarness::new(transport).print_policies(print_mode);
                assert!(policies.iter().all(|p| p.policy != PermissionPolicy::Ask));
            }
        }
        assert_eq!(
            CodexTransport::parse("app_server"),
            Some(CodexTransport::AppServer)
        );
        assert_eq!(CodexTransport::parse("x"), None);
    }

    #[test]
    fn own_trust_entry_is_not_a_config_change() {
        let ws = Path::new("/w/proj");
        let before = b"model = \"m\"\n";
        let own = b"model = \"m\"\n\n[projects.\"/w/proj\"]\ntrust_level = \"trusted\"\n";
        let other = b"model = \"m\"\n\n[projects.\"/elsewhere\"]\ntrust_level = \"trusted\"\n";
        let base = config_without_own_trust(before, ws).unwrap();
        assert_eq!(config_without_own_trust(own, ws).unwrap(), base);
        assert_ne!(config_without_own_trust(other, ws).unwrap(), base);
        assert_ne!(
            config_without_own_trust(b"model = \"x\"\n", ws).unwrap(),
            base
        );
        assert_eq!(config_without_own_trust(b"not [toml", ws), None);
    }

    #[test]
    fn own_servers_are_read_from_the_config() {
        let config = "model = \"x\"\n[mcp_servers.cq]\ncommand = \"cq\"\n[mcp_servers.docs]\nurl = \"https://e.com\"\n";
        assert_eq!(mcp_server_names(config), ["cq", "docs"]);
        assert!(mcp_server_names("model = \"x\"").is_empty());
        assert!(mcp_server_names("not toml [").is_empty());
    }

    #[test]
    fn mcp_servers_are_config_overrides() {
        let servers = crate::core::testing::sample_mcp_servers();
        let mcp = mcp_args(&servers);
        assert_eq!(
            mcp.overrides,
            [
                r#"mcp_servers.files.command="/usr/bin/files-mcp""#,
                r#"mcp_servers.files.args=["--root", "/my work"]"#,
                r#"mcp_servers.files.env={ TOKEN = 't"1' }"#,
                r#"mcp_servers.docs.url="https://example.com/mcp""#,
                r#"mcp_servers.docs.env_http_headers={ Authorization = "UNHARNESS_MCP_HEADER_0" }"#,
            ]
        );
        // The header's value is in the environment, not on the command line.
        assert_eq!(
            mcp.env,
            [("UNHARNESS_MCP_HEADER_0".to_string(), "Bearer x".to_string())]
        );
        // Each value is TOML, as `-c` parses it.
        for o in &mcp.overrides {
            let (_, value) = o.split_once('=').unwrap();
            assert!(format!("v = {value}").parse::<toml::Table>().is_ok(), "{o}");
        }

        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/codex"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("fix it".into()),
            print_mode: true,
            mcp_servers: servers,
            ..Default::default()
        };
        let cmd = CodexHarness::default().build_print_command(&cfg).unwrap();
        let a = args(&cmd);
        assert!(
            a.starts_with("exec --skip-git-repo-check -c mcp_servers.files.command=")
                && a.ends_with(" fix it")
                && !a.contains("Bearer"),
            "{a}"
        );
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "UNHARNESS_MCP_HEADER_0" && v.is_some_and(|v| v == "Bearer x"))
        );
    }
}
