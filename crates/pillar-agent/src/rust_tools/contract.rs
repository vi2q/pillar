//! Type/trait contracts through an optional semantic provider (design §6,
//! stage R2).
//!
//! `rs_contract` is the entry for "read the type contract you need to decide,
//! instead of the whole function". The semantic provider — rust-analyzer, in
//! the development host — is **optional**: when it is unavailable the service
//! falls back to the raw source line and says so (design §6: "原文の宣言は
//! 位置ref経由で取得し、`unavailable` / 原文位置へのfallbackを必ず持つ").
//!
//! The provider is a port, so the contract and its budget can be verified
//! deterministically with a fake provider and a seeded source host, and an
//! unavailable or partially-resolved result is never dressed up as a complete
//! one (design §6: "未解決・非対応・上限到達を表示する").
//!
//! The position carried by a reference is an LSP position plus the negotiated
//! [`PositionEncoding`], because that is what both the provider and the
//! fallback need; a raw UTF-8 byte offset would have to be converted first
//! (design §5.2).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::exp::store::Revision;

use super::error::RustToolError;
use super::host::SourceSnapshotPort;
use super::position::PositionEncoding;

/// A versioned source position. The revision is captured when the reference
/// is issued, so a contract read cannot silently describe a different document
/// (design §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePosition {
    pub path: String,
    pub revision: Revision,
    /// 0-based LSP line.
    pub line: u32,
    /// 0-based character in `encoding`.
    pub character: u32,
    pub encoding: PositionEncoding,
}

/// Whether the optional semantic provider is usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnalysisAvailability {
    Available,
    Unavailable { reason: String },
}

impl AnalysisAvailability {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// What the caller wants from the contract (design §6, example `include`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractRequest {
    pub position_ref: String,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub budget: ContractBudget,
}

/// Per-call contract budget. `output_tokens` is advisory (no tokenizer is
/// assumed); `max_nodes` is the hard structural cap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractBudget {
    #[serde(default)]
    pub max_nodes: Option<usize>,
    #[serde(default)]
    pub max_depth: Option<usize>,
    #[serde(default)]
    pub output_tokens: Option<usize>,
}

/// The query handed to a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticQuery {
    pub path: String,
    pub revision: Revision,
    pub line: u32,
    pub character: u32,
    pub encoding: PositionEncoding,
    pub include: Vec<String>,
    pub budget: ContractBudget,
}

/// Where a type came from: the declaration says it, or the analyzer inferred
/// it. The two must not be presented as the same (design §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Declared,
    Inferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractSpan {
    pub line_start: u64,
    pub line_end: u64,
    pub byte_start: u64,
    pub byte_end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default)]
    pub generics: Vec<String>,
    #[serde(default)]
    pub where_clauses: Vec<String>,
    pub provenance: Provenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<ContractSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImplCandidate {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whether this impl is the one a concrete call selected, as opposed to a
    /// search candidate (design §6).
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedItem {
    pub what: String,
    pub reason: String,
}

/// The provider's answer for one position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractSlice {
    pub path: String,
    pub revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Declaration>,
    #[serde(default)]
    pub types: Vec<TypeDefinition>,
    #[serde(default)]
    pub impls: Vec<ImplCandidate>,
    #[serde(default)]
    pub unresolved: Vec<UnresolvedItem>,
    /// The budget or the provider's own depth cap cut the result short.
    pub truncated: bool,
    /// Raw source text when the provider was unavailable (the fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_excerpt: Option<String>,
}

impl ContractSlice {
    /// An empty slice at a revision; providers fill in what they resolved.
    pub fn empty(path: &str, revision: Revision) -> Self {
        Self {
            path: path.to_string(),
            revision,
            configuration: None,
            declaration: None,
            types: Vec::new(),
            impls: Vec::new(),
            unresolved: Vec::new(),
            truncated: false,
            source_excerpt: None,
        }
    }

    /// The structural nodes the budget counts.
    pub fn node_count(&self) -> usize {
        usize::from(self.declaration.is_some()) + self.types.len() + self.impls.len()
    }
}

/// The response the tool returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractResponse {
    pub position_ref: String,
    pub availability: AnalysisAvailability,
    /// True when the slice came from source text, not semantic analysis.
    pub fallback_to_source: bool,
    pub slice: ContractSlice,
}

/// The optional semantic provider (design §6, "rust-analyzer接続").
///
/// A provider must not claim to resolve everything: it reports what it could
/// not resolve in [`ContractSlice::unresolved`].
#[async_trait]
pub trait SemanticProvider: Send + Sync {
    fn availability(&self) -> AnalysisAvailability;
    async fn contract(&self, query: SemanticQuery) -> Result<ContractSlice, RustToolError>;
}

/// Host policy for contract reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractLimits {
    pub max_nodes: usize,
    pub max_depth: usize,
    pub max_source_bytes: usize,
}

impl Default for ContractLimits {
    fn default() -> Self {
        Self {
            max_nodes: 24,
            max_depth: 3,
            max_source_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Default)]
struct PositionRegistry {
    by_id: HashMap<String, SourcePosition>,
    order: VecDeque<String>,
    counter: u64,
    capacity: usize,
}

/// Type/trait contracts, with the source fallback (design §6).
pub struct ContractService {
    provider: Arc<dyn SemanticProvider>,
    sources: Arc<dyn SourceSnapshotPort>,
    positions: Mutex<PositionRegistry>,
    limits: ContractLimits,
    next_generation: AtomicU64,
}

impl ContractService {
    pub fn new(
        provider: Arc<dyn SemanticProvider>,
        sources: Arc<dyn SourceSnapshotPort>,
        limits: ContractLimits,
    ) -> Self {
        Self {
            provider,
            sources,
            positions: Mutex::new(PositionRegistry {
                capacity: 256,
                ..PositionRegistry::default()
            }),
            limits,
            next_generation: AtomicU64::new(0),
        }
    }

    /// Issue an opaque, model-repeatable reference to a source position.
    pub fn issue_position(&self, position: SourcePosition) -> String {
        let mut registry = self.positions.lock().expect("positions lock");
        registry.counter += 1;
        let id = format!("src{}", registry.counter);
        while registry.by_id.len() >= registry.capacity {
            if let Some(evicted) = registry.order.pop_front() {
                registry.by_id.remove(&evicted);
            } else {
                break;
            }
        }
        registry.order.push_back(id.clone());
        registry.by_id.insert(id.clone(), position);
        id
    }

    /// Issue a reference whose document version is derived from the text read
    /// now. The generation is monotonic, so it is a usable LSP document
    /// version even when only a line was read (design §6: the document version
    /// travels with a semantic result).
    pub fn issue_position_for_text(
        &self,
        path: &str,
        line: u32,
        character: u32,
        encoding: PositionEncoding,
        text: &str,
    ) -> String {
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.issue_position(SourcePosition {
            path: path.to_string(),
            revision: Revision::new(generation, text.as_bytes()),
            line,
            character,
            encoding,
        })
    }

    pub fn availability(&self) -> AnalysisAvailability {
        self.provider.availability()
    }

    /// Read the contract for a position reference.
    pub async fn contract(
        &self,
        request: &ContractRequest,
    ) -> Result<ContractResponse, RustToolError> {
        let position = self
            .positions
            .lock()
            .expect("positions lock")
            .by_id
            .get(&request.position_ref)
            .cloned()
            .ok_or_else(|| {
                RustToolError::invalid_request(
                    "unknown position reference; read the source again to get a fresh one",
                )
            })?;

        let availability = self.provider.availability();
        if !availability.is_available() {
            return self.fallback(&request.position_ref, &position, availability);
        }

        let query = SemanticQuery {
            path: position.path.clone(),
            revision: position.revision,
            line: position.line,
            character: position.character,
            encoding: position.encoding,
            include: request.include.clone(),
            budget: request.budget.clone(),
        };
        let mut slice = self.provider.contract(query).await?;
        let max_nodes = request.budget.max_nodes.unwrap_or(self.limits.max_nodes);
        if slice.node_count() > max_nodes {
            slice.truncated = true;
            let mut kept = 0usize;
            let declaration = slice.declaration.take();
            if declaration.is_some() {
                kept += 1;
            }
            let mut types = Vec::new();
            for item in slice.types.drain(..) {
                if kept >= max_nodes {
                    break;
                }
                types.push(item);
                kept += 1;
            }
            let mut impls = Vec::new();
            for item in slice.impls.drain(..) {
                if kept >= max_nodes {
                    break;
                }
                impls.push(item);
                kept += 1;
            }
            slice.declaration = declaration;
            slice.types = types;
            slice.impls = impls;
            slice.unresolved.push(UnresolvedItem {
                what: "contract".to_string(),
                reason: format!("the node budget ({max_nodes}) cut the result short"),
            });
        }
        Ok(ContractResponse {
            position_ref: request.position_ref.clone(),
            availability,
            fallback_to_source: false,
            slice,
        })
    }

    fn fallback(
        &self,
        position_ref: &str,
        position: &SourcePosition,
        availability: AnalysisAvailability,
    ) -> Result<ContractResponse, RustToolError> {
        let reason = match &availability {
            AnalysisAvailability::Unavailable { reason } => reason.clone(),
            AnalysisAvailability::Available => unreachable!("fallback is only for unavailable"),
        };
        let mut slice = ContractSlice::empty(&position.path, position.revision);
        slice.configuration = None;
        slice.unresolved.push(UnresolvedItem {
            what: "semantic analysis".to_string(),
            reason,
        });
        // The raw source line, so the caller can still read the declaration
        // (design §6: fallback to source).
        let excerpt = self
            .sources
            .read_range(&position.path, position.line as u64 + 1, 1)
            .map(|slice| slice.text)
            .ok();
        slice.source_excerpt = excerpt.map(|text| {
            if text.len() > self.limits.max_source_bytes {
                text[..self.limits.max_source_bytes].to_string()
            } else {
                text
            }
        });
        Ok(ContractResponse {
            position_ref: position_ref.to_string(),
            availability,
            fallback_to_source: true,
            slice,
        })
    }
}

/// An in-memory provider for tests and for a host that has no analyzer: it is
/// permanently `Unavailable`, so the service exercises the fallback.
pub struct UnavailableProvider {
    pub reason: String,
}

impl Default for UnavailableProvider {
    fn default() -> Self {
        Self {
            reason: "no semantic provider is configured".to_string(),
        }
    }
}

#[async_trait]
impl SemanticProvider for UnavailableProvider {
    fn availability(&self) -> AnalysisAvailability {
        AnalysisAvailability::Unavailable {
            reason: self.reason.clone(),
        }
    }

    async fn contract(&self, query: SemanticQuery) -> Result<ContractSlice, RustToolError> {
        Err(RustToolError::analysis_unavailable(format!(
            "no provider for {}:{}",
            query.path, query.line
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_count_counts_declaration_types_and_impls() {
        let mut slice = ContractSlice::empty("f.rs", Revision::new(1, b""));
        assert_eq!(slice.node_count(), 0);
        slice.declaration = Some(Declaration {
            kind: "fn".to_string(),
            name: "f".to_string(),
            signature: None,
            generics: Vec::new(),
            where_clauses: Vec::new(),
            provenance: Provenance::Declared,
            span: None,
        });
        slice.types.push(TypeDefinition {
            name: "T".to_string(),
            path: None,
            definition: None,
        });
        assert_eq!(slice.node_count(), 2);
    }
}
