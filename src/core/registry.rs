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
            Box::new(crate::harness::claude::ClaudeHarness {
                relocate_config: config
                    .harness("claude")
                    .and_then(|h| h.relocate_config)
                    .unwrap_or(true),
                mcp_config_dir: None,
            }),
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
    /// default if installed, else the best installed one in priority order:
    /// the first that is signed in, else the first that cannot say without
    /// a session or a slow check (it may be), else the first installed.
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

        let mut installed = self
            .all()
            .filter_map(|h| find_bin(h).map(|bin| (h, bin)))
            .peekable();
        let mut unknown = None;
        let mut first = None;
        while let Some((h, bin)) = installed.next() {
            // Nothing to choose between: do not spend time asking.
            if first.is_none() && installed.peek().is_none() {
                return Ok((h, bin));
            }
            match h.quick_auth(&bin) {
                Some(true) => return Ok((h, bin)),
                None if unknown.is_none() => unknown = Some((h, bin)),
                _ if first.is_none() => first = Some((h, bin)),
                _ => {}
            }
        }
        if let Some(choice) = unknown.or(first) {
            return Ok(choice);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Capabilities, ModelInfo, ProviderId, SessionConfig, SessionHandle};
    use crate::harness::{AuthInfo, HarnessDescriptor, PrintConfig, ProviderSource};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A harness that is installed when its binary override exists and
    /// answers `quick_auth` with a fixed value, counting how often it is asked.
    struct Fake {
        descriptor: &'static HarnessDescriptor,
        auth: Option<bool>,
        asked: &'static AtomicUsize,
    }

    fn fake(name: &str, auth: Option<bool>) -> (Box<dyn Harness>, &'static AtomicUsize) {
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        let asked: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
        let descriptor = Box::leak(Box::new(HarnessDescriptor {
            id: HarnessId::intern(name),
            display_name: name,
            short_name: name,
            binary_names: &["unharness-test-binary-that-is-not-on-path"],
            providers: ProviderSource::Dynamic,
        }));
        (
            Box::new(Fake {
                descriptor,
                auth,
                asked,
            }),
            asked,
        )
    }

    impl Harness for Fake {
        fn descriptor(&self) -> &'static HarnessDescriptor {
            self.descriptor
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn probe(&self, _binary_override: Option<&Path>) -> Probe {
            Probe {
                binary: None,
                version: None,
                auth: AuthInfo::default(),
            }
        }
        fn quick_auth(&self, _binary: &Path) -> Option<bool> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.auth
        }
        fn list_models(&self, _binary: &Path, _provider: &ProviderId) -> Result<Vec<ModelInfo>> {
            Ok(Vec::new())
        }
        fn start_session(&self, _cfg: SessionConfig) -> Result<SessionHandle> {
            bail!("not a real harness")
        }
        fn build_print_command(&self, _cfg: &PrintConfig) -> Result<std::process::Command> {
            bail!("not a real harness")
        }
    }

    /// A registry of fakes, each `(name, installed, quick_auth)`.
    fn registry(
        dir: &Path,
        spec: &[(&str, bool, Option<bool>)],
    ) -> (
        Registry,
        HashMap<HarnessId, PathBuf>,
        Vec<&'static AtomicUsize>,
    ) {
        let mut reg = Registry::empty();
        let mut overrides = HashMap::new();
        let mut asked = Vec::new();
        for (name, installed, auth) in spec {
            let (h, counter) = fake(name, *auth);
            if *installed {
                let bin = dir.join(name);
                std::fs::write(&bin, "").unwrap();
                overrides.insert(h.descriptor().id, bin);
            }
            reg = reg.with(h);
            asked.push(counter);
        }
        (reg, overrides, asked)
    }

    fn pick(reg: &Registry, overrides: &HashMap<HarnessId, PathBuf>) -> &'static str {
        let (h, _) = reg.resolve(None, None, overrides).unwrap();
        h.descriptor().id.as_str()
    }

    #[test]
    fn default_prefers_a_signed_in_harness() {
        let tmp = tempfile::tempdir().unwrap();
        let (reg, ov, asked) = registry(
            tmp.path(),
            &[
                ("t-out", true, Some(false)),
                ("t-missing", false, Some(true)),
                ("t-in", true, Some(true)),
                ("t-later", true, Some(true)),
            ],
        );
        assert_eq!(pick(&reg, &ov), "t-in");
        // It stops at the first signed-in one and never asks one that is
        // not installed.
        let asked: Vec<usize> = asked.iter().map(|a| a.load(Ordering::SeqCst)).collect();
        assert_eq!(asked, [1, 0, 1, 0]);
    }

    #[test]
    fn default_falls_back_to_unknown_then_to_the_first_installed() {
        let tmp = tempfile::tempdir().unwrap();
        // An agent that can only tell at session start beats one that is
        // known to be signed out.
        let (reg, ov, _) = registry(
            tmp.path(),
            &[
                ("u-out", true, Some(false)),
                ("u-unknown", true, None),
                ("u-unknown-2", true, None),
            ],
        );
        assert_eq!(pick(&reg, &ov), "u-unknown");

        // Nobody signed in or unknown: the first installed, as before.
        let (reg, ov, _) = registry(
            tmp.path(),
            &[
                ("v-missing", false, Some(true)),
                ("v-out", true, Some(false)),
                ("v-out-2", true, Some(false)),
            ],
        );
        assert_eq!(pick(&reg, &ov), "v-out");

        // A single installed harness is taken without being asked.
        let (reg, ov, asked) = registry(
            tmp.path(),
            &[("w-missing", false, None), ("w-only", true, Some(false))],
        );
        assert_eq!(pick(&reg, &ov), "w-only");
        assert_eq!(asked[1].load(Ordering::SeqCst), 0);

        let (reg, ov, _) = registry(tmp.path(), &[("x-missing", false, Some(true))]);
        assert!(reg.resolve(None, None, &ov).is_err());
    }

    #[test]
    fn explicit_and_configured_choices_are_not_second_guessed() {
        let tmp = tempfile::tempdir().unwrap();
        let (reg, ov, asked) = registry(
            tmp.path(),
            &[("y-out", true, Some(false)), ("y-in", true, Some(true))],
        );
        let (h, _) = reg.resolve(Some("y-out"), None, &ov).unwrap();
        assert_eq!(h.descriptor().id.as_str(), "y-out");
        let (h, _) = reg.resolve(None, Some("y-out"), &ov).unwrap();
        assert_eq!(h.descriptor().id.as_str(), "y-out");
        assert!(asked.iter().all(|a| a.load(Ordering::SeqCst) == 0));
        // A configured default that is not installed is skipped.
        let (reg, ov, _) = registry(
            tmp.path(),
            &[("z-missing", false, Some(true)), ("z-in", true, Some(true))],
        );
        let (h, _) = reg.resolve(None, Some("z-missing"), &ov).unwrap();
        assert_eq!(h.descriptor().id.as_str(), "z-in");
    }
}
