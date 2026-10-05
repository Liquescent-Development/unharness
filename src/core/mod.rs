//! Harness-agnostic core: identifiers, capabilities, the event model, and the
//! session/process plumbing every harness transport is built on.

pub mod caps;
pub mod checkpoints;
pub mod conversations;
pub mod event;
pub mod guard;
pub mod ids;
pub mod jsonrpc;
pub mod mcp;
pub mod per_turn;
pub mod process;
pub mod registry;
pub mod rules;
pub mod sandbox;
pub mod session;
pub mod testing;

pub use caps::{
    Capabilities, McpChannel, McpSupport, PermissionPolicy, PolicyResolution, PolicySupport,
    RewindSupport, SubagentSupport, resolve_policy,
};
pub use event::{
    AgentEvent, CapsUpdate, ContextUsage, PermissionDecision, PermissionKind, PermissionRequest,
    PlanEntry, PlanStatus, Question, RateLimitInfo, RateLimitWindow, StopReason, SubagentStatus,
    Usage,
};
pub use ids::{HarnessId, ModelInfo, ModelRef, ProviderId};
pub use mcp::{McpServer, McpTransport};
pub use sandbox::{Sandbox, SandboxLevel, SandboxPaths};
pub use session::{
    Attachment, ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo,
};
