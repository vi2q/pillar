//! Rust-specific tooling core: saved Cargo metadata, normalized diagnostics,
//! and a verification planner (design: `docs/RUST-TOOLING-DESIGN.md`).
//!
//! This module is the *core* of the Rust workflow experiment. It owns the
//! contract — workspace identity, configurations, diagnostic bundles, verify
//! plans, budgets and error codes — and is deliberately free of OS, TUI and
//! parser dependencies. Cargo execution, LSP sessions and filesystem access
//! live in host adapters, so the CLI and an embedded harness share one
//! implementation of the parsing and planning rules and do not duplicate them
//! (design §2).
//!
//! # What this stage implements, and what it does not
//!
//! The design's stage R1 starts offline (design §12): "先にoffline fixtureで
//! JSON→診断の束とmetadata→計画を固め、次にhost配線する". This module is that
//! offline half:
//!
//! - [`metadata::WorkspaceCatalog`] parses a saved `cargo metadata` document
//!   and resolves changed paths to packages and reverse dependencies;
//! - [`diagnostic::DiagnosticCollector`] classifies a run's stdout/stderr
//!   lines into a [`diagnostic::CollectedRun`], keeping unknown reasons,
//!   non-JSON output and malformed lines instead of dropping them, and keeping
//!   build and test outcomes separate;
//! - [`plan::plan`] turns changed paths plus host-approved configurations into
//!   explicit `cargo test` steps and a structured list of what is *not*
//!   covered.
//!
//! Not yet implemented here (recorded in `docs/TASKS.md`): the
//! `CargoJobBroker` / `ArtifactStore` host ports, the four `rs_*` tool
//! definitions and their host wiring, and the optional rust-analyzer adapter
//! (design R2). No tool is registered or announced until that wiring exists
//! (design §11: 未実装APIを実装済みとして公開しない).
//!
//! # The LMPC boundary
//!
//! The module is behind the `rust-tools` feature, which an embedding profile
//! leaves off (the LMPC minimum builds `pillar-agent` with
//! `--no-default-features`). It uses no `std::process`, `std::fs` or
//! `std::net`, asserted by the LMPC dependency gate, so the pure core can be
//! shared without giving the runtime Cargo, LSP or OS process access
//! (design §1 "NPC / LMPC最小構成へCargo・LSP・OS processを持ち込まない").

pub mod diagnostic;
pub mod error;
pub mod metadata;
pub mod plan;

pub use diagnostic::{
    Applicability, ArtifactRecord, BuildFinished, BuildScriptRecord, BuildStatus, CollectedRun,
    CollectionLimits, CollectionState, CollectionStats, Diagnostic, DiagnosticCollector,
    DiagnosticLevel, DiagnosticSpan, MacroExpansion, ParseError, SourceBinding, SourcePolicy,
    SpanLine, SuggestionGroup, SuggestionReplacement, TestEvidence, TestStatus, UnknownMessage,
    UnstructuredLine,
};
pub use error::{RustToolError, RustToolErrorCode};
pub use metadata::{
    Configuration, DependencyRecord, PackageRecord, TargetRecord, WorkspaceCatalog,
};
pub use plan::{
    Coverage, PlanGoal, PlanRequest, PlanScope, Unverified, UnverifiedKind, VerifyPlan, VerifyStep,
    plan,
};

/// 64-bit FNV-1a, matching the experimental adapter's digest.
///
/// Identity hint only. It keeps the core dependency-free and is enough for a
/// catalog fingerprint, a plan id and a command digest; it is not proof that
/// two builds are input-equal (design §3).
pub fn digest64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Per-call hard caps for the Rust tools (design §8.2, §11).
///
/// The values are host policy. A refusal is a [`RustToolErrorCode::BudgetExceeded`]
/// with an explicit repair; nothing is truncated silently.
///
/// [`RustToolErrorCode::BudgetExceeded`]: error::RustToolErrorCode::BudgetExceeded
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustToolLimits {
    /// Lines retained from one run's stream.
    pub collection: CollectionLimits,
    /// Saved metadata documents accepted at once.
    pub max_saved_metadata: usize,
    /// Diagnostics returned in one response.
    pub max_diagnostics_per_response: usize,
    /// Source bytes fetched in one `rs_diagnostics` call.
    pub max_source_bytes: usize,
    /// Live runs a broker keeps a record of.
    pub max_live_runs: usize,
}

impl Default for RustToolLimits {
    fn default() -> Self {
        Self {
            collection: CollectionLimits::default(),
            max_saved_metadata: 4,
            max_diagnostics_per_response: 50,
            max_source_bytes: 16 * 1024,
            max_live_runs: 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_sensitive() {
        assert_eq!(digest64(b"hello"), digest64(b"hello"));
        assert_ne!(digest64(b"hello"), digest64(b"hellp"));
    }
}
