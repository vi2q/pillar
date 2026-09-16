//! Port of packages/coding-agent/src/modes/interactive/components/
//! tree-selector.ts (pi v0.84.3): the `/tree` session-tree navigator — an
//! ASCII-art tree with filtering, folding, search, labels and copy.
//!
//! divergences:
//! - pillar-tui keeps keybinding dispatch host-side, so
//!   [`TreeSelectorComponent::handle_key`] answers what the host must do
//!   (navigate / copy / append a label change / cancel) instead of invoking
//!   `onSelect` / `onCopy` / `onLabelEdit` callbacks.
//! - Label timestamps render in UTC ([`format_label_timestamp`]); the port
//!   has no local-time formatter (upstream uses `Date`'s local getters).
//! - Upstream the constructor schedules `onCancel` after 100 ms when the tree
//!   is empty; the mode checks for an empty tree before opening the selector
//!   (`showTreeSelector`), so the port has no timer.

use std::collections::{BTreeSet, HashMap, HashSet};

use pillar_ai::types::{Content, Message, UserContent};
use pillar_tui::input::{Input, dispatch_input_keybinding};
use pillar_tui::keybindings::with_global_keybindings;
use pillar_tui::stack_layout::slice_by_column;
use pillar_tui::text_utils::{truncate_to_width, visible_width, wrap_text_with_ansi};
use pillar_tui::tui::{Component, Focusable};

use crate::core::messages::{CodingAgentMessage, CustomContent};
use crate::core::session_entries::SessionEntry;
use crate::core::session_manager::SessionTreeNode;
use crate::core::settings_manager::TreeFilterMode;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::{
    format_key_text, KeyTextFormatOptions, key_hint,
};
use crate::modes::interactive::theme::theme;

const TREE_GUTTER_WIDTH: usize = 2;
const MIN_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 4;
const MAX_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 20;
const MIN_ANCHOR_CONTEXT_WIDTH: usize = 2;
const MAX_ANCHOR_CONTEXT_WIDTH: usize = 12;

/// Gutter info: position (displayIndent where the connector was) and whether
/// to show `│` (upstream `GutterInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GutterInfo {
    position: usize,
    show: bool,
}

/// Flattened tree node for navigation (upstream `FlatNode`).
#[derive(Debug, Clone)]
struct FlatNode {
    node: SessionTreeNode,
    /// Indentation level (each level = 3 chars).
    indent: usize,
    /// Whether to show the connector (`├─` / `└─`).
    show_connector: bool,
    /// If `show_connector`, true = last sibling (`└─`).
    is_last: bool,
    /// Gutter info for each ancestor branch point.
    gutters: Vec<GutterInfo>,
    /// True if this node is a root under a virtual branching root.
    is_virtual_root_child: bool,
}

/// One rendered row for the horizontal viewport (upstream
/// `HorizontalViewportRow`).
struct HorizontalViewportRow {
    gutter: String,
    body: String,
    anchor_col: usize,
    body_width: usize,
    is_selected: bool,
}

/// Tool call info for lookup (upstream `ToolCallInfo`).
#[derive(Debug, Clone)]
struct ToolCallInfo {
    name: String,
    arguments: serde_json::Value,
}

/// What the host must do after a key (upstream the component callbacks).
#[derive(Debug, Clone, PartialEq)]
pub enum TreeSelectorOutcome {
    /// Handled inside the selector (navigation, folding, filters, search,
    /// label input handling).
    Consumed,
    /// Enter: navigate to `entry_id` (upstream `onSelect`).
    Select(String),
    /// The copy binding (upstream `onCopy`): the selected entry's text, if
    /// any.
    Copy(Option<String>),
    /// A label was submitted (upstream `onLabelChange`): append a label
    /// change entry for `entry_id`.
    LabelChanged {
        entry_id: String,
        label: Option<String>,
    },
    /// Escape with an empty search / select-cancel (upstream `onCancel`).
    Cancel,
}

/// Render tree rows into a horizontally clipped viewport (upstream
/// `renderHorizontalViewport`).
///
/// The tree gutter is always kept visible. The row bodies are shifted left
/// only when the selected row's anchor would otherwise be too far right to
/// see useful content.
fn render_horizontal_viewport(rows: &[HorizontalViewportRow], width: usize) -> Vec<String> {
    let viewport_width = width.saturating_sub(TREE_GUTTER_WIDTH);
    let max_body_width = rows.iter().map(|row| row.body_width).max().unwrap_or(0);
    let max_horizontal_scroll = max_body_width.saturating_sub(viewport_width);
    let selected_row = rows.iter().find(|row| row.is_selected);

    let mut horizontal_scroll = 0usize;
    if let Some(selected_row) = selected_row {
        if max_horizontal_scroll > 0 {
            let min_visible_anchor_content_width = MAX_VISIBLE_ANCHOR_CONTENT_WIDTH.min(
                MIN_VISIBLE_ANCHOR_CONTENT_WIDTH.max(viewport_width / 3),
            );
            if selected_row.anchor_col > viewport_width.saturating_sub(min_visible_anchor_content_width)
            {
                let anchor_context_width =
                    MAX_ANCHOR_CONTEXT_WIDTH.min(MIN_ANCHOR_CONTEXT_WIDTH.max(viewport_width / 4));
                horizontal_scroll = max_horizontal_scroll
                    .min(selected_row.anchor_col.saturating_sub(anchor_context_width));
            }
        }
    }

    rows.iter()
        .map(|row| {
            let line = if horizontal_scroll > 0 {
                format!(
                    "{}{}\x1b[0m",
                    row.gutter,
                    slice_by_column(&row.body, horizontal_scroll, viewport_width, true)
                )
            } else {
                format!("{}{}", row.gutter, row.body)
            };
            truncate_to_width(&line, width, "", false)
        })
        .collect()
}

// --- message helpers -------------------------------------------------------

/// Upstream `message.role` for the coding-agent message union.
fn message_role(message: &CodingAgentMessage) -> &'static str {
    match message {
        CodingAgentMessage::Base(Message::User { .. }) => "user",
        CodingAgentMessage::Base(Message::Assistant(_)) => "assistant",
        CodingAgentMessage::Base(Message::ToolResult(_)) => "toolResult",
        CodingAgentMessage::BashExecution(_) => "bashExecution",
        CodingAgentMessage::Custom(_) => "custom",
        CodingAgentMessage::BranchSummary(_) => "branchSummary",
        CodingAgentMessage::CompactionSummary(_) => "compactionSummary",
    }
}

fn content_blocks_text(blocks: &[Content]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_blocks_text(blocks),
    }
}

fn custom_content_text(content: &[CustomContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            CustomContent::Text(text) => Some(text.clone()),
            CustomContent::Image { .. } => None,
        })
        .collect()
}

/// Upstream `getEntryDisplayText`'s `extractContent` (capped at 200 chars).
fn extract_content(content: &str) -> String {
    content.chars().take(200).collect()
}

/// Upstream the `normalize` helper of `getEntryDisplayText`.
fn normalize(s: &str) -> String {
    s.replace(['\n', '\t'], " ").trim().to_string()
}

/// Upstream `shortenPath` (HOME → `~`).
fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Upstream `formatToolCall`.
fn format_tool_call(name: &str, args: &serde_json::Value) -> String {
    let string_arg = |key: &str| -> String {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let path_arg = || -> String {
        let path = string_arg("path");
        let path = if path.is_empty() {
            string_arg("file_path")
        } else {
            path
        };
        shorten_path(&path)
    };
    match name {
        "read" => {
            let mut display = path_arg();
            let offset = args.get("offset").and_then(serde_json::Value::as_u64);
            let limit = args.get("limit").and_then(serde_json::Value::as_u64);
            if offset.is_some() || limit.is_some() {
                let start = offset.unwrap_or(1);
                let end = limit.map(|limit| start + limit - 1);
                display += &match end {
                    Some(end) => format!(":{start}-{end}"),
                    None => format!(":{start}"),
                };
            }
            format!("[read: {display}]")
        }
        "write" => format!("[write: {}]", path_arg()),
        "edit" => format!("[edit: {}]", path_arg()),
        "bash" => {
            let raw = string_arg("command");
            let cmd: String = raw
                .replace(['\n', '\t'], " ")
                .trim()
                .chars()
                .take(50)
                .collect();
            let ellipsis = if raw.chars().count() > 50 { "..." } else { "" };
            format!("[bash: {cmd}{ellipsis}]")
        }
        "grep" => {
            let pattern = string_arg("pattern");
            let path = args.get("path").and_then(serde_json::Value::as_str).unwrap_or(".");
            format!("[grep: /{pattern}/ in {}]", shorten_path(path))
        }
        "find" => {
            let pattern = string_arg("pattern");
            let path = args.get("path").and_then(serde_json::Value::as_str).unwrap_or(".");
            format!("[find: {pattern} in {}]", shorten_path(path))
        }
        "ls" => {
            let path = args.get("path").and_then(serde_json::Value::as_str).unwrap_or(".");
            format!("[ls: {}]", shorten_path(path))
        }
        _ => {
            // Custom tool: name plus truncated JSON args.
            let args_str = serde_json::to_string(args).unwrap_or_default();
            let truncated: String = args_str.chars().take(40).collect();
            let ellipsis = if args_str.chars().count() > 40 { "..." } else { "" };
            format!("[{name}: {truncated}{ellipsis}]")
        }
    }
}

/// Upstream `formatLabelTimestamp` (UTC; see the module divergence note).
fn format_label_timestamp(timestamp_ms: u64) -> String {
    let secs = (timestamp_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = crate::core::session_manager::civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;

    let now = pillar_ai::models::now_ms();
    let now_secs = (now / 1000) as i64;
    let now_days = now_secs.div_euclid(86_400);
    let (now_year, now_month, now_day) = crate::core::session_manager::civil_from_days(now_days);
    let time = format!("{hour:02}:{minute:02}");

    if year == now_year && month == now_month && day == now_day {
        return time;
    }
    if year == now_year {
        return format!("{month}/{day} {time}");
    }
    format!("{:02}/{month}/{day} {time}", year % 100)
}

// --- tree list -------------------------------------------------------------

/// Tree list component with selection and ASCII-art visualization (upstream
/// `TreeList`).
struct TreeList {
    flat_nodes: Vec<FlatNode>,
    /// Indices into `flat_nodes` (upstream `filteredNodes`).
    filtered_indices: Vec<usize>,
    selected_index: usize,
    current_leaf_id: Option<String>,
    max_visible_lines: usize,
    filter_mode: TreeFilterMode,
    search_query: String,
    tool_call_map: HashMap<String, ToolCallInfo>,
    multiple_roots: bool,
    show_label_timestamps: bool,
    active_path_ids: HashSet<String>,
    visible_parent_map: HashMap<String, Option<String>>,
    visible_children_map: HashMap<Option<String>, Vec<String>>,
    last_selected_id: Option<String>,
    folded_nodes: BTreeSet<String>,
}

impl TreeList {
    fn new(
        tree: &[SessionTreeNode],
        current_leaf_id: Option<String>,
        max_visible_lines: usize,
        initial_selected_id: Option<&str>,
        initial_filter_mode: TreeFilterMode,
    ) -> Self {
        let multiple_roots = tree.len() > 1;
        let mut list = Self {
            flat_nodes: Vec::new(),
            filtered_indices: Vec::new(),
            selected_index: 0,
            current_leaf_id,
            max_visible_lines,
            filter_mode: initial_filter_mode,
            search_query: String::new(),
            tool_call_map: HashMap::new(),
            multiple_roots,
            show_label_timestamps: false,
            active_path_ids: HashSet::new(),
            visible_parent_map: HashMap::new(),
            visible_children_map: HashMap::new(),
            last_selected_id: None,
            folded_nodes: BTreeSet::new(),
        };
        list.flat_nodes = list.flatten_tree(tree);
        list.build_active_path();
        list.apply_filter();

        let target_id = initial_selected_id
            .map(str::to_string)
            .or_else(|| list.current_leaf_id.clone());
        list.selected_index = list.find_nearest_visible_index(target_id.as_deref());
        list.last_selected_id = list
            .filtered_indices
            .get(list.selected_index)
            .map(|&index| list.flat_nodes[index].node.entry.id().to_string());
        list
    }

    fn entry_id(&self, flat_index: usize) -> &str {
        self.flat_nodes[flat_index].node.entry.id()
    }

    fn selected_flat_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected_index).copied()
    }

    /// Upstream `findNearestVisibleIndex`.
    fn find_nearest_visible_index(&self, entry_id: Option<&str>) -> usize {
        if self.filtered_indices.is_empty() {
            return 0;
        }
        let entry_map: HashMap<&str, usize> = self
            .flat_nodes
            .iter()
            .enumerate()
            .map(|(index, flat_node)| (flat_node.node.entry.id(), index))
            .collect();
        let visible_id_to_index: HashMap<&str, usize> = self
            .filtered_indices
            .iter()
            .enumerate()
            .map(|(index, &flat_index)| (self.entry_id(flat_index), index))
            .collect();

        let mut current_id = entry_id.map(str::to_string);
        while let Some(id) = current_id {
            if let Some(&index) = visible_id_to_index.get(id.as_str()) {
                return index;
            }
            let Some(&flat_index) = entry_map.get(id.as_str()) else {
                break;
            };
            current_id = self.flat_nodes[flat_index]
                .node
                .entry
                .parent_id()
                .map(str::to_string);
        }

        // Fallback: the last visible entry.
        self.filtered_indices.len() - 1
    }

    /// Upstream `buildActivePath`.
    fn build_active_path(&mut self) {
        self.active_path_ids.clear();
        let Some(leaf_id) = self.current_leaf_id.clone() else {
            return;
        };
        let entry_map: HashMap<String, usize> = self
            .flat_nodes
            .iter()
            .enumerate()
            .map(|(index, flat_node)| (flat_node.node.entry.id().to_string(), index))
            .collect();
        let mut current_id = Some(leaf_id);
        while let Some(id) = current_id {
            self.active_path_ids.insert(id.clone());
            let Some(&flat_index) = entry_map.get(&id) else {
                break;
            };
            current_id = self.flat_nodes[flat_index]
                .node
                .entry
                .parent_id()
                .map(str::to_string);
        }
    }

    /// Upstream `flattenTree`.
    fn flatten_tree(&mut self, roots: &[SessionTreeNode]) -> Vec<FlatNode> {
        let mut result: Vec<FlatNode> = Vec::new();
        self.tool_call_map.clear();

        // Which subtrees contain the active leaf (iterative post-order).
        let mut contains_active: HashMap<String, bool> = HashMap::new();
        let leaf_id = self.current_leaf_id.clone();
        {
            let mut all_nodes: Vec<&SessionTreeNode> = Vec::new();
            let mut pre_order_stack: Vec<&SessionTreeNode> = roots.iter().collect();
            while let Some(node) = pre_order_stack.pop() {
                all_nodes.push(node);
                for child in node.children.iter().rev() {
                    pre_order_stack.push(child);
                }
            }
            for node in all_nodes.iter().rev() {
                let mut has = leaf_id
                    .as_deref()
                    .is_some_and(|leaf| node.entry.id() == leaf);
                for child in &node.children {
                    if contains_active.get(child.entry.id()).copied().unwrap_or(false) {
                        has = true;
                    }
                }
                contains_active.insert(node.entry.id().to_string(), has);
            }
        }

        // Add roots in reverse order, prioritizing the active branch.
        let multiple_roots = roots.len() > 1;
        let mut ordered_roots: Vec<&SessionTreeNode> = roots.iter().collect();
        ordered_roots.sort_by_key(|node| {
            std::cmp::Reverse(
                contains_active
                    .get(node.entry.id())
                    .copied()
                    .unwrap_or(false),
            )
        });

        // Stack items: (node, indent, justBranched, showConnector, isLast,
        // gutters, isVirtualRootChild).
        type StackItem<'a> = (
            &'a SessionTreeNode,
            usize,
            bool,
            bool,
            bool,
            Vec<GutterInfo>,
            bool,
        );
        let mut stack: Vec<StackItem> = Vec::new();
        for (index, node) in ordered_roots.iter().enumerate().rev() {
            let is_last = index == ordered_roots.len() - 1;
            stack.push((
                node,
                usize::from(multiple_roots),
                multiple_roots,
                multiple_roots,
                is_last,
                Vec::new(),
                multiple_roots,
            ));
        }

        while let Some((node, indent, just_branched, show_connector, is_last, gutters, is_virtual_root_child)) =
            stack.pop()
        {
            let entry = &node.entry;
            if let SessionEntry::Message(message_entry) = entry {
                if let CodingAgentMessage::Base(Message::Assistant(assistant)) = &message_entry.message {
                    for block in &assistant.content {
                        if let Content::ToolCall { id, name, arguments, .. } = block {
                            self.tool_call_map.insert(
                                id.clone(),
                                ToolCallInfo {
                                    name: name.clone(),
                                    arguments: arguments.clone(),
                                },
                            );
                        }
                    }
                }
            }

            result.push(FlatNode {
                node: node.clone(),
                indent,
                show_connector,
                is_last,
                gutters: gutters.clone(),
                is_virtual_root_child,
            });

            let children = &node.children;
            let multiple_children = children.len() > 1;

            // Order children so the branch containing the active leaf comes
            // first.
            let mut ordered_children: Vec<&SessionTreeNode> = children.iter().collect();
            ordered_children.sort_by_key(|child| {
                std::cmp::Reverse(
                    contains_active
                        .get(child.entry.id())
                        .copied()
                        .unwrap_or(false),
                )
            });

            // Upstream splits the first two cases, but both are `indent + 1`.
            let child_indent =
                if multiple_children || (just_branched && indent > 0) {
                    indent + 1
                } else {
                    indent
                };

            let connector_displayed = show_connector && !is_virtual_root_child;
            let current_display_indent = if self.multiple_roots {
                indent.saturating_sub(1)
            } else {
                indent
            };
            let connector_position = current_display_indent.saturating_sub(1);
            let child_gutters: Vec<GutterInfo> = if connector_displayed {
                let mut gutters = gutters.clone();
                gutters.push(GutterInfo {
                    position: connector_position,
                    show: !is_last,
                });
                gutters
            } else {
                gutters.clone()
            };

            for (index, child) in ordered_children.iter().enumerate().rev() {
                let child_is_last = index == ordered_children.len() - 1;
                stack.push((
                    child,
                    child_indent,
                    multiple_children,
                    multiple_children,
                    child_is_last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }

        result
    }

    /// Upstream `applyFilter`.
    fn apply_filter(&mut self) {
        // Preserve the selection when switching through empty filter
        // results.
        if !self.filtered_indices.is_empty() {
            let selected = self
                .filtered_indices
                .get(self.selected_index)
                .map(|&index| self.entry_id(index).to_string());
            self.last_selected_id = selected.or_else(|| self.last_selected_id.clone());
        }

        let search_tokens: Vec<String> = self
            .search_query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();

        let current_leaf_id = self.current_leaf_id.clone();
        let filter_mode = self.filter_mode;
        let mut indices: Vec<usize> = Vec::new();
        for (index, flat_node) in self.flat_nodes.iter().enumerate() {
            let entry = &flat_node.node.entry;
            let is_current_leaf = current_leaf_id.as_deref() == Some(entry.id());

            // Hide assistant messages with only tool calls (no text) unless
            // error/aborted; always show the current leaf.
            if let SessionEntry::Message(message_entry) = entry {
                if let CodingAgentMessage::Base(Message::Assistant(assistant)) = &message_entry.message
                {
                    if !is_current_leaf {
                        let has_text = assistant.content.iter().any(|block| match block {
                            Content::Text { text, .. } => !text.trim().is_empty(),
                            _ => false,
                        });
                        let is_error_or_aborted = !matches!(
                            assistant.stop_reason,
                            pillar_ai::types::StopReason::Stop | pillar_ai::types::StopReason::ToolUse
                        );
                        if !has_text && !is_error_or_aborted {
                            continue;
                        }
                    }
                }
            }

            let is_settings_entry = matches!(
                entry,
                SessionEntry::Label(_)
                    | SessionEntry::Custom(_)
                    | SessionEntry::ModelChange(_)
                    | SessionEntry::ThinkingLevelChange(_)
                    | SessionEntry::SessionInfo(_)
            );

            let passes_filter = match filter_mode {
                TreeFilterMode::UserOnly => {
                    matches!(entry, SessionEntry::Message(m) if matches!(&m.message, CodingAgentMessage::Base(Message::User { .. })))
                }
                TreeFilterMode::NoTools => {
                    !is_settings_entry
                        && !matches!(entry, SessionEntry::Message(m) if matches!(&m.message, CodingAgentMessage::Base(Message::ToolResult(_))))
                }
                TreeFilterMode::LabeledOnly => flat_node.node.label.is_some(),
                TreeFilterMode::All => true,
                TreeFilterMode::Default => !is_settings_entry,
            };
            if !passes_filter {
                continue;
            }

            if !search_tokens.is_empty() {
                let node_text = self.get_searchable_text(&flat_node.node).to_lowercase();
                if !search_tokens.iter().all(|token| node_text.contains(token)) {
                    continue;
                }
            }

            indices.push(index);
        }

        // Filter out descendants of folded nodes.
        if !self.folded_nodes.is_empty() {
            let mut skip_set: BTreeSet<String> = BTreeSet::new();
            for flat_node in &self.flat_nodes {
                let (id, parent_id) = (
                    flat_node.node.entry.id().to_string(),
                    flat_node.node.entry.parent_id().map(str::to_string),
                );
                if let Some(parent_id) = parent_id {
                    if self.folded_nodes.contains(&parent_id) || skip_set.contains(&parent_id) {
                        skip_set.insert(id);
                    }
                }
            }
            indices.retain(|&index| !skip_set.contains(self.entry_id(index)));
        }

        self.filtered_indices = indices;

        // Recalculate the visual structure for the visible tree.
        self.recalculate_visual_structure();

        // Preserve the cursor on the same node, or the nearest visible
        // ancestor.
        if let Some(last_selected_id) = self.last_selected_id.clone() {
            self.selected_index = self.find_nearest_visible_index(Some(&last_selected_id));
        } else if self.selected_index >= self.filtered_indices.len() {
            self.selected_index = self.filtered_indices.len().saturating_sub(1);
        }

        if !self.filtered_indices.is_empty() {
            let selected = self
                .filtered_indices
                .get(self.selected_index)
                .map(|&index| self.entry_id(index).to_string());
            self.last_selected_id = selected.or_else(|| self.last_selected_id.clone());
        }
    }

    /// Upstream `recalculateVisualStructure`.
    fn recalculate_visual_structure(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }

        let visible_ids: HashSet<String> = self
            .filtered_indices
            .iter()
            .map(|&index| self.entry_id(index).to_string())
            .collect();

        let entry_map: HashMap<String, usize> = self
            .flat_nodes
            .iter()
            .enumerate()
            .map(|(index, flat_node)| (flat_node.node.entry.id().to_string(), index))
            .collect();

        let parent_of = |entry_map: &HashMap<String, usize>,
                         flat_nodes: &[FlatNode],
                         id: &str|
         -> Option<String> {
            entry_map.get(id).and_then(|&index| {
                flat_nodes[index]
                    .node
                    .entry
                    .parent_id()
                    .map(str::to_string)
            })
        };

        let find_visible_ancestor = |id: &str| -> Option<String> {
            let mut current = parent_of(&entry_map, &self.flat_nodes, id);
            while let Some(candidate) = current {
                if visible_ids.contains(&candidate) {
                    return Some(candidate);
                }
                current = parent_of(&entry_map, &self.flat_nodes, &candidate);
            }
            None
        };

        let mut visible_parent: HashMap<String, Option<String>> = HashMap::new();
        let mut visible_children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        visible_children.insert(None, Vec::new());

        for &index in &self.filtered_indices {
            let node_id = self.entry_id(index).to_string();
            let ancestor_id = find_visible_ancestor(&node_id);
            visible_parent.insert(node_id.clone(), ancestor_id.clone());
            visible_children
                .entry(ancestor_id)
                .or_default()
                .push(node_id);
        }

        let visible_root_ids = visible_children.get(&None).cloned().unwrap_or_default();
        self.multiple_roots = visible_root_ids.len() > 1;

        // DFS over the visible tree using flattenTree() indentation
        // semantics.
        type StackItem = (
            String,
            usize,
            bool,
            bool,
            bool,
            Vec<GutterInfo>,
            bool,
        );
        let mut stack: Vec<StackItem> = Vec::new();
        for (index, node_id) in visible_root_ids.iter().enumerate().rev() {
            let is_last = index == visible_root_ids.len() - 1;
            stack.push((
                node_id.clone(),
                usize::from(self.multiple_roots),
                self.multiple_roots,
                self.multiple_roots,
                is_last,
                Vec::new(),
                self.multiple_roots,
            ));
        }

        while let Some((node_id, indent, just_branched, show_connector, is_last, gutters, is_virtual_root_child)) =
            stack.pop()
        {
            let Some(&flat_index) = entry_map.get(&node_id) else {
                continue;
            };
            {
                let flat_node = &mut self.flat_nodes[flat_index];
                flat_node.indent = indent;
                flat_node.show_connector = show_connector;
                flat_node.is_last = is_last;
                flat_node.gutters = gutters.clone();
                flat_node.is_virtual_root_child = is_virtual_root_child;
            }

            let children = visible_children.get(&Some(node_id.clone())).cloned().unwrap_or_default();
            let multiple_children = children.len() > 1;
            // Upstream splits the first two cases, but both are `indent + 1`.
            let child_indent =
                if multiple_children || (just_branched && indent > 0) {
                    indent + 1
                } else {
                    indent
                };

            let connector_displayed = show_connector && !is_virtual_root_child;
            let current_display_indent = if self.multiple_roots {
                indent.saturating_sub(1)
            } else {
                indent
            };
            let connector_position = current_display_indent.saturating_sub(1);
            let child_gutters = if connector_displayed {
                let mut gutters = gutters.clone();
                gutters.push(GutterInfo {
                    position: connector_position,
                    show: !is_last,
                });
                gutters
            } else {
                gutters.clone()
            };

            for (index, child) in children.iter().enumerate().rev() {
                let child_is_last = index == children.len() - 1;
                stack.push((
                    child.clone(),
                    child_indent,
                    multiple_children,
                    multiple_children,
                    child_is_last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }

        self.visible_parent_map = visible_parent;
        self.visible_children_map = visible_children;
    }

    /// Upstream `getSearchableText`.
    fn get_searchable_text(&self, node: &SessionTreeNode) -> String {
        let entry = &node.entry;
        let mut parts: Vec<String> = Vec::new();

        if let Some(label) = &node.label {
            parts.push(label.clone());
        }

        match entry {
            SessionEntry::Message(message_entry) => {
                let message = &message_entry.message;
                parts.push(message_role(message).to_string());
                let content = match message {
                    CodingAgentMessage::Base(Message::User { content, .. }) => user_content_text(content),
                    CodingAgentMessage::Base(Message::Assistant(assistant)) => {
                        content_blocks_text(&assistant.content)
                    }
                    CodingAgentMessage::Base(Message::ToolResult(result)) => {
                        content_blocks_text(&result.content)
                    }
                    CodingAgentMessage::Custom(custom) => custom_content_text(&custom.content),
                    CodingAgentMessage::BashExecution(bash) => {
                        parts.push(bash.command.clone());
                        String::new()
                    }
                    CodingAgentMessage::BranchSummary(summary) => summary.summary.clone(),
                    CodingAgentMessage::CompactionSummary(summary) => summary.summary.clone(),
                };
                if !content.is_empty() {
                    parts.push(content);
                }
            }
            SessionEntry::CustomMessage(custom) => {
                parts.push(custom.custom_type.clone());
                parts.push(custom_content_text(&custom.content));
            }
            SessionEntry::Compaction(_) => parts.push("compaction".to_string()),
            SessionEntry::BranchSummary(summary) => {
                parts.push("branch summary".to_string());
                parts.push(summary.summary.clone());
            }
            SessionEntry::SessionInfo(info) => {
                parts.push("title".to_string());
                if let Some(name) = &info.name {
                    parts.push(name.clone());
                }
            }
            SessionEntry::ModelChange(change) => {
                parts.push("model".to_string());
                parts.push(change.model_id.clone());
            }
            SessionEntry::ThinkingLevelChange(change) => {
                parts.push("thinking".to_string());
                parts.push(change.thinking_level.clone());
            }
            SessionEntry::Custom(custom) => {
                parts.push("custom".to_string());
                parts.push(custom.custom_type.clone());
            }
            SessionEntry::Label(label) => {
                parts.push("label".to_string());
                parts.push(label.label.clone().unwrap_or_default());
            }
        }

        parts.join(" ")
    }

    fn get_status_labels(&self) -> String {
        let mut labels = String::new();
        match self.filter_mode {
            TreeFilterMode::NoTools => labels += " [no-tools]",
            TreeFilterMode::UserOnly => labels += " [user]",
            TreeFilterMode::LabeledOnly => labels += " [labeled]",
            TreeFilterMode::All => labels += " [all]",
            TreeFilterMode::Default => {}
        }
        if self.show_label_timestamps {
            labels += " [+label time]";
        }
        labels
    }

    /// Upstream `getEntryDisplayText`.
    fn get_entry_display_text(&self, node: &SessionTreeNode, is_selected: bool) -> String {
        let theme_handle = theme();
        let entry = &node.entry;
        let result: String = match entry {
            SessionEntry::Message(message_entry) => {
                let message = &message_entry.message;
                match message {
                    CodingAgentMessage::Base(Message::User { content, .. }) => {
                        let content = normalize(&extract_content(&user_content_text(content)));
                        format!("{}{content}", theme_handle.fg("accent", "user: "))
                    }
                    CodingAgentMessage::Base(Message::Assistant(assistant)) => {
                        let text_content = normalize(&extract_content(&content_blocks_text(
                            &assistant.content,
                        )));
                        if !text_content.is_empty() {
                            format!("{}{text_content}", theme_handle.fg("success", "assistant: "))
                        } else if assistant.stop_reason == pillar_ai::types::StopReason::Aborted {
                            format!(
                                "{}{}",
                                theme_handle.fg("success", "assistant: "),
                                theme_handle.fg("muted", "(aborted)")
                            )
                        } else if let Some(error_message) = &assistant.error_message {
                            let error: String =
                                normalize(error_message).chars().take(80).collect();
                            format!(
                                "{}{}",
                                theme_handle.fg("success", "assistant: "),
                                theme_handle.fg("error", &error)
                            )
                        } else {
                            format!(
                                "{}{}",
                                theme_handle.fg("success", "assistant: "),
                                theme_handle.fg("muted", "(no content)")
                            )
                        }
                    }
                    CodingAgentMessage::Base(Message::ToolResult(result)) => {
                        let tool_call = self.tool_call_map.get(&result.tool_call_id);
                        match tool_call {
                            Some(tool_call) => theme_handle
                                .fg("muted", &format_tool_call(&tool_call.name, &tool_call.arguments)),
                            None => theme_handle.fg(
                                "muted",
                                &format!(
                                    "[{}]",
                                    if result.tool_name.is_empty() {
                                        "tool"
                                    } else {
                                        &result.tool_name
                                    }
                                ),
                            ),
                        }
                    }
                    CodingAgentMessage::BashExecution(bash) => theme_handle.fg(
                        "dim",
                        &format!("[bash]: {}", normalize(&bash.command)),
                    ),
                    CodingAgentMessage::Custom(_) => theme_handle.fg("dim", "[custom]"),
                    CodingAgentMessage::BranchSummary(_) => {
                        theme_handle.fg("dim", "[branchSummary]")
                    }
                    CodingAgentMessage::CompactionSummary(_) => {
                        theme_handle.fg("dim", "[compactionSummary]")
                    }
                }
            }
            SessionEntry::CustomMessage(custom) => format!(
                "{}{}",
                theme_handle.fg("customMessageLabel", &format!("[{}]: ", custom.custom_type)),
                normalize(&custom_content_text(&custom.content))
            ),
            SessionEntry::Compaction(compaction) => {
                let tokens = compaction.tokens_before / 1000;
                theme_handle.fg("borderAccent", &format!("[compaction: {tokens}k tokens]"))
            }
            SessionEntry::BranchSummary(summary) => format!(
                "{}{}",
                theme_handle.fg("warning", "[branch summary]: "),
                normalize(&summary.summary)
            ),
            SessionEntry::ModelChange(change) => {
                theme_handle.fg("dim", &format!("[model: {}]", change.model_id))
            }
            SessionEntry::ThinkingLevelChange(change) => {
                theme_handle.fg("dim", &format!("[thinking: {}]", change.thinking_level))
            }
            SessionEntry::Custom(custom) => {
                theme_handle.fg("dim", &format!("[custom: {}]", custom.custom_type))
            }
            SessionEntry::Label(label) => theme_handle.fg(
                "dim",
                &format!(
                    "[label: {}]",
                    label.label.as_deref().unwrap_or("(cleared)")
                ),
            ),
            SessionEntry::SessionInfo(info) => match &info.name {
                Some(name) => format!(
                    "{}{}{}",
                    theme_handle.fg("dim", "[title: "),
                    theme_handle.fg("dim", name),
                    theme_handle.fg("dim", "]")
                ),
                None => format!(
                    "{}{}{}",
                    theme_handle.fg("dim", "[title: "),
                    theme_handle.italic(&theme_handle.fg("dim", "empty")),
                    theme_handle.fg("dim", "]")
                ),
            },
        };

        if is_selected {
            theme_handle.bold(&result)
        } else {
            result
        }
    }

    /// Upstream `getEntryCopyText`.
    fn get_entry_copy_text(&self, node: &SessionTreeNode) -> Option<String> {
        let entry = &node.entry;
        let text: Option<String> = match entry {
            SessionEntry::Message(message_entry) => match &message_entry.message {
                CodingAgentMessage::BashExecution(bash) => Some(bash.command.clone()),
                CodingAgentMessage::Base(Message::User { content, .. }) => {
                    Some(user_content_text(content))
                }
                CodingAgentMessage::Base(Message::Assistant(assistant)) => {
                    Some(content_blocks_text(&assistant.content))
                }
                CodingAgentMessage::Base(Message::ToolResult(result)) => {
                    Some(content_blocks_text(&result.content))
                }
                CodingAgentMessage::Custom(custom) => Some(custom_content_text(&custom.content)),
                CodingAgentMessage::BranchSummary(summary) => Some(summary.summary.clone()),
                CodingAgentMessage::CompactionSummary(summary) => Some(summary.summary.clone()),
            },
            SessionEntry::CustomMessage(custom) => Some(custom_content_text(&custom.content)),
            SessionEntry::Compaction(compaction) => Some(compaction.summary.clone()),
            SessionEntry::BranchSummary(summary) => Some(summary.summary.clone()),
            _ => None,
        };

        // Upstream falls back to the assistant error message when the text is
        // empty.
        let text = match text {
            Some(text) if !text.is_empty() => Some(text),
            _ => {
                if let SessionEntry::Message(message_entry) = entry {
                    if let CodingAgentMessage::Base(Message::Assistant(assistant)) =
                        &message_entry.message
                    {
                        return assistant
                            .error_message
                            .clone()
                            .filter(|text| !text.trim().is_empty());
                    }
                }
                None
            }
        };

        text.filter(|text| !text.trim().is_empty())
    }

    fn is_foldable(&self, entry_id: &str) -> bool {
        let Some(children) = self.visible_children_map.get(&Some(entry_id.to_string())) else {
            return false;
        };
        if children.is_empty() {
            return false;
        }
        let parent_id = self.visible_parent_map.get(entry_id).cloned().flatten();
        match parent_id {
            None => true,
            Some(parent_id) => self
                .visible_children_map
                .get(&Some(parent_id))
                .is_some_and(|siblings| siblings.len() > 1),
        }
    }

    /// Upstream `findBranchSegmentStart`.
    fn find_branch_segment_start(&self, direction: BranchDirection) -> usize {
        let Some(&selected_flat_index) = self.filtered_indices.get(self.selected_index) else {
            return self.selected_index;
        };
        let selected_id = self.entry_id(selected_flat_index).to_string();

        let index_by_entry_id: HashMap<String, usize> = self
            .filtered_indices
            .iter()
            .enumerate()
            .map(|(index, &flat_index)| (self.entry_id(flat_index).to_string(), index))
            .collect();

        let mut current_id = selected_id;
        if direction == BranchDirection::Down {
            loop {
                let children = self
                    .visible_children_map
                    .get(&Some(current_id.clone()))
                    .cloned()
                    .unwrap_or_default();
                if children.is_empty() {
                    return index_by_entry_id.get(&current_id).copied().unwrap_or(self.selected_index);
                }
                if children.len() > 1 {
                    return index_by_entry_id
                        .get(&children[0])
                        .copied()
                        .unwrap_or(self.selected_index);
                }
                current_id = children[0].clone();
            }
        }

        loop {
            let parent_id = self.visible_parent_map.get(&current_id).cloned().flatten();
            let Some(parent_id) = parent_id else {
                return index_by_entry_id.get(&current_id).copied().unwrap_or(self.selected_index);
            };
            let children = self
                .visible_children_map
                .get(&Some(parent_id.clone()))
                .cloned()
                .unwrap_or_default();
            if children.len() > 1 {
                let segment_start = index_by_entry_id
                    .get(&current_id)
                    .copied()
                    .unwrap_or(self.selected_index);
                if segment_start < self.selected_index {
                    return segment_start;
                }
            }
            current_id = parent_id;
        }
    }

    /// Host-driven key handling (upstream `TreeList.handleInput`).
    fn handle_key(&mut self, data: &str) -> TreeSelectorOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));

        if matches("tui.select.up") {
            self.selected_index = if self.selected_index == 0 {
                self.filtered_indices.len().saturating_sub(1)
            } else {
                self.selected_index - 1
            };
        } else if matches("tui.select.down") {
            self.selected_index = if self.selected_index + 1 >= self.filtered_indices.len() {
                0
            } else {
                self.selected_index + 1
            };
        } else if matches("app.tree.foldOrUp") {
            let current_id = self
                .selected_flat_index()
                .map(|index| self.entry_id(index).to_string());
            if let Some(current_id) = current_id {
                if self.is_foldable(&current_id) && !self.folded_nodes.contains(&current_id) {
                    self.folded_nodes.insert(current_id);
                    self.apply_filter();
                } else {
                    self.selected_index = self.find_branch_segment_start(BranchDirection::Up);
                }
            }
        } else if matches("app.tree.unfoldOrDown") {
            let current_id = self
                .selected_flat_index()
                .map(|index| self.entry_id(index).to_string());
            if let Some(current_id) = current_id {
                if self.folded_nodes.contains(&current_id) {
                    self.folded_nodes.remove(&current_id);
                    self.apply_filter();
                } else {
                    self.selected_index = self.find_branch_segment_start(BranchDirection::Down);
                }
            }
        } else if matches("tui.editor.cursorLeft") || matches("tui.select.pageUp") {
            self.selected_index = self.selected_index.saturating_sub(self.max_visible_lines);
        } else if matches("tui.editor.cursorRight") || matches("tui.select.pageDown") {
            self.selected_index = (self.selected_index + self.max_visible_lines)
                .min(self.filtered_indices.len().saturating_sub(1));
        } else if matches("tui.select.confirm") {
            if let Some(index) = self.selected_flat_index() {
                return TreeSelectorOutcome::Select(self.entry_id(index).to_string());
            }
        } else if matches("app.message.copy") {
            return TreeSelectorOutcome::Copy(self.copy_selected());
        } else if matches("tui.select.cancel") {
            if !self.search_query.is_empty() {
                self.search_query.clear();
                self.folded_nodes.clear();
                self.apply_filter();
            } else {
                return TreeSelectorOutcome::Cancel;
            }
        } else if matches("app.tree.filter.default") {
            self.filter_mode = TreeFilterMode::Default;
            self.folded_nodes.clear();
            self.apply_filter();
        } else if matches("app.tree.filter.noTools") {
            self.filter_mode = if self.filter_mode == TreeFilterMode::NoTools {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::NoTools
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if matches("app.tree.filter.userOnly") {
            self.filter_mode = if self.filter_mode == TreeFilterMode::UserOnly {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::UserOnly
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if matches("app.tree.filter.labeledOnly") {
            self.filter_mode = if self.filter_mode == TreeFilterMode::LabeledOnly {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::LabeledOnly
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if matches("app.tree.filter.all") {
            self.filter_mode = if self.filter_mode == TreeFilterMode::All {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::All
            };
            self.folded_nodes.clear();
            self.apply_filter();
        } else if matches("app.tree.filter.cycleBackward") {
            let modes = [
                TreeFilterMode::Default,
                TreeFilterMode::NoTools,
                TreeFilterMode::UserOnly,
                TreeFilterMode::LabeledOnly,
                TreeFilterMode::All,
            ];
            let current = modes
                .iter()
                .position(|mode| *mode == self.filter_mode)
                .unwrap_or(0);
            self.filter_mode = modes[(current + modes.len() - 1) % modes.len()];
            self.folded_nodes.clear();
            self.apply_filter();
        } else if matches("app.tree.filter.cycleForward") {
            let modes = [
                TreeFilterMode::Default,
                TreeFilterMode::NoTools,
                TreeFilterMode::UserOnly,
                TreeFilterMode::LabeledOnly,
                TreeFilterMode::All,
            ];
            let current = modes
                .iter()
                .position(|mode| *mode == self.filter_mode)
                .unwrap_or(0);
            self.filter_mode = modes[(current + 1) % modes.len()];
            self.folded_nodes.clear();
            self.apply_filter();
        } else if matches("tui.editor.deleteCharBackward") {
            if !self.search_query.is_empty() {
                self.search_query.pop();
                self.folded_nodes.clear();
                self.apply_filter();
            }
        } else if matches("app.tree.editLabel") {
            // Handled by the container (which owns the label input).
        } else if matches("app.tree.toggleLabelTimestamp") {
            self.show_label_timestamps = !self.show_label_timestamps;
        } else {
            let has_control_chars = data.chars().any(|ch| {
                let code = ch as u32;
                code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
            });
            if !has_control_chars && !data.is_empty() {
                self.search_query.push_str(data);
                self.folded_nodes.clear();
                self.apply_filter();
            }
        }
        TreeSelectorOutcome::Consumed
    }

    fn copy_selected(&self) -> Option<String> {
        self.selected_flat_index()
            .and_then(|index| self.get_entry_copy_text(&self.flat_nodes[index].node))
    }

    fn selected_label(&self) -> Option<(String, Option<String>)> {
        self.selected_flat_index().map(|index| {
            let node = &self.flat_nodes[index].node;
            (node.entry.id().to_string(), node.label.clone())
        })
    }

    fn update_node_label(&mut self, entry_id: &str, label: Option<String>, label_timestamp: u64) {
        for flat_node in &mut self.flat_nodes {
            if flat_node.node.entry.id() == entry_id {
                flat_node.node.label = label.clone();
                flat_node.node.label_timestamp = label.map(|_| label_timestamp);
                break;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchDirection {
    Up,
    Down,
}

impl Component for TreeList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();

        if self.filtered_indices.is_empty() {
            lines.push(truncate_to_width(
                &theme_handle.fg("muted", "  No entries found"),
                width,
                "",
                false,
            ));
            lines.push(truncate_to_width(
                &theme_handle.fg("muted", &format!("  (0/0){}", self.get_status_labels())),
                width,
                "",
                false,
            ));
            return lines;
        }

        let start_index = if self.selected_index >= self.max_visible_lines / 2 {
            (self.selected_index - self.max_visible_lines / 2)
                .min(self.filtered_indices.len().saturating_sub(self.max_visible_lines))
        } else {
            0
        };
        let end_index =
            (start_index + self.max_visible_lines).min(self.filtered_indices.len());

        let mut rendered_rows: Vec<HorizontalViewportRow> = Vec::new();
        for i in start_index..end_index {
            let flat_node = &self.flat_nodes[self.filtered_indices[i]];
            let entry = &flat_node.node.entry;
            let is_selected = i == self.selected_index;

            let cursor = if is_selected {
                theme_handle.fg("accent", "› ")
            } else {
                "  ".to_string()
            };

            let display_indent = if self.multiple_roots {
                flat_node.indent.saturating_sub(1)
            } else {
                flat_node.indent
            };

            let connector = if flat_node.show_connector && !flat_node.is_virtual_root_child {
                if flat_node.is_last {
                    "└─ "
                } else {
                    "├─ "
                }
            } else {
                ""
            };
            let connector_position = if connector.is_empty() {
                None
            } else {
                Some(display_indent.saturating_sub(1))
            };

            let total_chars = display_indent * 3;
            let prefix_chars: Vec<char> = {
                let is_folded = self.folded_nodes.contains(entry.id());
                let mut chars: Vec<char> = Vec::with_capacity(total_chars);
                for i in 0..total_chars {
                    let level = i / 3;
                    let pos_in_level = i % 3;
                    let gutter = flat_node
                        .gutters
                        .iter()
                        .find(|gutter| gutter.position == level);
                    if let Some(gutter) = gutter {
                        if pos_in_level == 0 {
                            chars.push(if gutter.show { '│' } else { ' ' });
                        } else {
                            chars.push(' ');
                        }
                    } else if connector_position == Some(level) {
                        if pos_in_level == 0 {
                            chars.push(if flat_node.is_last { '└' } else { '├' });
                        } else if pos_in_level == 1 {
                            let foldable = self.is_foldable(entry.id());
                            chars.push(if is_folded {
                                '⊞'
                            } else if foldable {
                                '⊟'
                            } else {
                                '─'
                            });
                        } else {
                            chars.push(' ');
                        }
                    } else {
                        chars.push(' ');
                    }
                }
                chars
            };
            let prefix: String = prefix_chars.into_iter().collect();

            let is_folded = self.folded_nodes.contains(entry.id());
            let shows_fold_in_connector =
                flat_node.show_connector && !flat_node.is_virtual_root_child;
            let fold_marker = if is_folded && !shows_fold_in_connector {
                theme_handle.fg("accent", "⊞ ")
            } else {
                String::new()
            };

            let is_on_active_path = self.active_path_ids.contains(entry.id());
            let path_marker = if is_on_active_path {
                theme_handle.fg("accent", "• ")
            } else {
                String::new()
            };

            let label = match &flat_node.node.label {
                Some(label) => theme_handle.fg("warning", &format!("[{label}] ")),
                None => String::new(),
            };
            let label_timestamp =
                if self.show_label_timestamps && flat_node.node.label.is_some() {
                    match flat_node.node.label_timestamp {
                        Some(timestamp) => theme_handle
                            .fg("muted", &format!("{} ", format_label_timestamp(timestamp))),
                        None => String::new(),
                    }
                } else {
                    String::new()
                };

            let content = self.get_entry_display_text(&flat_node.node, is_selected);
            let prefix_part = format!("{}{}{}", theme_handle.fg("dim", &prefix), fold_marker, path_marker);
            let anchor_col = visible_width(&prefix_part);
            let mut gutter = cursor;
            let mut body = format!("{prefix_part}{label}{label_timestamp}{content}");
            if is_selected {
                gutter = theme_handle.bg("selectedBg", &gutter);
                body = theme_handle.bg("selectedBg", &body);
            }
            let body_width = visible_width(&body);
            rendered_rows.push(HorizontalViewportRow {
                gutter,
                body,
                anchor_col,
                body_width,
                is_selected,
            });
        }

        lines.extend(render_horizontal_viewport(&rendered_rows, width));
        lines.push(truncate_to_width(
            &theme_handle.fg(
                "muted",
                &format!(
                    "  ({}/{}){}",
                    self.selected_index + 1,
                    self.filtered_indices.len(),
                    self.get_status_labels()
                ),
            ),
            width,
            "",
            false,
        ));

        lines
    }
}

// --- label input -----------------------------------------------------------

/// Label input component shown when editing a label (upstream `LabelInput`).
pub struct LabelInput {
    input: Input,
    entry_id: String,
    focused: bool,
}

impl LabelInput {
    pub fn new(entry_id: &str, current_label: Option<&str>) -> Self {
        let mut input = Input::new();
        if let Some(current_label) = current_label {
            input.set_value(current_label);
        }
        Self {
            input,
            entry_id: entry_id.to_string(),
            focused: false,
        }
    }

    /// Host-driven key handling (upstream `LabelInput.handleInput`).
    /// `Some((entry_id, label))` on submit, `Some(None)`… see
    /// [`LabelInputOutcome`].
    pub fn handle_key(&mut self, data: &str) -> LabelInputOutcome {
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));
        if matches("tui.select.confirm") {
            let value = self.input.get_value().trim().to_string();
            return LabelInputOutcome::Submit {
                entry_id: self.entry_id.clone(),
                label: (!value.is_empty()).then_some(value),
            };
        }
        if matches("tui.select.cancel") {
            return LabelInputOutcome::Cancel;
        }
        dispatch_input_keybinding(&mut self.input, data);
        self.input.handle_input(data);
        LabelInputOutcome::Consumed
    }

    pub fn render(&self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let indent = "  ";
        let available_width = width.saturating_sub(indent.len());
        let mut lines = Vec::new();
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}",
                theme_handle.fg("muted", "Label (empty to remove):")
            ),
            width,
            "",
            false,
        ));
        for line in self.input.render(available_width) {
            lines.push(truncate_to_width(
                &format!("{indent}{line}"),
                width,
                "",
                false,
            ));
        }
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}  {}",
                key_hint("tui.select.confirm", "save"),
                key_hint("tui.select.cancel", "cancel")
            ),
            width,
            "",
            false,
        ));
        lines
    }
}

/// What the label input did with a key.
#[derive(Debug, Clone, PartialEq)]
pub enum LabelInputOutcome {
    Consumed,
    Submit {
        entry_id: String,
        label: Option<String>,
    },
    Cancel,
}

impl Focusable for LabelInput {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

// --- tree help -------------------------------------------------------------

/// Upstream `TREE_HELP_ITEMS` (keybinding lists plus labels).
const TREE_HELP_ITEMS: &[(&[&str], &str, bool)] = &[
    (&["tui.select.up", "tui.select.down"], "move", false),
    (
        &["tui.editor.cursorLeft", "tui.editor.cursorRight"],
        "page",
        false,
    ),
    (
        &["app.tree.foldOrUp", "app.tree.unfoldOrDown"],
        "branch",
        false,
    ),
    (&["app.message.copy"], "copy", false),
    (&["app.tree.editLabel"], "label", false),
    (&["app.tree.toggleLabelTimestamp"], "label time", false),
    (
        &[
            "app.tree.filter.default",
            "app.tree.filter.noTools",
            "app.tree.filter.userOnly",
            "app.tree.filter.labeledOnly",
            "app.tree.filter.all",
        ],
        "filters",
        true,
    ),
    (
        &[
            "app.tree.filter.cycleForward",
            "app.tree.filter.cycleBackward",
        ],
        "cycle",
        true,
    ),
];

/// Upstream `compactRawKeys`.
fn compact_raw_keys(keys: &[String]) -> String {
    if keys.len() == 1 {
        return keys[0].clone();
    }
    let parts: Vec<(&str, &str)> = keys
        .iter()
        .map(|key| match key.rfind('+') {
            Some(index) => (&key[..index + 1], &key[index + 1..]),
            None => ("", key.as_str()),
        })
        .collect();
    let prefix = parts[0].0;
    if !prefix.is_empty() && parts.iter().all(|part| part.0 == prefix) {
        format!(
            "{prefix}{}",
            parts
                .iter()
                .map(|part| part.1)
                .collect::<Vec<_>>()
                .join("/")
        )
    } else {
        keys.join("/")
    }
}

/// Upstream `formatHelpKeys`.
fn format_help_keys(keybindings: &[&str]) -> String {
    let mut keys: Vec<String> = Vec::new();
    with_global_keybindings(|kb| {
        for keybinding in keybindings {
            if let Some(key) = kb.get_keys(keybinding).first() {
                keys.push(key.clone());
            }
        }
    });
    if keys.is_empty() {
        return String::new();
    }
    format_key_text(&compact_raw_keys(&keys), KeyTextFormatOptions::default())
        .replace("pageUp", "pgup")
        .replace("pageDown", "pgdn")
        .replace("up", "↑")
        .replace("down", "↓")
        .replace("left", "←")
        .replace("right", "→")
}

/// Upstream `TreeHelp` (semantic rows with chunk-aware wrapping).
fn render_tree_help(width: usize) -> Vec<String> {
    let theme_handle = theme();
    let items: Vec<String> = TREE_HELP_ITEMS
        .iter()
        .map(|(keys, label, label_first)| {
            let text = format_help_keys(keys);
            if text.is_empty() {
                (*label).to_string()
            } else if *label_first {
                format!("{label} {text}")
            } else {
                format!("{text} {label}")
            }
        })
        .collect();

    let available_width = width.max(1);
    let indent = "  ";
    let separator = " · ";
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();

    for item in items {
        let candidate = if !current_line.is_empty() {
            format!("{current_line}{separator}{item}")
        } else if visible_width(&format!("{indent}{item}")) <= available_width {
            format!("{indent}{item}")
        } else {
            item.clone()
        };
        if current_line.is_empty() || visible_width(&candidate) <= available_width {
            current_line = candidate;
            continue;
        }

        lines.extend(wrap_text_with_ansi(
            current_line.trim_end(),
            available_width,
        ));
        current_line = if visible_width(&format!("{indent}{item}")) <= available_width {
            format!("{indent}{item}")
        } else {
            item
        };
    }

    if !current_line.is_empty() {
        lines.extend(wrap_text_with_ansi(
            current_line.trim_end(),
            available_width,
        ));
    }

    lines
        .iter()
        .map(|line| theme_handle.fg("muted", line))
        .collect()
}

// --- selector --------------------------------------------------------------

/// A session tree selector for navigation (upstream
/// `TreeSelectorComponent`).
pub struct TreeSelectorComponent {
    tree_list: TreeList,
    label_input: Option<LabelInput>,
    focused: bool,
}

impl TreeSelectorComponent {
    /// Upstream the constructor (the empty-tree 100 ms auto-cancel lives in
    /// the mode; see the module divergence note).
    pub fn new(
        tree: &[SessionTreeNode],
        current_leaf_id: Option<String>,
        terminal_height: usize,
        initial_selected_id: Option<&str>,
        initial_filter_mode: TreeFilterMode,
    ) -> Self {
        let max_visible_lines = (terminal_height / 2).max(5);
        Self {
            tree_list: TreeList::new(
                tree,
                current_leaf_id,
                max_visible_lines,
                initial_selected_id,
                initial_filter_mode,
            ),
            label_input: None,
            focused: false,
        }
    }

    /// The search box contents (upstream `getSearchQuery`).
    pub fn search_query(&self) -> &str {
        &self.tree_list.search_query
    }

    /// Host-driven key handling (upstream `TreeSelectorComponent.handleInput`).
    pub fn handle_key(&mut self, data: &str) -> TreeSelectorOutcome {
        if let Some(label_input) = &mut self.label_input {
            return match label_input.handle_key(data) {
                LabelInputOutcome::Consumed => TreeSelectorOutcome::Consumed,
                LabelInputOutcome::Submit { entry_id, label } => {
                    let timestamp = pillar_ai::models::now_ms();
                    self.tree_list
                        .update_node_label(&entry_id, label.clone(), timestamp);
                    self.label_input = None;
                    TreeSelectorOutcome::LabelChanged { entry_id, label }
                }
                LabelInputOutcome::Cancel => {
                    self.label_input = None;
                    TreeSelectorOutcome::Consumed
                }
            };
        }

        // `app.tree.editLabel` opens the label input (upstream
        // `onLabelEdit` → `showLabelInput`).
        let matches = |keybinding: &str| with_global_keybindings(|kb| kb.matches(data, keybinding));
        if matches("app.tree.editLabel") {
            if let Some((entry_id, current_label)) = self.tree_list.selected_label() {
                let mut label_input = LabelInput::new(&entry_id, current_label.as_deref());
                label_input.set_focused(self.focused);
                self.label_input = Some(label_input);
            }
            return TreeSelectorOutcome::Consumed;
        }

        self.tree_list.handle_key(data)
    }

    /// Whether the label input currently owns the keys (upstream the
    /// `labelInput !== null` branch).
    pub fn is_editing_label(&self) -> bool {
        self.label_input.is_some()
    }
}

impl Component for TreeSelectorComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();

        lines.push(String::new());
        lines.extend(DynamicBorder::new().render(width));
        lines.extend(
            pillar_tui::components::Text::new(&theme_handle.bold("  Session Tree"), 1, 0)
                .render(width),
        );
        lines.extend(render_tree_help(width));

        // Search line.
        let query = self.tree_list.search_query.clone();
        let search_line = if query.is_empty() {
            format!("  {}", theme_handle.fg("muted", "Type to search:"))
        } else {
            format!(
                "  {} {}",
                theme_handle.fg("muted", "Type to search:"),
                theme_handle.fg("accent", &query)
            )
        };
        lines.push(truncate_to_width(&search_line, width, "", false));

        lines.extend(DynamicBorder::new().render(width));
        lines.push(String::new());

        // Tree list or label input.
        if let Some(label_input) = &self.label_input {
            lines.extend(label_input.render(width));
        } else {
            lines.extend(self.tree_list.render(width));
        }

        lines.push(String::new());
        lines.extend(DynamicBorder::new().render(width));
        lines
    }
}

impl Focusable for TreeSelectorComponent {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(label_input) = &mut self.label_input {
            label_input.set_focused(focused);
        }
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::messages::BashExecutionMessage;
    use crate::core::session_entries::{SessionEntryBase, SessionMessageEntry};
    use pillar_ai::types::{AssistantMessage, StopReason, Usage, UsageCost};

    /// These tests match the app-level `app.tree.*` / `app.message.copy`
    /// keybindings and render with a theme, so the merged keybinding table
    /// and a theme must be installed.
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::modes::interactive::components::test_support::setup()
    }

    fn base(id: &str, parent: Option<&str>) -> SessionEntryBase {
        SessionEntryBase {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            timestamp: 1,
        }
    }

    fn user_entry(id: &str, parent: Option<&str>, text: &str) -> SessionTreeNode {
        SessionTreeNode {
            entry: SessionEntry::Message(SessionMessageEntry {
                base: base(id, parent),
                message: CodingAgentMessage::Base(Message::User {
                    content: UserContent::Text(text.to_string()),
                    timestamp: 1,
                }),
            }),
            children: Vec::new(),
            label: None,
            label_timestamp: None,
        }
    }

    fn bash_entry(id: &str, parent: Option<&str>, command: &str) -> SessionTreeNode {
        SessionTreeNode {
            entry: SessionEntry::Message(SessionMessageEntry {
                base: base(id, parent),
                message: CodingAgentMessage::BashExecution(BashExecutionMessage {
                    command: command.to_string(),
                    output: String::new(),
                    exit_code: None,
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                    timestamp: 1,
                    exclude_from_context: false,
                }),
            }),
            children: Vec::new(),
            label: None,
            label_timestamp: None,
        }
    }

    fn assistant_entry(
        id: &str,
        parent: Option<&str>,
        text: Option<&str>,
        tool_calls: &[(&str, &str)],
    ) -> SessionTreeNode {
        let mut content: Vec<Content> = Vec::new();
        if let Some(text) = text {
            content.push(Content::text(text));
        }
        for (call_id, name) in tool_calls {
            content.push(Content::ToolCall {
                id: (*call_id).to_string(),
                name: (*name).to_string(),
                arguments: serde_json::json!({ "path": "/x" }),
                thought_signature: None,
                namespace: None,
            });
        }
        SessionTreeNode {
            entry: SessionEntry::Message(SessionMessageEntry {
                base: base(id, parent),
                message: CodingAgentMessage::Base(Message::Assistant(Box::new(AssistantMessage {
                    content,
                    api: "anthropic-messages".to_string(),
                    provider: "anthropic".to_string(),
                    model: "m".to_string(),
                    response_model: None,
                    response_id: None,
                    diagnostics: Vec::new(),
                    usage: Usage {
                        cost: UsageCost::default(),
                        ..Default::default()
                    },
                    stop_reason: if tool_calls.is_empty() {
                        StopReason::Stop
                    } else {
                        StopReason::ToolUse
                    },
                    deferred: None,
                    error_message: None,
                    raw_stop_reason: None,
                    end_turn: None,
                    timestamp: 1,
                }))),
            }),
            children: Vec::new(),
            label: None,
            label_timestamp: None,
        }
    }

    /// q1 / a1 / q2 / a2 chain.
    fn linear_tree() -> Vec<SessionTreeNode> {
        let a2 = assistant_entry("a2", Some("q2"), Some("a2 reply"), &[]);
        let q2 = SessionTreeNode {
            children: vec![a2],
            ..user_entry("q2", Some("a1"), "q2 question")
        };
        let a1 = SessionTreeNode {
            children: vec![q2],
            ..assistant_entry("a1", Some("q1"), Some("a1 reply"), &[])
        };
        let q1 = SessionTreeNode {
            children: vec![a1],
            ..user_entry("q1", None, "q1 question")
        };
        vec![q1]
    }

    fn plain(selector: &mut TreeSelectorComponent) -> String {
        let lines = selector.render(80);
        lines.join("\n")
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    #[test]
    fn renders_the_tree_with_the_current_leaf_and_footer() {
        let _guard = setup();
        let mut selector = TreeSelectorComponent::new(
            &linear_tree(),
            Some("a2".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(rendered.contains("Session Tree"), "{rendered}");
        assert!(rendered.contains("Type to search:"), "{rendered}");
        assert!(rendered.contains("q1 question"), "{rendered}");
        // The constructor selects the current leaf (the last entry).
        assert!(rendered.contains("(4/4)"), "{rendered}");
        // The active path is marked with a bullet.
        assert!(rendered.contains("• user: q1 question"), "{rendered}");
        assert!(rendered.contains("› • assistant: a2 reply"), "{rendered}");
    }

    #[test]
    fn up_down_wrap_and_enter_reports_the_selection() {
        let _guard = setup();
        let mut selector = TreeSelectorComponent::new(
            &linear_tree(),
            Some("a2".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        // The constructor selects the current leaf (the last entry).
        assert_eq!(
            selector.handle_key("\x1b[A"),
            TreeSelectorOutcome::Consumed
        );
        // Up moved to q2 → Enter reports it.
        let outcome = selector.handle_key("\r");
        assert_eq!(outcome, TreeSelectorOutcome::Select("q2".to_string()));
    }

    #[test]
    fn search_filters_and_escape_clears_it_first() {
        let _guard = setup();
        let mut selector = TreeSelectorComponent::new(
            &linear_tree(),
            Some("a2".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        for ch in "q1".chars() {
            selector.handle_key(&ch.to_string());
        }
        assert_eq!(selector.search_query(), "q1");
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(rendered.contains("q1 question"), "{rendered}");
        assert!(!rendered.contains("q2 question"), "{rendered}");

        // The first cancel clears the query, the second cancels.
        assert_eq!(
            selector.handle_key("\x1b"),
            TreeSelectorOutcome::Consumed
        );
        assert_eq!(selector.search_query(), "");
        assert_eq!(selector.handle_key("\x1b"), TreeSelectorOutcome::Cancel);
    }

    #[test]
    fn copy_reports_the_selected_entry_text() {
        let _guard = setup();
        let mut selector = TreeSelectorComponent::new(
            &linear_tree(),
            Some("a2".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        // Select the assistant entry (a1) and copy it.
        selector.handle_key("\x1b[A");
        selector.handle_key("\x1b[A");
        assert_eq!(
            selector.handle_key("\x18"),
            TreeSelectorOutcome::Copy(Some("a1 reply".to_string()))
        );
    }

    #[test]
    fn label_editing_updates_the_node_and_reports_the_change() {
        let _guard = setup();
        let mut selector = TreeSelectorComponent::new(
            &linear_tree(),
            Some("a2".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        selector.handle_key("\x1b[A");
        // shift+l opens the label input.
        selector.handle_key("L");
        assert!(selector.is_editing_label());
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(rendered.contains("Label (empty to remove):"), "{rendered}");
        for ch in "bookmark".chars() {
            selector.handle_key(&ch.to_string());
        }
        assert_eq!(
            selector.handle_key("\r"),
            TreeSelectorOutcome::LabelChanged {
                entry_id: "q2".to_string(),
                label: Some("bookmark".to_string()),
            }
        );
        assert!(!selector.is_editing_label());
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(rendered.contains("[bookmark]"), "{rendered}");
    }

    #[test]
    fn filter_modes_toggle_and_hide_settings_entries() {
        let _guard = setup();
        let mut tree = linear_tree();
        // A label entry is hidden in the default view.
        let label = SessionTreeNode {
            entry: SessionEntry::Label(crate::core::session_entries::LabelEntry {
                base: base("lbl", Some("a2")),
                target_id: "q1".to_string(),
                label: Some("tag".to_string()),
            }),
            children: Vec::new(),
            label: None,
            label_timestamp: None,
        };
        tree[0].children[0].children[0].children.push(label);

        let mut selector = TreeSelectorComponent::new(
            &tree,
            Some("a2".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(!rendered.contains("(lbl)"), "{rendered}");

        // Ctrl+A → all: the label entry shows.
        selector.handle_key("\x01");
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(rendered.contains("[label: tag]") || rendered.contains("[all]"), "{rendered}");
        assert!(rendered.contains("[all]"), "{rendered}");

        // Ctrl+U → user-only: only user messages.
        selector.handle_key("\x15");
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(rendered.contains("[user]"), "{rendered}");
        assert!(!rendered.contains("assistant:"), "{rendered}");
    }

    #[test]
    fn tool_only_assistant_messages_and_tool_results_render_compactly() {
        let _guard = setup();
        let tool_result = SessionTreeNode {
            entry: SessionEntry::Message(SessionMessageEntry {
                base: base("r1", Some("a1")),
                message: CodingAgentMessage::Base(Message::ToolResult(Box::new(
                    pillar_ai::types::ToolResultMessage {
                        tool_call_id: "call-1".to_string(),
                        tool_name: "read".to_string(),
                        content: vec![Content::text("file")],
                        details: None,
                        usage: None,
                        added_tool_names: None,
                        is_error: false,
                        timestamp: 1,
                    },
                ))),
            }),
            children: Vec::new(),
            label: None,
            label_timestamp: None,
        };
        let assistant = SessionTreeNode {
            children: vec![tool_result],
            ..assistant_entry("a1", Some("q1"), None, &[("call-1", "read")])
        };
        let bash = bash_entry("b1", Some("q1"), "echo hi");
        let q1 = SessionTreeNode {
            children: vec![assistant, bash],
            ..user_entry("q1", None, "question")
        };
        let tree = vec![q1];

        // The tool-only assistant message is hidden (not the current leaf)
        // and the tool result renders compactly from the tool call map.
        let mut selector = TreeSelectorComponent::new(
            &tree,
            Some("b1".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(!rendered.contains("assistant:"), "{rendered}");
        assert!(rendered.contains("[read: /x]"), "{rendered}");
        assert!(rendered.contains("[bash]: echo hi"), "{rendered}");

        // As the current leaf the tool-only assistant message stays visible.
        let mut selector = TreeSelectorComponent::new(
            &tree,
            Some("a1".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(rendered.contains("assistant: (no content)"), "{rendered}");
    }

    #[test]
    fn fold_hides_descendants() {
        let _guard = setup();
        let mut selector = TreeSelectorComponent::new(
            &linear_tree(),
            Some("a2".to_string()),
            24,
            None,
            TreeFilterMode::Default,
        );
        // Select q1 (root, foldable) and fold it (alt+left on macOS).
        selector.handle_key("\x1b[A");
        selector.handle_key("\x1b[A");
        selector.handle_key("\x1b[A");
        selector.handle_key("\x1b[1;3D");
        let rendered = strip_ansi(&plain(&mut selector));
        assert!(rendered.contains("(1/1)"), "{rendered}");
        assert!(rendered.contains('⊞'), "{rendered}");
    }
}
