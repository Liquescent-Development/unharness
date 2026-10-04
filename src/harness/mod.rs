//! Harness adapters: one module per vendor CLI implementing [`Harness`].

pub mod acp;
pub mod agy;
pub mod claude;
pub mod codex;
pub mod pi;

use std::path::{Path, PathBuf};

use anyhow::Result;

pub use crate::core::ModelInfo;
use crate::core::{
    Capabilities, HarnessId, ModelRef, PermissionPolicy, ProviderId, SessionConfig, SessionHandle,
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

/// Options for the non-TUI paths (`-p` print mode and `--no-tui` passthrough).
#[derive(Debug, Clone, Default)]
pub struct PrintConfig {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub prompt: Option<String>,
    /// `true` = one-shot print mode; `false` = hand the terminal to the vendor TUI.
    pub print_mode: bool,
    pub model: Option<ModelRef>,
    pub effort: Option<String>,
    pub policy: Option<PermissionPolicy>,
    pub format: Option<String>,
    pub resume: Option<String>,
    pub extra_args: Vec<String>,
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

    fn list_models(&self, binary: &Path, provider: &ProviderId) -> Result<Vec<ModelInfo>>;

    /// Spawn the driver task(s) for an interactive session.
    fn start_session(&self, cfg: SessionConfig) -> Result<SessionHandle>;

    /// Build the vendor command line for print or passthrough mode.
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
