//! Harness adapters: one module per vendor CLI implementing [`Harness`].

pub mod acp;
pub mod agy;
pub mod claude;
pub mod codex;
pub mod pi;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::core::guard::Guarded;
use crate::core::sandbox::{Sandbox, SandboxLevel, SandboxPaths};

pub use crate::core::ModelInfo;
use crate::core::{
    Capabilities, HarnessId, McpServer, ModelRef, PermissionPolicy, PolicySupport, ProviderId,
    SessionConfig, SessionHandle,
};

/// Where a harness's provider list comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSource {
    /// Fixed list of `(id, display name)`.
    Static(&'static [(&'static str, &'static str)]),
    /// Must be queried from the binary (`list_providers`).
    Dynamic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub display_name: &'static str,
    /// Short label for the TUI (sender name, status lines).
    pub short_name: &'static str,
    /// Executable names to look for on PATH, in order.
    pub binary_names: &'static [&'static str],
    pub providers: ProviderSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthInfo {
    pub authenticated: bool,
    pub details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub binary: Option<PathBuf>,
    pub version: Option<String>,
    pub auth: AuthInfo,
}

/// Options for the vendor's own interfaces (`-p --native` and `--no-tui`).
#[derive(Debug, Clone, Default)]
pub struct PrintConfig {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub prompt: Option<String>,
    /// `true` = one-shot print mode; `false` = hand the terminal to the vendor TUI.
    pub print_mode: bool,
    pub model: Option<ModelRef>,
    /// As `SessionConfig::provider`.
    pub provider: Option<ProviderId>,
    pub effort: Option<String>,
    pub policy: Option<PermissionPolicy>,
    pub format: Option<String>,
    pub resume: Option<String>,
    pub extra_args: Vec<String>,
    /// MCP servers for this run, already limited to what the harness takes.
    pub mcp_servers: Vec<McpServer>,
    /// Applied by the caller to the command `build_print_command` returns.
    pub sandbox: Sandbox,
}

/// One vendor CLI. Implementations live in `src/harness/<name>/`.
///
/// Adding a harness: add a `HarnessId` variant, implement this trait in a new
/// module with recorded fixtures under `fixtures/`, and register it in
/// `core::registry::Registry::new`.
pub trait Harness: Send + Sync {
    fn descriptor(&self) -> &'static HarnessDescriptor;

    fn capabilities(&self) -> Capabilities;

    /// Locate the binary, read its version and auth state. Cheap and synchronous.
    fn probe(&self, binary_override: Option<&Path>) -> Probe;

    /// Whether the harness is signed in, for choosing a default at startup.
    /// Must stay cheap (a file read or a quick status command, no model
    /// query): `None` when only a slower check or a live session can tell.
    fn quick_auth(&self, _binary: &Path) -> Option<bool> {
        None
    }

    /// Providers this harness can route to. Default: the static descriptor list.
    fn list_providers(&self, _binary: &Path) -> Result<Vec<(ProviderId, String)>> {
        match self.descriptor().providers {
            ProviderSource::Static(list) => Ok(list
                .iter()
                .map(|(id, name)| (ProviderId::from(*id), name.to_string()))
                .collect()),
            ProviderSource::Dynamic => Ok(Vec::new()),
        }
    }

    /// The provider this harness uses when it is told none, as far as can
    /// be told without starting it. Default: the first static provider.
    fn default_provider(&self) -> Option<ProviderId> {
        match self.descriptor().providers {
            ProviderSource::Static(list) => list.first().map(|(id, _)| ProviderId::from(*id)),
            ProviderSource::Dynamic => None,
        }
    }

    fn list_models(&self, binary: &Path, provider: &ProviderId) -> Result<Vec<ModelInfo>>;

    /// Setup this harness needs before its first process of a run. A
    /// returned line is shown to the user.
    fn prepare(&self) -> Result<Option<String>> {
        Ok(None)
    }

    /// The directories this CLI writes outside the workspace (sessions,
    /// credentials, logs), which the sandbox must leave writable. `~` is the
    /// home directory. This is the vendor writing its own files.
    fn sandbox_paths(&self) -> SandboxPaths {
        SandboxPaths::default()
    }

    /// The vendor files that decide how this CLI runs next time (settings,
    /// hooks, MCP servers, its binary) and that the sandbox has to leave
    /// writable. unharness tells the user when one changes during a turn.
    /// `workspace` is where the session runs, for what the CLI records about
    /// it on its own.
    fn guarded(&self, _workspace: &Path) -> Vec<Guarded> {
        Vec::new()
    }

    /// The MCP servers this CLI has in its own configuration, when one of
    /// the same name handed over for a session would be mixed with it
    /// instead of replacing it. Those are left to the CLI.
    fn own_mcp_servers(&self) -> Vec<String> {
        Vec::new()
    }

    /// The policies of a run that is not a session: `--print --native`
    /// when `print_mode`, the CLI's own interface (`--no-tui`) otherwise.
    fn print_policies(&self, _print_mode: bool) -> Vec<PolicySupport> {
        self.capabilities().permission_policies
    }

    /// The sandbox level when the user set none.
    fn default_sandbox(&self) -> SandboxLevel {
        SandboxLevel::WorkspaceWrite
    }

    /// How a prompt's text names a file for this CLI: `path` is relative to
    /// the session's directory, and the result is put into the prompt as
    /// typed text. The path alone unless the CLI was seen to read some
    /// other form itself on the input unharness sends.
    fn file_reference(&self, path: &str) -> String {
        path.to_string()
    }

    /// Spawn the driver task(s) for an interactive session.
    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle>;

    /// Build the vendor command line for `--print --native` or `--no-tui`.
    fn build_print_command(&self, cfg: &PrintConfig) -> Result<std::process::Command>;
}

/// Resolve a binary: an explicit override that exists, else the first
/// descriptor name found on PATH.
pub fn resolve_binary(desc: &HarnessDescriptor, override_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = override_path
        && p.exists()
    {
        return Some(p.to_path_buf());
    }
    desc.binary_names.iter().find_map(|name| which(name))
}

/// Run `<binary> --version` and return trimmed stdout.
pub fn probe_version(binary: &Path) -> Option<String> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Find an executable on `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if candidate
                .metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            {
                return Some(candidate);
            }
        }
        #[cfg(not(unix))]
        return Some(candidate);
    }
    None
}
