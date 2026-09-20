//! Port of packages/coding-agent/src/modes/interactive/components/
//! session-selector.ts (pi v0.84.3): the `/resume` session picker — search,
//! scope (current folder / all), sort modes, name filter, tree display,
//! delete (with confirmation) and rename.
//!
//! divergence: pillar-tui's `Input` keeps keybinding dispatch host-side and
//! the session listing is async host work, so
//! [`SessionSelectorComponent::handle_key`] answers what the host must do
//! (resume / delete / rename / load a scope) instead of invoking callbacks;
//! the host reports loads and mutations back through mode methods.

use std::time::Instant;

use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::keys::matches_key;
use pillar_tui::text_utils::{truncate_to_width, visible_width};
use pillar_tui::tui::{Component, Focusable, RenderLines, render_lines};

use crate::core::session_manager::SessionInfo;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::{key_hint, key_text};
use crate::modes::interactive::components::session_selector_search::{
    NameFilter, SortMode, filter_and_sort_sessions, has_session_name,
};
use crate::modes::interactive::theme::theme;

/// Upstream `SessionScope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    Current,
    All,
}

/// The status line colour (upstream `{ type: "info" | "error" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
}

/// What the host must do after a key (upstream the component callbacks).
#[derive(Debug, Clone, PartialEq)]
pub enum SessionSelectorOutcome {
    /// Handled inside the selector (navigation, search, sort / filter /
    /// path toggles, loading state changes).
    Consumed,
    /// Enter: resume `path` (upstream `onSelect`).
    Resume(String),
    /// Tab toggled the scope; `needs_load` says the host must load it
    /// (upstream `toggleScope` starting an async `loadScope`).
    ScopeToggled {
        scope: SessionScope,
        needs_load: bool,
    },
    /// A confirmed delete (upstream `onDeleteSession`).
    Delete(String),
    /// A submitted rename (upstream the `renameSession` callback).
    Rename { path: String, name: String },
    /// Escape (upstream `onCancel`).
    Cancel,
}

/// Upstream `shortenPath`: `$HOME` → `~`.
fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return path.to_string();
    }
    match path.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

/// Upstream `formatSessionDate`: the relative age of a session.
fn format_session_date(modified_ms: u64, now_ms: u64) -> String {
    let diff_ms = now_ms.saturating_sub(modified_ms);
    let diff_mins = diff_ms / 60_000;
    let diff_hours = diff_ms / 3_600_000;
    let diff_days = diff_ms / 86_400_000;
    if diff_mins < 1 {
        return "now".to_string();
    }
    if diff_mins < 60 {
        return format!("{diff_mins}m");
    }
    if diff_hours < 24 {
        return format!("{diff_hours}h");
    }
    if diff_days < 7 {
        return format!("{diff_days}d");
    }
    if diff_days < 30 {
        return format!("{}w", diff_days / 7);
    }
    if diff_days < 365 {
        return format!("{}mo", diff_days / 30);
    }
    format!("{}y", diff_days / 365)
}

/// Upstream `canonicalizePath` (best effort: the path as-is when it cannot
/// resolve).
fn canonicalize_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|canonical| canonical.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// A session tree node for hierarchical display (upstream `SessionTreeNode`).
struct SessionTreeNode {
    session: SessionInfo,
    children: Vec<SessionTreeNode>,
    latest_activity: u64,
}
/// Flattened node for display with tree structure info (upstream
/// `FlatSessionNode`).
struct FlatSessionNode {
    session: SessionInfo,
    depth: usize,
    is_last: bool,
    /// For each ancestor level, whether there are more siblings after it.
    ancestor_continues: Vec<bool>,
}

/// Build the session tree from parent links; roots and children sort by the
/// latest activity in their subtree, descending (upstream
/// `buildSessionTree`).
fn build_session_tree(sessions: Vec<SessionInfo>) -> Vec<SessionTreeNode> {
    // Index by canonical path (upstream the same keying; a duplicate path
    // replaces the previous entry).
    let mut by_path: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut infos: Vec<SessionInfo> = Vec::new();
    for session in sessions {
        let key = canonicalize_path(&session.path);
        by_path.insert(key, infos.len());
        infos.push(session);
    }

    // Resolve parent links, then attach children by index (a parent outside
    // the listing — or a cycle — makes the node a root).
    let parent_links: Vec<Option<usize>> = infos
        .iter()
        .map(|session| {
            session
                .parent_session_path
                .as_deref()
                .map(canonicalize_path)
                .and_then(|parent| by_path.get(&parent).copied())
        })
        .collect();
    let mut child_indices: Vec<Vec<usize>> = vec![Vec::new(); infos.len()];
    let mut root_indices: Vec<usize> = Vec::new();
    for (index, parent) in parent_links.iter().copied().enumerate() {
        match parent {
            Some(parent) if parent != index => child_indices[parent].push(index),
            _ => root_indices.push(index),
        }
    }

    // Latest activity per subtree (upstream `updateLatestActivity`).
    fn latest(indices: &[usize], child_indices: &[Vec<usize>], infos: &[SessionInfo]) -> u64 {
        indices
            .iter()
            .map(|index| {
                let own = infos[*index].modified_ms;
                let children = latest(&child_indices[*index], child_indices, infos);
                own.max(children)
            })
            .max()
            .unwrap_or(0)
    }

    // Materialize the tree depth-first, skipping already-visited nodes (a
    // cyclic parent graph cannot hang the selector).
    fn materialize(
        index: usize,
        child_indices: &[Vec<usize>],
        infos: &[SessionInfo],
        visited: &mut std::collections::BTreeSet<usize>,
    ) -> SessionTreeNode {
        visited.insert(index);
        let mut children: Vec<SessionTreeNode> = Vec::new();
        for child in child_indices[index].clone() {
            if visited.contains(&child) {
                continue;
            }
            children.push(materialize(child, child_indices, infos, visited));
        }
        children.sort_by_key(|a| std::cmp::Reverse(a.latest_activity));
        SessionTreeNode {
            latest_activity: infos[index].modified_ms.max(
                children
                    .iter()
                    .map(|child| child.latest_activity)
                    .max()
                    .unwrap_or(0),
            ),
            session: infos[index].clone(),
            children,
        }
    }

    let mut visited = std::collections::BTreeSet::new();
    let mut roots: Vec<SessionTreeNode> = root_indices
        .into_iter()
        .map(|index| materialize(index, &child_indices, &infos, &mut visited))
        .collect();
    let _ = latest;
    roots.sort_by_key(|a| std::cmp::Reverse(a.latest_activity));
    roots
}

/// Flatten the tree into display rows with structure metadata (upstream
/// `flattenSessionTree`).
fn flatten_session_tree(roots: Vec<SessionTreeNode>) -> Vec<FlatSessionNode> {
    fn walk(
        node: SessionTreeNode,
        depth: usize,
        ancestor_continues: Vec<bool>,
        is_last: bool,
        out: &mut Vec<FlatSessionNode>,
    ) {
        let SessionTreeNode {
            session,
            children,
            latest_activity: _,
        } = node;
        out.push(FlatSessionNode {
            session,
            depth,
            is_last,
            ancestor_continues: ancestor_continues.clone(),
        });
        let child_count = children.len();
        for (index, child) in children.into_iter().enumerate() {
            let child_is_last = index == child_count - 1;
            // Only show the continuation line for non-root ancestors.
            let continues = if depth > 0 { !is_last } else { false };
            let mut continues_for_child = ancestor_continues.clone();
            continues_for_child.push(continues);
            walk(child, depth + 1, continues_for_child, child_is_last, out);
        }
    }

    let mut result = Vec::new();
    let root_count = roots.len();
    for (index, root) in roots.into_iter().enumerate() {
        walk(root, 0, Vec::new(), index == root_count - 1, &mut result);
    }
    result
}

/// Upstream `SessionSelectorHeader` / `SessionList` / the selector body,
/// merged into one component (upstream splits them for the callback shape).
pub struct SessionSelectorComponent {
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    current_sessions: Option<Vec<SessionInfo>>,
    all_sessions: Option<Vec<SessionInfo>>,
    current_loading: bool,
    all_loading: bool,
    /// Progress of the in-flight load (upstream `header.setProgress`).
    progress: Option<(usize, usize)>,
    filtered_sessions: Vec<FlatSessionNode>,
    selected_index: usize,
    search_input: Input,
    show_path: bool,
    confirming_delete_path: Option<String>,
    current_session_canonical: Option<String>,
    status: Option<(StatusKind, String)>,
    status_expires_at: Option<Instant>,
    rename_active: bool,
    rename_input: Input,
    rename_target_path: Option<String>,
    focused: bool,
    /// Upstream `maxVisible` (one line each).
    max_visible: usize,
}

impl SessionSelectorComponent {
    /// Upstream the constructor: the list starts empty and the host kicks
    /// off the "current folder" load right away (upstream
    /// `loadCurrentSessions`).
    pub fn new(current_session_path: Option<String>) -> Self {
        Self {
            scope: SessionScope::Current,
            sort_mode: SortMode::Threaded,
            name_filter: NameFilter::All,
            current_sessions: None,
            all_sessions: None,
            // Upstream the constructor calls `loadCurrentSessions()`, which
            // marks the current folder as loading right away.
            current_loading: true,
            all_loading: false,
            progress: None,
            filtered_sessions: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            show_path: false,
            confirming_delete_path: None,
            current_session_canonical: current_session_path.map(|path| canonicalize_path(&path)),
            status: None,
            status_expires_at: None,
            rename_active: false,
            rename_input: Input::new(),
            rename_target_path: None,
            focused: false,
            max_visible: 10,
        }
    }

    /// The highlighted session path (upstream
    /// `getSelectedSessionPath()`).
    pub fn selected_session_path(&self) -> Option<String> {
        self.filtered_sessions
            .get(self.selected_index)
            .map(|node| node.session.path.clone())
    }

    pub fn scope(&self) -> SessionScope {
        self.scope
    }

    /// The search box contents.
    pub fn search_value(&self) -> String {
        self.search_input.get_value().to_string()
    }

    /// The path awaiting delete confirmation (upstream
    /// `confirmingDeletePath`).
    pub fn confirming_delete_path(&self) -> Option<String> {
        self.confirming_delete_path.clone()
    }

    /// Whether the displayed scope is loading (upstream the header's loading
    /// flag tracks the requested scope).
    pub fn displayed_loading(&self) -> bool {
        match self.scope {
            SessionScope::Current => self.current_loading,
            SessionScope::All => self.all_loading,
        }
    }

    // --- host-driven state --------------------------------------------------

    /// Upstream `header.setLoading` on a scope load: progress resets per
    /// load.
    pub fn set_loading(&mut self, scope: SessionScope, loading: bool) {
        match scope {
            SessionScope::Current => self.current_loading = loading,
            SessionScope::All => self.all_loading = loading,
        }
        if loading {
            self.progress = None;
        }
    }

    /// Upstream `header.setProgress`.
    pub fn set_progress(&mut self, loaded: usize, total: usize) {
        self.progress = Some((loaded, total));
    }

    /// Upstream `setStatusMessage`; `auto_hide_ms` schedules the clear via
    /// [`Self::tick`] (upstream `setTimeout` + `requestRender`).
    pub fn set_status(&mut self, kind: StatusKind, message: &str, auto_hide_ms: Option<u64>) {
        self.status_expires_at =
            auto_hide_ms.map(|ms| Instant::now() + std::time::Duration::from_millis(ms));
        self.status = Some((kind, message.to_string()));
    }

    /// Expire a timed status message (upstream the `setTimeout` in
    /// `setStatusMessage`). Returns whether anything changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        if let Some(deadline) = self.status_expires_at
            && now >= deadline
        {
            self.status_expires_at = None;
            self.status = None;
            return true;
        }
        false
    }

    /// Upstream `SessionList.setSessions` (+ `setSessions` cache fills from
    /// the loaded scope).
    pub fn set_sessions(&mut self, sessions: Vec<SessionInfo>, show_cwd: bool) {
        match self.scope {
            SessionScope::Current => self.current_sessions = Some(sessions.clone()),
            SessionScope::All => self.all_sessions = Some(sessions.clone()),
        }
        let query = self.search_input.get_value().to_string();
        self.filter_sessions(&query, sessions, show_cwd);
    }

    /// Upstream `loadScope`'s resolution: fill the scope's cache, clear its
    /// loading flag, and re-filter.
    pub fn finish_load(&mut self, scope: SessionScope, sessions: Vec<SessionInfo>) {
        self.set_loading(scope, false);
        let show_cwd = scope == SessionScope::All;
        let query = self.search_input.get_value().to_string();
        match scope {
            SessionScope::Current => self.current_sessions = Some(sessions.clone()),
            SessionScope::All => self.all_sessions = Some(sessions.clone()),
        }
        self.filter_sessions(&query, sessions, show_cwd);
    }

    /// Upstream the delete continuation's cache cleanup: drop the path from
    /// both scope caches.
    pub fn remove_session(&mut self, path: &str) {
        if let Some(sessions) = self.current_sessions.as_mut() {
            sessions.retain(|session| session.path != path);
        }
        if let Some(sessions) = self.all_sessions.as_mut() {
            sessions.retain(|session| session.path != path);
        }
        self.refresh_filtered();
    }

    fn filter_sessions(&mut self, query: &str, sessions: Vec<SessionInfo>, show_cwd: bool) {
        let _ = show_cwd;
        let name_filtered: Vec<SessionInfo> = match self.name_filter {
            NameFilter::All => sessions,
            NameFilter::Named => sessions.into_iter().filter(has_session_name).collect(),
        };
        let trimmed = query.trim();
        self.filtered_sessions = if self.sort_mode == SortMode::Threaded && trimmed.is_empty() {
            // Threaded mode without search: tree structure.
            flatten_session_tree(build_session_tree(name_filtered))
        } else {
            filter_and_sort_sessions(&name_filtered, trimmed, self.sort_mode, self.name_filter)
                .into_iter()
                .map(|session| FlatSessionNode {
                    session,
                    depth: 0,
                    is_last: true,
                    ancestor_continues: Vec::new(),
                })
                .collect()
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered_sessions.len().saturating_sub(1));
    }

    fn current_list(&self) -> Vec<SessionInfo> {
        match self.scope {
            SessionScope::Current => self.current_sessions.clone().unwrap_or_default(),
            SessionScope::All => self.all_sessions.clone().unwrap_or_default(),
        }
    }

    fn refresh_filtered(&mut self) {
        let query = self.search_input.get_value().to_string();
        let show_cwd = self.scope == SessionScope::All;
        let sessions = self.current_list();
        self.filter_sessions(&query, sessions, show_cwd);
    }

    fn is_current_session_path(&self, path: &str) -> bool {
        self.current_session_canonical
            .as_ref()
            .is_some_and(|current| current == &canonicalize_path(path))
    }

    /// Upstream `startDeleteConfirmationForSelectedSession`.
    fn start_delete_confirmation(&mut self) -> SessionSelectorOutcome {
        let Some(selected) = self.filtered_sessions.get(self.selected_index) else {
            return SessionSelectorOutcome::Consumed;
        };
        if self.is_current_session_path(&selected.session.path) {
            self.set_status(
                StatusKind::Error,
                "Cannot delete the currently active session",
                None,
            );
            return SessionSelectorOutcome::Consumed;
        }
        self.confirming_delete_path = Some(selected.session.path.clone());
        SessionSelectorOutcome::Consumed
    }

    /// Host-driven key handling (upstream `handleInput`).
    pub fn handle_key(&mut self, data: &str) -> SessionSelectorOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        // Rename mode intercepts everything (upstream the `mode === "rename"`
        // branch).
        if self.rename_active {
            if matches("tui.select.cancel") {
                self.rename_active = false;
                self.rename_target_path = None;
                return SessionSelectorOutcome::Consumed;
            }
            if matches("tui.input.submit") {
                let value = self.rename_input.get_value().trim().to_string();
                let path = self.rename_target_path.clone();
                if value.is_empty() {
                    return SessionSelectorOutcome::Consumed;
                }
                let Some(path) = path else {
                    self.rename_active = false;
                    self.rename_target_path = None;
                    return SessionSelectorOutcome::Consumed;
                };
                self.rename_active = false;
                self.rename_target_path = None;
                return SessionSelectorOutcome::Rename { path, name: value };
            }
            if dispatch_input_keybinding(&mut self.rename_input, data) {
                return SessionSelectorOutcome::Consumed;
            }
            self.rename_input.handle_input(data);
            return SessionSelectorOutcome::Consumed;
        }

        // Delete confirmation intercepts all keys (upstream the
        // `confirmingDeletePath !== null` branch).
        if let Some(path) = self.confirming_delete_path.clone() {
            if matches("tui.select.confirm") {
                self.confirming_delete_path = None;
                return SessionSelectorOutcome::Delete(path);
            }
            if matches("tui.select.cancel") {
                self.confirming_delete_path = None;
            }
            // Ignore all other keys while confirming.
            return SessionSelectorOutcome::Consumed;
        }

        if matches("tui.input.tab") {
            // Upstream `toggleScope`: a cached scope shows immediately, an
            // uncached one asks the host to load.
            let next_scope = match self.scope {
                SessionScope::Current => SessionScope::All,
                SessionScope::All => SessionScope::Current,
            };
            let needs_load = match next_scope {
                SessionScope::All => self.all_sessions.is_none() && !self.all_loading,
                SessionScope::Current => false,
            };
            self.scope = next_scope;
            self.progress = None;
            let query = self.search_input.get_value().to_string();
            if next_scope == SessionScope::All {
                if let Some(sessions) = self.all_sessions.clone() {
                    self.filter_sessions(&query, sessions, true);
                    return SessionSelectorOutcome::Consumed;
                }
            } else {
                let sessions = self.current_sessions.clone().unwrap_or_default();
                self.filter_sessions(&query, sessions, false);
            }
            return SessionSelectorOutcome::ScopeToggled {
                scope: self.scope,
                needs_load,
            };
        }

        if matches("app.session.toggleSort") {
            self.sort_mode = match self.sort_mode {
                SortMode::Threaded => SortMode::Recent,
                SortMode::Recent => SortMode::Relevance,
                SortMode::Relevance => SortMode::Threaded,
            };
            self.refresh_filtered();
            return SessionSelectorOutcome::Consumed;
        }

        if matches("app.session.toggleNamedFilter") {
            self.name_filter = match self.name_filter {
                NameFilter::All => NameFilter::Named,
                NameFilter::Named => NameFilter::All,
            };
            self.refresh_filtered();
            return SessionSelectorOutcome::Consumed;
        }

        if matches("app.session.togglePath") {
            self.show_path = !self.show_path;
            return SessionSelectorOutcome::Consumed;
        }

        // Delete initiation (useful on terminals that do not distinguish
        // Ctrl+Backspace from Backspace).
        if matches("app.session.delete") {
            return self.start_delete_confirmation();
        }

        if matches("app.session.rename") {
            let Some(selected) = self.filtered_sessions.get(self.selected_index) else {
                return SessionSelectorOutcome::Consumed;
            };
            let current_name = selected.session.name.clone();
            self.rename_active = true;
            self.rename_target_path = Some(selected.session.path.clone());
            self.rename_input
                .set_value(current_name.as_deref().unwrap_or(""));
            return SessionSelectorOutcome::Consumed;
        }

        // Ctrl+Backspace: non-invasive delete alias; forwards to the search
        // input when a query is active.
        if matches("app.session.deleteNoninvasive") {
            if !self.search_input.get_value().is_empty() {
                if dispatch_input_keybinding(&mut self.search_input, data) {
                    self.refresh_filtered();
                    return SessionSelectorOutcome::Consumed;
                }
                self.search_input.handle_input(data);
                self.refresh_filtered();
                return SessionSelectorOutcome::Consumed;
            }
            return self.start_delete_confirmation();
        }

        if matches("tui.select.up") {
            self.selected_index = self.selected_index.saturating_sub(1);
            return SessionSelectorOutcome::Consumed;
        }
        if matches("tui.select.down") {
            self.selected_index =
                (self.selected_index + 1).min(self.filtered_sessions.len().saturating_sub(1));
            return SessionSelectorOutcome::Consumed;
        }
        if matches("tui.select.pageUp") {
            self.selected_index = self.selected_index.saturating_sub(self.max_visible);
            return SessionSelectorOutcome::Consumed;
        }
        if matches("tui.select.pageDown") {
            self.selected_index = (self.selected_index + self.max_visible)
                .min(self.filtered_sessions.len().saturating_sub(1));
            return SessionSelectorOutcome::Consumed;
        }
        if matches("tui.select.confirm") {
            let Some(selected) = self.filtered_sessions.get(self.selected_index) else {
                return SessionSelectorOutcome::Consumed;
            };
            return SessionSelectorOutcome::Resume(selected.session.path.clone());
        }
        // Ctrl+C: clear the search, or cancel when it is empty (upstream the
        // same branch).
        if matches_key(data, "ctrl+c") {
            if !self.search_input.get_value().is_empty() {
                self.search_input.set_value("");
                self.refresh_filtered();
                return SessionSelectorOutcome::Consumed;
            }
            return SessionSelectorOutcome::Cancel;
        }

        if matches("tui.select.cancel") {
            return SessionSelectorOutcome::Cancel;
        }

        // Everything else goes to the search input.
        if dispatch_input_keybinding(&mut self.search_input, data) {
            self.refresh_filtered();
            return SessionSelectorOutcome::Consumed;
        }
        self.search_input.handle_input(data);
        self.refresh_filtered();
        SessionSelectorOutcome::Consumed
    }

    // --- rendering ----------------------------------------------------------

    /// Upstream the header's `render`: title + scope/name/sort state + hint
    /// lines.
    fn render_header(&self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let title = match self.scope {
            SessionScope::Current => "Resume Session (Current Folder)",
            SessionScope::All => "Resume Session (All)",
        };
        let left_text = theme_handle.bold(title);

        let sort_label = match self.sort_mode {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        };
        let sort_text = theme_handle.fg("muted", "Sort: ") + &theme_handle.fg("accent", sort_label);

        let name_label = match self.name_filter {
            NameFilter::All => "All",
            NameFilter::Named => "Named",
        };
        let name_text = theme_handle.fg("muted", "Name: ") + &theme_handle.fg("accent", name_label);

        let scope_text = if self.displayed_loading() {
            let progress_text = self
                .progress
                .as_ref()
                .map(|(loaded, total)| format!("{loaded}/{total}"))
                .unwrap_or_else(|| "...".to_string());
            format!(
                "{}{}",
                theme_handle.fg("muted", "○ Current Folder | "),
                theme_handle.fg("accent", &format!("Loading {progress_text}"))
            )
        } else {
            match self.scope {
                SessionScope::Current => format!(
                    "{}{}",
                    theme_handle.fg("accent", "◉ Current Folder"),
                    theme_handle.fg("muted", " | ○ All")
                ),
                SessionScope::All => format!(
                    "{}{}",
                    theme_handle.fg("muted", "○ Current Folder | "),
                    theme_handle.fg("accent", "◉ All")
                ),
            }
        };

        let right_text = truncate_to_width(
            &format!("  {scope_text}  {name_text}  {sort_text}"),
            width,
            "",
            false,
        );
        let available_left = width.saturating_sub(visible_width(&right_text) + 1);
        let left = truncate_to_width(&left_text, available_left, "", false);
        let spacing =
            " ".repeat(width.saturating_sub(visible_width(&left) + visible_width(&right_text)));

        let (hint_line1, hint_line2) = if self.confirming_delete_path.is_some() {
            let confirm_hint = format!(
                "Delete session? {} · {}",
                key_hint("tui.select.confirm", "confirm"),
                key_hint("tui.select.cancel", "cancel"),
            );
            (
                theme_handle.fg(
                    "error",
                    &truncate_to_width(&confirm_hint, width, "…", false),
                ),
                String::new(),
            )
        } else if let Some((kind, message)) = &self.status {
            let color = match kind {
                StatusKind::Info => "accent",
                StatusKind::Error => "error",
            };
            (
                theme_handle.fg(color, &truncate_to_width(message, width, "…", false)),
                String::new(),
            )
        } else {
            let path_state = if self.show_path { "(on)" } else { "(off)" };
            let sep = theme_handle.fg("muted", " · ");
            let hint1 = format!(
                "{}{}{}",
                key_hint("tui.input.tab", "scope"),
                sep,
                theme_handle.fg("muted", "re:<pattern> regex · \"phrase\" exact"),
            );
            let mut hint2_parts = vec![
                key_hint("app.session.toggleSort", "sort"),
                key_hint("app.session.toggleNamedFilter", "named"),
                key_hint("app.session.delete", "delete"),
                key_hint("app.session.togglePath", &format!("path {path_state}")),
            ];
            // Upstream `showRenameHint` (always on in interactive mode).
            hint2_parts.push(key_hint("app.session.rename", "rename"));
            (hint1, hint2_parts.join(&sep))
        };

        vec![
            format!("{left}{spacing}{right_text}"),
            truncate_to_width(&hint_line1, width, "…", false),
            truncate_to_width(&hint_line2, width, "…", false),
        ]
    }

    /// Upstream `buildTreePrefix`.
    fn tree_prefix(node: &FlatSessionNode) -> String {
        if node.depth == 0 {
            return String::new();
        }
        let parts: String = node
            .ancestor_continues
            .iter()
            .map(|continues| if *continues { "│  " } else { "   " })
            .collect();
        let branch = if node.is_last { "└─ " } else { "├─ " };
        format!("{parts}{branch}")
    }

    /// Upstream the `SessionList.render` rows.
    fn render_rows(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(
            self.search_input
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());

        if self.filtered_sessions.is_empty() {
            let empty_message = if self.name_filter == NameFilter::Named {
                let toggle_key = key_text("app.session.toggleNamedFilter");
                if self.scope == SessionScope::All {
                    format!("  No named sessions found. Press {toggle_key} to show all.")
                } else {
                    format!(
                        "  No named sessions in current folder. Press {toggle_key} to show all, or Tab to view all."
                    )
                }
            } else if self.scope == SessionScope::All {
                "  No sessions found".to_string()
            } else {
                "  No sessions in current folder. Press Tab to view all.".to_string()
            };
            lines.push(truncate_to_width(
                &theme_handle.fg("muted", &empty_message),
                width,
                "…",
                false,
            ));
            return lines;
        }

        let start = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(
                self.filtered_sessions
                    .len()
                    .saturating_sub(self.max_visible),
            );
        let end = (start + self.max_visible).min(self.filtered_sessions.len());

        for (offset, node) in self.filtered_sessions[start..end].iter().enumerate() {
            let index = start + offset;
            let session = &node.session;
            let is_selected = index == self.selected_index;
            let is_confirming_delete =
                self.confirming_delete_path.as_deref() == Some(session.path.as_str());
            let is_current = self.is_current_session_path(&session.path);

            let prefix = Self::tree_prefix(node);
            let has_name = session.name.is_some();
            let display_text = session
                .name
                .clone()
                .unwrap_or_else(|| session.first_message.clone());
            let normalized_message = display_text
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect::<String>()
                .trim()
                .to_string();

            let age = format_session_date(session.modified_ms, now_ms());
            let msg_count = session.message_count.to_string();
            let mut right_part = format!("{msg_count} {age}");
            if self.scope == SessionScope::All && !session.cwd.is_empty() {
                right_part = format!("{} {right_part}", shorten_path(&session.cwd));
            }
            if self.show_path {
                right_part = format!("{} {right_part}", shorten_path(&session.path));
            }

            let cursor = if is_selected {
                theme_handle.fg("accent", "› ")
            } else {
                "  ".to_string()
            };

            let prefix_width = visible_width(&prefix);
            let right_width = visible_width(&right_part) + 2;
            let available_for_msg = width.saturating_sub(2 + prefix_width + right_width);
            let truncated_msg =
                truncate_to_width(&normalized_message, available_for_msg.max(10), "…", false);

            let message_color: Option<&str> = if is_confirming_delete {
                Some("error")
            } else if is_current {
                Some("accent")
            } else if has_name {
                Some("warning")
            } else {
                None
            };
            let mut styled_msg = match message_color {
                Some(color) => theme_handle.fg(color, &truncated_msg),
                None => truncated_msg.clone(),
            };
            if is_selected {
                styled_msg = theme_handle.bold(&styled_msg);
            }

            let left_part = format!("{cursor}{}{styled_msg}", theme_handle.fg("dim", &prefix));
            let left_width = visible_width(&left_part);
            let spacing = " ".repeat(
                width
                    .saturating_sub(left_width + visible_width(&right_part))
                    .max(1),
            );
            let styled_right = theme_handle.fg(
                if is_confirming_delete { "error" } else { "dim" },
                &right_part,
            );
            let line = format!("{left_part}{spacing}{styled_right}");
            if is_selected {
                lines.push(truncate_to_width(
                    &theme_handle.bg("selectedBg", &line),
                    width,
                    "",
                    false,
                ));
            } else {
                lines.push(truncate_to_width(&line, width, "", false));
            }
        }

        if start > 0 || end < self.filtered_sessions.len() {
            let scroll_text = format!(
                "  ({}/{})",
                self.selected_index + 1,
                self.filtered_sessions.len()
            );
            lines.push(truncate_to_width(
                &theme_handle.fg("muted", &scroll_text),
                width,
                "",
                false,
            ));
        }
        lines
    }

    /// Upstream the rename panel (`enterRenameMode`).
    fn render_rename_panel(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines = Vec::new();
        lines.extend(
            pillar_tui::components::Text::new(" Rename Session", 1, 0)
                .render(width)
                .iter()
                .map(|line| theme_handle.bold(line)),
        );
        lines.push(String::new());
        lines.extend(
            self.rename_input
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());
        lines.extend(
            pillar_tui::components::Text::new(
                &format!(
                    "  {} to save · {} to cancel",
                    key_text("tui.select.confirm"),
                    key_text("tui.select.cancel")
                ),
                1,
                0,
            )
            .render(width)
            .iter()
            .map(|line| theme_handle.fg("muted", line)),
        );
        lines
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Component for SessionSelectorComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.extend(
            DynamicBorder::with_color(Box::new(|text| theme().fg("accent", text)))
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        lines.push(String::new());
        if self.rename_active {
            lines.extend(self.render_rename_panel(width));
        } else {
            lines.extend(self.render_header(width));
            lines.push(String::new());
            lines.extend(self.render_rows(width));
        }
        lines.push(String::new());
        lines.extend(
            DynamicBorder::with_color(Box::new(|text| theme().fg("accent", text)))
                .render(width)
                .iter()
                .map(|line| line.to_string()),
        );
        render_lines(lines)
    }
}

impl Focusable for SessionSelectorComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        // Upstream propagates to `sessionList` / `renameInput` for the IME
        // cursor.
        self.search_input.focused = focused;
        self.rename_input.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
