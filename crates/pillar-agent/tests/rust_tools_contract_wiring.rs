//! The R2 wiring: `rs_diagnostics` issues a versioned position reference and
//! `rs_contract` consumes it (design §6). The semantic provider is a fake, so
//! the flow is deterministic and needs no analyzer.

#![cfg(feature = "rust-tools")]

use std::sync::Arc;

use async_trait::async_trait;
use pillar_agent::exp::Revision;
use pillar_agent::rust_tools::{
    AnalysisAvailability, CargoJobBroker, Configuration, ContractLimits, ContractService,
    ContractSlice, Declaration, MemoryBroker, MemorySources, MemoryWorkspace, OwnerId, Provenance,
    RustToolError, RustToolLimits, RustToolkit, ScriptedRun, SemanticProvider, SemanticQuery,
    SourceSnapshotPort, WorkspaceCatalogPort,
};
use pillar_agent::types::AgentToolResult;
use serde_json::{Value, json};

const CORE: &str = "path+file:///ws/crates/example-core#example-core@0.1.0";
const SOURCE: &str = "/ws/lib.rs";

fn metadata_json() -> String {
    format!(
        r#"{{
  "packages": [{{
    "id": "{CORE}",
    "name": "example-core",
    "manifest_path": "/ws/crates/example-core/Cargo.toml",
    "targets": [{{"name": "example_core", "kind": ["lib"], "crate_types": ["lib"],
      "src_path": "{SOURCE}", "test": true, "doctest": true}}],
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

fn diagnostic_line() -> String {
    format!(
        r#"{{"reason":"compiler-message","package_id":"{CORE}","target":{{"name":"example_core"}},"message":{{"code":{{"code":"E0308"}},"level":"error","message":"mismatched types","spans":[{{"file_name":"{SOURCE}","byte_start":0,"byte_end":2,"line_start":1,"line_end":1,"column_start":1,"column_end":3,"is_primary":true}}]}}}}"#
    )
}

struct FakeProvider;

#[async_trait]
impl SemanticProvider for FakeProvider {
    fn availability(&self) -> AnalysisAvailability {
        AnalysisAvailability::Available
    }

    async fn contract(&self, query: SemanticQuery) -> Result<ContractSlice, RustToolError> {
        let mut slice = ContractSlice::empty(&query.path, Revision::new(1, b""));
        slice.declaration = Some(Declaration {
            kind: "fn".to_string(),
            name: "f".to_string(),
            signature: Some("fn f()".to_string()),
            generics: Vec::new(),
            where_clauses: Vec::new(),
            provenance: Provenance::Declared,
            span: None,
        });
        Ok(slice)
    }
}

fn toolkit() -> RustToolkit {
    let workspace =
        Arc::new(MemoryWorkspace::new(metadata_json()).with_configurations(vec![configuration()]));
    let broker: Arc<dyn CargoJobBroker> = Arc::new(MemoryBroker::new(ScriptedRun::finished(
        format!("{}\n", diagnostic_line()).into_bytes(),
        Vec::new(),
        1,
    )));
    let sources = Arc::new(MemorySources::new());
    sources.set_file(SOURCE, "fn f() {}\n");
    let contract = Arc::new(ContractService::new(
        Arc::new(FakeProvider),
        sources.clone() as Arc<dyn SourceSnapshotPort>,
        ContractLimits::default(),
    ));
    RustToolkit::new(
        workspace as Arc<dyn WorkspaceCatalogPort>,
        broker,
        sources as Arc<dyn SourceSnapshotPort>,
        OwnerId::new("session-a"),
        RustToolLimits::default(),
    )
    .with_contract(contract)
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

#[tokio::test]
async fn a_diagnostic_position_ref_is_consumed_by_rs_contract() {
    let toolkit = toolkit();
    let plan = execute(
        &toolkit,
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
        &toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
    )
    .await
    .expect("run");
    let run_id = run.details["run_id"].as_str().expect("run id").to_string();

    let diagnostics = execute(&toolkit, "rs_diagnostics", json!({"run_id": run_id}))
        .await
        .expect("diagnostics");
    let position_ref = diagnostics.details["diagnostics"][0]["position_ref"]
        .as_str()
        .expect("a position reference")
        .to_string();
    assert!(position_ref.starts_with("src"), "{position_ref}");

    let contract = execute(
        &toolkit,
        "rs_contract",
        json!({"position_ref": position_ref, "include": ["signature"]}),
    )
    .await
    .expect("contract");
    assert!(text(&contract).contains("fn f"), "{}", text(&contract));
    assert_eq!(contract.details["slice"]["declaration"]["name"], "f");
}
