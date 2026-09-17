//! Port of packages/agent/test/harness/session/search.test.ts (pi v0.84.3)
//! — the scanning session search over in-memory sessions and JSONL-backed
//! sessions on disk.
//!
//! divergence: the upstream `"apply" in search` assertion is a TypeScript
//! interface check (the sqlite backend's `apply` writer must not leak onto
//! the scanning search object) and has no Rust equivalent. The upstream
//! JSONL source generator yields storages one at a time; the port's source
//! closure resolves the metadata list on first poll and yields the loaded
//! readables from it.

#![cfg(all(feature = "search", feature = "session-files"))]

use std::sync::Arc;

use futures::{FutureExt, StreamExt};

use pillar_agent::abort::AbortSignal;
use pillar_agent::harness::env::StdFsExecutionEnv;
use pillar_agent::harness::session::jsonl::repo::JsonlSessionRepo;
use pillar_agent::harness::session::jsonl::types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionRepoOptions,
};
use pillar_agent::harness::session::memory::{InMemorySessionStorage, Session};
use pillar_agent::harness::session::types::SessionMetadata;
use pillar_agent::search::{ScanningReadable, ScanningReadableSource};
use pillar_agent::search::{
    ScanningSessionSearch, ScanningSessionSearchHit, SearchError, SessionSearchOptions,
};
use pillar_agent::types::AgentMessage;
use pillar_ai::types::{Content, Message, UserContent};
use std::sync::atomic::{AtomicUsize, Ordering};

fn temp_dir(label: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pillar-agent-search-{}-{}-{}",
        label,
        std::process::id(),
        count
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.to_string_lossy().into_owned()
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::User {
        content: UserContent::Blocks(vec![Content::text(text)]),
        timestamp: 1,
    })
}

fn create_memory_session(id: &str, created_at: u64) -> Session {
    Session::new(Box::new(InMemorySessionStorage::new(SessionMetadata {
        id: id.to_owned(),
        created_at,
        parent_session_id: None,
    })))
}

fn options() -> SessionSearchOptions {
    SessionSearchOptions::default()
}

/// Upstream `collect`: drain the search stream.
async fn collect(
    stream: impl futures::Stream<Item = Result<ScanningSessionSearchHit, SearchError>>,
) -> Vec<Result<ScanningSessionSearchHit, SearchError>> {
    stream.collect::<Vec<_>>().await
}

#[tokio::test]
async fn scans_an_arbitrary_in_memory_projected_source() {
    let root = create_memory_session("root", 1);
    root.append_message(user_message("fix auth flow")).unwrap();
    let other = create_memory_session("other", 2);
    other
        .append_message(user_message("auth in another workspace"))
        .unwrap();
    let search = ScanningSessionSearch::new(ScanningReadableSource::readables(vec![
        Arc::new(root),
        Arc::new(other),
    ]));

    let hits = collect(search.search("auth", options())).await;
    let hits = hits.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].session_id, "root");
    assert_eq!(hits[1].session_id, "other");
    assert!(
        collect(search.search("missing", options()))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn includes_labels_in_memory_scanning_projections() {
    let session = create_memory_session("session", 1);
    let entry_id = session.append_message(user_message("plain body")).unwrap();
    session
        .set_label(&entry_id, Some("important label".to_owned()))
        .unwrap();
    let search =
        ScanningSessionSearch::new(ScanningReadableSource::readables(vec![Arc::new(session)]));

    let hits = collect(search.search("important", options())).await;
    let hits = hits.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "session");
    assert_eq!(hits[0].entry_id, entry_id);
}

#[tokio::test]
async fn honors_entry_type_filters_and_abort_signals() {
    let session = create_memory_session("session", 1);
    let message_entry_id = session
        .append_message(user_message("auth message"))
        .unwrap();
    session
        .append_custom_entry("note", Some(serde_json::json!({ "text": "auth custom" })))
        .unwrap();
    let search =
        ScanningSessionSearch::new(ScanningReadableSource::readables(vec![Arc::new(session)]));

    let hits = collect(search.search(
        "auth",
        SessionSearchOptions {
            entry_types: Some(vec!["message".to_owned()]),
            ..options()
        },
    ))
    .await;
    let hits = hits.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry_id, message_entry_id);

    // An already-aborted signal fails the stream with the abort error
    // (upstream rejects with `AbortError`).
    let signal = AbortSignal::new();
    signal.abort();
    let results = collect(search.search(
        "auth",
        SessionSearchOptions {
            signal: Some(signal),
            ..options()
        },
    ))
    .await;
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0], Err(SearchError::Aborted)));
}

#[tokio::test]
async fn scans_jsonl_sessions_from_disk_through_the_jsonl_scanning_source() {
    let root = temp_dir("jsonl");
    let fs = Arc::new(StdFsExecutionEnv::new(&root));
    let repo = Arc::new(JsonlSessionRepo::new(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: root.clone(),
    }));
    let cwd = format!("{root}/workspace");
    let other_cwd = format!("{root}/other");
    let session = repo
        .create(&JsonlSessionCreateOptions {
            id: Some("jsonl".to_owned()),
            cwd,
            ..JsonlSessionCreateOptions::default()
        })
        .await
        .unwrap();
    let entry_id = session
        .append_message(user_message("jsonl backed auth entry"))
        .unwrap();
    session
        .set_label(&entry_id, Some("disk label".to_owned()))
        .unwrap();
    let other = repo
        .create(&JsonlSessionCreateOptions {
            id: Some("other".to_owned()),
            cwd: other_cwd,
            ..JsonlSessionCreateOptions::default()
        })
        .await
        .unwrap();
    let other_entry_id = other
        .append_message(user_message("jsonl backed auth entry in another cwd"))
        .unwrap();

    let search = ScanningSessionSearch::new(ScanningReadableSource::source_fn(Arc::new(
        move |_text: &str, _options: &SessionSearchOptions| {
            let repo = repo.clone();
            async move {
                let metadata = repo
                    .list_metadata(&JsonlSessionListOptions::default())
                    .await
                    .map_err(|error| SearchError::Storage(error.to_string()))?;
                let mut readables: Vec<Result<Arc<dyn ScanningReadable>, SearchError>> = Vec::new();
                for metadata in metadata {
                    let session = repo
                        .open(&metadata)
                        .await
                        .map_err(|error| SearchError::Storage(error.to_string()))?;
                    readables.push(Ok(Arc::new(session) as Arc<dyn ScanningReadable>));
                }
                Ok::<_, SearchError>(futures::stream::iter(readables).boxed())
            }
            .boxed()
        },
    )));

    let auth_hits = collect(search.search("auth", options())).await;
    let auth_hits = auth_hits
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(auth_hits.len(), 2);
    assert!(
        auth_hits
            .iter()
            .any(|hit| hit.session_id == "jsonl" && hit.entry_id == entry_id)
    );
    assert!(
        auth_hits
            .iter()
            .any(|hit| hit.session_id == "other" && hit.entry_id == other_entry_id)
    );

    let disk_hits = collect(search.search("disk", options())).await;
    let disk_hits = disk_hits
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(disk_hits.len(), 1);
    assert_eq!(disk_hits[0].session_id, "jsonl");
    assert_eq!(disk_hits[0].entry_id, entry_id);

    let _ = std::fs::remove_dir_all(&root);
}
