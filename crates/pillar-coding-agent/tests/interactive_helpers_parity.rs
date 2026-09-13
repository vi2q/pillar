//! Parity tests for the interactive-mode helpers:
//! utils/text.ts BOM handling, interactive/external-editor.ts, and
//! interactive/model-search.ts.

use pillar_coding_agent::modes::interactive::external_editor::{
    ExternalEditorOptions, ExternalEditorResult, edit_in_external_editor,
};
use pillar_coding_agent::modes::interactive::model_search::{
    ModelSearchItem, model_search_text, model_selector_search_text,
};
use pillar_coding_agent::utils::text::{split_bom, strip_bom};

#[test]
fn bom_helpers_match_upstream() {
    assert_eq!(strip_bom("\u{feff}hello"), "hello");
    assert_eq!(strip_bom("hello"), "hello");
    assert_eq!(strip_bom("\u{feff}"), "");

    assert_eq!(split_bom("\u{feff}hi"), ("\u{feff}", "hi"));
    assert_eq!(split_bom("hi"), ("", "hi"));
    assert_eq!(split_bom(""), ("", ""));
}

#[test]
fn external_editor_reads_the_edited_temp_file() {
    // `cat` exits 0 and leaves the file unchanged, so the content round-trips
    // (minus a trailing newline, as upstream strips it).
    let result = edit_in_external_editor(&ExternalEditorOptions {
        command: "cat".to_string(),
        content: "hello from the editor".to_string(),
    });
    assert_eq!(
        result,
        ExternalEditorResult::Complete {
            content: "hello from the editor".to_string()
        }
    );

    // A trailing newline is removed; a BOM is stripped first.
    let result = edit_in_external_editor(&ExternalEditorOptions {
        command: "cat".to_string(),
        content: "\u{feff}with bom\n".to_string(),
    });
    assert_eq!(
        result,
        ExternalEditorResult::Complete {
            content: "with bom".to_string()
        }
    );
}

#[test]
fn external_editor_failures_are_reported() {
    let failed = edit_in_external_editor(&ExternalEditorOptions {
        command: "false".to_string(),
        content: "x".to_string(),
    });
    assert_eq!(failed, ExternalEditorResult::Failed);

    // A command that cannot be spawned fails too.
    let missing = edit_in_external_editor(&ExternalEditorOptions {
        command: "pillar-definitely-not-a-real-editor".to_string(),
        content: "x".to_string(),
    });
    assert_eq!(missing, ExternalEditorResult::Failed);
}

#[test]
fn external_editor_passes_arguments_through() {
    // `command.split(" ")` means extra words are arguments; `sh -c true` runs
    // and `$0`/`$1` receive the split parts, so the appended file path lands in
    // a positional parameter and the exit code is 0.
    let result = edit_in_external_editor(&ExternalEditorOptions {
        command: "sh -c true".to_string(),
        content: "unchanged".to_string(),
    });
    assert_eq!(
        result,
        ExternalEditorResult::Complete {
            content: "unchanged".to_string()
        }
    );
}

#[test]
fn model_search_texts_match_upstream_layout() {
    let with_name = ModelSearchItem {
        id: "gpt-5",
        provider: "openai",
        name: Some("GPT-5"),
    };
    assert_eq!(
        model_search_text(&with_name),
        "gpt-5 openai openai/gpt-5 openai gpt-5 GPT-5"
    );
    assert_eq!(
        model_selector_search_text(&with_name),
        "openai openai/gpt-5 openai gpt-5 GPT-5"
    );

    let without_name = ModelSearchItem {
        id: "gpt-5",
        provider: "openai",
        name: None,
    };
    assert_eq!(
        model_search_text(&without_name),
        "gpt-5 openai openai/gpt-5 openai gpt-5"
    );
    // The selector variant starts with the provider so exact
    // `provider/id` queries outrank proxy-provider ids.
    assert!(
        model_selector_search_text(&without_name).starts_with("openai "),
        "provider must lead the selector search text"
    );
}
