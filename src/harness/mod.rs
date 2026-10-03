pub mod agy;
pub mod claude;
pub mod codex;

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HarnessKind {
    Agy,
    Claude,
    Codex,
}

impl HarnessKind {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "agy" | "antigravity" => Some(HarnessKind::Agy),
            "claude" | "claude-code" => Some(HarnessKind::Claude),
            "codex" => Some(HarnessKind::Codex),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HarnessKind::Agy => "agy",
            HarnessKind::Claude => "claude",
            HarnessKind::Codex => "codex",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            HarnessKind::Agy => "Antigravity (agy)",
            HarnessKind::Claude => "Claude Code (claude)",
            HarnessKind::Codex => "Codex (codex)",
        }
    }

    pub fn default_priority() -> &'static [HarnessKind] {
        &[HarnessKind::Agy, HarnessKind::Claude, HarnessKind::Codex]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub prompt: Option<String>,
    pub print_mode: bool,
    pub auto_approve: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub format: Option<String>,
    pub cwd: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub authenticated: bool,
    pub details: Option<String>,
}

pub trait HarnessAdapter {
    fn kind(&self) -> HarnessKind;
    fn binary_name(&self) -> &'static str;
    fn display_name(&self) -> &'static str {
        self.kind().display_name()
    }
    fn resolve_binary(&self, override_path: Option<&Path>) -> Option<PathBuf>;
    fn version(&self, binary: &Path) -> Option<String>;
    fn auth_status(&self, binary: &Path) -> AuthInfo;
    fn available_models(&self, binary: Option<&Path>) -> Vec<ModelInfo>;
    fn build_command(&self, binary: &Path, opts: &RunOptions) -> Result<Command>;
}

pub fn get_adapter(kind: HarnessKind) -> Box<dyn HarnessAdapter> {
    match kind {
        HarnessKind::Agy => Box::new(agy::AgyAdapter),
        HarnessKind::Claude => Box::new(claude::ClaudeAdapter),
        HarnessKind::Codex => Box::new(codex::CodexAdapter),
    }
}

pub fn resolve_active_harness(
    requested: Option<&str>,
    configured_default: Option<&str>,
    override_paths: &std::collections::HashMap<HarnessKind, PathBuf>,
) -> Result<(HarnessKind, PathBuf)> {
    if let Some(req) = requested {
        if let Some(kind) = HarnessKind::parse_str(req) {
            let adapter = get_adapter(kind);
            let binary_override = override_paths.get(&kind).map(|p| p.as_path());
            if let Some(bin) = adapter.resolve_binary(binary_override) {
                return Ok((kind, bin));
            } else {
                bail!(
                    "Requested harness '{}' was not found on PATH (expected executable '{}')",
                    req,
                    adapter.binary_name()
                );
            }
        } else {
            bail!(
                "Unknown harness '{}'. Supported harnesses: agy, claude, codex",
                req
            );
        }
    }

    if let Some(def) = configured_default
        && let Some(kind) = HarnessKind::parse_str(def)
    {
        let adapter = get_adapter(kind);
        let binary_override = override_paths.get(&kind).map(|p| p.as_path());
        if let Some(bin) = adapter.resolve_binary(binary_override) {
            return Ok((kind, bin));
        }
    }

    for &kind in HarnessKind::default_priority() {
        let adapter = get_adapter(kind);
        let binary_override = override_paths.get(&kind).map(|p| p.as_path());
        if let Some(bin) = adapter.resolve_binary(binary_override) {
            return Ok((kind, bin));
        }
    }

    bail!(
        "No supported AI harness found on PATH. Please install either Antigravity (`agy`), Claude Code (`claude`), or Codex (`codex`)."
    );
}

pub fn which(name: &str) -> Option<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = candidate.metadata()
                        && meta.permissions().mode() & 0o111 != 0
                    {
                        return Some(candidate);
                    }
                }
                #[cfg(not(unix))]
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_harness_str() {
        assert_eq!(HarnessKind::parse_str("agy"), Some(HarnessKind::Agy));
        assert_eq!(
            HarnessKind::parse_str("antigravity"),
            Some(HarnessKind::Agy)
        );
        assert_eq!(HarnessKind::parse_str("claude"), Some(HarnessKind::Claude));
        assert_eq!(
            HarnessKind::parse_str("claude-code"),
            Some(HarnessKind::Claude)
        );
        assert_eq!(HarnessKind::parse_str("codex"), Some(HarnessKind::Codex));
        assert_eq!(HarnessKind::parse_str("unknown"), None);
    }

    #[test]
    fn test_priority_order() {
        assert_eq!(
            HarnessKind::default_priority(),
            &[HarnessKind::Agy, HarnessKind::Claude, HarnessKind::Codex]
        );
    }
}
