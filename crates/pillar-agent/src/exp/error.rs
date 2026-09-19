//! The experiment's error surface (design §3, "共通エラー").

use serde::{Deserialize, Serialize};

/// Stable, model-visible error codes.
///
/// The set is closed on purpose: a caller can branch on it, and each variant
/// has a short repair path ("re-read the target", "raise the budget
/// explicitly") rather than a stack trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpErrorCode {
    /// The reference does not exist, does not belong to this owner, or a path
    /// no longer names the referenced resource. Deliberately does not say
    /// which: a foreign or unknown reference must not leak the target (§3).
    InvalidRef,
    /// The reference is past its lifetime, or belongs to a previous host
    /// generation (a restart), so it must not be treated as fresh (§7.2).
    ExpiredRef,
    /// The host refused the operation's authorization. Re-checked on every
    /// call, including duplicate-operation lookups.
    PermissionDenied,
    /// The target changed since the reference was issued, or the publication
    /// check failed against an external writer. No change was applied.
    RevisionConflict,
    /// The host cannot promise the requested guarantee (a weak filesystem);
    /// the caller may opt into the weak mode explicitly, but never by
    /// accident (design §4.3).
    UnsupportedGuarantee,
    /// A per-call cap would be exceeded. Nothing was applied.
    BudgetExceeded,
    /// The operation id was used before with different arguments.
    OperationIdMismatch,
    /// The operation's outcome cannot be determined (its reservation was
    /// dropped, or publication did not report back). Never reported as
    /// success (§7.1).
    OutcomeUnknown,
    /// The request itself is malformed (empty edits, out-of-range lines, a
    /// range that is not on a UTF-8 boundary).
    InvalidRequest,
    /// The host failed for a reason it has no code for.
    HostFailure,
}

impl ExpErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRef => "invalid_ref",
            Self::ExpiredRef => "expired_ref",
            Self::PermissionDenied => "permission_denied",
            Self::RevisionConflict => "revision_conflict",
            Self::UnsupportedGuarantee => "unsupported_guarantee",
            Self::BudgetExceeded => "budget_exceeded",
            Self::OperationIdMismatch => "operation_id_mismatch",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::InvalidRequest => "invalid_request",
            Self::HostFailure => "host_failure",
        }
    }
}

impl std::fmt::Display for ExpErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An experiment error: a stable code, a message for the model, and an
/// optional one-line repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ExpError {
    pub code: ExpErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<String>,
}

impl ExpError {
    pub fn new(code: ExpErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            repair: None,
        }
    }

    pub fn with_repair(mut self, repair: impl Into<String>) -> Self {
        self.repair = Some(repair.into());
        self
    }

    pub fn invalid_ref() -> Self {
        Self::new(
            ExpErrorCode::InvalidRef,
            "unknown reference for this session",
        )
        .with_repair("read the target again to get a fresh reference")
    }

    pub fn expired_ref() -> Self {
        Self::new(ExpErrorCode::ExpiredRef, "the reference is no longer valid")
            .with_repair("read the target again to get a fresh reference")
    }

    pub fn revision_conflict() -> Self {
        Self::new(
            ExpErrorCode::RevisionConflict,
            "the target changed since it was read",
        )
        .with_repair("read the target again and redo the edit against what you see")
    }

    pub fn budget_exceeded(message: impl Into<String>, repair: impl Into<String>) -> Self {
        Self::new(ExpErrorCode::BudgetExceeded, message).with_repair(repair)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ExpErrorCode::InvalidRequest, message)
    }
}
