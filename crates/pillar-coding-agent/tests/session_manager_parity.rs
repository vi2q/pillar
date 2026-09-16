//! Parity tests for session-manager.ts (pi v0.84.3): JSONL round-trip,
//! migrations, tree appends/branching, compaction-aware context building,
//! labels, session names, tree views, and branched-session extraction.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use pillar_ai::types::{Message, StopReason, Usage, UsageCost, UserContent};
use pillar_coding_agent::core::messages::{CodingAgentMessage, CustomContent};
use pillar_coding_agent::core::session_entries::SessionEntry as Entry;
use pillar_coding_agent::core::session_manager::{
    CURRENT_SESSION_VERSION, FileEntry, SessionManager, assert_valid_session_id, build_session_info,
    build_context_entries, build_session_path, default_session_dir_path, generate_id_with,
    get_latest_compaction_entry, load_entries_from_file, load_session_file,
    migrate_session_entries, parse_iso_timestamp, parse_session_entry_line,
    session_entry_to_context_messages,
};

fn user_msg(text: &str) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::User {
        content: UserContent::Text(text.to_string()),
        timestamp: 1000,
    })
}

fn assistant_msg(text: &str) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content: vec![pillar_ai::types::Content::text(text)],
            api: "test-api".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1000,
        },
    )))
}

fn temp_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-session-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir.join(name)
}

// --- session id validation ------------------------------------------------------------

#[test]
fn session_id_validation() {
    for valid in ["a", "ab", "a-b_c.d", "1x"] {
        assert_valid_session_id(valid);
    }
    for invalid in ["", "-abc", "abc-", "a b", "a/b"] {
        let result = std::panic::catch_unwind(|| assert_valid_session_id(invalid));
        assert!(result.is_err(), "expected panic for {invalid:?}");
    }
}

// --- id generation --------------------------------------------------------------------

#[test]
fn generate_id_avoids_collisions_and_is_short() {
    let existing = ["abcdef12".to_string()].into_iter().collect();
    let id = generate_id_with(&existing);
    assert_eq!(id.len(), 8);
    assert_ne!(id, "abcdef12");
}

// --- JSONL parsing / round trip ----------------------------------------------------------

#[test]
fn parse_session_entry_line_round_trip() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    let id1 = manager.append_message(user_msg("hello")).unwrap();
    manager.append_model_change("anthropic", "claude").unwrap();
    let id3 = manager
        .append_custom_entry("ext", Some(serde_json::json!({"a": 1})))
        .unwrap();

    let entries = manager.get_entries();
    assert_eq!(entries.len(), 3);
    let json = pillar_coding_agent::core::session_manager::entry_to_json(entries[0]);
    assert_eq!(json["type"], "message");
    assert_eq!(json["parentId"], serde_json::Value::Null);

    // Round trip through the parser preserves ids, parents, and payloads.
    let line = serde_json::to_string(&json).unwrap();
    match parse_session_entry_line(&line) {
        Some(FileEntry::Entry(Entry::Message(m))) => {
            assert_eq!(m.base.id, id1);
            assert_eq!(m.base.parent_id, None);
        }
        _ => panic!("expected message entry"),
    }
    let _ = id3;
}

#[test]
fn parse_skips_malformed_lines_and_blank_lines() {
    assert!(parse_session_entry_line("").is_none());
    assert!(parse_session_entry_line("  ").is_none());
    assert!(parse_session_entry_line("{not json").is_none());
    assert!(parse_session_entry_line("{\"type\":\"unknown-kind\"}").is_none());
}

#[test]
fn load_entries_from_file_requires_valid_header() {
    let path = temp_file("invalid.jsonl");
    std::fs::write(&path, "garbage line\n{\"type\":\"message\"}\n").unwrap();
    assert!(load_entries_from_file(&path).is_empty());

    // Valid header + entries parse.
    std::fs::write(
        &path,
        format!(
            "{{\"type\":\"session\",\"version\":{},\"id\":\"sess-1\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}}\n{{\"type\":\"model_change\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":1000,\"provider\":\"p\",\"modelId\":\"m\"}}\n",
            CURRENT_SESSION_VERSION
        ),
    )
    .unwrap();
    let entries = load_entries_from_file(&path);
    assert_eq!(entries.len(), 2);
    std::fs::remove_file(&path).ok();
}

// --- migrations ---------------------------------------------------------------------------

#[test]
fn migrate_v1_adds_ids_and_parents() {
    // v1 file: header without version, entries without ids. The parser
    // requires ids, so the v1 entry is constructed directly (upstream's
    // serde shapes make id optional pre-migration).
    let v1_entry = Entry::ModelChange(
        pillar_coding_agent::core::session_entries::ModelChangeEntry {
            base: Default::default(),
            provider: "p".to_string(),
            model_id: "m".to_string(),
        },
    );
    let mut entries: Vec<FileEntry> =
        vec![
        FileEntry::Header(serde_json::from_value(serde_json::json!({
            "type": "session", "id": "s1", "timestamp": "2026-01-01T00:00:00Z", "cwd": "/tmp"
        }))
        .unwrap()),
        FileEntry::Entry(v1_entry),
    ];
    assert!(migrate_session_entries(&mut entries));
    // Header version bumped; entry got an id and null parent.
    match &entries[0] {
        FileEntry::Header(header) => assert_eq!(header.version, Some(CURRENT_SESSION_VERSION)),
        _ => panic!("expected header"),
    }
    let entry = match &entries[1] {
        FileEntry::Entry(entry) => entry,
        _ => panic!("expected entry"),
    };
    assert!(!entry.id().is_empty());
    assert_eq!(entry.parent_id(), None);
}

#[test]
fn migrate_current_version_is_noop() {
    let mut entries: Vec<FileEntry> = vec![FileEntry::Header(
        serde_json::from_value(serde_json::json!({
            "type": "session", "version": CURRENT_SESSION_VERSION, "id": "s1",
            "timestamp": "2026-01-01T00:00:00Z", "cwd": "/tmp"
        }))
        .unwrap(),
    )];
    assert!(!migrate_session_entries(&mut entries));
}

// --- ISO timestamp parsing --------------------------------------------------------------------

#[test]
fn parse_iso_timestamp_matches_expected_epoch() {
    assert_eq!(parse_iso_timestamp("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(
        parse_iso_timestamp("2026-01-02T03:04:05.678Z"),
        Some(1_767_323_045_678)
    );
    assert_eq!(parse_iso_timestamp("not a date"), None);
}

// --- append / tree semantics ----------------------------------------------------------------------

#[test]
fn appends_form_a_linear_tree_and_leaf_advances() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    assert_eq!(manager.get_leaf_id(), None);
    let a = manager.append_message(user_msg("one")).unwrap();
    let b = manager.append_message(assistant_msg("two")).unwrap();
    assert_eq!(manager.get_leaf_id(), Some(b.as_str()));
    let branch = manager.get_branch(None);
    let ids: Vec<&str> = branch.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec![a.as_str(), b.as_str()]);
}

#[test]
fn branch_moves_leaf_without_deleting_history() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    let a = manager.append_message(user_msg("one")).unwrap();
    let b = manager.append_message(assistant_msg("two")).unwrap();
    manager.branch(&a).unwrap();
    assert_eq!(manager.get_leaf_id(), Some(a.as_str()));
    let c = manager.append_message(user_msg("three")).unwrap();
    // All three entries still exist; the new entry is a child of a.
    assert_eq!(manager.get_entries().len(), 3);
    assert_eq!(manager.get_entry(&c).unwrap().parent_id(), Some(a.as_str()));
    let _ = b;
}

#[test]
fn branch_to_unknown_entry_fails() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    assert!(manager.branch("nope").is_err());
}

#[test]
fn append_label_change_requires_existing_target() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    assert!(manager.append_label_change("missing", Some("x")).is_err());
    let a = manager.append_message(user_msg("one")).unwrap();
    let label_id = manager.append_label_change(&a, Some("mark")).unwrap();
    assert_eq!(manager.get_label(&a), Some("mark"));
    // Clearing works and drops the label.
    manager.append_label_change(&a, None).unwrap();
    assert_eq!(manager.get_label(&a), None);
    let _ = label_id;
}

// --- compaction-aware context --------------------------------------------------------------------

#[test]
fn build_context_entries_uses_kept_entries_after_compaction() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    let a = manager.append_message(user_msg("old 1")).unwrap();
    let _b = manager.append_message(assistant_msg("old 2")).unwrap();
    let c = manager.append_message(user_msg("kept 1")).unwrap();
    let d = manager.append_message(assistant_msg("kept 2")).unwrap();
    manager
        .append_compaction("summary", &c, 5000, None, false, None)
        .unwrap();
    let e = manager.append_message(user_msg("after")).unwrap();

    let context = manager.session_context();
    let texts: Vec<String> = context
        .messages
        .iter()
        .filter_map(|m| match m {
            CodingAgentMessage::CompactionSummary(s) => Some(s.summary.clone()),
            CodingAgentMessage::Base(Message::User {
                content: UserContent::Text(text),
                ..
            }) => Some(text.clone()),
            _ => None,
        })
        .collect();
    // Compaction entry first, then kept entries from c onward, then after.
    assert_eq!(texts, vec!["summary", "kept 1", "after"]);
    let _ = (a, d, e);
}

#[test]
fn session_context_tracks_thinking_and_model() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    manager.append_thinking_level_change("high").unwrap();
    manager.append_model_change("anthropic", "claude").unwrap();
    manager.append_message(assistant_msg("reply")).unwrap();
    let context = manager.session_context();
    assert_eq!(context.thinking_level, "high");
    // The latest assistant message also updates the model setting (upstream
    // getSessionContextSettings does the same).
    assert_eq!(context.model, Some(("p".to_string(), "m".to_string())));
}

#[test]
fn latest_compaction_entry_found() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    assert!(get_latest_compaction_entry(&manager.get_entries_owned()).is_none());
    manager
        .append_compaction("s", "x", 1, None, false, None)
        .unwrap();
    let owned = manager.get_entries_owned();
    let latest = get_latest_compaction_entry(&owned).unwrap();
    assert_eq!(latest.summary, "s");
}

// --- session names ----------------------------------------------------------------------------------

#[test]
fn session_name_latest_entry_wins_and_clears() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    assert_eq!(manager.session_name(), None);
    manager.append_session_info("My Session").unwrap();
    assert_eq!(manager.session_name().as_deref(), Some("My Session"));
    manager.append_session_info("Newer").unwrap();
    assert_eq!(manager.session_name().as_deref(), Some("Newer"));
    // Empty name explicitly clears.
    manager.append_session_info("").unwrap();
    assert_eq!(manager.session_name(), None);
}

// --- tree view -----------------------------------------------------------------------------------------

#[test]
fn get_tree_nests_children_and_labels() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    let a = manager.append_message(user_msg("root")).unwrap();
    let b = manager.append_message(assistant_msg("child")).unwrap();
    manager.append_label_change(&b, Some("tagged")).unwrap();
    manager.branch(&a).unwrap();
    let c = manager.append_message(user_msg("second child")).unwrap();
    let _ = c;
    let tree = manager.get_tree();
    assert_eq!(tree.len(), 1, "single root");
    // Two children of the root (b and c; label entry is a child of b).
    assert_eq!(tree[0].children.len(), 2);
    let child_labels: Vec<Option<&str>> = tree[0]
        .children
        .iter()
        .map(|n| n.label.as_deref())
        .collect();
    assert_eq!(child_labels, vec![Some("tagged"), None]);
}

// --- branched session -------------------------------------------------------------------------------------

#[test]
fn create_branched_session_extracts_path() {
    let dir = std::env::temp_dir().join(format!("pillar-branch-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut manager = SessionManager::create("/tmp", Some(&dir), None).unwrap();
    let a = manager.append_message(user_msg("one")).unwrap();
    let b = manager.append_message(assistant_msg("two")).unwrap();
    // Extract the path root->b (contains an assistant message, so the file is
    // written immediately per the deferred-persist contract).
    let new_file = manager.create_branched_session(&b).unwrap().unwrap();
    let loaded = load_entries_from_file(&new_file);
    assert_eq!(loaded.len(), 3, "header + two path entries");
    match &loaded[0] {
        FileEntry::Header(header) => {
            assert_eq!(header.version, Some(CURRENT_SESSION_VERSION));
            assert!(header.parent_session.is_some());
        }
        _ => panic!("expected header"),
    }
    match &loaded[1] {
        FileEntry::Entry(entry) => assert_eq!(entry.id(), a),
        _ => panic!("expected entry"),
    }
    // The branched session manager now points at the new file.
    assert_eq!(manager.session_file(), Some(new_file.as_path()));
    assert_eq!(manager.get_entries().len(), 2);
}

// --- default session dir --------------------------------------------------------------------------------------

#[test]
fn default_session_dir_encodes_cwd() {
    let path = default_session_dir_path("/Users/x/proj");
    let encoded = path.file_name().unwrap().to_string_lossy();
    assert!(
        encoded.starts_with("--") && encoded.ends_with("--"),
        "{encoded}"
    );
    assert!(!encoded.contains('/'));
}

// --- pure context helpers over raw entries ------------------------------------------------------------------------

#[test]
fn build_session_path_walks_parents() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    let a = manager.append_message(user_msg("1")).unwrap();
    let _b = manager.append_message(user_msg("2")).unwrap();
    let entries = manager
        .get_entries()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let by_id = entries.iter().map(|e| (e.id().to_string(), e)).collect();
    let path = build_session_path(&entries, Some(&a), &by_id);
    assert_eq!(path.len(), 1);

    // leaf_id None is upstream's null: no path (not the last entry).
    let path = build_session_path(&entries, None, &by_id);
    assert!(path.is_empty());
}

#[test]
fn session_entry_to_context_messages_for_custom_message() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    manager
        .append_custom_message_entry(
            "ext",
            vec![CustomContent::Text("injected".to_string())],
            true,
            None,
        )
        .unwrap();
    let entries = manager
        .get_entries()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let messages = session_entry_to_context_messages(&entries[0]);
    assert_eq!(messages.len(), 1);
    assert!(matches!(
        &messages[0],
        CodingAgentMessage::Custom(custom) if custom.custom_type == "ext"
    ));
}

#[test]
fn build_context_entries_without_compaction_is_the_full_path() {
    let mut manager = SessionManager::in_memory("/tmp", None).unwrap();
    manager.append_message(user_msg("1")).unwrap();
    manager.append_message(user_msg("2")).unwrap();
    let entries = manager
        .get_entries()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let by_id = entries.iter().map(|e| (e.id().to_string(), e)).collect();
    let context = build_context_entries(&entries, None, &by_id);
    assert!(context.is_empty());
}

// --- deferred file creation (upstream `_persist`) ------------------------------------

/// A fresh persisted session keeps its entries in memory: the file is only
/// created once an assistant message exists (upstream `_persist`).
#[test]
fn session_file_is_created_only_after_an_assistant_message() {
    let dir = std::env::temp_dir().join(format!("pillar-session-deferred-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut manager = SessionManager::create(&dir.to_string_lossy(), Some(&dir), None).unwrap();
    let file = manager
        .session_file()
        .expect("session file path")
        .to_path_buf();
    let _ = std::fs::remove_file(&file);
    assert!(!file.exists(), "a fresh session must not create its file");

    manager.append_thinking_level_change("off").unwrap();
    manager.append_message(user_msg("hi")).unwrap();
    assert!(
        !file.exists(),
        "entries before the first assistant message stay in memory"
    );

    manager.append_message(assistant_msg("hello")).unwrap();
    assert!(
        file.exists(),
        "the first assistant message flushes everything"
    );
    let lines: Vec<String> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 4, "header + thinking + user + assistant");
    assert!(lines[0].contains("\"type\":\"session\""), "{}", lines[0]);
    assert!(lines[1].contains("thinking_level_change"), "{}", lines[1]);
    assert!(lines[2].contains("hi"), "{}", lines[2]);
    assert!(lines[3].contains("hello"), "{}", lines[3]);
}

/// A session whose file has already been flushed appends entries immediately,
/// even while no assistant message is present (upstream `_persist`).
#[test]
fn flushed_session_appends_without_waiting_for_an_assistant() {
    let dir = std::env::temp_dir().join(format!("pillar-session-flushed-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut manager = SessionManager::create(&dir.to_string_lossy(), Some(&dir), None).unwrap();
    let file = manager
        .session_file()
        .expect("session file path")
        .to_path_buf();
    let _ = std::fs::remove_file(&file);
    manager.append_message(assistant_msg("hello")).unwrap();
    let before = std::fs::read_to_string(&file).unwrap().lines().count();

    let mut reopened = SessionManager::open(&file, None, None).unwrap();
    reopened.append_message(user_msg("after")).unwrap();

    let after = std::fs::read_to_string(&file).unwrap();
    assert_eq!(after.lines().count(), before + 1);
    assert!(after.lines().last().unwrap().contains("after"));
}

/// Opening a path that has no file starts a fresh session bound to that path
/// (upstream `SessionManager.open` → `_setSessionFile`); it is not an error.
#[test]
fn opening_a_missing_session_path_starts_a_fresh_session() {
    let dir = std::env::temp_dir().join(format!("pillar-session-missing-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let missing = dir.join("does-not-exist.jsonl");
    let _ = std::fs::remove_file(&missing);

    let manager = SessionManager::open(&missing, None, None).unwrap();
    assert_eq!(manager.session_file(), Some(missing.as_path()));
    assert!(
        !missing.exists(),
        "no file is written until an assistant message"
    );
    assert!(manager.get_entries_owned().is_empty());
}

// --- session listing (upstream buildSessionInfo / list / listAll) ---------------------

/// Like [`user_msg`] / [`assistant_msg`], but with real timestamps: the
/// listing sorts by the last message activity, so fixture entries need
/// distinct, current times.
fn stamp_msg(text: &str, role_user: bool) -> CodingAgentMessage {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    if role_user {
        CodingAgentMessage::Base(Message::User {
            content: UserContent::Text(text.to_string()),
            timestamp,
        })
    } else {
        CodingAgentMessage::Base(Message::Assistant(Box::new(
            pillar_ai::types::AssistantMessage {
                content: vec![pillar_ai::types::Content::text(text)],
                api: "test-api".to_string(),
                provider: "p".to_string(),
                model: "m".to_string(),
                response_model: None,
                usage: Usage {
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    cache_write_1h: None,
                    reasoning: None,
                    total_tokens: 0,
                    cost: UsageCost::default(),
                },
                stop_reason: StopReason::Stop,
                deferred: None,
                error_message: None,
                response_id: None,
                diagnostics: Vec::new(),
                raw_stop_reason: None,
                end_turn: None,
                timestamp,
            },
        )))
    }
}

fn write_session_file(
    dir: &std::path::Path,
    cwd: &str,
    name: Option<&str>,
    first: &str,
) -> PathBuf {
    let mut manager = SessionManager::create(cwd, Some(dir), None).expect("session");
    manager
        .append_message(stamp_msg(first, true))
        .expect("append");
    manager
        .append_message(stamp_msg("reply", false))
        .expect("append");
    if let Some(name) = name {
        manager.append_session_info(name).expect("name");
    }
    manager.session_file().map(Path::to_path_buf).unwrap()
}

#[test]
fn build_session_info_extracts_the_summary_fields() {
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-info-{}-{}",
        std::process::id(),
        chrono_unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_session_file(&dir, "/tmp/proj", Some("my session"), "hello world");

    let info = pillar_coding_agent::core::session_manager::build_session_info(&file)
        .expect("session info");
    assert_eq!(info.name.as_deref(), Some("my session"));
    assert_eq!(info.cwd, "/tmp/proj");
    assert_eq!(info.first_message, "hello world");
    assert_eq!(info.message_count, 2);
    assert!(info.all_messages_text.contains("hello world"));
    assert!(info.all_messages_text.contains("reply"));
    assert!(info.modified_ms > 0, "activity time: {info:?}");
    assert_eq!(
        pillar_coding_agent::core::session_manager::build_session_info(
            std::path::Path::new(&dir).join("missing.jsonl").as_path()
        ),
        None,
        "unreadable files are skipped"
    );
}

fn chrono_unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        + COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[test]
fn list_returns_the_directory_sorted_and_filters_the_cwd() {
    let unique = chrono_unique();
    let dir = std::env::temp_dir().join(format!("pillar-session-list-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();
    write_session_file(&dir, "/tmp", None, "older session");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let newer = write_session_file(&dir, "/tmp", Some("newer"), "newer session");

    let sessions = SessionManager::list("/tmp", Some(&dir), None);
    assert_eq!(sessions.len(), 2, "both files listed: {sessions:?}");
    assert_eq!(
        sessions[0].path,
        newer,
        "newest first: {:?}",
        sessions.iter().map(|s| s.path.clone()).collect::<Vec<_>>()
    );

    // A custom dir filters by header cwd (upstream the same condition).
    let other_dir = std::env::temp_dir().join(format!("pillar-session-list-{unique}-2"));
    std::fs::create_dir_all(&other_dir).unwrap();
    write_session_file(&other_dir, "/elsewhere", None, "elsewhere");
    let sessions = SessionManager::list("/tmp", Some(&other_dir), None);
    assert!(
        sessions.is_empty(),
        "mismatched cwds are filtered: {sessions:?}"
    );
}

#[test]
fn list_all_scans_the_given_directory_and_reports_progress() {
    let unique = chrono_unique();
    let dir = std::env::temp_dir().join(format!("pillar-session-list-all-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();
    write_session_file(&dir, "/tmp", None, "one");
    write_session_file(&dir, "/tmp", Some("two"), "two");

    let loaded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&loaded);
    let progress: pillar_coding_agent::core::session_manager::SessionListProgress =
        Arc::new(move |done, total| sink.lock().unwrap().push((done, total)));
    let sessions = SessionManager::list_all(Some(&dir), Some(&progress));
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert_eq!(loaded.lock().unwrap().len(), 2, "progress per file");
    assert_eq!(
        loaded.lock().unwrap().last().copied(),
        Some((2, 2)),
        "final progress covers every file"
    );

    // A missing directory lists nothing (upstream the same).
    let missing = std::env::temp_dir().join(format!("pillar-session-list-all-{unique}-missing"));
    assert!(SessionManager::list_all(Some(&missing), None).is_empty());
}

// --- durability (docs/ARCHITECTURE-REVIEW-s05c0.md E) ------------------------------------

/// A failed append must not leave the live state ahead of the file: the
/// entry, its index and the leaf all roll back.
#[test]
fn a_failed_append_rolls_the_session_state_back() {
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-rollback-{}-{}",
        std::process::id(),
        chrono_unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_session_file(&dir, "/tmp", None, "hello");
    let mut manager = SessionManager::open(&file, None, None).expect("open");
    let entries_before = manager.get_entries().len();
    let branch_before: Vec<String> = manager
        .get_branch(None)
        .iter()
        .map(|entry| entry.id().to_string())
        .collect();

    let original = std::fs::read(&file).unwrap();
    // Replace the session file with a directory: every write path fails.
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert!(manager.append_message(stamp_msg("later", true)).is_err());
    assert_eq!(manager.get_entries().len(), entries_before);
    assert_eq!(
        manager
            .get_branch(None)
            .iter()
            .map(|entry| entry.id().to_string())
            .collect::<Vec<_>>(),
        branch_before
    );

    // A session file that vanished is reported, not silently recreated (an
    // append would otherwise write a headerless file).
    std::fs::remove_dir(&file).unwrap();
    assert!(manager.append_message(stamp_msg("later", true)).is_err());

    // Restoring the file resumes the session: the next append continues from
    // the rolled-back leaf, leaving no gap.
    std::fs::write(&file, &original).unwrap();
    let id = manager
        .append_message(stamp_msg("later", true))
        .expect("append after the failed one");
    let ids: Vec<String> = manager
        .get_branch(None)
        .iter()
        .map(|entry| entry.id().to_string())
        .collect();
    assert_eq!(ids.last(), Some(&id));
    std::fs::remove_dir_all(&dir).ok();
}

/// A truncated last line (the writer died mid-append) is dropped and the file
/// repaired, so the next append does not splice onto the fragment.
#[test]
fn a_truncated_last_line_is_dropped_and_repaired() {
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-torn-{}-{}",
        std::process::id(),
        chrono_unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_session_file(&dir, "/tmp", None, "hello");
    let mut content = std::fs::read_to_string(&file).unwrap();
    content.push_str("{\"type\":\"message\",\"id\":\"torn\",\"parentId\":");
    std::fs::write(&file, content).unwrap();

    let load = load_session_file(&file);
    assert!(load.torn_tail, "the last line is reported as truncated");
    assert!(load.corrupt.is_empty(), "{:?}", load.corrupt);

    let manager = SessionManager::open(&file, None, None).expect("open repairs the tail");
    assert_eq!(manager.get_entries().len(), load.entries.len() - 1);
    let repaired = std::fs::read_to_string(&file).unwrap();
    assert!(!repaired.contains("torn"));
    assert!(repaired.ends_with('\n'));
    assert!(!load_session_file(&file).torn_tail);
    std::fs::remove_dir_all(&dir).ok();
}

/// A damaged line in the middle is reported instead of being dropped: the
/// session refuses to open and the file stays untouched for manual repair.
#[test]
fn a_damaged_middle_line_is_reported_instead_of_dropped() {
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-corrupt-{}-{}",
        std::process::id(),
        chrono_unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_session_file(&dir, "/tmp", None, "hello");
    let content = std::fs::read_to_string(&file).unwrap();
    let mut lines: Vec<&str> = content.lines().collect();
    lines.insert(1, "{\"type\":\"message\"}");
    std::fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();

    let load = load_session_file(&file);
    assert_eq!(load.corrupt.len(), 1, "{:?}", load.corrupt);
    assert_eq!(load.corrupt[0].0, 2);
    assert!(!load.torn_tail);
    assert_eq!(load.entries.len(), 3, "the readable entries survive");

    let error = match SessionManager::open(&file, None, None) {
        Ok(_) => panic!("corruption must be reported"),
        Err(error) => error,
    };
    assert!(error.contains("corrupt at line(s) 2"), "{error}");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("{\"type\":\"message\"}")
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A pre-migration file (v1: no header version, entries without ids) loads,
/// gets migrated, and keeps its original bytes next to it.
#[test]
fn a_pre_migration_file_is_migrated_and_backed_up() {
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-migrate-{}-{}",
        std::process::id(),
        chrono_unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("v1.jsonl");
    let original = concat!(
        "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"model_change\",\"provider\":\"p\",\"modelId\":\"m\",\"timestamp\":1000}\n"
    );
    std::fs::write(&file, original).unwrap();

    let manager = SessionManager::open(&file, None, None).expect("a v1 file migrates");
    let entries = manager.get_entries();
    assert_eq!(entries.len(), 1, "the id-less entry survived the load");
    assert!(!entries[0].id().is_empty(), "migration assigned an id");
    match &load_session_file(&file).entries[0] {
        FileEntry::Header(header) => assert_eq!(header.version, Some(CURRENT_SESSION_VERSION)),
        _ => panic!("expected header"),
    }
    // The pre-migration bytes stay recoverable.
    let backup = dir.join("v1.jsonl.bak");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
    std::fs::remove_dir_all(&dir).ok();
}

/// The file must describe exactly the live state after every write: a reload
/// sees the same entries, parents and leaf.
#[test]
fn the_file_matches_the_live_state_after_appends() {
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-roundtrip-{}-{}",
        std::process::id(),
        chrono_unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut manager = SessionManager::create("/tmp", Some(&dir), None).unwrap();
    // The first write is deferred until an assistant message (upstream
    // contract); after it the file exists and is complete.
    let user = manager.append_message(user_msg("hello")).unwrap();
    manager.append_message(assistant_msg("reply")).unwrap();
    let custom = manager
        .append_custom_entry("ext", Some(serde_json::json!({ "a": 1 })))
        .unwrap();
    manager.append_label_change(&user, Some("label")).unwrap();
    let file = manager.session_file().map(Path::to_path_buf).unwrap();

    let reloaded = SessionManager::open(&file, None, None).expect("reopen");
    let live: Vec<String> = manager.get_entries().iter().map(|e| e.id().to_string()).collect();
    let stored: Vec<String> = reloaded.get_entries().iter().map(|e| e.id().to_string()).collect();
    assert_eq!(stored, live);
    assert_eq!(reloaded.get_leaf_id(), manager.get_leaf_id());
    assert!(stored.contains(&custom));
    // The label survives as an entry too.
    let labels: Vec<&str> = reloaded
        .get_entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::Label(label) => label.label.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(labels, vec!["label"]);
    std::fs::remove_dir_all(&dir).ok();
}

/// An interrupted rewrite must leave the previous file bytes untouched: the
/// repair that a torn tail triggers fails, and the file still holds the tail
/// instead of half a rewrite.
#[test]
fn an_interrupted_rewrite_keeps_the_previous_file() {
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-rewrite-fail-{}-{}",
        std::process::id(),
        chrono_unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_session_file(&dir, "/tmp", None, "hello");
    let mut content = std::fs::read_to_string(&file).unwrap();
    content.push_str("{\"type\":\"message\",\"id\":\"torn\",");
    std::fs::write(&file, &content).unwrap();
    // Block the rewrite's temp file: `File::create` fails on a directory.
    let temp = dir.join(format!(
        "{}.tmp",
        file.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir(&temp).unwrap();

    let error = match SessionManager::open(&file, None, None) {
        Ok(_) => panic!("the repair must report the failed rewrite"),
        Err(error) => error,
    };
    assert!(error.contains("Failed to write session"), "{error}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), content);
    std::fs::remove_dir_all(&dir).ok();
}

/// A second writer is reported instead of interleaved: the file changed under
/// a manager that already knows what it wrote.
#[test]
fn a_foreign_append_is_reported_not_interleaved() {
    let dir = std::env::temp_dir().join(format!(
        "pillar-session-foreign-{}-{}",
        std::process::id(),
        chrono_unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = write_session_file(&dir, "/tmp", None, "hello");
    let mut manager = SessionManager::open(&file, None, None).expect("open");

    let mut foreign = std::fs::read_to_string(&file).unwrap();
    foreign.push_str(
        "{\"type\":\"session_info\",\"id\":\"other\",\"parentId\":null,\"timestamp\":2000,\"name\":\"other\"}\n",
    );
    std::fs::write(&file, &foreign).unwrap();

    let error = match manager.append_message(stamp_msg("mine", true)) {
        Ok(_) => panic!("a foreign write must be reported"),
        Err(error) => error,
    };
    assert!(error.contains("changed outside this process"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        foreign,
        "nothing of ours is written into the other writer's file"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// --- scale measurement (the corpus the storage decision is based on) ---------

/// Not part of the suite: builds a corpus and prints timings, so a change to
/// listing / loading can be compared against the recorded numbers (TASKS).
/// Run with `cargo test -p pillar-coding-agent --test session_manager_parity
/// measure_session_scale -- --ignored --nocapture`.
#[test]
#[ignore = "measurement: run explicitly, prints timings"]
fn measure_session_scale() {
    use std::time::Instant;

    let dir = std::env::temp_dir().join(format!("pillar-session-scale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 1000 sessions with a handful of entries each (the /resume listing case).
    const SESSIONS: usize = 1000;
    let started = Instant::now();
    let mut names = Vec::new();
    for index in 0..SESSIONS {
        let mut manager =
            SessionManager::create("/tmp/scale", Some(&dir), None).expect("create session");
        manager.append_message(user_msg("question")).unwrap();
        manager.append_message(assistant_msg("answer")).unwrap();
        names.push(manager.session_file().map(Path::to_path_buf).unwrap());
    }
    let created = started.elapsed();

    let started = Instant::now();
    let listed = SessionManager::list_all(Some(&dir), None);
    let listed_ms = started.elapsed();
    assert_eq!(listed.len(), SESSIONS);

    let started = Instant::now();
    let infos: Vec<_> = names.iter().filter_map(|path| build_session_info(path)).collect();
    let info_ms = started.elapsed();
    assert_eq!(infos.len(), SESSIONS);

    // One long session (the reload case): 10k entries.
    let long_dir = std::env::temp_dir().join(format!("pillar-session-long-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&long_dir);
    std::fs::create_dir_all(&long_dir).unwrap();
    let mut long = SessionManager::create("/tmp/long", Some(&long_dir), None).expect("create");
    let started = Instant::now();
    for index in 0..5_000 {
        long.append_message(user_msg(&format!("question {index}"))).unwrap();
        long.append_message(assistant_msg(&format!("answer {index}"))).unwrap();
    }
    let appended = started.elapsed();
    let file = long.session_file().map(Path::to_path_buf).unwrap();
    drop(long);

    let started = Instant::now();
    let reopened = SessionManager::open(&file, None, None).expect("reopen");
    let loaded = started.elapsed();
    assert_eq!(reopened.get_entries().len(), 10_000);

    println!(
        "scale: {SESSIONS} sessions created {created:?}, listed {listed_ms:?}, info {info_ms:?}; \
         10k entries appended {appended:?}, loaded {loaded:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&long_dir).ok();
}
