//! The host boundary: resource identity, snapshots, and atomic conditional
//! publication (design §2, §3, §4.3).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use super::error::ExpError;

/// 64-bit FNV-1a.
///
/// Identity hint only. The design (§3) is explicit that a digest match is
/// neither proof of unchanged history nor proof of permission, and that the
/// host computes digests so the model never has to. A 64-bit
/// non-cryptographic digest is enough for a reference's range marker and for
/// reporting a revision, and it keeps this core dependency-free; the store's
/// revision comparison is the actual gate.
pub fn digest64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// What a host can promise about publication (design §4.3).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Guarantee {
    /// The host can inspect an expected revision and publish atomically, so a
    /// strict edit is possible.
    Strict,
    /// A plain filesystem: a compare followed by a rename races an external
    /// writer, so strict editing is refused and a weak mode would need an
    /// explicit opt-in.
    Weak,
}

/// A host-owned resource identity.
///
/// A display path resolves to this; the path string alone is never the basis
/// for identity or authorization (design §3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceId(String);

impl ResourceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The version of a resource.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    /// Bumped by the host on every publication. It is what distinguishes an
    /// ABA write (content changed and changed back) from an unchanged
    /// resource.
    pub generation: u64,
    /// Digest of the content at that generation, for identity and reporting.
    pub digest: u64,
}

impl Revision {
    pub fn new(generation: u64, bytes: &[u8]) -> Self {
        Self {
            generation,
            digest: digest64(bytes),
        }
    }
}

/// Content at a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub resource: ResourceId,
    pub revision: Revision,
    pub bytes: Vec<u8>,
}

/// The platform boundary of the experiment: identity, snapshots, and atomic
/// conditional publication.
///
/// Every method may re-check authorization: a host that revoked access must
/// answer [`ExpErrorCode::PermissionDenied`](super::ExpErrorCode::PermissionDenied)
/// even for a reference that was valid when it was issued (design §3, §7.2).
#[async_trait]
pub trait ConditionalStore: Send + Sync {
    fn guarantee(&self) -> Guarantee;

    /// The resource a display path currently names.
    async fn resolve(&self, path: &str) -> Result<ResourceId, ExpError>;

    /// The current bytes and revision of a resource.
    async fn snapshot(&self, resource: &ResourceId) -> Result<Snapshot, ExpError>;

    /// Publish `new_bytes` only while the resource is still at `expected`,
    /// and return the new revision. `expected.generation` is compared, so an
    /// ABA write conflicts too.
    async fn compare_and_swap(
        &self,
        resource: &ResourceId,
        expected: Revision,
        new_bytes: &[u8],
    ) -> Result<Revision, ExpError>;
}

/// An in-memory host with a strict guarantee: snapshots are byte copies and
/// publication is a compare-and-swap under one lock.
///
/// This is stage A's host (design §11). It is also the fixture the gate tests
/// drive, with explicit hooks for the cases the gates name: a path being
/// replaced by a different resource, and an external writer rewriting the
/// resource in place.
pub struct MemoryHost {
    files: Mutex<HashMap<String, Entry>>,
    next_resource: AtomicU64,
    generation: AtomicU64,
}

struct Entry {
    resource: ResourceId,
    revision: Revision,
    bytes: Vec<u8>,
}

impl Default for MemoryHost {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryHost {
    pub fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            next_resource: AtomicU64::new(1),
            generation: AtomicU64::new(0),
        }
    }

    /// Create the path as a **new resource** (a create, or a replacement of
    /// whatever the path named). A reference to the previous resource no
    /// longer matches this path.
    pub fn set_file(&self, path: &str, bytes: &[u8]) {
        let resource = ResourceId::new(format!(
            "r{}",
            self.next_resource.fetch_add(1, Ordering::SeqCst)
        ));
        let revision = Revision::new(self.bump_generation(), bytes);
        self.files.lock().expect("memory host lock").insert(
            normalize(path),
            Entry {
                resource,
                revision,
                bytes: bytes.to_vec(),
            },
        );
    }

    /// An external writer changes the resource **in place**: same identity,
    /// new generation. This is what makes a stale reference conflict, and an
    /// ABA write (same bytes again) conflict as well.
    pub fn rewrite_file(&self, path: &str, bytes: &[u8]) {
        let mut files = self.files.lock().expect("memory host lock");
        let generation = self.bump_generation();
        if let Some(entry) = files.get_mut(&normalize(path)) {
            entry.revision = Revision::new(generation, bytes);
            entry.bytes = bytes.to_vec();
        }
    }

    /// The host's current content, for the tests' before/after assertions.
    pub fn content(&self, path: &str) -> Option<Vec<u8>> {
        self.files
            .lock()
            .expect("memory host lock")
            .get(&normalize(path))
            .map(|entry| entry.bytes.clone())
    }

    fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }
}

#[async_trait]
impl ConditionalStore for MemoryHost {
    fn guarantee(&self) -> Guarantee {
        Guarantee::Strict
    }

    async fn resolve(&self, path: &str) -> Result<ResourceId, ExpError> {
        self.files
            .lock()
            .expect("memory host lock")
            .get(&normalize(path))
            .map(|entry| entry.resource.clone())
            .ok_or_else(|| {
                ExpError::new(
                    super::ExpErrorCode::InvalidRequest,
                    format!("no such file: {path}"),
                )
            })
    }

    async fn snapshot(&self, resource: &ResourceId) -> Result<Snapshot, ExpError> {
        let files = self.files.lock().expect("memory host lock");
        let entry = files
            .values()
            .find(|entry| &entry.resource == resource)
            .ok_or_else(ExpError::invalid_ref)?;
        Ok(Snapshot {
            resource: entry.resource.clone(),
            revision: entry.revision,
            bytes: entry.bytes.clone(),
        })
    }

    async fn compare_and_swap(
        &self,
        resource: &ResourceId,
        expected: Revision,
        new_bytes: &[u8],
    ) -> Result<Revision, ExpError> {
        let mut files = self.files.lock().expect("memory host lock");
        let entry = files
            .values_mut()
            .find(|entry| &entry.resource == resource)
            .ok_or_else(ExpError::invalid_ref)?;
        if entry.revision.generation != expected.generation {
            return Err(ExpError::revision_conflict());
        }
        let revision = Revision::new(self.bump_generation(), new_bytes);
        entry.revision = revision;
        entry.bytes = new_bytes.to_vec();
        Ok(revision)
    }
}

/// Path normalization for the in-memory host: leading `./` and repeated
/// separators are the same path. A real adapter supplies its own identity.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_across_calls() {
        assert_eq!(digest64(b"hello"), digest64(b"hello"));
        assert_ne!(digest64(b"hello"), digest64(b"hellp"));
    }

    #[tokio::test]
    async fn compare_and_swap_conflicts_on_a_generation_change() {
        let host = MemoryHost::new();
        host.set_file("f.txt", b"one\n");
        let first = host.resolve("f.txt").await.expect("resolve");
        let snapshot = host.snapshot(&first).await.expect("snapshot");

        // An ABA write: the content is unchanged in the end, the generation
        // is not.
        host.rewrite_file("f.txt", b"one\n");
        let conflict = host
            .compare_and_swap(&first, snapshot.revision, b"two\n")
            .await
            .expect_err("conflict");
        assert_eq!(conflict.code, super::super::ExpErrorCode::RevisionConflict);
        assert_eq!(host.content("f.txt").as_deref(), Some(&b"one\n"[..]));
    }
}
