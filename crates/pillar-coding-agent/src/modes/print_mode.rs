//! Port of packages/coding-agent/src/modes/print-mode.ts (pi v0.84.3):
//! single-shot mode for `pillar -p "prompt"` (text output) and
//! `pillar --mode json` (event stream).
//!
//! divergence: the JSON event stream needs `modes/json-event.ts`, which is
//! not ported yet, so [`PrintModeMode::Json`] returns an error. Output is
//! written to the caller's writer instead of raw stdout, and signal
//! handlers / detached-child cleanup live in the host.

use std::io::Write;

use pillar_agent::types::AgentMessage;
use pillar_ai::types::{Content, Message, StopReason};

use crate::core::agent_session_class::{AgentSession, PromptOptions};

/// Output mode for print mode (upstream `"text" | "json"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrintModeMode {
    #[default]
    Text,
    Json,
}

/// Options for [`run_print_mode`] (upstream `PrintModeOptions`).
#[derive(Debug, Clone, Default)]
pub struct PrintModeOptions {
    pub mode: PrintModeMode,
    /// Additional prompts sent after the initial message.
    pub messages: Vec<String>,
    /// First message to send (may contain `@file` content resolved by the host).
    pub initial_message: Option<String>,
    /// Images attached to the initial message.
    pub initial_images: Option<Vec<Content>>,
}

/// Run in print (single-shot) mode: send the prompts and write the result.
/// Returns the process exit code (0 on success, 1 on an assistant error).
pub async fn run_print_mode(
    session: &AgentSession,
    options: PrintModeOptions,
    out: &mut impl Write,
) -> Result<i32, String> {
    if let Some(initial) = &options.initial_message {
        session
            .prompt(
                initial,
                Some(&PromptOptions {
                    images: options.initial_images.clone(),
                    ..Default::default()
                }),
            )
            .await?;
    }
    for message in &options.messages {
        session.prompt(message, None).await?;
    }

    if options.mode == PrintModeMode::Json {
        return Err("json print mode is not ported yet".to_string());
    }

    let state = session.state();
    if let Some(AgentMessage::Message(Message::Assistant(assistant))) = state.messages.last() {
        if assistant.stop_reason == StopReason::Error
            || assistant.stop_reason == StopReason::Aborted
        {
            let message = assistant
                .error_message
                .clone()
                .unwrap_or_else(|| format!("Request {}", stop_reason_label(assistant.stop_reason)));
            return Err(message);
        }
        for content in &assistant.content {
            if let Content::Text { text, .. } = content {
                writeln!(out, "{text}").map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(0)
}

fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Pending => "pending",
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        StopReason::Deferred => "deferred",
    }
}
