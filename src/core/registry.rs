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
        // The default config has no table to be wrong.
        Self::checked(&crate::config::Config::default()).0
    }

    /// Build the registry, honouring per-harness settings. A table that is
    /// wrong ([`config_errors`]) is an error, never the built-in or preset
    /// of its name in its place: a run, `switch` and `models` go through
    /// here.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let (registry, errors) = Self::checked(config);
        match errors.into_iter().next() {
            Some(e) => Err(anyhow::anyhow!(e.message)),
            None => Ok(registry),
        }
    }

    /// The harnesses the config defines whole, and what is wrong with it
    /// (`doctor`, which shows the rest). A harness whose table cannot be
    /// built is left out, no preset takes its name, and its errors say so.
    pub fn checked(config: &crate::config::Config) -> (Self, Vec<ConfigError>) {
        use crate::harness::acp::{AcpHarness, ModeMap as AcpModeMap};
        use crate::harness::agy::AgyHarness;
        use crate::harness::codex::{CodexHarness, CodexTransport};
        let mut errors = config_errors(config);
        let mut left_out: Vec<String> = Vec::new();
        let codex_transport = config
            .harness("codex")
            .and_then(|h| h.transport.as_deref())
            .and_then(CodexTransport::parse)
            .unwrap_or_default();
        let mut harnesses: Vec<Box<dyn Harness>> = vec![
            Box::new(AgyHarness),
            Box::new(crate::harness::claude::ClaudeHarness {
                relocate_config: config
                    .harness("claude")
                    .and_then(|h| h.relocate_config)
                    .unwrap_or(true),
                mcp_config_dir: None,
            }),
            Box::new(CodexHarness::new(codex_transport)),
            Box::new(crate::harness::pi::PiHarness::default()),
        ];
        // A `[harnesses.vibe]` table sets its command, modes and the rest
        // in place of the built-in's; one that cannot be built (its modes
        // or providers, both in `config_errors`) leaves vibe out.
        let vibe = HarnessId::VIBE.as_str();
        match config.harness(vibe) {
            None => harnesses.push(Box::new(crate::harness::vibe::VibeHarness::default())),
            Some(s) => match crate::harness::vibe::VibeHarness::from_settings(s) {
                Ok(h) => harnesses.push(Box::new(h)),
                Err(e) => {
                    if !errors.iter().any(|c| c.table.as_deref() == Some(vibe)) {
                        errors.push(ConfigError::new(
                            Some(vibe),
                            format!("[harnesses.{vibe}]: {e:#}"),
                        ));
                    }
                    left_out.push(vibe.to_string());
                }
            },
        }

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
            // A definition without a command, or with a map that does not
            // parse, is in `config_errors`, and leaves the agent out rather
            // than in a mode of its own.
            let (Ok(h), Ok(modes)) = (
                AcpHarness::new(name, settings.display_name.as_deref(), &settings.command),
                AcpModeMap::from_config(&settings.modes),
            ) else {
                left_out.push(name.clone());
                continue;
            };
            harnesses.push(Box::new(
                h.asking_permission(settings.asks_permission.unwrap_or(false))
                    .with_modes(modes)
                    .signing_in_with(settings.sign_in.as_deref()),
            ));
        }
        // A table left out keeps its name: no preset takes its place.
        let mut taken: Vec<HarnessId> = harnesses.iter().map(|h| h.descriptor().id).collect();
        taken.extend(left_out.iter().map(|name| HarnessId::intern(name)));
        for preset in AcpHarness::installed_presets(&taken) {
            harnesses.push(Box::new(preset));
        }
        for e in &mut errors {
            e.left_out = e.table.as_ref().is_some_and(|t| left_out.contains(t));
        }
        (Registry { harnesses }, errors)
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

/// Something wrong with the harness settings in the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The `[harnesses.<name>]` table it is in, if any.
    pub table: Option<String>,
    pub message: String,
    /// The table's harness could not be built and is not registered.
    pub left_out: bool,
}

impl ConfigError {
    fn new(table: Option<&str>, message: String) -> Self {
        ConfigError {
            table: table.map(str::to_string),
            message,
            left_out: false,
        }
    }
}

/// What is wrong with the config's harness settings, in a stable order:
/// an ACP agent without a command, an unknown policy (the TUI reads them again when the harness is
/// switched, where an error could no longer end the run), a mode map that
/// does not parse (it would leave the agent in a mode of its own
/// choosing), a provider that is not whole or not Mistral Vibe's (a
/// session on it would fail, or run somewhere else).
pub fn config_errors(config: &crate::config::Config) -> Vec<ConfigError> {
    use crate::core::PermissionPolicy;
    const POLICIES: &str = "plan, ask, accept-edits, auto, bypass";
    let mut errors = Vec::new();
    if let Some(p) = config.default_policy.as_deref()
        && PermissionPolicy::parse(p).is_none()
    {
        errors.push(ConfigError::new(
            None,
            format!("unknown policy '{p}' in config ({POLICIES})"),
        ));
    }
    let mut tables: Vec<(&String, &crate::config::HarnessSettings)> =
        config.harnesses.iter().collect();
    tables.sort_by_key(|(name, _)| name.as_str());
    for (name, h) in tables {
        let table = Some(name.as_str());
        if let Some(p) = h.default_policy.as_deref()
            && PermissionPolicy::parse(p).is_none()
        {
            errors.push(ConfigError::new(
                table,
                format!("[harnesses.{name}] default_policy: unknown policy '{p}' ({POLICIES})"),
            ));
        }
        if h.protocol.as_deref() == Some("acp")
            && HarnessId::parse(name).is_none()
            && h.command.is_empty()
        {
            errors.push(ConfigError::new(
                table,
                format!("[harnesses.{name}] protocol = \"acp\" but no command"),
            ));
        }
        if let Err(e) = crate::harness::acp::ModeMap::from_config(&h.modes) {
            errors.push(ConfigError::new(table, format!("[harnesses.{name}]: {e}")));
        }
        if h.providers.is_empty() {
            continue;
        }
        if name != HarnessId::VIBE.as_str() {
            errors.push(ConfigError::new(
                table,
                format!(
                    "[harnesses.{name}] providers: only Mistral Vibe takes providers from unharness's config"
                ),
            ));
        } else if let Err(e) = crate::harness::vibe::providers(&h.providers) {
            errors.push(ConfigError::new(table, format!("{e:#}")));
        }
    }
    errors
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
    use crate::core::{Capabilities, ModelInfo, ProviderId, Sandbox, SessionConfig, SessionHandle};
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
        fn list_models(
            &self,
            _binary: &Path,
            _provider: &ProviderId,
            _sandbox: &Sandbox,
        ) -> Result<Vec<ModelInfo>> {
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

    fn parse(text: &str) -> crate::config::Config {
        toml::from_str(text).unwrap()
    }

    /// Policies a config names must be ones, in every table and in a mode
    /// map.
    #[test]
    fn configured_policies_are_checked() {
        assert!(config_errors(&parse("default_policy = \"ask\"\n")).is_empty());
        let e = config_errors(&parse("default_policy = \"nah\"\n"));
        assert_eq!(e.len(), 1);
        assert!(
            e[0].table.is_none() && e[0].message.contains("'nah'"),
            "{e:?}"
        );
        let e = config_errors(&parse("[harnesses.agy]\ndefault_policy = \"nah\"\n"));
        assert_eq!(e[0].table.as_deref(), Some("agy"));
        assert!(e[0].message.contains("[harnesses.agy]"), "{e:?}");

        // A mode map names policies too.
        let good = "[harnesses.vibe]\nmodes = { ask = \"ask\" }\n";
        assert!(config_errors(&parse(good)).is_empty());
        let e = config_errors(&parse(
            "[harnesses.vibe]\nmodes = { ask = \"ask\", always = \"auto-approve\" }\n",
        ));
        assert!(
            e[0].message.contains("[harnesses.vibe]") && e[0].message.contains("'always'"),
            "{e:?}"
        );
    }

    /// Providers are vibe's only, and must be whole.
    #[test]
    fn configured_providers_are_checked() {
        let good = "[harnesses.vibe.providers.lan]\napi_base = \"http://127.0.0.1:8080/v1\"\nmodels = [\"m\"]\n";
        assert!(config_errors(&parse(good)).is_empty());
        assert!(config_errors(&crate::config::Config::default()).is_empty());
        let e = config_errors(&parse(
            "[harnesses.vibe.providers.lan]\napi_base = \"http://h\"\nmodels = []\n",
        ));
        assert!(
            e[0].message.contains("[harnesses.vibe.providers.lan]")
                && e[0].message.contains("models"),
            "{e:?}"
        );
        let e = config_errors(&parse(&good.replace("vibe", "codex")));
        assert!(e[0].message.contains("[harnesses.codex]"), "{e:?}");
    }

    /// A table that cannot be built is an error, and is never the built-in
    /// in its place: `from_config` fails, `checked` leaves the harness out
    /// and says so (#217).
    #[test]
    fn a_wrong_table_is_no_harness() {
        let bad_vibe = "[harnesses.vibe]\ncommand = [\"/opt/my-vibe-acp\"]\n\
                        [harnesses.vibe.providers.lan]\napi_base = \"http://h\"\nmodels = []\n";
        let e = Registry::from_config(&parse(bad_vibe)).err().unwrap();
        assert!(
            e.to_string().contains("[harnesses.vibe.providers.lan]"),
            "{e}"
        );
        let (reg, errors) = Registry::checked(&parse(bad_vibe));
        assert!(reg.get(HarnessId::VIBE).is_none());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].left_out && errors[0].table.as_deref() == Some("vibe"));

        let bad_acp = "[harnesses.myagent]\nprotocol = \"acp\"\ncommand = [\"my-agent\"]\n\
                       modes = { always = \"x\" }\n";
        assert!(Registry::from_config(&parse(bad_acp)).is_err());
        let (reg, errors) = Registry::checked(&parse(bad_acp));
        assert!(reg.parse("myagent").is_none());
        assert!(errors.len() == 1 && errors[0].left_out, "{errors:?}");
        // The rest is still there.
        assert!(reg.get(HarnessId::VIBE).is_some() && reg.get(HarnessId::CLAUDE).is_some());

        // A preset's name with a wrong table, or none without a command,
        // is not taken by the preset, whatever is on PATH.
        for bad in [
            "[harnesses.gemini]\nprotocol = \"acp\"\ncommand = [\"gemini\", \"--acp\"]\n\
             modes = { always = \"x\" }\n",
            "[harnesses.gemini]\nprotocol = \"acp\"\ndisplay_name = \"My Gemini\"\n",
        ] {
            assert!(Registry::from_config(&parse(bad)).is_err(), "{bad}");
            let (reg, errors) = Registry::checked(&parse(bad));
            assert!(reg.parse("gemini").is_none(), "{bad}");
            assert!(errors.iter().all(|e| e.left_out), "{errors:?}");
        }
        let e = config_errors(&parse("[harnesses.gemini]\nprotocol = \"acp\"\n"));
        assert_eq!(
            e[0].message,
            "[harnesses.gemini] protocol = \"acp\" but no command"
        );

        // A wrong default policy is an error; the harness itself is whole.
        let (reg, errors) =
            Registry::checked(&parse("[harnesses.vibe]\ndefault_policy = \"nah\"\n"));
        assert!(reg.get(HarnessId::VIBE).is_some());
        assert!(errors.len() == 1 && !errors[0].left_out, "{errors:?}");

        let good = "[harnesses.vibe]\nmodes = { ask = \"ask\" }\n\
                    [harnesses.myagent]\nprotocol = \"acp\"\ncommand = [\"my-agent\"]\n";
        let reg = Registry::from_config(&parse(good)).unwrap();
        assert!(reg.get(HarnessId::VIBE).is_some() && reg.parse("myagent").is_some());
    }
}
