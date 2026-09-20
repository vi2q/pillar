//! Port of components/user-message.ts: a user message on its own background
//! with OSC 133 shell-integration markers around it.

use crate::core::extensions_types::{MarkdownMessageType, MarkdownTransformer};
use crate::modes::interactive::components::markdown_transform::{
    MarkdownThemeFactory, create_markdown_transform, default_markdown_theme_factory,
};
use crate::modes::interactive::theme::theme;
use pillar_tui::components::BoxComponent;
use pillar_tui::markdown::{DefaultTextStyle, Markdown, MarkdownOptions};
use pillar_tui::tui::{Component, Container, RenderLines};

const OSC133_ZONE_START: &str = "\u{1b}]133;A\u{7}";
const OSC133_ZONE_END: &str = "\u{1b}]133;B\u{7}";
const OSC133_ZONE_FINAL: &str = "\u{1b}]133;C\u{7}";

/// A user message (upstream `UserMessageComponent`).
pub struct UserMessageComponent {
    text: String,
    markdown_theme: MarkdownThemeFactory,
    output_pad: usize,
    markdown_transformers: Vec<MarkdownTransformer>,
    container: Container,
}

impl UserMessageComponent {
    pub fn new(
        text: &str,
        markdown_theme: Option<MarkdownThemeFactory>,
        output_pad: usize,
        markdown_transformers: Vec<MarkdownTransformer>,
    ) -> Self {
        let mut component = Self {
            text: text.to_string(),
            markdown_theme: markdown_theme.unwrap_or_else(default_markdown_theme_factory),
            output_pad,
            markdown_transformers,
            container: Container::new(),
        };
        component.rebuild();
        component
    }

    /// Change the horizontal padding and rebuild (upstream `setOutputPad`).
    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        self.rebuild();
    }

    /// Replace the message text and rebuild.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.rebuild();
    }

    pub fn rebuild(&mut self) {
        self.container.clear();
        let theme_handle = theme();
        let mut content_box = BoxComponent::new(self.output_pad, 1);
        let bg_theme = theme_handle.clone();
        content_box.set_bg_fn(Some(Box::new(move |content: &str| {
            bg_theme.bg("userMessageBg", content)
        })));
        content_box.add_child(Box::new(Markdown::new(
            &self.text,
            0,
            0,
            (self.markdown_theme)(),
            Some(DefaultTextStyle {
                color: Some(Box::new(|content: &str| {
                    theme().fg("userMessageText", content)
                })),
                ..Default::default()
            }),
            MarkdownOptions {
                preserve_ordered_list_markers: true,
                preserve_backslash_escapes: true,
                transform: Some(create_markdown_transform(
                    MarkdownMessageType::User,
                    false,
                    self.markdown_transformers.clone(),
                )),
                ..Default::default()
            },
        )));
        self.container.add_child(Box::new(content_box));
    }
}

impl Component for UserMessageComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        let lines = self.container.render(width);
        if lines.is_empty() {
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
        self.rebuild();
    }
}
