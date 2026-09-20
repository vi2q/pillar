//! Port of packages/coding-agent/src/modes/print-mode.ts (pi v0.84.3):
//! single-shot mode for `pillar -p "prompt"` (text output) and
//! `pillar --mode json` (event stream).
//!
//! divergence: the JSON event stream needs `modes/json-event.ts`, which is
//! not ported yet, so [`PrintModeMode::Json`] returns an error. Output is
//! written to the caller's writer instead of raw stdout, and signal
//! handlers / detached-child cleanup live in the host.

use std::io::Write;
use std::sync::{Arc, Mutex};

use pillar_agent::types::AgentMessage;
use pillar_ai::types::{Content, Message, StopReason};
use serde_json::Value;

use crate::core::agent_session_class::{AgentSession, PromptOptions};
use crate::modes::json_event::to_json_event;

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
    if options.mode == PrintModeMode::Json {
        return run_json_mode(session, options, out).await;
    }

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

/// JSON mode: write the session header, then one JSON event per line
/// (upstream `--mode json`).
async fn run_json_mode(
    session: &AgentSession,
    options: PrintModeOptions,
    out: &mut impl Write,
) -> Result<i32, String> {
    if let Some(header) = session
        .session_manager()
        .lock()
        .expect("session lock")
        .get_header()
        .cloned()
    {
        let value = serde_json::to_value(&header).map_err(|error| error.to_string())?;
        writeln!(out, "{value}").map_err(|error| error.to_string())?;
    }

    let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&events);
    let error_slot = Arc::clone(&first_error);
    let unsubscribe = session.subscribe(Arc::new(move |event| match to_json_event(event) {
        Ok(value) => sink.lock().expect("events lock").push(value),
        Err(error) => {
            let mut slot = error_slot.lock().expect("error slot");
            if slot.is_none() {
                *slot = Some(error);
            }
        }
    }));

    let mut run_result: Result<(), String> = Ok(());
    if let Some(initial) = &options.initial_message
        && let Err(error) = session
            .prompt(
                initial,
                Some(&PromptOptions {
                    images: options.initial_images.clone(),
                    ..Default::default()
                }),
            )
            .await
    {
        run_result = Err(error);
    }
    if run_result.is_ok() {
        for message in &options.messages {
            if let Err(error) = session.prompt(message, None).await {
                run_result = Err(error);
                break;
            }
        }
    }
    unsubscribe();

    for value in events.lock().expect("events lock").drain(..) {
        writeln!(out, "{value}").map_err(|error| error.to_string())?;
    }
    if let Some(error) = first_error.lock().expect("error slot").take() {
        return Err(error);
    }
    run_result?;
    Ok(0)
}
