//! A deterministic information-cost measurement for the Rust tools
//! (design §13.2 step 2): the same diagnostic, measured as the raw build log
//! the baseline reads and as the normalized bundle `rs_diagnostics` returns.
//!
//! This is not a model comparison — it is the byte/token accounting that
//! precedes one, so the output budget is compared on the same input. The
//! design is explicit that a shorter body which forces more round trips is not
//! an improvement, so the live A/B (in `pillar-coding-agent`) measures the
//! round trips and success; this file pins the payload.
//!
//! Run with `cargo test -p pillar-agent --test rust_tools_measure -- --nocapture`
//! to print the table.

#![cfg(feature = "rust-tools")]

use std::sync::Arc;

use pillar_agent::rust_tools::{
    CargoJobBroker, MemoryBroker, MemorySources, MemoryWorkspace, OwnerId, RustToolLimits,
    RustToolkit, ScriptedRun, SourceSnapshotPort, WorkspaceCatalogPort,
};
use pillar_agent::types::AgentToolResult;
use serde_json::{Value, json};

const CORE: &str = "path+file:///ws#example-core@0.1.0";
const SOURCE: &str = "/ws/src/lib.rs";

/// A plausible human `cargo build` rendering of the fixture below: what the
/// baseline would read with `read`/`bash`.
fn human_log() -> String {
    "\
error[E0277]: the trait bound `Widget: Clone` is not satisfied
 --> /ws/src/lib.rs:2:14
  |
2 |     let w = Widget::new();
  |              ^^^^^^^^^^^^ the trait `Clone` is not implemented for `Widget`
  |
note: required by a bound in `duplicate`
 --> /ws/src/lib.rs:10:20
   |
10 | fn duplicate<T: Clone>(value: T) -> (T, T) {
   |                    ^^^^^ required by this bound in `duplicate`
   |
help: consider annotating `Widget` with `#[derive(Clone)]`
   |
 1 | #[derive(Clone)]
   | ++++++++++++++++

For more information about this error, try `rustc --explain E0277`.
error: could not compile `example-core` (lib) due to 1 previous error
"
    .to_string()
}

/// The JSON `compiler-message` for the same diagnostic.
fn compiler_message() -> String {
    let rendered = serde_json::to_string(&human_log()).expect("rendered");
    format!(
        r##"{{"reason":"compiler-message","package_id":"{CORE}","target":{{"name":"example_core"}},"message":{{"rendered":{rendered},"code":{{"code":"E0277"}},"level":"error","message":"the trait bound `Widget: Clone` is not satisfied","spans":[{{"file_name":"{SOURCE}","byte_start":20,"byte_end":32,"line_start":2,"line_end":2,"column_start":14,"column_end":26,"is_primary":true,"label":"the trait `Clone` is not implemented for `Widget`","suggested_replacement":"#[derive(Clone)]\n","suggestion_applicability":"MachineApplicable"}},{{"file_name":"{SOURCE}","byte_start":120,"byte_end":125,"line_start":10,"line_end":10,"column_start":20,"column_end":25,"is_primary":false,"label":"required by this bound in `duplicate`"}}],"children":[{{"code":null,"level":"note","message":"required by a bound in `duplicate`","spans":[],"children":[]}}]}}}}"##
    )
}

fn metadata_json() -> String {
    format!(
        r#"{{"packages":[{{"id":"{CORE}","name":"example-core","manifest_path":"/ws/Cargo.toml","targets":[{{"name":"example_core","kind":["lib"],"crate_types":["lib"],"src_path":"{SOURCE}","test":true,"doctest":true}}],"dependencies":[],"features":{{}}}}],"workspace_members":["{CORE}"],"workspace_default_members":["{CORE}"],"workspace_root":"/ws","target_directory":"/ws/target","metadata":null}}"#
    )
}

fn configuration() -> pillar_agent::rust_tools::Configuration {
    pillar_agent::rust_tools::Configuration {
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

fn toolkit(source_text: &str) -> RustToolkit {
    let workspace =
        Arc::new(MemoryWorkspace::new(metadata_json()).with_configurations(vec![configuration()]));
    let broker: Arc<dyn CargoJobBroker> = Arc::new(MemoryBroker::new(ScriptedRun::finished(
        format!("{}\n", compiler_message()).into_bytes(),
        Vec::new(),
        101,
    )));
    let sources = Arc::new(MemorySources::new());
    sources.set_file(SOURCE, source_text);
    RustToolkit::new(
        workspace as Arc<dyn WorkspaceCatalogPort>,
        broker,
        sources as Arc<dyn SourceSnapshotPort>,
        OwnerId::new("measure"),
        RustToolLimits::default(),
    )
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

/// A rough token estimate for comparison; the design requires an explicit byte
/// hard cap too, because no tokenizer is assumed (design §8.2).
fn estimated_tokens(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

async fn diagnostic_run(toolkit: &RustToolkit) -> String {
    let plan = execute(
        toolkit,
        "rs_verify_plan",
        json!({
            "changed_paths": ["src/lib.rs"],
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
        .expect("step")
        .to_string();
    let run = execute(
        toolkit,
        "rs_run",
        json!({"plan_id": plan_id, "step_id": step_id, "request_id": "rq1"}),
    )
    .await
    .expect("run");
    run.details["run_id"].as_str().expect("run id").to_string()
}

#[tokio::test]
async fn the_normalized_bundle_is_smaller_than_the_raw_log() {
    let source = "fn main() {\n    let w = Widget::new();\n    let (a, b) = duplicate(w);\n    println!(\"{a:?} {b:?}\");\n}\n\n#[derive(Debug)]\nstruct Widget;\nfn duplicate<T: Clone>(value: T) -> (T, T) {\n    (value.clone(), value)\n}\n";
    let toolkit = toolkit(source);
    let run_id = diagnostic_run(&toolkit).await;

    let with_source = execute(&toolkit, "rs_diagnostics", json!({"run_id": run_id}))
        .await
        .expect("diagnostics");
    let with_source_text = text(&with_source);
    let without_source = execute(
        &toolkit,
        "rs_diagnostics",
        json!({"run_id": run_id, "include": ["suggestions"]}),
    )
    .await
    .expect("diagnostics");
    let without_source_text = text(&without_source);

    let human = human_log();
    let json_line = compiler_message();
    let details_bytes = serde_json::to_string(&with_source.details)
        .expect("details")
        .len();

    let rows = [
        ("raw human log (baseline reads this)", human.len()),
        ("raw cargo JSON line", json_line.len()),
        (
            "rs_diagnostics content + source line",
            with_source_text.len(),
        ),
        (
            "rs_diagnostics content, no source",
            without_source_text.len(),
        ),
        (
            "rs_diagnostics details (must not reach the model)",
            details_bytes,
        ),
    ];
    println!("\n{:<50} {:>8} {:>10}", "payload", "bytes", "~tokens");
    for (name, bytes) in rows {
        println!("{:<50} {:>8} {:>10}", name, bytes, estimated_tokens(bytes));
    }

    // The normalized bundle is smaller than the human log it stands in for,
    // and the structured details (which carry `rendered`) do not flow.
    assert!(
        with_source_text.len() < human.len(),
        "the bundle ({} bytes) must be smaller than the raw human log ({} bytes)",
        with_source_text.len(),
        human.len()
    );
    assert!(with_source_text.len() < details_bytes);
    assert!(without_source_text.len() <= with_source_text.len());
    // The bundle still names the code, the message, the location and the
    // suggestion id, so the reduction is not achieved by dropping the evidence.
    assert!(with_source_text.contains("E0277"));
    assert!(with_source_text.contains("the trait bound"));
    assert!(with_source_text.contains("src/lib.rs:2:14"));
}
