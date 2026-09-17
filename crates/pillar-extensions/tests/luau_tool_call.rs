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

use luaur_rt::Function;
use pillar_ai::types::Content;
use pillar_agent::abort::AbortSignal;
use pillar_extensions::runtime::{ExtensionRuntime, HostApi, ToolStep, VmBudget};
use pillar_extensions_contract::{
    ExecOptions, ExecResult, ExtensionContextFacts, ExtensionCustomSurface, ExtensionMode,
};

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

/// A component that never calls `done` (and a pump that never answers it) must
/// not pin the runtime for the whole wait timeout: the wait is sliced, so the
/// tool call's abort ends it within a slice and the call returns promptly (the
/// VM guard then stops the Lua at its next safepoint).
const STUCK_UI: &str = r#"
    --!strict
    local pillar = require("@pillar")

    pillar.register_tool({
        name = "stuck_ui",
        description = "opens a component that never finishes",
        parameters = pillar.schema.object({}),
        execute = function(tool_call_id, params, signal, on_update, ctx)
            local result = ctx.ui.custom(function(tui, theme, keybindings, done)
                return { render = function(width) return { "waiting" } end }
            end)
            return { content = { { type = "text", text = tostring(result) } } }
        end,
    })

    return nil
"#;

#[test]
fn an_abort_ends_a_waiting_ui_component() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime_with_exec(Arc::clone(&log));
    // The host mounts the surface — and *keeps* it, so the pump's event channel
    // stays open — and never sends an event to it.
    let mounted: Arc<Mutex<Option<ExtensionCustomSurface>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&mounted);
    runtime.set_host_api(HostApi {
        context: Some(Arc::new(|| ExtensionContextFacts {
            cwd: "/tmp".to_string(),
            mode: ExtensionMode::Tui,
            has_ui: true,
        })),
        ui_custom: Some(Arc::new(move |surface: ExtensionCustomSurface| {
            *slot.lock().unwrap() = Some(surface);
            Ok(())
        })),
        ..Default::default()
    });
    runtime.load_extension("/ext/stuck.luau", STUCK_UI).unwrap();

    let signal = AbortSignal::new();
    let killer = signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        killer.abort();
    });
    let started = Instant::now();
    // The component never answers, so this would previously wait the whole
    // timeout (600 s): the abort ends the wait within one slice, and the tool
    // then finishes with whatever the closed component returned.
    match runtime.call_tool(
        "stuck_ui",
        "call-3",
        serde_json::json!({}),
        Some(signal),
        None,
    ) {
        Ok(value) => assert_eq!(
            value["content"][0]["text"], "nil",
            "the closed component returned nothing: {value}"
        ),
        // A longer run reaches a safepoint with the signal set first, in which
        // case the guard ends the call.
        Err(error) => assert!(error.contains("aborted"), "{error}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the abort ended the wait promptly: {:?}",
        started.elapsed()
    );
    assert!(
        mounted.lock().unwrap().is_some(),
        "the host still holds the mounted surface"
    );
}

/// The memory ceiling: the step budget stops a *loop*, but a single allocation
/// that never reaches a safepoint (here one `string.rep`) is what the VM's
/// memory limit is for — the host must not be sized by an extension.
const MEMORY_BOMB: &str = r#"
    --!strict
    local pillar = require("@pillar")

    pillar.register_tool({
        name = "allocate",
        description = "allocates far past the ceiling",
        parameters = pillar.schema.object({}),
        execute = function(tool_call_id, params, signal, on_update, ctx)
            local huge = string.rep("x", 64 * 1024 * 1024)
            return { content = { { type = "text", text = tostring(#huge) } } }
        end,
    })

    pillar.register_tool({
        name = "small",
        description = "a normal tool, to show the runtime still works",
        parameters = pillar.schema.object({}),
        execute = function(tool_call_id, params, signal, on_update, ctx)
            return { content = { { type = "text", text = "small ok" } } }
        end,
    })

    return nil
"#;

#[test]
fn a_single_huge_allocation_is_refused() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime_with_exec(log);
    runtime.set_vm_budget(VmBudget {
        memory_bytes: 16 * 1024 * 1024,
        ..VmBudget::steps(50_000_000)
    });
    runtime
        .load_extension("/ext/bomb.luau", MEMORY_BOMB)
        .unwrap();

    let error = runtime
        .call_tool("allocate", "call-1", serde_json::json!({}), None, None)
        .expect_err("the allocation is refused");
    assert!(
        error.to_lowercase().contains("memory"),
        "the failure names memory: {error}"
    );

    // The runtime is still usable: the guard bounds what one call may allocate,
    // it does not poison the VM.
    let result = runtime
        .call_tool("small", "call-2", serde_json::json!({}), None, None)
        .expect("a normal tool still runs");
    assert_eq!(result["content"][0]["text"], "small ok");
    assert_eq!(runtime.registry().tools.len(), 2);
}

/// The host-side registry cap: the VM's budgets bound the VM, not the host's
/// own vectors, so a setup that registers without end must be refused.
#[test]
fn an_endless_registration_loop_is_refused() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime_with_exec(log);
    runtime.set_max_registrations(25);
    let error = runtime
        .load_extension(
            "/ext/greedy.luau",
            r#"
            --!strict
            local pillar = require("@pillar")
            for index = 1, 100 do
                pillar.on("tool_call", function() end)
            end
            return nil
            "#,
        )
        .expect_err("the registration loop is refused");
    assert!(
        error.to_string().contains("more than 25"),
        "the failure names the cap: {error}"
    );
    // The failed load was rolled back: nothing of it stays registered.
    assert_eq!(
        runtime.registry().event_handlers.len(),
        0,
        "the failed setup left nothing behind"
    );
}

/// The host functions a tool calls must work inside a coroutine too — that is
/// the precondition for running tool calls there (so a waiting host call can
/// suspend instead of blocking). They build their results with the *calling*
/// state (`Lua::create_function`), which this pins against the real runtime.
#[test]
fn the_runtimes_host_functions_survive_a_coroutine() {
    let runtime = runtime_with_exec(Arc::new(Mutex::new(Vec::new())));
    runtime.set_host_api(HostApi {
        fs: Some(Arc::new(|op: &str, _path: &str, _content: Option<&str>| {
            Ok(match op {
                "read" => serde_json::json!("content"),
                "list" => serde_json::json!(["a.luau"]),
                "stat" => serde_json::json!({ "type": "file" }),
                _ => serde_json::Value::Null,
            })
        })),
        get_commands: Some(Arc::new(|| serde_json::json!([{ "name": "hello" }]))),
        get_flag: Some(Arc::new(|name: &str| {
            (name == "level").then(|| serde_json::json!("high"))
        })),
        ..Default::default()
    });
    let lua = runtime.vm().clone();
    // `pillar.fs.*` is not here: it *suspends* inside a tool now, so a bare
    // coroutine cannot complete it (see the host-call tests).
    let script = r#"
        local pillar = require("@pillar")
        return function()
            local commands = pillar.get_commands()
            local flag = pillar.get_flag("level")
            local schema = pillar.schema.object({ name = pillar.schema.string() })
            return commands[1].name .. "/" .. flag .. "/" .. schema.type
        end
    "#;
    let expected = "hello/high/object";

    // Inside a coroutine (the shape a tool call will run in).
    let body: Function = lua.load(script).eval().expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");
    let out: String = thread.resume(()).expect("the call runs");
    assert_eq!(
        out, expected,
        "the host functions answer inside a coroutine"
    );

    // …and on the main state, unchanged.
    let out: String = lua
        .load(script)
        .eval::<Function>()
        .expect("the body compiles")
        .call(())
        .expect("the call runs");
    assert_eq!(out, expected);
}

/// The cooperative protocol from the host's side: a tool that waits can be
/// driven step by step, with the runtime lock released while the host does the
/// work (TASKS: 非同期 host 呼出, step 2). This is what lets a long build stop
/// blocking the extension runtime.
const BUILD_TOOL: &str = r#"
    --!strict
    local pillar = require("@pillar")

    pillar.register_tool({
        name = "build",
        description = "runs the build through the host",
        parameters = pillar.schema.object({ target = pillar.schema.string() }),
        execute = function(tool_call_id, params, signal, on_update, ctx)
            local result = pillar.exec("make", { params.target })
            return {
                content = { { type = "text", text = "built " .. tostring(result.stdout) } },
                details = { code = result.code, id = tool_call_id },
            }
        end,
    })

    return nil
"#;

#[test]
fn a_host_can_drive_a_tool_call_step_by_step() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = Arc::new(Mutex::new(runtime_with_exec(Arc::clone(&log))));
    runtime
        .lock()
        .unwrap()
        .load_extension("/ext/build.luau", BUILD_TOOL)
        .expect("the extension loads");

    // Step 1: start the call. It suspends on `pillar.exec` and reports the
    // request instead of running anything.
    let (mut call, request) = {
        let mut guard = runtime.lock().unwrap();
        let (call, step) = guard
            .start_tool_call(
                "build",
                "call-1",
                serde_json::json!({ "target": "story" }),
                None,
                None,
            )
            .expect("the call starts");
        let request = match step {
            ToolStep::HostCall(request) => request,
            ToolStep::Done(result) => panic!("the tool should be waiting: {result:?}"),
        };
        (call, request)
    };
    assert_eq!(request.kind, "exec");
    assert_eq!(request.json["command"], "make");
    assert_eq!(request.json["args"], serde_json::json!(["story"]));
    assert_eq!(call.tool_name(), "build");

    // While the call is suspended the runtime is free: another thread can use
    // it (this is the property the state machine exists for).
    let other = Arc::clone(&runtime);
    let borrowed = std::thread::spawn(move || {
        let guard = other.lock().unwrap();
        guard.registry().tools.len()
    })
    .join()
    .expect("the runtime was not held by the waiting call");
    assert_eq!(borrowed, 1, "the other thread saw the registered tool");

    // Step 2: the host answers (its own work, no lock held), and the tool
    // continues to its result.
    let result = {
        let mut guard = runtime.lock().unwrap();
        match guard
            .step_tool_call(
                &mut call,
                serde_json::json!({ "stdout": "story.bin", "code": 0 }),
            )
            .expect("the call resumes")
        {
            ToolStep::Done(result) => result.expect("the tool returned a result"),
            ToolStep::HostCall(repeat) => panic!("the tool asked again: {repeat:?}"),
        }
    };
    assert_eq!(result["content"][0]["text"], "built story.bin");
    assert_eq!(result["details"]["code"], 0);
    assert_eq!(result["details"]["id"], "call-1");
}

#[test]
fn a_tools_fs_read_returns_the_string_through_the_sync_driver() {
    use pillar_extensions::runtime::HostApi;
    let mut runtime = runtime_with_exec(Arc::new(Mutex::new(Vec::new())));
    runtime.set_host_api(HostApi {
        fs: Some(Arc::new(|op: &str, path: &str, _content: Option<&str>| {
            if op == "read" && path == "notes.txt" {
                Ok(serde_json::json!("file content"))
            } else {
                Ok(serde_json::Value::Null)
            }
        })),
        ..Default::default()
    });
    runtime
        .load_extension(
            "/ext/notes.luau",
            r#"
            local pillar = require("@pillar")
            pillar.register_tool({
                name = "notes",
                description = "reads notes",
                parameters = pillar.schema.object({}),
                execute = function(tool_call_id, params, signal, on_update, ctx)
                    local text = pillar.fs.read("notes.txt")
                    return { content = { { type = "text", text = type(text) .. "=" .. tostring(text) } } }
                end,
            })
            return nil
            "#,
        )
        .expect("the extension loads");
    let result = runtime
        .call_tool("notes", "call-1", serde_json::json!({}), None, None)
        .expect("the tool finishes");
    println!("RESULT: {}", result["content"][0]["text"]);
    assert_eq!(result["content"][0]["text"], "string=file content");
}

/// The same fs calls through the *bridge runner* (the CLI's path): the host
/// answers the `fs` request with `run_with` while holding no runtime lock.
#[test]
fn a_tools_fs_calls_run_through_the_host_call_runner() {
    use pillar_extensions::bridge::{HostCallRunner, bridge_to_agent_tools_with};

    let runtime = Arc::new(Mutex::new(runtime_with_exec(Arc::new(Mutex::new(Vec::new())))));
    runtime.lock().unwrap().set_host_api(HostApi {
        fs: Some(Arc::new(|op: &str, path: &str, _content: Option<&str>| {
            match (op, path) {
                ("read", "notes.txt") => Ok(serde_json::json!("file content")),
                ("exists", "notes.txt") => Ok(serde_json::json!(true)),
                ("read", "gone.txt") => Ok(serde_json::Value::Null),
                _ => Err(format!("no such path: {path}")),
            }
        })),
        ..Default::default()
    });
    runtime.lock().unwrap().load_extension(
        "/ext/notes.luau",
        r#"
        local pillar = require("@pillar")
        pillar.register_tool({
            name = "notes",
            description = "reads notes",
            parameters = pillar.schema.object({}),
            execute = function(tool_call_id, params, signal, on_update, ctx)
                local text = pillar.fs.read("notes.txt")
                local missing = pillar.fs.read("gone.txt")
                local ok, err = pcall(function() return pillar.fs.read("boom.txt") end)
                return {
                    content = { { type = "text", text = text .. "/" .. tostring(missing) .. "/" .. tostring(ok) .. "/" .. tostring(err) } },
                }
            end,
        })
        return nil
        "#,
    )
    .expect("the extension loads");

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let runner_seen = Arc::clone(&seen);
    let runner_runtime = Arc::clone(&runtime);
    let runner: HostCallRunner = Arc::new(move |request, abort| {
        let seen = Arc::clone(&runner_seen);
        let runtime = Arc::clone(&runner_runtime);
        Box::pin(async move {
            assert!(
                runtime.try_lock().is_ok(),
                "the runtime is free while the host does the I/O"
            );
            let services = runtime.lock().unwrap().host_services();
            seen.lock().unwrap().push(format!(
                "{}/{}",
                request.json["kind"].as_str().unwrap_or_default(),
                request.json["op"].as_str().unwrap_or_default()
            ));
            request.run_with(&services, abort)
        })
    });

    let tools = bridge_to_agent_tools_with(&runtime, Some(runner));
    let tool = tools.first().expect("the tool is bridged");
    let result = futures::executor::block_on((tool.execute)(
        "call-1".to_string(),
        serde_json::json!({}),
        None,
        None,
    ))
    .expect("the tool finishes");
    let text = match &result.content[0] {
        Content::Text { text, .. } => text.clone(),
        other => panic!("unexpected content {other:?}"),
    };
    assert!(text.starts_with("file content/nil/"), "{text}");
    assert!(text.contains("no such path"), "the host error reached the tool: {text}");
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[
            "fs/read".to_string(),
            "fs/read".to_string(),
            "fs/read".to_string()
        ]
    );
}
