//! Parity tests for tools/write.ts (pi v0.84.3): create-or-overwrite with
//! parent directory creation, the success message shape, mutation-queue
//! serialization, and abort semantics.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use pillar_agent::abort::AbortSignal;
use pillar_coding_agent::core::tools::file_mutation_queue::FileMutationQueue;
use pillar_coding_agent::core::tools::write::{write, write_description, write_parameters_json};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-write-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn description_matches_upstream_wording() {
    assert_eq!(
        write_description(),
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    );
}

#[test]
fn parameters_schema_matches_upstream_shape() {
    let schema = write_parameters_json();
    assert_eq!(schema["properties"]["path"]["type"], "string");
    assert_eq!(
        schema["properties"]["content"]["description"],
        "Content to write to the file"
    );
    assert_eq!(schema["required"], serde_json::json!(["path", "content"]));
}

#[test]
fn write_creates_file_and_reports_byte_count() {
    let dir = temp_dir("create");
    let queue = FileMutationQueue::new();
    let result = write(
        dir.join("new.txt").to_str().unwrap(),
        "hello world",
        "/tmp",
        None,
        &queue,
    )
    .unwrap();
    // Upstream formats "Successfully wrote N bytes to <raw path arg>".
    assert!(
        result.text.starts_with("Successfully wrote 11 bytes to ")
            && result.text.ends_with("new.txt"),
        "{}",
        result.text
    );
    assert_eq!(
        fs::read_to_string(dir.join("new.txt")).unwrap(),
        "hello world"
    );
}

#[test]
fn write_overwrites_existing_file() {
    let dir = temp_dir("overwrite");
    fs::write(dir.join("f.txt"), "old content").unwrap();
    let queue = FileMutationQueue::new();
    write(
        dir.join("f.txt").to_str().unwrap(),
        "new content",
        "/tmp",
        None,
        &queue,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.join("f.txt")).unwrap(),
        "new content"
    );
}

#[test]
fn write_creates_parent_directories() {
    let dir = temp_dir("parents");
    let queue = FileMutationQueue::new();
    let target = dir.join("a").join("b").join("c.txt");
    write(target.to_str().unwrap(), "deep", "/tmp", None, &queue).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "deep");
}

#[test]
fn write_resolves_relative_paths_against_cwd() {
    let dir = temp_dir("relative");
    let queue = FileMutationQueue::new();
    write(
        "nested/f.txt",
        "content",
        dir.to_str().unwrap(),
        None,
        &queue,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dir.join("nested").join("f.txt")).unwrap(),
        "content"
    );
}

#[test]
fn write_aborts_before_touching_the_filesystem() {
    let dir = temp_dir("abort");
    let signal = AbortSignal::new();
    signal.abort();
    let queue = FileMutationQueue::new();
    let error = write(
        dir.join("f.txt").to_str().unwrap(),
        "content",
        "/tmp",
        Some(&signal),
        &queue,
    )
    .unwrap_err();
    assert_eq!(error, "Operation aborted");
    assert!(!dir.join("f.txt").exists());
}

#[test]
fn write_same_file_mutations_never_tear() {
    let dir = temp_dir("serialized");
    let queue = Arc::new(FileMutationQueue::new());
    let target = dir.join("shared.txt");

    let mut handles = Vec::new();
    for i in 0..8 {
        let queue = queue.clone();
        let target = target.clone();
        handles.push(std::thread::spawn(move || {
            write(
                target.to_str().unwrap(),
                &format!("content-{i}"),
                "/tmp",
                None,
                &queue,
            )
            .unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    // The queue serializes the writes: the final content is exactly one of
    // the complete payloads, never a torn mix.
    let content = fs::read_to_string(&target).unwrap();
    let complete: Vec<String> = (0..8).map(|i| format!("content-{i}")).collect();
    assert!(
        complete.iter().any(|c| c == &content),
        "torn write: {content}"
    );
}
