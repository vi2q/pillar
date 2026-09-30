use std::sync::atomic::Ordering;

use super::*;
use crate::tool_dispatch::{ToolCallOutcome, ToolExposure, ToolRegistration, ToolRequest};

impl Agent {
    /// Register a tool independently of its model declaration. Hidden tools are withdrawn.
    pub fn register_tool(&self, registration: ToolRegistration) {
        let name = registration.tool.name().to_owned();
        {
            let mut registrations = self.registrations.lock().unwrap_or_else(|p| p.into_inner());
            registrations.retain(|entry| entry.tool.name() != name);
            registrations.push(registration.clone());
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.tools.retain(|tool| tool.name() != name);
        if matches!(
            registration.exposure,
            ToolExposure::Direct | ToolExposure::ModelOnly
        ) {
            state.tools.push(registration.tool);
        }
    }

    /// Current executable catalog, including tools omitted from model declarations.
    pub fn tool_catalog(&self) -> Vec<ToolRegistration> {
        let active = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .tools
            .clone();
        let mut catalog = self
            .registrations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        for tool in active {
            if let Some(entry) = catalog
                .iter_mut()
                .find(|entry| entry.tool.name() == tool.name())
            {
                entry.tool = tool;
            } else {
                catalog.push(ToolRegistration::direct(tool));
            }
        }
        catalog
    }

    /// Declare discovered tools for subsequent model requests. Hidden tools cannot be activated.
    pub fn activate_tools(&self, names: &[String]) -> Vec<String> {
        let catalog = self.tool_catalog();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut added = Vec::new();
        for name in names {
            if state.tools.iter().any(|tool| tool.name() == name) {
                continue;
            }
            if let Some(entry) = catalog.iter().find(|entry| {
                entry.tool.name() == name
                    && matches!(
                        entry.exposure,
                        ToolExposure::Direct | ToolExposure::Deferred
                    )
            }) {
                state.tools.push(entry.tool.clone());
                added.push(name.clone());
            }
        }
        added
    }

    /// Execute tools owned by an assistant-issued parent call, without adding transcript entries.
    /// Validation and interception are shared with model-issued calls. Execution progress carries
    /// the parent identity through the provided sink; the caller owns bounded audit storage.
    pub async fn execute_nested_tools(
        &self,
        parent_id: &str,
        requests: Vec<ToolRequest>,
        signal: Option<AbortSignal>,
        emit: AgentEventSink,
    ) -> Result<Vec<ToolCallOutcome>, crate::ToolExecuteError> {
        let (assistant, parent_context) =
            self.running_tool_calls.get(parent_id).ok_or_else(|| {
                crate::ToolExecuteError("Parent tool call is no longer running".into())
            })?;
        let catalog = self.tool_catalog();
        let callable: Vec<_> = catalog
            .iter()
            .filter(|entry| match entry.exposure {
                ToolExposure::Direct => parent_context
                    .tools
                    .iter()
                    .any(|tool| tool.name() == entry.tool.name()),
                ToolExposure::Codemode | ToolExposure::Deferred => true,
                ToolExposure::ModelOnly | ToolExposure::Hidden => false,
            })
            .map(|entry| entry.tool.clone())
            .collect();
        let context = AgentContext {
            system_prompt: parent_context.system_prompt,
            messages: parent_context.messages,
            tools: callable,
        };
        let config = self.create_loop_config(false);
        let calls: Vec<_> = requests
            .into_iter()
            .map(|request| crate::AgentToolCall {
                id: format!(
                    "{parent_id}/{}",
                    self.nested_sequence.fetch_add(1, Ordering::Relaxed)
                ),
                name: request.name,
                arguments: request.arguments,
            })
            .collect();
        let sequential = self.tool_execution == ToolExecutionMode::Sequential
            || calls.iter().any(|call| {
                context.tools.iter().any(|tool| {
                    tool.name() == call.name
                        && tool.execution_mode == Some(ToolExecutionMode::Sequential)
                })
            });
        let run = |call: crate::AgentToolCall| {
            let context = &context;
            let assistant = &assistant;
            let config = &config;
            let signal = signal.clone();
            let emit = &emit;
            async move {
                crate::agent_loop::run_tool_call(
                    context,
                    assistant,
                    &call,
                    config,
                    signal,
                    emit,
                    Some(parent_id),
                )
                .await
            }
        };
        if sequential {
            let mut outcomes = Vec::new();
            for call in calls {
                let outcome = run(call).await;
                let stop = outcome.result.terminate
                    || signal.as_ref().is_some_and(AbortSignal::is_aborted);
                outcomes.push(outcome);
                if stop {
                    break;
                }
            }
            Ok(outcomes)
        } else {
            Ok(crate::agent_loop::run_tool_calls_parallel(
                &context,
                &assistant,
                calls,
                &config,
                signal,
                &emit,
                Some(parent_id),
            )
            .await)
        }
    }
}
