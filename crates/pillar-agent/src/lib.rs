//! pillar-agent: agent runtime with tool calling and state management.
//! Port of pi `packages/agent` (pi v0.84.3, commit `56700d4`).
//!
//! divergence: `CustomAgentMessages` declaration merging has no Rust
//! equivalent; the base `Message` union is fixed and apps wrap it. The
//! typebox schema validation becomes JSON-Schema validation over
//! `serde_json::Value`.

#![forbid(unsafe_code)]

pub mod abort;
pub mod agent;
pub mod agent_loop;
/// Experimental tool adapter (references, budgets, receipts). See
/// `docs/TOOL-EFFICIENCY-DESIGN.md` and the module documentation. A profile
/// opts in with the `exp` feature; the LMPC minimum leaves it off.
#[cfg(feature = "exp")]
pub mod exp;
pub mod harness;
#[cfg(feature = "proxy")]
pub mod proxy;
/// Rust-specific tooling core (saved Cargo metadata, normalized diagnostics,
/// verify planning). See `docs/RUST-TOOLING-DESIGN.md`. A profile opts in with
/// the `rust-tools` feature; the LMPC minimum leaves it off so no Cargo, LSP
/// or OS-process capability enters the runtime.
#[cfg(feature = "rust-tools")]
pub mod rust_tools;
#[cfg(feature = "search")]
pub mod search;
pub mod spawn;
pub mod stream_fn;
pub mod tool_schema;
pub mod types;

pub use abort::AbortSignal;
pub use agent::{
    Agent, AgentBusyError, AgentOptions, AgentState, ContinueError, ListenerFuture, ResetError,
};
pub use agent_loop::{AgentEventSink, AgentStream, agent_loop, agent_loop_continue};
pub use harness::messages::{
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX, bash_execution_to_text, convert_to_llm as harness_convert_to_llm,
    create_branch_summary_message, create_compaction_summary_message, create_custom_message,
};
#[cfg(feature = "search")]
pub use search::{
    ScanningReadableSource, ScanningSessionSearch, ScanningSessionSearchHit,
    ScanningSessionSearchOptions, SearchError, SessionSearch, SessionSearchHit,
    SessionSearchOptions,
};
pub use spawn::{SpawnFn, spawn_background};
pub use stream_fn::{get_default_stream_fn, set_default_stream_fn};
pub use types::thinking::AgentThinkingLevel;
pub use types::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, AgentEvent, AgentLoopConfig,
    AgentLoopTurnUpdate, AgentMessage, AgentTool, AgentToolCall, AgentToolResult,
    AgentToolUpdateCallback, BashExecutionMessage, BeforeToolCallContext, BeforeToolCallResult,
    BranchSummaryMessage, CompactionSummaryMessage, CustomMessage, FauxModelRef, PrepareNextFuture,
    QueueMode, ShouldStopAfterTurnContext, StopFuture, StreamCallOptions, StreamFn,
    ToolExecuteError, ToolExecutionMode,
};
