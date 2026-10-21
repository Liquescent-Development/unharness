//! Harness-agnostic core: identifiers, capabilities, the event model, and the
//! session/process plumbing every harness transport is built on.

pub mod bridge;
pub mod caps;
pub mod checkpoints;
pub mod controller;
pub mod conversations;
pub mod driver;
pub mod event;
pub mod guard;
pub mod ids;
pub mod jsonrpc;
pub mod mcp;
pub mod parser;
pub mod permissions;
pub mod process;
pub mod reaper;
pub mod registry;
pub mod rules;
pub mod sandbox;
pub mod session;
pub mod shell;
pub mod testing;
pub mod text;
pub mod transcript;

pub use caps::{
    Ability, Capabilities, Choices, McpChannel, McpSupport, PermissionPolicy, PolicyResolution,
    PolicySupport, PolicyUnavailable, RewindSupport, SendNow, SubagentSupport, resolve_policy,
};
pub use event::{
    AgentEvent, CapsUpdate, ContextUsage, HarnessCommand, HookOutcome, Overage, PermissionDecision,
    PermissionKind, PermissionRequest, PlanEntry, PlanStatus, Question, QuestionOption,
    RateLimitInfo, RateLimitWindow, RemoteControl, StopReason, SubagentStatus, Usage, UsageScope,
};
pub use ids::{HarnessId, ModelInfo, ModelRef, ProviderId};
pub use mcp::{McpServer, McpTransport};
pub use parser::Parser;
pub use rules::{Rule, Rules, ToolAction};
pub use sandbox::{Sandbox, SandboxLevel, SandboxPaths};
pub use session::{
    Attachment, ProcessModel, SessionCommand, SessionConfig, SessionHandle, SessionInfo,
    shutdown_queued,
};
