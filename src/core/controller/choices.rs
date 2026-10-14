//! The provider, model and effort each harness runs with, and where each
//! came from.

use super::{Origin, Pick};
use crate::core::{ModelRef, ProviderId};

/// What a harness runs with besides its policy.
#[derive(Debug, Clone, Default)]
pub struct ChoiceState {
    /// The provider: the user's choice (`Origin::User`) or the configured
    /// one (`Origin::Configured`), which are what the harness is told;
    /// otherwise a guess at the harness's own default, corrected by what
    /// its session reports (`Origin::Harness`).
    pub(super) provider: Option<Pick<ProviderId>>,
    /// The provider a session runs on when it is not the chosen one.
    pub(super) runs_on: Option<ProviderId>,
    /// The configured model (`Origin::Configured`), or one chosen in this
    /// run or for the conversation; only a chosen one is kept with the
    /// conversation.
    pub(super) model: Option<Pick<ModelRef>>,
    /// As `model`.
    pub(super) effort: Option<Pick<String>>,
}

impl ChoiceState {
    pub fn provider(&self) -> Option<&ProviderId> {
        self.provider.as_ref().map(|p| &p.value)
    }

    /// The provider the harness is told: one chosen or configured.
    pub fn chosen_provider(&self) -> Option<&ProviderId> {
        self.provider
            .as_ref()
            .filter(|p| p.origin != Origin::Harness)
            .map(|p| &p.value)
    }

    pub fn model(&self) -> Option<&ModelRef> {
        self.model.as_ref().map(|m| &m.value)
    }

    pub fn effort(&self) -> Option<&str> {
        self.effort.as_ref().map(|e| e.value.as_str())
    }

    /// The model, when it was chosen rather than configured: what the
    /// conversation keeps.
    pub(super) fn chosen_model(&self) -> Option<&ModelRef> {
        self.model
            .as_ref()
            .filter(|m| m.origin != Origin::Configured)
            .map(|m| &m.value)
    }

    /// As `chosen_model`.
    pub(super) fn chosen_effort(&self) -> Option<&String> {
        self.effort
            .as_ref()
            .filter(|e| e.origin != Origin::Configured)
            .map(|e| &e.value)
    }
}
