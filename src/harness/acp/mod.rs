//! Generic harness for any agent that speaks the Agent Client Protocol
//! (<https://agentclientprotocol.com>). One instance per agent; the agent's
//! command line comes from config or from [`PRESETS`], never from the TUI.
//!
//! Verified against `@agentclientprotocol/claude-agent-acp` 0.85.1 and
//! `@agentclientprotocol/codex-acp` 2.1.1 (see `fixtures/`). What an agent
//! can do is mostly learned at the handshake, so the static capabilities are
//! the protocol's baseline and the session reports the rest.
//!
//! Status notes: `NOTES.md` beside this file.

pub mod auth;
pub mod elicitation;
pub mod modes;
pub mod parse;
pub mod profile;
pub mod spawn;
pub mod transport;

use std::path::{Path, PathBuf};

pub use modes::{ModeMap, OwnModes};
pub use profile::{AcpProfile, AgentHooks};
pub use spawn::{KnownModels, ProviderModels, SharedModels, SpawnChoice, SpawnPlan};

use anyhow::{Result, bail};

use super::{
    AuthInfo, Harness, HarnessDescriptor, ModelInfo, PrintConfig, Probe, ProviderSource,
    probe_version, resolve_binary, which,
};
use crate::core::sandbox::{Sandbox, SandboxPaths};
use crate::core::{
    Capabilities, Choices, HarnessId, McpChannel, McpSupport, PermissionPolicy, PolicySupport,
    ProviderId, RewindSupport, SendNow, SessionConfig, SessionHandle, SubagentSupport,
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
    /// What unharness knows about the agent that the protocol does not
    /// say, and its hooks.
    profile: AcpProfile,
}

impl AcpHarness {
    /// `command` is the full agent command line; its first word is the
    /// binary. `profile` is what is known about the agent beyond the
    /// protocol: [`AcpProfile::default`] plus what its table says for an
    /// agent from config or a preset.
    pub fn new(
        name: &str,
        display_name: Option<&str>,
        command: &[String],
        profile: AcpProfile,
    ) -> Result<Self> {
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
        let sign_in = profile
            .sign_in
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        Ok(AcpHarness {
            descriptor,
            args: args.to_vec(),
            preset: false,
            profile: AcpProfile { sign_in, ..profile },
        })
    }

    /// The command that signs the agent in, if one is known.
    pub fn sign_in(&self) -> Option<&str> {
        self.profile.sign_in.as_deref()
    }

    /// A session of the agent, its commands held to `caps`, what the
    /// harness declares (`core::driver` answers one they do not offer): a
    /// harness built around this one (Mistral Vibe) gives its own.
    pub fn start_session_with(
        &self,
        cfg: SessionConfig,
        caps: &Capabilities,
    ) -> Result<SessionHandle> {
        transport::start(
            self.descriptor.id,
            self.args.clone(),
            self.profile.clone(),
            cfg,
            caps,
        )
    }

    /// The presets whose name is not taken, each with whether its binary
    /// is on PATH.
    pub fn presets(taken: &[HarnessId]) -> Vec<(&'static str, bool)> {
        PRESETS
            .iter()
            .filter(|(name, _, _)| !taken.contains(&HarnessId::intern(name)))
            .map(|(name, _, command)| (*name, which(command[0]).is_some()))
            .collect()
    }
}

/// What an ACP agent's table takes besides the keys every harness takes.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Own {
    /// This agent asks permission before it writes, runs a command or
    /// reaches out, so the `ask` policy can be offered for it. unharness
    /// cannot check that; set it for an agent seen to do so.
    asks_permission: Option<bool>,
    /// Which of the agent's own session modes backs each policy, e.g.
    /// `{ ask = "ask", bypass = "auto-approve" }`. unharness sets that mode
    /// before the first prompt and on every policy change, and holds the
    /// session when it does not take. A policy without an entry takes the
    /// nearest less permissive one's. An agent with modes has no `ask`
    /// without one (`modes.rs`).
    #[serde(default)]
    modes: std::collections::BTreeMap<String, String>,
    /// The command that signs the agent in (`vibe --setup`), told to the
    /// user when a session fails for want of sign-in. unharness never runs
    /// it.
    sign_in: Option<String>,
}

/// What an unknown key's error adds where a table does not define an ACP
/// agent: how to define one.
pub const DEFINING_AN_AGENT: &str = "a table with protocol = \"acp\" and a command defines an \
                                     agent that takes asks_permission, modes and sign_in";

/// An ACP agent from its `[harnesses.<name>]` table: one the table defines
/// (`protocol = "acp"` and its `command`), or else the preset of that
/// name, whose table takes only the keys every harness takes: the preset
/// runs its own command under its own name, and `binary` is where another
/// of its binaries is set. A missing command and a mode map that does not
/// parse are both reported ([`crate::config::TableErrors`]).
pub fn build(name: &str, table: &crate::config::HarnessSettings) -> Result<Box<dyn Harness>> {
    let defined = table.protocol.as_deref() == Some("acp");
    if let Some((name, display, command)) = PRESETS.iter().find(|(n, ..)| *n == name && !defined) {
        table
            .own::<crate::config::NoOwnKeys>(name)
            .map_err(|e| anyhow::anyhow!("{e:#} ({DEFINING_AN_AGENT})"))?;
        let command: Vec<String> = command.iter().map(|s| s.to_string()).collect();
        let preset = AcpHarness::new(name, Some(display), &command, AcpProfile::default())?;
        return Ok(Box::new(AcpHarness {
            preset: true,
            ..preset
        }));
    }
    let own: Own = table.own_of_agent(name)?;
    let command = table.command.as_deref().unwrap_or_default();
    let modes = match (ModeMap::from_config(&own.modes), command.is_empty()) {
        (Ok(modes), false) => modes,
        (modes, no_command) => {
            let no_command =
                no_command.then(|| format!("[harnesses.{name}] protocol = \"acp\" but no command"));
            let modes = modes.err().map(|e| format!("[harnesses.{name}]: {e}"));
            let wrong = no_command.into_iter().chain(modes).collect();
            return Err(crate::config::TableErrors(wrong).into());
        }
    };
    let profile = AcpProfile {
        asks: own.asks_permission.unwrap_or(false),
        modes,
        sign_in: own.sign_in,
        ..AcpProfile::default()
    };
    Ok(Box::new(AcpHarness::new(
        name,
        table.display_name.as_deref(),
        command,
        profile,
    )?))
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
            ..Default::default()
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
            // `auto` is the agent's own judgement, so only for one whose map
            // names a mode for it (vibe's smart-approve); a session that does
            // not offer that mode refuses it.
            permission_policies: self
                .profile
                .asks
                .then_some(PolicySupport::full(PermissionPolicy::Ask))
                .into_iter()
                .chain([PolicySupport::full(PermissionPolicy::AcceptEdits)])
                .chain(
                    self.profile
                        .modes
                        .names(PermissionPolicy::Auto)
                        .then_some(PolicySupport::full(PermissionPolicy::Auto)),
                )
                .chain([PolicySupport::full(PermissionPolicy::Bypass)])
                .collect(),
            // Effort levels, image input and resume are reported by the session.
            effort_levels: Vec::new(),
            resume_by_id: true,
            live_model_list: true,
            multi_provider: false,
            // Chosen at spawn, and kept by a session as it last ran: its
            // driver answers a session told one with `NewSession`.
            between_turns: if self.profile.hooks.chooses_at_spawn() {
                Choices::ALL
            } else {
                Choices::default()
            },
            stored_session_keeps: self
                .profile
                .hooks
                .chooses_at_spawn()
                .then(|| self.profile.hooks.keeps_choices()),
            // Form elicitation is declared at `initialize`; whether the
            // agent asks is reported once it sends one.
            ask_user_question: false,
            interrupt: true,
            usage_reporting: true,
            image_input: false,
            file_input: true,
            plan_updates: true,
            subagents: SubagentSupport::default(),
            steer: false,
            // The next `session/prompt` goes once the cancelled one has
            // answered (`stopReason: cancelled`): one sent before is
            // refused while a turn runs (vibe-acp 2.26.1).
            send_now: SendNow::AfterTurn,
            take_back_sent_now: false,
            compaction: false,
            // `usage_update` (`used`, `size`) is the agent's to send, and
            // every one recorded does; one that does not is shown as not
            // known yet, never as one that cannot report.
            context_usage: true,
            rate_limits: false,
            // Not in the spec; an agent known to have one declares it.
            rewind: RewindSupport {
                conversation: self.profile.hooks.rewind_method().is_some(),
                // A forked session's inherited messages have ids of its
                // own (vibe 2.26.1 numbers them by position).
                anchors_survive_fork: false,
            },
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
        let how = match self.sign_in() {
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
        self.start_session_with(cfg, &self.capabilities())
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

    /// An agent `x` with `profile`.
    fn with(profile: AcpProfile) -> AcpHarness {
        AcpHarness::new("x", None, &["x".into()], profile).unwrap()
    }

    #[test]
    fn config_defined_agent() {
        let h = AcpHarness::new(
            "My-Agent",
            Some("My Agent"),
            &["my-agent".into(), "--acp".into()],
            AcpProfile::default(),
        )
        .unwrap();
        let d = h.descriptor();
        assert_eq!(d.id, HarnessId::intern("my-agent"));
        assert_eq!(d.display_name, "My Agent");
        assert_eq!(d.binary_names, ["my-agent"]);
        assert_eq!(h.args, ["--acp"]);
        assert!(AcpHarness::new("x", None, &[], AcpProfile::default()).is_err());

        let caps = h.capabilities();
        assert!(caps.interactive_permissions && caps.plan_updates && !caps.steer);
        // Rewind is not in the spec: only an agent known to have it does.
        assert!(!caps.rewind.conversation);
        struct Rewinds;
        impl AgentHooks for Rewinds {
            fn rewind_method(&self) -> Option<&'static str> {
                Some("_rewind/to")
            }
        }
        let rewinds = with(AcpProfile {
            hooks: std::sync::Arc::new(Rewinds),
            ..AcpProfile::default()
        });
        assert!(rewinds.capabilities().rewind.conversation);
        assert!(!rewinds.capabilities().rewind.anchors_survive_fork);
        assert!(caps.supports_policy(PermissionPolicy::Auto).is_none());
        assert!(h.build_print_command(&PrintConfig::default()).is_err());

        // `ask` only for an agent the config says asks.
        assert!(caps.supports_policy(PermissionPolicy::Ask).is_none());
        let asking = AcpProfile {
            asks: true,
            ..AcpProfile::default()
        };
        let h = with(asking.clone());
        assert!(
            h.capabilities()
                .supports_policy(PermissionPolicy::Ask)
                .is_some()
        );

        // No effort option is assumed; a harness declares its own.
        assert_eq!(h.profile.effort_option, None);
        let h = with(AcpProfile {
            effort_option: Some("thinking".into()),
            ..asking.clone()
        });
        assert_eq!(h.profile.effort_option.as_deref(), Some("thinking"));
        assert!(h.profile.asks, "declaring keeps the rest");

        // No mode map is assumed either.
        assert!(h.profile.modes.is_empty());
        let h = with(AcpProfile {
            modes: ModeMap::new([(PermissionPolicy::Ask, "ask")]),
            ..h.profile.clone()
        });
        assert_eq!(
            h.profile.modes.for_policy(PermissionPolicy::Bypass),
            Some((PermissionPolicy::Ask, "ask"))
        );
        assert!(h.profile.asks && h.profile.effort_option.is_some());
        // A map that names a mode for auto itself declares auto; one that
        // leaves it to ask's entry does not.
        assert!(
            h.capabilities()
                .supports_policy(PermissionPolicy::Auto)
                .is_none()
        );
        let with_auto = with(AcpProfile {
            modes: ModeMap::new([
                (PermissionPolicy::Ask, "ask"),
                (PermissionPolicy::Auto, "smart-approve"),
            ]),
            ..AcpProfile::default()
        });
        assert!(
            with_auto
                .capabilities()
                .supports_policy(PermissionPolicy::Auto)
                .is_some()
        );

        // Nor a sign-in command: doctor names one that is declared.
        let auth = |h: &AcpHarness| h.probe(None).auth.details.unwrap();
        assert!(!auth(&h).contains("to sign in"));
        assert!(h.sign_in().is_none());
        let signing_in = |command: &str| {
            with(AcpProfile {
                sign_in: Some(command.into()),
                ..h.profile.clone()
            })
        };
        let h = signing_in(" vibe --setup ");
        assert_eq!(h.sign_in(), Some("vibe --setup"));
        assert!(
            auth(&h).ends_with("(to sign in, run `vibe --setup`)"),
            "{}",
            auth(&h)
        );
        assert!(signing_in("  ").sign_in().is_none());
    }

    #[test]
    fn presets_do_not_shadow_configured_names() {
        let taken: Vec<HarnessId> = PRESETS
            .iter()
            .map(|(n, _, _)| HarnessId::intern(n))
            .collect();
        assert!(AcpHarness::presets(&taken).is_empty());
    }
}
