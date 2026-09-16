//! A Luau tool's abort signal and progress callback reach the host
//! (docs/DEVELOPMENT-STRATEGY.md §8-1): the extension passes
//! `pillar.exec(..., { signal = signal })`, an aborted turn kills the running
//! command instead of waiting for it, and `on_update` reaches the agent's
//! update sink.

#![cfg(feature = "luau")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_agent::abort::AbortSignal;
use pillar_agent::types::{AgentToolResult, AgentToolUpdateCallback};
use pillar_cli::effects::EffectBroker;
use pillar_extensions::bridge::bridge_to_agent_tools;
use pillar_extensions::runtime::ExtensionRuntime;

const SLOW_TOOL: &str = r#"
--!strict
local pillar = require("@pillar")

pillar.register_tool({
  name = "slow_build",
  description = "runs a slow command",
  parameters = pillar.schema.object({}),
  execute = function(tool_call_id, params, signal, on_update, ctx)
    on_update({ content = { { type = "text", text = "building" } } })
    local result = pillar.exec(
      "sh",
      { "-c", "printf start; sleep 30; printf end" },
      { signal = signal }
    )
    return {
      content = { { type = "text", text = "killed=" .. tostring(result.killed) .. " out=" .. result.stdout } },
      details = {
        aborted = signal.aborted(),
        signal_type = type(signal),
        code = result.code,
      },
    }
  end,
})

return nil
"#;

fn runtime_with_exec() -> Arc<Mutex<ExtensionRuntime>> {
    let runtime = Arc::new(Mutex::new(ExtensionRuntime::new()));
    let broker = EffectBroker::permissive();
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    runtime
        .lock()
        .unwrap()
        .set_exec_host(Arc::new(move |command, args, options| {
            broker.exec(&cwd, command, args, options)
        }));
    runtime
}

#[tokio::test]
async fn an_aborted_tool_command_is_killed_and_reports_progress() {
    let runtime = runtime_with_exec();
    runtime
        .lock()
        .unwrap()
        .load_extension("slow.luau", SLOW_TOOL)
        .expect("the extension loads");
    let tools = bridge_to_agent_tools(&runtime);
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "slow_build")
        .expect("the tool is bridged");

    let signal = AbortSignal::new();
    let killer = signal.clone();
    // The abort comes from the UI's thread while the tool's thread is blocked
    // in the command (upstream: the pump aborts the run).
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        killer.abort();
    });

    let updates: Arc<Mutex<Vec<AgentToolResult>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&updates);
    let on_update: AgentToolUpdateCallback = Arc::new(move |result| {
        sink.lock().unwrap().push(result);
    });

    let started = Instant::now();
    let result = (tool.execute)(
        "call-1".to_string(),
        serde_json::json!({}),
        Some(signal),
        Some(on_update),
    )
    .await
    .expect("the tool returns after the abort");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the aborted command returned promptly: {:?}",
        started.elapsed()
    );

    let delivered = updates.lock().unwrap();
    assert_eq!(delivered.len(), 1, "on_update reached the agent");
    assert_eq!(delivered[0].details, serde_json::Value::Null);

    let details = &result.details;
    assert_eq!(details["signal_type"], serde_json::json!("table"));
    assert_eq!(details["aborted"], serde_json::json!(true));
    let text = serde_json::to_value(&result.content).unwrap().to_string();
    assert!(
        text.contains("killed=true"),
        "the extension saw the kill: {text}"
    );
    assert!(
        text.contains("out=start"),
        "the output captured before the kill is kept: {text}"
    );
}
