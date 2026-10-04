//! Harness-agnostic core: identifiers, capabilities, the event model, and the
//! session/process plumbing every harness transport is built on.

pub mod caps;
pub mod checkpoints;
pub mod conversations;
pub mod event;
pub mod ids;
pub mod jsonrpc;
pub mod per_turn;
pub mod process;
pub mod registry;
pub mod session;
pub mod testing;

pub use caps::{
    Capabilities, PermissionPolicy, PolicyResolution, PolicySupport, RewindSupport, resolve_policy,
};
pub use event::{
    AgentEvent, CapsUpdate, ContextUsage, PermissionDecision, PermissionKind, PermissionRequest,
    PlanEntry, PlanStatus, Question, RateLimitInfo, RateLimitWindow, StopReason, Usage,
};
pub use ids::{HarnessId, ModelInfo, ModelRef, ProviderId};
pub use session::{
    Attachment, ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo,
};
