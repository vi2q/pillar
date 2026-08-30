//! Port of packages/agent/src/harness/tools (pi v0.84.3) — the built-in
//! execution tools (bash, edit, read, write) plus path resolution, image
//! detection, and the cross-path file mutation queue.
//!
//! divergence: upstream `AgentHarnessTool.execute` receives a typebox
//! schema-validated input object, an `AbortSignal`, and an `onUpdate`
//! callback; the port's tools are async free functions over
//! [`ExecutionEnv`](crate::harness::types::ExecutionEnv) with a typed
//! `Result` error channel (upstream throws). Tool registration into the
//! agent loop lands with the harness operation implementations.

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod file_mutation_queue;
pub mod image;
pub mod path_utils;
pub mod read;
pub mod tool_context;
pub mod write;
