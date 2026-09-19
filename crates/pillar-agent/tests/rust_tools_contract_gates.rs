//! Contract gates (design §6, stage R2): the semantic provider is optional,
//! the budget is enforced, and an unavailable provider falls back to source
//! text instead of pretending to resolve.
//!
//! A fake provider keeps these deterministic; the LSP adapter is a separate
//! host concern.

#![cfg(feature = "rust-tools")]

use std::sync::Arc;

use async_trait::async_trait;
use pillar_agent::exp::Revision;
use pillar_agent::rust_tools::{
    AnalysisAvailability, CargoJobBroker, ContractBudget, ContractLimits, ContractRequest,
    ContractService, ContractSlice, Declaration, ImplCandidate, MemoryBroker, MemorySources,
    MemoryWorkspace, OwnerId, PositionEncoding, Provenance, RustToolError, RustToolErrorCode,
    RustToolLimits, RustToolkit, ScriptedRun, SemanticProvider, SemanticQuery, SourcePosition,
    SourceSnapshotPort, TypeDefinition, UnavailableProvider, WorkspaceCatalogPort,
};
use pillar_agent::types::AgentToolResult;
use serde_json::json;

const PATH: &str = "/ws/lib.rs";
const TEXT: &str = "fn f() -> T {}\n";

struct FakeProvider {
    availability: AnalysisAvailability,
    slice: ContractSlice,
}

#[async_trait]
impl SemanticProvider for FakeProvider {
    fn availability(&self) -> AnalysisAvailability {
        self.availability.clone()
    }

    async fn contract(&self, _query: SemanticQuery) -> Result<ContractSlice, RustToolError> {
        Ok(self.slice.clone())
    }
}

fn available_slice() -> ContractSlice {
    ContractSlice {
        path: PATH.to_string(),
        revision: Revision::new(1, TEXT.as_bytes()),
        configuration: Some("native-default".to_string()),
        declaration: Some(Declaration {
            kind: "fn".to_string(),
            name: "f".to_string(),
            signature: Some("fn f() -> T".to_string()),
            generics: vec!["T".to_string()],
            where_clauses: vec!["T: Clone".to_string()],
            provenance: Provenance::Declared,
            span: None,
        }),
        types: vec![TypeDefinition {
            name: "T".to_string(),
            path: Some("crate::T".to_string()),
            definition: Some("struct T;".to_string()),
        }],
        impls: vec![ImplCandidate {
            text: "impl Clone for T".to_string(),
            path: Some("crate::t".to_string()),
            selected: true,
        }],
        unresolved: Vec::new(),
        truncated: false,
        source_excerpt: None,
    }
}

fn sources() -> Arc<MemorySources> {
    let sources = Arc::new(MemorySources::new());
    sources.set_file(PATH, TEXT);
    sources
}

fn position() -> SourcePosition {
    SourcePosition {
        path: PATH.to_string(),
        revision: Revision::new(1, TEXT.as_bytes()),
        line: 0,
        character: 3,
        encoding: PositionEncoding::Utf16,
    }
}

fn service(provider: Arc<dyn SemanticProvider>) -> ContractService {
    ContractService::new(
        provider,
        sources() as Arc<dyn SourceSnapshotPort>,
        ContractLimits::default(),
    )
}

#[tokio::test]
async fn an_available_provider_returns_the_declared_contract() {
    let service = service(Arc::new(FakeProvider {
        availability: AnalysisAvailability::Available,
        slice: available_slice(),
    }));
    let reference = service.issue_position(position());
    let response = service
        .contract(&ContractRequest {
            position_ref: reference,
            include: vec!["signature".to_string()],
            budget: ContractBudget::default(),
        })
        .await
        .expect("contract");

    assert!(response.availability.is_available());
    assert!(!response.fallback_to_source);
    let declaration = response.slice.declaration.expect("declaration");
    assert_eq!(declaration.name, "f");
    assert_eq!(declaration.provenance, Provenance::Declared);
    assert_eq!(declaration.where_clauses, vec!["T: Clone".to_string()]);
    assert!(response.slice.impls[0].selected);
}

#[tokio::test]
async fn the_node_budget_truncates_and_says_so() {
    let mut slice = available_slice();
    slice.types = (0..4)
        .map(|index| TypeDefinition {
            name: format!("T{index}"),
            path: None,
            definition: None,
        })
        .collect();
    // declaration(1) + types(4) + impl(1) = 6 nodes.
    let service = service(Arc::new(FakeProvider {
        availability: AnalysisAvailability::Available,
        slice,
    }));
    let reference = service.issue_position(position());
    let response = service
        .contract(&ContractRequest {
            position_ref: reference,
            include: Vec::new(),
            budget: ContractBudget {
                max_nodes: Some(2),
                ..Default::default()
            },
        })
        .await
        .expect("contract");

    assert!(response.slice.truncated);
    assert!(response.slice.node_count() <= 2);
    assert!(
        response
            .slice
            .unresolved
            .iter()
            .any(|item| item.reason.contains("node budget")),
        "{:?}",
        response.slice.unresolved
    );
}

#[tokio::test]
async fn an_unavailable_provider_falls_back_to_the_source_line() {
    let service = service(Arc::new(UnavailableProvider::default()));
    let reference = service.issue_position(position());
    let response = service
        .contract(&ContractRequest {
            position_ref: reference,
            include: Vec::new(),
            budget: ContractBudget::default(),
        })
        .await
        .expect("contract");

    assert!(!response.availability.is_available());
    assert!(response.fallback_to_source);
    assert_eq!(response.slice.source_excerpt.as_deref(), Some(TEXT));
    assert!(
        response
            .slice
            .unresolved
            .iter()
            .any(|item| item.what == "semantic analysis")
    );
}

#[tokio::test]
async fn an_unknown_position_reference_is_refused() {
    let service = service(Arc::new(UnavailableProvider::default()));
    let error = service
        .contract(&ContractRequest {
            position_ref: "src-does-not-exist".to_string(),
            include: Vec::new(),
            budget: ContractBudget::default(),
        })
        .await
        .expect_err("unknown");
    assert_eq!(error.code, RustToolErrorCode::InvalidRequest);
}

fn metadata_json() -> String {
    r#"{"packages":[],"workspace_members":[],"workspace_default_members":[],"workspace_root":"/ws","target_directory":"/ws/target"}"#.to_string()
}

fn toolkit(contract: Arc<ContractService>) -> RustToolkit {
    let workspace = Arc::new(MemoryWorkspace::new(metadata_json()));
    let broker: Arc<dyn CargoJobBroker> = Arc::new(MemoryBroker::new(ScriptedRun::finished(
        Vec::new(),
        Vec::new(),
        0,
    )));
    RustToolkit::new(
        workspace as Arc<dyn WorkspaceCatalogPort>,
        broker,
        sources() as Arc<dyn SourceSnapshotPort>,
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

#[tokio::test]
async fn the_rs_contract_tool_is_registered_and_reads_through_the_service() {
    let service = Arc::new(service(Arc::new(FakeProvider {
        availability: AnalysisAvailability::Available,
        slice: available_slice(),
    })));
    let reference = service.issue_position(position());
    let toolkit = toolkit(Arc::clone(&service));

    let names: Vec<String> = toolkit
        .tools()
        .into_iter()
        .map(|tool| tool.tool.name)
        .collect();
    assert!(names.contains(&"rs_contract".to_string()));

    let tool = toolkit
        .tools()
        .into_iter()
        .find(|tool| tool.tool.name == "rs_contract")
        .expect("tool");
    let result = (tool.execute)(
        "call-1".to_string(),
        json!({"position_ref": reference}),
        None,
        None,
    )
    .await
    .expect("execute");
    assert!(text(&result).contains("fn f"), "{}", text(&result));
    assert_eq!(
        result.details["slice"]["declaration"]["provenance"],
        "declared"
    );
}

#[tokio::test]
async fn the_rs_contract_tool_reports_the_source_fallback() {
    let service = Arc::new(service(Arc::new(UnavailableProvider::default())));
    let reference = service.issue_position(position());
    let toolkit = toolkit(service);
    let tool = toolkit
        .tools()
        .into_iter()
        .find(|tool| tool.tool.name == "rs_contract")
        .expect("tool");
    let result = (tool.execute)(
        "call-1".to_string(),
        json!({"position_ref": reference}),
        None,
        None,
    )
    .await
    .expect("execute");
    assert_eq!(result.details["fallback_to_source"], true);
    assert!(text(&result).contains("unavailable"), "{}", text(&result));
}
