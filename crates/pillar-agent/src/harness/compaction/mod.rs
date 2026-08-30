//! Port of packages/agent/src/harness/compaction (pi v0.84.3).

pub mod branch_summarization;
// The upstream compaction.ts pipeline module shares its parent name; the
// module_inception lint is silenced for layout parity.
#[allow(clippy::module_inception)]
pub mod compaction;
pub mod shared;
pub mod utils;
