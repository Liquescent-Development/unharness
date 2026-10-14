//! What a harness knows about its ACP agent that the protocol does not
//! say ([`AcpProfile`]), given to [`AcpHarness::new`](super::AcpHarness::new),
//! and what the driver calls back into it for ([`AgentHooks`]).
//!
//! A config-defined agent or a preset has the default profile plus what
//! its `[harnesses.<name>]` table says (`asks_permission`, `modes`,
//! `sign_in`), and no hooks; Mistral Vibe's harness builds its own
//! (`harness/vibe/`).

use std::sync::Arc;

use super::modes::ModeMap;
use super::parse::{MetaReader, SessionOptions};
use super::spawn::SpawnPlan;
use crate::core::SessionConfig;

/// What unharness knows about an ACP agent beyond the protocol. The
/// default is an agent that is not known to ask, has no mode map, and
/// does everything the protocol's way ([`AgentHooks`]' defaults).
#[derive(Clone)]
pub struct AcpProfile {
    /// The agent is known to ask before it writes, runs a command or
    /// reaches out, which is what the `ask` policy stands on
    /// (`asks_permission`).
    pub asks: bool,
    /// The config option that is the agent's reasoning effort, whatever
    /// its category (Mistral Vibe offers its level as `thinking`), when it
    /// is not under the spec's `thought_level`. For a harness that knows
    /// what setting the option does: unharness then sends
    /// `session/set_config_option` for it on `/effort` and for a
    /// configured effort, unless the hooks choose it at spawn
    /// ([`AgentHooks::chooses_at_spawn`]), when it is read for its levels
    /// only. Nothing declares one from config.
    pub effort_option: Option<String>,
    /// Which of the agent's own session modes backs each policy (see
    /// [`ModeMap`]): from `[harnesses.<name>] modes`, or declared by a
    /// harness that knows its agent. An agent with modes and no map gets
    /// no policy below `bypass` (`modes::mode_step`).
    pub modes: ModeMap,
    /// The command that signs the agent in (`vibe --setup`), from
    /// `[harnesses.<name>] sign_in` or declared by a harness that knows
    /// its agent. It is only told to the user, with the sign-in methods
    /// the agent offers, when a session fails for want of sign-in
    /// (`auth.rs`); the generic text without one points at the agent's
    /// own command line. Never run. Blank is none.
    pub sign_in: Option<String>,
    /// Take a relative path in an edit's `diff` content as one in the
    /// session's directory, for allow rules
    /// (`AcpParser::resolving_relative_edits`). Only for an agent known to
    /// write it there and to name it in `locations` (Mistral Vibe 2.26.1);
    /// for any other a relative path stays unknown.
    pub relative_edits: bool,
    /// What the driver calls back into the harness for; [`NoHooks`] by
    /// default.
    pub hooks: Arc<dyn AgentHooks>,
}

impl Default for AcpProfile {
    fn default() -> Self {
        AcpProfile {
            asks: false,
            effort_option: None,
            modes: ModeMap::default(),
            sign_in: None,
            relative_edits: false,
            hooks: Arc::new(NoHooks),
        }
    }
}

/// What the ACP driver (`transport.rs`) and [`AcpHarness`](super::AcpHarness)
/// call back into a harness for, at each point where its agent does what
/// the protocol does not say. Every method has a default that is the
/// protocol's own way (nothing chosen at spawn, added to the environment,
/// read from `_meta` or rewound), so an agent without a quirk needs none,
/// and the driver calls them all for every agent. One per harness, shared
/// by its sessions.
pub trait AgentHooks: Send + Sync {
    /// The agent's model and effort are chosen when its process starts,
    /// in the environment [`Self::spawn_plan`] gives it, and never with
    /// `session/set_config_option`: for an agent that writes a model or
    /// effort set over the protocol into its own configuration (Mistral
    /// Vibe, whose sessions also keep the ones they last ran). A model,
    /// effort or provider sent to a session is answered `NewSession`, for
    /// the reason [`Self::keeps_choices`] gives (the TUI starts a session
    /// nothing was sent to again with it instead); the session's answer is
    /// compared with the choice, since an agent may run another model or
    /// level without an error.
    fn chooses_at_spawn(&self) -> bool {
        false
    }

    /// Why a session of an agent that chooses at spawn
    /// ([`Self::chooses_at_spawn`]) cannot take a model, effort or provider
    /// chosen after it started: said as the conversation goes on in a new
    /// session.
    fn keeps_choices(&self) -> &'static str {
        "its sessions keep the model and effort they last ran"
    }

    /// What the agent's process for `cfg` is started with: variables for
    /// its environment, and what the driver holds its sessions to
    /// ([`SpawnPlan`]). Called for every process, also one started again
    /// ([`Self::respawn_plan`]); nothing by default. Pure but for reading the
    /// agent's own configuration (never writing it) and writing what the
    /// harness keeps in unharness's own state directory (vibe's
    /// profiles).
    fn spawn_plan(&self, cfg: &SessionConfig) -> SpawnPlan {
        let _ = cfg;
        SpawnPlan::default()
    }

    /// A session answered (new, reattached or forked) in a process
    /// started with `running`, with `options`: keep what it says for the
    /// processes started after it (vibe's model names, `KnownModels`).
    fn learn(&self, running: &SpawnPlan, options: &SessionOptions, cfg: &SessionConfig) {
        let _ = (running, options, cfg);
    }

    /// After [`Self::learn`], before anything went to the session: the
    /// plan to start the process `running` again with, when what it was
    /// started with lacked what only the session could tell (vibe's level
    /// goes in a model entry that needs the model's name), or `None` to go
    /// on. The session's answer is then not passed on: the next process
    /// answers again. Asked once per session at most.
    fn respawn_plan(&self, running: &SpawnPlan, cfg: &SessionConfig) -> Option<SpawnPlan> {
        let _ = (running, cfg);
        None
    }

    /// A reader of what the agent says in a tool call's fields of its own
    /// (`_meta`), one per session ([`MetaReader`]): for a harness that
    /// knows its agent (Mistral Vibe's subagents).
    fn meta_reader(&self) -> Option<Box<dyn MetaReader>> {
        None
    }

    /// The extension method that rewinds the session, which takes
    /// `sessionId`, `messageId` (the id the agent's `user_message_chunk`
    /// gave the turn's message, reported as its `TurnAnchor`) and
    /// `restoreFiles: false`, drops that message and everything after it
    /// in place, and answers an unknown id with an error (Mistral Vibe's
    /// `_rewind/to`, 2.26.1). Not the spec: only for an agent known to
    /// have it. Files are restored by unharness from its checkpoints.
    fn rewind_method(&self) -> Option<&'static str> {
        None
    }
}

/// The hooks of an agent that does everything the protocol's way.
pub struct NoHooks;

impl AgentHooks for NoHooks {}
