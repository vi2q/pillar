//! The rust-analyzer adapter for `rs_contract` (design §6, stage R2).
//!
//! The design treats rust-analyzer as an **optional** provider: when it is
//! missing, the contract service falls back to source text, so a failed start
//! is never a planning failure. This module provides:
//!
//! - [`LspProcess`], a minimal LSP client over the server's stdio (JSON-RPC
//!   framing, an `initialize` handshake with `general.positionEncodings`, and
//!   request/response matching with a timeout);
//! - [`LanguageServer`], the transport the provider depends on, so the
//!   provider's request/response mapping is verified with a fake server
//!   without rust-analyzer installed;
//! - [`RustAnalyzerProvider`], which implements
//!   [`SemanticProvider`](pillar_agent::rust_tools::SemanticProvider).
//!
//! What the adapter deliberately does **not** do: it does not reinterpret a
//! hover string as a structured type AST, and it does not pretend standard LSP
//! provides complete where-clauses or the impl a concrete call selected — those
//! are reported in `unresolved` (design §6). It also does not start a server
//! on its own initiative: `start` is an explicit host call.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use pillar_agent::rust_tools::{
    AnalysisAvailability, ContractSlice, Declaration, ImplCandidate, Provenance, RustToolError,
    SemanticProvider, SemanticQuery, TypeDefinition, UnresolvedItem,
};
use pillar_agent::rust_tools::{LineIndex, LspPosition, PositionEncoding};

/// How much of the server's stderr is retained for a failure message.
const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// A child-process server the adapter can drive.
///
/// The provider depends on this rather than on `Child`, so its mapping is
/// testable with a fake server (design §6: the server is an optional adapter).
#[async_trait]
pub trait LanguageServer: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value, RustToolError>;
    async fn notify(&self, method: &str, params: Value) -> Result<(), RustToolError>;
    /// The encoding the server negotiated for positions.
    fn position_encoding(&self) -> PositionEncoding;
    /// `serverInfo`, for associating a result with the server that produced it.
    fn server_version(&self) -> Option<String>;
}

/// Host configuration for starting the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustAnalyzerConfig {
    /// argv, e.g. `["rust-analyzer"]`.
    pub command: Vec<String>,
    pub workspace_root: String,
    /// Per-request timeout.
    pub timeout_ms: u64,
}

impl Default for RustAnalyzerConfig {
    fn default() -> Self {
        Self {
            command: vec!["rust-analyzer".to_string()],
            workspace_root: ".".to_string(),
            timeout_ms: 30_000,
        }
    }
}

/// Reads a whole document for `didOpen` (the analyzer needs the text).
pub trait DocumentReader: Send + Sync {
    fn read(&self, path: &str) -> Result<String, RustToolError>;
}

// --- framing ---------------------------------------------------------------

fn write_frame(writer: &mut impl Write, message: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }
    let Some(length) = content_length else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing Content-Length header",
        ));
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let message = serde_json::from_slice(&body).map_err(std::io::Error::other)?;
    Ok(Some(message))
}

// --- process client --------------------------------------------------------

type Pending = Arc<Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Result<Value, RustToolError>>>>>;

/// A minimal LSP client over one child process.
pub struct LspProcess {
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicI64,
    child: Mutex<Child>,
    encoding: Mutex<PositionEncoding>,
    version: Mutex<Option<String>>,
    timeout_ms: u64,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
}

impl LspProcess {
    /// Spawn the server and run the `initialize`/`initialized` handshake.
    pub async fn start(config: &RustAnalyzerConfig) -> Result<Self, RustToolError> {
        let program = config.command.first().cloned().ok_or_else(|| {
            RustToolError::analysis_unavailable("no rust-analyzer command configured")
        })?;
        let mut command = Command::new(&program);
        command
            .args(config.command.iter().skip(1))
            .current_dir(&config.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            RustToolError::analysis_unavailable(format!("could not start {program}: {error}"))
        })?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));

        {
            let pending = Arc::clone(&pending);
            std::thread::spawn(move || read_loop(stdout, pending));
        }
        {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut chunk = [0u8; 4096];
                while let Ok(read) = reader.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    tail.extend_from_slice(&chunk[..read]);
                    if tail.len() > STDERR_TAIL_BYTES {
                        let excess = tail.len() - STDERR_TAIL_BYTES;
                        tail.drain(..excess);
                    }
                }
            });
        }

        let process = Self {
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicI64::new(1),
            child: Mutex::new(child),
            encoding: Mutex::new(PositionEncoding::Utf16),
            version: Mutex::new(None),
            timeout_ms: config.timeout_ms,
            stderr_tail,
        };

        let initialize = process
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": path_to_uri(&config.workspace_root),
                    "capabilities": {
                        "general": { "positionEncodings": ["utf-16"] },
                        "textDocument": {
                            "hover": { "contentFormat": ["markdown", "plaintext"] },
                            "definition": { "linkSupport": true },
                            "implementation": { "linkSupport": true }
                        }
                    }
                }),
            )
            .await?;
        let encoding = match initialize
            .pointer("/capabilities/positionEncoding")
            .and_then(|value| value.as_str())
        {
            Some("utf-8") => PositionEncoding::Utf8,
            Some("utf-32") => PositionEncoding::Utf32,
            _ => PositionEncoding::Utf16,
        };
        *process.encoding.lock().expect("encoding lock") = encoding;
        if let Some(name) = initialize
            .pointer("/serverInfo/name")
            .and_then(|v| v.as_str())
        {
            let version = initialize
                .pointer("/serverInfo/version")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            *process.version.lock().expect("version lock") =
                Some(format!("{name} {version}").trim().to_string());
        }
        process.notify("initialized", json!({})).await?;
        Ok(process)
    }

    fn send(&self, message: &Value) -> Result<(), RustToolError> {
        let mut stdin = self
            .stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(&mut *stdin, message)
            .map_err(|error| RustToolError::host_failure(format!("lsp write failed: {error}")))
    }

    /// The retained stderr tail, for a failure message.
    pub fn stderr_tail(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&tail).into_owned()
    }
}

#[async_trait]
impl LanguageServer for LspProcess {
    async fn request(&self, method: &str, params: Value) -> Result<Value, RustToolError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, sender);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        match tokio::time::timeout(Duration::from_millis(self.timeout_ms), receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                Err(RustToolError::analysis_unavailable(format!(
                    "the semantic server closed during `{method}`"
                )))
            }
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                Err(RustToolError::analysis_unavailable(format!(
                    "the semantic server timed out on `{method}`"
                )))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), RustToolError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn position_encoding(&self) -> PositionEncoding {
        *self
            .encoding
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn server_version(&self) -> Option<String> {
        self.version
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl std::fmt::Debug for LspProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspProcess")
            .field("server_version", &self.server_version())
            .finish_non_exhaustive()
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_loop(stdout: ChildStdout, pending: Pending) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader) {
            Ok(Some(message)) => {
                let Some(id) = message.get("id").and_then(|value| value.as_i64()) else {
                    // A notification (e.g. publishDiagnostics): not needed here.
                    continue;
                };
                let sender = pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                let Some(sender) = sender else {
                    continue;
                };
                let outcome = if let Some(error) = message.get("error") {
                    Err(RustToolError::host_failure(format!(
                        "lsp error {}: {}",
                        error.get("code").and_then(|v| v.as_i64()).unwrap_or(0),
                        error.get("message").and_then(|v| v.as_str()).unwrap_or("")
                    )))
                } else {
                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = sender.send(outcome);
            }
            Ok(None) | Err(_) => {
                let mut pending = pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for (_, sender) in pending.drain() {
                    let _ = sender.send(Err(RustToolError::analysis_unavailable(
                        "the semantic server stopped",
                    )));
                }
                break;
            }
        }
    }
}

// --- provider --------------------------------------------------------------

/// The `SemanticProvider` over a [`LanguageServer`].
pub struct RustAnalyzerProvider {
    server: Arc<dyn LanguageServer>,
    documents: Arc<dyn DocumentReader>,
    workspace_root: Option<String>,
}

impl RustAnalyzerProvider {
    /// Start rust-analyzer and return a ready provider.
    pub async fn start(
        config: RustAnalyzerConfig,
        documents: Arc<dyn DocumentReader>,
    ) -> Result<Self, RustToolError> {
        let workspace_root = config.workspace_root.clone();
        let server = Arc::new(LspProcess::start(&config).await?);
        Ok(Self {
            server,
            documents,
            workspace_root: Some(workspace_root),
        })
    }

    /// A provider over an injected server (tests, and a host that owns the
    /// server lifecycle).
    pub fn with_server(
        server: Arc<dyn LanguageServer>,
        documents: Arc<dyn DocumentReader>,
        workspace_root: Option<String>,
    ) -> Self {
        Self {
            server,
            documents,
            workspace_root,
        }
    }

    /// The server that answered, so a result can be associated with it
    /// (design §6: server版・文書版を結果に紐付ける).
    pub fn server_version(&self) -> Option<String> {
        self.server.server_version()
    }
}

#[async_trait]
impl SemanticProvider for RustAnalyzerProvider {
    fn availability(&self) -> AnalysisAvailability {
        AnalysisAvailability::Available
    }

    async fn contract(&self, query: SemanticQuery) -> Result<ContractSlice, RustToolError> {
        let text = self.documents.read(&query.path)?;
        let uri = path_to_uri(&query.path);
        let position = self.server_position(&text, &query)?;
        self.server
            .notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": "rust",
                        "version": query.revision.generation,
                        "text": text
                    }
                }),
            )
            .await?;

        let location_params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": position.line, "character": position.character }
        });
        let definition = self
            .server
            .request("textDocument/definition", location_params.clone())
            .await?;
        let implementation = self
            .server
            .request("textDocument/implementation", location_params.clone())
            .await?;
        let hover = self
            .server
            .request("textDocument/hover", location_params)
            .await?;

        let mut slice = ContractSlice::empty(&query.path, query.revision);
        slice.configuration = self.workspace_root.clone();
        if let Some(text) = hover_text(&hover) {
            slice.declaration = Some(Declaration {
                kind: "symbol".to_string(),
                name: String::new(),
                signature: Some(text),
                generics: Vec::new(),
                where_clauses: Vec::new(),
                // A hover string is what the server inferred/rendered, not a
                // declared AST (design §6).
                provenance: Provenance::Inferred,
                span: None,
            });
        }
        for (uri, line) in locations(&definition) {
            let path = uri_to_path(&uri);
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            slice.types.push(TypeDefinition {
                name,
                path: Some(path),
                definition: line.map(|line| format!("line {}", line + 1)),
            });
        }
        for (uri, _) in locations(&implementation) {
            let path = uri_to_path(&uri);
            slice.impls.push(ImplCandidate {
                text: path.clone(),
                path: Some(path),
                selected: false,
            });
        }
        slice.unresolved.push(UnresolvedItem {
            what: "where_clauses".to_string(),
            reason: "standard LSP hover does not provide structured where clauses".to_string(),
        });
        slice.unresolved.push(UnresolvedItem {
            what: "related_impls".to_string(),
            reason: "standard LSP does not say which impl a concrete call selected".to_string(),
        });
        Ok(slice)
    }
}

impl RustAnalyzerProvider {
    fn server_position(
        &self,
        text: &str,
        query: &SemanticQuery,
    ) -> Result<LspPosition, RustToolError> {
        let server_encoding = self.server.position_encoding();
        if server_encoding == query.encoding {
            return Ok(LspPosition {
                line: query.line,
                character: query.character,
            });
        }
        let index = LineIndex::new(text);
        let byte = index
            .lsp_to_byte(
                text,
                LspPosition {
                    line: query.line,
                    character: query.character,
                },
                query.encoding,
            )
            .ok_or_else(|| {
                RustToolError::invalid_request("the position is not in this document")
            })?;
        index
            .byte_to_lsp(text, byte, server_encoding)
            .ok_or_else(|| RustToolError::invalid_request("the position is not in this document"))
    }
}

// --- response mapping ------------------------------------------------------

/// Extract `(uri, line)` from a definition/implementation response: either a
/// `Location`, a `Location[]`, or `LocationLink[]`.
fn locations(value: &Value) -> Vec<(String, Option<i64>)> {
    let items: Vec<Value> = match value {
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    };
    let mut out = Vec::new();
    for item in items {
        if let Some(uri) = item.get("uri").and_then(|value| value.as_str()) {
            out.push((
                uri.to_string(),
                item.pointer("/range/start/line").and_then(Value::as_i64),
            ));
        } else if let Some(uri) = item.get("targetUri").and_then(|value| value.as_str()) {
            out.push((
                uri.to_string(),
                item.pointer("/targetSelectionRange/start/line")
                    .and_then(Value::as_i64),
            ));
        }
    }
    out
}

fn hover_text(value: &Value) -> Option<String> {
    let contents = value.get("contents")?;
    match contents {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => object
            .get("value")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(text.clone()),
                    Value::Object(object) => object
                        .get("value")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    _ => None,
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

/// `file://` URI helpers. Percent-encoding is not applied: the paths this host
/// works with are plain ASCII workspace paths.
pub fn path_to_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        format!("file://{normalized}")
    } else {
        format!("file:///{normalized}")
    }
}

pub fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_frame_round_trips() {
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &message).expect("write");
        let mut cursor = Cursor::new(buffer);
        let read = read_frame(&mut cursor).expect("read").expect("a frame");
        assert_eq!(read, message);
        // EOF after the frame.
        assert!(read_frame(&mut cursor).expect("eof").is_none());
    }

    #[test]
    fn a_missing_content_length_is_an_error() {
        let mut cursor = Cursor::new(b"X-Header: 1\r\n\r\n{}".to_vec());
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn a_response_error_is_extracted_from_locations_and_hover() {
        let definition = json!([
            {"uri": "file:///ws/a.rs", "range": {"start": {"line": 4, "character": 0}}},
            {"targetUri": "file:///ws/b.rs", "targetSelectionRange": {"start": {"line": 9, "character": 1}}}
        ]);
        let locations = locations(&definition);
        assert_eq!(locations[0].0, "file:///ws/a.rs");
        assert_eq!(locations[0].1, Some(4));
        assert_eq!(locations[1].0, "file:///ws/b.rs");
        assert_eq!(locations[1].1, Some(9));

        assert_eq!(
            hover_text(&json!({"contents": {"kind": "markdown", "value": "```rust\nT\n```"}}))
                .as_deref(),
            Some("```rust\nT\n```")
        );
        assert_eq!(
            hover_text(&json!({"contents": "plain"})).as_deref(),
            Some("plain")
        );
    }

    #[test]
    fn uri_helpers_round_trip_an_absolute_path() {
        assert_eq!(path_to_uri("/ws/lib.rs"), "file:///ws/lib.rs");
        assert_eq!(uri_to_path("file:///ws/lib.rs"), "/ws/lib.rs");
        assert_eq!(path_to_uri("relative.rs"), "file:///relative.rs");
    }
}
