//! Port of packages/ai/src/utils/deferred-tools.ts (pi v0.84.3).
//!
//! Splits current tools into prefix (immediate) definitions and
//! transcript-loaded (deferred) ones.

use std::collections::{BTreeMap, HashSet};

use crate::types::{Context, Tool};

pub struct SplitDeferredTools {
    pub immediate: Vec<Tool>,
    /// Deferred tools keyed by (normalized) name.
    pub deferred: BTreeMap<String, Tool>,
}

/// Split current tools into prefix and transcript-loaded definitions.
pub fn split_deferred_tools(context: &Context, enabled: bool) -> SplitDeferredTools {
    split_deferred_tools_with(context, enabled, |name| name.to_string())
}

/// Upstream also supports a tool-name normalizer; callers currently use the
/// identity normalization.
pub fn split_deferred_tools_with(
    context: &Context,
    enabled: bool,
    normalize_name: impl Fn(&str) -> String,
) -> SplitDeferredTools {
    let mut unique_tools: BTreeMap<String, Tool> = BTreeMap::new();
    for tool in &context.tools {
        unique_tools.insert(normalize_name(&tool.name), tool.clone());
    }
    if !enabled {
        return SplitDeferredTools {
            immediate: unique_tools.into_values().collect(),
            deferred: BTreeMap::new(),
        };
    }

    let mut deferred_names: HashSet<String> = HashSet::new();
    let mut used_names: HashSet<String> = HashSet::new();
    for message in &context.messages {
        match message {
            crate::types::Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let crate::types::Content::ToolCall { name, .. } = block {
                        used_names.insert(normalize_name(name));
                    }
                }
            }
            crate::types::Message::ToolResult(tool_result) => {
                for name in tool_result.added_tool_names.iter().flatten() {
                    let normalized = normalize_name(name);
                    if !used_names.contains(&normalized) {
                        deferred_names.insert(normalized);
                    }
                }
            }
            _ => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = BTreeMap::new();
    for (name, tool) in unique_tools {
        if deferred_names.contains(&name) {
            deferred.insert(name, tool);
        } else {
            immediate.push(tool);
        }
    }
    SplitDeferredTools {
        immediate,
        deferred,
    }
}
