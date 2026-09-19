//! Cargo / rustc JSON diagnostics: classification, normalization, and the
//! "bundle" the model reads (design §5).
//!
//! Three properties the design insists on are encoded here:
//!
//! - A JSON line that is not a compiler message is not an error to discard:
//!   unknown reasons, added fields, non-JSON lines and malformed JSON are kept
//!   as [`UnknownMessage`] / [`UnstructuredLine`] / [`ParseError`], and the
//!   collection is marked partial (design §5.1).
//! - `build-finished.success` is a *build* phase outcome. It says nothing
//!   about tests, so [`BuildStatus`] and [`TestStatus`] are separate and a
//!   successful build with no test evidence leaves `test_status` `Unknown`
//!   (design §5.1). Stable libtest output is not assumed to share Cargo's JSON
//!   schema: the summary is parsed best-effort and [`TestEvidence::parsed`]
//!   says whether a trusted summary was actually seen.
//! - A span's source binding is computed against an authorized root, and a
//!   registry / sysroot / macro-virtual file is never an editable workspace
//!   target (design §3, §5.3). [rustc's byte span is 0-based and half-open;
//!   the line and column fields are *not* bytes](DiagnosticSpan).
//!
//! The module is pure: it reads lines and never spawns Cargo or touches the
//! filesystem. The run that produces the lines is the broker's job (design
//! §9).

use serde::{Deserialize, Serialize};

use super::digest64;

/// How much of the stream the collector kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CollectionState {
    /// Every line was classified within the limits.
    Complete,
    /// At least one line was dropped, truncated or could not be classified;
    /// the run is not a complete observation (design §3, §5.1).
    Partial { reason: String },
}

impl CollectionState {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// Compile phase outcome, distinct from the test phase (design §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    /// No `build-finished` was observed yet.
    Unknown,
    Succeeded,
    Failed,
}

impl BuildStatus {
    /// The wire form (matches the `serde` name).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Test phase outcome. `Unknown` is not success: a filter that selected no
/// tests, or a run whose summary was never parsed, stays unknown (design §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    /// No test phase was requested or observed.
    NotRun,
    Unknown,
    Passed,
    Failed,
}

impl TestStatus {
    /// The wire form (matches the `serde` name).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRun => "not_run",
            Self::Unknown => "unknown",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// rustc diagnostic severities, plus an unknown escape hatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Error,
    Warning,
    Note,
    Help,
    FailureNote,
    Ice,
    Bug,
    Unknown(String),
}

impl DiagnosticLevel {
    fn parse(level: &str) -> Self {
        match level {
            "error" => Self::Error,
            "warning" => Self::Warning,
            "note" => Self::Note,
            "help" => Self::Help,
            "failure-note" => Self::FailureNote,
            "error: internal compiler error" | "ice" => Self::Ice,
            "bug" => Self::Bug,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error | Self::Ice | Self::Bug)
    }

    /// The wire form (matches the `serde` name).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
            Self::Help => "help",
            Self::FailureNote => "failure_note",
            Self::Ice => "ice",
            Self::Bug => "bug",
            Self::Unknown(other) => other,
        }
    }
}

/// rustc suggestion applicability, with an unknown escape hatch so a new value
/// does not break parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Applicability {
    /// The compiler is confident; only these are candidates for application
    /// (design §8.2).
    MachineApplicable,
    MaybeIncorrect,
    HasPlaceholders,
    Unspecified,
    Unknown(String),
}

impl Applicability {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("MachineApplicable") => Self::MachineApplicable,
            Some("MaybeIncorrect") => Self::MaybeIncorrect,
            Some("HasPlaceholders") => Self::HasPlaceholders,
            Some("Unspecified") => Self::Unspecified,
            Some(other) => Self::Unknown(other.to_string()),
            None => Self::Unspecified,
        }
    }

    /// How weak the applicability is; higher is weaker.
    fn rank(&self) -> u8 {
        match self {
            Self::MachineApplicable => 0,
            Self::MaybeIncorrect => 1,
            Self::HasPlaceholders => 2,
            Self::Unspecified => 3,
            Self::Unknown(_) => 4,
        }
    }

    fn weakest(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    pub fn is_machine_applicable(&self) -> bool {
        matches!(self, Self::MachineApplicable)
    }
}

/// Where a diagnostic span points, relative to the authorized workspace
/// (design §3, §5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceBinding {
    /// A file under the authorized workspace root. The only editable category.
    Workspace { path: String },
    /// A registry or sysroot file: readable through a different capability,
    /// never an editing target.
    RegistryOrSysroot { path: String },
    /// A virtual file produced by macro expansion (`<foo macros>`).
    MacroVirtual { name: String },
    /// The span does not map to a known root; it must not be edited.
    Unmapped { file_name: String },
}

impl SourceBinding {
    /// Whether the design's initial edit conditions could apply.
    pub fn is_editable_candidate(&self) -> bool {
        matches!(self, Self::Workspace { .. })
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Workspace { path } | Self::RegistryOrSysroot { path } => Some(path),
            Self::MacroVirtual { .. } | Self::Unmapped { .. } => None,
        }
    }
}

/// The roots a span is classified against. A missing workspace root means no
/// span can be bound to the workspace, so nothing is editable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcePolicy {
    pub workspace_root: Option<String>,
    pub registry_roots: Vec<String>,
    pub sysroot_roots: Vec<String>,
}

impl SourcePolicy {
    pub fn new(workspace_root: impl Into<String>) -> Self {
        Self {
            workspace_root: Some(workspace_root.into()),
            ..Self::default()
        }
    }

    pub fn bind(&self, file_name: &str) -> SourceBinding {
        let trimmed = file_name.trim();
        if trimmed.starts_with('<') && trimmed.ends_with('>') {
            return SourceBinding::MacroVirtual {
                name: trimmed.to_string(),
            };
        }
        if let Some(root) = &self.workspace_root
            && super::metadata::path_is_under(trimmed, root)
        {
            return SourceBinding::Workspace {
                path: super::metadata::normalize_path(trimmed),
            };
        }
        for root in self.registry_roots.iter().chain(self.sysroot_roots.iter()) {
            if super::metadata::path_is_under(trimmed, root) {
                return SourceBinding::RegistryOrSysroot {
                    path: super::metadata::normalize_path(trimmed),
                };
            }
        }
        SourceBinding::Unmapped {
            file_name: trimmed.to_string(),
        }
    }
}

/// One line of source context rustc prints under a span.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpanLine {
    pub text: String,
    pub highlight_start: u64,
    pub highlight_end: u64,
}

/// A macro call site, kept so a reported span is not mistaken for the
/// expansion's own file (design §5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MacroExpansion {
    pub macro_decl_name: String,
    pub span: Option<Box<DiagnosticSpan>>,
    pub def_site_span: Option<Box<DiagnosticSpan>>,
}

/// One rustc span.
///
/// `byte_start` / `byte_end` are the 0-based, half-open byte range
/// (design §5.2). `line_*` / `column_*` are rustc's 1-based display
/// coordinates and are **not** byte offsets; never pass them to an edit as if
/// they were. LSP position encoding conversion belongs to the semantic
/// adapter (R2), not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticSpan {
    pub file_name: String,
    pub binding: SourceBinding,
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub column_start: u64,
    pub column_end: u64,
    pub is_primary: bool,
    pub label: Option<String>,
    pub suggested_replacement: Option<String>,
    pub applicability: Applicability,
    pub expansion: Option<MacroExpansion>,
    pub text: Vec<SpanLine>,
}

impl DiagnosticSpan {
    /// The rustc byte range, 0-based and half-open.
    pub fn byte_range(&self) -> (u64, u64) {
        (self.byte_start, self.byte_end)
    }

    /// A zero-length span is an insertion point; the initial apply contract
    /// rejects it (design §8.2).
    pub fn is_zero_length(&self) -> bool {
        self.byte_start == self.byte_end
    }
}

/// One replacement inside a suggestion group, carrying the exact byte range
/// and the source binding at extraction time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuggestionReplacement {
    pub file_name: String,
    pub binding: SourceBinding,
    pub byte_start: u64,
    pub byte_end: u64,
    pub replacement: String,
    pub applicability: Applicability,
    pub zero_length: bool,
}

/// The replacements rustc offered for one diagnostic, grouped as one proposal
/// (design §8.1). `applicability` is the weakest of its replacements, so a
/// group is only machine-applicable when every part is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuggestionGroup {
    pub diagnostic_id: String,
    pub applicability: Applicability,
    pub replacements: Vec<SuggestionReplacement>,
}

impl SuggestionGroup {
    pub fn is_machine_applicable(&self) -> bool {
        self.applicability.is_machine_applicable()
    }

    pub fn has_insertions(&self) -> bool {
        self.replacements.iter().any(|item| item.zero_length)
    }
}

/// One normalized diagnostic, with its children and suggestions intact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub id: String,
    pub run_id: String,
    pub package_id: Option<String>,
    pub target_name: Option<String>,
    pub level: DiagnosticLevel,
    pub code: Option<String>,
    pub message: String,
    pub rendered: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    pub children: Vec<Diagnostic>,
    pub suggestions: Vec<SuggestionGroup>,
    pub primary_binding: Option<SourceBinding>,
}

impl Diagnostic {
    /// A stable-ish identity for diffing runs. It is deliberately not the
    /// rendered text: line moves and message edits change it, which is why the
    /// design calls an unmatched diagnostic `unmatched` rather than hiding it
    /// (design §5.3).
    pub fn identity_key(&self) -> String {
        let primary = self
            .spans
            .iter()
            .find(|span| span.is_primary)
            .or_else(|| self.spans.first());
        let location = primary
            .map(|span| format!("{}:{}", span.file_name, span.byte_start))
            .unwrap_or_default();
        format!(
            "{}|{}|{}|{}",
            self.code.as_deref().unwrap_or(""),
            self.level_name(),
            self.message,
            location
        )
    }

    fn level_name(&self) -> &str {
        match &self.level {
            DiagnosticLevel::Error => "error",
            DiagnosticLevel::Warning => "warning",
            DiagnosticLevel::Note => "note",
            DiagnosticLevel::Help => "help",
            DiagnosticLevel::FailureNote => "failure-note",
            DiagnosticLevel::Ice => "ice",
            DiagnosticLevel::Bug => "bug",
            DiagnosticLevel::Unknown(other) => other,
        }
    }

    pub fn is_error(&self) -> bool {
        self.level.is_error()
    }
}

/// An unknown `reason` value, or a JSON object without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnknownMessage {
    pub line_number: usize,
    pub reason: String,
    pub preview: String,
}

/// A line that could not be classified as JSON at all (proc-macro or build
/// script chatter, a torn line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnstructuredLine {
    pub line_number: usize,
    pub preview: String,
}

/// A JSON line that failed to deserialize into the expected shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseError {
    pub line_number: usize,
    pub message: String,
    pub preview: String,
}

/// A parsed `compiler-artifact` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactRecord {
    pub package_id: Option<String>,
    pub target_name: Option<String>,
    pub features: Vec<String>,
    pub filenames: Vec<String>,
    pub executable: Option<String>,
    pub fresh: bool,
}

/// A `build-script-executed` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildScriptRecord {
    pub package_id: Option<String>,
    pub out_dir: Option<String>,
    pub cfgs: Vec<String>,
    pub linked_paths: Vec<String>,
}

/// `build-finished`, the build phase's terminal notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BuildFinished {
    pub success: bool,
}

/// Best-effort libtest summary (design §5.1: not a stable Cargo JSON schema).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestSummary {
    pub result_ok: bool,
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    pub measured: u64,
    pub filtered_out: u64,
}

/// What the raw output revealed about tests. `parsed == false` means the
/// process may have exited 0 without a trustworthy summary (design §5.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TestEvidence {
    pub summary: Option<TestSummary>,
    pub parsed: bool,
    /// A `running 0 tests` line was seen: a filter may have selected nothing.
    pub zero_selected: bool,
}

impl TestEvidence {
    fn observe(&mut self, line: &str) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("running ") {
            if let Some(count) = rest
                .split_whitespace()
                .next()
                .and_then(|token| token.parse::<u64>().ok())
                && count == 0
            {
                self.zero_selected = true;
            }
        }
        if let Some(summary) = parse_libtest_summary(trimmed) {
            self.summary = Some(summary);
            self.parsed = true;
        }
    }
}

/// Parse `test result: ok. 3 passed; 0 failed; …` best-effort.
fn parse_libtest_summary(line: &str) -> Option<TestSummary> {
    let rest = line.strip_prefix("test result:")?.trim_start();
    let result_ok = rest.starts_with("ok");
    let mut summary = TestSummary {
        result_ok,
        passed: 0,
        failed: 0,
        ignored: 0,
        measured: 0,
        filtered_out: 0,
    };
    // Pair every number with the label that follows it. Splitting on the
    // result word (`ok.` / `FAILED.`) as well keeps the first `N passed` from
    // being skipped, which happens when the whole head is one `;` segment.
    let tokens: Vec<&str> = rest
        .split(|character: char| character == ';' || character == '.' || character.is_whitespace())
        .filter(|token| !token.is_empty())
        .collect();
    let mut index = 0usize;
    while index + 1 < tokens.len() {
        let Some(count) = tokens[index].parse::<u64>().ok() else {
            index += 1;
            continue;
        };
        match tokens[index + 1] {
            "passed" => summary.passed = count,
            "failed" => summary.failed = count,
            "ignored" => summary.ignored = count,
            "measured" => summary.measured = count,
            "filtered" => summary.filtered_out = count,
            _ => {
                index += 1;
                continue;
            }
        }
        index += 2;
    }
    Some(summary)
}

/// Whether a non-JSON line is the libtest harness's own output rather than
/// foreign chatter (design §5.1: test output is a separate, best-effort
/// channel).
fn is_test_output(line: &str) -> bool {
    let line = line.trim_start();
    [
        "running ",
        "test result:",
        "test ",
        "failures:",
        "failures ",
        "successes:",
        "---- ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

/// Bounds on what one collector retains (design §5.1, §8.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionLimits {
    /// A single line longer than this is not parsed and marks the collection
    /// partial; the host still drains the pipe.
    pub max_line_bytes: usize,
    /// Total JSON lines retained.
    pub max_lines: usize,
    /// Total bytes retained across all classified lines.
    pub max_total_bytes: usize,
    /// Diagnostics retained; beyond this the run is partial.
    pub max_diagnostics: usize,
    /// Unstructured lines described (with a bounded preview).
    pub max_unstructured: usize,
    /// Parse errors described.
    pub max_parse_errors: usize,
    /// Bytes kept of any one evidence preview.
    pub max_preview_bytes: usize,
}

impl Default for CollectionLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: 1024 * 1024,
            max_lines: 100_000,
            max_total_bytes: 64 * 1024 * 1024,
            max_diagnostics: 2_000,
            max_unstructured: 200,
            max_parse_errors: 200,
            max_preview_bytes: 240,
        }
    }
}

/// Counters for the collected run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CollectionStats {
    pub lines_seen: usize,
    pub lines_kept: usize,
    pub bytes_kept: usize,
    pub diagnostics_dropped: usize,
}

/// The normalized result of one Cargo run's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CollectedRun {
    pub run_id: String,
    pub diagnostics: Vec<Diagnostic>,
    pub artifacts: Vec<ArtifactRecord>,
    pub build_scripts: Vec<BuildScriptRecord>,
    pub build_finished: Option<BuildFinished>,
    pub build_status: BuildStatus,
    pub test_status: TestStatus,
    pub test_evidence: TestEvidence,
    pub other_messages: Vec<UnknownMessage>,
    pub unstructured: Vec<UnstructuredLine>,
    pub parse_errors: Vec<ParseError>,
    pub collection: CollectionState,
    pub stats: CollectionStats,
}

impl CollectedRun {
    /// The first primary span bound to the workspace, if any.
    pub fn editable_spans(&self) -> impl Iterator<Item = &DiagnosticSpan> {
        self.diagnostics.iter().flat_map(|diagnostic| {
            diagnostic
                .spans
                .iter()
                .filter(|span| span.binding.is_editable_candidate())
        })
    }
}

/// A bounded collector for one run's line stream.
///
/// It never stops the caller from draining a pipe: an over-budget line is
/// dropped from storage and recorded, not propagated as an error (design
/// §5.1, §8.2 "保存打切りと欠落を表示").
#[derive(Debug)]
pub struct DiagnosticCollector {
    run_id: String,
    limits: CollectionLimits,
    diagnostics: Vec<Diagnostic>,
    artifacts: Vec<ArtifactRecord>,
    build_scripts: Vec<BuildScriptRecord>,
    build_finished: Option<BuildFinished>,
    other_messages: Vec<UnknownMessage>,
    unstructured: Vec<UnstructuredLine>,
    parse_errors: Vec<ParseError>,
    test_evidence: TestEvidence,
    stats: CollectionStats,
    partial_reason: Option<String>,
}

impl DiagnosticCollector {
    pub fn new(run_id: impl Into<String>, limits: CollectionLimits) -> Self {
        Self {
            run_id: run_id.into(),
            limits,
            diagnostics: Vec::new(),
            artifacts: Vec::new(),
            build_scripts: Vec::new(),
            build_finished: None,
            other_messages: Vec::new(),
            unstructured: Vec::new(),
            parse_errors: Vec::new(),
            test_evidence: TestEvidence::default(),
            stats: CollectionStats::default(),
            partial_reason: None,
        }
    }

    fn mark_partial(&mut self, reason: impl Into<String>) {
        if self.partial_reason.is_none() {
            self.partial_reason = Some(reason.into());
        }
    }

    fn preview(&self, line: &str) -> String {
        let limit = self.limits.max_preview_bytes;
        if line.len() <= limit {
            return line.to_string();
        }
        let mut end = limit;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &line[..end])
    }

    /// Ingest one raw line from stdout or stderr.
    ///
    /// Returns whether the line was retained. A `false` return does not mean
    /// the caller should stop reading; it means storage is full (design §5.1).
    pub fn push_line(&mut self, line: &str) -> bool {
        self.stats.lines_seen += 1;
        if line.len() > self.limits.max_line_bytes {
            self.mark_partial("a line exceeded the per-line byte cap");
            self.record_parse_error(
                self.stats.lines_seen,
                "line exceeded the per-line byte cap".to_string(),
                line,
            );
            return false;
        }
        if self.stats.lines_seen > self.limits.max_lines
            || self.stats.bytes_kept + line.len() > self.limits.max_total_bytes
        {
            self.mark_partial("the run exceeded its line or byte budget");
            return false;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            self.stats.lines_kept += 1;
            self.stats.bytes_kept += line.len();
            return true;
        }

        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                // Not JSON: the test harness's own output is expected in a
                // test run, so it is observed but is not "foreign" output.
                // Anything else is retained and marks the run partial.
                self.test_evidence.observe(trimmed);
                if !is_test_output(trimmed) {
                    self.record_unstructured(self.stats.lines_seen, line);
                }
                self.stats.lines_kept += 1;
                self.stats.bytes_kept += line.len();
                return true;
            }
        };

        let Some(reason) = value.get("reason").and_then(|reason| reason.as_str()) else {
            self.record_unknown(self.stats.lines_seen, "<none>".to_string(), line);
            self.stats.lines_kept += 1;
            self.stats.bytes_kept += line.len();
            return true;
        };

        let line_number = self.stats.lines_seen;
        match reason {
            "compiler-message" => match serde_json::from_value::<RawCompilerMessage>(value) {
                Ok(raw) => {
                    if self.diagnostics.len() >= self.limits.max_diagnostics {
                        self.stats.diagnostics_dropped += 1;
                        self.mark_partial("diagnostics exceeded the retention cap");
                        return false;
                    }
                    let diagnostic = normalize(
                        &self.run_id,
                        self.diagnostics.len(),
                        raw,
                        &SourcePolicy::default(),
                    );
                    self.diagnostics.push(diagnostic);
                }
                Err(error) => {
                    self.record_parse_error(line_number, error.to_string(), line);
                }
            },
            "compiler-artifact" => {
                if let Ok(raw) = serde_json::from_value::<RawArtifact>(value) {
                    self.artifacts.push(ArtifactRecord {
                        package_id: raw.package_id,
                        target_name: raw.target.map(|target| target.name),
                        features: raw.features,
                        filenames: raw.filenames,
                        executable: raw.executable,
                        fresh: raw.fresh,
                    });
                } else {
                    self.record_parse_error(line_number, "compiler-artifact".to_string(), line);
                }
            }
            "build-script-executed" => {
                if let Ok(raw) = serde_json::from_value::<RawBuildScript>(value) {
                    self.build_scripts.push(BuildScriptRecord {
                        package_id: raw.package_id,
                        out_dir: raw.out_dir,
                        cfgs: raw.cfgs,
                        linked_paths: raw.linked_paths,
                    });
                } else {
                    self.record_parse_error(line_number, "build-script-executed".to_string(), line);
                }
            }
            "build-finished" => {
                if let Ok(raw) = serde_json::from_value::<RawBuildFinished>(value) {
                    self.build_finished = Some(BuildFinished {
                        success: raw.success,
                    });
                } else {
                    self.record_parse_error(line_number, "build-finished".to_string(), line);
                }
            }
            other => {
                self.record_unknown(line_number, other.to_string(), line);
            }
        }

        self.stats.lines_kept += 1;
        self.stats.bytes_kept += line.len();
        true
    }

    fn record_unstructured(&mut self, line_number: usize, line: &str) {
        let preview = self.preview(line);
        self.mark_partial("the stream contained non-JSON output");
        if self.unstructured.len() < self.limits.max_unstructured {
            self.unstructured.push(UnstructuredLine {
                line_number,
                preview,
            });
        }
    }

    fn record_unknown(&mut self, line_number: usize, reason: String, line: &str) {
        let preview = self.preview(line);
        self.mark_partial("the stream contained an unknown reason");
        if self.other_messages.len() < self.limits.max_parse_errors {
            self.other_messages.push(UnknownMessage {
                line_number,
                reason,
                preview,
            });
        }
    }

    fn record_parse_error(&mut self, line_number: usize, message: String, line: &str) {
        let preview = self.preview(line);
        self.mark_partial("a line could not be parsed");
        if self.parse_errors.len() < self.limits.max_parse_errors {
            self.parse_errors.push(ParseError {
                line_number,
                message,
                preview,
            });
        }
    }

    /// Finish the run, binding spans against `policy`.
    ///
    /// Binding happens at finish so a host can pass the authorized root once,
    /// after collection, without the collector retaining policy per line.
    pub fn finish(mut self, policy: &SourcePolicy) -> CollectedRun {
        // Re-bind every span now that the policy is known.
        for diagnostic in &mut self.diagnostics {
            rebind(diagnostic, policy);
            rebind_children(diagnostic, policy);
        }

        let build_status = match self.build_finished {
            Some(finished) if finished.success => BuildStatus::Succeeded,
            Some(_) => BuildStatus::Failed,
            None => BuildStatus::Unknown,
        };
        let test_status = match &self.test_evidence.summary {
            Some(summary) if summary.result_ok && summary.failed == 0 => TestStatus::Passed,
            Some(_) => TestStatus::Failed,
            None => {
                if self.test_evidence.parsed || self.test_evidence.zero_selected {
                    TestStatus::Unknown
                } else {
                    TestStatus::NotRun
                }
            }
        };
        let collection = match self.partial_reason {
            Some(reason) => CollectionState::Partial { reason },
            None => CollectionState::Complete,
        };

        CollectedRun {
            run_id: self.run_id,
            diagnostics: self.diagnostics,
            artifacts: self.artifacts,
            build_scripts: self.build_scripts,
            build_finished: self.build_finished,
            build_status,
            test_status,
            test_evidence: self.test_evidence,
            other_messages: self.other_messages,
            unstructured: self.unstructured,
            parse_errors: self.parse_errors,
            collection,
            stats: self.stats,
        }
    }
}

fn rebind(diagnostic: &mut Diagnostic, policy: &SourcePolicy) {
    for span in &mut diagnostic.spans {
        rebind_span(span, policy);
    }
    diagnostic.primary_binding = diagnostic
        .spans
        .iter()
        .find(|span| span.is_primary)
        .or_else(|| diagnostic.spans.first())
        .map(|span| span.binding.clone());
    for suggestion in &mut diagnostic.suggestions {
        for replacement in &mut suggestion.replacements {
            replacement.binding = policy.bind(&replacement.file_name);
        }
    }
}

fn rebind_span(span: &mut DiagnosticSpan, policy: &SourcePolicy) {
    span.binding = policy.bind(&span.file_name);
    if let Some(expansion) = &mut span.expansion {
        if let Some(inner) = &mut expansion.span {
            rebind_span(inner, policy);
        }
        if let Some(inner) = &mut expansion.def_site_span {
            rebind_span(inner, policy);
        }
    }
}

fn rebind_children(diagnostic: &mut Diagnostic, policy: &SourcePolicy) {
    for child in &mut diagnostic.children {
        rebind(child, policy);
        rebind_children(child, policy);
    }
}

/// Build a suggestion group from the replacements on one diagnostic's spans.
fn suggestions_for(id: &str, spans: &[DiagnosticSpan]) -> Vec<SuggestionGroup> {
    let replacements: Vec<SuggestionReplacement> = spans
        .iter()
        .filter_map(|span| {
            let replacement = span.suggested_replacement.as_ref()?;
            Some(SuggestionReplacement {
                file_name: span.file_name.clone(),
                binding: span.binding.clone(),
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                replacement: replacement.clone(),
                applicability: span.applicability.clone(),
                zero_length: span.is_zero_length(),
            })
        })
        .collect();
    if replacements.is_empty() {
        return Vec::new();
    }
    let weakest = replacements
        .iter()
        .fold(Applicability::MachineApplicable, |current, item| {
            current.weakest(item.applicability.clone())
        });
    vec![SuggestionGroup {
        diagnostic_id: id.to_string(),
        applicability: weakest,
        replacements,
    }]
}

fn normalize(
    run_id: &str,
    index: usize,
    raw: RawCompilerMessage,
    policy: &SourcePolicy,
) -> Diagnostic {
    let id = format!("d{index}");
    normalize_message(
        run_id,
        &id,
        &raw.package_id,
        raw.target.as_ref(),
        raw.message,
        policy,
    )
}

fn normalize_message(
    run_id: &str,
    id: &str,
    package_id: &Option<String>,
    target: Option<&RawTarget>,
    message: RawMessage,
    policy: &SourcePolicy,
) -> Diagnostic {
    let level = DiagnosticLevel::parse(&message.level);
    let spans = message
        .spans
        .into_iter()
        .map(|span| span.into_span(policy))
        .collect::<Vec<_>>();
    let suggestions = suggestions_for(id, &spans);
    let children = message
        .children
        .into_iter()
        .enumerate()
        .map(|(child_index, child)| {
            normalize_message(
                run_id,
                &format!("{id}.c{child_index}"),
                package_id,
                target,
                child,
                policy,
            )
        })
        .collect::<Vec<_>>();
    let primary_binding = spans
        .iter()
        .find(|span| span.is_primary)
        .or_else(|| spans.first())
        .map(|span| span.binding.clone());
    Diagnostic {
        id: id.to_string(),
        run_id: run_id.to_string(),
        package_id: package_id.clone(),
        target_name: target.map(|target| target.name.clone()),
        level,
        code: message.code.map(|code| code.code),
        message: message.message,
        rendered: message.rendered,
        spans,
        children,
        suggestions,
        primary_binding,
    }
}

// --- raw JSON shapes -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawCompilerMessage {
    #[serde(default)]
    package_id: Option<String>,
    #[serde(default)]
    target: Option<RawTarget>,
    message: RawMessage,
}

#[derive(Debug, Deserialize)]
struct RawTarget {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    #[serde(default)]
    rendered: Option<String>,
    #[serde(default)]
    children: Vec<RawMessage>,
    #[serde(default)]
    code: Option<RawCode>,
    level: String,
    message: String,
    #[serde(default)]
    spans: Vec<RawSpan>,
}

#[derive(Debug, Deserialize)]
struct RawCode {
    code: String,
}

#[derive(Debug, Deserialize)]
struct RawSpan {
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    byte_start: u64,
    #[serde(default)]
    byte_end: u64,
    #[serde(default)]
    line_start: u64,
    #[serde(default)]
    line_end: u64,
    #[serde(default)]
    column_start: u64,
    #[serde(default)]
    column_end: u64,
    #[serde(default)]
    is_primary: bool,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    suggested_replacement: Option<String>,
    #[serde(default)]
    suggestion_applicability: Option<String>,
    #[serde(default)]
    expansion: Option<Box<RawExpansion>>,
    #[serde(default)]
    text: Vec<RawSpanLine>,
}

impl RawSpan {
    fn into_span(self, policy: &SourcePolicy) -> DiagnosticSpan {
        DiagnosticSpan {
            binding: policy.bind(&self.file_name),
            file_name: self.file_name,
            byte_start: self.byte_start,
            byte_end: self.byte_end,
            line_start: self.line_start,
            line_end: self.line_end,
            column_start: self.column_start,
            column_end: self.column_end,
            is_primary: self.is_primary,
            label: self.label,
            applicability: Applicability::parse(self.suggestion_applicability.as_deref()),
            suggested_replacement: self.suggested_replacement,
            expansion: self.expansion.map(|expansion| MacroExpansion {
                macro_decl_name: expansion.macro_decl_name,
                span: expansion.span.map(|span| Box::new(span.into_span(policy))),
                def_site_span: expansion
                    .def_site_span
                    .map(|span| Box::new(span.into_span(policy))),
            }),
            text: self
                .text
                .into_iter()
                .map(|line| SpanLine {
                    text: line.text,
                    highlight_start: line.highlight_start,
                    highlight_end: line.highlight_end,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawExpansion {
    #[serde(default)]
    macro_decl_name: String,
    #[serde(default)]
    span: Option<RawSpan>,
    #[serde(default)]
    def_site_span: Option<RawSpan>,
}

#[derive(Debug, Deserialize)]
struct RawSpanLine {
    #[serde(default)]
    text: String,
    #[serde(default)]
    highlight_start: u64,
    #[serde(default)]
    highlight_end: u64,
}

#[derive(Debug, Deserialize)]
struct RawArtifact {
    #[serde(default)]
    package_id: Option<String>,
    #[serde(default)]
    target: Option<RawTarget>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    filenames: Vec<String>,
    #[serde(default)]
    executable: Option<String>,
    #[serde(default)]
    fresh: bool,
}

#[derive(Debug, Deserialize)]
struct RawBuildScript {
    #[serde(default)]
    package_id: Option<String>,
    #[serde(default)]
    out_dir: Option<String>,
    #[serde(default)]
    cfgs: Vec<String>,
    #[serde(default)]
    linked_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawBuildFinished {
    #[serde(default)]
    success: bool,
}

/// A digest of the normalized diagnostics, for comparing two runs of the same
/// configuration and scope (design §5.3). Only a complete run is comparable;
/// the caller checks [`CollectionState`] first.
pub fn diagnostics_digest(run: &CollectedRun) -> u64 {
    let mut canonical = String::new();
    for diagnostic in &run.diagnostics {
        canonical.push_str(&diagnostic.identity_key());
        canonical.push('\n');
        for suggestion in &diagnostic.suggestions {
            canonical.push_str(&format!("{:?}\n", suggestion.applicability));
        }
    }
    digest64(canonical.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_libtest_summary_is_parsed_but_flagged_best_effort() {
        let summary = parse_libtest_summary(
            "test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out",
        )
        .expect("summary");
        assert!(!summary.result_ok);
        // The count is still read when the result is FAILED.
        assert_eq!(summary.passed, 3);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.filtered_out, 2);

        let ok = parse_libtest_summary(
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
        )
        .expect("summary");
        assert!(ok.result_ok);
        assert_eq!(ok.passed, 0);
    }

    #[test]
    fn the_identity_key_uses_code_level_message_and_byte_location() {
        let diagnostic = Diagnostic {
            id: "d0".to_string(),
            run_id: "r1".to_string(),
            package_id: None,
            target_name: None,
            level: DiagnosticLevel::Error,
            code: Some("E0277".to_string()),
            message: "trait bound not satisfied".to_string(),
            rendered: None,
            spans: vec![DiagnosticSpan {
                file_name: "src/lib.rs".to_string(),
                binding: SourceBinding::Workspace {
                    path: "src/lib.rs".to_string(),
                },
                byte_start: 10,
                byte_end: 12,
                line_start: 1,
                line_end: 1,
                column_start: 1,
                column_end: 3,
                is_primary: true,
                label: None,
                suggested_replacement: None,
                applicability: Applicability::Unspecified,
                expansion: None,
                text: Vec::new(),
            }],
            children: Vec::new(),
            suggestions: Vec::new(),
            primary_binding: None,
        };
        assert_eq!(
            diagnostic.identity_key(),
            "E0277|error|trait bound not satisfied|src/lib.rs:10"
        );
    }
}
