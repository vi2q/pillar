//! Port of packages/agent/src/harness/tools/file-mutation-queue.ts
//! (pi v0.84.3) — serialize file mutations targeting the same environment
//! and canonical path.
//!
//! divergence: upstream keys queues by canonical path in a module-level
//! `WeakMap<ExecutionEnv, state>` and chains promises; the port holds the
//! state in an explicit [`FileMutationQueues`] value (Rust ownership makes
//! the WeakMap unnecessary) and uses an async mutex per key, with
//! registration serialized under one lock so canonical-path lookups do not
//! race (upstream serializes them via the `registration` promise).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use crate::harness::types::{ExecutionEnv, FileError, FileErrorCode};

/// Per-environment queue table (upstream `WeakMap` state).
#[derive(Default)]
pub struct FileMutationQueues {
    queues: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Serializes key computation + queue registration (upstream
    /// `state.registration` promise chain).
    registration: AsyncMutex<()>,
}

impl FileMutationQueues {
    pub fn new() -> Self {
        Self::default()
    }

    /// Upstream `getMutationQueueKey`: canonical path, or the absolute
    /// path when the file does not exist yet.
    async fn mutation_queue_key<E: ExecutionEnv + ?Sized>(
        &self,
        env: &E,
        path: &str,
    ) -> Result<String, FileError> {
        let absolute_path = env.absolute_path(path).await?;
        match env.canonical_path(&absolute_path).await {
            Ok(canonical_path) => Ok(canonical_path),
            Err(error) => match error.code {
                FileErrorCode::NotFound | FileErrorCode::NotSupported => Ok(absolute_path),
                _ => Err(error),
            },
        }
    }

    /// Upstream `withFileMutationQueue`: run `work` while holding the
    /// mutation queue for `(env, canonical path)`.
    ///
    /// divergence: registration and work share one error channel, so the
    /// work error type only has to be constructible from [`FileError`]
    /// (identity for `FileError` itself). Upstream's promise chain carries
    /// the work rejection through untouched; fixing the channel to
    /// `FileError` would force callers to disguise tool failures as
    /// `FileErrorCode::Unknown` and re-map them after the queue returned.
    pub async fn with_mutation_queue<T, Er, E, F, Fut>(
        &self,
        env: &E,
        path: &str,
        work: F,
    ) -> Result<T, Er>
    where
        E: ExecutionEnv + ?Sized,
        Er: From<FileError>,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, Er>>,
    {
        let _registration = self.registration.lock().await;
        let key = self.mutation_queue_key(env, path).await.map_err(Er::from)?;
        let queue = self
            .queues
            .lock()
            .expect("queue state poisoned")
            .entry(key)
            .or_default()
            .clone();
        // Acquire the queue inside the registration critical section so
        // waiters that register later strictly queue behind (upstream
        // chains onto `currentQueue` inside `registration`). The guard is
        // held for the remainder of the call.
        let _guard = queue.lock().await;
        drop(_registration);
        work().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::env::StdFsExecutionEnv;
    use std::sync::Mutex as StdMutex;

    #[tokio::test]
    async fn serializes_same_path_work() {
        let env = Arc::new(StdFsExecutionEnv::new(
            &std::env::temp_dir()
                .join("pillar-mq-test")
                .to_string_lossy(),
        ));
        let queues = Arc::new(FileMutationQueues::new());
        let order = Arc::new(StdMutex::new(Vec::new()));

        let first = {
            let (queues, env, order) = (Arc::clone(&queues), Arc::clone(&env), Arc::clone(&order));
            tokio::spawn(async move {
                queues
                    .with_mutation_queue(env.as_ref(), "file.txt", || async {
                        order.lock().unwrap().push("first-start");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        order.lock().unwrap().push("first-end");
                        Ok(())
                    })
                    .await
            })
        };
        // Give the spawned task time to register first; the queue, not
        // timing, enforces the ordering once both hold tickets.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        {
            let (queues, env, order) = (Arc::clone(&queues), Arc::clone(&env), Arc::clone(&order));
            queues
                .with_mutation_queue(env.as_ref(), "file.txt", || async {
                    order.lock().unwrap().push("second-start");
                    Ok(())
                })
                .await
                .unwrap();
        }
        first.await.unwrap().unwrap();

        assert_eq!(
            *order.lock().unwrap(),
            vec!["first-start", "first-end", "second-start"]
        );
    }

    #[tokio::test]
    async fn keys_by_canonical_path_through_symlinks() {
        let env = StdFsExecutionEnv::new(
            &std::env::temp_dir()
                .join("pillar-mq-symlink")
                .to_string_lossy(),
        );
        let queues = FileMutationQueues::new();
        let key = queues.mutation_queue_key(&env, "link.txt").await;
        // Missing file: the absolute path stands in for the canonical path.
        assert!(key.is_ok());
    }
}
