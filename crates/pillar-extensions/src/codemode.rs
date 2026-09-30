use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{FutureExt, StreamExt};
use luaur_rt::{Lua, LuaSerdeExt, ThreadStatus, Value as LuaValue};
use pillar_agent::tool_dispatch::{ToolCallOutcome, ToolExposure, ToolRegistration, ToolRequest};
use pillar_agent::{
    AbortSignal, AgentEvent, AgentEventSink, AgentTool, AgentToolResult, AgentToolUpdateCallback,
    ToolExecuteError,
};
use pillar_ai::types::{Content, Tool, Usage};
use serde_json::{Value, json};

/// Replies of a host-executed batch, in request order.
pub type ExecuteFuture =
    Pin<Box<dyn Future<Output = Result<Vec<ToolCallOutcome>, ToolExecuteError>> + Send>>;
/// An isolated VM body that the host runs on a dedicated execution lane, never inline.
pub type VmBody = Box<dyn FnOnce() + Send>;

/// Explicit capabilities available to generated code.
#[derive(Clone)]
pub struct CodeModeHost {
    /// Currently callable catalog, including undeclared tools.
    pub catalog: Arc<dyn Fn() -> Vec<ToolRegistration> + Send + Sync>,
    /// Execute through the agent pipeline, retaining validation, hooks and cancellation.
    pub execute: Arc<
        dyn Fn(String, Vec<ToolRequest>, AbortSignal, AgentEventSink) -> ExecuteFuture
            + Send
            + Sync,
    >,
    /// Declare discovered tools for subsequent requests; return newly declared names.
    pub activate: Arc<dyn Fn(&[String]) -> Vec<String> + Send + Sync>,
    /// Run the VM on a dedicated execution lane; its thread must never change.
    pub spawn_vm: Arc<dyn Fn(VmBody) -> Result<(), String> + Send + Sync>,
}

const MAX_BYTES: usize = 32 * 1024;
const MAX_CALLS: usize = 256;
const DEADLINE_MS: u64 = 30_000;
const MEMORY_BYTES: usize = 32 * 1024 * 1024;
const STEPS: u64 = 100_000;

#[derive(Default)]
struct Audit {
    calls: Vec<Value>,
    started: std::collections::HashMap<String, Instant>,
    argument_bytes: usize,
    usage: Option<Usage>,
    complete: bool,
    terminate: bool,
}

fn catalog(host: &CodeModeHost) -> Vec<ToolRegistration> {
    (host.catalog)()
        .into_iter()
        .filter(|entry| {
            !matches!(
                entry.exposure,
                ToolExposure::Hidden | ToolExposure::ModelOnly
            )
        })
        .collect()
}

fn describe(entry: &ToolRegistration) -> Value {
    json!({"name": entry.tool.name(), "description": entry.tool.tool.description,
        "namespace": entry.namespace, "inputSchema": entry.tool.tool.parameters, "outputSchema": entry.output_schema})
}

fn search(entries: &[ToolRegistration], query: &str, limit: usize) -> Vec<Value> {
    let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut ranked: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let text = format!(
                "{} {} {}",
                entry.tool.name(),
                entry.tool.tool.description,
                entry.namespace.as_deref().unwrap_or("")
            )
            .to_lowercase();
            let score = words
                .iter()
                .filter(|word| text.contains(word.as_str()))
                .count();
            (words.is_empty() || score > 0).then_some((score, entry))
        })
        .collect();
    ranked.sort_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            .then_with(|| a.tool.name().cmp(b.tool.name()))
    });
    ranked
        .into_iter()
        .take(limit.min(32))
        .map(|(_, entry)| describe(entry))
        .collect()
}

/// Create the orchestrator and discovery tools; hosts register both with `ModelOnly` exposure.
pub fn create_tools(host: CodeModeHost) -> Vec<AgentTool> {
    let search_host = host.clone();
    let discovery = AgentTool {
        tool: Tool {
            name: "tool_search".into(),
            description: "Search callable tools and declare matches for the next model request. Returns exact input and output schemas. Hidden tools are excluded.".into(),
            parameters: json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":32}},"required":["query"],"additionalProperties":false}),
            constrained_sampling: None,
        },
        label: "Tool search".into(), prepare_arguments: None, execution_mode: None,
        execute: Arc::new(move |_, args, signal, _| {
            let host = search_host.clone();
            Box::pin(async move {
                if signal.as_ref().is_some_and(AbortSignal::is_aborted) { return Err(ToolExecuteError("Tool search aborted".into())); }
                let matches = search(&catalog(&host), args["query"].as_str().unwrap_or(""), args["limit"].as_u64().unwrap_or(8) as usize);
                let names: Vec<_> = matches.iter().filter_map(|entry| entry["name"].as_str().map(str::to_owned)).collect();
                let added = (host.activate)(&names);
                Ok(AgentToolResult { content: vec![Content::text(serde_json::to_string(&matches).unwrap_or_default())],
                    structured_content: Some(json!(matches)), added_tool_names: Some(added), ..Default::default() })
            })
        }),
    };
    let codemode = AgentTool {
        tool: Tool {
            name: "codemode".into(),
            description: "Run a Luau code body to orchestrate tools and return only useful data. tools.call(name, arguments) returns {isError, content, structuredContent, value}; value is structured data when available, otherwise text. tools.parallel({{name=..., arguments=...}, ...}) returns ordered replies. Use search_tools(query, limit?) and describe_tool(name) for exact schemas. Return a JSON-compatible value. Check isError before using replies. Calls share normal validation, permissions and cancellation. No filesystem, shell, require, or extension globals are exposed except through tools. Limits: 30 seconds, 32 MiB VM memory, 256 calls, 32 KiB output. Completed effects remain if later work fails; do not automatically retry writes.".into(),
            parameters: json!({"type":"object","properties":{"code":{"type":"string","maxLength":32768}},"required":["code"],"additionalProperties":false}),
            constrained_sampling: None,
        },
        label: "Luau code mode".into(), prepare_arguments: None, execution_mode: None,
        execute: Arc::new(move |id, args, signal, updates| {
            let host = host.clone();
            Box::pin(async move { execute(&host, &id, args["code"].as_str().unwrap_or(""), signal, updates).await })
        }),
    };
    vec![codemode, discovery]
}

struct AbortOnDrop(AbortSignal);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type VmRequest = (
    Vec<ToolRequest>,
    futures::channel::oneshot::Sender<Vec<Value>>,
);

async fn execute(
    host: &CodeModeHost,
    parent: &str,
    source: &str,
    signal: Option<AbortSignal>,
    updates: Option<AgentToolUpdateCallback>,
) -> Result<AgentToolResult, ToolExecuteError> {
    if source.len() > MAX_BYTES {
        return Err(ToolExecuteError("Code mode source exceeds 32 KiB".into()));
    }
    let local = AbortSignal::new();
    let _lifetime = AbortOnDrop(local.clone());
    if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        return Err(ToolExecuteError("Code mode aborted".into()));
    }
    let audit = Arc::new(Mutex::new(Audit {
        complete: true,
        ..Default::default()
    }));
    let (request_tx, mut requests) = futures::channel::mpsc::unbounded::<VmRequest>();
    let (result_tx, result_rx) = futures::channel::oneshot::channel();
    let source = source.to_owned();
    let entries = catalog(host);
    let vm_abort = local.clone();
    // luaur's interrupt callbacks are thread-local, so a VM must not migrate after an await.
    (host.spawn_vm)(Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_vm(&source, entries, vm_abort, request_tx)
        }))
        .unwrap_or_else(|_| Err("Code mode VM panicked".into()));
        let _ = result_tx.send(result);
    }))
    .map_err(ToolExecuteError)?;
    let run = async {
        while let Some((batch, answer)) = requests.next().await {
            let sink = audit_sink(audit.clone(), parent.to_owned(), updates.clone());
            let outcomes = (host.execute)(parent.to_owned(), batch, local.clone(), sink)
                .await
                .map_err(|e| e.to_string())?;
            if outcomes.iter().any(|outcome| outcome.result.terminate) {
                audit.lock().unwrap_or_else(|p| p.into_inner()).terminate = true;
                return Err("A child tool requested termination".to_owned());
            }
            let replies = outcomes.into_iter().map(|outcome| {
                let value = outcome.result.structured_content.clone().unwrap_or_else(|| json!(outcome.result.content.iter().filter_map(|block| match block { Content::Text { text, .. } => Some(text.as_str()), _ => None }).collect::<Vec<_>>().join("\n")));
                json!({"isError":outcome.is_error,"content":outcome.result.content,"structuredContent":outcome.result.structured_content,"value":value})
            }).collect();
            answer
                .send(replies)
                .map_err(|_| "Code mode VM closed".to_string())?;
        }
        result_rx
            .await
            .map_err(|_| "Code mode VM closed".to_string())?
    };
    let caller_abort = async {
        match signal {
            Some(signal) => signal.aborted().await,
            None => std::future::pending::<()>().await,
        }
    };
    let deadline = pillar_ai::clock::timeout(Duration::from_millis(DEADLINE_MS), run);
    let result = {
        futures::pin_mut!(deadline, caller_abort);
        futures::select! {
            result = deadline.fuse() => result.map_err(|_| "Code mode deadline exceeded".to_string()).and_then(|result| result),
            () = caller_abort.fuse() => Err("Code mode aborted".into()),
        }
    };
    local.abort();
    let mut audit = audit.lock().unwrap_or_else(|p| p.into_inner());
    for call in &mut audit.calls {
        if call["status"] == "running" {
            call["status"] = json!("outcome_unknown");
        }
    }
    let (value, error) = match result {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    };
    let rendered = match &value {
        Some(value) => serde_json::to_string(value).map_err(|e| ToolExecuteError(e.to_string()))?,
        None => error.clone().unwrap_or_default(),
    };
    let (content, value, error) = if rendered.len() > MAX_BYTES {
        (
            "Code mode output exceeds 32 KiB; filter data before returning it".to_owned(),
            None,
            Some("output_limit".to_owned()),
        )
    } else {
        (rendered, value, error)
    };
    Ok(AgentToolResult {
        content: vec![Content::text(content)],
        structured_content: value,
        is_error: error.is_some(),
        details: json!({"nestedCalls": {"calls": audit.calls, "complete": audit.complete}, "error": error}),
        usage: audit.usage.clone(),
        terminate: audit.terminate,
        ..Default::default()
    })
}

fn audit_sink(
    audit: Arc<Mutex<Audit>>,
    parent: String,
    updates: Option<AgentToolUpdateCallback>,
) -> AgentEventSink {
    AgentEventSink::new(move |event| {
        let mut audit = audit.lock().unwrap_or_else(|p| p.into_inner());
        let mut progress = json!({"parentToolCallId":parent,"type":event.kind()});
        match event {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let bytes = serde_json::to_vec(&args)
                    .map(|v| v.len())
                    .unwrap_or(MAX_BYTES + 1);
                let keep = bytes <= 8 * 1024 && audit.argument_bytes + bytes <= MAX_BYTES;
                let mut record = json!({"toolCallId":tool_call_id,"parentToolCallId":parent,"name":tool_name,"status":"running"});
                if keep {
                    record["arguments"] = args;
                    audit.argument_bytes += bytes;
                } else {
                    audit.complete = false;
                }
                audit.started.insert(tool_call_id.clone(), Instant::now());
                audit.calls.push(record);
                progress["toolCallId"] = json!(tool_call_id);
                progress["name"] = json!(tool_name);
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                is_error,
                result,
                ..
            } => {
                let elapsed = audit
                    .started
                    .remove(&tool_call_id)
                    .map(|t| t.elapsed().as_millis() as u64);
                if let Some(call) = audit
                    .calls
                    .iter_mut()
                    .find(|call| call["toolCallId"] == tool_call_id)
                {
                    call["status"] = json!(if is_error { "error" } else { "ok" });
                    call["durationMs"] = json!(elapsed);
                }
                if let Some(usage) = result
                    .get("usage")
                    .and_then(|v| serde_json::from_value::<Usage>(v.clone()).ok())
                {
                    add_usage(&mut audit.usage, &usage);
                }
                progress["toolCallId"] = json!(tool_call_id);
                progress["isError"] = json!(is_error);
            }
            _ => {}
        }
        drop(audit);
        if let Some(update) = &updates {
            update(AgentToolResult {
                details: progress,
                ..Default::default()
            });
        }
        Box::pin(async {})
    })
}

fn run_vm(
    source: &str,
    entries: Vec<ToolRegistration>,
    signal: AbortSignal,
    requests: futures::channel::mpsc::UnboundedSender<VmRequest>,
) -> Result<Value, String> {
    let lua = Lua::new();
    lua.set_memory_limit(MEMORY_BYTES)
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let remaining = Arc::new(std::sync::atomic::AtomicU64::new(STEPS));
    let interrupted = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let interruption = interrupted.clone();
    lua.set_interrupt(move |_| {
        let reason =
            if signal.is_aborted() || started.elapsed() >= Duration::from_millis(DEADLINE_MS) {
                1
            } else if remaining
                .fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |n| n.checked_sub(1),
                )
                .is_err()
            {
                2
            } else {
                0
            };
        if reason != 0 {
            interruption.store(reason, std::sync::atomic::Ordering::Relaxed);
            // Yielding escapes pcall; a catchable Lua error could let a script ignore cancellation.
            Ok(luaur_rt::VmState::Yield)
        } else {
            Ok(luaur_rt::VmState::Continue)
        }
    });
    let environment = lua.create_table_result().map_err(|e| e.to_string())?;
    for name in [
        "assert", "error", "ipairs", "pairs", "next", "pcall", "xpcall", "select", "tonumber",
        "tostring", "type", "typeof", "math", "string", "table", "utf8", "bit32",
    ] {
        environment
            .set(
                name,
                lua.globals()
                    .get::<LuaValue>(name)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
    }
    let tools: luaur_rt::Table = lua
        .load(
            r#"
        local yield = coroutine.yield
        return {
            call = function(name, arguments)
                return yield({requests = {{name = name, arguments = arguments or {}}}})[1]
            end,
            parallel = function(requests) return yield({requests = requests}) end,
        }
    "#,
        )
        .eval()
        .map_err(|e| e.to_string())?;
    environment.set("tools", tools).map_err(|e| e.to_string())?;
    let search_entries = entries.clone();
    let find = lua
        .create_function(move |lua: &Lua, (query, limit): (String, Option<usize>)| {
            lua.to_value(&search(&search_entries, &query, limit.unwrap_or(8)))
        })
        .map_err(|e| e.to_string())?;
    environment
        .set("search_tools", find)
        .map_err(|e| e.to_string())?;
    let describe = lua
        .create_function(move |lua: &Lua, name: String| {
            match entries.iter().find(|entry| entry.tool.name() == name) {
                Some(entry) => lua.to_value(&describe(entry)),
                None => Ok(LuaValue::Nil),
            }
        })
        .map_err(|e| e.to_string())?;
    environment
        .set("describe_tool", describe)
        .map_err(|e| e.to_string())?;
    lua.sandbox(true).map_err(|e| e.to_string())?;
    let body = lua
        .load(source)
        .set_environment(environment)
        .into_function()
        .map_err(|e| e.to_string())?;
    let thread = lua.create_thread(body).map_err(|e| e.to_string())?;
    let mut answer = LuaValue::Nil;
    let mut count = 0;
    loop {
        let resumed = thread.resume::<LuaValue>(answer);
        match interrupted.load(std::sync::atomic::Ordering::Relaxed) {
            1 => return Err("Code mode aborted or deadline exceeded".into()),
            2 => return Err("Code mode exceeded its instruction budget".into()),
            _ => {}
        }
        let value = resumed.map_err(|e| e.to_string())?;
        let value: Value = lua.from_value(value).map_err(|e| e.to_string())?;
        if thread.status() == ThreadStatus::Finished {
            return Ok(value);
        }
        let batch: Vec<ToolRequest> = serde_json::from_value(
            value
                .get("requests")
                .cloned()
                .ok_or("Invalid code mode suspension")?,
        )
        .map_err(|e| e.to_string())?;
        count += batch.len();
        if count > MAX_CALLS {
            return Err("Code mode exceeded its tool call budget".into());
        }
        if serde_json::to_vec(&batch).map_err(|e| e.to_string())?.len() > MAX_BYTES {
            return Err("Code mode request exceeds 32 KiB".into());
        }
        let (answer_tx, answer_rx) = futures::channel::oneshot::channel();
        requests
            .unbounded_send((batch, answer_tx))
            .map_err(|_| "Code mode host closed".to_string())?;
        let replies = futures::executor::block_on(answer_rx)
            .map_err(|_| "Code mode host closed".to_string())?;
        answer = lua.to_value(&replies).map_err(|e| e.to_string())?;
    }
}

fn add_usage(total: &mut Option<Usage>, usage: &Usage) {
    let total = total.get_or_insert_with(Usage::default);
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.total_tokens += usage.total_tokens;
    if let Some(n) = usage.cache_write_1h {
        total.cache_write_1h = Some(total.cache_write_1h.unwrap_or(0) + n);
    }
    if let Some(n) = usage.reasoning {
        total.reasoning = Some(total.reasoning.unwrap_or(0) + n);
    }
    total.cost.input += usage.cost.input;
    total.cost.output += usage.cost.output;
    total.cost.cache_read += usage.cost.cache_read;
    total.cost.cache_write += usage.cost.cache_write;
    total.cost.total += usage.cost.total;
}
