//! Port of packages/tui/src/autocomplete.ts (pi v0.84.3): the slash
//! command + file path autocomplete decision core — prefix extraction
//! (quoted/@-prefixed tokens), path prefix parsing, completion value
//! building, directory listing suggestions, fuzzy file scoring, and
//! applyCompletion text surgery.
//!
//! divergences: the `fd` walk (walkDirectoryWithFd) shells out to a
//! process and stays host-side via the [`FdRunner`] callback; async
//! abort signals become plain parameter checks.

use std::path::{Path, PathBuf};

const PATH_DELIMITERS: [char; 5] = [' ', '\t', '"', '\'', '='];

fn to_display_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn find_last_delimiter(text: &str) -> isize {
    for (index, ch) in text.char_indices().rev() {
        if PATH_DELIMITERS.contains(&ch) {
            return index as isize;
        }
    }
    -1
}

fn is_token_start(text: &str, index: usize) -> bool {
    index == 0 || PATH_DELIMITERS.contains(&text.chars().nth(index - 1).unwrap_or('\0'))
}

fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut quote_start = None;
    for (index, ch) in text.char_indices() {
        if ch == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = Some(index);
            }
        }
    }
    in_quotes.then_some(quote_start).flatten()
}

fn extract_quoted_prefix(text: &str) -> Option<String> {
    let quote_start = find_unclosed_quote_start(text)?;
    let chars: Vec<char> = text.chars().collect();
    if quote_start > 0 && chars.get(quote_start - 1) == Some(&'@') {
        if !is_token_start(text, quote_start - 1) {
            return None;
        }
        return Some(text.chars().skip(quote_start - 1).collect());
    }
    if !is_token_start(text, quote_start) {
        return None;
    }
    Some(text.chars().skip(quote_start).collect())
}

fn parse_path_prefix(prefix: &str) -> (String, bool, bool) {
    if let Some(rest) = prefix.strip_prefix("@\"") {
        return (rest.to_string(), true, true);
    }
    if let Some(rest) = prefix.strip_prefix('"') {
        return (rest.to_string(), false, true);
    }
    if let Some(rest) = prefix.strip_prefix('@') {
        return (rest.to_string(), true, false);
    }
    (prefix.to_string(), false, false)
}

fn build_completion_value(
    path: &str,
    _is_directory: bool,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
) -> String {
    let needs_quotes = is_quoted_prefix || path.contains(' ');
    let prefix = if is_at_prefix { "@" } else { "" };
    if !needs_quotes {
        return format!("{prefix}{path}");
    }
    format!("{prefix}\"{path}\"")
}

/// An autocomplete suggestion (upstream `AutocompleteItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// A command registry entry (upstream `SlashCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
}

/// Suggestions at a cursor position (upstream
/// `AutocompleteSuggestions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    /// What we're matching against (e.g., "/" or "src/").
    pub prefix: String,
}

/// A file entry produced by the host walk (upstream the fd results).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub is_directory: bool,
}

/// Host-side `fd` walk callback (upstream `walkDirectoryWithFd`).
pub type FdRunner<'a> = &'a mut dyn FnMut(&str, &str, usize) -> Vec<FileEntry>;

/// The command name and argument text of an in-progress slash command
/// invocation: `/model op` → `("model", "op")` (upstream the private parsing
/// inside `getSuggestions`). `None` when the text is not `/name <args>`.
///
/// The port exposes it because argument completions stay host-side (upstream
/// `SlashCommand.getArgumentCompletions`), and the host must split the text the
/// same way the provider does.
pub fn slash_command_argument_prefix(text_before_cursor: &str) -> Option<(&str, &str)> {
    let rest = text_before_cursor.strip_prefix('/')?;
    let (name, arguments) = rest.split_once(' ')?;
    if name.is_empty() {
        return None;
    }
    Some((name, arguments))
}

/// The combined provider (upstream `CombinedAutocompleteProvider`).
pub struct CombinedAutocompleteProvider {
    commands: Vec<SlashCommand>,
    base_path: PathBuf,
    fd_path: Option<PathBuf>,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<SlashCommand>, base_path: &Path, fd_path: Option<&Path>) -> Self {
        Self {
            commands,
            base_path: base_path.to_path_buf(),
            fd_path: fd_path.map(Path::to_path_buf),
        }
    }

    /// Slash-command-only provider (no `fd` file search).
    pub fn commands_only(commands: Vec<SlashCommand>) -> Self {
        Self::new(commands, Path::new("."), None)
    }

    fn expand_home_path(&self, path: &str) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        if let Some(rest) = path.strip_prefix("~/") {
            let expanded = Path::new(&home).join(rest).to_string_lossy().to_string();
            if path.ends_with('/') && !expanded.ends_with('/') {
                format!("{expanded}/")
            } else {
                expanded
            }
        } else if path == "~" {
            home
        } else {
            path.to_string()
        }
    }

    /// Extract the @-prefixed token (upstream `extractAtPrefix`).
    pub fn extract_at_prefix(text: &str) -> Option<String> {
        if let Some(quoted) = extract_quoted_prefix(text)
            && quoted.starts_with("@\"")
        {
            return Some(quoted);
        }
        let last_delimiter = find_last_delimiter(text);
        let token_start = if last_delimiter == -1 {
            0
        } else {
            (last_delimiter + 1) as usize
        };
        if text.chars().nth(token_start) == Some('@') {
            return Some(text.chars().skip(token_start).collect());
        }
        None
    }

    /// Extract a path-like prefix (upstream `extractPathPrefix`).
    pub fn extract_path_prefix(text: &str, force_extract: bool) -> Option<String> {
        if let Some(quoted) = extract_quoted_prefix(text) {
            return Some(quoted);
        }
        let last_delimiter = find_last_delimiter(text);
        let path_prefix = if last_delimiter == -1 {
            text.to_string()
        } else {
            text.chars().skip((last_delimiter + 1) as usize).collect()
        };
        if force_extract {
            return Some(path_prefix);
        }
        if path_prefix.contains('/')
            || path_prefix.starts_with('.')
            || path_prefix.starts_with("~/")
        {
            return Some(path_prefix);
        }
        if path_prefix.is_empty() && text.ends_with(' ') {
            return Some(path_prefix);
        }
        None
    }

    /// Get suggestions (upstream `getSuggestions`; the fd walk is
    /// host-injected). `force` mirrors the Tab-completion trigger.
    pub fn get_suggestions(
        &self,
        lines: &[&str],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
        fd: FdRunner<'_>,
    ) -> Option<AutocompleteSuggestions> {
        let current_line = lines.get(cursor_line).copied().unwrap_or("");
        let text_before: String = current_line.chars().take(cursor_col).collect();

        // @-prefixed fuzzy file suggestions.
        if let Some(at_prefix) = Self::extract_at_prefix(&text_before) {
            let (raw_prefix, _is_at, is_quoted) = parse_path_prefix(&at_prefix);
            let suggestions = self.get_fuzzy_file_suggestions(&raw_prefix, is_quoted, fd);
            if suggestions.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions {
                items: suggestions,
                prefix: at_prefix,
            });
        }

        // Slash commands at the start of the line.
        if !force && text_before.starts_with('/') {
            if text_before.find(' ').is_none() {
                let prefix = &text_before[1..];
                let items: Vec<AutocompleteItem> = self
                    .commands
                    .iter()
                    .map(|command| {
                        let full_description = match (&command.argument_hint, &command.description)
                        {
                            (Some(hint), Some(desc)) => format!("{hint} — {desc}"),
                            (Some(hint), None) => hint.clone(),
                            (None, Some(desc)) => desc.clone(),
                            (None, None) => String::new(),
                        };
                        AutocompleteItem {
                            value: command.name.clone(),
                            label: command.name.clone(),
                            description: (!full_description.is_empty()).then_some(full_description),
                        }
                    })
                    .collect();
                let names: Vec<String> = items.iter().map(|item| item.value.clone()).collect();
                let filtered_names = pillar_tui_fuzzy_filter(&names, prefix);
                let filtered: Vec<AutocompleteItem> = items
                    .into_iter()
                    .filter(|item| filtered_names.contains(&item.value))
                    .collect();
                if filtered.is_empty() {
                    return None;
                }
                return Some(AutocompleteSuggestions {
                    items: filtered,
                    prefix: text_before.clone(),
                });
            }

            let (command_name, argument_text) =
                slash_command_argument_prefix(&text_before).expect("space checked above");
            let command = self
                .commands
                .iter()
                .find(|command| command.name == command_name)?;
            // Argument completions are host-driven (upstream
            // getArgumentCompletions); the port returns None here.
            let _ = (argument_text, command);
            return None;
        }

        // Plain path completion.
        let path_match = Self::extract_path_prefix(&text_before, force)?;
        let suggestions = self.get_file_suggestions(&path_match);
        if suggestions.is_empty() {
            return None;
        }
        Some(AutocompleteSuggestions {
            items: suggestions,
            prefix: path_match,
        })
    }

    /// Directory listing suggestions (upstream `getFileSuggestions`).
    pub fn get_file_suggestions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let (raw_prefix, is_at_prefix, is_quoted_prefix) = parse_path_prefix(prefix);
        let mut expanded_prefix = raw_prefix.clone();
        if expanded_prefix.starts_with('~') {
            expanded_prefix = self.expand_home_path(&expanded_prefix);
        }

        let is_root_prefix = matches!(raw_prefix.as_str(), "" | "./" | "../" | "~" | "~/" | "/")
            || (is_at_prefix && raw_prefix.is_empty());

        let (search_dir, search_prefix): (PathBuf, String) = if is_root_prefix
            || raw_prefix.ends_with('/')
        {
            let search_dir = if raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                PathBuf::from(&expanded_prefix)
            } else {
                self.base_path.join(&expanded_prefix)
            };
            (search_dir, String::new())
        } else {
            let expanded_path = Path::new(&expanded_prefix);
            let dir = expanded_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let file = expanded_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let search_dir = if raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                dir
            } else {
                self.base_path.join(dir)
            };
            (search_dir, file)
        };

        let Ok(entries) = std::fs::read_dir(&search_dir) else {
            return Vec::new();
        };
        let mut dir_entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        dir_entries.sort_by_key(|e| e.file_name());

        let mut suggestions: Vec<AutocompleteItem> = Vec::new();
        for entry in dir_entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name
                .to_lowercase()
                .starts_with(&search_prefix.to_lowercase())
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            // Symlinks to directories count as directories.
            let is_directory = metadata.is_dir()
                || (metadata.is_symlink()
                    && entry.path().metadata().map(|m| m.is_dir()).unwrap_or(false));

            let display_prefix = raw_prefix.as_str();
            let relative_path = if display_prefix.ends_with('/') {
                format!("{display_prefix}{name}")
            } else if display_prefix.contains('/') || display_prefix.contains('\\') {
                if let Some(home_relative) = display_prefix.strip_prefix("~/") {
                    let dir = parent_display(home_relative);
                    if dir == "." {
                        format!("~/{name}")
                    } else {
                        format!("~/{dir}/{name}")
                    }
                } else if display_prefix.starts_with('/') {
                    let dir = parent_display(display_prefix);
                    if dir == "/" {
                        format!("/{name}")
                    } else {
                        format!("{dir}/{name}")
                    }
                } else {
                    let dir = parent_display(display_prefix);
                    let joined = if dir == "." {
                        name.clone()
                    } else {
                        format!("{dir}/{name}")
                    };
                    if display_prefix.starts_with("./") && !joined.starts_with("./") {
                        format!("./{joined}")
                    } else {
                        joined
                    }
                }
            } else if display_prefix.starts_with('~') {
                format!("~/{name}")
            } else {
                name.clone()
            };

            let relative_path = to_display_path(&relative_path);
            let path_value = if is_directory {
                format!("{relative_path}/")
            } else {
                relative_path.clone()
            };
            let value =
                build_completion_value(&path_value, is_directory, is_at_prefix, is_quoted_prefix);
            suggestions.push(AutocompleteItem {
                value,
                label: format!("{name}{}", if is_directory { "/" } else { "" }),
                description: None,
            });
        }

        // Directories first, then alphabetical.
        suggestions.sort_by(|a, b| {
            let a_dir = a.value.ends_with('/');
            let b_dir = b.value.ends_with('/');
            match (a_dir, b_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.label.cmp(&b.label),
            }
        });
        suggestions
    }

    fn resolve_scoped_fuzzy_query(&self, raw_query: &str) -> Option<(PathBuf, String, String)> {
        let normalized = to_display_path(raw_query);
        let slash_index = normalized.rfind('/')?;
        let display_base = normalized[..=slash_index].to_string();
        let query = normalized[slash_index + 1..].to_string();
        let base_dir = if let Some(rest) = display_base.strip_prefix("~/") {
            self.base_path_for_home(rest)
        } else if display_base.starts_with('/') {
            PathBuf::from(&display_base)
        } else {
            self.base_path.join(&display_base)
        };
        if !base_dir.is_dir() {
            return None;
        }
        Some((base_dir, query, display_base))
    }

    fn base_path_for_home(&self, rest: &str) -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        Path::new(&home).join(rest)
    }

    fn scoped_path_for_display(&self, display_base: &str, relative_path: &str) -> String {
        let normalized = to_display_path(relative_path);
        if display_base == "/" {
            format!("/{normalized}")
        } else {
            format!("{}{}", to_display_path(display_base), normalized)
        }
    }

    /// Fuzzy file search (upstream `getFuzzyFileSuggestions`); the fd
    /// walk is host-injected.
    pub fn get_fuzzy_file_suggestions(
        &self,
        query: &str,
        is_quoted_prefix: bool,
        fd: FdRunner<'_>,
    ) -> Vec<AutocompleteItem> {
        if self.fd_path.is_none() {
            return Vec::new();
        }
        let scoped = self.resolve_scoped_fuzzy_query(query);
        let (fd_base_dir, fd_query) = match &scoped {
            Some((base_dir, query, _)) => (base_dir.clone(), query.clone()),
            None => (self.base_path.clone(), query.to_string()),
        };
        // Upstream: base-dir entries (max depth 1) first, then recursive
        // entries deduped.
        let base_dir_entries = fd(&fd_base_dir.to_string_lossy(), &fd_query, 1);
        let recursive_entries = fd(&fd_base_dir.to_string_lossy(), &fd_query, usize::MAX);
        let mut seen: std::collections::BTreeSet<String> =
            base_dir_entries.iter().map(|e| e.path.clone()).collect();
        let mut entries = base_dir_entries;
        for entry in recursive_entries {
            if seen.insert(entry.path.clone()) {
                entries.push(entry);
            }
        }

        let mut scored: Vec<(FileEntry, i64)> = entries
            .into_iter()
            .map(|entry| {
                let score = if fd_query.is_empty() {
                    1
                } else {
                    score_entry(&entry.path, &fd_query, entry.is_directory)
                };
                (entry, score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();

        scored.sort_by(|(a, a_score), (b, b_score)| {
            b_score.cmp(a_score).then_with(|| {
                let a_depth = to_display_path(&a.path)
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .count();
                let b_depth = to_display_path(&b.path)
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .count();
                a_depth.cmp(&b_depth).then_with(|| {
                    a.path
                        .len()
                        .cmp(&b.path.len())
                        .then_with(|| a.path.cmp(&b.path))
                })
            })
        });

        scored
            .into_iter()
            .take(20)
            .map(|(entry, _)| {
                let path_without_slash = if entry.is_directory {
                    entry.path.trim_end_matches('/').to_string()
                } else {
                    entry.path.clone()
                };
                let display_path = match &scoped {
                    Some((_, _, display_base)) => {
                        self.scoped_path_for_display(display_base, &path_without_slash)
                    }
                    None => path_without_slash.clone(),
                };
                let entry_name = Path::new(&path_without_slash)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path_without_slash.clone());
                let completion_path = if entry.is_directory {
                    format!("{display_path}/")
                } else {
                    display_path.clone()
                };
                AutocompleteItem {
                    value: build_completion_value(
                        &completion_path,
                        entry.is_directory,
                        true,
                        is_quoted_prefix,
                    ),
                    label: format!("{entry_name}{}", if entry.is_directory { "/" } else { "" }),
                    description: Some(display_path),
                }
            })
            .collect()
    }

    /// Whether Tab should trigger file completion (upstream
    /// `shouldTriggerFileCompletion`).
    pub fn should_trigger_file_completion(
        lines: &[&str],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let current_line = lines.get(cursor_line).copied().unwrap_or("");
        let text_before: String = current_line.chars().take(cursor_col).collect();
        let trimmed = text_before.trim();
        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            return false;
        }
        true
    }
}

fn parent_display(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match Path::new(trimmed).parent() {
        Some(parent) => {
            let parent = parent.to_string_lossy().to_string();
            if parent.is_empty() {
                ".".to_string()
            } else {
                parent
            }
        }
        None => ".".to_string(),
    }
}

/// Score an entry against the query (upstream `scoreEntry`): exact
/// filename 100, filename prefix 80, filename substring 50, path
/// substring 30; directories get +10.
pub fn score_entry(file_path: &str, query: &str, is_directory: bool) -> i64 {
    let file_name = Path::new(file_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let lower_file_name = file_name.to_lowercase();
    let lower_query = query.to_lowercase();
    let mut score: i64 = 0;
    if lower_file_name == lower_query {
        score = 100;
    } else if lower_file_name.starts_with(&lower_query) {
        score = 80;
    } else if lower_file_name.contains(&lower_query) {
        score = 50;
    } else if file_path.to_lowercase().contains(&lower_query) {
        score = 30;
    }
    if is_directory && score > 0 {
        score += 10;
    }
    score
}

fn pillar_tui_fuzzy_filter(items: &[String], query: &str) -> Vec<String> {
    use crate::fuzzy::fuzzy_match;
    if query.trim().is_empty() {
        return items.to_vec();
    }
    let tokens: Vec<String> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut results: Vec<(String, f64)> = Vec::new();
    for item in items {
        let text = item.to_lowercase();
        let mut total = 0.0;
        let mut all = true;
        for token in &tokens {
            let matched = fuzzy_match(token, &text);
            if matched.matches {
                total += matched.score;
            } else {
                all = false;
                break;
            }
        }
        if all {
            results.push((item.clone(), total));
        }
    }
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

/// Apply a selected completion (upstream `applyCompletion`): slash
/// commands get a trailing space and cursor after it; @-attachments
/// keep directories open for further completion; argument/path
/// completions splice the value in place.
pub fn apply_completion(
    lines: &[String],
    cursor_line: usize,
    cursor_col: usize,
    item: &AutocompleteItem,
    prefix: &str,
) -> (Vec<String>, usize, usize) {
    let current_line = lines.get(cursor_line).cloned().unwrap_or_default();
    let before_prefix: String = current_line
        .chars()
        .take(cursor_col.saturating_sub(prefix.chars().count()))
        .collect();
    let after_cursor: String = current_line.chars().skip(cursor_col).collect();
    let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
    let has_leading_quote_after = after_cursor.starts_with('"');
    let has_trailing_quote_in_item = item.value.ends_with('"');
    let adjusted_after =
        if is_quoted_prefix && has_trailing_quote_in_item && has_leading_quote_after {
            after_cursor[1..].to_string()
        } else {
            after_cursor
        };

    // Slash command name completion.
    let is_slash_command =
        prefix.starts_with('/') && before_prefix.trim().is_empty() && !prefix[1..].contains('/');
    if is_slash_command {
        let new_line = format!("{before_prefix}/{} {adjusted_after}", item.value);
        let mut new_lines = lines.to_vec();
        new_lines[cursor_line] = new_line;
        let cursor = before_prefix.chars().count() + item.value.chars().count() + 2;
        return (new_lines, cursor_line, cursor);
    }

    // @ file attachment.
    if prefix.starts_with('@') {
        let is_directory = item.label.ends_with('/');
        let suffix = if is_directory { "" } else { " " };
        let new_line = format!("{before_prefix}{}{suffix}{adjusted_after}", item.value);
        let mut new_lines = lines.to_vec();
        new_lines[cursor_line] = new_line;
        let has_trailing_quote = item.value.ends_with('"');
        let cursor_offset = if is_directory && has_trailing_quote {
            item.value.chars().count() - 1
        } else {
            item.value.chars().count()
        };
        let cursor = before_prefix.chars().count() + cursor_offset + suffix.chars().count();
        return (new_lines, cursor_line, cursor);
    }

    // Command argument context.
    let text_before: String = current_line.chars().take(cursor_col).collect();
    if text_before.contains('/') && text_before.contains(' ') {
        let new_line = format!("{before_prefix}{}{adjusted_after}", item.value);
        let mut new_lines = lines.to_vec();
        new_lines[cursor_line] = new_line;
        let is_directory = item.label.ends_with('/');
        let has_trailing_quote = item.value.ends_with('"');
        let cursor_offset = if is_directory && has_trailing_quote {
            item.value.chars().count() - 1
        } else {
            item.value.chars().count()
        };
        return (
            new_lines,
            cursor_line,
            before_prefix.chars().count() + cursor_offset,
        );
    }

    // Plain file path.
    let new_line = format!("{before_prefix}{}{adjusted_after}", item.value);
    let mut new_lines = lines.to_vec();
    new_lines[cursor_line] = new_line;
    let is_directory = item.label.ends_with('/');
    let has_trailing_quote = item.value.ends_with('"');
    let cursor_offset = if is_directory && has_trailing_quote {
        item.value.chars().count() - 1
    } else {
        item.value.chars().count()
    };
    (
        new_lines,
        cursor_line,
        before_prefix.chars().count() + cursor_offset,
    )
}
