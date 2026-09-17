//! Minimizing the luaur assertion the fs host-call work hit.
//!
//! Making `pillar.fs.*` yield like `pillar.exec` tripped, with the real
//! extension, `LUAU_ASSERT failed: (*(*to).ci).top.offset_from((*to).top) >= n
//! as isize` in `luaur-vm/src/functions/lua_xmove.rs:17` — i.e. the *resume*
//! found no room on the coroutine's current frame for the values it carries.
//!
//! `vm_resume_probe::a_coroutine_can_be_resumed_many_times` shows twenty plain
//! yield/resume cycles are fine (yield directly in the coroutine body). The real
//! extension yields from a *helper* function, called from the tool, so this test
//! yields from a nested call: `body -> helper -> helper2 -> yield`.
//!
//! Result: **it passes** — twelve cycles through two nested helpers are fine, so
//! neither the cycle count nor the nesting depth is the trigger by itself. The
//! remaining suspects are the *contents* of the real extension's frames (stack
//! pressure: the assertion says the resumed frame had no room for the values the
//! resume carried) and the operations around the yield (the extension formats
//! strings and inserts into tables between its host calls).
//!
//! Keeping the test: it pins what is *not* the bug, so a fork-side fix cannot
//! regress it, and it is the harness for the next minimization round.

use luaur_rt::{Function, Lua, LuaSerdeExt, Value};

fn to_lua(lua: &Lua, json: &serde_json::Value) -> Value {
    lua.to_value(json).expect("a JSON value converts")
}

fn from_lua(lua: &Lua, value: Value) -> serde_json::Value {
    lua.from_value(value).expect("a Lua value converts")
}

/// The extension's shape: the tool calls helpers, a helper yields (that is the
/// host call), and the tool asks several times.
const NESTED: &str = r#"
    -- the "host call" the port generates: it yields inside a helper
    local function host_call(kind, index)
        local answer = coroutine.yield({ kind = kind, index = index })
        return answer
    end

    local function read_notes(path, index)
        return host_call("fs", index)
    end

    local function tidy(index)
        local first = read_notes("docs/TASKS.md", index)
        return first.value or 0
    end

    return function()
        local total = 0
        for index = 1, 12 do
            total = total + tidy(index)
        end
        return total
    end
"#;

#[test]
fn a_yield_from_a_nested_helper_through_many_cycles_is_fine() {
    let lua = Lua::new();
    let body: Function = lua.load(NESTED).eval().expect("the body compiles");
    let thread = lua.create_thread(body).expect("the thread is created");

    let mut yielded = from_lua(&lua, thread.resume(()).expect("the first yield"));
    let mut total = 0i64;
    let mut finished = None;
    for cycle in 1..=12 {
        assert_eq!(yielded["kind"], "fs", "cycle {cycle} yields a host call");
        assert_eq!(yielded["index"], cycle, "cycle {cycle} asks for itself");
        total += cycle;
        let value = from_lua(
            &lua,
            thread
                .resume::<Value>(to_lua(&lua, &serde_json::json!({ "value": cycle })))
                .expect("the coroutine is resumable"),
        );
        if let Some(done) = value.as_i64() {
            finished = Some(done);
            break;
        }
        yielded = value;
    }
    assert_eq!(finished, Some(total), "the tool ran all {total} cycles");
}
