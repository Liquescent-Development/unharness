//! The provider, model and effort each harness runs with, where each came
//! from, and when one may be chosen.

use super::{Origin, Pick};
use crate::core::{ModelRef, ProviderId, SessionCommand};

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

/// A choice of a harness's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Model,
    Effort,
    Provider,
}

impl Choice {
    pub fn what(self) -> &'static str {
        match self {
            Choice::Model => "model",
            Choice::Effort => "effort",
            Choice::Provider => "provider",
        }
    }
}

/// A choice sent to a session that takes it only between turns, until the
/// session has said how it took it: prompts typed meanwhile are queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChoice {
    /// The prompts queued when it was sent: any after them were typed
    /// after it, and go when it has been taken.
    pub queued: usize,
    /// What was chosen: `model`, `effort`, `provider`.
    pub what: &'static str,
    /// A provider's line, which says how the session took it, waits for
    /// the answer.
    pub provider: Option<ProviderId>,
}

/// What the TUI knows of the active harness's session that decides whether
/// a choice may be made now, and how it reaches the session.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionNow {
    /// Its process is live.
    pub alive: bool,
    /// It was sent something: one started early has seen nothing of the
    /// conversation.
    pub prompted: bool,
    /// Its start waits for the CLI before it to be gone (`tui::Ending`).
    pub start_held: bool,
    /// A turn runs.
    pub turn: bool,
    /// Subagents at work whose end will be told.
    pub subagents: usize,
    /// The conversation has a stored session of the harness.
    pub stored: bool,
    /// Prompts waiting to be sent.
    pub queued: usize,
}

/// How a choice just made reaches the session.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// It does not: no session is live (the next one starts with it), the
    /// live one was sent nothing and takes a provider any time, or it is
    /// the provider it had.
    None,
    /// The stored session keeps what it ran, and no session prompted is
    /// there to say so: the conversation goes on in a new session, for
    /// this reason.
    Renew(&'static str),
    /// The live session was sent nothing yet: it is started again with
    /// the choice.
    Restart,
    /// The session takes it only between turns: it is sent, and what is
    /// typed after it waits until the session has said how it took it
    /// (`Controller::choice_answered`).
    Hold(SessionCommand),
    /// It is sent.
    Send(SessionCommand),
}
