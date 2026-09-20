//! Live A/B of the Rust diagnostic tools (design §13.3), opt-in.
//!
//! Condition A is the baseline a model would use anyway: `read` the human
//! build log and the source, then `edit`. Condition B adds the `rs_*` tools:
//! `rs_verify_plan` -> `rs_run` -> `rs_diagnostics` give the normalized error
//! and source line, then the same `edit`. Model, prompt, task, temperature and
//! editing tool are shared, so the comparison is paired.
//!
//! Run it with:
//!
//! ```text
//! PILLAR_RUST_LIVE_AB=1 cargo test -p pillar-coding-agent --test rust_tools_live_ab -- --nocapture
//! ```
//!
//! Without that variable the test skips: `scripts/check.sh` runs offline. The
//! api key comes from the existing credential store and is never printed.
//!
//! The run is synthetic: `rs_run` is a test-local broker whose "cargo output"
//! is a captured compiler message, so the comparison isolates the tool
//! contract (what the model reads) from cargo's own speed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pillar_agent::rust_tools::{
    BuildStatus, CargoJobBroker, Configuration, MemoryBroker, MemorySources, MemoryWorkspace,
    OwnerId, RustToolLimits, RustToolkit, ScriptedRun, SourceSnapshotPort, TestStatus,
    WorkspaceCatalogPort,
};
use pillar_agent::types::AgentTool;
use pillar_ai::api::openai_completions::{OpenaiCompletionsOptions, stream};
use pillar_ai::event_stream::collect_events;
use pillar_ai::types::{
    AssistantMessage, Content, Context, Message, Model, ToolResultMessage, Usage, UserContent,
};
use serde_json::Value;

const MODEL_ID: &str = "deepseek-v4.1-flash";
const CONFIGURATION_ID: &str = "native-default";
const MAX_TURNS: usize = 10;
/// Repetitions per (condition, task): live runs vary.
const REPETITIONS: usize = 2;

// --- live plumbing ---------------------------------------------------------

struct LiveModel {
    model: Model,
    api_key: String,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn live_model(model_id: &str) -> Option<LiveModel> {
    let catalog: Value = serde_json::from_str(
        &std::fs::read_to_string(home().join(".pillar/agent/models.json")).ok()?,
    )
    .ok()?;
    let auth: Value = serde_json::from_str(
        &std::fs::read_to_string(home().join(".pillar/agent/auth.json")).ok()?,
    )
    .ok()?;
    let providers = catalog.get("providers").unwrap_or(&catalog).as_object()?;
    for (provider, config) in providers {
        let models = config.get("models")?.as_array()?;
        let Some(entry) = models
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model_id))
        else {
            continue;
        };
        let api_key = auth
            .get(provider)
            .and_then(|auth| auth.get("key"))
            .and_then(Value::as_str)
            .map(str::to_string)?;
        let mut model = Model {
            id: model_id.to_string(),
            name: entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(model_id)
                .to_string(),
            api: config
                .get("api")
                .and_then(Value::as_str)
                .unwrap_or("openai-completions")
                .to_string(),
            provider: provider.clone(),
            base_url: config.get("baseUrl").and_then(Value::as_str)?.to_string(),
            reasoning: entry
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            thinking_level_map: None,
            input: vec!["text".to_string()],
            cost: pillar_ai::types::ModelCost::default(),
            context_window: entry
                .get("contextWindow")
                .and_then(Value::as_u64)
                .unwrap_or(128_000),
            max_tokens: entry
                .get("maxTokens")
                .and_then(Value::as_u64)
                .unwrap_or(4096),
            sampling_params: None,
            headers: None,
            compat: None,
        };
        // The Go endpoint routes by session: the port's own header list does
        // not cover it (docs/TASKS.md).
        model.headers = Some(
            [(
                "x-opencode-session".to_string(),
                Some(pillar_ai::uuid::uuidv7()),
            )]
            .into_iter()
            .collect(),
        );
        return Some(LiveModel { model, api_key });
    }
    None
}

fn usage_totals(usage: &Usage) -> (u64, u64) {
    let value = serde_json::to_value(usage).unwrap_or(Value::Null);
    let get = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    (get("input"), get("output"))
}

// --- tasks -----------------------------------------------------------------

struct Task {
    name: &'static str,
    source: &'static str,
    expected: &'static str,
    /// The human `cargo build` rendering the baseline reads.
    human_log: fn(&str) -> String,
    /// The `compiler-message` JSON line the broker returns.
    message: fn(&str) -> String,
    prompt_a: &'static str,
    prompt_b: &'static str,
}

fn missing_mut_human(root: &str) -> String {
    format!(
        "error[E0596]: cannot borrow `values` as mutable, as it is not declared as mutable\n \
         --> {root}/src/main.rs:3:5\n  |\n3 |     values.push(1);\n  |     ^^^^^^ cannot borrow as mutable\n  |\n  \
         help: consider changing this to be mutable\n  |\n2 |     let mut values = Vec::new();\n  |         +++\n"
    )
}

fn missing_mut_message(root: &str) -> String {
    let rendered = serde_json::to_string(&missing_mut_human(root)).expect("rendered");
    format!(
        r##"{{"reason":"compiler-message","package_id":"example-core","target":{{"name":"example_core"}},"message":{{"rendered":{rendered},"code":{{"code":"E0596"}},"level":"error","message":"cannot borrow `values` as mutable, as it is not declared as mutable","spans":[{{"file_name":"{root}/src/main.rs","byte_start":36,"byte_end":42,"line_start":3,"line_end":3,"column_start":5,"column_end":11,"is_primary":true,"label":"cannot borrow as mutable"}}]}}}}"##
    )
}

fn tasks() -> Vec<Task> {
    vec![Task {
        name: "missing_mut",
        source: "fn main() {\n    let values = Vec::new();\n    values.push(1);\n    println!(\"{values:?}\");\n}\n",
        expected: "fn main() {\n    let mut values = Vec::new();\n    values.push(1);\n    println!(\"{values:?}\");\n}\n",
        human_log: missing_mut_human,
        message: missing_mut_message,
        prompt_a: "Fix the Rust compile error reported in build.log, for the source in src/main.rs. \
             Read build.log and src/main.rs with the read tool, then fix src/main.rs with the \
             edit tool. When the compile error is fixed, reply DONE with no tool call.",
        prompt_b: "Fix the Rust compile error reported by the build tools, for the source in \
             src/main.rs. The saved workspace metadata has the approved configuration id \
             `native-default`. Call rs_verify_plan with changed_paths [\"src/main.rs\"], \
             configuration_ids [\"native-default\"], goal \"validate_change\", scope \"focused\"; \
             rs_run the returned step; then rs_diagnostics on the returned run to read the error \
             and the source line. Fix src/main.rs with the edit tool. When the compile error is \
             fixed, reply DONE with no tool call.",
    }]
}

// --- the two conditions ---------------------------------------------------

#[derive(Clone, Copy)]
enum Condition {
    Baseline,
    RustTools,
}

impl Condition {
    fn name(&self) -> &'static str {
        match self {
            Self::Baseline => "A read+edit",
            Self::RustTools => "B rs_* + edit",
        }
    }
}

struct Outcome {
    turns: usize,
    provider_failures: usize,
    tool_calls: usize,
    failed_calls: usize,
    input_tokens: u64,
    output_tokens: u64,
    succeeded: bool,
}

fn catalog_json(root: &str) -> String {
    format!(
        r#"{{"packages":[{{"id":"example-core","name":"example-core","manifest_path":"{root}/Cargo.toml","targets":[{{"name":"example_core","kind":["bin"],"crate_types":["bin"],"src_path":"{root}/src/main.rs","test":true,"doctest":false}}],"dependencies":[],"features":{{}}}}],"workspace_members":["example-core"],"workspace_default_members":["example-core"],"workspace_root":"{root}","target_directory":"{root}/target","metadata":null}}"#
    )
}

fn configuration() -> Configuration {
    Configuration {
        id: CONFIGURATION_ID.to_string(),
        toolchain: Some("stable".to_string()),
        host_triple: Some("x86_64-unknown-linux-gnu".to_string()),
        target_triple: Some("x86_64-unknown-linux-gnu".to_string()),
        profile: Some("test".to_string()),
        packages: Vec::new(),
        features: Vec::new(),
        all_features: false,
        no_default_features: false,
        selected_targets: Vec::new(),
        cargo_config_digest: None,
        lock_digest: None,
        rustflags_digest: None,
        env_digest: None,
    }
}

fn rust_toolkit(root: &str, log: &str, source: &str) -> RustToolkit {
    let workspace = Arc::new(
        MemoryWorkspace::new(catalog_json(root)).with_configurations(vec![configuration()]),
    );
    let broker: Arc<dyn CargoJobBroker> = Arc::new(MemoryBroker::new(
        // The stream must agree with the record: a failed compile, no test run.
        ScriptedRun::finished(
            format!("{log}\n{{\"reason\":\"build-finished\",\"success\":false}}\n").into_bytes(),
            Vec::new(),
            101,
        )
        .with_statuses(BuildStatus::Failed, TestStatus::NotRun),
    ));
    let sources = Arc::new(MemorySources::new());
    sources.set_file(format!("{root}/src/main.rs"), source);
    RustToolkit::new(
        workspace as Arc<dyn WorkspaceCatalogPort>,
        broker,
        sources as Arc<dyn SourceSnapshotPort>,
        OwnerId::new("live-ab"),
        RustToolLimits::default(),
    )
}

fn tools_for(condition: Condition, root: &Path, toolkit: Option<&RustToolkit>) -> Vec<AgentTool> {
    let cwd = root.to_string_lossy();
    let read = pillar_coding_agent::core::tools::index::create_tool("read", &cwd).expect("read");
    let edit = pillar_coding_agent::core::tools::index::create_tool("edit", &cwd).expect("edit");
    match condition {
        Condition::Baseline => vec![read, edit],
        Condition::RustTools => {
            let mut tools = toolkit.expect("toolkit").tools();
            tools.push(read);
            tools.push(edit);
            tools
        }
    }
}

async fn run_task(
    condition: Condition,
    task: &Task,
    workspace: &Path,
    live: &LiveModel,
) -> Outcome {
    let root = workspace.to_path_buf();
    std::fs::create_dir_all(root.join("src")).expect("workspace");
    std::fs::write(root.join("src/main.rs"), task.source.as_bytes()).expect("seed source");
    let root_str = root.to_string_lossy().to_string();
    let human = (task.human_log)(&root_str);
    let message = (task.message)(&root_str);
    std::fs::write(root.join("build.log"), human.as_bytes()).expect("seed log");

    let toolkit = match condition {
        Condition::Baseline => None,
        Condition::RustTools => Some(rust_toolkit(&root_str, &message, task.source)),
    };
    let tools = tools_for(condition, &root, toolkit.as_ref());
    let prompt = match condition {
        Condition::Baseline => task.prompt_a,
        Condition::RustTools => task.prompt_b,
    };

    let mut context = Context {
        system_prompt: Some(
            "You fix Rust compiler errors. Use the tools you are given. When the compile error \
             is fixed, reply with the single word DONE and no tool call."
                .to_string(),
        ),
        messages: vec![Message::User {
            content: UserContent::Text(prompt.to_string()),
            timestamp: 0,
        }],
        tools: tools.iter().map(|tool| tool.tool.clone()).collect(),
    };

    let mut outcome = Outcome {
        turns: 0,
        provider_failures: 0,
        tool_calls: 0,
        failed_calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        succeeded: false,
    };

    let mut retries = 0usize;
    for _ in 0..MAX_TURNS {
        let stream = stream(
            live.model.clone(),
            context.clone(),
            Some(OpenaiCompletionsOptions {
                api_key: Some(live.api_key.clone()),
                ..Default::default()
            }),
        );
        let _ = collect_events(&stream).await;
        let assistant: AssistantMessage = stream.result().await;
        outcome.turns += 1;
        let (input, output) = usage_totals(&assistant.usage);
        outcome.input_tokens += input;
        outcome.output_tokens += output;
        if let Some(message) = assistant.error_message.clone() {
            let truncated = message.contains("ended without");
            outcome.provider_failures += 1;
            println!(
                "      provider {}: {}",
                if truncated { "truncation" } else { "error" },
                message.chars().take(150).collect::<String>()
            );
            if truncated && retries < 2 {
                retries += 1;
                continue;
            }
            break;
        }

        let mut results = Vec::new();
        for block in &assistant.content {
            if let Some(call) = pillar_agent::types::AgentToolCall::from_content(block) {
                let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
                    continue;
                };
                outcome.tool_calls += 1;
                match (tool.execute)(call.id.clone(), call.arguments.clone(), None, None).await {
                    Ok(result) => {
                        let first_line = result
                            .content
                            .iter()
                            .find_map(|part| match part {
                                Content::Text { text, .. } => {
                                    Some(text.lines().next().unwrap_or(""))
                                }
                                _ => None,
                            })
                            .unwrap_or("");
                        println!(
                            "      -> {} {} | {}",
                            call.name,
                            call.arguments
                                .to_string()
                                .chars()
                                .take(120)
                                .collect::<String>(),
                            first_line.chars().take(120).collect::<String>()
                        );
                        results.push(Message::ToolResult(Box::new(ToolResultMessage {
                            tool_call_id: call.id,
                            tool_name: call.name,
                            content: result.content,
                            details: Some(result.details),
                            usage: None,
                            added_tool_names: None,
                            is_error: false,
                            timestamp: 0,
                        })));
                    }
                    Err(error) => {
                        outcome.failed_calls += 1;
                        results.push(Message::ToolResult(Box::new(ToolResultMessage {
                            tool_call_id: call.id,
                            tool_name: call.name,
                            content: vec![Content::text(error.to_string())],
                            details: None,
                            usage: None,
                            added_tool_names: None,
                            is_error: true,
                            timestamp: 0,
                        })));
                    }
                }
            }
        }

        context
            .messages
            .push(Message::Assistant(Box::new(assistant)));
        if results.is_empty() {
            break;
        }
        context.messages.extend(results);
    }

    let final_text = std::fs::read_to_string(root.join("src/main.rs")).unwrap_or_default();
    outcome.succeeded = final_text == task.expected;
    outcome
}

#[test]
fn preflight_finds_the_model_and_its_key() {
    match live_model(MODEL_ID) {
        Some(live) => {
            assert!(!live.model.base_url.is_empty());
            assert!(!live.api_key.is_empty());
            println!(
                "preflight: {MODEL_ID} via {} ({})",
                live.model.provider, live.model.api
            );
        }
        None => println!(
            "preflight: no {MODEL_ID} entry + key in ~/.pillar/agent (live run would skip)"
        ),
    }
}

#[tokio::test]
async fn the_baseline_and_rust_tools_on_a_live_model() {
    if std::env::var("PILLAR_RUST_LIVE_AB").as_deref() != Ok("1") {
        println!("skipped: set PILLAR_RUST_LIVE_AB=1 to spend real model calls");
        return;
    }
    let Some(live) = live_model(MODEL_ID) else {
        println!("skipped: no {MODEL_ID} entry and api key in the user stores");
        return;
    };

    let sandbox = std::env::temp_dir().join(format!("pillar-rust-ab-{}", std::process::id()));
    println!(
        "\n{:<24} {:>6} {:>6} {:>7} {:>8} {:>8} {:>6}",
        "condition / task", "turns", "calls", "failed", "in_tok", "out_tok", "ok"
    );
    for repetition in 0..REPETITIONS {
        for task in tasks() {
            for condition in [Condition::Baseline, Condition::RustTools] {
                let workspace = sandbox.join(format!(
                    "{}-{}-{}",
                    condition.name().split(' ').next().expect("letter"),
                    task.name,
                    repetition
                ));
                let outcome = run_task(condition, &task, &workspace, &live).await;
                println!(
                    "{:<24} {:>6} {:>6} {:>7} {:>8} {:>8} {:>6}",
                    format!("{} / {}", condition.name(), task.name),
                    outcome.turns,
                    outcome.tool_calls,
                    outcome.failed_calls,
                    outcome.input_tokens,
                    outcome.output_tokens,
                    outcome.succeeded
                );
                let final_text =
                    std::fs::read_to_string(workspace.join("src/main.rs")).unwrap_or_default();
                assert_eq!(outcome.succeeded, final_text == task.expected);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&sandbox);
}
