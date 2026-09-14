//! Port of the transcript-driving half of
//! packages/coding-agent/src/modes/interactive/interactive-mode.ts (pi
//! v0.84.3): the chat container assembly (`addMessageToChat`,
//! `addCustomEntryToChat`, the transcript-visible subset of `handleEvent`)
//! and the populate path (`renderSessionItems` / `renderSessionEntries`).
//!
//! The status indicators, pending-messages display, editor/footer wiring and
//! the extension UI face of `handleEvent` stay in the interactive-mode
//! slices that follow; this module owns exactly what renders into the chat
//! container.
//!
//! divergences:
//! - upstream components are shared objects (a `ToolExecutionComponent`
//!   lives both in the container and the `pendingTools` map); the port wraps
//!   them in [`Shared`] (`Arc<Mutex<..>>`) to keep the same sharing.
//! - upstream removes the streaming component by identity; the port tracks
//!   its container index and keeps it correct across the custom-entry splice
//!   (later appends never shift it).
//! - `requestRender` / `footer.invalidate` calls are host-driven and
//!   omitted.
//! - the retry-attempt count for abort messages is a field the host updates
//!   (upstream reads `session.retryAttempt` inline).
//! - extension-provided tool definitions are not registered yet, so
//!   `getRegisteredToolDefinition` answers `None` (built-ins resolve inside
//!   `ToolExecutionComponent`); entry/message renderers come from injected
//!   lookup callbacks (the extension runner surface lands later).
//! - cache-miss notices: the render helpers live here, but the
//!   `showCacheMissNotices` gating and the `detectCacheMiss` wiring stay
//!   with the host (cache-stats entries are built from sessions by the
//!   caller).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pillar_ai::types::{Content, StopReason};
use pillar_tui::components::{Spacer, Text};
use pillar_tui::tui::{Component, Container};

use crate::core::agent_session::parse_skill_block;
use crate::core::agent_session_class::{AgentSessionEvent, agent_message_to_coding};
use crate::core::cache_stats::{CACHE_TTL_MS, CacheMiss};
use crate::core::extensions_types::{EntryRenderer, MarkdownTransformer, MessageRenderer};
use crate::core::messages::CodingAgentMessage;
use crate::core::session_entries::{CustomEntry, SessionEntry};
use crate::core::session_manager::session_entry_to_context_messages;
use crate::modes::interactive::components::assistant_message::AssistantMessageComponent;
use crate::modes::interactive::components::bash_execution::BashExecutionComponent;
use crate::modes::interactive::components::branch_summary_message::BranchSummaryMessageComponent;
use crate::modes::interactive::components::compaction_summary_message::CompactionSummaryMessageComponent;
use crate::modes::interactive::components::custom_entry::CustomEntryComponent;
use crate::modes::interactive::components::custom_message::CustomMessageComponent;
use crate::modes::interactive::components::footer::format_tokens;
use crate::modes::interactive::components::markdown_transform::MarkdownThemeFactory;
use crate::modes::interactive::components::skill_invocation_message::SkillInvocationMessageComponent;
use crate::modes::interactive::components::tool_execution::{
    ToolExecutionComponent, ToolExecutionOptions, ToolExecutionResult,
};
use crate::modes::interactive::components::user_message::UserMessageComponent;
use crate::modes::interactive::theme::theme;

/// Parse a tool result object from the event payload (upstream the
/// `partialResult` / `result` objects of `tool_execution_update` / `_end`).
pub fn tool_execution_result_from_json(
    value: &serde_json::Value,
    is_error: bool,
) -> ToolExecutionResult {
    let content = value
        .get("content")
        .and_then(|content| content.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| serde_json::from_value::<Content>(block.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ToolExecutionResult {
        content,
        details: value.get("details").cloned().unwrap_or_default(),
        is_error,
    }
}

/// The text of a user message (upstream `getUserMessageText`): the content's
/// text blocks joined without separators.
pub fn get_user_message_text(message: &pillar_ai::types::Message) -> String {
    match message {
        pillar_ai::types::Message::User { content, .. } => match content {
            pillar_ai::types::UserContent::Text(text) => text.clone(),
            pillar_ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    Content::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect(),
        },
        _ => String::new(),
    }
}

/// Entry renderer lookup (upstream
/// `session.extensionRunner.getEntryRenderer`).
pub type EntryRendererLookup = Box<dyn Fn(&str) -> Option<EntryRenderer> + Send>;

/// Message renderer lookup (upstream
/// `session.extensionRunner.getMessageRenderer`).
pub type MessageRendererLookup = Box<dyn Fn(&str) -> Option<MessageRenderer> + Send>;

/// Editor history sink (upstream `editor.addToHistory`).
pub type HistorySink = Box<dyn FnMut(&str) + Send>;

/// A component shared between the chat container and another owner (upstream
/// the shared object references of `streamingComponent` / `pendingTools`).
pub struct Shared<C>(Arc<Mutex<C>>);

impl<C> Shared<C> {
    pub fn new(component: C) -> Self {
        Self(Arc::new(Mutex::new(component)))
    }

    /// The shared component for updates (upstream's direct references).
    pub fn lock(&self) -> std::sync::MutexGuard<'_, C> {
        self.0.lock().expect("shared component")
    }

    /// Whether the two handles share the same object (upstream identity
    /// checks).
    pub fn ptr_eq(a: &Shared<C>, b: &Shared<C>) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }
}

impl<C> Clone for Shared<C> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<C: Component> Component for Shared<C> {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.lock().render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.lock().handle_input(data);
    }

    fn wants_key_release(&self) -> bool {
        self.lock().wants_key_release()
    }

    fn invalidate(&mut self) {
        self.lock().invalidate();
    }
}

/// Transcript-level knobs (upstream the settings-derived fields).
#[derive(Debug, Clone)]
pub struct TranscriptSettings {
    pub hide_thinking_block: bool,
    pub hidden_thinking_label: String,
    pub output_pad: usize,
    pub tool_output_expanded: bool,
    pub show_images: bool,
    pub image_width_cells: usize,
    pub show_cache_miss_notices: bool,
}

impl Default for TranscriptSettings {
    fn default() -> Self {
        Self {
            hide_thinking_block: false,
            hidden_thinking_label: "Thinking...".to_string(),
            output_pad: 1,
            tool_output_expanded: false,
            show_images: true,
            image_width_cells: 60,
            show_cache_miss_notices: false,
        }
    }
}

/// One flattened item of the populate path (upstream `RenderSessionItem`).
#[derive(Debug, Clone)]
pub enum RenderSessionItem {
    Custom(CustomEntry),
    Message(CodingAgentMessage),
    CompactionCost {
        kind: CompactionCostKind,
        usage: pillar_ai::types::Usage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionCostKind {
    Compaction,
    BranchSummary,
}

/// The transcript half of interactive mode (upstream the chat-container
/// fields and transcript methods of `InteractiveMode`).
pub struct InteractiveTranscript {
    pub chat: Container,
    /// Upstream `pendingTools: Map<toolCallId, ToolExecutionComponent>`.
    pending_tools: BTreeMap<String, Shared<ToolExecutionComponent>>,
    /// Upstream `streamingComponent` (shared) with its container index.
    streaming: Option<(usize, Shared<AssistantMessageComponent>)>,
    /// Upstream `streamingMessage`.
    streaming_message: Option<pillar_ai::types::AssistantMessage>,
    settings: TranscriptSettings,
    markdown_theme: Option<MarkdownThemeFactory>,
    markdown_transformers: Vec<MarkdownTransformer>,
    /// Upstream `session.extensionRunner.getEntryRenderer` (injected until
    /// the extension runner surface exposes renderers).
    entry_renderer_lookup: Option<EntryRendererLookup>,
    /// Upstream `session.extensionRunner.getMessageRenderer`.
    message_renderer_lookup: Option<MessageRendererLookup>,
    /// Editor history sink (upstream `editor.addToHistory?.(textContent)`).
    on_history: Option<HistorySink>,
    /// Upstream `session.retryAttempt`, host-updated.
    pub retry_attempt: u32,
    cwd: String,
}

impl InteractiveTranscript {
    pub fn new(
        settings: TranscriptSettings,
        markdown_theme: Option<MarkdownThemeFactory>,
        markdown_transformers: Vec<MarkdownTransformer>,
        cwd: &str,
    ) -> Self {
        Self {
            chat: Container::new(),
            pending_tools: BTreeMap::new(),
            streaming: None,
            streaming_message: None,
            settings,
            markdown_theme,
            markdown_transformers,
            entry_renderer_lookup: None,
            message_renderer_lookup: None,
            on_history: None,
            retry_attempt: 0,
            cwd: cwd.to_string(),
        }
    }

    pub fn set_entry_renderer_lookup(&mut self, lookup: Option<EntryRendererLookup>) {
        self.entry_renderer_lookup = lookup;
    }

    pub fn set_message_renderer_lookup(&mut self, lookup: Option<MessageRendererLookup>) {
        self.message_renderer_lookup = lookup;
    }

    pub fn set_on_history(&mut self, on_history: Option<HistorySink>) {
        self.on_history = on_history;
    }

    /// The transcript settings (hosts flip knobs like
    /// `show_cache_miss_notices` / `tool_output_expanded` here).
    pub fn settings_mut(&mut self) -> &mut TranscriptSettings {
        &mut self.settings
    }

    /// Replace the hidden-thinking label for later components (upstream also
    /// updates every existing assistant child; the port keeps the label
    /// lookup lazy via [`AssistantMessageComponent::set_hidden_thinking_label`]
    /// at the call sites that rebuild).
    pub fn set_hidden_thinking_label(&mut self, label: &str) {
        self.settings.hidden_thinking_label = label.to_string();
        if let Some((_, streaming)) = &self.streaming {
            streaming.lock().set_hidden_thinking_label(label);
        }
    }

    pub fn set_tool_output_expanded(&mut self, expanded: bool) {
        self.settings.tool_output_expanded = expanded;
    }

    /// The streaming message, if any (upstream `streamingMessage`).
    pub fn streaming_message(&self) -> Option<&pillar_ai::types::AssistantMessage> {
        self.streaming_message.as_ref()
    }

    /// The streaming component handle, if any (upstream `streamingComponent`).
    pub fn streaming(&self) -> Option<&Shared<AssistantMessageComponent>> {
        self.streaming.as_ref().map(|(_, shared)| shared)
    }

    /// The pending tool executions (upstream `pendingTools`).
    pub fn pending_tools(&self) -> &BTreeMap<String, Shared<ToolExecutionComponent>> {
        &self.pending_tools
    }

    fn lookup_entry_renderer(&self, custom_type: &str) -> Option<EntryRenderer> {
        let lookup = self.entry_renderer_lookup.as_ref()?;
        lookup(custom_type)
    }

    fn lookup_message_renderer(&self, custom_type: &str) -> Option<MessageRenderer> {
        let lookup = self.message_renderer_lookup.as_ref()?;
        lookup(custom_type)
    }

    /// A pending tool's renderer for a tool name (upstream
    /// `getRegisteredToolDefinition`; extension definitions are not
    /// registered yet, so only `None` for now — built-ins resolve inside the
    /// component).
    fn registered_tool_definition(
        &self,
        _tool_name: &str,
    ) -> Option<Box<dyn crate::core::tools::render_definitions::ToolRenderer>> {
        None
    }

    fn new_tool_component(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        args: serde_json::Value,
    ) -> ToolExecutionComponent {
        let mut component = ToolExecutionComponent::new(
            tool_name,
            tool_call_id,
            args,
            ToolExecutionOptions {
                show_images: Some(self.settings.show_images),
                image_width_cells: Some(self.settings.image_width_cells),
            },
            self.registered_tool_definition(tool_name),
            &self.cwd,
        );
        component.set_expanded(self.settings.tool_output_expanded);
        component
    }

    // --- upstream addMessageToChat ---

    pub fn add_message_to_chat(&mut self, message: CodingAgentMessage, populate_history: bool) {
        match message {
            CodingAgentMessage::BashExecution(bash) => {
                let mut component = BashExecutionComponent::new(
                    &crate::modes::interactive::theme::theme(),
                    &bash.command,
                    bash.exclude_from_context,
                );
                if !bash.output.is_empty() {
                    component.append_output(&bash.output);
                }
                component.set_complete(
                    bash.exit_code,
                    bash.cancelled,
                    if bash.truncated {
                        Some(crate::core::truncate::TruncationResult {
                            truncated: true,
                            ..Default::default()
                        })
                    } else {
                        None
                    },
                    bash.full_output_path,
                );
                self.chat.add_child(Box::new(component));
            }
            CodingAgentMessage::Custom(custom) => {
                if custom.display {
                    let renderer = self.lookup_message_renderer(&custom.custom_type);
                    let mut component = CustomMessageComponent::new(
                        custom,
                        renderer,
                        self.markdown_theme.clone(),
                        self.settings.output_pad,
                    );
                    component.set_expanded(self.settings.tool_output_expanded);
                    self.chat.add_child(Box::new(component));
                }
            }
            CodingAgentMessage::CompactionSummary(summary) => {
                self.chat.add_child(Box::new(Spacer::new(1)));
                let mut component =
                    CompactionSummaryMessageComponent::new(summary, self.markdown_theme.clone());
                component.set_expanded(self.settings.tool_output_expanded);
                self.chat.add_child(Box::new(component));
            }
            CodingAgentMessage::BranchSummary(summary) => {
                self.chat.add_child(Box::new(Spacer::new(1)));
                let mut component =
                    BranchSummaryMessageComponent::new(summary, self.markdown_theme.clone());
                component.set_expanded(self.settings.tool_output_expanded);
                self.chat.add_child(Box::new(component));
            }
            CodingAgentMessage::Base(message)
                if matches!(message, pillar_ai::types::Message::User { .. }) =>
            {
                let text_content = get_user_message_text(&message);
                if text_content.is_empty() {
                    return;
                }
                if !self.chat.is_empty() {
                    self.chat.add_child(Box::new(Spacer::new(1)));
                }
                if let Some(skill_block) = parse_skill_block(&text_content) {
                    // Skill block (collapsible), then the user message.
                    let mut component = SkillInvocationMessageComponent::new(
                        skill_block.clone(),
                        self.markdown_theme.clone(),
                    );
                    component.set_expanded(self.settings.tool_output_expanded);
                    self.chat.add_child(Box::new(component));
                    if let Some(user_message) = &skill_block.user_message {
                        self.chat.add_child(Box::new(Spacer::new(1)));
                        let user_component = UserMessageComponent::new(
                            user_message,
                            self.markdown_theme.clone(),
                            self.settings.output_pad,
                            self.markdown_transformers.clone(),
                        );
                        self.chat.add_child(Box::new(user_component));
                    }
                } else {
                    let user_component = UserMessageComponent::new(
                        &text_content,
                        self.markdown_theme.clone(),
                        self.settings.output_pad,
                        self.markdown_transformers.clone(),
                    );
                    self.chat.add_child(Box::new(user_component));
                }
                if populate_history {
                    if let Some(on_history) = self.on_history.as_mut() {
                        on_history(&text_content);
                    }
                }
            }
            CodingAgentMessage::Base(message @ pillar_ai::types::Message::Assistant(_)) => {
                let pillar_ai::types::Message::Assistant(assistant) = message else {
                    unreachable!("matched assistant")
                };
                let component = AssistantMessageComponent::new(
                    Some(*assistant),
                    self.settings.hide_thinking_block,
                    self.markdown_theme.clone(),
                    &self.settings.hidden_thinking_label,
                    self.settings.output_pad,
                    self.markdown_transformers.clone(),
                );
                self.chat.add_child(Box::new(component));
            }
            CodingAgentMessage::Base(pillar_ai::types::Message::ToolResult(_)) => {
                // Tool results render inline with their tool calls.
            }
            CodingAgentMessage::Base(_) => {}
        }
    }

    // --- upstream addCustomEntryToChat ---

    pub fn add_custom_entry_to_chat(&mut self, entry: &CustomEntry) {
        let Some(renderer) = self.lookup_entry_renderer(&entry.custom_type) else {
            return;
        };
        let mut component = CustomEntryComponent::new(entry.clone(), renderer);
        component.set_expanded(self.settings.tool_output_expanded);
        if !component.has_content() {
            return;
        }

        if let Some((streaming_index, _)) = &self.streaming {
            // Insert before the streaming component.
            self.chat
                .insert_child(*streaming_index, Box::new(component));
            if let Some((index, _)) = self.streaming.as_mut() {
                *index += 1;
            }
            return;
        }

        self.chat.add_child(Box::new(component));
    }

    /// The abort/error message shown for a failed run (upstream the
    /// `message_end` retry-aware wording).
    fn abort_error_message(
        &self,
        stop_reason: StopReason,
        message: &pillar_ai::types::AssistantMessage,
    ) -> String {
        if stop_reason == StopReason::Aborted {
            if self.retry_attempt > 0 {
                return format!(
                    "Aborted after {} retry attempt{}",
                    self.retry_attempt,
                    if self.retry_attempt > 1 { "s" } else { "" }
                );
            }
            return "Operation aborted".to_string();
        }
        message
            .error_message
            .clone()
            .unwrap_or_else(|| "Error".to_string())
    }

    // --- upstream handleEvent (transcript-visible subset) ---

    pub fn handle_event(&mut self, event: &AgentSessionEvent) {
        match event {
            AgentSessionEvent::AgentStart => {
                self.pending_tools.clear();
            }
            AgentSessionEvent::MessageStart { message } => match message {
                pillar_agent::types::AgentMessage::Custom(_) => {
                    self.add_message_to_chat(agent_message_to_coding(message.clone()), false);
                }
                pillar_agent::types::AgentMessage::Message(pillar_ai::types::Message::User {
                    ..
                }) => {
                    self.add_message_to_chat(agent_message_to_coding(message.clone()), false);
                }
                pillar_agent::types::AgentMessage::Message(
                    pillar_ai::types::Message::Assistant(assistant),
                ) => {
                    let shared = Shared::new(AssistantMessageComponent::new(
                        None,
                        self.settings.hide_thinking_block,
                        self.markdown_theme.clone(),
                        &self.settings.hidden_thinking_label,
                        self.settings.output_pad,
                        self.markdown_transformers.clone(),
                    ));
                    let index = self.chat.len();
                    self.chat.add_child(Box::new(shared.clone()));
                    self.streaming = Some((index, shared));
                    self.streaming_message = Some((**assistant).clone());
                    self.streaming
                        .as_ref()
                        .expect("streaming")
                        .1
                        .lock()
                        .update_content(assistant, true);
                }
                _ => {}
            },
            AgentSessionEvent::MessageUpdate { message, .. } => {
                let Some(shared) = self.streaming.as_ref().map(|(_, shared)| shared.clone()) else {
                    return;
                };
                let pillar_agent::types::AgentMessage::Message(
                    pillar_ai::types::Message::Assistant(assistant),
                ) = message
                else {
                    return;
                };
                self.streaming_message = Some((**assistant).clone());
                shared.lock().update_content(assistant, true);

                // Create/update the pending tool components for the
                // streaming tool calls.
                let streaming = self.streaming_message.as_ref().expect("streaming");
                for content in &streaming.content {
                    if let Content::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } = content
                    {
                        if let Some(component) = self.pending_tools.get(id) {
                            component.lock().update_args(arguments.clone());
                        } else {
                            let component =
                                Shared::new(self.new_tool_component(name, id, arguments.clone()));
                            self.chat.add_child(Box::new(component.clone()));
                            self.pending_tools.insert(id.clone(), component);
                        }
                    }
                }
            }
            AgentSessionEvent::MessageEnd { message } => {
                let pillar_agent::types::AgentMessage::Message(
                    pillar_ai::types::Message::Assistant(assistant),
                ) = message
                else {
                    return;
                };
                let Some((_index, shared)) = self.streaming.clone() else {
                    return;
                };
                self.streaming_message = Some((**assistant).clone());
                let mut error_message: Option<String> = None;
                if assistant.stop_reason == StopReason::Aborted {
                    let error = self.abort_error_message(StopReason::Aborted, assistant);
                    error_message = Some(error.clone());
                    // The message keeps the abort notice (upstream mutates
                    // `streamingMessage.errorMessage`).
                    if let Some(streaming) = self.streaming_message.as_mut() {
                        streaming.error_message = Some(error);
                    }
                }
                shared.lock().update_content(assistant, false);

                if assistant.stop_reason == StopReason::Aborted
                    || assistant.stop_reason == StopReason::Error
                {
                    let error_message = error_message.unwrap_or_else(|| {
                        self.abort_error_message(assistant.stop_reason, assistant)
                    });
                    for (_, component) in self.pending_tools.iter() {
                        component.lock().update_result(
                            ToolExecutionResult {
                                content: vec![Content::text(error_message.clone())],
                                details: serde_json::Value::Null,
                                is_error: true,
                            },
                            false,
                        );
                    }
                    self.pending_tools.clear();
                } else {
                    // Args are now complete: trigger diff computation.
                    for (_, component) in self.pending_tools.iter() {
                        component.lock().set_args_complete();
                    }
                }
                self.streaming = None;
                self.streaming_message = None;
            }
            AgentSessionEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let component = match self.pending_tools.get(tool_call_id) {
                    Some(component) => component.clone(),
                    None => {
                        let component = Shared::new(self.new_tool_component(
                            tool_name,
                            tool_call_id,
                            args.clone(),
                        ));
                        self.chat.add_child(Box::new(component.clone()));
                        self.pending_tools
                            .insert(tool_call_id.clone(), component.clone());
                        component
                    }
                };
                component.lock().mark_execution_started();
            }
            AgentSessionEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                if let Some(component) = self.pending_tools.get(tool_call_id) {
                    component.lock().update_result(
                        tool_execution_result_from_json(partial_result, false),
                        true,
                    );
                }
            }
            AgentSessionEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                if let Some(component) = self.pending_tools.get(tool_call_id) {
                    component
                        .lock()
                        .update_result(tool_execution_result_from_json(result, *is_error), false);
                    self.pending_tools.remove(tool_call_id);
                }
            }
            AgentSessionEvent::AgentEnd { .. } => {
                if let Some((index, _)) = self.streaming.take() {
                    self.chat.remove_child(index);
                }
                self.streaming_message = None;
                self.pending_tools.clear();
            }
            AgentSessionEvent::EntryAppended { entry }
                if matches!(entry, SessionEntry::Custom(_)) =>
            {
                if let SessionEntry::Custom(custom) = entry {
                    self.add_custom_entry_to_chat(custom);
                }
            }
            _ => {}
        }
    }

    // --- upstream renderSessionItems ---

    pub fn render_session_items(
        &mut self,
        items: Vec<RenderSessionItem>,
        misses: Option<&BTreeMap<(u64, String), CacheMiss>>,
    ) {
        self.pending_tools.clear();
        let mut rendered_pending_tools: BTreeMap<String, Shared<ToolExecutionComponent>> =
            BTreeMap::new();

        for item in items {
            match item {
                RenderSessionItem::Custom(entry) => {
                    self.add_custom_entry_to_chat(&entry);
                }
                RenderSessionItem::CompactionCost { kind, usage } => {
                    self.add_compaction_cost_notice(kind, &usage);
                }
                RenderSessionItem::Message(message) => {
                    // Assistant messages need special handling for tool calls.
                    if let CodingAgentMessage::Base(
                        assistant_message @ pillar_ai::types::Message::Assistant(_),
                    ) = &message
                    {
                        let pillar_ai::types::Message::Assistant(assistant) = assistant_message
                        else {
                            unreachable!("matched assistant")
                        };
                        self.add_message_to_chat(message.clone(), false);
                        for content in &assistant.content {
                            if let Content::ToolCall {
                                id,
                                name,
                                arguments,
                                ..
                            } = content
                            {
                                let component = Shared::new(self.new_tool_component(
                                    name,
                                    id,
                                    arguments.clone(),
                                ));
                                self.chat.add_child(Box::new(component.clone()));

                                if assistant.stop_reason == StopReason::Aborted
                                    || assistant.stop_reason == StopReason::Error
                                {
                                    let error_message =
                                        self.abort_error_message(assistant.stop_reason, assistant);
                                    component.lock().update_result(
                                        ToolExecutionResult {
                                            content: vec![Content::text(error_message)],
                                            details: serde_json::Value::Null,
                                            is_error: true,
                                        },
                                        false,
                                    );
                                } else {
                                    rendered_pending_tools.insert(id.clone(), component);
                                }
                            }
                        }
                        if assistant.stop_reason != StopReason::Aborted
                            && assistant.stop_reason != StopReason::Error
                        {
                            if let Some(miss) = misses.and_then(|misses| {
                                misses.get(&(
                                    assistant.timestamp,
                                    format!("{}/{}", assistant.provider, assistant.model),
                                ))
                            }) {
                                self.add_cache_miss_notice(miss);
                            }
                        }
                    } else if let CodingAgentMessage::Base(pillar_ai::types::Message::ToolResult(
                        tool_result,
                    )) = &message
                    {
                        // Match tool results to pending tool components.
                        if let Some(component) =
                            rendered_pending_tools.get(&tool_result.tool_call_id)
                        {
                            component
                                .lock()
                                .update_result(tool_result_to_render_result(tool_result), false);
                            rendered_pending_tools.remove(&tool_result.tool_call_id);
                        }
                    } else {
                        // All other messages use standard rendering.
                        self.add_message_to_chat(message.clone(), false);
                    }
                }
            }
        }

        for (tool_call_id, component) in rendered_pending_tools {
            self.pending_tools.insert(tool_call_id, component);
        }
    }

    /// Upstream `renderSessionEntries`: flatten entries to items and render.
    pub fn render_session_entries(
        &mut self,
        entries: &[SessionEntry],
        misses: Option<&BTreeMap<(u64, String), CacheMiss>>,
    ) {
        let items = entries
            .iter()
            .flat_map(|entry| -> Vec<RenderSessionItem> {
                if let SessionEntry::Custom(custom) = entry {
                    return vec![RenderSessionItem::Custom(custom.clone())];
                }
                let messages = session_entry_to_context_messages(entry);
                let usage = match entry {
                    SessionEntry::Compaction(compaction) => compaction.usage.clone(),
                    SessionEntry::BranchSummary(branch_summary) => branch_summary.usage.clone(),
                    _ => None,
                };
                if let Some(usage) = usage.filter(|_| !messages.is_empty()) {
                    let mut items: Vec<RenderSessionItem> = messages
                        .into_iter()
                        .map(RenderSessionItem::Message)
                        .collect();
                    items.push(RenderSessionItem::CompactionCost {
                        kind: if matches!(entry, SessionEntry::Compaction(_)) {
                            CompactionCostKind::Compaction
                        } else {
                            CompactionCostKind::BranchSummary
                        },
                        usage,
                    });
                    return items;
                }
                messages
                    .into_iter()
                    .map(RenderSessionItem::Message)
                    .collect()
            })
            .collect();
        self.render_session_items(items, misses);
    }

    // --- upstream addCompactionCostNotice ---

    pub fn add_compaction_cost_notice(
        &mut self,
        kind: CompactionCostKind,
        usage: &pillar_ai::types::Usage,
    ) {
        if !self.settings.show_cache_miss_notices {
            return;
        }

        let tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
        let cost = if usage.cost.total >= 0.01 {
            format!(" (~${:.2})", usage.cost.total)
        } else {
            String::new()
        };
        let label = match kind {
            CompactionCostKind::Compaction => "Compaction",
            CompactionCostKind::BranchSummary => "Branch summary",
        };
        self.chat.add_child(Box::new(Spacer::new(1)));
        self.chat.add_child(Box::new(Text::new(
            &theme().fg(
                "warning",
                &format!("{label}: {} tokens billed{cost}", format_tokens(tokens)),
            ),
            1,
            0,
        )));
    }

    // --- upstream addCacheMissNotice ---

    pub fn add_cache_miss_notice(&mut self, miss: &CacheMiss) {
        if miss.missed_tokens < 20_000 && miss.missed_cost < 0.1 {
            return;
        }

        let cost = if miss.missed_cost >= 0.01 {
            format!(" (~${:.2})", miss.missed_cost)
        } else {
            String::new()
        };
        let re_billed = format!(
            "{} tokens re-billed{cost}",
            format_tokens(miss.missed_tokens)
        );
        let mut label = "Cache miss".to_string();
        if miss.model_changed {
            label = "Cache miss after model switch".to_string();
        } else if miss.idle_ms >= CACHE_TTL_MS {
            label = format!("Cache miss after {}m idle", miss.idle_ms / 60_000);
        }
        let text = theme().fg("warning", &format!("{label}: {re_billed}"));
        self.chat.add_child(Box::new(Spacer::new(1)));
        self.chat.add_child(Box::new(Text::new(&text, 1, 0)));
    }
}

/// Shape a persisted tool result message for the tool execution component
/// (upstream passes the message object itself; the port maps it to the
/// render result).
fn tool_result_to_render_result(
    tool_result: &pillar_ai::types::ToolResultMessage,
) -> ToolExecutionResult {
    ToolExecutionResult {
        content: tool_result.content.clone(),
        details: tool_result.details.clone().unwrap_or_default(),
        is_error: false,
    }
}
