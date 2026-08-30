//! Port of packages/agent/src/harness/tools/tool-context.ts (pi v0.84.3).

use crate::harness::types::ExecutionEnv;

/// Filesystem and shell context required by the built-in execution tools
/// (upstream `ExecutionToolContext`).
pub struct ExecutionToolContext<'a, E: ExecutionEnv + ?Sized> {
    pub env: &'a E,
}
