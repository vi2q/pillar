//! Probe: host functions must build their result with the **calling** state.
//!
//! This is what the cooperative/async host-call work depends on
//! (TASKS: 非同期 host 呼出の実装). Both designs suspend a **tool call** in a
//! coroutine (`Thread`) so the host can work without holding the runtime lock —
//! and a tool almost always calls Rust-backed host functions (`ctx.ui.*`,
//! `pillar.fs.*`, `pillar.get_*`, `custom.next`, …).
//!
//! `Function::wrap` closures never see the calling state, so they build their
//! result with a captured `Lua` handle — the main state. On the main state that
//! is the same state and everything works. Inside a coroutine it pushes the
//! result onto the *wrong* stack, and the VM returns the coroutine's own
//! arguments instead (the bug that showed up as `attempt to index number with
//! 'kind'` in the UI loop).
//!
//! `Lua::create_function` hands the closure the calling state, which is the
//! shape a host function must use once tools run in coroutines — proven by the
//! last test here. The runtime's host functions therefore move from
//! `Function::wrap` to `create_function` where they build values (the reading
//! ones, like `signal.aborted()`, never needed the state and keep working).

use luaur_rt::{Function, Lua, LuaSerdeExt, Value};

/// A Rust host function returning a table, built with a *captured* handle (the
/// `Function::wrap` shape).
fn captured_host(lua: &Lua) -> Value {
    use luaur_rt::IntoLua;
    let captured = lua.clone();
    Function::wrap(move |id: String| {
        captured
            .to_value(&serde_json::json!({ "kind": "close", "id": id }))
            .map_err(luaur_rt::Error::external)
    })
    .into_lua(lua)
    .expect("the host function converts")
}

/// A Rust host function returning a table, built with the *calling* handle (the
/// `create_function` shape).
fn calling_host(lua: &Lua) -> Function {
    lua.create_function(move |calling: &Lua, id: String| {
        calling
            .to_value(&serde_json::json!({ "kind": "close", "id": id }))
            .map_err(luaur_rt::Error::external)
    })
    .expect("the host function is created")
}

const CALL: &str = r#"
    return function(host)
        local event = host("7")
        return type(event) .. "/" .. tostring(event.kind) .. "/" .. tostring(event.id)
    end
"#;

/// The `Function::wrap` shape works on the main state — which is why the port's
/// host functions behave correctly today, with tools running there.
#[test]
fn a_captured_handle_works_on_the_main_state() {
    let lua = Lua::new();
    let host = captured_host(&lua);
    let body: Function = lua.load(CALL).eval().expect("the body compiles");
    let out: String = body.call((host,)).expect("the call runs");
    assert_eq!(out, "table/close/7");
}

/// …and is wrong inside a coroutine: the value lands on the main state and the
/// Lua sees the call's own argument instead.
#[test]
fn a_captured_handle_loses_its_value_inside_a_thread() {
    let lua = Lua::new();
    let host: Value = captured_host(&lua);
    let body: Function = lua.load(CALL).eval().expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");
    let out: String = thread.resume((host,)).expect("the call runs");
    assert_eq!(
        out, "string/nil/nil",
        "the coroutine receives its own argument instead of the host's table"
    );
}

/// The `create_function` shape survives a coroutine: this is the fix the
/// coroutine-based host calls need (no luaur change required).
#[test]
fn the_calling_state_survives_a_thread() {
    let lua = Lua::new();
    let host = calling_host(&lua);
    let body: Function = lua.load(CALL).eval().expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");
    let out: String = thread.resume((host,)).expect("the call runs");
    assert_eq!(out, "table/close/7");
}

/// A host function that only *reads* (no value built from the state) never had
/// the problem: `signal.aborted()` works inside a tool coroutine.
#[test]
fn a_reading_host_function_survives_a_thread() {
    let lua = Lua::new();
    let host = lua
        .create_function(move |_calling: &Lua, _ignored: f64| Ok::<bool, luaur_rt::Error>(true))
        .expect("the host function is created");
    let body: Function = lua
        .load(r#"return function(host) return tostring(host(1)) end"#)
        .eval()
        .expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");
    let out: String = thread.resume((host,)).expect("the call runs");
    assert_eq!(out, "true");
}
