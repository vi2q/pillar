//! Stage A gates for the experimental tool adapter
//! (`docs/TOOL-EFFICIENCY-DESIGN.md` §10.1, §11): references, edits, conflicts
//! and duplicate operations, decided by the in-memory conditional-update host.
//!
//! Every assertion here is a deterministic gate, not a benchmark: a rejected
//! call must report the design's error code *and* leave the target
//! byte-identical.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use pillar_agent::exp::{
    ByteRange, ConditionalStore, DeliveryState, ExpEditRequest, ExpEditRequestItem, ExpError,
    ExpErrorCode, ExpLimits, ExpRange, ExpReadRequest, Guarantee, MemoryHost, OperationId,
    OperationLedger, OwnerId, RefId, RefStore, ResourceId, Revision, Snapshot, WithheldReason,
    exp_edit, exp_read, manual_clock,
};

// --- fixtures -------------------------------------------------------------

struct Fixture {
    host: Arc<MemoryHost>,
    refs: Arc<RefStore>,
    owner: OwnerId,
    now: Arc<AtomicU64>,
    limits: ExpLimits,
}

impl Fixture {
    fn new() -> Self {
        let limits = ExpLimits::default();
        let (clock, now) = manual_clock(1_000);
        let next = Arc::new(AtomicU64::new(0));
        let ids: Arc<dyn Fn() -> String + Send + Sync> =
            Arc::new(move || format!("ref-{}", next.fetch_add(1, Ordering::SeqCst)));
        Self {
            host: Arc::new(MemoryHost::new()),
            refs: Arc::new(RefStore::with_id_source(
                clock,
                limits.max_live_refs,
                ids,
            )),
            owner: OwnerId::new("session-a"),
            now,
            limits,
        }
    }

    fn with_file(bytes: &[u8]) -> Self {
        let fixture = Self::new();
        fixture.host.set_file("f.txt", bytes);
        fixture
    }
}

fn read_request(start_line: usize, line_count: usize) -> ExpReadRequest {
    ExpReadRequest {
        path: "f.txt".to_string(),
        range: ExpRange {
            start_line,
            line_count,
        },
    }
}

fn edit_request(operation: &str, edits: &[(&RefId, &str)]) -> ExpEditRequest {
    ExpEditRequest {
        operation_id: OperationId::new(operation),
        edits: edits
            .iter()
            .map(|(reference, replacement)| ExpEditRequestItem {
                reference: (*reference).clone(),
                replacement: (*replacement).to_string(),
            })
            .collect(),
    }
}

/// Read a range and hand back its reference (panics when none was issued).
async fn read_ref(fixture: &Fixture, start_line: usize, line_count: usize) -> RefId {
    let response = exp_read(
        fixture.host.as_ref(),
        &fixture.refs,
        &fixture.limits,
        &fixture.owner,
        &read_request(start_line, line_count),
    )
    .await
    .expect("read");
    response.reference.expect("an editable reference")
}

fn ledger(fixture: &Fixture) -> Arc<OperationLedger> {
    Arc::new(OperationLedger::new(fixture.limits.ledger_capacity))
}

// --- hosts used by the gates ---------------------------------------------

/// A host that can only promise a weak publication (a plain filesystem).
struct WeakHost(MemoryHost);

#[async_trait]
impl ConditionalStore for WeakHost {
    fn guarantee(&self) -> Guarantee {
        Guarantee::Weak
    }

    async fn resolve(&self, path: &str) -> Result<ResourceId, ExpError> {
        self.0.resolve(path).await
    }

    async fn snapshot(&self, resource: &ResourceId) -> Result<Snapshot, ExpError> {
        self.0.snapshot(resource).await
    }

    async fn compare_and_swap(
        &self,
        resource: &ResourceId,
        expected: Revision,
        new_bytes: &[u8],
    ) -> Result<Revision, ExpError> {
        self.0.compare_and_swap(resource, expected, new_bytes).await
    }
}

/// A host whose authorization for the target was withdrawn after the read.
struct RevokedHost(MemoryHost);

#[async_trait]
impl ConditionalStore for RevokedHost {
    fn guarantee(&self) -> Guarantee {
        Guarantee::Strict
    }

    async fn resolve(&self, _path: &str) -> Result<ResourceId, ExpError> {
        Err(ExpError::new(
            ExpErrorCode::PermissionDenied,
            "access to this target was revoked",
        ))
    }

    async fn snapshot(&self, resource: &ResourceId) -> Result<Snapshot, ExpError> {
        self.0.snapshot(resource).await
    }

    async fn compare_and_swap(
        &self,
        resource: &ResourceId,
        expected: Revision,
        new_bytes: &[u8],
    ) -> Result<Revision, ExpError> {
        self.0.compare_and_swap(resource, expected, new_bytes).await
    }
}

/// A strict host that pauses inside the publication check, so a second call
/// with the same operation id really is in flight at the same time.
struct GatedHost {
    inner: MemoryHost,
    inside: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl GatedHost {
    fn new() -> Self {
        Self {
            inner: MemoryHost::new(),
            inside: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

#[async_trait]
impl ConditionalStore for GatedHost {
    fn guarantee(&self) -> Guarantee {
        Guarantee::Strict
    }

    async fn resolve(&self, path: &str) -> Result<ResourceId, ExpError> {
        self.inner.resolve(path).await
    }

    async fn snapshot(&self, resource: &ResourceId) -> Result<Snapshot, ExpError> {
        self.inner.snapshot(resource).await
    }

    async fn compare_and_swap(
        &self,
        resource: &ResourceId,
        expected: Revision,
        new_bytes: &[u8],
    ) -> Result<Revision, ExpError> {
        self.inside.notify_one();
        self.release.notified().await;
        self.inner
            .compare_and_swap(resource, expected, new_bytes)
            .await
    }
}

// --- references (§10.1 参照) ---------------------------------------------

#[tokio::test]
async fn a_read_issues_a_reference_for_the_delivered_range() {
    let fixture = Fixture::with_file(b"one\ntwo\nthree\n");
    let response = exp_read(
        fixture.host.as_ref(),
        &fixture.refs,
        &fixture.limits,
        &fixture.owner,
        &read_request(2, 1),
    )
    .await
    .expect("read");

    assert_eq!(response.text, "two\n");
    assert_eq!(response.start_line, 2);
    assert_eq!(response.line_count, 1);
    assert_eq!(response.total_lines, 3);
    assert_eq!(response.delivery_state, DeliveryState::Complete);
    assert_eq!(response.guarantee, Guarantee::Strict);
    assert!(response.editable);
    assert!(response.reference.is_some());
    assert!(response.withheld.is_none());
}

#[tokio::test]
async fn a_foreign_reference_is_rejected_without_leaking_the_target() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;

    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &OwnerId::new("session-b"),
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect_err("foreign reference");

    assert_eq!(error.code, ExpErrorCode::InvalidRef);
    assert!(
        !error.message.contains("f.txt") && !error.message.contains("ref-"),
        "the refusal must not describe the target: {error}"
    );
    assert_eq!(fixture.host.content("f.txt").as_deref(), Some(&b"one\ntwo\n"[..]));
}

#[tokio::test]
async fn an_expired_reference_is_rejected() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;

    fixture.now.store(
        1_000 + fixture.limits.max_ref_ttl_ms + 1,
        Ordering::SeqCst,
    );
    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect_err("expired");

    assert_eq!(error.code, ExpErrorCode::ExpiredRef);
    assert_eq!(fixture.host.content("f.txt").as_deref(), Some(&b"one\ntwo\n"[..]));
}

#[tokio::test]
async fn a_restart_expires_old_references() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;

    fixture.refs.restart();
    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect_err("restart");

    assert_eq!(error.code, ExpErrorCode::ExpiredRef);
}

#[tokio::test]
async fn replacing_the_path_conflicts_instead_of_writing_the_old_resource() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;

    // A different file now lives at the same path (create-over / rename).
    fixture.host.set_file("f.txt", b"other\ncontent\n");
    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect_err("path swap");

    assert_eq!(error.code, ExpErrorCode::RevisionConflict);
    assert_eq!(
        fixture.host.content("f.txt").as_deref(),
        Some(&b"other\ncontent\n"[..])
    );
}

#[tokio::test]
async fn revoked_authorization_is_rejected_and_changes_nothing() {
    let inner = MemoryHost::new();
    inner.set_file("f.txt", b"one\ntwo\n");
    let limits = ExpLimits::default();
    let (clock, _now) = manual_clock(0);
    let refs = Arc::new(RefStore::new(clock, limits.max_live_refs));
    let owner = OwnerId::new("session-a");

    let response = exp_read(
        &inner,
        &refs,
        &limits,
        &owner,
        &read_request(1, 1),
    )
    .await
    .expect("read");
    let reference = response.reference.expect("reference");

    let revoked = RevokedHost(MemoryHost::new());
    revoked.0.set_file("f.txt", b"one\ntwo\n");
    let error = exp_edit(
        &revoked,
        &refs,
        &Arc::new(OperationLedger::new(8)),
        &limits,
        &owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect_err("revoked");

    assert_eq!(error.code, ExpErrorCode::PermissionDenied);
}

#[tokio::test]
async fn a_weak_host_reads_but_refuses_a_strict_edit() {
    let inner = MemoryHost::new();
    inner.set_file("f.txt", b"one\ntwo\n");
    let weak = WeakHost(inner);
    let limits = ExpLimits::default();
    let (clock, _now) = manual_clock(0);
    let refs = Arc::new(RefStore::new(clock, limits.max_live_refs));
    let owner = OwnerId::new("session-a");

    let response = exp_read(&weak, &refs, &limits, &owner, &read_request(1, 1))
        .await
        .expect("read");
    assert_eq!(response.guarantee, Guarantee::Weak);
    assert!(!response.editable, "a weak host cannot promise a strict edit");
    let reference = response.reference.expect("reference is still issued");

    let error = exp_edit(
        &weak,
        &refs,
        &Arc::new(OperationLedger::new(8)),
        &limits,
        &owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect_err("weak");
    assert_eq!(error.code, ExpErrorCode::UnsupportedGuarantee);
    assert_eq!(weak.0.content("f.txt").as_deref(), Some(&b"one\ntwo\n"[..]));
}

// --- edits (§10.1 編集) ---------------------------------------------------

#[tokio::test]
async fn only_the_referenced_occurrence_of_repeated_text_changes() {
    let fixture = Fixture::with_file(b"same\nother\nsame\nsame\n");
    let reference = read_ref(&fixture, 2, 1).await;

    let response = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "second\n")]),
    )
    .await
    .expect("edit");

    assert_eq!(response.receipt.edits_applied, 1);
    assert_eq!(response.receipt.bytes_written, 22);
    assert_eq!(
        fixture.host.content("f.txt").as_deref(),
        Some(&b"same\nsecond\nsame\nsame\n"[..])
    );
}

#[tokio::test]
async fn overlapping_references_are_rejected_before_any_side_effect() {
    let fixture = Fixture::with_file(b"one\ntwo\nthree\n");
    let first = read_ref(&fixture, 1, 2).await;
    let second = read_ref(&fixture, 2, 2).await;

    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&first, "ONE\nTWO\n"), (&second, "TWO\nTHREE\n")]),
    )
    .await
    .expect_err("overlap");

    assert_eq!(error.code, ExpErrorCode::InvalidRequest);
    assert_eq!(
        fixture.host.content("f.txt").as_deref(),
        Some(&b"one\ntwo\nthree\n"[..])
    );
}

#[tokio::test]
async fn crlf_bom_and_trailing_newline_survive_outside_the_range() {
    let fixture = Fixture::with_file("\u{feff}one\r\ntwo\r\nthree\r\n".as_bytes());
    // Lines: BOM+one\r\n, two\r\n, three\r\n.
    let reference = read_ref(&fixture, 2, 1).await;

    exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "TWO\r\n")]),
    )
    .await
    .expect("edit");

    assert_eq!(
        String::from_utf8(fixture.host.content("f.txt").expect("content")).expect("utf8"),
        "\u{feff}one\r\nTWO\r\nthree\r\n"
    );
}

#[tokio::test]
async fn the_replacement_is_literal_not_normalized() {
    let fixture = Fixture::with_file("α\nβ\n".as_bytes());
    let reference = read_ref(&fixture, 2, 1).await;

    // LF where the file uses LF, and a newline style the replacement states
    // explicitly, with a multi-byte character at both ends.
    exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "Ω\n")]),
    )
    .await
    .expect("edit");

    assert_eq!(
        String::from_utf8(fixture.host.content("f.txt").expect("content")).expect("utf8"),
        "α\nΩ\n"
    );
}

#[tokio::test]
async fn a_non_utf8_target_yields_no_editable_reference() {
    let fixture = Fixture::with_file(b"one\n\xff\xfe\ntwo\n");
    let response = exp_read(
        fixture.host.as_ref(),
        &fixture.refs,
        &fixture.limits,
        &fixture.owner,
        &read_request(1, 1),
    )
    .await
    .expect("read");

    assert!(!response.editable);
    assert!(response.reference.is_none());
    assert_eq!(response.withheld, Some(WithheldReason::NotUtf8));
}

#[tokio::test]
async fn a_range_over_the_budget_is_partial_and_not_editable() {
    let fixture = Fixture::with_file(b"one\ntwo\nthree\n");
    let limits = ExpLimits {
        max_read_lines: 2,
        ..ExpLimits::default()
    };

    let response = exp_read(
        fixture.host.as_ref(),
        &fixture.refs,
        &limits,
        &fixture.owner,
        &read_request(1, 3),
    )
    .await
    .expect("read");

    assert_eq!(response.delivery_state, DeliveryState::Partial);
    assert_eq!(response.line_count, 2);
    assert_eq!(response.text, "one\ntwo\n");
    assert!(!response.editable);
    assert!(response.reference.is_none());
    assert_eq!(response.withheld, Some(WithheldReason::BudgetExceeded));
    assert!(response.repair.is_some());
}

#[tokio::test]
async fn reading_beyond_the_file_is_an_invalid_request() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let error = exp_read(
        fixture.host.as_ref(),
        &fixture.refs,
        &fixture.limits,
        &fixture.owner,
        &read_request(9, 1),
    )
    .await
    .expect_err("beyond the end");
    assert_eq!(error.code, ExpErrorCode::InvalidRequest);
}

// --- conflicts (§10.1 競合) ----------------------------------------------

#[tokio::test]
async fn an_external_change_after_the_read_is_a_conflict() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;

    fixture.host.rewrite_file("f.txt", b"one\nTWO\n");
    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect_err("conflict");

    assert_eq!(error.code, ExpErrorCode::RevisionConflict);
    assert_eq!(
        fixture.host.content("f.txt").as_deref(),
        Some(&b"one\nTWO\n"[..])
    );
}

#[tokio::test]
async fn an_aba_write_is_a_conflict() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;

    // The content ends up identical; the generation moved, and that is what
    // the publication check compares.
    fixture.host.rewrite_file("f.txt", b"one\ntwo\n");
    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect_err("aba");

    assert_eq!(error.code, ExpErrorCode::RevisionConflict);
    assert_eq!(
        fixture.host.content("f.txt").as_deref(),
        Some(&b"one\ntwo\n"[..])
    );
}

// --- retries (§10.1 再送) -------------------------------------------------

#[tokio::test]
async fn a_duplicate_call_returns_the_same_receipt_and_applies_once() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;
    let ledger = ledger(&fixture);

    let first = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger,
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect("first");
    let second = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger,
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect("duplicate");

    assert_eq!(first.receipt, second.receipt);
    assert_eq!(
        fixture.host.content("f.txt").as_deref(),
        Some(&b"ONE\ntwo\n"[..])
    );
}

#[tokio::test]
async fn the_same_operation_id_with_different_arguments_is_rejected() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let first = read_ref(&fixture, 1, 1).await;
    let second = read_ref(&fixture, 2, 1).await;
    let ledger = ledger(&fixture);

    exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger,
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&first, "ONE\n")]),
    )
    .await
    .expect("first");

    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger,
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&second, "TWO\n")]),
    )
    .await
    .expect_err("mismatch");

    assert_eq!(error.code, ExpErrorCode::OperationIdMismatch);
    assert_eq!(
        fixture.host.content("f.txt").as_deref(),
        Some(&b"ONE\ntwo\n"[..])
    );
}

#[tokio::test]
async fn an_in_flight_duplicate_joins_and_does_not_double_apply() {
    let host = Arc::new(GatedHost::new());
    host.inner.set_file("f.txt", b"one\ntwo\n");
    let limits = ExpLimits::default();
    let (clock, _now) = manual_clock(0);
    let refs = Arc::new(RefStore::with_id_source(
        clock,
        limits.max_live_refs,
        Arc::new(|| "ref-fixed".to_string()),
    ));
    let owner = OwnerId::new("session-a");
    let response = exp_read(host.as_ref(), &refs, &limits, &owner, &read_request(1, 1))
        .await
        .expect("read");
    let reference = response.reference.expect("reference");
    let ledger = Arc::new(OperationLedger::new(8));

    let request = edit_request("op-1", &[(&reference, "ONE\n")]);
    let first = {
        let (host, refs, ledger, owner, request, limits) = (
            Arc::clone(&host),
            Arc::clone(&refs),
            Arc::clone(&ledger),
            owner.clone(),
            request.clone(),
            limits.clone(),
        );
        tokio::spawn(async move {
            exp_edit(
                host.as_ref(),
                &refs,
                &ledger,
                &limits,
                &owner,
                &request,
            )
            .await
        })
    };
    // The first call is now inside the publication check.
    host.inside.notified().await;

    let second = {
        let (host, refs, ledger, owner, request, limits) = (
            Arc::clone(&host),
            Arc::clone(&refs),
            Arc::clone(&ledger),
            owner.clone(),
            request,
            limits,
        );
        tokio::spawn(async move {
            exp_edit(
                host.as_ref(),
                &refs,
                &ledger,
                &limits,
                &owner,
                &request,
            )
            .await
        })
    };
    tokio::task::yield_now().await;
    host.release.notify_waiters();

    let first = first.await.expect("join").expect("first edit");
    let second = second.await.expect("join").expect("duplicate");
    assert_eq!(first.receipt, second.receipt);
    assert_eq!(
        host.inner.content("f.txt").as_deref(),
        Some(&b"ONE\ntwo\n"[..]),
        "the duplicate must not apply a second time"
    );
}

#[tokio::test]
async fn a_cancelled_operation_is_unknown_and_leaves_no_change() {
    let host = Arc::new(GatedHost::new());
    host.inner.set_file("f.txt", b"one\ntwo\n");
    let limits = ExpLimits::default();
    let (clock, _now) = manual_clock(0);
    let refs = Arc::new(RefStore::with_id_source(
        clock,
        limits.max_live_refs,
        Arc::new(|| "ref-fixed".to_string()),
    ));
    let owner = OwnerId::new("session-a");
    let response = exp_read(host.as_ref(), &refs, &limits, &owner, &read_request(1, 1))
        .await
        .expect("read");
    let reference = response.reference.expect("reference");
    let ledger = Arc::new(OperationLedger::new(8));
    let request = edit_request("op-1", &[(&reference, "ONE\n")]);

    // Start the call, let it reach the publication check, then cancel it: a
    // cancellation before publication is a definite "no change".
    let task = {
        let (host, refs, ledger, owner, request, limits) = (
            Arc::clone(&host),
            Arc::clone(&refs),
            Arc::clone(&ledger),
            owner.clone(),
            request.clone(),
            limits.clone(),
        );
        tokio::spawn(async move {
            exp_edit(host.as_ref(), &refs, &ledger, &limits, &owner, &request).await
        })
    };
    host.inside.notified().await;
    task.abort();
    let _ = task.await;
    assert_eq!(
        host.inner.content("f.txt").as_deref(),
        Some(&b"one\ntwo\n"[..])
    );

    // The abandoned operation is unknown, never success, and a retry of the
    // same id does not apply it behind the caller's back.
    let error = exp_edit(host.as_ref(), &refs, &ledger, &limits, &owner, &request)
        .await
        .expect_err("unknown");
    assert_eq!(error.code, ExpErrorCode::OutcomeUnknown);
    assert_eq!(
        host.inner.content("f.txt").as_deref(),
        Some(&b"one\ntwo\n"[..])
    );
}

#[tokio::test]
async fn a_full_ledger_refuses_a_new_operation_without_publishing() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;
    let ledger = Arc::new(OperationLedger::new(1));

    exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger,
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n")]),
    )
    .await
    .expect("first");

    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger,
        &fixture.limits,
        &fixture.owner,
        &edit_request("op-2", &[(&reference, "One\n")]),
    )
    .await
    .expect_err("full");

    assert_eq!(error.code, ExpErrorCode::BudgetExceeded);
    assert_eq!(
        fixture.host.content("f.txt").as_deref(),
        Some(&b"ONE\ntwo\n"[..])
    );
}

#[tokio::test]
async fn too_many_replacements_in_one_call_is_a_budget_refusal() {
    let fixture = Fixture::with_file(b"one\ntwo\n");
    let reference = read_ref(&fixture, 1, 1).await;
    let limits = ExpLimits {
        max_edits_per_call: 1,
        ..ExpLimits::default()
    };

    let error = exp_edit(
        fixture.host.as_ref(),
        &fixture.refs,
        &ledger(&fixture),
        &limits,
        &fixture.owner,
        &edit_request("op-1", &[(&reference, "ONE\n"), (&reference, "again\n")]),
    )
    .await
    .expect_err("budget");
    assert_eq!(error.code, ExpErrorCode::BudgetExceeded);
}

#[test]
fn ranges_overlap_only_when_they_share_bytes() {
    assert!(ByteRange::new(0, 4).overlaps(&ByteRange::new(3, 5)));
    assert!(!ByteRange::new(0, 4).overlaps(&ByteRange::new(4, 5)));
    assert!(ByteRange::new(0, 4).overlaps(&ByteRange::new(0, 4)));
}
