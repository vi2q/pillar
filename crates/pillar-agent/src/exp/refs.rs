//! References: an opaque id that names a resource, a revision and a byte
//! range for as long as the host generation and the lifetime allow (design
//! §3, §4.1, §4.2).
//!
//! A reference is *not* a write permission: the host re-checks authorization
//! on every operation, and the ref only says which bytes of which revision
//! the model was shown.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::error::ExpError;
#[cfg(test)]
use super::error::ExpErrorCode;
use super::store::{ResourceId, Revision};

/// Milliseconds since an unspecified epoch.
///
/// Injected rather than read from the platform: tests then control expiry and
/// TTL exactly, and a host without a wall clock (the Wasm profiles) supplies
/// its own.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A clock a test advances by hand. Returns the clock and the cell it reads.
pub fn manual_clock(start_ms: u64) -> (Clock, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(start_ms));
    let handle = Arc::clone(&now);
    let clock: Clock = Arc::new(move || handle.load(Ordering::SeqCst));
    (clock, now)
}

/// A wall clock over `SystemTime`, for hosts that have one.
#[cfg(not(target_arch = "wasm32"))]
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    })
}

/// Who holds a reference. References are bound to an owner (a session) and to
/// a host generation: another owner, or the same owner after a restart, must
/// not be able to use one (design §10.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnerId(String);

impl OwnerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An opaque reference id. Opaque means the id carries nothing the model can
/// reinterpret as a path, an offset or a permission.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RefId(String);

impl RefId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RefId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A half-open byte range. Edits carry byte ranges, never line numbers: line
/// numbers address display only and drift as soon as anything else changes
/// (design §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl ByteRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// A short, unguessable reference id: 16 hex characters (64 bits) drawn from
/// the entropy source `pillar_ai` already uses for uuidv7.
///
/// Measured on real edit traffic (`docs/PERF-BASELINE.md`): the reference form
/// saves 29.7% of argument bytes with 36-character UUIDs and 32.9% with 16
/// characters, and the share of calls where the envelope costs more than the
/// repeated text drops from 4.2% to 0.85%. 64 bits is still not guessable, and
/// every lookup is additionally bound to the owner, the host generation and the
/// lifetime (design §3).
pub fn opaque_id() -> String {
    format!(
        "{:016x}",
        super::store::digest64(pillar_ai::uuid::uuidv7().as_bytes())
    )
}

/// What the store remembers about a reference.
#[derive(Debug, Clone)]
pub struct RefRecord {
    pub id: RefId,
    pub owner: OwnerId,
    /// The path the reference was read through (display, and the target of the
    /// path-identity check before publishing).
    pub path: String,
    pub resource: ResourceId,
    pub revision: Revision,
    pub range: ByteRange,
    /// Digest of the referenced bytes (identity hint; design §3).
    pub range_digest: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    /// Only a range that was fully delivered to the model is editable
    /// (design §4.2).
    pub editable: bool,
    issued_generation: u64,
}

/// Short-lived reference store for one host generation.
///
/// Deliberately in memory and per generation (design §11 stage A): a restart
/// bumps the generation, so old ids are reported expired rather than treated
/// as fresh. A bounded store reclaims expired entries and otherwise refuses
/// new ones — it never evicts a live reference.
pub struct RefStore {
    refs: Mutex<HashMap<String, RefRecord>>,
    clock: Clock,
    generation: AtomicU64,
    capacity: usize,
    id_source: Arc<dyn Fn() -> String + Send + Sync>,
}

impl RefStore {
    /// A store with short opaque ids (see [`opaque_id`]) and room for
    /// `capacity` live references.
    pub fn new(clock: Clock, capacity: usize) -> Self {
        Self::with_id_source(clock, capacity, Arc::new(opaque_id))
    }

    /// A store with an injected id source, for deterministic tests.
    pub fn with_id_source(
        clock: Clock,
        capacity: usize,
        id_source: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        Self {
            refs: Mutex::new(HashMap::new()),
            clock,
            generation: AtomicU64::new(1),
            capacity,
            id_source,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Start a new host generation: every existing reference becomes expired
    /// (design §7.2 — an id must not be reused as if nothing happened). The
    /// records are kept so a lookup can say *why* it is refused, and they are
    /// reclaimed by lifetime or by the next capacity check.
    pub fn restart(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn live_count(&self) -> usize {
        self.refs.lock().expect("ref store lock").len()
    }

    /// Issue a reference for a fully delivered range.
    pub fn issue(&self, record: RefRecord, ttl_ms: u64) -> Result<RefId, ExpError> {
        let now = (self.clock)();
        let mut refs = self.refs.lock().expect("ref store lock");

        if refs.len() >= self.capacity {
            let generation = self.generation.load(Ordering::SeqCst);
            refs.retain(|_, record| {
                record.expires_at_ms > now && record.issued_generation == generation
            });
            if refs.len() >= self.capacity {
                return Err(ExpError::budget_exceeded(
                    format!("more than {} live references", self.capacity),
                    "finish or drop references before reading more",
                ));
            }
        }

        let id = RefId::new((self.id_source)());
        let record = RefRecord {
            id: id.clone(),
            expires_at_ms: now.saturating_add(ttl_ms),
            issued_at_ms: now,
            issued_generation: self.generation.load(Ordering::SeqCst),
            ..record
        };
        refs.insert(id.as_str().to_string(), record);
        Ok(id)
    }

    /// Look up a reference for an edit.
    ///
    /// A reference that is unknown, foreign, or from another generation is
    /// reported without echoing the path, the digest or anything else about
    /// the target (design §3).
    pub fn lookup(&self, owner: &OwnerId, id: &RefId) -> Result<RefRecord, ExpError> {
        let now = (self.clock)();
        let mut refs = self.refs.lock().expect("ref store lock");
        let record = refs
            .get(id.as_str())
            .cloned()
            .ok_or_else(ExpError::invalid_ref)?;

        if &record.owner != owner {
            return Err(ExpError::invalid_ref());
        }
        if record.issued_generation != self.generation.load(Ordering::SeqCst) {
            refs.remove(id.as_str());
            return Err(ExpError::expired_ref());
        }
        if record.expires_at_ms <= now {
            refs.remove(id.as_str());
            return Err(ExpError::expired_ref());
        }
        Ok(record)
    }

    /// Drop every reference to a resource whose revision moved on: a stale
    /// reference must not be handed back as if it were fresh (design §8.1).
    pub fn invalidate_resource(&self, resource: &ResourceId) {
        self.refs
            .lock()
            .expect("ref store lock")
            .retain(|_, record| &record.resource != resource);
    }
}

impl RefRecord {
    /// A record for the given owner; `issued_at_ms`/`expires_at_ms` are filled
    /// in by [`RefStore::issue`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner: &OwnerId,
        path: &str,
        resource: &ResourceId,
        revision: Revision,
        range: ByteRange,
        range_digest: u64,
        editable: bool,
    ) -> Self {
        Self {
            id: RefId::new(String::new()),
            owner: owner.clone(),
            path: path.to_string(),
            resource: resource.clone(),
            revision,
            range,
            range_digest,
            issued_at_ms: 0,
            expires_at_ms: 0,
            editable,
            issued_generation: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn counter_ids() -> Arc<dyn Fn() -> String + Send + Sync> {
        let next = Arc::new(AtomicU64::new(0));
        Arc::new(move || format!("ref-{}", next.fetch_add(1, Ordering::SeqCst)))
    }

    fn record(owner: &OwnerId) -> RefRecord {
        RefRecord::new(
            owner,
            "f.txt",
            &ResourceId::new("r1"),
            Revision::new(1, b"one\n"),
            ByteRange::new(0, 4),
            digest_stub(),
            true,
        )
    }

    fn digest_stub() -> u64 {
        crate::exp::store::digest64(b"one\n")
    }

    #[test]
    fn opaque_ids_are_short_and_distinct() {
        let first = opaque_id();
        let second = opaque_id();
        assert_eq!(first.len(), 16, "{first}");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
        assert_ne!(first, second);
    }

    #[test]
    fn a_reference_is_bound_to_its_owner_and_generation() {
        let (clock, now) = manual_clock(0);
        let store = RefStore::with_id_source(clock, 8, counter_ids());
        let owner = OwnerId::new("session-a");
        let id = store.issue(record(&owner), 1_000).expect("issue");

        store
            .lookup(&OwnerId::new("session-b"), &id)
            .expect_err("foreign");
        store.lookup(&owner, &id).expect("own");

        now.store(1_000, Ordering::SeqCst);
        let expired = store.lookup(&owner, &id).expect_err("expired");
        assert_eq!(expired.code, ExpErrorCode::ExpiredRef);

        let id = store.issue(record(&owner), 1_000).expect("issue again");
        store.restart();
        let after_restart = store.lookup(&owner, &id).expect_err("restart");
        assert_eq!(after_restart.code, ExpErrorCode::ExpiredRef);
    }

    #[test]
    fn a_full_store_refills_from_expired_entries_and_otherwise_refuses() {
        let (clock, now) = manual_clock(0);
        let store = RefStore::with_id_source(clock, 1, counter_ids());
        let owner = OwnerId::new("session-a");
        store.issue(record(&owner), 100).expect("first");

        let refused = store.issue(record(&owner), 100).expect_err("full");
        assert_eq!(refused.code, ExpErrorCode::BudgetExceeded);

        now.store(100, Ordering::SeqCst);
        store.issue(record(&owner), 100).expect("expired reclaimed");
    }
}
