//! Parity tests for tui autocomplete.ts (pi v0.84.3): prefix extraction,
//! slash-command filtering, file suggestions, scoring, and completion
//! application.

use std::path::PathBuf;

use pillar_tui::autocomplete::{
    AutocompleteItem, CombinedAutocompleteProvider, FileEntry, SlashCommand, apply_completion,
    score_entry,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-ac-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "compact".to_string(),
            description: Some("Manually compact the session context".to_string()),
            argument_hint: None,
        },
        SlashCommand {
            name: "model".to_string(),
            description: Some("Select model".to_string()),
            argument_hint: Some("<provider/model>".to_string()),
        },
        SlashCommand {
            name: "copy".to_string(),
            description: Some("Copy last agent message to clipboard".to_string()),
            argument_hint: None,
        },
    ]
}

fn make_tree() -> PathBuf {
    // Unique per call: parallel tests share the process id.
    let base = temp_dir(&format!(
        "base-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(base.join("src/nested")).unwrap();
    std::fs::write(base.join("src/main.rs"), "").unwrap();
    std::fs::write(base.join("src/nested/deep.rs"), "").unwrap();
    std::fs::write(base.join("readme.md"), "").unwrap();
    base
}

// --- prefix extraction -------------------------------------------------------------------

#[test]
fn extract_at_prefix_variants() {
    assert_eq!(
        CombinedAutocompleteProvider::extract_at_prefix("hello @src/ma"),
        Some("@src/ma".to_string())
    );
    // Quoted @ prefix.
    assert_eq!(
        CombinedAutocompleteProvider::extract_at_prefix("see @\"src/ma"),
        Some("@\"src/ma".to_string())
    );
    // Not at a token start.
    assert_eq!(
        CombinedAutocompleteProvider::extract_at_prefix("email@a"),
        None
    );
    // No @ at all.
    assert_eq!(
        CombinedAutocompleteProvider::extract_at_prefix("plain"),
        None
    );
}

#[test]
fn extract_path_prefix_rules() {
    // Path-like prefixes (with / or .) are extracted naturally.
    assert_eq!(
        CombinedAutocompleteProvider::extract_path_prefix("check src/", false),
        Some("src/".to_string())
    );
    assert_eq!(
        CombinedAutocompleteProvider::extract_path_prefix("./config", false),
        Some("./config".to_string())
    );
    // Plain words do not trigger.
    assert_eq!(
        CombinedAutocompleteProvider::extract_path_prefix("hello world", false),
        None
    );
    // Empty after a space does.
    assert_eq!(
        CombinedAutocompleteProvider::extract_path_prefix("run ", false),
        Some(String::new())
    );
    // Empty without a trailing space does not.
    assert_eq!(
        CombinedAutocompleteProvider::extract_path_prefix("", false),
        None
    );
    // Forced extraction (Tab) always returns the last token.
    assert_eq!(
        CombinedAutocompleteProvider::extract_path_prefix("hello", true),
        Some("hello".to_string())
    );
    // Quoted prefix.
    assert_eq!(
        CombinedAutocompleteProvider::extract_path_prefix("open \"src/fi", false),
        Some("\"src/fi".to_string())
    );
}

// --- slash command suggestions --------------------------------------------------------------

#[test]
fn slash_commands_filtered_and_sorted() {
    let provider = CombinedAutocompleteProvider::commands_only(commands());
    let suggestions = provider
        .get_suggestions(&["/co"], 0, 3, false, &mut |_, _, _| Vec::new())
        .unwrap();
    assert_eq!(suggestions.prefix, "/co");
    let values: Vec<&str> = suggestions.items.iter().map(|i| i.value.as_str()).collect();
    assert_eq!(values, vec!["compact", "copy"]);

    // Full prefix: all commands, description carries the argument hint.
    let suggestions = provider
        .get_suggestions(&["/"], 0, 1, false, &mut |_, _, _| Vec::new())
        .unwrap();
    assert_eq!(suggestions.items.len(), 3);
    let model = suggestions
        .items
        .iter()
        .find(|i| i.value == "model")
        .unwrap();
    assert_eq!(
        model.description.as_deref(),
        Some("<provider/model> — Select model")
    );

    // No matches → None.
    assert!(
        provider
            .get_suggestions(&["/zzz"], 0, 4, false, &mut |_, _, _| Vec::new())
            .is_none()
    );
}

#[test]
fn slash_command_with_arguments_returns_none() {
    // Once a space is present, argument completions are host-driven.
    let provider = CombinedAutocompleteProvider::commands_only(commands());
    assert!(
        provider
            .get_suggestions(&["/model "], 0, 7, false, &mut |_, _, _| Vec::new())
            .is_none()
    );
}

// --- file suggestions ------------------------------------------------------------------------

#[test]
fn file_suggestions_list_directory_contents() {
    let base = make_tree();
    let provider = CombinedAutocompleteProvider::new(commands(), &base, None);
    let suggestions = provider.get_file_suggestions("");
    let labels: Vec<&str> = suggestions.iter().map(|i| i.label.as_str()).collect();
    // Directories first, then files alphabetically.
    assert_eq!(labels, vec!["src/", "readme.md"]);

    // Subdirectory listing via trailing slash.
    let suggestions = provider.get_file_suggestions("src/");
    let labels: Vec<&str> = suggestions.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["nested/", "main.rs"]);

    // Prefix filter.
    let suggestions = provider.get_file_suggestions("read");
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].value, "readme.md");

    // ./ prefix preserved.
    let suggestions = provider.get_file_suggestions("./read");
    assert_eq!(suggestions[0].value, "./readme.md");
}

#[test]
fn file_suggestions_missing_directory_is_empty() {
    let base = make_tree();
    let provider = CombinedAutocompleteProvider::new(commands(), &base, None);
    assert!(provider.get_file_suggestions("nonexistent/").is_empty());
}

// --- scoring --------------------------------------------------------------------------------

#[test]
fn score_entry_ranks_matches() {
    assert_eq!(score_entry("readme.md", "readme.md", false), 100);
    assert_eq!(score_entry("readme.md", "read", false), 80);
    assert_eq!(score_entry("src/main.rs", "main", false), 80);
    assert_eq!(score_entry("src/main.rs", "src", false), 30);
    // Directory bonus.
    assert_eq!(score_entry("src", "src", true), 110);
    // No match.
    assert_eq!(score_entry("src/main.rs", "zzz", false), 0);
}

// --- fuzzy file suggestions -------------------------------------------------------------------

#[test]
fn fuzzy_file_suggestions_rank_and_limit() {
    let base = make_tree();
    let provider = CombinedAutocompleteProvider::new(commands(), &base, Some(&base));
    let suggestions =
        provider.get_fuzzy_file_suggestions("deep", false, &mut |base_dir, query, _depth| {
            // Minimal host walk: report files under the base matching the query
            // name in their path.
            let mut results = Vec::new();
            let base = PathBuf::from(base_dir);
            for entry in walk(&base) {
                let display = entry.to_string_lossy().replace('\\', "/");
                if query.is_empty() || display.to_lowercase().contains(&query.to_lowercase()) {
                    results.push(FileEntry {
                        path: display,
                        is_directory: entry.is_dir(),
                    });
                }
            }
            results
        });
    assert!(!suggestions.is_empty());
    let first = &suggestions[0];
    assert!(first.value.contains("deep"), "{first:?}");
    assert!(first.description.is_some());
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            out.push(path.clone());
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn fuzzy_suggestions_without_fd_are_empty() {
    let base = make_tree();
    let provider = CombinedAutocompleteProvider::new(commands(), &base, None);
    assert!(
        provider
            .get_fuzzy_file_suggestions("deep", false, &mut |_, _, _| Vec::new())
            .is_empty()
    );
}

// --- applyCompletion ----------------------------------------------------------------------------

#[test]
fn apply_slash_command_completion() {
    let lines = vec!["/co".to_string()];
    let item = AutocompleteItem {
        value: "compact".to_string(),
        label: "compact".to_string(),
        description: None,
    };
    let (new_lines, cursor_line, cursor_col) = apply_completion(&lines, 0, 3, &item, "/co");
    assert_eq!(new_lines[0], "/compact ");
    assert_eq!(cursor_line, 0);
    assert_eq!(cursor_col, 9); // after "/" + value + space
}

#[test]
fn apply_at_file_completion() {
    let lines = vec!["see @src/ma and more".to_string()];
    let item = AutocompleteItem {
        value: "@src/main.rs".to_string(),
        label: "main.rs".to_string(),
        description: None,
    };
    let (new_lines, _line, cursor_col) = apply_completion(&lines, 0, 11, &item, "@src/ma");
    // The cursor was mid-line, so the original text after the cursor
    // (" and more") keeps its leading space alongside the suffix.
    assert_eq!(new_lines[0], "see @src/main.rs  and more");
    assert_eq!(cursor_col, 4 + 12 + 1);
}

#[test]
fn apply_at_directory_completion_keeps_continuing() {
    let lines = vec!["@src/ma".to_string()];
    let item = AutocompleteItem {
        value: "@src/nested/".to_string(),
        label: "nested/".to_string(),
        description: None,
    };
    let (new_lines, _line, _cursor) = apply_completion(&lines, 0, 7, &item, "@src/ma");
    // Directories get no trailing space.
    assert_eq!(new_lines[0], "@src/nested/");
}

#[test]
fn apply_path_completion_splices_value() {
    let lines = vec!["check ./read".to_string()];
    let item = AutocompleteItem {
        value: "./readme.md".to_string(),
        label: "readme.md".to_string(),
        description: None,
    };
    let (new_lines, _line, cursor_col) = apply_completion(&lines, 0, 12, &item, "./read");
    assert_eq!(new_lines[0], "check ./readme.md");
    assert_eq!(cursor_col, 17);
}

#[test]
fn apply_completion_with_quoted_prefix_drops_duplicate_quote() {
    let lines = vec!["open \"src/fi".to_string()];
    let item = AutocompleteItem {
        value: "\"src/file.rs\"".to_string(),
        label: "file.rs".to_string(),
        description: None,
    };
    let (new_lines, _line, _cursor) = apply_completion(&lines, 0, 12, &item, "\"src/fi");
    // The trailing quote in the item replaces the one after the cursor.
    assert_eq!(new_lines[0], "open \"src/file.rs\"");
}

// --- Tab trigger -----------------------------------------------------------------------------------

#[test]
fn should_trigger_file_completion_rules() {
    // Slash command at line start without a space: no trigger.
    assert!(!CombinedAutocompleteProvider::should_trigger_file_completion(&["/model"], 0, 6));
    // A slash token with a space in the trimmed text triggers.
    assert!(CombinedAutocompleteProvider::should_trigger_file_completion(&["/model x"], 0, 8));
    // "/model " (trailing space only) still counts as a bare command.
    assert!(!CombinedAutocompleteProvider::should_trigger_file_completion(&["/model "], 0, 7));
    assert!(CombinedAutocompleteProvider::should_trigger_file_completion(&["some text"], 0, 9));
}

#[test]
fn slash_command_argument_prefix_splits_name_and_arguments() {
    use pillar_tui::autocomplete::slash_command_argument_prefix;
    assert_eq!(
        slash_command_argument_prefix("/model op"),
        Some(("model", "op"))
    );
    assert_eq!(
        slash_command_argument_prefix("/model a/b c"),
        Some(("model", "a/b c")),
        "only the first space splits"
    );
    assert_eq!(
        slash_command_argument_prefix("/thinking "),
        Some(("thinking", ""))
    );
    assert_eq!(slash_command_argument_prefix("/model"), None, "no space yet");
    assert_eq!(slash_command_argument_prefix("model x"), None, "not a command");
    assert_eq!(slash_command_argument_prefix("/ x"), None, "empty name");
}
