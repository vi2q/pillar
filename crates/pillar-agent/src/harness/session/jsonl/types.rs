//! Port of packages/agent/src/harness/session/jsonl/types.ts (pi v0.84.3).

use serde::{Deserialize, Serialize};

use super::super::memory::SessionCreateOptions;
use super::super::types::SessionMetadata;
use crate::harness::types::FileSystem;

/// Filesystem capabilities the JSONL backend needs (upstream
/// `JsonlSessionRepoFileSystem` is a Pick of `FileSystem`). The port uses
/// the shared [`crate::harness::types::FileSystem`] trait directly; the
/// narrower Pick is enforced at the call sites, not in the type.
pub use crate::harness::types::FileSystem as JsonlSessionRepoFileSystem;

/// Options for constructing the JSONL repo (upstream
/// `JsonlSessionRepoOptions`).
pub struct JsonlSessionRepoOptions<F: FileSystem + ?Sized> {
    pub fs: std::sync::Arc<F>,
    /// Root containing coding-agent-compatible cwd-encoded session
    /// directories.
    pub sessions_root: String,
}

/// Metadata for a JSONL-backed session (upstream `JsonlSessionMetadata`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonlSessionMetadata {
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub cwd: String,
    pub path: String,
    /// Filesystem modification time as milliseconds since Unix epoch.
    #[serde(rename = "modifiedAt")]
    pub modified_at: u64,
    /// 3 or 4 (upstream `sourceFormat: 3 | 4`).
    pub source_format: u8,
    /// Present only when a v3 parent path could not be resolved to a
    /// session id.
    #[serde(
        rename = "legacyParentSessionPath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_parent_session_path: Option<String>,
    /// Opaque application-owned metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl From<SessionMetadata> for JsonlSessionMetadata {
    fn from(metadata: SessionMetadata) -> Self {
        Self {
            id: metadata.id,
            created_at: metadata.created_at,
            parent_session_id: metadata.parent_session_id,
            cwd: String::new(),
            path: String::new(),
            modified_at: 0,
            source_format: 4,
            legacy_parent_session_path: None,
            metadata: None,
        }
    }
}

/// Options for creating a JSONL session (upstream
/// `JsonlSessionCreateOptions`).
#[derive(Debug, Clone, Default)]
pub struct JsonlSessionCreateOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
    pub cwd: String,
    pub metadata: Option<serde_json::Value>,
}

/// Options for listing sessions (upstream `JsonlSessionListOptions`).
#[derive(Debug, Clone, Default)]
pub struct JsonlSessionListOptions {
    pub cwd: Option<String>,
}

/// The first line of a v4 session file (upstream `JsonlV4Header`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonlV4Header {
    pub kind: String,
    pub version: u32,
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    pub cwd: String,
    #[serde(
        rename = "parentSessionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session_id: Option<String>,
    /// Preserved only when a v3 parent path could not be resolved to a
    /// session id.
    #[serde(
        rename = "legacyParentSessionPath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_parent_session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Re-exported so callers can build create options from the generic base
/// (upstream extends `SessionCreateOptions`).
pub type BaseSessionCreateOptions = SessionCreateOptions;
