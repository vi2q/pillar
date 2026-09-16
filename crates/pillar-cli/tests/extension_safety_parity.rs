//! Regression tests for the safety findings of
//! docs/ARCHITECTURE-REVIEW-s05c0.md (A/B/C/0): an untrusted project's
//! extensions are never evaluated, a handler that blocks stops the action
//! even when it gives no reason, one registration dispatches exactly once, a
//! `/reload` swaps the whole extension generation (command handler, tools,
//! host snapshot) instead of running the old VM, and every extension effect
//! goes through the one authorizer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pillar_agent::{Agent, AgentOptions, AgentState, StreamFn};
use pillar_ai::types::{AssistantMessage, StopReason, Usage};
use pillar_cli::effects::EffectBroker;
use pillar_cli::runner::{
    ExtensionCommandSlot, ExtensionHostSlots, SessionSlot, build_extension_runner,
    build_extension_runner_with_slots,
};
use pillar_cli::trust::{project_extension_dir, resolve_project_trust, stored_project_trust};
use pillar_coding_agent::core::agent_session_class::{
    AgentSession, AgentSessionConfig, ExtensionRunnerFactory,
};
use pillar_coding_agent::core::effects::{EffectDecision, EffectIntent};
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pillar_coding_agent::core::resource_loader::{ResourceLoader, ResourceLoaderOptions};
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use pillar_extensions::bridge::bridge_to_runner;
use pillar_extensions::runtime::ExtensionRuntime;

fn temp_dir(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pillar-cli-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write one extension file into a fresh directory and return the directory.
fn extension_dir(name: &str, file: &str, source: &str) -> std::path::PathBuf {
    let dir = temp_dir(name);
    std::fs::write(dir.join(file), source).unwrap();
    dir
}

/// The extension writes a marker file while its setup runs, so the marker
/// proves evaluation: an untrusted project must leave it absent.
#[test]
fn untrusted_project_extensions_are_not_evaluated() {
    let root = temp_dir("trust-gate");
    let project = root.join("project");
    let agent = root.join("agent");
    let extensions = project.join(".pillar").join("extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        extensions.join("probe.luau"),
        r#"
        local pillar = require("@pillar")
        pillar.fs.write("marker.txt", "executed")
        return nil
        "#,
    )
    .unwrap();
    let cwd = project.to_string_lossy().to_string();
    let agent_dir = agent.to_string_lossy().to_string();
    let marker = project.join("marker.txt");

    // No stored decision and no interactive prompt (print / json / rpc): deny.
    assert!(!stored_project_trust(&cwd, &agent_dir, None));
    assert!(!resolve_project_trust(&cwd, &agent_dir, None, false));
    let gate = project_extension_dir(&cwd, false);
    assert!(gate.is_none(), "untrusted project must not load extensions");
    let _ = build_extension_runner(&cwd, None, gate.as_deref(), &[]);
    assert!(!marker.exists(), "untrusted extension was evaluated");

    // `--approve` loads them (the control run: the marker proves evaluation).
    let gate = project_extension_dir(&cwd, true);
    assert_eq!(gate.as_deref(), Some(extensions.as_path()));
    let wiring = build_extension_runner(&cwd, None, gate.as_deref(), &[]);
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "executed");
    std::fs::remove_file(&marker).unwrap();

    // A stored decision applies; `--no-approve` still wins over it.
    std::fs::write(agent.join("trust.json"), format!("{{ \"{cwd}\": true }}")).unwrap();
    assert!(stored_project_trust(&cwd, &agent_dir, None));
    assert!(!stored_project_trust(&cwd, &agent_dir, Some(false)));
}

/// `{ block = true }` without a reason must stop the action: the block keys
/// are set by the bridge, not derived from the optional reason.
#[test]
fn block_without_reason_still_blocks() {
    let runtime = Arc::new(Mutex::new(ExtensionRuntime::new()));
    runtime
        .lock()
        .unwrap()
        .load_extension(
            "blocker.luau",
            r#"
            local pillar = require("@pillar")
            pillar.on("tool_call", function(event)
                return { block = true }
            end)
            return nil
            "#,
        )
        .unwrap();
    let extension = bridge_to_runner("blocker.luau", &runtime).unwrap();
    let runner = ExtensionRunner::new(vec![extension]);
    let result = runner
        .emit_tool_call(&serde_json::json!({
            "type": "tool_call",
            "toolName": "bash",
            "input": {},
        }))
        .unwrap()
        .expect("handler blocked the call");
    assert_eq!(result["block"], serde_json::json!(true));
}

/// One registration is one call: two handlers on the same event run twice in
/// total, in registration order (dispatching the whole event from every
/// bridge handler ran them four times).
#[test]
fn one_registration_dispatches_once() {
    let runtime = Arc::new(Mutex::new(ExtensionRuntime::new()));
    runtime
        .lock()
        .unwrap()
        .load_extension(
            "two.luau",
            r#"
            local pillar = require("@pillar")
            local n = 0
            local function record(event)
                n = n + 1
                pillar.set_session_name("h" .. n)
                return nil
            end
            pillar.on("session_start", record)
            pillar.on("session_start", record)
            return nil
            "#,
        )
        .unwrap();
    let extension = bridge_to_runner("two.luau", &runtime).unwrap();
    assert_eq!(extension.handlers["session_start"].len(), 2);
    let runner = ExtensionRunner::new(vec![extension]);
    runner.emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }));
    let names = runtime.lock().unwrap().registry().session_names;
    assert_eq!(names, vec!["h1".to_string(), "h2".to_string()]);
}

/// The session holds a stable command handler; after a rebuild it must reach
/// the new VM (a captured handler kept running the previous generation).
#[test]
fn reload_swaps_the_command_handler_generation() {
    let commands = |generation: &str| {
        format!(
            r#"
            local pillar = require("@pillar")
            pillar.register_command("gen", {{
                description = "gen",
                handler = function(args, ctx)
                    pillar.fs.write("gen.txt", "{generation}")
                end,
            }})
            return nil
            "#
        )
    };
    let cwd = temp_dir("cmdgen");
    let cwd_str = cwd.to_string_lossy().to_string();
    let dir1 = extension_dir("cmdgen1", "gen.luau", &commands("v1"));
    let dir2 = extension_dir("cmdgen2", "gen.luau", &commands("v2"));
    let out = cwd.join("gen.txt");

    let wiring1 =
        build_extension_runner(&cwd_str, None, None, &[dir1.to_string_lossy().to_string()]);
    let slot = ExtensionCommandSlot::new(&wiring1.runtime);
    let handler = slot.handler();
    assert!(handler("gen", "").unwrap());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "v1");

    // A rebuild repoints the slot; the handler object the session kept must
    // reach the new generation.
    let wiring2 =
        build_extension_runner(&cwd_str, None, None, &[dir2.to_string_lossy().to_string()]);
    slot.set_runtime(&wiring2.runtime);
    assert!(handler("gen", "").unwrap());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "v2");
}

/// A `/reload` is a generation swap: the rebuilt runtime's tools replace the
/// previous generation's, and the host snapshot is published after the new
/// runner is installed.
#[tokio::test]
async fn reload_replaces_extension_tools_and_publishes() {
    let tools = |name: &str| {
        format!(
            r#"
            local pillar = require("@pillar")
            pillar.register_tool({{ name = "{name}", description = "{name}" }})
            return nil
            "#
        )
    };
    let dir1 = extension_dir("tools1", "tools.luau", &tools("v1tool"));
    let dir2 = extension_dir("tools2", "tools.luau", &tools("v2tool"));
    let configured1 = vec![dir1.to_string_lossy().to_string()];
    let configured2 = vec![dir2.to_string_lossy().to_string()];

    let mut wiring1 = build_extension_runner("", None, None, &configured1);
    assert!(wiring1.errors.is_empty(), "{:?}", wiring1.errors);
    let tools1 = wiring1.custom_tools();
    let runner1 = wiring1.take_runner();
    let session_slot: SessionSlot = Arc::new(Mutex::new(None));
    let published = Arc::new(AtomicBool::new(false));

    // The host factory of a rebuild: load into a fresh VM and hand the
    // session the new generation's tools while the previous runner is still
    // installed (that is what identifies the tools being replaced).
    let factory: ExtensionRunnerFactory = {
        let session_slot = Arc::clone(&session_slot);
        Arc::new(move |_flags| {
            let mut rebuilt = build_extension_runner("", None, None, &configured2);
            assert!(rebuilt.errors.is_empty(), "{:?}", rebuilt.errors);
            if let Some(session) = session_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                session.replace_extension_tools(rebuilt.custom_tools());
            }
            rebuilt.take_runner()
        })
    };

    let session = session_for_reload(runner1, tools1, factory);
    *session_slot.lock().unwrap() = Some(Arc::clone(&session));
    let published_for_session = Arc::clone(&published);
    session.set_extension_reload_publish(Arc::new(move || {
        published_for_session.store(true, Ordering::SeqCst);
    }));
    assert_eq!(tool_names(&session), vec!["v1tool".to_string()]);

    session.reload(None).await.expect("reload succeeds");

    assert_eq!(
        tool_names(&session),
        vec!["v2tool".to_string()],
        "the rebuilt generation's tools must replace the old ones"
    );
    assert!(
        published.load(Ordering::SeqCst),
        "the host snapshot is published after the new runner is installed"
    );
}

fn tool_names(session: &Arc<AgentSession>) -> Vec<String> {
    session
        .state()
        .tools
        .iter()
        .map(|tool| tool.name().to_string())
        .collect()
}

/// A session with the rebuilt-generation wiring and no stream (the test never
/// prompts).
fn session_for_reload(
    runner: ExtensionRunner,
    tools: Vec<pillar_agent::AgentTool>,
    factory: ExtensionRunnerFactory,
) -> Arc<AgentSession> {
    let mut config = base_session_config(runner, tools);
    config.extension_runner_rebuild = Some(factory);
    Arc::new(AgentSession::new(config))
}

fn base_session_config(
    runner: ExtensionRunner,
    tools: Vec<pillar_agent::AgentTool>,
) -> AgentSessionConfig {
    let mut options = AgentOptions::new(StreamFn::new(|_, _| async { panic!("unused stream") }));
    options.initial_state = Some(AgentState {
        system_prompt: "Test".to_string(),
        tools,
        ..AgentState::default()
    });
    let agent = Arc::new(Agent::new(options));
    let model_runtime =
        Arc::new(ModelRuntime::new(CreateModelRuntimeOptions::default()).expect("model runtime"));
    let session_manager = Arc::new(Mutex::new(
        SessionManager::in_memory("", None).expect("in-memory session"),
    ));
    let settings_manager = Arc::new(Mutex::new(SettingsManager::in_memory(
        serde_json::json!({}),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    )));
    let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: temp_dir("reload-agent").to_string_lossy().to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));
    AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        String::new(),
        resource_loader,
        model_runtime,
        Arc::new(Mutex::new(runner)),
    )
}

// ============================================================================
// Effect gate (docs/ARCHITECTURE-REVIEW-s05c0.md 0)
// ============================================================================

/// A policy that denies every effect with a fixed reason.
fn deny_all(reason: &str) -> Arc<EffectBroker> {
    let reason = reason.to_string();
    EffectBroker::new(Arc::new(move |_intent: &EffectIntent| {
        EffectDecision::Deny {
            reason: reason.clone(),
        }
    }))
}

/// `pillar.fs.write` must be authorized before it touches the filesystem.
#[test]
fn a_denied_fs_write_keeps_the_file_absent() {
    let cwd = temp_dir("effects-fs");
    let cwd_str = cwd.to_string_lossy().to_string();
    let dir = extension_dir(
        "denied-fs",
        "write.luau",
        r#"
        local pillar = require("@pillar")
        pillar.fs.write("marker.txt", "executed")
        return nil
        "#,
    );
    let broker = deny_all("policy");
    let slots = ExtensionHostSlots::with_broker(&cwd_str, Arc::clone(&broker));
    let wiring = build_extension_runner_with_slots(
        &cwd_str,
        None,
        None,
        &[dir.to_string_lossy().to_string()],
        &slots,
    );
    assert!(
        !wiring.errors.is_empty(),
        "the denied setup must not report success"
    );
    assert!(!cwd.join("marker.txt").exists());
    assert_eq!(
        broker.audit(),
        vec![EffectIntent::FsWrite {
            path: cwd.join("marker.txt").to_string_lossy().to_string(),
        }]
    );
}

/// `pi.exec` must be authorized before a process is spawned: the same
/// extension writes the marker with a permissive broker and does not with a
/// denying one.
#[test]
fn a_denied_exec_never_spawns_the_process() {
    let cwd = temp_dir("effects-exec");
    let cwd_str = cwd.to_string_lossy().to_string();
    let configured = extension_dir(
        "exec-ext",
        "run.luau",
        r#"
        local pillar = require("@pillar")
        pillar.exec("touch", { "spawned.txt" })
        return nil
        "#,
    );
    let configured = vec![configured.to_string_lossy().to_string()];
    let spawned = cwd.join("spawned.txt");

    let allowed = ExtensionHostSlots::new(&cwd_str);
    let _ = build_extension_runner_with_slots(&cwd_str, None, None, &configured, &allowed);
    assert!(spawned.exists(), "the control run must spawn the process");
    std::fs::remove_file(&spawned).unwrap();

    let broker = deny_all("policy");
    let slots = ExtensionHostSlots::with_broker(&cwd_str, Arc::clone(&broker));
    let _ = build_extension_runner_with_slots(&cwd_str, None, None, &configured, &slots);
    assert!(!spawned.exists(), "a denied exec must not spawn anything");
    assert!(broker.audit().iter().any(|intent| matches!(
        intent,
        EffectIntent::Exec { command, .. } if command == "touch"
    )));
}

/// The session's tool path uses the same authorizer (and therefore the same
/// audit trail) as the host's `exec` / `fs` callbacks.
#[tokio::test]
async fn tool_calls_pass_the_same_effect_gate() {
    let broker = deny_all("policy");
    let mut config = base_session_config(ExtensionRunner::new(Vec::new()), Vec::new());
    config.effect_authorizer = Some(broker.authorizer());
    let session = Arc::new(AgentSession::new(config));
    session.install_tool_hooks();

    let hook = session
        .agent()
        .before_tool_call_hook()
        .expect("tool hook installed");
    let result = hook(tool_context("noop"), None)
        .await
        .expect("the gate answers a result");
    assert!(result.block, "a denied tool call must be blocked");
    assert_eq!(result.reason.as_deref(), Some("policy"));
    assert!(broker.audit().contains(&EffectIntent::ToolCall {
        name: "noop".to_string(),
        input: serde_json::json!({}),
    }));
}

fn tool_context(name: &str) -> pillar_agent::BeforeToolCallContext {
    pillar_agent::BeforeToolCallContext {
        assistant_message: AssistantMessage {
            content: Vec::new(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        },
        tool_call: pillar_agent::AgentToolCall {
            id: "call-1".to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        },
        args: Arc::new(Mutex::new(serde_json::json!({}))),
    }
}

/// The host callbacks must not reach the process or the filesystem directly:
/// a path that does would skip both the policy and the audit trail.
#[test]
fn host_callbacks_do_not_bypass_the_effect_broker() {
    let runner = include_str!("../src/runner.rs");
    assert!(
        !runner.contains("std::process::"),
        "runner.rs must go through the effect broker to run a process"
    );
    assert!(
        !runner.contains("std::fs::"),
        "runner.rs must go through the effect broker to touch the filesystem"
    );
}
