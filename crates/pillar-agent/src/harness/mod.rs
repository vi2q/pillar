//! Port of pi packages/agent/src/harness (pi v0.84.3, commit `56700d4`).
//!
//! The harness packages the agent loop into a session-aware runtime with
//! resources (skills, prompt templates), an execution environment, and a
//! run/event model.
//!
//! divergence: upstream `Result<T, E>` maps to `std::result::Result` in
//! Rust; `TaggedError` classes map to typed error enums. Filesystem and
//! shell capabilities are traits implemented per host (upstream: one
//! Node.js implementation).
//!
//! divergence: `CustomAgentMessages` declaration merging is represented by
//! concrete enum variants on `crate::types::AgentMessage` (see
//! `messages.rs`).

pub mod agent_harness;
pub mod compaction;
// Native-only: the std/tokio filesystem and process execution environment.
// wasm32 hosts provide the `FileSystem`/`Shell` traits instead, and an
// embedding host that brings its own tools turns the whole capability off
// (`harness-tools`; docs/DEVELOPMENT-STRATEGY.md §4).
#[cfg(all(not(target_arch = "wasm32"), feature = "harness-tools"))]
pub mod env;
pub mod events;
pub mod messages;
pub mod prompt_templates;
pub mod reducer;
pub mod result;
pub mod session;
pub mod skills;
pub mod system_prompt;
pub mod telemetry;
// The coding agent's file/shell tools: OS-bound capabilities a host provides
// itself in the embedding profiles (see the `harness-tools` feature).
#[cfg(feature = "harness-tools")]
pub mod tools;
pub mod types;
pub mod utils;
