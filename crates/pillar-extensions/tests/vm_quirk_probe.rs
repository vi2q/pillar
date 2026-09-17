//! Probe: does a Rust host function's return value reach Lua inside a
//! coroutine?
//!
//! This is the blocker for the cooperative/async host-call work (TASKS: 非同期
//! host 呼出の実装). Both designs suspend a **tool call** in a coroutine
//! (`Thread`) so the host can do the work without holding the runtime lock — but
//! an extension's tool almost always calls Rust-backed host functions
//! (`ctx.ui.*`, `pillar.fs.*`, `pillar.get_*`, `custom.next`, …), and with luaur
//! 0.1.8 those calls lose their return value *inside a coroutine*:
//!
//! - on the main state the value arrives (first test),
//! - inside a `Thread` the Lua sees the call's **first argument** instead of the
//!   value the host produced (second test), whatever the argument's type.
//!
//! The second test is `#[ignore]`d and asserts the *desired* behaviour, so a
//! luaur release that fixes this turns it green; until then the runtime keeps
//! calling tools on the main state (where waiting host calls run inline), which
//! is why the WIP state machine is parked on the
//! `wip/luau-async-state-machine` branch instead of merged.

use luaur_rt::{Function, Lua, LuaSerdeExt, Value};

/// A Rust host function returning a table (the shape `ctx.ui.*` / `pillar.fs.*`
/// have). [`Function::wrap`] answers an `IntoLua`, so each caller converts it.
fn table_host(lua: &Lua) -> Value {
    use luaur_rt::IntoLua;
    let table_lua = lua.clone();
    let host = Function::wrap(move |id: String| {
        table_lua
            .to_value(&serde_json::json!({ "kind": "close", "id": id }))
            .map_err(luaur_rt::Error::external)
    });
    host.into_lua(lua).expect("the host function converts")
}

/// The supported shape today: a Rust host function answers normally when it is
/// called from the main state.
#[test]
fn a_rust_function_returns_its_table_on_the_main_state() {
    let lua = Lua::new();
    let host = table_host(&lua);
    let out: String = lua
        .load(
            r#"
            local host = ...
            local event = host("7")
            return type(event) .. "/" .. tostring(event.kind) .. "/" .. tostring(event.id)
            "#,
        )
        .call::<String>(host)
        .expect("the call runs");
    assert_eq!(out, "table/close/7");
}

/// The desired shape inside a coroutine — the shape the cooperative protocol
/// needs. Ignored while luaur 0.1.8 returns nothing (or the argument) instead.
#[test]
#[ignore = "luaur 0.1.8: a Rust host function called inside a coroutine does not deliver its return value"]
fn a_rust_function_returns_its_table_inside_a_thread() {
    let lua = Lua::new();
    let host = table_host(&lua);
    let body: Function = lua
        .load(
            r#"
            return function(host)
                local event = host("7")
                return type(event) .. "/" .. tostring(event.kind) .. "/" .. tostring(event.id)
            end
            "#,
        )
        .eval()
        .expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");
    let out: String = thread.resume((host,)).expect("the call runs");
    assert_eq!(out, "table/close/7");
}

/// The current (broken) behaviour, kept as evidence with the exact shapes: the
/// call answers its own first argument, whatever its type.
#[test]
fn a_rust_function_loses_its_value_inside_a_thread_today() {
    let lua = Lua::new();
    let host = table_host(&lua);
    let body: Function = lua
        .load(
            r#"
            return function(host)
                local event = host("7")
                return type(event) .. "/" .. tostring(event)
            end
            "#,
        )
        .eval()
        .expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");
    let out: String = thread.resume((host,)).expect("the call runs");
    assert_eq!(
        out, "string/7",
        "luaur 0.1.8: the host function's table is replaced by its argument"
    );

    // The same call with a numeric argument returns the argument itself, which
    // is what surfaced as `attempt to index number with 'kind'` in the UI loop.
    let number_lua = lua.clone();
    let numeric: Value = {
        use luaur_rt::IntoLua;
        Function::wrap(move |id: f64| {
            number_lua
                .to_value(&serde_json::json!({ "kind": "close", "id": id }))
                .map_err(luaur_rt::Error::external)
        })
        .into_lua(&lua)
        .expect("the host function converts")
    };
    let body: Function = lua
        .load(
            r#"
            return function(host)
                return type(host(7)) .. ":" .. tostring(host(7))
            end
            "#,
        )
        .eval()
        .expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");
    let out: String = thread.resume((numeric,)).expect("the call runs");
    assert_eq!(out, "number:7");
}

/// A `bool`-returning host function is the one shape that *does* survive a
/// coroutine today, which is why `signal.aborted()` works inside a tool while
/// `ctx.ui.*` does not.
#[test]
fn a_bool_returning_host_function_survives_a_thread() {
    let lua = Lua::new();
    let host: Value = {
        use luaur_rt::IntoLua;
        Function::wrap(|_ignored: f64| Ok::<bool, luaur_rt::Error>(true))
            .into_lua(&lua)
            .expect("the host function converts")
    };
    let body: Function = lua
        .load(
            r#"
            return function(host)
                return tostring(host(1))
            end
            "#,
        )
        .eval()
        .expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");
    let out: String = thread.resume((host,)).expect("the call runs");
    assert_eq!(out, "true");
}

