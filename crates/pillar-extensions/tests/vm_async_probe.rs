//! Probe: luaur's async surface for waiting host calls (the `async` feature).
//!
//! `vm_resume_probe.rs` shows the *manual* protocol (the wrapper yields and the
//! host resumes it). luaur-rt also ships mlua's async model, which is the shape
//! the port should adopt: a host callback is an `async fn`, the Lua coroutine
//! suspends while the future is pending, and the *host's* executor (tokio in the
//! CLI, a game loop in an engine) drives it. luaur needs no runtime of its own.
//!
//! What this establishes for the redesign (TASKS: "非同期 host 呼出の実装"):
//!
//! - an async host function suspends a tool's Lua and resumes with a value the
//!   host produced on **another thread** (the extension's thread is not the one
//!   waiting),
//! - the caller drives it with any executor (`futures::executor::block_on` here;
//!   the CLI would pass tokio),
//! - two waiting calls in one tool body work in sequence,
//! - a host failure travels as a *result* the wrapper raises: luaur 0.1.8 parks
//!   the coroutine when the async future itself resolves to `Err` (and when the
//!   argument conversion fails), so the port must not rely on that path.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use luaur_rt::{Function, Lua, LuaSerdeExt};

/// A one-shot slot the host fills when its work is done. The waker is what makes
/// the host's executor (not a thread parked in the VM) do the waiting.
#[derive(Default)]
struct Slot {
    value: Mutex<Option<serde_json::Value>>,
    waker: Mutex<Option<Waker>>,
}

impl Slot {
    fn complete(&self, value: serde_json::Value) {
        *self.value.lock().unwrap() = Some(value);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }
}

struct SlotFuture {
    slot: Arc<Slot>,
}

impl Future for SlotFuture {
    type Output = serde_json::Value;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<serde_json::Value> {
        // Register first, then check: the host may complete the slot between
        // the two, and checking before registering would lose that wakeup (the
        // coroutine would park forever).
        *self.slot.waker.lock().unwrap() = Some(context.waker().clone());
        if let Some(value) = self.slot.value.lock().unwrap().take() {
            self.slot.waker.lock().unwrap().take();
            return Poll::Ready(value);
        }
        Poll::Pending
    }
}

/// The tool body: two host calls, each of which suspends the coroutine.
const TOOL: &str = r#"
    return function(host_exec)
        local first = host_exec("make", { "story" })
        local second = host_exec("make", { "preview" })
        return first.stdout .. "+" .. second.stdout
    end
"#;

#[test]
fn an_async_host_call_suspends_the_tool_until_the_host_answers() {
    let lua = Lua::new();
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let slot: Arc<Slot> = Arc::new(Slot::default());
    let (requests, answers) = std::sync::mpsc::channel::<String>();
    let tool_slot = Arc::clone(&slot);

    let host_exec = lua
        .create_async_function(move |lua: Lua, (command, args): (String, Vec<String>)| {
            let requests = requests.clone();
            let slot = Arc::clone(&tool_slot);
            async move {
                // Tell the host what was asked, then suspend until it answers.
                requests
                    .send(format!("{command} {args:?}"))
                    .expect("the host listens");
                let value = SlotFuture { slot }.await;
                lua.to_value(&value)
            }
        })
        .expect("the async host function is created");

    // The host: it answers each request from its own thread, so the extension's
    // thread is free while the Lua is suspended.
    let host = std::thread::spawn({
        let calls = Arc::clone(&calls);
        let slot = Arc::clone(&slot);
        move || {
            for answer in ["one", "two"] {
                let request = answers.recv().expect("the tool asked for something");
                calls.lock().unwrap().push(request);
                std::thread::sleep(std::time::Duration::from_millis(20));
                slot.complete(serde_json::json!({ "stdout": answer, "code": 0 }));
            }
        }
    });

    let body: Function = lua.load(TOOL).eval().expect("the tool compiles");
    let result: String = futures::executor::block_on(body.call_async((host_exec,)))
        .expect("the tool finishes once the host answered");

    host.join().unwrap();
    assert_eq!(result, "one+two");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[
            "make [\"story\"]".to_string(),
            "make [\"preview\"]".to_string()
        ]
    );
}

#[test]
fn an_async_host_error_surfaces_as_a_lua_error() {
    let lua = Lua::new();
    let (requests, answers) = std::sync::mpsc::channel::<String>();
    let slot: Arc<Slot> = Arc::new(Slot::default());
    let tool_slot = Arc::clone(&slot);
    let host_exec = lua
        .create_async_function(move |lua: Lua, (command, _args): (String, Vec<String>)| {
            let requests = requests.clone();
            let slot = Arc::clone(&tool_slot);
            async move {
                requests.send(command).expect("the host listens");
                // The host's failure arrives when its work finishes, so the
                // coroutine is suspended while it happens. It travels as a
                // *result*: an `Err` from the future parks the coroutine in
                // luaur 0.1.8 (see the note below), so the wrapper — which the
                // port generates anyway — raises it.
                let value = SlotFuture { slot }.await;
                lua.to_value(&value)
            }
        })
        .expect("the async host function is created");
    let host = std::thread::spawn({
        let slot = Arc::clone(&slot);
        move || {
            let _ = answers.recv();
            std::thread::sleep(std::time::Duration::from_millis(20));
            slot.complete(serde_json::json!({ "error": "exec host failed" }));
        }
    });

    let body: Function = lua
        .load(
            r#"
            return function(host_exec)
                local result = host_exec("make", {})
                if result.error then error(result.error) end
                return result.stdout
            end
            "#,
        )
        .eval()
        .expect("the tool compiles");
    let error = futures::executor::block_on(body.call_async::<String>((host_exec,)))
        .expect_err("the host error reaches the caller");
    host.join().unwrap();
    assert!(
        error.to_string().contains("exec host failed"),
        "the failure names the host error: {error}"
    );
}
