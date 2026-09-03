//! Parity tests for tools/edit.ts (pi v0.84.3): multi-edit application with
//! BOM stripping and line-ending preservation, the success message,
//! details (diff/patch/firstChangedLine), and queue serialization.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use pillar_ai::abort::AbortSignal;
use pillar_coding_agent::core::tools::edit::{
    edit as edit_file, edit_description, edit_parameters_json,
};
use pillar_coding_agent::core::tools::edit_diff::Edit;
use pillar_coding_agent::core::tools::file_mutation_queue::FileMutationQueue;

fn mk_edit(old_text: &str, new_text: &str) -> Edit {
    Edit {
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-edit-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn description_matches_upstream_wording() {
    assert_eq!(
        edit_description(),
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    );
}

#[test]
fn parameters_schema_matches_upstream_shape() {
    let schema = edit_parameters_json();
    assert_eq!(schema["properties"]["path"]["type"], "string");
    assert_eq!(
        schema["properties"]["edits"]["items"]["properties"]["oldText"]["type"],
        "string"
    );
    assert!(
        schema["properties"]["edits"]["description"]
            .as_str()
            .unwrap()
            .starts_with("One or more targeted replacements.")
    );
    assert_eq!(schema["required"], serde_json::json!(["path", "edits"]));
}

#[test]
fn edit_requires_at_least_one_edit() {
    let dir = temp_dir("empty-edits");
    let path = dir.join("f.txt");
    fs::write(&path, "content").unwrap();
    let queue = FileMutationQueue::new();
    let error = edit_file(path.to_str().unwrap(), &[], "/tmp", None, &queue).unwrap_err();
    assert_eq!(
        error,
        "Edit tool input is invalid. edits must contain at least one replacement."
    );
}

#[test]
fn edit_replaces_text_and_reports_details() {
    let dir = temp_dir("replace");
    let path = dir.join("f.ts");
    fs::write(&path, "const a = 1;\nconst b = 2;\n").unwrap();
    let queue = FileMutationQueue::new();
    let result = edit_file(
        path.to_str().unwrap(),
        &[mk_edit("const b = 2;", "const b = 3;")],
        "/tmp",
        None,
        &queue,
    )
    .unwrap();
    assert_eq!(
        result.text,
        format!(
            "Successfully replaced 1 block(s) in {}.",
            path.to_str().unwrap()
        )
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "const a = 1;\nconst b = 3;\n"
    );
    assert!(result.diff.contains("-2 const b = 2;"), "{}", result.diff);
    assert!(result.diff.contains("+2 const b = 3;"), "{}", result.diff);
    assert_eq!(result.first_changed_line, Some(2));
    assert!(result.patch.contains("---") && result.patch.contains("+++"));
    assert!(result.patch.contains("-const b = 2;"), "{}", result.patch);
    assert!(result.patch.contains("+const b = 3;"), "{}", result.patch);
}

#[test]
fn edit_multiple_disjoint_edits_in_one_call() {
    let dir = temp_dir("multi");
    let path = dir.join("f.txt");
    fs::write(&path, "one\ntwo\nthree\nfour\nfive").unwrap();
    let queue = FileMutationQueue::new();
    let result = edit_file(
        path.to_str().unwrap(),
        &[
            mk_edit("one", "ONE"),
            mk_edit("three", "THREE"),
            mk_edit("five", "FIVE"),
        ],
        "/tmp",
        None,
        &queue,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "ONE\ntwo\nTHREE\nfour\nFIVE"
    );
    assert_eq!(
        result.text,
        format!(
            "Successfully replaced 3 block(s) in {}.",
            path.to_str().unwrap()
        )
    );
}

#[test]
fn edit_strips_bom_before_matching_and_preserves_it() {
    let dir = temp_dir("bom");
    let path = dir.join("f.txt");
    fs::write(&path, "\u{feff}alpha\nbeta\n").unwrap();
    let queue = FileMutationQueue::new();
    // The model's oldText will not include the BOM.
    edit_file(
        path.to_str().unwrap(),
        &[mk_edit("alpha", "ALPHA")],
        "/tmp",
        None,
        &queue,
    )
    .unwrap();
    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.starts_with('\u{feff}'),
        "BOM preserved: {content:?}"
    );
    assert!(content.contains("ALPHA"));
}

#[test]
fn edit_preserves_crlf_line_endings() {
    let dir = temp_dir("crlf");
    let path = dir.join("f.txt");
    fs::write(&path, "one\r\ntwo\r\nthree\r\n").unwrap();
    let queue = FileMutationQueue::new();
    edit_file(
        path.to_str().unwrap(),
        &[mk_edit("two", "TWO")],
        "/tmp",
        None,
        &queue,
    )
    .unwrap();
    let content = fs::read_to_string(&path).unwrap();
    assert_eq!(content, "one\r\nTWO\r\nthree\r\n");
}

#[test]
fn edit_fuzzy_match_strips_trailing_whitespace_and_preserves_other_lines() {
    let dir = temp_dir("fuzzy");
    let path = dir.join("f.txt");
    fs::write(&path, "keep me   \nchange me  \nalso keep \t").unwrap();
    let queue = FileMutationQueue::new();
    edit_file(
        path.to_str().unwrap(),
        &[mk_edit("change me", "CHANGED")],
        "/tmp",
        None,
        &queue,
    )
    .unwrap();
    // Unchanged lines keep their original bytes; the touched line keeps its
    // trailing whitespace beyond the matched segment.
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "keep me   \nCHANGED  \nalso keep \t"
    );
}

#[test]
fn edit_missing_file_errors_with_upstream_wording() {
    let queue = FileMutationQueue::new();
    let error = edit_file(
        "/definitely/not/real.ts",
        &[mk_edit("a", "b")],
        "/tmp",
        None,
        &queue,
    )
    .unwrap_err();
    assert_eq!(
        error,
        "Could not edit file: /definitely/not/real.ts. Error code: ENOENT."
    );
}

#[test]
fn edit_aborts_before_touching_the_filesystem() {
    let dir = temp_dir("abort");
    let path = dir.join("f.txt");
    fs::write(&path, "content").unwrap();
    let signal = AbortSignal::new();
    signal.abort(None);
    let queue = FileMutationQueue::new();
    let error = edit_file(
        path.to_str().unwrap(),
        &[mk_edit("content", "new")],
        "/tmp",
        Some(&signal),
        &queue,
    )
    .unwrap_err();
    assert_eq!(error, "Operation aborted");
    assert_eq!(fs::read_to_string(&path).unwrap(), "content");
}

#[test]
fn edit_serializes_same_file_mutations() {
    let dir = temp_dir("serialized");
    let path = dir.join("shared.txt");
    fs::write(&path, "aaa bbb ccc ddd eee").unwrap();
    let queue = Arc::new(FileMutationQueue::new());

    let mut handles = Vec::new();
    for (old, new) in [("aaa", "AAA"), ("bbb", "BBB"), ("ccc", "CCC")] {
        let queue = queue.clone();
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            edit_file(
                path.to_str().unwrap(),
                &[mk_edit(old, new)],
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
    // All three disjoint edits land (serialized by the queue).
    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("AAA") && content.contains("BBB") && content.contains("CCC"),
        "{content}"
    );
}
