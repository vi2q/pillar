//! Live A/B of the two edit contracts (design §10.3 step 3), opt-in.
//!
//! Condition A is the existing `read` + `edit` pair; condition B is
//! `exp_read` + `exp_edit` over a test-local native host. Everything else —
//! model, prompt, task, temperature — is shared, and each task runs through
//! both conditions so the comparison is paired.
//!
//! Run it with:
//!
//! ```text
//! PILLAR_LIVE_AB=1 cargo test -p pillar-coding-agent --test exp_live_ab -- --nocapture
//! ```
//!
//! Without that variable the test skips: `scripts/check.sh` runs offline, and a
//! live call must never be an accident. The API key comes from the existing
//! credential store (`~/.pillar/agent/auth.json`); it is never printed and never
//! written into the workspace.
//!
//! The host in condition B is *this process*: it is the only writer, which is
//! the ground on which design §4.3 allows a strict comparison-and-swap. That is
//! a property of the test, not of a user-visible workspace, and reusing this
//! host elsewhere would need a mediated write path (or a weak, explicitly
//! opted-in mode).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pillar_agent::exp::{
    ConditionalStore, ExpError, ExpLimits, ExpToolkit, Guarantee, OperationLedger, OwnerId,
    RefStore, ResourceId, Revision, Snapshot, manual_clock,
};
use pillar_agent::types::AgentTool;
use pillar_ai::api::openai_completions::{OpenaiCompletionsOptions, stream};
use pillar_ai::event_stream::collect_events;
use pillar_ai::types::{
    AssistantMessage, Content, Context, Message, Model, ToolResultMessage, Usage, UserContent,
};
use serde_json::Value;

const MODEL_ID: &str = "deepseek-v4.1-flash";
const MAX_TURNS: usize = 8;
/// Repetitions per (condition, task): live runs vary, so one sample decides nothing.
const REPETITIONS: usize = 3;

// --- live plumbing ---------------------------------------------------------

struct LiveModel {
    model: Model,
    api_key: String,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

/// The model entry and its api key from the user's own stores.
fn live_model(model_id: &str) -> Option<LiveModel> {
    let catalog: Value = serde_json::from_str(
        &std::fs::read_to_string(home().join(".pillar/agent/models.json")).ok()?,
    )
    .ok()?;
    let auth: Value = serde_json::from_str(
        &std::fs::read_to_string(home().join(".pillar/agent/auth.json")).ok()?,
    )
    .ok()?;

    // `models.json` nests the catalog under `providers` (the runtime store);
    // the bundled catalog is a flat map, so accept both shapes.
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
        // Hand-built: the catalog entry's `compat` is the store's own shape,
        // which does not round-trip through the typed `Model` here.
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
        // not cover it, so the experiment states it explicitly.
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

// --- a native host for the test -------------------------------------------

/// `ConditionalStore` over real files, valid only while this process is the
/// only writer (see the module docs).
struct NativeHost {
    root: PathBuf,
}

impl NativeHost {
    fn path(&self, resource: &ResourceId) -> PathBuf {
        self.root.join(resource.as_str())
    }

    fn revision(&self, resource: &ResourceId) -> Result<Revision, ExpError> {
        let bytes = std::fs::read(self.path(resource)).map_err(|error| {
            ExpError::new(
                pillar_agent::exp::ExpErrorCode::InvalidRequest,
                format!("cannot read the target: {error}"),
            )
        })?;
        Ok(Revision::new(bytes.len() as u64, &bytes))
    }
}

#[async_trait]
impl ConditionalStore for NativeHost {
    fn guarantee(&self) -> Guarantee {
        Guarantee::Strict
    }

    async fn resolve(&self, path: &str) -> Result<ResourceId, ExpError> {
        let candidate = self.root.join(path);
        if candidate.is_file() {
            Ok(ResourceId::new(path))
        } else {
            Err(ExpError::new(
                pillar_agent::exp::ExpErrorCode::InvalidRequest,
                format!("no such file: {path}"),
            ))
        }
    }

    async fn snapshot(&self, resource: &ResourceId) -> Result<Snapshot, ExpError> {
        let bytes = std::fs::read(self.path(resource)).map_err(|error| {
            ExpError::new(
                pillar_agent::exp::ExpErrorCode::InvalidRequest,
                format!("cannot read the target: {error}"),
            )
        })?;
        Ok(Snapshot {
            resource: resource.clone(),
            revision: Revision::new(bytes.len() as u64, &bytes),
            bytes,
        })
    }

    async fn compare_and_swap(
        &self,
        resource: &ResourceId,
        expected: Revision,
        new_bytes: &[u8],
    ) -> Result<Revision, ExpError> {
        if self.revision(resource)? != expected {
            return Err(ExpError::new(
                pillar_agent::exp::ExpErrorCode::RevisionConflict,
                "the target changed under the test host",
            ));
        }
        std::fs::write(self.path(resource), new_bytes).map_err(|error| {
            ExpError::new(
                pillar_agent::exp::ExpErrorCode::HostFailure,
                format!("cannot publish: {error}"),
            )
        })?;
        Ok(Revision::new(new_bytes.len() as u64, new_bytes))
    }
}

// --- tasks ----------------------------------------------------------------

struct Task {
    name: &'static str,
    file: String,
    expected: String,
    prompt: String,
}

fn lines(count: usize, body: impl Fn(usize) -> String) -> String {
    (0..count).map(body).collect::<Vec<_>>().join("\n") + "\n"
}

fn tasks() -> Vec<Task> {
    let mut long = String::from("fn big(input: &[i32]) -> i32 {\n");
    for index in 1..198 {
        if index == 120 {
            long.push_str("    let target = 41;\n");
        } else {
            long.push_str(&format!("    let value_{index} = {index};\n"));
        }
    }
    long.push_str("}\n");
    let long_expected = long.replace("let target = 41;", "let target = 42;");

    let block = lines(60, |index| format!("    old_{index}();"));
    let block_expected = {
        let mut out = lines(20, |index| format!("    old_{index}();"));
        out.push_str(&lines(30, |index| format!("    new_{index}();")));
        out.push_str(&lines(10, |index| format!("    old_{}();", index + 50)));
        out
    };

    vec![
        Task {
            name: "one_line_in_a_long_file",
            file: long,
            expected: long_expected,
            prompt: "In f.txt, change the value of `target` in the function to 42. \
                     Use the tools; do not rewrite the whole file."
                .to_string(),
        },
        Task {
            name: "thirty_line_block",
            file: block,
            expected: block_expected,
            prompt: "In f.txt, replace the block of 30 consecutive lines that start with \
                     `    old_20();` through `    old_49();` with 30 lines named new_0(); \
                     through new_29(); (indentation `    `, one per line). Use the tools."
                .to_string(),
        },
    ]
}

// --- the two conditions ---------------------------------------------------

#[derive(Clone, Copy)]
enum Condition {
    Existing,
    Reference,
}

impl Condition {
    fn name(&self) -> &'static str {
        match self {
            Self::Existing => "A existing read+edit",
            Self::Reference => "B exp_read+exp_edit",
        }
    }
}

struct Outcome {
    turns: usize,
    /// Provider failures (truncated streams), counted as failed attempts whose
    /// tokens stay in the totals (design §10.3).
    provider_failures: usize,
    tool_calls: usize,
    failed_calls: usize,
    input_tokens: u64,
    output_tokens: u64,
    succeeded: bool,
}

fn tools_for(condition: &Condition, root: &Path, exp: Option<&ExpToolkit>) -> Vec<AgentTool> {
    match condition {
        Condition::Existing => vec![
            pillar_coding_agent::core::tools::index::create_tool("read", &root.to_string_lossy())
                .expect("read"),
            pillar_coding_agent::core::tools::index::create_tool("edit", &root.to_string_lossy())
                .expect("edit"),
        ],
        Condition::Reference => exp.expect("toolkit").tools(),
    }
}

fn usage_totals(usage: &Usage) -> (u64, u64) {
    let value = serde_json::to_value(usage).unwrap_or(Value::Null);
    let get = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    (get("input"), get("output"))
}

/// One scripted agent: the model chooses the calls, this loop executes them.
async fn run_task(
    condition: Condition,
    task: &Task,
    workspace: &Path,
    live: &LiveModel,
) -> Outcome {
    let root = workspace.to_path_buf();
    std::fs::create_dir_all(&root).expect("workspace");
    std::fs::write(root.join("f.txt"), task.file.as_bytes()).expect("seed the file");

    let limits = ExpLimits::default();
    let (clock, _now) = manual_clock(0);
    let toolkit = ExpToolkit::with_state(
        Arc::new(NativeHost { root: root.clone() }) as Arc<dyn ConditionalStore>,
        Arc::new(RefStore::new(clock, limits.max_live_refs)),
        Arc::new(OperationLedger::new(limits.ledger_capacity)),
        limits,
        OwnerId::new("live-ab"),
    );
    let tools = tools_for(&condition, &root, Some(&toolkit));

    let mut context = Context {
        system_prompt: Some(
            "You edit files with the tools you are given. Work file f.txt. \
             When the task is done, reply with the single word DONE and no tool call."
                .to_string(),
        ),
        messages: vec![Message::User {
            content: UserContent::Text(task.prompt.clone()),
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
            // A truncated stream is a failed attempt: the tokens stay in the
            // totals, and the turn is re-sent. A re-sent edit is safe on the
            // reference path (its operation id is the guard); the existing
            // path offers no such guarantee, which is part of the comparison.
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
                    // The trace: what the model asked for, and the first line of
                    // what it got back (for `exp_read` that is the range+ref
                    // header, which is what the diagnosis needs).
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

    let final_text = std::fs::read_to_string(root.join("f.txt")).unwrap_or_default();
    outcome.succeeded = final_text == task.expected;
    if !outcome.succeeded {
        let actual: Vec<&str> = final_text.lines().collect();
        let expected: Vec<&str> = task.expected.lines().collect();
        println!(
            "      mismatch: actual {} lines vs expected {} lines",
            actual.len(),
            expected.len()
        );
        for index in 0..actual.len().max(expected.len()) {
            let got = actual.get(index).copied().unwrap_or("<missing>");
            let want = expected.get(index).copied().unwrap_or("<missing>");
            if got != want {
                println!("      line {}: got {got:?} want {want:?}", index + 1);
                break;
            }
        }
    }
    outcome
}

/// Preflight, so the live run does not fail on discovery: the model entry and
/// an api key must be found in the user's stores. Always runs (no model call).
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
async fn the_reference_and_existing_paths_on_a_live_model() {
    if std::env::var("PILLAR_LIVE_AB").as_deref() != Ok("1") {
        println!("skipped: set PILLAR_LIVE_AB=1 to spend real model calls");
        return;
    }
    let Some(live) = live_model(MODEL_ID) else {
        println!("skipped: no {MODEL_ID} entry and api key in the user stores");
        return;
    };

    let sandbox = std::env::temp_dir().join(format!("pillar-live-ab-{}", std::process::id()));
    println!(
        "{:<28} {:>6} {:>6} {:>7} {:>8} {:>8} {:>6}",
        "condition / task", "turns", "calls", "failed", "in_tok", "out_tok", "ok"
    );

    for repetition in 0..REPETITIONS {
        for task in tasks() {
            for condition in [Condition::Existing, Condition::Reference] {
                let workspace = sandbox.join(format!(
                    "{}-{}-{}",
                    condition.name().split(' ').next().expect("letter"),
                    task.name,
                    repetition
                ));
                let outcome = run_task(condition, &task, &workspace, &live).await;
                println!(
                    "{:<28} {:>6} {:>6} {:>7} {:>8} {:>8} {:>6}",
                    format!(
                        "{} / {}",
                        condition.name().split(' ').next().unwrap(),
                        task.name
                    ),
                    outcome.turns,
                    outcome.tool_calls,
                    outcome.failed_calls,
                    outcome.input_tokens,
                    outcome.output_tokens,
                    outcome.succeeded
                );
                // The harness gate: a run that reports success must have produced
                // the expected file, and vice versa.
                let final_text =
                    std::fs::read_to_string(workspace.join("f.txt")).unwrap_or_default();
                assert_eq!(
                    outcome.succeeded,
                    final_text == task.expected,
                    "{}",
                    task.name
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&sandbox);
}
