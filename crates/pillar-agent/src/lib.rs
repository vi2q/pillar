//! pillar-agent: agent runtime with tool calling and state management.
//! Port of pi `packages/agent` (pi v0.84.3, commit `56700d4`).
//!
//! divergence: `CustomAgentMessages` declaration merging has no Rust
//! equivalent; the base `Message` union is fixed and apps wrap it. The
//! typebox schema validation becomes JSON-Schema validation over
//! `serde_json::Value`.

#![forbid(unsafe_code)]

pub mod abort;
pub mod agent_loop;
pub mod types;

pub use abort::AbortSignal;
pub use agent_loop::{agent_loop, agent_loop_continue, AgentEventSink, AgentStream};
pub use types::thinking::AgentThinkingLevel;
pub use types::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, AgentEvent, AgentLoopConfig,
    AgentLoopTurnUpdate, AgentMessage, AgentTool, AgentToolCall, AgentToolResult,
    AgentToolUpdateCallback, BeforeToolCallContext, BeforeToolCallResult, FauxModelRef, QueueMode,
    ShouldStopAfterTurnContext, StreamCallOptions, StreamFn, ToolExecuteError, ToolExecutionMode,
};
