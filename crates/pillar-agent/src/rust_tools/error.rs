//! The Rust tooling error surface (design §11, "共通エラーの追加").
//!
//! The codes are stable and model-visible: a caller branches on the code and
//! follows the one-line repair instead of reading a stack trace. The list is
//! closed on purpose; a host failure with no code for it is [`HostFailure`].
//!
//! [`HostFailure`]: RustToolErrorCode::HostFailure

use serde::{Deserialize, Serialize};

/// Stable error codes for the Rust workflow tools.
///
/// Each variant carries the meaning the design gives it, so a repair can be
/// derived without guessing:
///
/// - [`MetadataUnavailable`]: no saved Cargo metadata, or it could not be read.
/// - [`StalePlan`]: the workspace, configuration or authorization changed
///   since the plan was made; the old commands must not run silently.
/// - [`ConfigurationMismatch`]: a requested configuration id is unknown, or an
///   observed artifact does not belong to the expected configuration.
/// - [`AnalysisUnavailable`]: an optional semantic provider (rust-analyzer) is
///   not available; callers fall back to source text.
/// - [`SourceUnbound`]: a diagnostic span does not map to an authorized source
///   file, so it is not an editing target.
/// - [`UnsupportedSuggestion`]: the suggestion cannot be applied under the
///   initial conditions (not `MachineApplicable`, a macro/registry/sysroot
///   span, zero-length insertion, or a weak host).
/// - [`RunLost`]: the broker cannot determine the outcome of a run (it exited
///   uncleanly, or its result was not collected). Never reported as success.
/// - [`InvalidRequest`]: the request itself is malformed.
/// - [`BudgetExceeded`]: a per-call cap would be exceeded; nothing ran.
/// - [`PermissionDenied`]: the host refused authorization. A refusal must not
///   leak the existence, path or digest of the target.
/// - [`HostFailure`]: the host failed for a reason it has no code for.
///
/// [`MetadataUnavailable`]: RustToolErrorCode::MetadataUnavailable
/// [`StalePlan`]: RustToolErrorCode::StalePlan
/// [`ConfigurationMismatch`]: RustToolErrorCode::ConfigurationMismatch
/// [`AnalysisUnavailable`]: RustToolErrorCode::AnalysisUnavailable
/// [`SourceUnbound`]: RustToolErrorCode::SourceUnbound
/// [`UnsupportedSuggestion`]: RustToolErrorCode::UnsupportedSuggestion
/// [`RunLost`]: RustToolErrorCode::RunLost
/// [`InvalidRequest`]: RustToolErrorCode::InvalidRequest
/// [`BudgetExceeded`]: RustToolErrorCode::BudgetExceeded
/// [`PermissionDenied`]: RustToolErrorCode::PermissionDenied
/// [`HostFailure`]: RustToolErrorCode::HostFailure
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustToolErrorCode {
    MetadataUnavailable,
    StalePlan,
    ConfigurationMismatch,
    AnalysisUnavailable,
    SourceUnbound,
    UnsupportedSuggestion,
    RunLost,
    InvalidRequest,
    BudgetExceeded,
    PermissionDenied,
    HostFailure,
}

impl RustToolErrorCode {
    /// The wire form (the `serde` name, also usable in a plain string).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MetadataUnavailable => "metadata_unavailable",
            Self::StalePlan => "stale_plan",
            Self::ConfigurationMismatch => "configuration_mismatch",
            Self::AnalysisUnavailable => "analysis_unavailable",
            Self::SourceUnbound => "source_unbound",
            Self::UnsupportedSuggestion => "unsupported_suggestion",
            Self::RunLost => "run_lost",
            Self::InvalidRequest => "invalid_request",
            Self::BudgetExceeded => "budget_exceeded",
            Self::PermissionDenied => "permission_denied",
            Self::HostFailure => "host_failure",
        }
    }
}

impl std::fmt::Display for RustToolErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Rust tooling error: a stable code, a message for the model, and an
/// optional one-line repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct RustToolError {
    pub code: RustToolErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<String>,
}

impl RustToolError {
    pub fn new(code: RustToolErrorCode, message: impl Into<String>) -> Self {
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

    pub fn metadata_unavailable(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::MetadataUnavailable, message)
            .with_repair("refresh the workspace metadata explicitly, then plan again")
    }

    pub fn stale_plan(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::StalePlan, message)
            .with_repair("re-plan against the current metadata and configuration")
    }

    pub fn configuration_mismatch(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::ConfigurationMismatch, message)
            .with_repair("use a configuration id the host has approved")
    }

    pub fn analysis_unavailable(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::AnalysisUnavailable, message)
            .with_repair("fall back to reading the declaration's source text")
    }

    pub fn source_unbound(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::SourceUnbound, message)
            .with_repair("treat this location as read-only; it is not an editable workspace file")
    }

    pub fn unsupported_suggestion(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::UnsupportedSuggestion, message)
            .with_repair("apply the change with a normal edit, or preview the suggestion only")
    }

    pub fn run_lost(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::RunLost, message)
            .with_repair("start a new run; do not treat the lost run as a success")
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::InvalidRequest, message)
    }

    pub fn budget_exceeded(message: impl Into<String>, repair: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::BudgetExceeded, message).with_repair(repair)
    }

    pub fn host_failure(message: impl Into<String>) -> Self {
        Self::new(RustToolErrorCode::HostFailure, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_codes_are_stable_snake_case() {
        assert_eq!(
            RustToolErrorCode::MetadataUnavailable.as_str(),
            "metadata_unavailable"
        );
        assert_eq!(RustToolErrorCode::RunLost.as_str(), "run_lost");
        let json = serde_json::to_string(&RustToolErrorCode::StalePlan).unwrap();
        assert_eq!(json, "\"stale_plan\"");
        let parsed: RustToolErrorCode = serde_json::from_str("\"stale_plan\"").unwrap();
        assert_eq!(parsed, RustToolErrorCode::StalePlan);
    }

    #[test]
    fn a_repair_is_carried_but_optional() {
        let error = RustToolError::invalid_request("missing configuration_ids");
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["code"], "invalid_request");
        assert!(json.get("repair").is_none());

        let with_repair = RustToolError::stale_plan("metadata moved");
        assert!(with_repair.repair.is_some());
    }
}
