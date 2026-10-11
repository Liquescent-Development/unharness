//! Generic harness for any agent that speaks the Agent Client Protocol
//! (<https://agentclientprotocol.com>). One instance per agent; the agent's
//! command line comes from config or from [`PRESETS`], never from the TUI.
//!
//! Verified against `@agentclientprotocol/claude-agent-acp` 0.85.1 and
//! `@agentclientprotocol/codex-acp` 2.1.1 (see `fixtures/`). What an agent
//! can do is mostly learned at the handshake, so the static capabilities are
//! the protocol's baseline and the session reports the rest.

pub mod auth;
pub mod elicitation;
pub mod modes;
pub mod parse;
pub mod spawn;
pub mod transport;

use std::path::{Path, PathBuf};

pub use modes::ModeMap;
pub use spawn::{KnownModels, SharedModels, SpawnChoice, SpawnEnv, SpawnPlan};

use anyhow::{Result, bail};

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary, which,
};
use crate::core::sandbox::{Sandbox, SandboxPaths};
use crate::core::{
    Capabilities, HarnessId, McpChannel, McpSupport, PermissionPolicy, PolicySupport, ProviderId,
    RewindSupport, SessionConfig, SessionHandle, SubagentSupport,
};

/// Agents with a native ACP mode: (id, display name, command). Each is
/// offered only when its binary is on PATH. The launch commands are the ones
/// published in the ACP agent registry; none of these was installed when
/// this was written, so they are unverified until someone records a fixture.
pub const PRESETS: &[(&str, &str, &[&str])] = &[
    ("gemini", "Gemini CLI", &["gemini", "--acp"]),
    ("opencode", "OpenCode", &["opencode", "acp"]),
    ("goose", "Goose", &["goose", "acp"]),
    ("copilot", "GitHub Copilot", &["copilot", "--acp"]),
    ("cursor", "Cursor Agent", &["cursor-agent", "acp"]),
    ("qwen", "Qwen Code", &["qwen", "--acp"]),
    ("kiro", "Kiro", &["kiro-cli", "acp"]),
];

/// Where each preset's agent keeps its own state, for the sandbox. As
/// unverified as the presets; a config-defined agent lists its directories
/// in `[harnesses.<name>].sandbox_writable`.
fn preset_state(name: &str) -> &'static [&'static str] {
    match name {
        "gemini" => &["~/.gemini"],
        "opencode" => &[
            "~/.config/opencode",
            "~/.local/share/opencode",
            "~/.local/state/opencode",
            "~/.cache/opencode",
        ],
        "goose" => &[
            "~/.config/goose",
            "~/.local/share/goose",
            "~/.local/state/goose",
        ],
        "copilot" => &["~/.copilot"],
        "cursor" => &["~/.cursor"],
        "qwen" => &["~/.qwen"],
        "kiro" => &["~/.kiro"],
        _ => &[],
    }
}

const PROVIDERS: &[(&str, &str)] = &[(parse::PROVIDER, "ACP agent")];

pub struct AcpHarness {
    descriptor: &'static HarnessDescriptor,
    /// Arguments after the binary (`--acp`, `acp`, an npx package…).
    args: Vec<String>,
    /// Came from [`PRESETS`] rather than the user's config.
    preset: bool,
    /// The user's config says this agent asks before it acts
    /// (`asks_permission`).
    asks: bool,
    /// The config option that is this agent's reasoning effort, when it is
    /// not under the spec's `thought_level` category.
    effort_option: Option<String>,
    /// Which of the agent's own session modes backs each policy.
    modes: ModeMap,
    /// Builds the agent's environment for its model and effort, which are
    /// then never set over the protocol.
    spawn_env: Option<SpawnEnv>,
    /// What the agent's sessions said about its models, this run.
    known: SharedModels,
    /// The command that signs the agent in, told to the user when a
    /// session fails for want of sign-in (`auth.rs`). Never run.
    sign_in: Option<String>,
    /// A relative path in an edit's `diff` is in the session's directory.
    relative_edits: bool,
}

impl AcpHarness {
    /// `command` is the full agent command line; its first word is the binary.
    pub fn new(name: &str, display_name: Option<&str>, command: &[String]) -> Result<Self> {
        let Some((binary, args)) = command.split_first() else {
            bail!("harness '{name}' has protocol = \"acp\" but no command");
        };
        // Descriptors are `'static` for every harness; a config-defined one
        // is built once per run and kept for its lifetime.
        let leak = |s: &str| -> &'static str { Box::leak(s.to_string().into_boxed_str()) };
        let display = leak(display_name.unwrap_or(name));
        let descriptor = Box::leak(Box::new(HarnessDescriptor {
            id: HarnessId::intern(name),
            display_name: display,
            short_name: display,
            binary_names: Box::leak(vec![leak(binary)].into_boxed_slice()),
            providers: ProviderSource::Static(PROVIDERS),
        }));
        Ok(AcpHarness {
            descriptor,
            args: args.to_vec(),
            preset: false,
            asks: false,
            effort_option: None,
            modes: ModeMap::default(),
            spawn_env: None,
            known: SharedModels::default(),
            sign_in: None,
            relative_edits: false,
        })
    }

    /// Whether the agent is known to ask before it writes, runs a command
    /// or reaches out, which is what the `ask` policy stands on.
    pub fn asking_permission(self, asks: bool) -> Self {
        AcpHarness { asks, ..self }
    }

    /// Take the config option with this id as the agent's reasoning effort,
    /// whatever its category (Mistral Vibe offers its level as `thinking`).
    /// For a harness that knows what setting the option does: unharness then
    /// sends `session/set_config_option` for it on `/effort` and for a
    /// configured effort. Nothing declares one from config.
    pub fn declaring_effort_option(self, id: &str) -> Self {
        AcpHarness {
            effort_option: Some(id.to_string()),
            ..self
        }
    }

    /// Hold the agent to these of its session modes, one per policy (see
    /// [`ModeMap`]): from `[harnesses.<name>] modes`, or declared by a
    /// harness that knows its agent. An agent with modes and no map gets no
    /// policy below `bypass` (`modes::mode_step`).
    pub fn with_modes(self, modes: ModeMap) -> Self {
        AcpHarness { modes, ..self }
    }

    /// Choose the agent's model and effort when its process starts, with
    /// the environment `build` returns, and never with
    /// `session/set_config_option`: for an agent that writes a model or
    /// effort set over the protocol into its own configuration (Mistral
    /// Vibe, whose sessions also keep the ones they last ran). A change of
    /// either starts a new process, and a new session once the old one was
    /// prompted (`Capabilities::model_per_process`). The effort option is
    /// still the one declared with [`declaring_effort_option`]
    /// (`Self::declaring_effort_option`), read for its levels only.
    pub fn choosing_at_spawn(self, build: SpawnEnv) -> Self {
        AcpHarness {
            spawn_env: Some(build),
            ..self
        }
    }

    /// The command that signs the agent in (`vibe --setup`), from
    /// `[harnesses.<name>] sign_in` or declared by a harness that knows
    /// its agent. It is only told to the user, with the sign-in methods
    /// the agent offers, when a session fails for want of sign-in; the
    /// generic text without one points at the agent's own command line.
    pub fn signing_in_with(self, command: Option<&str>) -> Self {
        AcpHarness {
            sign_in: command
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string),
            ..self
        }
    }

    /// The command that signs the agent in, if one is known.
    pub fn sign_in(&self) -> Option<&str> {
        self.sign_in.as_deref()
    }

    /// Take a relative path in an edit's `diff` content as one in the
    /// session's directory, for allow rules
    /// (`AcpParser::resolving_relative_edits`). Only for an agent known to
    /// write it there and to name it in `locations` (Mistral Vibe 2.26.1);
    /// for any other a relative path stays unknown.
    pub fn resolving_relative_edits(self) -> Self {
        AcpHarness {
            relative_edits: true,
            ..self
        }
    }

    /// The presets whose binary is installed and whose name is not taken.
    pub fn installed_presets(taken: &[HarnessId]) -> Vec<AcpHarness> {
        PRESETS
            .iter()
            .filter(|(name, _, command)| {
                !taken.contains(&HarnessId::intern(name)) && which(command[0]).is_some()
            })
            .filter_map(|(name, display, command)| {
                let command: Vec<String> = command.iter().map(|s| s.to_string()).collect();
                AcpHarness::new(name, Some(display), &command).ok()
            })
            .map(|h| AcpHarness { preset: true, ..h })
            .collect()
    }
}

impl Harness for AcpHarness {
    fn descriptor(&self) -> &'static HarnessDescriptor {
        self.descriptor
    }

    fn sandbox_paths(&self) -> SandboxPaths {
        let state: &[&str] = if self.preset {
            preset_state(self.descriptor.id.as_str())
        } else {
            &[]
        };
        SandboxPaths {
            writable: state.iter().map(PathBuf::from).collect(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming_input: true,
            text_deltas: true,
            thinking: true,
            tool_events: true,
            interactive_permissions: true,
            // Enforced by unharness when the agent asks; an agent that never
            // asks cannot be made to, so `ask` is only for one known to
            // (codex-acp 2.1.1 asked for nothing in three recordings). One
            // with session modes of its own also needs a mode map, or its
            // session refuses every policy below bypass (`modes::mode_step`).
            permission_policies: self
                .asks
                .then_some(PolicySupport::full(PermissionPolicy::Ask))
                .into_iter()
                .chain([
                    PolicySupport::full(PermissionPolicy::AcceptEdits),
                    PolicySupport::full(PermissionPolicy::Bypass),
                ])
                .collect(),
            // Effort levels, image input and resume are reported by the session.
            effort_levels: Vec::new(),
            resume_by_id: true,
            live_model_list: true,
            multi_provider: false,
            provider_per_process: false,
            // Form elicitation is declared at `initialize`; whether the
            // agent asks is reported once it sends one.
            model_per_process: self.spawn_env.is_some(),
            ask_user_question: false,
            interrupt: true,
            usage_reporting: true,
            image_input: false,
            file_input: true,
            plan_updates: true,
            subagents: SubagentSupport::default(),
            steer: false,
            compaction: false,
            context_usage: true,
            rate_limits: false,
            rewind: RewindSupport::default(),
            fork: false,
            // `mcpServers` of the session request. Every agent must take
            // stdio servers; http ones and a plan mode are reported by the
            // session.
            mcp: McpSupport::via(McpChannel::Protocol, false),
            plan_mode: false,
            // The spec's commands are run by a prompt that starts with
            // `/name`; an agent that lists some says so
            // (`available_commands_update`), and codex-acp 2.1.1 listed none.
            slash_commands: false,
            // Its commands come only from a session. Unverified: whether
            // an agent keeps a session nothing was sent to.
            start_unprompted: true,
            remote_control: false,
        }
    }

    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        let binary = resolve_binary(self.descriptor, binary_override);
        let version = binary.as_deref().and_then(probe_version);
        let how = match &self.sign_in {
            Some(command) => format!(" (to sign in, run `{command}`)"),
            None => String::new(),
        };
        Probe {
            // Whether the agent is signed in is only known once a session starts.
            auth: AuthInfo {
                authenticated: false,
                details: Some(if self.preset {
                    format!(
                        "ACP preset (launch command not yet verified by a recording); sign-in is checked at session start{how}"
                    )
                } else {
                    format!("ACP agent from config; sign-in is checked at session start{how}")
                }),
            },
            binary,
            version,
        }
    }

    /// The command's first word may be a launcher (`npx`) or an agent that
    /// does not answer `--version`; unharness reaches it over ACP.
    fn reports_version(&self) -> bool {
        false
    }

    /// Models are a property of a live session (`configOptions`); the TUI
    /// receives them through `CapabilitiesChanged`.
    fn list_models(
        &self,
        _binary: &Path,
        _provider: &ProviderId,
        _sandbox: &Sandbox,
    ) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        transport::start(
            self.descriptor.id,
            self.args.clone(),
            transport::AgentSpec {
                effort_option: self.effort_option.clone(),
                modes: self.modes.clone(),
                spawn_env: self.spawn_env.clone(),
                known: self.known.clone(),
                sign_in: self.sign_in.clone(),
                relative_edits: self.relative_edits,
            },
            cfg,
        )
    }

    fn build_print_command(&self, _cfg: &PrintConfig) -> Result<std::process::Command> {
        bail!(
            "{} is driven over ACP, which has no native print or passthrough mode; use --print without --native, or the TUI",
            self.descriptor.display_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defined_agent() {
        let h = AcpHarness::new(
            "My-Agent",
            Some("My Agent"),
            &["my-agent".into(), "--acp".into()],
        )
        .unwrap();
        let d = h.descriptor();
        assert_eq!(d.id, HarnessId::intern("my-agent"));
        assert_eq!(d.display_name, "My Agent");
        assert_eq!(d.binary_names, ["my-agent"]);
        assert_eq!(h.args, ["--acp"]);
        assert!(AcpHarness::new("x", None, &[]).is_err());

        let caps = h.capabilities();
        assert!(caps.interactive_permissions && caps.plan_updates && !caps.steer);
        assert!(caps.supports_policy(PermissionPolicy::Auto).is_none());
        assert!(h.build_print_command(&PrintConfig::default()).is_err());

        // `ask` only for an agent the config says asks.
        assert!(caps.supports_policy(PermissionPolicy::Ask).is_none());
        let h = h.asking_permission(true);
        assert!(
            h.capabilities()
                .supports_policy(PermissionPolicy::Ask)
                .is_some()
        );

        // No effort option is assumed; a harness declares its own.
        assert_eq!(h.effort_option, None);
        let h = h.declaring_effort_option("thinking");
        assert_eq!(h.effort_option.as_deref(), Some("thinking"));
        assert!(h.asks, "declaring keeps the rest");

        // No mode map is assumed either.
        assert!(h.modes.is_empty());
        let h = h.with_modes(ModeMap::new([(PermissionPolicy::Ask, "ask")]));
        assert_eq!(
            h.modes.for_policy(PermissionPolicy::Bypass),
            Some((PermissionPolicy::Ask, "ask"))
        );
        assert!(h.asks && h.effort_option.is_some());

        // Nor a sign-in command: doctor names one that is declared.
        let auth = |h: &AcpHarness| h.probe(None).auth.details.unwrap();
        assert!(!auth(&h).contains("to sign in"));
        assert!(h.sign_in.is_none());
        let h = h.signing_in_with(Some(" vibe --setup "));
        assert_eq!(h.sign_in.as_deref(), Some("vibe --setup"));
        assert!(
            auth(&h).ends_with("(to sign in, run `vibe --setup`)"),
            "{}",
            auth(&h)
        );
        assert!(h.signing_in_with(Some("  ")).sign_in.is_none());
    }

    #[test]
    fn presets_do_not_shadow_configured_names() {
        let taken: Vec<HarnessId> = PRESETS
            .iter()
            .map(|(n, _, _)| HarnessId::intern(n))
            .collect();
        assert!(AcpHarness::installed_presets(&taken).is_empty());
    }
}
