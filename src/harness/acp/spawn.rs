//! Agents whose model and reasoning effort are chosen when their process
//! starts, by its environment, rather than with `session/set_config_option`.
//!
//! Mistral Vibe 2.26.1 writes a model or thinking level set over ACP into
//! the user's `~/.vibe/config.toml`, where the next plain vibe run starts
//! from it; set in its environment they apply to that process alone. A
//! harness that knows this about its agent declares a [`SpawnEnv`]
//! (`AcpProfile::spawn_env`): the transport then never sends the
//! model or effort option, puts what the builder returns in the agent's
//! environment, and a change of either starts a new process, and a new
//! session once the old one was prompted (`Capabilities::model_per_process`):
//! vibe pins a session's model and level at every turn, and the pins
//! outrank its environment when the session is loaded.
//!
//! What the environment has to say can depend on what only a session
//! tells (vibe's thinking level goes in a whole model entry, which needs
//! the model's name): [`KnownModels`] keeps that across the agent's
//! processes in a run, and a process started before it was known is
//! started again once its session has said it (`transport.rs`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::modes::OwnModes;
use super::parse::SessionOptions;
use crate::core::{CapsUpdate, PermissionPolicy, ProviderId};

/// What the agent's sessions have said about its models, kept for the
/// processes started after them in this run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownModels {
    /// Each model's value (vibe's alias) → its description (vibe's model
    /// name), as the agent itself had it.
    pub descriptions: HashMap<String, String>,
    /// The model a session ran when none was chosen.
    pub default: Option<String>,
}

/// [`KnownModels`] shared by a harness and the sessions it starts.
pub type SharedModels = Arc<Mutex<KnownModels>>;

impl KnownModels {
    /// Take what a session said in its `configOptions`. `overridden` is
    /// the model whose entry the process's environment replaced: what the
    /// session says of it is the environment's, not the agent's own, and
    /// is not kept. `chosen` is whether a model was chosen for the
    /// process; only a session that was not told one shows the default.
    pub fn learn(&mut self, options: &SessionOptions, overridden: Option<&str>, chosen: bool) {
        for (value, _, description) in &options.models {
            if Some(value.as_str()) == overridden {
                continue;
            }
            if let Some(d) = description.as_deref().filter(|d| !d.is_empty()) {
                self.descriptions.insert(value.clone(), d.to_string());
            }
        }
        if !chosen && let Some(current) = &options.current_model {
            self.default = Some(current.clone());
        }
    }
}

/// What a process is to run, for the builder.
#[derive(Debug, Clone, Copy)]
pub struct SpawnChoice<'a> {
    /// The chosen model's value, as the session's model option lists it.
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    /// The session's policy as the process starts: an agent may need to be
    /// told at spawn to offer the mode that backs it (vibe's smart-approve).
    pub policy: PermissionPolicy,
    pub known: &'a KnownModels,
    /// What the session adds to unharness's own environment.
    pub env: &'a [(String, String)],
    /// The provider the user chose (`SessionConfig::provider`).
    pub provider: Option<&'a str>,
    /// The session's directory (`SessionConfig::cwd`), whose configuration
    /// the agent may read.
    pub cwd: &'a Path,
}

/// What the builder made of a [`SpawnChoice`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnPlan {
    /// Variables to set on the agent's process.
    pub env: Vec<(String, String)>,
    /// The model whose entry `env` replaces (see [`KnownModels::learn`]).
    pub overrides: Option<String>,
    /// Why the effort is not in `env` although one was chosen. The
    /// transport starts the process again once a session has said what
    /// was missing, if that is enough; otherwise it tells the user.
    pub unmet: Option<String>,
    /// The process runs on a provider the harness defined for it in its
    /// environment: the agent lists its own models beside that provider's,
    /// and only these are reported, as that provider's.
    pub provider: Option<ProviderModels>,
    /// Modes the builder put in place for the process (vibe's own
    /// profiles), each held only when the session describes it so.
    pub own_modes: OwnModes,
}

/// A provider handed to the agent in its environment, and its models.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderModels {
    pub provider: String,
    /// The values the session's model option lists them under.
    pub models: Vec<String>,
}

impl SpawnPlan {
    /// What a session reports, for a process on a provider of the
    /// harness's ([`SpawnPlan::provider`]): the provider, and of the models
    /// only that provider's.
    pub fn restrict(&self, update: &mut CapsUpdate) {
        let Some(p) = &self.provider else {
            return;
        };
        let id = ProviderId::new(p.provider.clone());
        update.provider = Some(id.clone());
        if let Some(models) = &mut update.models {
            models.retain(|m| p.models.contains(&m.model_ref.model));
            for m in models.iter_mut() {
                m.model_ref.provider = id.clone();
            }
        }
    }
}

/// Builds the agent's environment for a model and effort. Pure but for
/// reading the agent's own configuration (never writing it) and writing
/// what the harness keeps in unharness's own state directory (vibe's
/// profiles).
pub type SpawnEnv = Arc<dyn Fn(&SpawnChoice<'_>) -> SpawnPlan + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    fn options(current: &str, models: &[(&str, Option<&str>)]) -> SessionOptions {
        SessionOptions {
            model_option: Some("model".into()),
            current_model: Some(current.into()),
            models: models
                .iter()
                .map(|(v, d)| (v.to_string(), v.to_string(), d.map(str::to_string)))
                .collect(),
            ..Default::default()
        }
    }

    /// A session on a provider in the agent's environment lists the
    /// agent's own models too (vibe 2.26.1, `fixtures/vibe_acp_provider.jsonl`):
    /// only the provider's are reported, as its.
    #[test]
    fn a_provider_plan_reports_its_models_only() {
        use crate::core::{HarnessId, ModelInfo, ModelRef};
        let model = |v: &str| ModelInfo {
            model_ref: ModelRef::new(HarnessId::VIBE, "agent", v),
            display_name: v.to_string(),
            description: None,
            effort_levels: None,
        };
        let update = CapsUpdate {
            models: Some(vec![model("mistral-medium-3.5"), model("qwen")]),
            ..Default::default()
        };
        let mut same = update.clone();
        SpawnPlan::default().restrict(&mut same);
        assert_eq!(same, update, "a plan without a provider leaves it be");

        let plan = SpawnPlan {
            provider: Some(ProviderModels {
                provider: "lan".into(),
                models: vec!["qwen".into()],
            }),
            ..Default::default()
        };
        let mut update = update;
        plan.restrict(&mut update);
        assert_eq!(update.provider, Some(ProviderId::from("lan")));
        let models = update.models.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_ref.model, "qwen");
        assert_eq!(models[0].model_ref.provider, ProviderId::from("lan"));

        // An update without models still names the provider.
        let mut bare = CapsUpdate::default();
        plan.restrict(&mut bare);
        assert_eq!(bare.provider, Some(ProviderId::from("lan")));
        assert!(bare.models.is_none());
    }

    #[test]
    fn a_session_teaches_names_and_the_default() {
        let mut known = KnownModels::default();
        let opts = options(
            "glm-5-3",
            &[
                ("mistral-medium-3.5", Some("mistral-vibe-cli-latest")),
                ("glm-5-3", Some("zai-glm-5-3")),
                ("bare", None),
            ],
        );
        known.learn(&opts, None, false);
        assert_eq!(known.default.as_deref(), Some("glm-5-3"));
        assert_eq!(known.descriptions["glm-5-3"], "zai-glm-5-3");
        assert!(!known.descriptions.contains_key("bare"));

        // A process told its model does not say which is the default, and
        // the entry its environment replaced is not the agent's own.
        let opts = options(
            "mistral-medium-3.5",
            &[
                ("mistral-medium-3.5", Some("ours")),
                ("glm-5-3", Some("zai-glm-5-3b")),
            ],
        );
        known.learn(&opts, Some("mistral-medium-3.5"), true);
        assert_eq!(known.default.as_deref(), Some("glm-5-3"));
        assert_eq!(
            known.descriptions["mistral-medium-3.5"],
            "mistral-vibe-cli-latest"
        );
        assert_eq!(known.descriptions["glm-5-3"], "zai-glm-5-3b");
    }
}
