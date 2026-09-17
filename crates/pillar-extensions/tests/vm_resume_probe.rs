//! Probe: can a waiting host call be suspended and resumed from the host?
//!
//! This is the mechanism the remaining Luau async work needs (TASKS: "host 呼出の
//! 完全な非同期化"). Today every waiting host call blocks the extension's thread
//! — and with it the `ExtensionRuntime` mutex — for its whole duration. The
//! design under test is cooperative instead:
//!
//! 1. the Luau wrapper of a waiting call (`pillar.exec`, `pillar.fs`,
//!    `ctx.ui.*`) does `local result = coroutine.yield(request)`,
//! 2. the host resumes the coroutine, receives the request, and is then *free*
//!    (it can drop the runtime lock, run the command, or abort),
//! 3. the host resumes again with the result, which becomes `coroutine.yield`'s
//!    return value, and the wrapper returns it to the caller.
//!
//! The probe drives exactly that with luaur's `Thread`, plus the abort shape
//! (`resume_error`). It does not change the product path: it establishes that
//! the primitives behave as the design needs before that work is scheduled, and
//! documents the protocol the wrappers would follow.

use luaur_rt::{Function, Lua, LuaSerdeExt, ThreadStatus, Value};

fn to_lua(lua: &Lua, json: &serde_json::Value) -> Value {
    lua.to_value(json).expect("a JSON value converts")
}

fn from_lua(lua: &Lua, value: Value) -> serde_json::Value {
    lua.from_value(value).expect("a Lua value converts")
}

/// The wrapper shape the port would generate: the host request travels out as a
/// yield, its result comes back as the yield's return value.
const WRAPPER: &str = r#"
    local function exec(command, args)
        local request = { kind = "exec", command = command, args = args }
        local result = coroutine.yield(request)
        if result.aborted then
            error("the extension's command was aborted")
        end
        return result.stdout
    end

    return function()
        local out = exec("make", { "story" })
        return "built: " .. out
    end
"#;

#[test]
fn a_host_request_can_be_suspended_and_resumed_with_its_result() {
    let lua = Lua::new();
    let body: Function = lua.load(WRAPPER).eval().expect("the wrapper compiles");
    let thread = lua.create_thread(body).expect("the coroutine is created");

    // First resume: the coroutine runs to the host call and yields its request.
    let request: Value = thread.resume(()).expect("the coroutine yields a request");
    let request = from_lua(&lua, request);
    assert_eq!(request["kind"], "exec");
    assert_eq!(request["command"], "make");
    assert_eq!(request["args"], serde_json::json!(["story"]));
    assert_eq!(
        thread.status(),
        ThreadStatus::Resumable,
        "the coroutine is suspended and resumable"
    );

    // The host is free here (no Lua frame on the stack, no lock held): it would
    // run the command on its own thread. The result is handed back by resuming.
    let result: String = thread
        .resume(to_lua(
            &lua,
            &serde_json::json!({ "stdout": "ok", "code": 0 }),
        ))
        .expect("the coroutine finishes with the host's result");
    assert_eq!(result, "built: ok");
    assert_eq!(thread.status(), ThreadStatus::Finished);
}

#[test]
fn an_abort_resumes_the_waiting_call_with_an_error() {
    let lua = Lua::new();
    let body: Function = lua.load(WRAPPER).eval().expect("the wrapper compiles");
    let thread = lua.create_thread(body).expect("the coroutine is created");
    let _request: Value = thread.resume(()).expect("the coroutine yields a request");

    // An aborted call is resumed as an error: the waiting call unwinds instead
    // of staying suspended forever.
    let error = thread
        .resume_error::<Value>("the tool call was aborted")
        .expect_err("the resume carries the error");
    assert!(
        error.to_string().contains("aborted"),
        "the error reaches the caller: {error}"
    );
    assert!(
        matches!(
            thread.status(),
            ThreadStatus::Error | ThreadStatus::Finished
        ),
        "the coroutine is no longer waiting: {:?}",
        thread.status()
    );
}

/// The wrapper can also turn an aborted *result* into the error itself (the
/// shape a host that cannot resume with an error uses).
#[test]
fn an_aborted_result_raises_inside_the_wrapper() {
    let lua = Lua::new();
    let body: Function = lua.load(WRAPPER).eval().expect("the wrapper compiles");
    let thread = lua.create_thread(body).expect("the coroutine is created");
    let _request: Value = thread.resume(()).expect("the coroutine yields a request");

    let error = thread
        .resume::<Value>(to_lua(&lua, &serde_json::json!({ "aborted": true })))
        .expect_err("the wrapper raises");
    assert!(
        error.to_string().contains("aborted"),
        "the wrapper's error reaches the caller: {error}"
    );
}

/// The same protocol carries a *stream* of host events (what `ctx.ui.custom`
/// needs): each resume delivers one event and the coroutine yields again until
/// it is done.
#[test]
fn a_sequence_of_host_values_can_drive_one_waiting_call() {
    let lua = Lua::new();
    let body: Function = lua
        .load(
            r#"
            -- A chunk runs on the main state, which cannot yield: the body must
            -- be a function the host turns into a coroutine (the runtime would
            -- drive a tool call the same way).
            return function()
                local events = {}
                for index = 1, 3 do
                    local event = coroutine.yield({ kind = "next", index = index })
                    table.insert(events, event.value)
                    if event.done then break end
                end
                return table.concat(events, ",")
            end
            "#,
        )
        .eval()
        .expect("the loop compiles");
    let thread = lua.create_thread(body).expect("the coroutine is created");

    // Each resume asks for the next event; the third one ends the loop.
    let yielded: Value = thread.resume(()).expect("the first event");
    assert_eq!(from_lua(&lua, yielded)["index"], 1);
    let yielded: Value = thread
        .resume(to_lua(&lua, &serde_json::json!({ "value": 1 })))
        .expect("the second event");
    assert_eq!(from_lua(&lua, yielded)["index"], 2);
    let yielded: Value = thread
        .resume(to_lua(&lua, &serde_json::json!({ "value": 2 })))
        .expect("the third event");
    assert_eq!(from_lua(&lua, yielded)["index"], 3);

    let finished: String = thread
        .resume(to_lua(
            &lua,
            &serde_json::json!({ "value": 3, "done": true }),
        ))
        .expect("the waiting call finishes");
    assert_eq!(finished, "1,2,3");
    assert_eq!(thread.status(), ThreadStatus::Finished);
}

/// How many yield/resume cycles one coroutine can take before the VM gives up?
///
/// This is the probe for the assertion we hit when a tool's host calls suspend:
/// a tool doing several host calls tripped `LUAU_ASSERT ... lua_xmove.rs:17`. A
/// single cycle works; the question is where it breaks.
#[test]
fn a_coroutine_can_be_resumed_many_times() {
    let lua = Lua::new();
    let body: Function = lua
        .load(
            r#"
            return function()
                local total = 0
                for index = 1, 20 do
                    local answer = coroutine.yield({ kind = "next", index = index })
                    total = total + (answer.value or 0)
                end
                return total
            end
            "#,
        )
        .eval()
        .expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");

    let mut yielded = from_lua(&lua, thread.resume(()).expect("the first yield"));
    let mut total = 0i64;
    let mut finished = None;
    for index in 1..=20 {
        assert_eq!(yielded["index"], index, "cycle {index}");
        total += index;
        let value = from_lua(
            &lua,
            thread
                .resume::<Value>(to_lua(&lua, &serde_json::json!({ "value": index })))
                .expect("the coroutine is resumable"),
        );
        if let Some(done) = value.as_i64() {
            finished = Some(done);
            break;
        }
        yielded = value;
    }
    assert_eq!(
        finished,
        Some(total),
        "the coroutine ran all {total} cycles"
    );
    assert_eq!(thread.status(), ThreadStatus::Finished);
}
