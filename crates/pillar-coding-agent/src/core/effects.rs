//! The effect contract: every side effect the host performs on an
//! extension's behalf is authorized through one port, so a new path cannot
//! skip the policy (docs/ARCHITECTURE-REVIEW-s05c0.md 0).
//!
//! The port lives in the core because two layers need the same gate: the
//! session's tool-call hook (this crate) and the host's `exec` / `fs`
//! callbacks (the CLI adapter).

use std::sync::Arc;

/// A side effect the host is about to perform, in normalized form. Paths are
/// resolved before authorization, so the policy sees what will actually
/// happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectIntent {
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    Exec {
        command: String,
        args: Vec<String>,
    },
    FsWrite {
        path: String,
    },
    FsRead {
        path: String,
    },
    FsList {
        path: String,
    },
    FsStat {
        path: String,
    },
    /// Fetch a package source (npm / git) into the agent or project directory.
    PackageInstall {
        source: String,
        /// Project scope (`-l`) instead of the user scope.
        project_scope: bool,
    },
    /// Delete an installed package source.
    PackageRemove {
        source: String,
        project_scope: bool,
    },
}

/// What the policy decided for one intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectDecision {
    Allow,
    Deny { reason: String },
}

/// Authorizes one effect. A denial must stop the effect; the embedder treats
/// a missing authorizer as "allow" only when it owns the trust decision
/// itself.
pub type EffectAuthorizer = Arc<dyn Fn(&EffectIntent) -> EffectDecision + Send + Sync>;

/// Allow every effect: the extensions the host loaded are the user's own
/// (the project trust gate refuses untrusted ones before they run).
pub fn allow_all() -> EffectAuthorizer {
    Arc::new(|_intent| EffectDecision::Allow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_all_permits_every_intent() {
        let authorizer = allow_all();
        assert_eq!(
            authorizer(&EffectIntent::FsWrite {
                path: "/tmp/x".to_string()
            }),
            EffectDecision::Allow
        );
    }
}
