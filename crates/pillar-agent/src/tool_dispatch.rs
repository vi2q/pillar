use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AgentTool, AgentToolCall, AgentToolResult};

/// Execution-owned parent contexts, independent of delayed event consumers.
#[derive(Clone, Debug, Default)]
pub struct RunningToolCalls(
    std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                (pillar_ai::types::AssistantMessage, crate::AgentContext),
            >,
        >,
    >,
);

impl RunningToolCalls {
    pub(crate) fn enter(
        &self,
        id: &str,
        assistant: &pillar_ai::types::AssistantMessage,
        context: &crate::AgentContext,
    ) -> RunningToolCallGuard {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_owned(), (assistant.clone(), context.clone()));
        RunningToolCallGuard {
            calls: self.clone(),
            id: id.to_owned(),
        }
    }

    pub(crate) fn get(
        &self,
        id: &str,
    ) -> Option<(pillar_ai::types::AssistantMessage, crate::AgentContext)> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }
}

pub(crate) struct RunningToolCallGuard {
    calls: RunningToolCalls,
    id: String,
}

impl Drop for RunningToolCallGuard {
    fn drop(&mut self) {
        self.calls
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

/// Reachability of a registered tool; visibility does not authorize its effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolExposure {
    /// Declared to the model when active, and callable by other tools when active.
    Direct,
    /// Declared to the model, but unavailable to other tools.
    ModelOnly,
    /// Callable by other tools without adding a model declaration.
    Codemode,
    /// Callable by other tools, and declarable through discovery.
    Deferred,
    /// Unreachable through either execution route.
    Hidden,
}

/// Registration metadata independent of the model's active tool declarations.
#[derive(Clone, Debug)]
pub struct ToolRegistration {
    /// Executable definition; every invocation still uses the agent pipeline.
    pub tool: AgentTool,
    /// Routes permitted to reach this tool.
    pub exposure: ToolExposure,
    /// Optional grouping used by discovery.
    pub namespace: Option<String>,
    /// Optional schema declaration for programmatic data; the producer owns conformance.
    pub output_schema: Option<Value>,
}

impl ToolRegistration {
    /// A tool declared directly to the model.
    pub fn direct(tool: AgentTool) -> Self {
        Self {
            tool,
            exposure: ToolExposure::Direct,
            namespace: None,
            output_schema: None,
        }
    }
}

/// A tool invocation requested by an orchestrating tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRequest {
    /// Registered tool name.
    pub name: String,
    /// Arguments validated by the normal execution pipeline.
    pub arguments: Value,
}

/// A completed invocation, including failures that carry programmatic data.
#[derive(Clone, Debug)]
pub struct ToolCallOutcome {
    /// Identity assigned by the executor.
    pub tool_call: AgentToolCall,
    /// Content, structured data, and usage after result interception.
    pub result: AgentToolResult,
    /// Final status after result interception.
    pub is_error: bool,
}
