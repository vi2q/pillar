//! Port of components/assistant-message.ts: an assistant message with its
//! thinking runs, truncation/abort/error notices and OSC 133 markers.

use crate::core::extensions_types::{MarkdownMessageType, MarkdownTransformer};
use crate::modes::interactive::components::markdown_transform::{
    MarkdownThemeFactory, create_markdown_transform, default_markdown_theme_factory,
};
use crate::modes::interactive::theme::theme;
use pillar_ai::types::{AssistantMessage, Content, StopReason};
use pillar_tui::components::{Spacer, Text};
use pillar_tui::markdown::{DefaultTextStyle, Markdown, MarkdownOptions};
use pillar_tui::tui::{Component, Container, RenderLines};

const OSC133_ZONE_START: &str = "\u{1b}]133;A\u{7}";
const OSC133_ZONE_END: &str = "\u{1b}]133;B\u{7}";
const OSC133_ZONE_FINAL: &str = "\u{1b}]133;C\u{7}";

/// A complete assistant message (upstream `AssistantMessageComponent`).
///
/// divergence: upstream keeps a nested content container and clears only that;
/// the port rebuilds its single container (the outer container only ever holds
/// the content container).
pub struct AssistantMessageComponent {
    container: Container,
    hide_thinking_block: bool,
    markdown_theme: MarkdownThemeFactory,
    hidden_thinking_label: String,
    output_pad: usize,
    markdown_transformers: Vec<MarkdownTransformer>,
    last_message: Option<AssistantMessage>,
    has_tool_calls: bool,
    is_streaming: bool,
}

impl AssistantMessageComponent {
    pub fn new(
        message: Option<AssistantMessage>,
        hide_thinking_block: bool,
        markdown_theme: Option<MarkdownThemeFactory>,
        hidden_thinking_label: &str,
        output_pad: usize,
        markdown_transformers: Vec<MarkdownTransformer>,
    ) -> Self {
        let mut component = Self {
            container: Container::new(),
            hide_thinking_block,
            markdown_theme: markdown_theme.unwrap_or_else(default_markdown_theme_factory),
            hidden_thinking_label: hidden_thinking_label.to_string(),
            output_pad,
            markdown_transformers,
            last_message: None,
            has_tool_calls: false,
            is_streaming: false,
        };
        if let Some(message) = message {
            component.update_content(&message, false);
        }
        component
    }

    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking_block = hide;
        self.refresh();
    }

    pub fn set_hidden_thinking_label(&mut self, label: &str) {
        self.hidden_thinking_label = label.to_string();
        self.refresh();
    }

    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        self.refresh();
    }

    /// Whether the message contains tool calls (upstream `hasToolCalls`).
    pub fn has_tool_calls(&self) -> bool {
        self.has_tool_calls
    }

    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    pub fn last_message(&self) -> Option<&AssistantMessage> {
        self.last_message.as_ref()
    }

    fn refresh(&mut self) {
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.is_streaming);
        }
    }

    /// Render the message content again (upstream `updateContent`).
    pub fn update_content(&mut self, message: &AssistantMessage, is_streaming: bool) {
        self.last_message = Some(message.clone());
        self.is_streaming = is_streaming;
        self.container.clear();

        let has_visible_content = message.content.iter().any(|content| match content {
            Content::Text { text, .. } => !text.trim().is_empty(),
            Content::Thinking { thinking, .. } => !thinking.trim().is_empty(),
            _ => false,
        });
        if has_visible_content {
            self.container.add_child(Box::new(Spacer::new(1)));
        }

        let mut index = 0usize;
        while index < message.content.len() {
            match &message.content[index] {
                Content::Text { text, .. } if !text.trim().is_empty() => {
                    self.container.add_child(Box::new(Markdown::new(
                        text.trim(),
                        self.output_pad,
                        0,
                        (self.markdown_theme)(),
                        None,
                        MarkdownOptions {
                            transform: Some(create_markdown_transform(
                                MarkdownMessageType::Assistant,
                                self.is_streaming,
                                self.markdown_transformers.clone(),
                            )),
                            ..Default::default()
                        },
                    )));
                }
                Content::Thinking { thinking, .. } => {
                    let mut thinking_blocks: Vec<String> = Vec::new();
                    if !thinking.trim().is_empty() {
                        thinking_blocks.push(thinking.trim().to_string());
                    }
                    let mut next = index + 1;
                    while next < message.content.len() {
                        match &message.content[next] {
                            Content::Thinking { thinking, .. } => {
                                if !thinking.trim().is_empty() {
                                    thinking_blocks.push(thinking.trim().to_string());
                                }
                                next += 1;
                            }
                            _ => break,
                        }
                    }
                    index = next.saturating_sub(1);

                    if !thinking_blocks.is_empty() {
                        // Spacing only when another visible block follows.
                        let has_visible_content_after =
                            message.content[next..].iter().any(|content| match content {
                                Content::Text { text, .. } => !text.trim().is_empty(),
                                Content::Thinking { thinking, .. } => !thinking.trim().is_empty(),
                                _ => false,
                            });

                        if self.hide_thinking_block {
                            let theme_handle = theme();
                            self.container.add_child(Box::new(Text::new(
                                &theme_handle.italic(
                                    &theme_handle.fg("thinkingText", &self.hidden_thinking_label),
                                ),
                                self.output_pad,
                                0,
                            )));
                        } else {
                            self.container.add_child(Box::new(Markdown::new(
                                &thinking_blocks.join("\n\n"),
                                self.output_pad,
                                0,
                                (self.markdown_theme)(),
                                Some(DefaultTextStyle {
                                    color: Some(Box::new(|text: &str| {
                                        theme().fg("thinkingText", text)
                                    })),
                                    italic: true,
                                    ..Default::default()
                                }),
                                MarkdownOptions {
                                    transform: Some(create_markdown_transform(
                                        MarkdownMessageType::AssistantThinking,
                                        self.is_streaming,
                                        self.markdown_transformers.clone(),
                                    )),
                                    ..Default::default()
                                },
                            )));
                        }
                        if has_visible_content_after {
                            self.container.add_child(Box::new(Spacer::new(1)));
                        }
                    }
                }
                _ => {}
            }
            index += 1;
        }

        let has_tool_calls = message
            .content
            .iter()
            .any(|content| matches!(content, Content::ToolCall { .. }));
        self.has_tool_calls = has_tool_calls;
        if message.stop_reason == StopReason::Length {
            self.container.add_child(Box::new(Spacer::new(1)));
            self.container.add_child(Box::new(Text::new(
                &theme().fg("error", "Response was truncated before completion."),
                self.output_pad,
                0,
            )));
        } else if !has_tool_calls {
            match message.stop_reason {
                StopReason::Aborted => {
                    let abort_message = match message.error_message.as_deref() {
                        Some(message) if message != "Request was aborted" => message.to_string(),
                        _ => "Operation aborted".to_string(),
                    };
                    self.container.add_child(Box::new(Spacer::new(1)));
                    self.container.add_child(Box::new(Text::new(
                        &theme().fg("error", &abort_message),
                        self.output_pad,
                        0,
                    )));
                }
                StopReason::Error => {
                    let error = message.error_message.as_deref().unwrap_or("Unknown error");
                    self.container.add_child(Box::new(Spacer::new(1)));
                    self.container.add_child(Box::new(Text::new(
                        &theme().fg("error", &format!("Error: {error}")),
                        self.output_pad,
                        0,
                    )));
                }
                _ => {}
            }
        }
    }
}

impl Component for AssistantMessageComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        let lines = self.container.render(width);
        if self.has_tool_calls || lines.is_empty() {
            return lines;
        }
        // Two lines get an OSC 133 marker: rebuild those entries (the frame is
        // shared, so it cannot be mutated in place).
        let mut owned: Vec<std::sync::Arc<str>> = lines.iter().cloned().collect();
        let last = owned.len() - 1;
        owned[0] = std::sync::Arc::from(format!("{OSC133_ZONE_START}{}", owned[0]).as_str());
        owned[last] =
            std::sync::Arc::from(format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}{}", owned[last]).as_str());
        owned.into()
    }

    fn invalidate(&mut self) {
        self.refresh();
    }
}
