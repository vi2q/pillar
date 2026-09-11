//! High-performance file finding and content search for the coding agent,
//! modeled on the FFF (dmtrKovalenko/fff) design: a parallel, gitignore-
//! aware directory walk with a glob matcher, run once per invocation but
//! structured so the traversal stage is reusable and results stream in
//! bounded memory.
//!
//! Replaces the upstream find/grep tools' external-process strategy
//! (`fd`/`ripgrep` downloads and spawns) with native traversal:
//! - `find_files`: glob matching over a parallel walk (ignore crate's
//!   `WalkParallel`, the same engine ripgrep/fff use), honoring
//!   `.gitignore` inside git repos.
//! - `grep_files`: content search over the same walk with the `regex`
//!   engine, match limits, context blocks, and per-line truncation.
//!
//! divergence from upstream: no external binary download/spawn (fd/rg);
//! glob matching uses globset and content matching uses the regex crate,
//! both running in-process. Output shapes (relativized paths, notices,
//! details) match the upstream tools so callers see identical results.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState};

use pillar_agent::types::{AgentTool, AgentToolResult, ToolExecuteError};
use pillar_ai::types::Content;

use crate::core::tools::path_utils::resolve_to_cwd;
use crate::core::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, truncate_head, truncate_line,
};

/// Default result/match limit (upstream `DEFAULT_LIMIT` for find).
pub const FIND_DEFAULT_LIMIT: usize = 1000;
/// Default match limit (upstream `DEFAULT_LIMIT` for grep).
pub const GREP_DEFAULT_LIMIT: usize = 100;

/// Directories never searched (upstream's hard ignore list for custom glob
/// backends).
pub const HARD_IGNORE: [&str; 2] = ["**/node_modules/**", "**/.git/**"];

// ============================================================================
// find
// ============================================================================

/// Find execution result (upstream `{ content, details }` shape).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FindResult {
    pub text: String,
    /// Result limit hit, with the effective limit.
    pub result_limit_reached: Option<usize>,
    /// Set when byte truncation occurred.
    pub truncation_max_bytes: Option<usize>,
    /// Full byte-truncation details, present when truncation occurred
    /// (upstream `details.truncation`).
    pub truncation: Option<crate::core::truncate::TruncationResult>,
}

/// Options for `find_files`.
#[derive(Debug, Clone)]
pub struct FindOptions {
    /// Effective limit (upstream `limit ?? 1000`).
    pub limit: usize,
    /// Include hidden files (upstream fd `--hidden`).
    pub hidden: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            limit: FIND_DEFAULT_LIMIT,
            hidden: true,
        }
    }
}

/// Relativize a find result against the search root and normalize to posix
/// separators (upstream `relativizeFindResultPath`).
pub fn relativize_find_result_path(result_path: &Path, search_path: &Path) -> String {
    let had_trailing_separator = result_path.to_string_lossy().ends_with('/');
    let relative = result_path.strip_prefix(search_path).unwrap_or(result_path);
    let posix = relative.to_string_lossy().replace('\\', "/");
    if had_trailing_separator && !posix.ends_with('/') {
        format!("{posix}/")
    } else {
        posix
    }
}

/// Build a case-insensitive GlobSet from a glob pattern. Patterns with a
/// path separator match the full relative path; bare patterns match the
/// basename (mirroring fd's `--glob` vs `--full-path` behavior).
pub fn build_globset(pattern: &str) -> Result<GlobSet, String> {
    let full_path_mode = pattern.contains('/');
    let mut builder = GlobSetBuilder::new();
    let candidate = if full_path_mode && !pattern.starts_with("**/") && pattern != "**" {
        format!("**/{pattern}")
    } else {
        pattern.to_string()
    };
    let mut glob = GlobBuilder::new(&candidate);
    glob.case_insensitive(true);
    glob.literal_separator(full_path_mode);
    let built = glob
        .build()
        .map_err(|e| format!("Invalid glob pattern \"{pattern}\": {e}"))?;
    builder.add(built);
    builder
        .build()
        .map_err(|e| format!("Invalid glob pattern \"{pattern}\": {e}"))
}

/// Check whether any `.git` entry exists in `start` or its ancestors
/// (upstream's insideGitRepo loop).
pub fn inside_git_repo(start: &Path) -> bool {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return true;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return false,
        }
    }
}

/// Find files matching a glob pattern under `search_dir` (upstream find
/// tool execute). Parallel walk with gitignore support; stops collecting at
/// the limit.
pub fn find_files(
    pattern: &str,
    search_dir: &str,
    cwd: &str,
    options: FindOptions,
) -> Result<FindResult, String> {
    let search_path = resolve_to_cwd(search_dir, cwd);
    if !search_path.exists() {
        return Err(format!("Path not found: {}", search_path.display()));
    }

    let globset = build_globset(pattern)?;
    let limit = options.limit;
    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let reached_limit = Arc::new(AtomicUsize::new(0));

    let mut builder = WalkBuilder::new(&search_path);
    builder.hidden(!options.hidden);
    // fd semantics: outside a git repo .gitignore rules need no-git
    // requirement; inside a repo git-aware behavior stops at nested repo
    // boundaries (upstream issue #5960).
    builder.require_git(inside_git_repo(&search_path));
    builder.threads(
        std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(16),
    );

    let walker = FindWalker {
        globset,
        collected: collected.clone(),
        limit,
        reached_limit: reached_limit.clone(),
        search_path: search_path.clone(),
    };
    builder
        .build_parallel()
        .visit(&mut FindVisitorBuilder { walker });

    let mut results = Arc::try_unwrap(collected)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| arc.lock().unwrap().clone());
    results.sort();
    finish_find_output(results, reached_limit.load(Ordering::SeqCst) == 1, limit)
}

fn finish_find_output(
    results: Vec<String>,
    result_limit_reached: bool,
    effective_limit: usize,
) -> Result<FindResult, String> {
    if results.is_empty() {
        return Ok(FindResult {
            text: "No files found matching pattern".to_string(),
            ..Default::default()
        });
    }
    let raw_output = results.join("\n");
    let truncation = truncate_head(
        &raw_output,
        TruncationOptions {
            max_lines: Some(usize::MAX),
            max_bytes: None,
        },
    );
    let mut output = truncation.content.clone();
    let mut details = FindResult::default();
    let mut notices: Vec<String> = Vec::new();
    if result_limit_reached {
        notices.push(format!(
            "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
            effective_limit * 2
        ));
        details.result_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!(
            "{} limit reached",
            crate::core::truncate::format_size(DEFAULT_MAX_BYTES)
        ));
        details.truncation_max_bytes = Some(DEFAULT_MAX_BYTES);
        details.truncation = Some(truncation.clone());
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    details.text = output;
    Ok(details)
}

// ============================================================================
// grep
// ============================================================================

/// Grep execution result (upstream `{ content, details }` shape).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GrepResult {
    pub text: String,
    /// Match limit hit, with the effective limit.
    pub match_limit_reached: Option<usize>,
    /// Set when byte truncation occurred.
    pub truncation_max_bytes: Option<usize>,
    /// Set when long lines were truncated.
    pub lines_truncated: bool,
    /// Full byte-truncation details, present when truncation occurred
    /// (upstream `details.truncation`).
    pub truncation: Option<crate::core::truncate::TruncationResult>,
}

/// Options for `grep_files` (upstream grepSchema fields).
#[derive(Debug, Clone)]
pub struct GrepOptions {
    /// Optional glob filter for files, e.g. `*.ts`.
    pub glob: Option<String>,
    /// Case-insensitive search.
    pub ignore_case: bool,
    /// Treat the pattern as a literal string instead of a regex.
    pub literal: bool,
    /// Lines of context before and after each match.
    pub context: usize,
    /// Match limit (minimum 1).
    pub limit: usize,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            glob: None,
            ignore_case: false,
            literal: false,
            context: 0,
            limit: GREP_DEFAULT_LIMIT,
        }
    }
}

/// A single content match (file, line number, line text).
#[derive(Debug, Clone)]
pub struct ContentMatch {
    pub file_path: PathBuf,
    pub line_number: usize,
    pub line_text: String,
}

/// Search file contents for a pattern (upstream grep tool execute). The
/// walk is parallel and gitignore-aware; matching uses the regex engine in
/// process.
pub fn grep_files(
    pattern: &str,
    search_dir: &str,
    cwd: &str,
    options: GrepOptions,
) -> Result<GrepResult, String> {
    let search_path = resolve_to_cwd(search_dir, cwd);
    let is_directory = search_path.is_dir();
    if !search_path.exists() {
        return Err(format!("Path not found: {}", search_path.display()));
    }

    let effective_limit = options.limit.max(1);
    let context_value = options.context;
    let regex = if options.literal {
        regex::RegexBuilder::new(&regex::escape(pattern))
            .case_insensitive(options.ignore_case)
            .build()
            .map_err(|e| format!("Invalid pattern: {e}"))?
    } else {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(options.ignore_case)
            .build()
            .map_err(|e| format!("Invalid pattern: {e}"))?
    };

    let file_glob: Option<GlobSet> = match &options.glob {
        Some(glob) => {
            let mut glob_builder = GlobBuilder::new(glob);
            glob_builder.literal_separator(glob.contains('/'));
            let built = glob_builder
                .build()
                .map_err(|e| format!("Invalid glob \"{glob}\": {e}"))?;
            Some(GlobSetBuilder::new().add(built).build().unwrap())
        }
        None => None,
    };

    let matches: Arc<Mutex<Vec<ContentMatch>>> = Arc::new(Mutex::new(Vec::new()));
    let limit_hit = Arc::new(AtomicUsize::new(0));

    let mut builder = WalkBuilder::new(&search_path);
    builder.hidden(true);
    builder.require_git(inside_git_repo(&search_path));
    builder.threads(
        std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(16),
    );

    let single_file = !is_directory;
    let walker = GrepWalker {
        regex,
        file_glob,
        matches: matches.clone(),
        limit: effective_limit,
        limit_hit: limit_hit.clone(),
        search_path: search_path.clone(),
        single_file,
    };
    builder
        .build_parallel()
        .visit(&mut GrepVisitorBuilder { walker });

    let mut matches = Arc::try_unwrap(matches)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| arc.lock().unwrap().clone());
    // Sort by (path, line) for deterministic output like rg's sequential walk.
    matches.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.line_number.cmp(&b.line_number))
    });
    finish_grep_output(
        matches,
        limit_hit.load(Ordering::SeqCst) == 1,
        effective_limit,
        context_value,
        is_directory,
        &search_path,
    )
}

fn format_grep_path(file_path: &Path, is_directory: bool, search_path: &Path) -> String {
    if is_directory {
        if let Ok(relative) = file_path.strip_prefix(search_path) {
            let rel = relative.to_string_lossy().replace('\\', "/");
            if !rel.is_empty() && !rel.starts_with("..") {
                return rel;
            }
        }
    }
    file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn finish_grep_output(
    matches: Vec<ContentMatch>,
    match_limit_reached: bool,
    effective_limit: usize,
    context_value: usize,
    is_directory: bool,
    search_path: &Path,
) -> Result<GrepResult, String> {
    if matches.is_empty() {
        return Ok(GrepResult {
            text: "No matches found".to_string(),
            ..Default::default()
        });
    }

    // Per-file line cache for context blocks (upstream fileCache).
    let mut file_cache: std::collections::BTreeMap<PathBuf, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut output_lines: Vec<String> = Vec::new();
    let mut lines_truncated = false;

    for ContentMatch {
        file_path,
        line_number,
        line_text,
    } in &matches
    {
        let relative_path = format_grep_path(file_path, is_directory, search_path);
        if context_value == 0 {
            let sanitized = line_text.replace('\r', "");
            let (truncated_text, was_truncated) = truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
            if was_truncated {
                lines_truncated = true;
            }
            output_lines.push(format!("{relative_path}:{line_number}: {truncated_text}"));
        } else {
            let lines = file_cache.entry(file_path.clone()).or_insert_with(|| {
                std::fs::read_to_string(file_path)
                    .unwrap_or_default()
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .lines()
                    .map(str::to_string)
                    .collect()
            });
            if lines.is_empty() {
                output_lines.push(format!(
                    "{relative_path}:{line_number}: (unable to read file)"
                ));
                continue;
            }
            let start = line_number.saturating_sub(context_value).max(1);
            let end = (*line_number + context_value).min(lines.len());
            for current in start..=end {
                let line_text = lines.get(current - 1).map(String::as_str).unwrap_or("");
                let sanitized = line_text.replace('\r', "");
                let (truncated_text, was_truncated) =
                    truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
                if was_truncated {
                    lines_truncated = true;
                }
                if current == *line_number {
                    output_lines.push(format!("{relative_path}:{current}: {truncated_text}"));
                } else {
                    output_lines.push(format!("{relative_path}-{current}- {truncated_text}"));
                }
            }
        }
    }

    let raw_output = output_lines.join("\n");
    let truncation = truncate_head(
        &raw_output,
        TruncationOptions {
            max_lines: Some(usize::MAX),
            max_bytes: None,
        },
    );
    let mut output = truncation.content.clone();
    let mut details = GrepResult::default();
    let mut notices: Vec<String> = Vec::new();
    if match_limit_reached {
        notices.push(format!(
            "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
            effective_limit * 2
        ));
        details.match_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!(
            "{} limit reached",
            crate::core::truncate::format_size(DEFAULT_MAX_BYTES)
        ));
        details.truncation_max_bytes = Some(DEFAULT_MAX_BYTES);
        details.truncation = Some(truncation.clone());
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        details.lines_truncated = true;
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    details.text = output;
    Ok(details)
}

// ============================================================================
// WalkParallel visitor plumbing
// ============================================================================

/// Shared state for the find walk (glob matcher + results).
struct FindWalker {
    globset: GlobSet,
    collected: Arc<Mutex<Vec<String>>>,
    limit: usize,
    reached_limit: Arc<AtomicUsize>,
    search_path: PathBuf,
}

struct FindVisitorBuilder {
    walker: FindWalker,
}

impl<'s> ignore::ParallelVisitorBuilder<'s> for FindVisitorBuilder {
    fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 's> {
        Box::new(FindVisitor {
            globset: self.walker.globset.clone(),
            collected: self.walker.collected.clone(),
            limit: self.walker.limit,
            reached_limit: self.walker.reached_limit.clone(),
            search_path: self.walker.search_path.clone(),
        })
    }
}

struct FindVisitor {
    globset: GlobSet,
    collected: Arc<Mutex<Vec<String>>>,
    limit: usize,
    reached_limit: Arc<AtomicUsize>,
    search_path: PathBuf,
}

impl ignore::ParallelVisitor for FindVisitor {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        if self.collected.lock().unwrap().len() >= self.limit {
            self.reached_limit.store(1, Ordering::SeqCst);
            return WalkState::Quit;
        }
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        let path = entry.path();
        if path == self.search_path {
            return WalkState::Continue;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            return WalkState::Continue;
        }
        // Hard ignore: node_modules and .git never match (upstream custom
        // glob backend's ignore list).
        if path.components().any(|c| {
            let name = c.as_os_str().to_string_lossy();
            name == "node_modules" || name == ".git"
        }) {
            return WalkState::Continue;
        }
        let relative = path.strip_prefix(&self.search_path).unwrap_or(path);
        let matched = relative.to_str().is_some_and(|rel| {
            self.globset.is_match(rel)
                || self.globset.is_match(
                    relative
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .as_ref(),
                )
        });
        if matched {
            let mut collected = self.collected.lock().unwrap();
            if collected.len() < self.limit {
                collected.push(relativize_find_result_path(path, &self.search_path));
            }
            if collected.len() >= self.limit {
                self.reached_limit.store(1, Ordering::SeqCst);
                return WalkState::Quit;
            }
        }
        WalkState::Continue
    }
}

/// Shared state for the grep walk (regex engine + matches).
struct GrepWalker {
    regex: regex::Regex,
    file_glob: Option<GlobSet>,
    matches: Arc<Mutex<Vec<ContentMatch>>>,
    limit: usize,
    limit_hit: Arc<AtomicUsize>,
    search_path: PathBuf,
    single_file: bool,
}

struct GrepVisitorBuilder {
    walker: GrepWalker,
}

impl<'s> ignore::ParallelVisitorBuilder<'s> for GrepVisitorBuilder {
    fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 's> {
        Box::new(GrepVisitor {
            regex: self.walker.regex.clone(),
            file_glob: self.walker.file_glob.clone(),
            matches: self.walker.matches.clone(),
            limit: self.walker.limit,
            limit_hit: self.walker.limit_hit.clone(),
            search_path: self.walker.search_path.clone(),
            single_file: self.walker.single_file,
        })
    }
}

struct GrepVisitor {
    regex: regex::Regex,
    file_glob: Option<GlobSet>,
    matches: Arc<Mutex<Vec<ContentMatch>>>,
    limit: usize,
    limit_hit: Arc<AtomicUsize>,
    search_path: PathBuf,
    single_file: bool,
}

impl ignore::ParallelVisitor for GrepVisitor {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        if self.matches.lock().unwrap().len() >= self.limit {
            self.limit_hit.store(1, Ordering::SeqCst);
            return WalkState::Quit;
        }
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        let path = entry.path().to_path_buf();
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            return WalkState::Continue;
        }
        // Single-file search mode only matches the target file.
        if self.single_file && path != self.search_path {
            return WalkState::Continue;
        }
        // Hard ignore: node_modules and .git never match.
        if path.components().any(|c| {
            let name = c.as_os_str().to_string_lossy();
            name == "node_modules" || name == ".git"
        }) {
            return WalkState::Continue;
        }
        if let Some(globset) = &self.file_glob {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let rel = path
                .strip_prefix(&self.search_path)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            if !globset.is_match(&name) && !globset.is_match(&rel) {
                return WalkState::Continue;
            }
        }
        // Skip binary-looking files (NUL byte in the first 1KB).
        let Ok(content) = std::fs::read(&path) else {
            return WalkState::Continue;
        };
        if content[..content.len().min(1024)].contains(&0u8) {
            return WalkState::Continue;
        }
        let text = String::from_utf8_lossy(&content);
        let mut count = self.matches.lock().unwrap().len();
        for (index, line) in text.lines().enumerate() {
            if self.regex.is_match(line) {
                if count >= self.limit {
                    self.limit_hit.store(1, Ordering::SeqCst);
                    return WalkState::Quit;
                }
                self.matches.lock().unwrap().push(ContentMatch {
                    file_path: path.clone(),
                    line_number: index + 1,
                    line_text: line.to_string(),
                });
                count += 1;
            }
        }
        if count >= self.limit {
            self.limit_hit.store(1, Ordering::SeqCst);
            return WalkState::Quit;
        }
        WalkState::Continue
    }
}

/// Serialize a truncation result to the upstream camelCase shape.
fn truncation_json(truncation: &crate::core::truncate::TruncationResult) -> serde_json::Value {
    serde_json::json!({
        "content": truncation.content,
        "truncated": truncation.truncated,
        "truncatedBy": truncation.truncated_by.map(|by| match by {
            crate::core::truncate::TruncatedBy::Lines => "lines",
            crate::core::truncate::TruncatedBy::Bytes => "bytes",
        }),
        "totalLines": truncation.total_lines,
        "totalBytes": truncation.total_bytes,
        "outputLines": truncation.output_lines,
        "outputBytes": truncation.output_bytes,
        "lastLinePartial": truncation.last_line_partial,
        "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
        "maxLines": truncation.max_lines,
        "maxBytes": truncation.max_bytes,
    })
}

/// The find tool parameter shape as JSON (upstream `findSchema`).
pub fn find_parameters_json() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},
            "path": {"type": "string", "description": "Directory to search in (default: current directory)"},
            "limit": {"type": "number", "description": "Maximum number of results (default: 1000)"}
        },
        "required": ["pattern"]
    })
}

/// The find tool description (upstream `description`).
pub fn find_description() -> String {
    format!(
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {} results or {}KB (whichever is hit first).",
        FIND_DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024
    )
}

/// Build the find tool as an `AgentTool` (upstream `createFindTool`).
pub fn find_tool(cwd: &str) -> AgentTool {
    let cwd = cwd.to_string();
    AgentTool {
        tool: pillar_ai::types::Tool {
            name: "find".to_string(),
            description: find_description(),
            parameters: find_parameters_json(),
            constrained_sampling: None,
        },
        label: "find".to_string(),
        prepare_arguments: None,
        execute: Arc::new(move |_id, args, signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return Err(ToolExecuteError("Operation aborted".to_string()));
                }
                let pattern = args
                    .get("pattern")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        ToolExecuteError("Missing required parameter: pattern".to_string())
                    })?;
                let search_dir = args
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or(".");
                let limit = args
                    .get("limit")
                    .and_then(|value| value.as_u64())
                    .map(|value| value as usize)
                    .unwrap_or(FIND_DEFAULT_LIMIT);
                let result = find_files(
                    pattern,
                    search_dir,
                    &cwd,
                    FindOptions {
                        limit,
                        hidden: false,
                    },
                )
                .map_err(ToolExecuteError)?;
                let mut details = serde_json::Map::new();
                if let Some(limit) = result.result_limit_reached {
                    details.insert("resultLimitReached".to_string(), serde_json::json!(limit));
                }
                if let Some(truncation) = &result.truncation {
                    details.insert("truncation".to_string(), truncation_json(truncation));
                }
                Ok(AgentToolResult {
                    content: vec![Content::text(result.text)],
                    details: serde_json::Value::Object(details),
                    ..Default::default()
                })
            })
        }),
        execution_mode: None,
    }
}

/// The grep tool parameter shape as JSON (upstream `grepSchema`).
pub fn grep_parameters_json() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Search pattern (regex or literal string)"},
            "path": {"type": "string", "description": "Directory or file to search (default: current directory)"},
            "glob": {"type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},
            "ignoreCase": {"type": "boolean", "description": "Case-insensitive search (default: false)"},
            "literal": {"type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)"},
            "context": {"type": "number", "description": "Number of lines to show before and after each match (default: 0)"},
            "limit": {"type": "number", "description": "Maximum number of matches to return (default: 100)"}
        },
        "required": ["pattern"]
    })
}

/// The grep tool description (upstream `description`).
pub fn grep_description() -> String {
    format!(
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {} matches or {}KB (whichever is hit first). Long lines are truncated to {} chars.",
        GREP_DEFAULT_LIMIT,
        DEFAULT_MAX_BYTES / 1024,
        GREP_MAX_LINE_LENGTH
    )
}

/// Build the grep tool as an `AgentTool` (upstream `createGrepTool`).
pub fn grep_tool(cwd: &str) -> AgentTool {
    let cwd = cwd.to_string();
    AgentTool {
        tool: pillar_ai::types::Tool {
            name: "grep".to_string(),
            description: grep_description(),
            parameters: grep_parameters_json(),
            constrained_sampling: None,
        },
        label: "grep".to_string(),
        prepare_arguments: None,
        execute: Arc::new(move |_id, args, signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return Err(ToolExecuteError("Operation aborted".to_string()));
                }
                let pattern = args
                    .get("pattern")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        ToolExecuteError("Missing required parameter: pattern".to_string())
                    })?;
                let search_dir = args
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or(".");
                let options = GrepOptions {
                    glob: args
                        .get("glob")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    ignore_case: args
                        .get("ignoreCase")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                    literal: args
                        .get("literal")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                    context: args
                        .get("context")
                        .and_then(|value| value.as_u64())
                        .map(|value| value as usize)
                        .unwrap_or(0),
                    limit: args
                        .get("limit")
                        .and_then(|value| value.as_u64())
                        .map(|value| value as usize)
                        .unwrap_or(GREP_DEFAULT_LIMIT),
                };
                let result =
                    grep_files(pattern, search_dir, &cwd, options).map_err(ToolExecuteError)?;
                let mut details = serde_json::Map::new();
                if let Some(limit) = result.match_limit_reached {
                    details.insert("matchLimitReached".to_string(), serde_json::json!(limit));
                }
                if let Some(truncation) = &result.truncation {
                    details.insert("truncation".to_string(), truncation_json(truncation));
                }
                if result.lines_truncated {
                    details.insert("linesTruncated".to_string(), serde_json::json!(true));
                }
                Ok(AgentToolResult {
                    content: vec![Content::text(result.text)],
                    details: serde_json::Value::Object(details),
                    ..Default::default()
                })
            })
        }),
        execution_mode: None,
    }
}
