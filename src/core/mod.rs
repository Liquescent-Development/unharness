//! Harness-agnostic core: identifiers, capabilities, the event model, and the
//! session/process plumbing every harness transport is built on.

pub mod caps;
pub mod event;
pub mod ids;
pub mod jsonrpc;
pub mod per_turn;
pub mod process;
pub mod registry;
pub mod session;
pub mod sessions_store;
pub mod testing;

pub use caps::{Capabilities, PermissionPolicy, PolicyResolution, PolicySupport, resolve_policy};
pub use event::{
    AgentEvent, PermissionDecision, PermissionKind, PermissionRequest, Question, StopReason, Usage,
};
pub use ids::{HarnessId, ModelRef, ProviderId};
pub use session::{ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo};
