//! Generic harness for any agent that speaks the Agent Client Protocol
//! (<https://agentclientprotocol.com>). One instance per agent; the agent's
//! command line comes from config or from [`PRESETS`], never from the TUI.
//!
//! Verified against `@agentclientprotocol/claude-agent-acp` 0.85.1 and
//! `@agentclientprotocol/codex-acp` 2.1.1 (see `fixtures/`). What an agent
//! can do is mostly learned at the handshake, so the static capabilities are
//! the protocol's baseline and the session reports the rest.

pub mod parse;
pub mod transport;

use std::path::{Path, PathBuf};

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
        })
    }

    /// Whether the agent is known to ask before it writes, runs a command
    /// or reaches out, which is what the `ask` policy stands on.
    pub fn asking_permission(self, asks: bool) -> Self {
        AcpHarness { asks, ..self }
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
            // (codex-acp 2.1.1 asked for nothing in three recordings).
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
            // `/name`, and listed by `available_commands_update`.
            slash_commands: true,
        }
    }

    fn probe(&self, binary_override: Option<&Path>) -> Probe {
        let binary = resolve_binary(self.descriptor, binary_override);
        let version = binary.as_deref().and_then(probe_version);
        Probe {
            // Whether the agent is signed in is only known once a session starts.
            auth: AuthInfo {
                authenticated: false,
                details: Some(if self.preset {
                    "ACP preset (launch command not yet verified by a recording); sign-in is checked at session start".into()
                } else {
                    "ACP agent from config; sign-in is checked at session start".into()
                }),
            },
            binary,
            version,
        }
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
        transport::start(self.descriptor.id, self.args.clone(), cfg)
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
        let caps = h.asking_permission(true).capabilities();
        assert!(caps.supports_policy(PermissionPolicy::Ask).is_some());
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
