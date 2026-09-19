//! rust-analyzer adapter gates (design §6, stage R2): the provider's
//! request/response mapping and position-encoding conversion are verified with
//! a fake [`LanguageServer`], so no rust-analyzer install is required.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pillar_agent::exp::Revision;
use pillar_agent::rust_tools::{
    PositionEncoding, RustToolError, RustToolErrorCode, SemanticProvider, SemanticQuery,
};
use pillar_coding_agent::core::rust_analyzer::{
    DocumentReader, LanguageServer, LspProcess, RustAnalyzerConfig, RustAnalyzerProvider,
};

struct FakeServer {
    requests: Mutex<Vec<(String, serde_json::Value)>>,
    notifications: Mutex<Vec<(String, serde_json::Value)>>,
    encoding: PositionEncoding,
}

impl FakeServer {
    fn new(encoding: PositionEncoding) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            notifications: Mutex::new(Vec::new()),
            encoding,
        }
    }
}

#[async_trait]
impl LanguageServer for FakeServer {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RustToolError> {
        self.requests
            .lock()
            .unwrap()
            .push((method.to_string(), params));
        match method {
            "textDocument/definition" => Ok(serde_json::json!([
                {"uri": "file:///ws/other.rs", "range": {"start": {"line": 5, "character": 0}}}
            ])),
            "textDocument/implementation" => Ok(serde_json::json!([
                {"uri": "file:///ws/impl.rs", "range": {"start": {"line": 1, "character": 0}}}
            ])),
            "textDocument/hover" => Ok(serde_json::json!({
                "contents": {"kind": "markdown", "value": "```rust\nstruct T;\n```"}
            })),
            other => Err(RustToolError::host_failure(format!("unexpected {other}"))),
        }
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), RustToolError> {
        self.notifications
            .lock()
            .unwrap()
            .push((method.to_string(), params));
        Ok(())
    }

    fn position_encoding(&self) -> PositionEncoding {
        self.encoding
    }

    fn server_version(&self) -> Option<String> {
        Some("fake-analyzer 0.1".to_string())
    }
}

struct MapDocuments(HashMap<String, String>);

impl MapDocuments {
    fn new(entries: &[(&str, &str)]) -> Self {
        Self(
            entries
                .iter()
                .map(|(path, text)| (path.to_string(), text.to_string()))
                .collect(),
        )
    }
}

impl DocumentReader for MapDocuments {
    fn read(&self, path: &str) -> Result<String, RustToolError> {
        self.0
            .get(path)
            .cloned()
            .ok_or_else(|| RustToolError::source_unbound("the document is not in this host"))
    }
}

fn query(path: &str, line: u32, character: u32, encoding: PositionEncoding) -> SemanticQuery {
    SemanticQuery {
        path: path.to_string(),
        revision: Revision::new(7, b""),
        line,
        character,
        encoding,
        include: Vec::new(),
        budget: Default::default(),
    }
}

#[tokio::test]
async fn the_provider_maps_lsp_responses_to_a_contract_slice() {
    let server = Arc::new(FakeServer::new(PositionEncoding::Utf16));
    let documents = Arc::new(MapDocuments::new(&[("/ws/lib.rs", "fn f() {}\n")]));
    let provider =
        RustAnalyzerProvider::with_server(server.clone(), documents, Some("/ws".to_string()));

    let slice = provider
        .contract(query("/ws/lib.rs", 0, 3, PositionEncoding::Utf16))
        .await
        .expect("contract");

    assert!(
        slice
            .declaration
            .as_ref()
            .and_then(|declaration| declaration.signature.as_deref())
            .is_some_and(|signature| signature.contains("struct T")),
        "hover becomes the signature: {:?}",
        slice.declaration
    );
    assert_eq!(slice.types[0].path.as_deref(), Some("/ws/other.rs"));
    assert_eq!(slice.impls[0].path.as_deref(), Some("/ws/impl.rs"));
    assert!(
        slice
            .unresolved
            .iter()
            .any(|item| item.what == "where_clauses"),
        "structured where-clauses are declared unresolved"
    );

    let notifications = server.notifications.lock().unwrap();
    let did_open = notifications
        .iter()
        .find(|(method, _)| method == "textDocument/didOpen")
        .expect("didOpen");
    assert_eq!(did_open.1["textDocument"]["version"], 7);
    assert_eq!(did_open.1["textDocument"]["text"], "fn f() {}\n");

    let requests = server.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|(method, _)| method == "textDocument/definition")
    );
}

#[tokio::test]
async fn the_provider_converts_the_position_encoding() {
    // The query is in UTF-8 bytes; the server negotiated UTF-16.
    let server = Arc::new(FakeServer::new(PositionEncoding::Utf16));
    let text = "日本語x";
    let documents = Arc::new(MapDocuments::new(&[("/ws/lib.rs", text)]));
    let provider = RustAnalyzerProvider::with_server(server.clone(), documents, None);

    provider
        .contract(query("/ws/lib.rs", 0, 9, PositionEncoding::Utf8))
        .await
        .expect("contract");

    let requests = server.requests.lock().unwrap();
    let (_, params) = requests
        .iter()
        .find(|(method, _)| method == "textDocument/definition")
        .expect("definition");
    assert_eq!(
        params["position"]["character"], 3,
        "9 UTF-8 bytes after 日本語 is UTF-16 character 3"
    );
}

#[tokio::test]
async fn starting_a_missing_server_reports_analysis_unavailable() {
    let config = RustAnalyzerConfig {
        command: vec!["definitely-not-rust-analyzer-xyz".to_string()],
        workspace_root: ".".to_string(),
        timeout_ms: 500,
    };
    let error = LspProcess::start(&config).await.expect_err("missing");
    assert_eq!(error.code, RustToolErrorCode::AnalysisUnavailable);
}
