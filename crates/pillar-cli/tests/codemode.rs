#![cfg(feature = "luau")]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pillar_agent::tool_dispatch::{ToolExposure, ToolRegistration};
use pillar_agent::{
    Agent, AgentMessage, AgentOptions, AgentState, AgentTool, AgentToolResult, StreamFn,
    ToolExecutionMode,
};
use pillar_ai::event_stream::assistant_message_event_stream;
use pillar_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Context, Message, StopReason, Tool,
    ToolResultMessage, Usage,
};
use pillar_cli::codemode::{bind_tools, tools_for_slot};
use pillar_cli::runner::{SessionSlot, bind_session, build_extension_runner};
use pillar_coding_agent::core::agent_session_class::{AgentSession, AgentSessionConfig};
use pillar_coding_agent::core::effects::{EffectAuthorizer, EffectDecision, EffectIntent};
use pillar_coding_agent::core::extensions_runner::{
    ExtensionHandler, ExtensionRunner, HostExtension,
};
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pillar_coding_agent::core::resource_loader::{ResourceLoader, ResourceLoaderOptions};
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use serde_json::{Value, json};

struct Fixture {
    session: Arc<AgentSession>,
    answers: Arc<Mutex<VecDeque<Vec<Content>>>>,
    contexts: Arc<Mutex<Vec<Context>>>,
    directory: std::path::PathBuf,
}

impl Fixture {
    fn new(
        tools: Vec<AgentTool>,
        handlers: BTreeMap<String, Vec<ExtensionHandler>>,
        authorizer: Option<EffectAuthorizer>,
        excluded: &[&str],
    ) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "pillar-code-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let cwd = directory.to_string_lossy().to_string();
        let slot: SessionSlot = Arc::new(Mutex::new(None));
        let answers = Arc::new(Mutex::new(VecDeque::<Vec<Content>>::new()));
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let stream_fn = {
            let answers = answers.clone();
            let contexts = contexts.clone();
            StreamFn::new(move |context, _| {
                let answers = answers.clone();
                let contexts = contexts.clone();
                async move {
                    contexts.lock().unwrap().push(context);
                    let content = answers
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected model roundtrip");
                    let stop_reason = if content
                        .iter()
                        .any(|part| matches!(part, Content::ToolCall { .. }))
                    {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    };
                    let stream = assistant_message_event_stream();
                    stream.push(AssistantMessageEvent::Done {
                        reason: stop_reason,
                        message: AssistantMessage {
                            content,
                            api: "host".into(),
                            provider: "host".into(),
                            model: "host".into(),
                            response_model: None,
                            response_id: None,
                            diagnostics: Vec::new(),
                            usage: Usage::default(),
                            stop_reason,
                            deferred: None,
                            error_message: None,
                            raw_stop_reason: None,
                            end_turn: None,
                            timestamp: 1,
                        },
                    });
                    stream
                }
            })
        };
        let mut all_tools = tools;
        all_tools.extend(tools_for_slot(&slot));
        let mut options = AgentOptions::new(stream_fn);
        options.initial_state = Some(AgentState {
            tools: all_tools,
            ..Default::default()
        });
        let agent = Arc::new(Agent::new(options));
        let settings = Arc::new(Mutex::new(SettingsManager::in_memory(
            json!({}),
            SettingsManagerCreateOptions {
                project_trusted: Some(false),
            },
        )));
        let loader = Arc::new(Mutex::new(ResourceLoader::new(
            &cwd,
            ResourceLoaderOptions {
                agent_dir: cwd.clone(),
                no_skills: true,
                no_prompt_templates: true,
                no_themes: true,
                no_context_files: true,
                ..Default::default()
            },
            settings.clone(),
        )));
        let runtime = Arc::new(
            ModelRuntime::new(CreateModelRuntimeOptions {
                auth_path: Some(directory.join("auth.json")),
                models_path: Some(directory.join("models.json")),
                models_store_path: Some(directory.join("models-store.json")),
                ..Default::default()
            })
            .unwrap(),
        );
        let runner = ExtensionRunner::new(vec![HostExtension {
            path: "test".into(),
            handlers,
            commands: Vec::new(),
            tools: BTreeMap::new(),
            flags: BTreeMap::new(),
            shortcuts: BTreeMap::new(),
            message_renderers: BTreeMap::new(),
            entry_renderers: BTreeMap::new(),
            markdown_transformer: None,
        }]);
        let mut config = AgentSessionConfig::new(
            agent,
            Arc::new(Mutex::new(SessionManager::in_memory(&cwd, None).unwrap())),
            settings,
            cwd,
            loader,
            runtime,
            Arc::new(Mutex::new(runner)),
        );
        config.effect_authorizer = authorizer;
        config.excluded_tool_names = Some(
            excluded
                .iter()
                .map(|name| name.to_string())
                .collect::<BTreeSet<_>>(),
        );
        let session = Arc::new(AgentSession::new(config));
        bind_session(&slot, &session);
        bind_tools(&session);
        session.install_tool_hooks();
        Self {
            session,
            answers,
            contexts,
            directory,
        }
    }

    fn queue(&self, name: &str, arguments: Value) {
        self.answers.lock().unwrap().extend([
            vec![Content::tool_call("parent", name, arguments)],
            vec![Content::text("done")],
        ]);
    }

    async fn run(&self, code: &str) -> ToolResultMessage {
        self.queue("codemode", json!({"code":code}));
        tokio::time::timeout(Duration::from_secs(10), self.session.agent().prompt("test"))
            .await
            .expect("turn stalled")
            .unwrap();
        self.last_result()
    }

    fn last_result(&self) -> ToolResultMessage {
        self.session
            .state()
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                AgentMessage::Message(Message::ToolResult(result)) => Some((**result).clone()),
                _ => None,
            })
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.session.dispose();
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

fn data_tool(name: &str, calls: Arc<AtomicUsize>, data: Value, is_error: bool) -> AgentTool {
    AgentTool {
        tool: Tool {
            name: name.into(),
            description: format!("Read {name} data"),
            parameters: json!({"type":"object","properties":{"n":{"type":"integer"}},"required":["n"],"additionalProperties":false}),
            constrained_sampling: None,
        },
        label: name.into(),
        prepare_arguments: None,
        execution_mode: None,
        execute: Arc::new(move |_, args, _, _| {
            let calls = calls.clone();
            let data = data.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(AgentToolResult {
                    content: vec![Content::text("large unneeded text")],
                    structured_content: Some(json!({"n":args["n"],"data":data})),
                    is_error,
                    usage: Some(Usage {
                        input: 2,
                        total_tokens: 2,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            })
        }),
    }
}

fn value(result: &ToolResultMessage) -> Value {
    assert!(!result.is_error, "{:?}", result.content);
    serde_json::from_str(result.content[0].as_text().unwrap()).unwrap()
}

#[tokio::test]
async fn one_model_roundtrip_orchestrates_parallel_and_dependent_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut tool = data_tool(
        "read",
        calls.clone(),
        json!({"keep":42,"secret":"discard"}),
        false,
    );
    let execute = tool.execute.clone();
    let peak_copy = peak.clone();
    tool.execute = Arc::new(move |id, args, signal, updates| {
        let active = active.clone();
        let peak = peak_copy.clone();
        let execute = execute.clone();
        Box::pin(async move {
            peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            let result = execute(id, args, signal, updates).await;
            active.fetch_sub(1, Ordering::SeqCst);
            result
        })
    });
    let fixture = Fixture::new(vec![tool], BTreeMap::new(), None, &[]);
    let result = fixture.run(r#"
        local replies = tools.parallel({{name="read",arguments={n=1}},{name="read",arguments={n=2}}})
        assert(not replies[1].isError and not replies[2].isError)
        local next = tools.call("read", {n=replies[1].value.n + replies[2].value.n})
        return {sum=next.value.n, keep=next.value.data.keep}
    "#).await;
    assert_eq!(value(&result), json!({"sum":3,"keep":42}));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(result.usage.as_ref().unwrap().total_tokens, 6);
    let audit = &result.details.as_ref().unwrap()["nestedCalls"];
    assert_eq!(audit["calls"].as_array().unwrap().len(), 3);
    assert_eq!(audit["complete"], true);
    for call in audit["calls"].as_array().unwrap() {
        assert_eq!(call["parentToolCallId"], "parent");
        assert_eq!(call["status"], "ok");
    }
    let contexts = fixture.contexts.lock().unwrap();
    assert_eq!(contexts.len(), 2);
    let results: Vec<_> = contexts[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool_call_id, "parent");
    assert!(!results[0].content[0].as_text().unwrap().contains("discard"));
    let saved: Vec<_> = fixture
        .session
        .session_manager()
        .lock()
        .unwrap()
        .get_entries_owned()
        .iter()
        .map(pillar_coding_agent::core::session_manager::entry_to_json)
        .collect();
    assert!(saved.iter().any(|entry| {
        entry["message"]["details"]["nestedCalls"]["calls"]
            .as_array()
            .is_some_and(|calls| calls.len() == 3)
    }));
}

#[tokio::test]
async fn child_dispatch_does_not_wait_for_parent_event_consumption() {
    let started = Arc::new(tokio::sync::Notify::new());
    let agent_slot = Arc::new(Mutex::new(std::sync::Weak::<AgentSession>::new()));
    let mut tool = data_tool("read", Arc::new(AtomicUsize::new(0)), json!(42), false);
    let execute = tool.execute.clone();
    let notification = started.clone();
    let slot = agent_slot.clone();
    tool.execute = Arc::new(move |id, args, signal, updates| {
        let agent = slot.lock().unwrap().upgrade().unwrap();
        assert!(agent.agent().state().pending_tool_calls.is_empty());
        notification.notify_one();
        execute(id, args, signal, updates)
    });
    let fixture = Fixture::new(vec![tool], BTreeMap::new(), None, &[]);
    *agent_slot.lock().unwrap() = Arc::downgrade(&fixture.session);
    let unsubscribe = fixture.session.agent().subscribe(move |event, _| {
        let started = started.clone();
        Box::pin(async move {
            if matches!(event, pillar_agent::AgentEvent::MessageEnd { message }
                if matches!(message.as_ref(), AgentMessage::Message(Message::Assistant(assistant)) if assistant.stop_reason == StopReason::ToolUse)) {
                started.notified().await;
            }
        })
    });
    assert_eq!(
        value(&fixture.run("return tools.call('read',{n=1}).value.n").await),
        1
    );
    unsubscribe();
}

#[tokio::test]
async fn validation_authorization_and_exposure_prevent_child_effects() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate_seen = Arc::new(Mutex::new(Vec::new()));
    let gate = gate_seen.clone();
    let fixture = Fixture::new(
        vec![
            data_tool("read", calls.clone(), json!(42), false),
            data_tool("deny", calls.clone(), json!(42), false),
        ],
        BTreeMap::new(),
        Some(Arc::new(move |intent| {
            gate.lock().unwrap().push(intent.clone());
            if matches!(intent, EffectIntent::ToolCall { name, .. } if name == "deny") {
                EffectDecision::Deny {
                    reason: "denied".into(),
                }
            } else {
                EffectDecision::Allow
            }
        })),
        &[],
    );
    let mut hidden = ToolRegistration::direct(data_tool("hidden", calls.clone(), json!(42), false));
    hidden.exposure = ToolExposure::Hidden;
    fixture.session.agent().register_tool(hidden);
    let result = fixture
        .run(
            r#"
        assert(tools.call("read",{n="bad"}).isError)
        assert(tools.call("deny",{n=1}).isError)
        assert(tools.call("hidden",{n=1}).isError)
        assert(tools.call("codemode",{code="return 1"}).isError)
        assert(describe_tool("hidden") == nil)
        return "blocked"
    "#,
        )
        .await;
    assert_eq!(value(&result), "blocked");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        gate_seen
            .lock()
            .unwrap()
            .iter()
            .any(|intent| matches!(intent, EffectIntent::ToolCall {name,..} if name == "deny"))
    );
}

#[tokio::test]
async fn result_hooks_cannot_leak_original_structured_data() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let before_seen = seen.clone();
    let after_seen = seen.clone();
    let handlers: BTreeMap<String, Vec<ExtensionHandler>> = BTreeMap::from([
        (
            "tool_call".into(),
            vec![Arc::new(move |event: &Value| {
                before_seen.lock().unwrap().push(event.clone());
                Ok(None)
            }) as ExtensionHandler],
        ),
        (
            "tool_result".into(),
            vec![Arc::new(move |event: &Value| {
                after_seen.lock().unwrap().push(event.clone());
                Ok((event["toolName"] == "read")
                    .then(|| json!({"content":[{"type":"text","text":"redacted"}]})))
            }) as ExtensionHandler],
        ),
    ]);
    let fixture = Fixture::new(
        vec![data_tool(
            "read",
            Arc::new(AtomicUsize::new(0)),
            json!("secret"),
            false,
        )],
        handlers,
        None,
        &[],
    );
    let result = fixture
        .run("local r = tools.call('read',{n=1}); return r.value")
        .await;
    assert_eq!(value(&result), "redacted");
    assert_eq!(result.usage.unwrap().total_tokens, 2);
    let seen = seen.lock().unwrap();
    for event in seen.iter().filter(|event| event["toolName"] == "read") {
        assert_eq!(event["parentToolCallId"], "parent");
    }
    assert_eq!(
        seen.iter()
            .filter(|event| event["toolName"] == "read")
            .count(),
        2
    );
}

#[tokio::test]
async fn typed_failures_and_later_script_errors_preserve_completed_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::new(
        vec![data_tool(
            "fail",
            calls.clone(),
            json!({"reason":"retry_later"}),
            true,
        )],
        BTreeMap::new(),
        None,
        &[],
    );
    let result = fixture
        .run("local r=tools.call('fail',{n=1}); assert(r.isError); return r.value.data.reason")
        .await;
    assert_eq!(value(&result), "retry_later");
    let result = fixture
        .run("tools.call('fail',{n=1}); error('later failure')")
        .await;
    assert!(result.is_error);
    assert_eq!(result.usage.unwrap().total_tokens, 2);
    assert_eq!(
        result.details.unwrap()["nestedCalls"]["calls"][0]["status"],
        "error"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn each_script_has_fresh_globals_and_no_ambient_host_capabilities() {
    let fixture = Fixture::new(Vec::new(), BTreeMap::new(), None, &[]);
    assert_eq!(value(&fixture.run("leaked=123; return leaked").await), 123);
    assert_eq!(value(&fixture.run("assert(leaked==nil and require==nil and os==nil and io==nil and debug==nil and getfenv==nil and loadstring==nil and coroutine==nil); return true").await), true);
}

#[tokio::test]
async fn resource_limits_stop_before_excess_calls_or_large_results() {
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::new(
        vec![data_tool("read", calls.clone(), json!(42), false)],
        BTreeMap::new(),
        None,
        &[],
    );
    let result = fixture
        .run("for i=1,257 do tools.call('read',{n=i}) end; return true")
        .await;
    assert!(result.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 256, "{:?}", result);
    assert_eq!(
        result.details.unwrap()["nestedCalls"]["calls"]
            .as_array()
            .unwrap()
            .len(),
        256
    );
    assert!(fixture.run("return string.rep('x',40000)").await.is_error);
    assert!(
        fixture
            .run("return string.rep('x',64*1024*1024)")
            .await
            .is_error
    );
    let result = fixture
        .run("while true do pcall(function() while true do end end) end")
        .await;
    assert!(result.is_error);
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .contains("instruction budget")
    );
}

#[tokio::test]
async fn discovery_declares_deferred_tools_but_keeps_codemode_tools_undeclared() {
    let fixture = Fixture::new(Vec::new(), BTreeMap::new(), None, &[]);
    for (name, exposure) in [
        ("deferred", ToolExposure::Deferred),
        ("only_code", ToolExposure::Codemode),
        ("hidden", ToolExposure::Hidden),
    ] {
        let mut entry = ToolRegistration::direct(data_tool(
            name,
            Arc::new(AtomicUsize::new(0)),
            json!(42),
            false,
        ));
        entry.exposure = exposure;
        entry.namespace = Some("database".into());
        entry.output_schema = Some(json!({"type":"object"}));
        fixture.session.agent().register_tool(entry);
    }
    fixture.queue("tool_search", json!({"query":"database","limit":8}));
    fixture.session.agent().prompt("discover").await.unwrap();
    let contexts = fixture.contexts.lock().unwrap();
    assert!(!contexts[0].tools.iter().any(|tool| tool.name == "deferred"));
    assert!(contexts[1].tools.iter().any(|tool| tool.name == "deferred"));
    assert!(
        !contexts[1]
            .tools
            .iter()
            .any(|tool| tool.name == "only_code" || tool.name == "hidden")
    );
    drop(contexts);
    let matches = value(&fixture.last_result());
    assert_eq!(matches.as_array().unwrap().len(), 2);
    assert!(matches[0]["inputSchema"].is_object());
    assert!(matches[0]["outputSchema"].is_object());
    assert_eq!(
        value(
            &fixture
                .run("return tools.call('only_code',{n=1}).value.n")
                .await
        ),
        1
    );
}

#[tokio::test]
async fn host_exclusions_apply_to_child_calls_and_search() {
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::new(
        vec![data_tool("read", calls.clone(), json!(42), false)],
        BTreeMap::new(),
        None,
        &["read"],
    );
    let result = fixture
        .run("assert(describe_tool('read')==nil); tools.call('read',{n=1}); return true")
        .await;
    assert!(result.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn changed_arguments_are_revalidated_and_child_termination_stops_the_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::new(
        vec![data_tool("read", calls.clone(), json!(42), false)],
        BTreeMap::new(),
        None,
        &[],
    );
    fixture
        .session
        .agent()
        .set_before_tool_call(Arc::new(|context, _| {
            Box::pin(async move {
                if context.parent_tool_call_id.is_some() {
                    context.args.lock().unwrap()["n"] = json!("invalid");
                }
                None
            })
        }));
    assert_eq!(
        value(&fixture.run("return tools.call('read',{n=1}).isError").await),
        true
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    fixture
        .session
        .agent()
        .set_before_tool_call(Arc::new(|context, _| {
            Box::pin(async move {
                context
                    .parent_tool_call_id
                    .is_some()
                    .then(|| pillar_agent::BeforeToolCallResult {
                        block: true,
                        reason: Some("stop".into()),
                        terminate: true,
                    })
            })
        }));
    let previous = fixture.contexts.lock().unwrap().len();
    let result = fixture
        .run("tools.call('read',{n=1}); tools.call('read',{n=2}); return true")
        .await;
    assert!(result.is_error);
    assert_eq!(fixture.contexts.lock().unwrap().len(), previous + 1);
    assert_eq!(
        result.details.unwrap()["nestedCalls"]["calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn cancellation_drops_child_future_and_records_unknown_outcome() {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    struct OnDrop(Arc<AtomicUsize>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let mut tool = data_tool("wait", Arc::new(AtomicUsize::new(0)), json!(42), false);
    let notification = started.clone();
    let dropped_copy = dropped.clone();
    tool.execute = Arc::new(move |_, _, _, _| {
        let notification = notification.clone();
        let dropped = dropped_copy.clone();
        Box::pin(async move {
            let _guard = OnDrop(dropped);
            notification.notify_one();
            std::future::pending().await
        })
    });
    let fixture = Fixture::new(vec![tool], BTreeMap::new(), None, &[]);
    fixture.queue(
        "codemode",
        json!({"code":"tools.call('wait',{n=1}); return true"}),
    );
    let agent = fixture.session.agent();
    let prompt = agent.prompt("test");
    let abort = async {
        started.notified().await;
        agent.abort();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(prompt, abort)
    })
    .await
    .unwrap();
    result.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let result = fixture.last_result();
    assert!(result.is_error);
    assert_eq!(
        result.details.unwrap()["nestedCalls"]["calls"][0]["status"],
        "outcome_unknown"
    );
}

#[tokio::test]
async fn sequential_tool_override_controls_parallel_requests() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut tool = data_tool("read", Arc::new(AtomicUsize::new(0)), json!(42), false);
    tool.execution_mode = Some(ToolExecutionMode::Sequential);
    let execute = tool.execute.clone();
    let peak_copy = peak.clone();
    tool.execute = Arc::new(move |id, args, signal, updates| {
        let active = active.clone();
        let peak = peak_copy.clone();
        let execute = execute.clone();
        Box::pin(async move {
            peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            let result = execute(id, args, signal, updates).await;
            active.fetch_sub(1, Ordering::SeqCst);
            result
        })
    });
    let fixture = Fixture::new(vec![tool], BTreeMap::new(), None, &[]);
    assert_eq!(value(&fixture.run("return #tools.parallel({{name='read',arguments={n=1}},{name='read',arguments={n=2}}})").await), 2);
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[test]
fn cli_wiring_adds_orchestrators_without_reload_duplicates() {
    let wiring = build_extension_runner("/tmp", None, None, &[]);
    let fixture = Fixture::new(Vec::new(), BTreeMap::new(), None, &[]);
    for _ in 0..2 {
        fixture
            .session
            .replace_extension_tools(wiring.custom_tools());
    }
    for name in ["codemode", "tool_search"] {
        assert_eq!(
            fixture
                .session
                .state()
                .tools
                .iter()
                .filter(|tool| tool.name() == name)
                .count(),
            1
        );
    }
}
