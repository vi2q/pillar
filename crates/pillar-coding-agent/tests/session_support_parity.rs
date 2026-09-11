//! Parity tests for small pi v0.84.3 core modules: defaults.ts,
//! session-export.ts, slash-commands.ts, session-cwd.ts, and
//! settings-diagnostics.ts.

use std::path::PathBuf;

use serde_json::json;

use pillar_ai::types::{Message, UserContent};
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::session_support::{
    BUILTIN_SLASH_COMMANDS, DEFAULT_THINKING_LEVEL, THINKING_LEVEL_OPTIONS,
    assert_session_cwd_exists, collect_settings_diagnostics, deduplicate_diagnostics,
    export_session_to_jsonl, format_epoch_millis, format_missing_session_cwd_error,
    format_missing_session_cwd_prompt, get_missing_session_cwd_issue,
};
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};

fn temp_dir(name: &str) -> PathBuf {
    // Per-call unique directory: parallel tests in one process must never
    // share a pid-derived path (race: one test removes it while another
    // creates/uses it).
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("pillar-sessup-{}-{id}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// --- defaults -----------------------------------------------------------------------

#[test]
fn thinking_level_defaults() {
    assert_eq!(DEFAULT_THINKING_LEVEL, "medium");
    assert_eq!(
        THINKING_LEVEL_OPTIONS,
        ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
    );
}

// --- session export -------------------------------------------------------------------

#[test]
fn export_session_writes_header_and_chained_parent_ids() {
    let cwd = temp_dir("export-cwd");
    let _agent_dir = temp_dir("unused");
    let mut manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    manager
        .append_message(CodingAgentMessage::Base(Message::User {
            content: UserContent::Text("hello".to_string()),
            timestamp: 1000,
        }))
        .unwrap();

    let out = temp_dir("export-out");
    let path = export_session_to_jsonl(
        &manager,
        Some(out.join("s.jsonl").to_str().unwrap()),
        &cwd.to_string_lossy(),
        None,
    )
    .unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.trim_end().split('\n').collect();
    // Header line is first.
    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(header["type"], "session");
    assert_eq!(header["id"], manager.session_id());
    assert_eq!(header["cwd"], cwd.to_string_lossy().to_string());
    assert!(header["version"].is_u64());

    // Entry lines chain parentId: first null, then the previous id.
    if lines.len() > 1 {
        let first: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(first["parentId"].is_null());
        for pair in lines[1..].windows(2) {
            let prev: serde_json::Value = serde_json::from_str(pair[0]).unwrap();
            let next: serde_json::Value = serde_json::from_str(pair[1]).unwrap();
            assert_eq!(next["parentId"], prev["id"]);
        }
    }
}

#[test]
fn export_session_appends_trailing_entries() {
    let cwd = temp_dir("trail-cwd");
    let _agent_dir = temp_dir("unused");
    let manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    let out = temp_dir("trail-out");
    let path = export_session_to_jsonl(
        &manager,
        Some(out.join("s.jsonl").to_str().unwrap()),
        &cwd.to_string_lossy(),
        Some(&|parent_id, timestamp| {
            vec![json!({
                "type": "custom",
                "id": "trailing-1",
                "parentId": parent_id,
                "timestamp": timestamp,
                "customType": "note",
                "data": "value"
            })]
        }),
    )
    .unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("\"trailing-1\""), "{content}");
    assert!(content.contains("\"customType\":\"note\""), "{content}");
}

#[test]
fn export_session_creates_missing_directories() {
    let cwd = temp_dir("mkdir-cwd");
    let _agent_dir = temp_dir("unused");
    let manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();

    let out = temp_dir("mkdir-out");
    let target = out.join("a").join("b").join("s.jsonl");
    let path = export_session_to_jsonl(
        &manager,
        Some(target.to_str().unwrap()),
        &cwd.to_string_lossy(),
        None,
    )
    .unwrap();
    assert_eq!(path, target);
    assert!(path.exists());
}

#[test]
fn epoch_millis_formats_like_js_to_iso_string() {
    // 2024-01-01T00:00:00.000Z == 1704067200000
    assert_eq!(
        format_epoch_millis(1_704_067_200_000),
        "2024-01-01T00:00:00.000Z"
    );
    // Leap-year day: 2024-02-29T12:34:56.789Z == 1709210096789
    assert_eq!(
        format_epoch_millis(1_709_210_096_789),
        "2024-02-29T12:34:56.789Z"
    );
    // Epoch itself.
    assert_eq!(format_epoch_millis(0), "1970-01-01T00:00:00.000Z");
}

// --- slash commands -------------------------------------------------------------------

#[test]
fn builtin_slash_commands_match_upstream_list() {
    let names: Vec<&str> = BUILTIN_SLASH_COMMANDS.iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        [
            "settings",
            "model",
            "tree",
            "thinking",
            "scoped-models",
            "export",
            "import",
            "share",
            "copy",
            "name",
            "session",
            "changelog",
            "hotkeys",
            "fork",
            "clone",
            "trust",
            "login",
            "logout",
            "new",
            "compact",
            "resume",
            "reload"
        ]
    );

    // argumentHint entries.
    let model = BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|c| c.name == "model")
        .unwrap();
    assert_eq!(model.argument_hint, Some("<provider/model>"));
    let thinking = BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|c| c.name == "thinking")
        .unwrap();
    assert_eq!(thinking.argument_hint, Some("<level>"));
    let login = BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|c| c.name == "login")
        .unwrap();
    assert_eq!(login.argument_hint, Some("<provider>"));
    let settings = BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|c| c.name == "settings")
        .unwrap();
    assert_eq!(settings.argument_hint, None);
    assert_eq!(settings.description, "Open settings menu");
}

// --- session cwd -----------------------------------------------------------------------

#[test]
fn missing_session_cwd_detected_and_formatted() {
    // A stored cwd that was never created (never exists).
    let stored_cwd = PathBuf::from("/definitely/not/a/real/dir/pillar-sessup");
    let mut manager = SessionManager::in_memory(&stored_cwd.to_string_lossy(), None).unwrap();
    let file_path = temp_dir("scwd-store").join("s.jsonl");
    manager.set_session_file(&file_path).unwrap();

    let issue = get_missing_session_cwd_issue(&manager, "/current/dir");
    if stored_cwd.exists() {
        assert!(issue.is_none());
    } else {
        let issue = issue.unwrap();
        assert_eq!(issue.session_cwd, stored_cwd.to_string_lossy());
        assert_eq!(issue.fallback_cwd, "/current/dir");

        let error = format_missing_session_cwd_error(&issue);
        assert!(
            error.starts_with(&format!(
                "Stored session working directory does not exist: {}",
                stored_cwd.display()
            )),
            "{error}"
        );
        assert!(
            error.contains(&format!("Session file: {}", file_path.display())),
            "{error}"
        );
        assert!(
            error.ends_with("Current working directory: /current/dir"),
            "{error}"
        );

        let prompt = format_missing_session_cwd_prompt(&issue);
        assert!(
            prompt.starts_with("cwd from session file does not exist\n"),
            "{prompt}"
        );
        assert!(prompt.contains("continue in current cwd\n"), "{prompt}");

        assert!(assert_session_cwd_exists(&manager, "/current/dir").is_err());
    }
}

#[test]
fn existing_session_cwd_has_no_issue() {
    let cwd = temp_dir("scwd-ok");
    let _agent_dir = temp_dir("unused");
    let mut manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    let file_path = cwd.join("s.jsonl");
    manager.set_session_file(&file_path).unwrap();

    assert!(get_missing_session_cwd_issue(&manager, "/other").is_none());
    assert!(assert_session_cwd_exists(&manager, "/other").is_ok());
}

// --- settings diagnostics ---------------------------------------------------------------

#[test]
fn settings_diagnostics_collect_and_dedupe() {
    let cwd = temp_dir("diag-cwd");
    let agent_dir = temp_dir("diag-agent");
    let options = SettingsManagerCreateOptions {
        project_trusted: Some(true),
    };
    let mut settings = SettingsManager::create(&cwd.to_string_lossy(), &agent_dir, options);

    let diagnostics = collect_settings_diagnostics(
        &mut pillar_coding_agent::core::session_support::SessionSettingsSource(&mut settings),
    );
    // A pristine in-memory pair has no errors.
    assert!(diagnostics.is_empty());

    // Dedupe preserves first occurrence by type+message.
    let deduped = deduplicate_diagnostics(vec![
        pillar_coding_agent::core::session_support::AgentSessionRuntimeDiagnostic {
            diagnostic_type: "warning",
            message: "bad file".to_string(),
        },
        pillar_coding_agent::core::session_support::AgentSessionRuntimeDiagnostic {
            diagnostic_type: "warning",
            message: "bad file".to_string(),
        },
        pillar_coding_agent::core::session_support::AgentSessionRuntimeDiagnostic {
            diagnostic_type: "warning",
            message: "other".to_string(),
        },
    ]);
    assert_eq!(deduped.len(), 2);
    assert_eq!(deduped[0].message, "bad file");
    assert_eq!(deduped[1].message, "other");
}
