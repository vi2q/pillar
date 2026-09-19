//! Experimental tool adapter: versioned read references and budgeted edits.
//!
//! Design: `docs/TOOL-EFFICIENCY-DESIGN.md` — §4 (a read reference that an
//! edit consumes), §7 (operation ids and receipts), §8.2 (per-call budgets),
//! §11 stage A. This module is the *core* of that experiment: it owns the
//! contract (references, revisions, receipts, budgets, error codes) and stays
//! free of OS, TUI and parser dependencies, so the CLI and harness entries
//! share one implementation and only the host adapter is platform-specific
//! (design §2: do not duplicate ref/retry logic per entry point).
//!
//! It deliberately adds nothing to the existing tools: `read`, `edit`, `write`,
//! `bash`, `grep` and `find` keep their schemas, content and details (design
//! §9); the `exp_*` tools are opt-in.
//!
//! # What stage A promises, and what it does not
//!
//! - The host's guarantee is *declared*, never assumed. [`Guarantee::Strict`]
//!   means the host can inspect an expected revision and publish atomically
//!   (an in-memory or VFS host, [`MemoryHost`] here); [`Guarantee::Weak`] (a
//!   plain native filesystem, where a compare-then-rename races an external
//!   writer) is refused by [`exp_edit`] with
//!   [`ExpErrorCode::UnsupportedGuarantee`] instead of being silently
//!   downgraded (design §4.3).
//! - A reference is not authorization: every operation re-checks through the
//!   host, and a reference belonging to another owner or another host
//!   generation is reported as [`ExpErrorCode::InvalidRef`] /
//!   [`ExpErrorCode::ExpiredRef`] *without* echoing the path or digest
//!   (design §3).
//! - Digests are identity hints, not proof of history or of permission: the
//!   store's revision comparison is the gate (design §3).
//! - The operation ledger is in memory for one host generation. A full ledger
//!   refuses new operations instead of evicting a receipt that a retry would
//!   then re-apply; durable recovery is stage D (design §7.2).
//! - Stage A covers one file per edit and no insert/create/delete/rename or
//!   multi-file transaction; line numbers address *display*, byte ranges are
//!   the identity that edits carry (design §4.2).
//!
//! Not yet in this module (recorded in `docs/TASKS.md`): the `exp_read` /
//! `exp_edit` tool definitions and schemas, and the argument-token comparison
//! against the existing `read` / `edit` pair.

pub mod edit;
pub mod error;
pub mod ledger;
pub mod read;
pub mod refs;
pub mod store;
pub mod tools;

pub use edit::{ExpEditRequest, ExpEditRequestItem, ExpEditResponse, exp_edit};
pub use error::{ExpError, ExpErrorCode};
pub use ledger::{OperationId, OperationLedger, Receipt, Reservation};
pub use read::{
    DeliveryState, ExpRange, ExpReadRequest, ExpReadResponse, WithheldReason, exp_read,
};
pub use refs::{ByteRange, Clock, OwnerId, RefId, RefRecord, RefStore, manual_clock};
#[cfg(not(target_arch = "wasm32"))]
pub use refs::system_clock;
pub use store::{
    ConditionalStore, Guarantee, MemoryHost, ResourceId, Revision, Snapshot, digest64,
};
pub use tools::{
    ExpToolkit, exp_edit_description, exp_edit_parameters_json, exp_read_description,
    exp_read_parameters_json,
};

/// Per-call hard caps (design §8.2: a call-level cap first, a cross-cutting
/// scheduler only once measurements justify one).
///
/// A budget refusal is [`ExpErrorCode::BudgetExceeded`] with a repair hint that
/// says how to proceed explicitly; nothing is truncated silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpLimits {
    /// Largest response body a single read may deliver.
    pub max_read_bytes: usize,
    /// Largest line count a single read may deliver.
    pub max_read_lines: usize,
    /// Longest lifetime a reference may be issued with.
    pub max_ref_ttl_ms: u64,
    /// Live references held at once; expired ones are reclaimed first.
    pub max_live_refs: usize,
    /// Replacements accepted in one edit call.
    pub max_edits_per_call: usize,
    /// Total replacement bytes accepted in one edit call.
    pub max_edit_bytes: usize,
    /// Operation receipts retained for the live generation.
    pub ledger_capacity: usize,
}

impl Default for ExpLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: 32 * 1024,
            max_read_lines: 1_000,
            max_ref_ttl_ms: 5 * 60 * 1_000,
            max_live_refs: 256,
            max_edits_per_call: 8,
            max_edit_bytes: 256 * 1024,
            ledger_capacity: 256,
        }
    }
}
