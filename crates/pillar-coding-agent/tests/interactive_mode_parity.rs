//! Parity tests for modes/interactive/interactive-mode.ts (pi v0.84.3): the
//! pure helpers, including upstream test/format-resume-command.test.ts.

use std::path::{Path, PathBuf};

use pillar_ai::types::{Model, ModelCost};
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::modes::interactive::interactive_mode::{
    ANTHROPIC_SUBSCRIPTION_AUTH_WARNING, create_fuzzy_autocomplete_items,
    format_resume_command_with, has_default_model_provider, is_anthropic_subscription_auth_key,
    is_dead_terminal_error, is_unknown_model, llama_cpp_post_login_guidance, quote_if_needed,
};
use pillar_tui::autocomplete::AutocompleteItem;

fn temp_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pillar-interactive-mode-{}-{id}-{name}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn model(provider: &str, id: &str, api: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: Vec::new(),
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// A persisted manager in the default session dir whose session file exists.
fn default_dir_manager(name: &str) -> (SessionManager, PathBuf) {
    let cwd = temp_dir(name).join("project");
    let _ = std::fs::create_dir_all(&cwd);
    let cwd = cwd.to_string_lossy().to_string();
    let mut manager = SessionManager::create(&cwd, None, None).expect("session manager");
    assert!(manager.uses_default_session_dir());
    let file = manager
        .session_file()
        .expect("session file path")
        .to_path_buf();
    std::fs::write(&file, "\n").expect("create session file");
    let _ = &mut manager;
    (manager, file)
}

#[test]
fn resume_command_for_default_session_dirs() {
    let (manager, _file) = default_dir_manager("resume-default");
    assert_eq!(
        format_resume_command_with(&manager, true),
        Some(format!("pi --session {}", manager.session_id()))
    );
}

#[test]
fn resume_command_includes_custom_session_dirs() {
    let root = temp_dir("resume-custom");
    let cwd = root.join("project");
    let custom = root.join("custom-pi-sessions");
    let _ = std::fs::create_dir_all(&cwd);
    let cwd = cwd.to_string_lossy().to_string();
    let manager = SessionManager::create(&cwd, Some(&custom), None).expect("session manager");
    assert!(!manager.uses_default_session_dir());
    let file = manager
        .session_file()
        .expect("session file path")
        .to_path_buf();
    std::fs::write(&file, "\n").expect("create session file");

    assert_eq!(
        format_resume_command_with(&manager, true),
        Some(format!(
            "pi --session-dir {} --session {}",
            custom.to_string_lossy(),
            manager.session_id()
        ))
    );
}

#[test]
fn resume_command_quotes_session_dirs_with_spaces_and_quotes() {
    let root = temp_dir("resume-quotes");
    let cwd = root.join("project");
    let _ = std::fs::create_dir_all(&cwd);
    let cwd = cwd.to_string_lossy().to_string();

    for (dir_name, expected) in [
        ("custom pi sessions", "'/tmp/custom pi sessions'"),
        ("custom pi's sessions", r"'/tmp/custom pi'\''s sessions'"),
    ] {
        let dir = PathBuf::from("/tmp").join(dir_name);
        let manager = SessionManager::create(&cwd, Some(&dir), None).expect("session manager");
        let file = manager
            .session_file()
            .expect("session file path")
            .to_path_buf();
        std::fs::write(&file, "\n").expect("create session file");
        assert!(!manager.uses_default_session_dir());
        let command = format_resume_command_with(&manager, true).expect("command");
        assert!(
            command.contains(&format!("--session-dir {expected}")),
            "unexpected quoting in {command}"
        );
    }
}

#[test]
fn resume_command_is_absent_without_a_tty_a_persisted_session_or_a_file() {
    let (manager, file) = default_dir_manager("resume-absent");
    // Not a TTY.
    assert_eq!(format_resume_command_with(&manager, false), None);

    // In-memory session.
    let in_memory = SessionManager::in_memory("/tmp", None).expect("in-memory manager");
    assert_eq!(format_resume_command_with(&in_memory, true), None);

    // The session file does not exist yet.
    std::fs::remove_file(&file).expect("remove session file");
    assert_eq!(format_resume_command_with(&manager, true), None);
}

#[test]
fn quoting_only_when_needed() {
    assert_eq!(quote_if_needed("abc"), "abc");
    assert_eq!(
        quote_if_needed("/tmp/a-b_c.D/e:f@g~h"),
        "/tmp/a-b_c.D/e:f@g~h"
    );
    assert_eq!(quote_if_needed("/tmp/a b"), "'/tmp/a b'");
    assert_eq!(quote_if_needed("it's"), r"'it'\''s'");
    assert_eq!(quote_if_needed(""), "''");
}

#[test]
fn dead_terminal_errors_match_the_error_codes() {
    assert!(is_dead_terminal_error(&std::io::Error::from_raw_os_error(
        32 // EPIPE
    )));
    assert!(is_dead_terminal_error(&std::io::Error::from_raw_os_error(
        57 // ENOTCONN
    )));
    assert!(is_dead_terminal_error(&std::io::Error::from_raw_os_error(
        5 // EIO
    )));
    assert!(!is_dead_terminal_error(&std::io::Error::from_raw_os_error(
        2 // ENOENT
    )));
    assert!(is_dead_terminal_error(&std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "closed"
    )));
}

#[test]
fn anthropic_subscription_keys_and_unknown_models() {
    assert!(is_anthropic_subscription_auth_key(Some("sk-ant-oat01-xyz")));
    assert!(!is_anthropic_subscription_auth_key(Some("sk-ant-api03-x")));
    assert!(!is_anthropic_subscription_auth_key(None));

    assert!(is_unknown_model(Some(&model(
        "unknown", "unknown", "unknown"
    ))));
    assert!(!is_unknown_model(Some(&model(
        "anthropic",
        "unknown",
        "unknown"
    ))));
    assert!(!is_unknown_model(Some(&model(
        "anthropic",
        "claude-sonnet-4-5",
        "anthropic-messages"
    ))));
    assert!(!is_unknown_model(None));
    assert!(!ANTHROPIC_SUBSCRIPTION_AUTH_WARNING.contains("sk-ant"));
    assert!(ANTHROPIC_SUBSCRIPTION_AUTH_WARNING.starts_with("Anthropic subscription auth"));
}

#[test]
fn llama_cpp_guidance_depends_on_loaded_models() {
    assert_eq!(
        llama_cpp_post_login_guidance("Logged in", 0),
        "Logged in. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it."
    );
    assert_eq!(
        llama_cpp_post_login_guidance("Logged in", 2),
        "Logged in. Use /model to select a loaded llama.cpp model, or /llama to manage models."
    );
}

#[test]
fn default_provider_lookup_and_fuzzy_autocomplete_items() {
    assert!(has_default_model_provider("anthropic"));
    assert!(!has_default_model_provider("definitely-not-a-provider"));

    #[derive(Clone)]
    struct Entry {
        id: &'static str,
        name: &'static str,
    }
    let items = vec![
        Entry {
            id: "gpt-5",
            name: "GPT-5",
        },
        Entry {
            id: "claude-sonnet-4-5",
            name: "Claude Sonnet 4.5",
        },
    ];
    let to_item = |entry: &Entry| AutocompleteItem {
        value: entry.id.to_string(),
        label: entry.name.to_string(),
        description: None,
    };

    let matched = create_fuzzy_autocomplete_items(&items, "gpt", |e| e.id.to_string(), to_item)
        .expect("matches");
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].value, "gpt-5");

    // No matches -> None (upstream returns null).
    assert!(
        create_fuzzy_autocomplete_items(&items, "zzzz", |e| e.id.to_string(), to_item).is_none()
    );
}

#[test]
fn session_manager_reports_the_default_session_dir() {
    let root = temp_dir("default-dir-predicate");
    let cwd = root.join("project");
    let _ = std::fs::create_dir_all(&cwd);
    let cwd = cwd.to_string_lossy().to_string();
    let default_manager = SessionManager::create(&cwd, None, None).expect("manager");
    assert!(default_manager.uses_default_session_dir());

    let custom_dir: &Path = &root.join("custom");
    let custom_manager =
        SessionManager::create(&cwd, Some(custom_dir), None).expect("custom manager");
    assert!(!custom_manager.uses_default_session_dir());
}
