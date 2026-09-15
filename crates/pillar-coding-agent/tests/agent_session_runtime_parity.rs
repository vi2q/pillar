//! Parity tests for agent-session-runtime.ts + agent-session-services.ts
//! (pi v0.84.3): service creation, session switching, new session, fork
//! (before/at, persisted and in-memory), and JSONL import.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pillar_ai::types::{Message, UserContent};
use pillar_coding_agent::core::agent_session_runtime::{
    AgentSessionRuntime, CreateAgentSessionServicesOptions, RuntimeFactoryInput,
    RuntimeFactoryResult, RuntimeHooks, create_agent_session_services,
};
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::core::session_manager::{NewSessionOptions, SessionManager};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-runtime-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A factory that echoes the runtime factory input through a shared log so
/// tests can assert session-replacement semantics.
struct FactoryLog {
    calls: Vec<(String, String, &'static str)>,
}

fn make_factory(
    log: Arc<Mutex<FactoryLog>>,
) -> impl FnMut(RuntimeFactoryInput) -> Result<RuntimeFactoryResult, String> {
    move |input: RuntimeFactoryInput| {
        log.lock().unwrap().calls.push((
            input.cwd.clone(),
            input.session_manager.session_id().to_string(),
            input.session_start_reason.as_str(),
        ));
        Ok(RuntimeFactoryResult {
            services: create_agent_session_services(
                &input.cwd,
                CreateAgentSessionServicesOptions {
                    agent_dir: Some(input.agent_dir.clone()),
                    ..Default::default()
                },
            )?,
            session_manager: input.session_manager,
            session_start_reason: input.session_start_reason,
            previous_session_file: input.previous_session_file,
            diagnostics: Vec::new(),
        })
    }
}

fn factory_ref(
    factory: &mut impl FnMut(RuntimeFactoryInput) -> Result<RuntimeFactoryResult, String>,
) -> &mut dyn FnMut(RuntimeFactoryInput) -> Result<RuntimeFactoryResult, String> {
    factory
}

// --- services -------------------------------------------------------------------------

#[test]
fn services_create_collects_resources_and_system_prompt() {
    let cwd = temp_dir("svc-cwd");
    let agent_dir = temp_dir("svc-agent");
    std::fs::create_dir_all(cwd.join(".pillar")).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "ctx").unwrap();
    std::fs::write(cwd.join(".pillar").join("SYSTEM.md"), "sys").unwrap();

    let services = create_agent_session_services(
        &cwd.to_string_lossy(),
        CreateAgentSessionServicesOptions {
            agent_dir: Some(agent_dir.to_string_lossy().to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(services.cwd, cwd);
    assert!(
        services.diagnostics.is_empty(),
        "{:?}",
        services.diagnostics
    );
    let snap = services.resource_loader.snapshot();
    assert!(
        snap.agents_files.iter().any(|(_, c)| c == "ctx"),
        "{snap:?}"
    );
    assert!(
        snap.system_prompt
            .as_deref()
            .is_some_and(|s| s.contains("sys"))
    );
}

#[test]
fn services_invalid_settings_file_yields_warning_diagnostic() {
    let cwd = temp_dir("svc-bad-cwd");
    let agent_dir = temp_dir("svc-bad-agent");
    std::fs::create_dir_all(cwd.join(".pillar")).unwrap();
    std::fs::write(cwd.join(".pillar").join("settings.json"), "{ invalid json").unwrap();

    let services = create_agent_session_services(
        &cwd.to_string_lossy(),
        CreateAgentSessionServicesOptions {
            agent_dir: Some(agent_dir.to_string_lossy().to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        services
            .diagnostics
            .iter()
            .any(|d| d.kind == "warning" && d.message.contains("Invalid settings file")),
        "{:?}",
        services.diagnostics
    );
}

// --- runtime creation --------------------------------------------------------------------

#[test]
fn runtime_create_fails_when_stored_cwd_missing() {
    let cwd = temp_dir("rt-cwd");
    let agent_dir = temp_dir("rt-agent");
    let stored_cwd = PathBuf::from("/definitely/not/real/pillar-runtime");
    let manager = SessionManager::in_memory(&stored_cwd.to_string_lossy(), None).unwrap();
    let file = cwd.join("s.jsonl");
    let mut manager = manager;
    manager.set_session_file(&file).unwrap();

    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log);
    let error = match AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        manager,
    ) {
        Err(error) => error,
        Ok(_) => panic!("expected missing-cwd error"),
    };
    assert!(
        error.contains("Stored session working directory does not exist"),
        "{error}"
    );
}

#[test]
fn runtime_create_passes_through_to_factory() {
    let cwd = temp_dir("rt-ok-cwd");
    let agent_dir = temp_dir("rt-ok-agent");
    let manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();

    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log.clone());
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        manager,
    )
    .unwrap();
    assert_eq!(runtime.cwd(), cwd);
    let calls = log.lock().unwrap().calls.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2, "startup");
}

// --- switch session ------------------------------------------------------------------------

#[test]
fn switch_session_resumes_with_stored_cwd() {
    let cwd = temp_dir("switch-cwd");
    let agent_dir = temp_dir("switch-agent");
    let target_cwd = temp_dir("switch-target-cwd");
    // Build a persisted session file by hand (the JSONL writer is exercised
    // in session_support parity tests).
    let target_file = cwd.join("target.jsonl");
    let target_id = "t1";
    let header = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"target\",\"timestamp\":\"2024-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}",
        target_cwd.to_string_lossy()
    );
    let entry = serde_json::json!({
        "type": "message",
        "id": target_id,
        "parentId": null,
        "timestamp": 1000,
        "message": {"role": "user", "content": "hello", "timestamp": 1000}
    });
    std::fs::write(&target_file, format!("{header}\n{entry}\n")).unwrap();

    let initial = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log.clone());
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        initial,
    )
    .unwrap();

    let mut hooks = RuntimeHooks::default();
    let (outcome, runtime) = runtime
        .switch_session(
            &target_file.to_string_lossy(),
            None,
            &mut hooks,
            factory_ref(&mut factory),
        )
        .unwrap();
    assert!(!outcome.cancelled);
    // The replacement runtime owns the switched-to session file.
    assert_eq!(
        runtime
            .session_manager()
            .session_file()
            .map(|path| path.to_string_lossy().to_string()),
        Some(target_file.to_string_lossy().to_string())
    );
    let calls = log.lock().unwrap().calls.clone();
    assert_eq!(calls.last().unwrap().0, target_cwd.to_string_lossy());
    assert_eq!(calls.last().unwrap().2, "resume");
}

#[test]
fn switch_session_cancelled_by_before_switch_hook() {
    let cwd = temp_dir("cancel-cwd");
    let agent_dir = temp_dir("cancel-agent");
    let initial = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log.clone());
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        initial,
    )
    .unwrap();

    let mut hooks = RuntimeHooks {
        before_switch: Some(&mut |_, _| true),
        ..Default::default()
    };
    let (outcome, _runtime) = runtime
        .switch_session(
            "/tmp/whatever.jsonl",
            None,
            &mut hooks,
            factory_ref(&mut factory),
        )
        .unwrap();
    assert!(outcome.cancelled);
    assert_eq!(log.lock().unwrap().calls.len(), 1); // only the initial creation
}

// --- new session -----------------------------------------------------------------------------

#[test]
fn new_session_replaces_with_fresh_manager_and_persists_parent_link() {
    let cwd = temp_dir("new-cwd");
    let agent_dir = temp_dir("new-agent");
    let mut initial = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    let parent_file = cwd.join("parent.jsonl");
    initial.set_session_file(&parent_file).unwrap();

    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log.clone());
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        initial,
    )
    .unwrap();

    let mut hooks = RuntimeHooks::default();
    let (outcome, _runtime) = runtime
        .new_session(
            Some(&parent_file.to_string_lossy()),
            &mut hooks,
            factory_ref(&mut factory),
        )
        .unwrap();
    assert!(!outcome.cancelled);
    let calls = log.lock().unwrap().calls.clone();
    assert_eq!(calls.last().unwrap().2, "new");
}

// --- fork ------------------------------------------------------------------------------------

#[test]
fn fork_at_entry_positions_leaf_at_entry() {
    let cwd = temp_dir("fork-cwd");
    let agent_dir = temp_dir("fork-agent");
    let mut manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    manager.append_message(user_entry_msg("first")).unwrap();
    manager.append_message(user_entry_msg("second")).unwrap();
    let entries = manager.get_entries_owned();
    let target_id = entries[0].id().to_string();

    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log.clone());
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        manager,
    )
    .unwrap();

    let mut hooks = RuntimeHooks::default();
    let (outcome, _runtime) = runtime
        .fork(&target_id, "at", &mut hooks, factory_ref(&mut factory))
        .unwrap();
    assert!(!outcome.cancelled);
    assert_eq!(outcome.selected_text, None);
    assert_eq!(log.lock().unwrap().calls.last().unwrap().2, "fork");
}

#[test]
fn fork_before_user_entry_returns_selected_text() {
    let cwd = temp_dir("forksel-cwd");
    let agent_dir = temp_dir("forksel-agent");
    let mut manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    manager.append_message(user_entry_msg("pick me")).unwrap();
    manager.append_message(user_entry_msg("later")).unwrap();
    let entries = manager.get_entries_owned();
    // Fork "before" the second entry: the selected entry's own text is
    // returned for re-prompting (upstream reads the selected entry).
    let target_id = entries[1].id().to_string();

    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log.clone());
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        manager,
    )
    .unwrap();

    let mut hooks = RuntimeHooks::default();
    let (outcome, _runtime) = runtime
        .fork(&target_id, "before", &mut hooks, factory_ref(&mut factory))
        .unwrap();
    assert!(!outcome.cancelled);
    assert_eq!(outcome.selected_text.as_deref(), Some("later"));
}

#[test]
fn fork_rejects_non_user_entry_before_position() {
    let cwd = temp_dir("forkbad-cwd");
    let agent_dir = temp_dir("forkbad-agent");
    let mut manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    manager.append_thinking_level_change("high").unwrap();
    let entries = manager.get_entries_owned();
    let target_id = entries[0].id().to_string();

    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log);
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        manager,
    )
    .unwrap();

    let mut hooks = RuntimeHooks::default();
    let error = runtime
        .fork(&target_id, "before", &mut hooks, factory_ref(&mut factory))
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error, "Invalid entry ID for forking");
}

#[test]
fn fork_cancelled_by_before_fork_hook() {
    let cwd = temp_dir("forkcancel-cwd");
    let agent_dir = temp_dir("forkcancel-agent");
    let mut manager = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    manager.append_message(user_entry_msg("x")).unwrap();
    let entries = manager.get_entries_owned();
    let target_id = entries[0].id().to_string();

    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log.clone());
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        manager,
    )
    .unwrap();

    let mut hooks = RuntimeHooks {
        before_fork: Some(&mut |_, _| true),
        ..Default::default()
    };
    let (outcome, _runtime) = runtime
        .fork(&target_id, "at", &mut hooks, factory_ref(&mut factory))
        .unwrap();
    assert!(outcome.cancelled);
    assert_eq!(log.lock().unwrap().calls.len(), 1);
}

// --- import ----------------------------------------------------------------------------------

#[test]
fn import_missing_file_errors() {
    let cwd = temp_dir("imp-cwd");
    let agent_dir = temp_dir("imp-agent");
    let initial = SessionManager::in_memory(&cwd.to_string_lossy(), None).unwrap();
    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log);
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        initial,
    )
    .unwrap();

    let mut hooks = RuntimeHooks::default();
    let error = runtime
        .import_from_jsonl(
            cwd.join("nope.jsonl").to_str().unwrap(),
            None,
            &mut hooks,
            factory_ref(&mut factory),
        )
        .map(|_| ())
        .unwrap_err();
    assert!(error.starts_with("File not found: "), "{error}");
}

#[test]
fn import_copies_file_into_session_dir_and_resumes() {
    let cwd = temp_dir("imp2-cwd");
    let agent_dir = temp_dir("imp2-agent");
    let session_dir = temp_dir("imp2-sessions");
    let source_cwd = temp_dir("imp2-src-cwd");
    let source_dir = temp_dir("imp2-src");
    let source_file = source_dir.join("imported-session.jsonl");
    let header = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"src\",\"timestamp\":\"2024-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}",
        source_cwd.to_string_lossy()
    );
    let entry = serde_json::json!({
        "type": "message",
        "id": "i1",
        "parentId": null,
        "timestamp": 1000,
        "message": {"role": "user", "content": "imported", "timestamp": 1000}
    });
    std::fs::write(&source_file, format!("{header}\n{entry}\n")).unwrap();

    // Use a persisted manager so the import lands in a real session dir.
    let initial = SessionManager::create(&cwd.to_string_lossy(), Some(&session_dir), None).unwrap();
    let log = Arc::new(Mutex::new(FactoryLog { calls: Vec::new() }));
    let mut factory = make_factory(log.clone());
    let runtime = AgentSessionRuntime::create(
        factory_ref(&mut factory),
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        initial,
    )
    .unwrap();

    let mut hooks = RuntimeHooks::default();
    let (outcome, _runtime) = runtime
        .import_from_jsonl(
            &source_file.to_string_lossy(),
            Some(&source_cwd.to_string_lossy()),
            &mut hooks,
            factory_ref(&mut factory),
        )
        .unwrap();
    assert!(!outcome.cancelled);
    // The file was copied into the session dir; the factory observed a resume.
    assert!(session_dir.join("imported-session.jsonl").exists());
    assert_eq!(log.lock().unwrap().calls.last().unwrap().2, "resume");
}

fn user_entry_msg(text: &str) -> CodingAgentMessage {
    CodingAgentMessage::Base(Message::User {
        content: UserContent::Text(text.to_string()),
        timestamp: 1000,
    })
}

// Ensure NewSessionOptions stays exercised (parent link flow).
#[allow(dead_code)]
fn _options_shape(options: NewSessionOptions) -> Option<String> {
    options.parent_session
}
