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

use std::io::{BufRead, BufReader};
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

/// How many scanned lines pass between two abort checks inside one file
/// (the entry check alone would keep a huge file uninterruptible).
pub const ABORT_CHECK_LINES: usize = 256;

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
    /// The tool call's abort signal: the walk checks it per entry and the
    /// call answers upstream's "Operation aborted" instead of a partial list.
    pub signal: Option<pillar_agent::abort::AbortSignal>,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            limit: FIND_DEFAULT_LIMIT,
            hidden: true,
            signal: None,
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
    let aborted = Arc::new(AtomicUsize::new(0));

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
        aborted: aborted.clone(),
        signal: options.signal.clone(),
        search_path: search_path.clone(),
    };
    builder
        .build_parallel()
        .visit(&mut FindVisitorBuilder { walker });

    // Upstream rejects the whole call on abort instead of answering a
    // partial list.
    if aborted.load(Ordering::SeqCst) == 1 {
        return Err("Operation aborted".to_string());
    }

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
    /// The tool call's abort signal: the walk checks it per entry and per
    /// [`ABORT_CHECK_LINES`] scanned lines, and the call answers upstream's
    /// "Operation aborted" instead of a partial result.
    pub signal: Option<pillar_agent::abort::AbortSignal>,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            glob: None,
            ignore_case: false,
            literal: false,
            context: 0,
            limit: GREP_DEFAULT_LIMIT,
            signal: None,
        }
    }
}

/// The lines around one match, captured *while scanning* rather than by
/// re-reading (or retaining) the file at output time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextWindow {
    /// Up to `context` lines before the match (each already cut to
    /// [`GREP_MAX_LINE_LENGTH`], so a pathological line is not copied per
    /// match).
    pub before: Vec<String>,
    /// Up to `context` lines after the match.
    pub after: Vec<String>,
    /// Whether a line in this window was longer than [`GREP_MAX_LINE_LENGTH`].
    pub truncated: bool,
}

/// One context line as it will be rendered: line terminators removed and the
/// length budget applied *at capture*, so the search never holds a copy of a
/// huge line.
fn capture_context_line(text: &str, truncated: &mut bool) -> String {
    let (line, was_truncated) = truncate_line(&text.replace('\r', ""), GREP_MAX_LINE_LENGTH);
    if was_truncated {
        *truncated = true;
    }
    line
}

/// A single content match (file, line number, line text, and — when context was
/// requested — the captured window around it).
#[derive(Debug, Clone)]
pub struct ContentMatch {
    pub file_path: PathBuf,
    pub line_number: usize,
    pub line_text: String,
    /// `Some` when `context > 0`: the window the scan captured for this match.
    ///
    /// Keeping it per match bounds what the search retains by the *output* size
    /// (limit × (2·context + 1) lines) instead of by the corpus, and lets the
    /// output stage render without touching the file again.
    pub window: Option<ContextWindow>,
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
    let aborted = Arc::new(AtomicUsize::new(0));
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
        aborted: aborted.clone(),
        context: context_value,
        signal: options.signal.clone(),
        search_path: search_path.clone(),
        single_file,
    };
    builder
        .build_parallel()
        .visit(&mut GrepVisitorBuilder { walker });

    // Upstream rejects the whole call on abort instead of answering a
    // partial match list.
    if aborted.load(Ordering::SeqCst) == 1 {
        return Err("Operation aborted".to_string());
    }

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
    if is_directory && let Ok(relative) = file_path.strip_prefix(search_path) {
        let rel = relative.to_string_lossy().replace('\\', "/");
        if !rel.is_empty() && !rel.starts_with("..") {
            return rel;
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

    // `flush` enforces the limit while appending; this is the belt-and-braces
    // check that the rendered list can never exceed it.
    let matches = if matches.len() > effective_limit {
        matches[..effective_limit].to_vec()
    } else {
        matches
    };

    let mut output_lines: Vec<String> = Vec::new();
    let mut lines_truncated = false;

    for ContentMatch {
        file_path,
        line_number,
        line_text,
        window,
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
            let empty = ContextWindow::default();
            let captured = window.as_ref().unwrap_or(&empty);
            if captured.truncated {
                lines_truncated = true;
            }
            for (offset, context_line) in captured.before.iter().enumerate() {
                let current = line_number.saturating_sub(captured.before.len() - offset);
                output_lines.push(format!("{relative_path}-{current}- {context_line}"));
            }
            let (truncated_text, was_truncated) =
                truncate_line(&line_text.replace('\r', ""), GREP_MAX_LINE_LENGTH);
            if was_truncated {
                lines_truncated = true;
            }
            output_lines.push(format!("{relative_path}:{line_number}: {truncated_text}"));
            for (offset, context_line) in captured.after.iter().enumerate() {
                let current = line_number + 1 + offset;
                output_lines.push(format!("{relative_path}-{current}- {context_line}"));
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
    aborted: Arc<AtomicUsize>,
    signal: Option<pillar_agent::abort::AbortSignal>,
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
            aborted: self.walker.aborted.clone(),
            signal: self.walker.signal.clone(),
            search_path: self.walker.search_path.clone(),
        })
    }
}

struct FindVisitor {
    globset: GlobSet,
    collected: Arc<Mutex<Vec<String>>>,
    limit: usize,
    reached_limit: Arc<AtomicUsize>,
    aborted: Arc<AtomicUsize>,
    signal: Option<pillar_agent::abort::AbortSignal>,
    search_path: PathBuf,
}

impl FindVisitor {
    /// Whether the tool call was aborted (a cheap atomic check after the
    /// signal's first observation).
    fn aborted(&mut self) -> bool {
        if self.aborted.load(Ordering::SeqCst) == 1 {
            return true;
        }
        let aborted = self
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        if aborted {
            self.aborted.store(1, Ordering::SeqCst);
        }
        aborted
    }
}

impl ignore::ParallelVisitor for FindVisitor {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        if self.aborted() {
            return WalkState::Quit;
        }
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
    aborted: Arc<AtomicUsize>,
    /// Lines of context each match carries (captured during the scan, so the
    /// output stage never re-reads a file — upstream re-reads through
    /// `GrepOperations.readFile`).
    context: usize,
    signal: Option<pillar_agent::abort::AbortSignal>,
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
            aborted: self.walker.aborted.clone(),
            context: self.walker.context,
            signal: self.walker.signal.clone(),
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
    aborted: Arc<AtomicUsize>,
    context: usize,
    signal: Option<pillar_agent::abort::AbortSignal>,
    search_path: PathBuf,
    single_file: bool,
}

impl GrepVisitor {
    /// Whether the tool call was aborted (a cheap atomic check after the
    /// signal's first observation).
    fn aborted(&mut self) -> bool {
        if self.aborted.load(Ordering::SeqCst) == 1 {
            return true;
        }
        let aborted = self
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted());
        if aborted {
            self.aborted.store(1, Ordering::SeqCst);
        }
        aborted
    }

    /// Scan one file line by line. The file is streamed (no whole-file
    /// allocation, reusing one line buffer); when context is requested each
    /// match carries a bounded window (at most `context` lines each side) that
    /// is filled as the scan passes, so nothing is retained per file.
    fn scan_file(&mut self, path: &Path) -> WalkState {
        let Ok(file) = std::fs::File::open(path) else {
            return WalkState::Continue;
        };
        let mut reader = BufReader::new(file);
        // Skip binary-looking files (NUL byte in the first 1KB).
        let probe = match reader.fill_buf() {
            Ok(probe) => probe,
            Err(_) => return WalkState::Continue,
        };
        if probe[..probe.len().min(1024)].contains(&0u8) {
            return WalkState::Continue;
        }

        let total = self.matches.lock().unwrap().len();
        let context = self.context;
        let mut local: Vec<ContentMatch> = Vec::new();
        // The lines just before the current one: the `before` side of the next
        // match's window, bounded by `context`.
        let mut before: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        // Local matches still waiting for their `after` lines (a line can be
        // the trailing context of several).
        let mut open: Vec<usize> = Vec::new();
        // Once the limit is reached no new match is collected; the scan reads on
        // only to complete the open windows, so the last match is not left
        // without its trailing context.
        let mut limit_reached = false;
        // Whether a captured context line was cut to `GREP_MAX_LINE_LENGTH`.
        let mut captured_truncated = false;
        let mut raw: Vec<u8> = Vec::new();
        let mut line_number = 0usize;
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            line_number += 1;
            if line_number.is_multiple_of(ABORT_CHECK_LINES) && self.aborted() {
                return WalkState::Quit;
            }
            // `str::lines` semantics: drop the terminator, then one `\r`.
            let text = String::from_utf8_lossy(&raw);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let text = text.strip_suffix('\r').unwrap_or(text);
            if context > 0 {
                // Trailing context for the open windows (the match line itself
                // is part of the *previous* match's window, as upstream's
                // line-range rendering has it).
                let line = capture_context_line(text, &mut captured_truncated);
                for index in &open {
                    if let Some(window) = local[*index].window.as_mut()
                        && window.after.len() < context
                    {
                        window.after.push(line.clone());
                    }
                }
                open.retain(|index| {
                    local[*index]
                        .window
                        .as_ref()
                        .is_some_and(|window| window.after.len() < context)
                });
            }
            if self.regex.is_match(text) && !limit_reached {
                if total + local.len() >= self.limit {
                    // The limit is reached: stop collecting matches, but finish
                    // the windows already open.
                    limit_reached = true;
                    self.limit_hit.store(1, Ordering::SeqCst);
                } else {
                    local.push(ContentMatch {
                        file_path: path.to_path_buf(),
                        line_number,
                        line_text: text.to_string(),
                        window: (context > 0).then(|| ContextWindow {
                            before: before.iter().cloned().collect(),
                            after: Vec::new(),
                            truncated: false,
                        }),
                    });
                    if context > 0 {
                        open.push(local.len() - 1);
                    }
                }
            }
            if context > 0 {
                before.push_back(capture_context_line(text, &mut captured_truncated));
                while before.len() > context {
                    before.pop_front();
                }
            }
            if limit_reached && open.is_empty() {
                break;
            }
        }
        if captured_truncated {
            // The window lines are already cut; record that this file's windows
            // were so the output can say so.
            for entry in &mut local {
                if let Some(window) = entry.window.as_mut() {
                    window.truncated = true;
                }
            }
        }
        self.flush(path, &mut local);
        if limit_reached {
            return WalkState::Quit;
        }
        WalkState::Continue
    }

    /// Move one file's matches into the shared state: one lock per file instead
    /// of one per line.
    ///
    /// The limit is applied **here**, under the lock: workers used to read the
    /// shared count when they started and append unconditionally, so several
    /// files scanned in parallel could push the total past the limit. Appending
    /// only what fits makes the count a hard bound no matter how the walk
    /// interleaves.
    fn flush(&self, _path: &Path, local: &mut Vec<ContentMatch>) {
        if local.is_empty() {
            return;
        }
        let mut shared = self.matches.lock().unwrap();
        let room = self.limit.saturating_sub(shared.len());
        if local.len() > room {
            local.truncate(room);
            self.limit_hit.store(1, Ordering::SeqCst);
        }
        shared.append(local);
    }
}

impl ignore::ParallelVisitor for GrepVisitor {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        if self.aborted() {
            return WalkState::Quit;
        }
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
        self.scan_file(&path)
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
                        signal: signal.clone(),
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
                    signal: signal.clone(),
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
