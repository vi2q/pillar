//! Port of packages/ai/src/auth/context.ts (pi v0.84.3) — the default auth
//! context: env vars from the process environment, file existence via the
//! filesystem (with `~` expansion against `$HOME`).

use async_trait::async_trait;

use crate::auth_types::AuthContext;

/// Default auth context: env vars from the process environment, file
/// existence via the filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultAuthContext;

#[async_trait]
impl AuthContext for DefaultAuthContext {
    async fn env(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }

    async fn file_exists(&self, path: &str) -> bool {
        let resolved = if let Some(rest) = path.strip_prefix('~') {
            match std::env::var("HOME") {
                Ok(home) => format!("{home}{rest}"),
                Err(_) => return false,
            }
        } else {
            path.to_string()
        };
        tokio::fs::metadata(&resolved).await.is_ok()
    }
}
