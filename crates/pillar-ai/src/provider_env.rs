//! Port of packages/ai/src/utils/provider-env.ts (pi v0.84.3).
//!
//! Resolves a provider env value from scoped overrides, then the process
//! environment.
//!
//! divergence: upstream's Bun-sandbox fallback re-reads `/proc/self/environ`
//! when a Bun-compiled binary exposes an empty `process.env`. That bug is
//! specific to Bun; the Rust port reads `std::env::var` directly.

use crate::types::ProviderEnv;

/// Resolve a provider env value from scoped overrides, then the process
/// environment. An empty override or process value counts as unset
/// (upstream `||` semantics).
pub fn get_provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    let scoped = env
        .and_then(|env| env.get(name))
        .filter(|value| !value.is_empty());
    match scoped {
        Some(value) => Some(value.clone()),
        None => std::env::var(name).ok().filter(|value| !value.is_empty()),
    }
}
