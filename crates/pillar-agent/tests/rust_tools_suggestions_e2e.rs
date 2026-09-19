//! Suggestion application gates (design §8, stage R3): register a proposal
//! group from a run, preview it, apply it once under a strict host, and refuse
//! what the initial conditions exclude.
//!
//! The host is the experimental adapter's in-memory [`MemoryHost`] — the only
//! strict [`ConditionalStore`] in the workspace — so these are deterministic.

#![cfg(feature = "rust-tools")]

use std::sync::Arc;

use async_trait::async_trait;
use pillar_agent::exp::{
    ConditionalStore, ExpError, Guarantee, MemoryHost, OperationLedger, ResourceId, Revision,
    Snapshot,
};
use pillar_agent::rust_tools::{
    BuildStatus, CargoJobBroker, Configuration, MemoryBroker, MemorySources, MemoryWorkspace,
    OwnerId, RustToolLimits, RustToolkit, ScriptedRun, SourceSnapshotPort, SuggestionService,
    TestStatus, WorkspaceCatalogPort,
};
use pillar_agent::types::AgentToolResult;
use serde_json::{Value, json};

const CORE: &str = "path+file:///ws/crates/example-core#example-core@0.1.0";
const TARGET: &str = "/ws/f.txt";

fn metadata_json() -> String {
    format!(
        r#"{{
  "packages": [{{
    "id": "{CORE}",
    "name": "example-core",
    "manifest_path": "/ws/crates/example-core/Cargo.toml",
    "targets": [{{"name": "example_core", "kind": ["lib"], "crate_types": ["lib"],
      "src_path": "/ws/crates/example-core/src/lib.rs", "test": true, "doctest": true}}],
    "dependencies": [], "features": {{}}
  }}],
  "workspace_members": ["{CORE}"],
  "workspace_default_members": ["{CORE}"],
  "workspace_root": "/ws",
  "target_directory": "/ws/target",
  "metadata": null
}}"#
    )
}

fn configuration() -> Configuration {
    Configuration {
        id: "native-default".to_string(),
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
        lock_digest: Some("lock".to_string()),
        rustflags_digest: None,
        env_digest: None,
    }
}

fn diagnostic_line(
    file: &str,
    replacement: &str,
    applicability: &str,
    start: u64,
    end: u64,
) -> String {
    format!(
        r#"{{"reason":"compiler-message","package_id":"{CORE}","target":{{"name":"example_core"}},"message":{{"code":{{"code":"E0308"}},"level":"error","message":"mismatched types","spans":[{{"file_name":"{file}","byte_start":{start},"byte_end":{end},"line_start":1,"line_end":1,"column_start":1,"column_end":4,"is_primary":true,"label":"here","suggested_replacement":"{replacement}","suggestion_applicability":"{applicability}"}}]}}}}"#
    )
}

/// A host that can only promise weak publication.
struct WeakHost(MemoryHost);

#[async_trait]
impl ConditionalStore for WeakHost {
    fn guarantee(&self) -> Guarantee {
        Guarantee::Weak
    }
    async fn resolve(&self, path: &str) -> Result<ResourceId, ExpError> {
        self.0.resolve(path).await
    }
    async fn snapshot(&self, resource: &ResourceId) -> Result<Snapshot, ExpError> {
        self.0.snapshot(resource).await
    }
    async fn compare_and_swap(
        &self,
        resource: &ResourceId,
        expected: Revision,
        new_bytes: &[u8],
    ) -> Result<Revision, ExpError> {
        self.0.compare_and_swap(resource, expected, new_bytes).await
    }
}

/// Build a toolkit whose suggestion service uses `host` and whose broker
/// produces `line`.
fn toolkit(host: Arc<dyn ConditionalStore>, line: String) -> RustToolkit {
    let workspace =
        Arc::new(MemoryWorkspace::new(metadata_json()).with_configurations(vec![configuration()]));
    let broker: Arc<dyn CargoJobBroker> = Arc::new(MemoryBroker::new(
        ScriptedRun::finished(format!("{line}\n").into_bytes(), Vec::new(), 0)
            .with_statuses(BuildStatus::Succeeded, TestStatus::Unknown),
    ));
    let sources = Arc::new(MemorySources::new());
    sources.set_file(TARGET, "one two\n");
    let service = Arc::new(SuggestionService::new(
        host,
        Arc::new(OperationLedger::new(16)),
        16,
    ));
    RustToolkit::new(
        workspace as Arc<dyn WorkspaceCatalogPort>,
        broker,
        sources as Arc<dyn SourceSnapshotPort>,
        OwnerId::new("session-a"),
        RustToolLimits::default(),
    )
    .with_suggestions(service)
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
) -> Result<AgentToolResult, pillar_agent::types::ToolExecuteError> {
    let tool = toolkit
        .tools()
        .into_iter()
        .find(|candidate| candidate.tool.name == tool)
        .expect("tool exists");
    (tool.execute)("call-1".to_string(), args, None, None).await
}

/// Plan, run and read diagnostics; return the first suggestion notice.
async fn first_notice(toolkit: &RustToolkit) -> Value {
    let plan = execute(
        toolkit,
        "rs_verify_plan",
        json!({
            "changed_paths": ["crates/example-core/src/lib.rs"],
            "configuration_ids": ["native-default"],
            "goal": "validate_change",
            "scope": "focused"
        }),
    )
    .await
    .expect("plan");
    let plan_id = plan.details["plan_id"]
        .as_str()
        .expect("plan id")
        .to_string();
    let step_id = plan.details["steps"][0]["step_id"]
        .as_str()
        .expect("step id")
        .to_string();
    let run = execute(
        toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
    )
    .await
    .expect("run");
    let run_id = run.details["run_id"].as_str().expect("run id").to_string();
    let diagnostics = execute(toolkit, "rs_diagnostics", json!({"run_id": run_id}))
        .await
        .expect("diagnostics");
    diagnostics.details["diagnostics"][0]["suggestions"][0].clone()
}

fn strict_host() -> Arc<MemoryHost> {
    let host = Arc::new(MemoryHost::new());
    host.set_file(TARGET, b"one two\n");
    host
}

#[tokio::test]
async fn a_machine_applicable_suggestion_applies_once_and_reports_a_receipt() {
    let memory = strict_host();
    let toolkit = toolkit(
        memory.clone() as Arc<dyn ConditionalStore>,
        diagnostic_line(TARGET, "ONE", "MachineApplicable", 0, 3),
    );
    let notice = first_notice(&toolkit).await;
    assert_eq!(notice["applicable"], true, "{notice}");
    let id = notice["suggestion_id"].as_str().expect("id").to_string();

    let applied = execute(
        &toolkit,
        "rs_apply_suggestion",
        json!({"suggestion_id": id, "operation_id": "op1"}),
    )
    .await
    .expect("apply");
    assert!(text(&applied).contains("applied"));
    assert_eq!(applied.details["validated"], false);
    assert_eq!(memory.content(TARGET).as_deref(), Some(&b"ONE two\n"[..]));

    // A retry of the same operation id returns the same receipt and does not
    // apply twice.
    let retry = execute(
        &toolkit,
        "rs_apply_suggestion",
        json!({"suggestion_id": id, "operation_id": "op1"}),
    )
    .await
    .expect("retry");
    assert_eq!(applied.details, retry.details);
    assert_eq!(memory.content(TARGET).as_deref(), Some(&b"ONE two\n"[..]));

    // A new operation id against the now-stale revision conflicts.
    let conflict = execute(
        &toolkit,
        "rs_apply_suggestion",
        json!({"suggestion_id": id, "operation_id": "op2"}),
    )
    .await
    .expect_err("stale revision");
    assert!(conflict.0.contains("revision_conflict"), "{conflict}");
}

#[tokio::test]
async fn an_expected_preview_digest_mismatch_is_refused() {
    let memory = strict_host();
    let toolkit = toolkit(
        memory.clone() as Arc<dyn ConditionalStore>,
        diagnostic_line(TARGET, "ONE", "MachineApplicable", 0, 3),
    );
    let id = first_notice(&toolkit).await["suggestion_id"]
        .as_str()
        .expect("id")
        .to_string();
    let error = execute(
        &toolkit,
        "rs_apply_suggestion",
        json!({"suggestion_id": id, "operation_id": "op1", "expected_preview_digest": "not-it"}),
    )
    .await
    .expect_err("preview mismatch");
    assert!(error.0.contains("revision_conflict"), "{error}");
    assert_eq!(memory.content(TARGET).as_deref(), Some(&b"one two\n"[..]));
}

#[tokio::test]
async fn a_weak_host_registers_but_refuses_application() {
    let inner = MemoryHost::new();
    inner.set_file(TARGET, b"one two\n");
    let toolkit = toolkit(
        Arc::new(WeakHost(inner)) as Arc<dyn ConditionalStore>,
        diagnostic_line(TARGET, "ONE", "MachineApplicable", 0, 3),
    );
    let notice = first_notice(&toolkit).await;
    assert_eq!(notice["applicable"], false, "{notice}");
    assert!(
        notice["reason"]
            .as_str()
            .expect("reason")
            .contains("weak guarantee"),
        "{notice}"
    );
    let id = notice["suggestion_id"].as_str().expect("id").to_string();
    let error = execute(
        &toolkit,
        "rs_apply_suggestion",
        json!({"suggestion_id": id, "operation_id": "op1"}),
    )
    .await
    .expect_err("weak");
    assert!(error.0.contains("unsupported_suggestion"), "{error}");
}

#[tokio::test]
async fn a_zero_length_insertion_is_preview_only() {
    let memory = strict_host();
    let toolkit = toolkit(
        memory.clone() as Arc<dyn ConditionalStore>,
        diagnostic_line(TARGET, "X", "MachineApplicable", 3, 3),
    );
    let notice = first_notice(&toolkit).await;
    assert_eq!(notice["applicable"], false, "{notice}");
    assert!(
        notice["reason"]
            .as_str()
            .expect("reason")
            .contains("insertion"),
        "{notice}"
    );
    let id = notice["suggestion_id"].as_str().expect("id").to_string();
    let error = execute(
        &toolkit,
        "rs_apply_suggestion",
        json!({"suggestion_id": id, "operation_id": "op1"}),
    )
    .await
    .expect_err("insertion");
    assert!(error.0.contains("unsupported_suggestion"), "{error}");
    assert_eq!(memory.content(TARGET).as_deref(), Some(&b"one two\n"[..]));
}

#[tokio::test]
async fn a_non_machine_applicable_suggestion_is_preview_only() {
    let memory = strict_host();
    let toolkit = toolkit(
        memory.clone() as Arc<dyn ConditionalStore>,
        diagnostic_line(TARGET, "ONE", "MaybeIncorrect", 0, 3),
    );
    let notice = first_notice(&toolkit).await;
    assert_eq!(notice["applicable"], false, "{notice}");
    assert!(
        notice["reason"]
            .as_str()
            .expect("reason")
            .contains("MachineApplicable"),
        "{notice}"
    );
}

#[tokio::test]
async fn a_macro_virtual_span_is_preview_only() {
    let memory = strict_host();
    let toolkit = toolkit(
        memory.clone() as Arc<dyn ConditionalStore>,
        diagnostic_line("<vec macros>", "ONE", "MachineApplicable", 0, 3),
    );
    let notice = first_notice(&toolkit).await;
    assert_eq!(notice["applicable"], false, "{notice}");
    assert!(
        notice["reason"]
            .as_str()
            .expect("reason")
            .contains("workspace file"),
        "{notice}"
    );
    assert_eq!(memory.content(TARGET).as_deref(), Some(&b"one two\n"[..]));
}
