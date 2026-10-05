//! Claude Code harness.

pub mod parse;
pub mod transport;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde_json::Value;

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary,
};
use crate::core::guard::Guarded;
use crate::core::sandbox::SandboxPaths;
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
    providers: ProviderSource::Static(&[("anthropic", "Anthropic")]),
};

pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Static catalog, checked 2026-10-02. Aliases resolve to the latest release.
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
        for tree in ["agents", "commands", "skills", "hooks"] {
            guarded.push(Guarded::Tree(state.join(tree)));
        }
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
            live_model_list: false,
            multi_provider: false,
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

    fn list_models(&self, _binary: &Path, provider: &ProviderId) -> Result<Vec<ModelInfo>> {
        if provider.as_str() != "anthropic" {
            return Ok(Vec::new());
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

    #[test]
    fn print_command_shape() {
        let cfg = PrintConfig {
            binary: PathBuf::from("/bin/claude"),
            cwd: PathBuf::from("/tmp"),
            prompt: Some("run tests".into()),
            print_mode: true,
            model: Some(ModelRef::new(HarnessId::CLAUDE, "anthropic", "sonnet")),
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
        assert_eq!(caps.permission_policies.len(), 4);
        assert!(caps.supports_effort("xhigh"));
        let models = ClaudeHarness::default()
            .list_models(Path::new("claude"), &ProviderId::from("anthropic"))
            .unwrap();
        assert!(models.iter().any(|m| m.model_ref.model == "opus"));
        assert!(
            ClaudeHarness::default()
                .list_models(Path::new("claude"), &ProviderId::from("openai"))
                .unwrap()
                .is_empty()
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
