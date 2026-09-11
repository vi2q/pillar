//! Port of packages/coding-agent/src/core/tools/index.ts (pi v0.84.3):
//! the built-in tool registry and the coding / read-only / all tool sets.
//!
//! divergence: `powershell` is Windows-only upstream and is not ported, so
//! [`create_tool`] returns `None` for it and [`create_all_tools`] omits it
//! (upstream throws on unknown names, the port returns an `Option`).

use std::collections::BTreeMap;

use pillar_agent::types::AgentTool;

use crate::core::tools::bash::bash_tool;
use crate::core::tools::edit::edit_tool;
use crate::core::tools::ls::ls_tool;
use crate::core::tools::read::read_tool;
use crate::core::tools::search::{find_tool, grep_tool};
use crate::core::tools::write::write_tool;

/// The built-in tool names (upstream `ToolName`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolName {
    Read,
    Bash,
    Powershell,
    Edit,
    Write,
    Grep,
    Find,
    Ls,
}

impl ToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Bash => "bash",
            Self::Powershell => "powershell",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Grep => "grep",
            Self::Find => "find",
            Self::Ls => "ls",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "read" => Self::Read,
            "bash" => Self::Bash,
            "powershell" => Self::Powershell,
            "edit" => Self::Edit,
            "write" => Self::Write,
            "grep" => Self::Grep,
            "find" => Self::Find,
            "ls" => Self::Ls,
            _ => return None,
        })
    }
}

/// Every built-in tool name (upstream `allToolNames`).
pub const ALL_TOOL_NAMES: [ToolName; 8] = [
    ToolName::Read,
    ToolName::Bash,
    ToolName::Powershell,
    ToolName::Edit,
    ToolName::Write,
    ToolName::Grep,
    ToolName::Find,
    ToolName::Ls,
];

/// Create one built-in tool (upstream `createTool`). `None` for names that
/// have no ported implementation.
pub fn create_tool(tool_name: &str, cwd: &str) -> Option<AgentTool> {
    Some(match ToolName::parse(tool_name)? {
        ToolName::Read => read_tool(cwd),
        ToolName::Bash => bash_tool(cwd),
        ToolName::Powershell => return None,
        ToolName::Edit => edit_tool(cwd),
        ToolName::Write => write_tool(cwd),
        ToolName::Grep => grep_tool(cwd),
        ToolName::Find => find_tool(cwd),
        ToolName::Ls => ls_tool(cwd),
    })
}

/// The default coding tools: read, bash, edit, write (upstream
/// `createCodingTools`).
pub fn create_coding_tools(cwd: &str) -> Vec<AgentTool> {
    vec![
        read_tool(cwd),
        bash_tool(cwd),
        edit_tool(cwd),
        write_tool(cwd),
    ]
}

/// The read-only tools: read, grep, find, ls (upstream
/// `createReadOnlyTools`).
pub fn create_read_only_tools(cwd: &str) -> Vec<AgentTool> {
    vec![read_tool(cwd), grep_tool(cwd), find_tool(cwd), ls_tool(cwd)]
}

/// Every ported built-in tool, keyed by name (upstream `createAllTools`,
/// minus the unported `powershell`).
pub fn create_all_tools(cwd: &str) -> BTreeMap<String, AgentTool> {
    let mut tools = BTreeMap::new();
    for name in ALL_TOOL_NAMES {
        if let Some(tool) = create_tool(name.as_str(), cwd) {
            tools.insert(name.as_str().to_string(), tool);
        }
    }
    tools
}
