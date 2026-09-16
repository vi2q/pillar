//! Regression tests for the safety findings of
//! docs/ARCHITECTURE-REVIEW-s05c0.md (A/B): an untrusted project's
//! extensions are never evaluated, a handler that blocks stops the action
//! even when it gives no reason, and one registration dispatches exactly
//! once.

use std::sync::{Arc, Mutex};

use pillar_cli::runner::build_extension_runner;
use pillar_cli::trust::{project_extension_dir, resolve_project_trust, stored_project_trust};
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
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
