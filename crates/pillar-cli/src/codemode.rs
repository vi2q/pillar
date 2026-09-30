use std::sync::Arc;

use pillar_agent::tool_dispatch::{ToolExposure, ToolRegistration};
use pillar_agent::{AgentTool, ToolExecuteError};
use pillar_coding_agent::core::agent_session_class::AgentSession;
use pillar_extensions::codemode::{CodeModeHost, create_tools};

use crate::runner::{SessionSlot, resolve_session};

/// Create Code mode tools before the session exists. Binding the weak slot enables execution.
pub fn tools_for_slot(slot: &SessionSlot) -> Vec<AgentTool> {
    let catalog_slot = slot.clone();
    let execute_slot = slot.clone();
    let activate_slot = slot.clone();
    create_tools(CodeModeHost {
        catalog: Arc::new(move || {
            let Some(session) = resolve_session(&catalog_slot) else {
                return Vec::new();
            };
            let active = session.state().tools;
            session
                .agent()
                .tool_catalog()
                .into_iter()
                .filter(|entry| {
                    let name = entry.tool.name();
                    let allowed = session
                        .allowed_tool_names()
                        .is_none_or(|names| names.contains(name));
                    let excluded = session
                        .excluded_tool_names()
                        .is_some_and(|names| names.contains(name));
                    allowed
                        && !excluded
                        && match entry.exposure {
                            ToolExposure::Direct => active.iter().any(|tool| tool.name() == name),
                            ToolExposure::Codemode | ToolExposure::Deferred => true,
                            ToolExposure::ModelOnly | ToolExposure::Hidden => false,
                        }
                })
                .collect()
        }),
        execute: Arc::new(move |parent, requests, signal, sink| {
            let session = resolve_session(&execute_slot);
            Box::pin(async move {
                let session = session
                    .ok_or_else(|| ToolExecuteError("Code mode session is unavailable".into()))?;
                if requests.iter().any(|request| {
                    session
                        .allowed_tool_names()
                        .is_some_and(|names| !names.contains(&request.name))
                        || session
                            .excluded_tool_names()
                            .is_some_and(|names| names.contains(&request.name))
                }) {
                    return Err(ToolExecuteError(
                        "Code mode tool is excluded by the host".into(),
                    ));
                }
                session
                    .agent()
                    .execute_nested_tools(&parent, requests, Some(signal), sink)
                    .await
            })
        }),
        activate: Arc::new(move |names| {
            let Some(session) = resolve_session(&activate_slot) else {
                return Vec::new();
            };
            let names: Vec<_> = names
                .iter()
                .filter(|name| {
                    session
                        .allowed_tool_names()
                        .is_none_or(|allowed| allowed.contains(*name))
                        && session
                            .excluded_tool_names()
                            .is_none_or(|excluded| !excluded.contains(*name))
                })
                .cloned()
                .collect();
            session.agent().activate_tools(&names)
        }),
        spawn_vm: Arc::new(|body| {
            std::thread::Builder::new()
                .name("pillar-codemode".into())
                .spawn(body)
                .map(|_| ())
                .map_err(|error| error.to_string())
        }),
    })
}

/// Mark the built-in orchestrators as model-only, preventing script recursion.
pub fn bind_tools(session: &AgentSession) {
    for tool in session.state().tools {
        if (tool.name() == "codemode" && tool.label == "Luau code mode")
            || (tool.name() == "tool_search" && tool.label == "Tool search")
        {
            let mut registration = ToolRegistration::direct(tool);
            registration.exposure = ToolExposure::ModelOnly;
            session.agent().register_tool(registration);
        }
    }
}
