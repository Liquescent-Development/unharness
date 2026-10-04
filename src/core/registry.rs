//! The set of known harnesses, in default resolution priority.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::ids::HarnessId;
use crate::harness::{Harness, Probe, resolve_binary};

pub struct Registry {
    harnesses: Vec<Box<dyn Harness>>,
}

impl Registry {
    /// All harnesses in default priority order.
    pub fn new() -> Self {
        Self::from_config(&crate::config::Config::default())
    }

    /// Build the registry, honouring per-harness transport settings.
    pub fn from_config(config: &crate::config::Config) -> Self {
        use crate::harness::acp::AcpHarness;
        use crate::harness::agy::{AgyHarness, AgyTransport};
        use crate::harness::codex::{CodexHarness, CodexTransport};
        let agy_transport = config
            .harness("agy")
            .and_then(|h| h.transport.as_deref())
            .and_then(AgyTransport::parse)
            .unwrap_or_default();
        let codex_transport = config
            .harness("codex")
            .and_then(|h| h.transport.as_deref())
            .and_then(CodexTransport::parse)
            .unwrap_or_default();
        let mut harnesses: Vec<Box<dyn Harness>> = vec![
            Box::new(AgyHarness::new(agy_transport)),
            Box::new(crate::harness::claude::ClaudeHarness),
            Box::new(CodexHarness::new(codex_transport)),
            Box::new(crate::harness::pi::PiHarness),
        ];

        // ACP agents: the ones defined in config (by name, for a stable
        // order), then the presets found on PATH. Neither may take a
        // built-in harness's name.
        let mut defined: Vec<(&String, &crate::config::HarnessSettings)> = config
            .harnesses
            .iter()
            .filter(|(name, s)| {
                s.protocol.as_deref() == Some("acp") && HarnessId::parse(name).is_none()
            })
            .collect();
        defined.sort_by_key(|(name, _)| name.as_str());
        for (name, settings) in defined {
            // A definition without a command is reported by `doctor`.
            if let Ok(h) =
                AcpHarness::new(name, settings.display_name.as_deref(), &settings.command)
            {
                harnesses.push(Box::new(h));
            }
        }
        let taken: Vec<HarnessId> = harnesses.iter().map(|h| h.descriptor().id).collect();
        for preset in AcpHarness::installed_presets(&taken) {
            harnesses.push(Box::new(preset));
        }
        Registry { harnesses }
    }

    pub fn empty() -> Self {
        Registry {
            harnesses: Vec::new(),
        }
    }

    pub fn with(mut self, h: Box<dyn Harness>) -> Self {
        self.harnesses.push(h);
        self
    }

    pub fn all(&self) -> impl Iterator<Item = &dyn Harness> {
        self.harnesses.iter().map(|h| h.as_ref())
    }

    pub fn ids(&self) -> Vec<HarnessId> {
        self.harnesses.iter().map(|h| h.descriptor().id).collect()
    }

    pub fn get(&self, id: HarnessId) -> Option<&dyn Harness> {
        self.harnesses
            .iter()
            .find(|h| h.descriptor().id == id)
            .map(|h| h.as_ref())
    }

    /// A registered harness by id, or by a built-in alias (`antigravity`, `claude-code`).
    pub fn parse(&self, alias: &str) -> Option<&dyn Harness> {
        let alias = alias.trim().to_lowercase();
        self.harnesses
            .iter()
            .find(|h| h.descriptor().id.as_str() == alias)
            .map(|h| h.as_ref())
            .or_else(|| HarnessId::parse(&alias).and_then(|id| self.get(id)))
    }

    /// Pick the harness to run: an explicit request, else the configured
    /// default if installed, else the first installed in priority order.
    pub fn resolve(
        &self,
        requested: Option<&str>,
        configured_default: Option<&str>,
        overrides: &HashMap<HarnessId, PathBuf>,
    ) -> Result<(&dyn Harness, PathBuf)> {
        let find_bin = |h: &dyn Harness| -> Option<PathBuf> {
            let id = h.descriptor().id;
            resolve_binary(h.descriptor(), overrides.get(&id).map(PathBuf::as_path))
        };

        if let Some(req) = requested {
            let Some(h) = self.parse(req) else {
                bail!(
                    "Unknown harness '{}'. Supported: {}",
                    req,
                    self.ids()
                        .iter()
                        .map(|i| i.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            };
            return match find_bin(h) {
                Some(bin) => Ok((h, bin)),
                None => bail!(
                    "Requested harness '{}' was not found on PATH (expected one of: {})",
                    req,
                    h.descriptor().binary_names.join(", ")
                ),
            };
        }

        if let Some(def) = configured_default
            && let Some(h) = self.parse(def)
            && let Some(bin) = find_bin(h)
        {
            return Ok((h, bin));
        }

        for h in self.all() {
            if let Some(bin) = find_bin(h) {
                return Ok((h, bin));
            }
        }

        bail!(
            "No supported AI harness found on PATH. Install one of: {}",
            self.all()
                .flat_map(|h| h.descriptor().binary_names.iter())
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    /// Probe every harness (for `doctor` and the harness picker).
    pub fn probe_all(&self, overrides: &HashMap<HarnessId, PathBuf>) -> Vec<(&dyn Harness, Probe)> {
        self.all()
            .map(|h| {
                let id = h.descriptor().id;
                let probe = h.probe(overrides.get(&id).map(PathBuf::as_path));
                (h, probe)
            })
            .collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience for callers that only have a `Path` override.
pub fn overrides_from<'a>(
    iter: impl Iterator<Item = (HarnessId, Option<&'a Path>)>,
) -> HashMap<HarnessId, PathBuf> {
    iter.filter_map(|(id, p)| p.map(|p| (id, p.to_path_buf())))
        .collect()
}
