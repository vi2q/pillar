//! Port of packages/coding-agent/src/core/tools/file-mutation-queue.ts (pi
//! v0.84.3): serialize file mutation operations targeting the same file.
//!
//! divergence: upstream chains per-file promises with realpath keys; the
//! port holds a global mutex plus per-path mutexes (registration ordered by
//! the global lock, execution serialized per resolved path). Missing paths
//! use the lexical resolution as the key (upstream realpath fallback).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The per-path serialization registry (upstream `fileMutationQueues`).
#[derive(Default)]
pub struct FileMutationQueue {
    queues: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
}

impl FileMutationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `f` serialized against other mutations of the same file
    /// (upstream `withFileMutationQueue`). Different files run in parallel.
    pub fn with_file_mutation_queue<T>(
        &self,
        file_path: &Path,
        f: impl FnOnce() -> T,
    ) -> Result<T, String> {
        let key = mutation_queue_key(file_path)?;
        let queue = {
            let mut queues = self.queues.lock().expect("file mutation queues lock");
            queues.entry(key).or_default().clone()
        };
        let _guard = queue.lock().expect("file mutation queue lock");
        Ok(f())
    }
}

/// Resolve the queue key: the real path when the file exists, the lexical
/// resolution otherwise (upstream `getMutationQueueKey`).
fn mutation_queue_key(file_path: &Path) -> Result<PathBuf, String> {
    // The port has no realpath; the lexical resolution serves both cases
    // (upstream falls back to it for ENOENT/ENOTDIR as well).
    Ok(clean_path(file_path))
}

fn clean_path(path: &Path) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::CurDir => {}
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut cleaned = PathBuf::new();
    for part in parts {
        cleaned.push(part);
    }
    cleaned
}
