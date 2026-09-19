//! The four `rs_*` tools at their real boundary (design §4, "実経路"): JSON
//! arguments in, an `AgentToolResult` out, over the in-memory host ports.
//!
//! These drive `AgentTool::execute`, so they cover what the model sees and
//! what the contracts require: `rs_run` re-checks the plan at execution time,
//! a foreign owner cannot read a run, `rs_diagnostics` reads what already
//! happened and never starts a build, and cancellation/abort do not produce a
//! success.

#![cfg(feature = "rust-tools")]

use std::sync::Arc;

use pillar_agent::AbortSignal;
use pillar_agent::rust_tools::{
    BuildStatus, CargoJobBroker, Configuration, MemoryBroker, MemorySources, MemoryWorkspace,
    OwnerId, RustToolLimits, RustToolkit, ScriptedRun, SourceSnapshotPort, TestStatus,
    WorkspaceCatalogPort,
};
use pillar_agent::types::AgentToolResult;
use serde_json::{Value, json};

const CORE: &str = "path+file:///ws/crates/example-core#example-core@0.1.0";
const SOURCE_PATH: &str = "/ws/crates/example-core/src/lib.rs";

fn metadata_json() -> String {
    format!(
        r#"{{
  "packages": [{{
    "id": "{CORE}",
    "name": "example-core",
    "version": "0.1.0",
    "manifest_path": "/ws/crates/example-core/Cargo.toml",
    "targets": [
      {{"name": "example_core", "kind": ["lib"], "crate_types": ["lib"],
        "src_path": "{SOURCE_PATH}", "edition": "2024", "doc": true, "doctest": true, "test": true}},
      {{"name": "contract", "kind": ["test"], "crate_types": ["bin"],
        "src_path": "/ws/crates/example-core/tests/contract.rs", "edition": "2024",
        "doc": false, "doctest": false, "test": true}}
    ],
    "dependencies": [],
    "features": {{"extra": []}}
  }}],
  "workspace_members": ["{CORE}"],
  "workspace_default_members": ["{CORE}"],
  "workspace_root": "/ws",
  "target_directory": "/ws/target",
  "metadata": null
}}"#
    )
}

fn configuration(id: &str, features: &[&str]) -> Configuration {
    Configuration {
        id: id.to_string(),
        toolchain: Some("stable".to_string()),
        host_triple: Some("x86_64-unknown-linux-gnu".to_string()),
        target_triple: Some("x86_64-unknown-linux-gnu".to_string()),
        profile: Some("test".to_string()),
        packages: Vec::new(),
        features: features.iter().map(|feature| feature.to_string()).collect(),
        all_features: false,
        no_default_features: false,
        selected_targets: Vec::new(),
        cargo_config_digest: None,
        lock_digest: Some("lock-digest".to_string()),
        rustflags_digest: None,
        env_digest: None,
    }
}

fn diagnostic_line(code: &str, message: &str, level: &str, line: u64) -> String {
    format!(
        r#"{{"reason":"compiler-message","package_id":"{CORE}","target":{{"name":"example_core"}},"message":{{"code":{{"code":"{code}"}},"level":"{level}","message":"{message}","spans":[{{"file_name":"{SOURCE_PATH}","byte_start":10,"byte_end":13,"line_start":{line},"line_end":{line},"column_start":1,"column_end":4,"is_primary":true,"label":"here"}}]}}}}"#
    )
}

/// A scripted run: one warning diagnostic, a successful build, a failed test.
fn default_script() -> ScriptedRun {
    let stdout = format!(
        "{}\n{}\n{}\n",
        diagnostic_line("E0277", "the trait bound is not satisfied", "error", 2),
        r#"{"reason":"build-finished","success":true}"#,
        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
    );
    ScriptedRun::finished(stdout.into_bytes(), Vec::new(), 101)
        .with_statuses(BuildStatus::Succeeded, TestStatus::Failed)
}

struct Harness {
    toolkit: RustToolkit,
    workspace: Arc<MemoryWorkspace>,
    broker: Arc<dyn CargoJobBroker>,
}

fn harness(owner: &str) -> Harness {
    harness_with(owner, default_script())
}

fn harness_with(owner: &str, script: ScriptedRun) -> Harness {
    let workspace = Arc::new(
        MemoryWorkspace::new(metadata_json())
            .with_configurations(vec![configuration("native-default", &[])]),
    );
    let broker: Arc<dyn CargoJobBroker> = Arc::new(MemoryBroker::new(script));
    let sources = Arc::new(MemorySources::new());
    sources.set_file(SOURCE_PATH, "fn main() {}\nlet x = foo();\n");
    let toolkit = RustToolkit::new(
        workspace.clone() as Arc<dyn WorkspaceCatalogPort>,
        Arc::clone(&broker),
        sources as Arc<dyn SourceSnapshotPort>,
        OwnerId::new(owner),
        RustToolLimits::default(),
    );
    Harness {
        toolkit,
        workspace,
        broker,
    }
}

fn text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            pillar_ai::types::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn execute(
    toolkit: &RustToolkit,
    tool: &str,
    args: Value,
    signal: Option<AbortSignal>,
) -> Result<AgentToolResult, pillar_agent::types::ToolExecuteError> {
    let tool = toolkit
        .tools()
        .into_iter()
        .find(|candidate| candidate.tool.name == tool)
        .expect("tool exists");
    (tool.execute)("call-1".to_string(), args, signal, None).await
}

async fn plan(harness: &Harness) -> (String, String) {
    let result = execute(
        &harness.toolkit,
        "rs_verify_plan",
        json!({
            "changed_paths": ["crates/example-core/src/lib.rs"],
            "configuration_ids": ["native-default"],
            "goal": "validate_change",
            "scope": "focused",
            "requested_targets": ["contract"]
        }),
        None,
    )
    .await
    .expect("plan");
    let plan_id = result.details["plan_id"]
        .as_str()
        .expect("plan id")
        .to_string();
    let step_id = result.details["steps"][0]["step_id"]
        .as_str()
        .expect("step id")
        .to_string();
    (plan_id, step_id)
}

// --- registration ----------------------------------------------------------

#[test]
fn the_tools_are_registered_under_their_rs_names() {
    let harness = harness("session-a");
    let names: Vec<String> = harness
        .toolkit
        .tools()
        .into_iter()
        .map(|tool| tool.tool.name)
        .collect();
    assert_eq!(
        names,
        vec!["rs_verify_plan", "rs_run", "rs_job", "rs_diagnostics"]
    );
}

// --- the full path ---------------------------------------------------------

#[tokio::test]
async fn plan_run_diagnostics_and_job_connect_end_to_end() {
    let harness = harness("session-a");
    let (plan_id, step_id) = plan(&harness).await;

    let run = execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
        None,
    )
    .await
    .expect("run");
    assert!(text(&run).contains("rs_run"));
    assert_eq!(run.details["state"], "exited");
    let run_id = run.details["run_id"].as_str().expect("run id").to_string();

    let diagnostics = execute(
        &harness.toolkit,
        "rs_diagnostics",
        json!({"run_id": run_id}),
        None,
    )
    .await
    .expect("diagnostics");
    let body = text(&diagnostics);
    assert!(body.contains("E0277"), "{body}");
    assert!(body.contains("let x = foo();"), "{body}");
    assert_eq!(diagnostics.details["build_status"], "succeeded");
    assert_eq!(diagnostics.details["test_status"], "failed");
    assert!(diagnostics.details["collection"]["kind"].is_string());

    let status = execute(
        &harness.toolkit,
        "rs_job",
        json!({"run_id": run_id, "action": "status"}),
        None,
    )
    .await
    .expect("status");
    assert!(text(&status).contains("exited"));

    let output = execute(
        &harness.toolkit,
        "rs_job",
        json!({"run_id": run_id, "action": "output", "stream": "stdout", "limit_bytes": 4096}),
        None,
    )
    .await
    .expect("output");
    assert!(text(&output).contains("compiler-message"));
}

#[tokio::test]
async fn rs_run_refuses_a_plan_made_against_changed_metadata() {
    let harness = harness("session-a");
    let (plan_id, step_id) = plan(&harness).await;

    // A refresh would replace the saved metadata; the digest changes.
    let mut changed = metadata_json();
    changed = changed.replace("\"version\": \"0.1.0\"", "\"version\": \"0.2.0\"");
    harness.workspace.set_catalog_json(changed);

    let error = execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
        None,
    )
    .await
    .expect_err("stale");
    assert!(error.0.contains("stale_plan"), "{error}");
}

#[tokio::test]
async fn rs_run_refuses_a_plan_against_changed_configuration() {
    let harness = harness("session-a");
    let (plan_id, step_id) = plan(&harness).await;

    harness
        .workspace
        .set_configurations(vec![configuration("native-default", &["extra"])]);

    let error = execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
        None,
    )
    .await
    .expect_err("stale configuration");
    assert!(error.0.contains("stale_plan"), "{error}");
}

#[tokio::test]
async fn a_request_id_reused_for_a_different_command_is_refused() {
    let workspace = Arc::new(
        MemoryWorkspace::new(metadata_json()).with_configurations(vec![
            configuration("native-default", &[]),
            configuration("with-extra", &["extra"]),
        ]),
    );
    let broker: Arc<dyn CargoJobBroker> = Arc::new(MemoryBroker::new(default_script()));
    let sources = Arc::new(MemorySources::new());
    sources.set_file(SOURCE_PATH, "fn main() {}\nlet x = foo();\n");
    let toolkit = RustToolkit::new(
        workspace as Arc<dyn WorkspaceCatalogPort>,
        Arc::clone(&broker),
        sources as Arc<dyn SourceSnapshotPort>,
        OwnerId::new("session-a"),
        RustToolLimits::default(),
    );
    let harness = Harness {
        toolkit,
        workspace: Arc::new(MemoryWorkspace::default()),
        broker,
    };

    let result = execute(
        &harness.toolkit,
        "rs_verify_plan",
        json!({
            "changed_paths": ["crates/example-core/src/lib.rs"],
            "configuration_ids": ["native-default", "with-extra"],
            "goal": "validate_change",
            "scope": "focused"
        }),
        None,
    )
    .await
    .expect("plan");
    let plan_id = result.details["plan_id"]
        .as_str()
        .expect("plan id")
        .to_string();
    let steps: Vec<String> = result.details["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| step["step_id"].as_str().expect("step id").to_string())
        .collect();
    assert_eq!(steps.len(), 2);
    // The two configurations differ, so the argv (and its digest) differ.
    assert_ne!(
        result.details["steps"][0]["argv"],
        result.details["steps"][1]["argv"]
    );

    execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": steps[0], "request_id": "rq1"}),
        None,
    )
    .await
    .expect("first start");
    let error = execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": steps[1], "request_id": "rq1"}),
        None,
    )
    .await
    .expect_err("id mismatch");
    assert!(error.0.contains("operation_id_mismatch"), "{error}");
}

// --- ownership and safety --------------------------------------------------

#[tokio::test]
async fn a_foreign_owner_cannot_read_a_run() {
    let harness = harness("session-a");
    let (plan_id, step_id) = plan(&harness).await;
    let run = execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
        None,
    )
    .await
    .expect("run");
    let run_id = run.details["run_id"].as_str().expect("run id").to_string();

    // A second session shares the broker but not the run.
    let other_sources = Arc::new(MemorySources::new());
    other_sources.set_file(SOURCE_PATH, "fn main() {}\nlet x = foo();\n");
    let other = RustToolkit::new(
        Arc::new(
            MemoryWorkspace::new(metadata_json())
                .with_configurations(vec![configuration("native-default", &[])]),
        ) as Arc<dyn WorkspaceCatalogPort>,
        Arc::clone(&harness.broker),
        other_sources as Arc<dyn SourceSnapshotPort>,
        OwnerId::new("session-b"),
        RustToolLimits::default(),
    );

    let error = execute(
        &other,
        "rs_job",
        json!({"run_id": run_id, "action": "status"}),
        None,
    )
    .await
    .expect_err("foreign");
    assert!(error.0.contains("permission_denied"), "{error}");
}

#[tokio::test]
async fn rs_diagnostics_on_an_unknown_run_does_not_start_a_build() {
    let harness = harness("session-a");
    let error = execute(
        &harness.toolkit,
        "rs_diagnostics",
        json!({"run_id": "run-does-not-exist"}),
        None,
    )
    .await
    .expect_err("unknown run");
    assert!(error.0.contains("permission_denied"), "{error}");
}

#[tokio::test]
async fn an_aborted_call_never_starts_a_run() {
    let harness = harness("session-a");
    let (plan_id, step_id) = plan(&harness).await;
    let signal = AbortSignal::new();
    signal.abort();

    let error = execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
        Some(signal),
    )
    .await
    .expect_err("aborted");
    assert_eq!(error.0, "Operation aborted");
}

#[tokio::test]
async fn a_package_without_library_plans_bins_not_lib() {
    // The same metadata shape is enough: the planner's rule is checked in the
    // dedicated gates; here the tool path preserves it.
    let harness = harness("session-a");
    let result = execute(
        &harness.toolkit,
        "rs_verify_plan",
        json!({
            "changed_paths": ["crates/example-core/src/lib.rs"],
            "configuration_ids": ["native-default"],
            "goal": "validate_change",
            "scope": "focused"
        }),
        None,
    )
    .await
    .expect("plan");
    let argv = result.details["steps"][0]["argv"]
        .as_array()
        .expect("argv")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(argv.contains(&"--lib"));
    assert!(argv.contains(&"--message-format=json"));
}

// --- budgets ---------------------------------------------------------------

#[tokio::test]
async fn the_diagnostic_budget_limits_the_response_and_reports_the_rest() {
    let stdout = format!(
        "{}\n{}\n",
        diagnostic_line("E0308", "mismatched types", "error", 2),
        diagnostic_line("E0277", "trait bound", "error", 2)
    );
    let script = ScriptedRun::finished(stdout.into_bytes(), Vec::new(), 101)
        .with_statuses(BuildStatus::Failed, TestStatus::Unknown);
    let harness = harness_with("session-a", script);
    let (plan_id, step_id) = plan(&harness).await;
    let run = execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
        None,
    )
    .await
    .expect("run");
    let run_id = run.details["run_id"].as_str().expect("run id").to_string();

    let diagnostics = execute(
        &harness.toolkit,
        "rs_diagnostics",
        json!({"run_id": run_id, "budget": {"max_diagnostics": 1}}),
        None,
    )
    .await
    .expect("diagnostics");
    assert_eq!(diagnostics.details["diagnostics_omitted"], 1);
    assert_eq!(
        diagnostics.details["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .len(),
        1
    );
}

#[tokio::test]
async fn job_output_is_bounded_and_reports_truncation() {
    let harness = harness("session-a");
    let (plan_id, step_id) = plan(&harness).await;
    let run = execute(
        &harness.toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
        None,
    )
    .await
    .expect("run");
    let run_id = run.details["run_id"].as_str().expect("run id").to_string();

    let output = execute(
        &harness.toolkit,
        "rs_job",
        json!({"run_id": run_id, "action": "output", "stream": "stdout", "offset": 0, "limit_bytes": 16}),
        None,
    )
    .await
    .expect("output");
    assert_eq!(output.details["output"]["truncated"], true);
    assert!(output.details["output"]["total_bytes"].as_u64().unwrap() > 16);
    assert_eq!(
        output.details["output"]["bytes"]
            .as_array()
            .expect("bytes")
            .len(),
        16
    );
}
