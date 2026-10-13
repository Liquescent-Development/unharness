//! What a harness knows about its ACP agent that the protocol does not
//! say ([`AcpProfile`]), given to [`AcpHarness::new`](super::AcpHarness::new).
//!
//! A config-defined agent or a preset has the default profile plus what
//! its `[harnesses.<name>]` table says (`asks_permission`, `modes`,
//! `sign_in`); Mistral Vibe's harness builds its own (`harness/vibe/`).

use super::modes::ModeMap;
use super::parse::MetaReaderFactory;
use super::spawn::SpawnEnv;

/// What unharness knows about an ACP agent beyond the protocol. The
/// default is an agent that is not known to ask, has no mode map, and
/// does everything the protocol's way.
#[derive(Clone, Default)]
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
    /// configured effort. Nothing declares one from config.
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
    /// The extension method that rewinds the session, which takes
    /// `sessionId`, `messageId` (the id the agent's `user_message_chunk`
    /// gave the turn's message, reported as its `TurnAnchor`) and
    /// `restoreFiles: false`, drops that message and everything after it
    /// in place, and answers an unknown id with an error (Mistral Vibe's
    /// `_rewind/to`, 2.26.1). Not the spec: only for an agent known to
    /// have it. Files are restored by unharness from its checkpoints.
    pub rewind_method: Option<&'static str>,
    /// Choose the agent's model and effort when its process starts, with
    /// the environment this builds, and never with
    /// `session/set_config_option`: for an agent that writes a model or
    /// effort set over the protocol into its own configuration (Mistral
    /// Vibe, whose sessions also keep the ones they last ran). A change of
    /// either starts a new process, and a new session once the old one was
    /// prompted (`Capabilities::model_per_process`). The effort option is
    /// still [`Self::effort_option`], read for its levels only.
    pub spawn_env: Option<SpawnEnv>,
    /// Read each tool call's fields of the agent's own with a reader from
    /// this, one per session (`parse::MetaReader`): for a harness that
    /// knows its agent (Mistral Vibe's subagents). Nothing declares one
    /// from config.
    pub meta_reader: Option<MetaReaderFactory>,
}
