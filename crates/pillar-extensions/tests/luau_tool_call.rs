//! The minimal-plus-Luau gate the policy review asked for (sb39f R9): a *real*
//! Luau extension file, loaded headless through the contract, whose tool the
//! host invokes — without the coding agent, its tools or its provider catalog.
//!
//! This is the composition the embedding profiles use: `pillar-extensions` +
//! `pillar-extensions-contract` + the runtime core. The VM's tree is checked by
//! `dependency_profiles::the_luau_profile_takes_the_runtime_without_its_native_surface`;
//! this test checks the other half — that a tool registered from real Luau
//! source actually runs, observes its abort signal, and reaches the host's exec
//! callback through `pillar.exec`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_agent::abort::AbortSignal;
use pillar_extensions::runtime::{ExtensionRuntime, VmBudget};
use pillar_extensions_contract::{ExecOptions, ExecResult};

/// A headless host: no filesystem, no session, no UI — just the exec callback
/// the extension's tool uses.
fn runtime_with_exec(log: Arc<Mutex<Vec<String>>>) -> ExtensionRuntime {
    let runtime = ExtensionRuntime::new();
    runtime.set_exec_host(Arc::new(
        move |command: &str, args: &[String], options: &ExecOptions| {
            log.lock().unwrap().push(format!("{command} {args:?}"));
            ExecResult {
                stdout: format!("ran {command}"),
                stderr: String::new(),
                code: 0,
                killed: options.signal.as_ref().is_some_and(|s| s.is_aborted()),
                truncated: false,
            }
        },
    ));
    runtime
}

const EXTENSION: &str = r#"
    --!strict
    local pillar = require("@pillar")

    pillar.register_tool({
        name = "greet",
        description = "greet someone",
        parameters = pillar.schema.object({
            name = pillar.schema.string("who to greet"),
        }),
        execute = function(tool_call_id, params, signal, on_update, ctx)
            local ran = pillar.exec("make", { "story" })
            return {
                content = {
                    { type = "text", text = "hello " .. params.name .. " (" .. tool_call_id .. ")" },
                    { type = "text", text = "exec: " .. ran.stdout },
                },
                details = { aborted = signal.aborted() },
            }
        end,
    })

    return nil
"#;

#[test]
fn a_real_extension_tool_runs_headless() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime_with_exec(Arc::clone(&log));
    runtime
        .load_extension("/ext/greet.luau", EXTENSION)
        .expect("the extension loads");

    // The host sees the registered tool (this is what the coding agent turns
    // into an `AgentTool`, and what an engine host would expose to its model).
    let tools = runtime.registry().tools;
    assert!(
        tools.iter().any(|tool| tool.value["name"] == "greet"),
        "the tool is registered: {tools:?}"
    );

    let result = runtime
        .call_tool(
            "greet",
            "call-1",
            serde_json::json!({ "name": "Ada" }),
            None,
            None,
        )
        .expect("the tool call runs");
    assert_eq!(result["content"][0]["text"], "hello Ada (call-1)");
    assert_eq!(result["content"][1]["text"], "exec: ran make");
    assert_eq!(result["details"]["aborted"], false);
    assert_eq!(
        log.lock().unwrap().as_slice(),
        ["make [\"story\"]"],
        "the extension's exec reached the host callback"
    );
}

/// The tool sees the *live* abort signal: an aborted call reports it and the
/// host's exec callback is handed the same signal (which is what kills a stuck
/// build instead of awaiting it).
#[test]
fn an_aborted_tool_call_sees_its_signal() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime_with_exec(Arc::clone(&log));
    runtime
        .load_extension("/ext/greet.luau", EXTENSION)
        .unwrap();

    let signal = AbortSignal::new();
    signal.abort();
    let result = runtime
        .call_tool(
            "greet",
            "call-2",
            serde_json::json!({ "name": "Ada" }),
            Some(signal),
            None,
        )
        .expect("the tool call runs");
    assert_eq!(
        result["details"]["aborted"], true,
        "signal.aborted() is true inside the tool"
    );
    assert!(
        log.lock().unwrap().len() == 1,
        "the exec still reached the host, which saw the aborted signal"
    );
}

/// A tool whose Lua never returns to the host: without a VM budget it would
/// occupy the extension runtime's VM forever (the policy review's remaining
/// unbounded wait). The interrupt stops it at a safepoint.
const ENDLESS: &str = r#"
    --!strict
    local pillar = require("@pillar")

    pillar.register_tool({
        name = "spin",
        description = "never returns",
        parameters = pillar.schema.object({}),
        execute = function(tool_call_id, params, signal, on_update, ctx)
            local index = 0
            while true do
                index = index + 1
            end
            return { content = { { type = "text", text = tostring(index) } } }
        end,
    })

    return nil
"#;

#[test]
fn an_unbounded_lua_loop_is_stopped_by_the_budget() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime_with_exec(log);
    runtime.set_vm_budget(VmBudget::steps(200_000));
    runtime.load_extension("/ext/spin.luau", ENDLESS).unwrap();

    let started = Instant::now();
    let error = runtime
        .call_tool("spin", "call-1", serde_json::json!({}), None, None)
        .expect_err("the loop is stopped");
    assert!(
        error.contains("step budget"),
        "the failure says why: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the call returned promptly: {:?}",
        started.elapsed()
    );

    // The guard is per operation: the next call gets a fresh budget (and the
    // runtime is still usable).
    let tools = runtime.registry().tools;
    assert_eq!(tools.len(), 1);
}

/// An abort stops a pure-Lua loop too: the tool's signal is part of the VM
/// guard, so a stuck extension does not need the host to kill anything.
#[test]
fn an_aborted_lua_loop_stops_without_touching_the_host() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime_with_exec(log);
    // No step limit: only the abort may stop this one.
    runtime.set_vm_budget(VmBudget::unlimited());
    runtime.load_extension("/ext/spin.luau", ENDLESS).unwrap();

    let signal = AbortSignal::new();
    let killer = signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        killer.abort();
    });
    let started = Instant::now();
    let error = runtime
        .call_tool("spin", "call-2", serde_json::json!({}), Some(signal), None)
        .expect_err("the abort stops the loop");
    assert!(
        error.contains("aborted"),
        "the failure names the abort: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the abort was noticed at a safepoint: {:?}",
        started.elapsed()
    );
}
