//! Port of components/custom-message.ts: an extension's custom message, with a
//! custom renderer when one is registered and the default box otherwise.

use crate::core::extensions_types::{MessageRenderOptions, MessageRenderer};
use crate::core::messages::{CustomContent, CustomMessage};
use crate::modes::interactive::components::markdown_transform::{
    MarkdownThemeFactory, default_markdown_theme_factory,
};
use crate::modes::interactive::theme::theme;
use pillar_tui::components::{BoxComponent, Spacer, Text};
use pillar_tui::markdown::{DefaultTextStyle, Markdown};
use pillar_tui::tui::{Component, Container, RenderLines};

/// A custom message (upstream `CustomMessageComponent`).
///
/// divergence: upstream attaches and detaches the renderer's component from a
/// container; the port rebuilds its children on every change.
pub struct CustomMessageComponent {
    container: Container,
    message: CustomMessage,
    custom_renderer: Option<MessageRenderer>,
    markdown_theme: MarkdownThemeFactory,
    expanded: bool,
    output_pad: usize,
    used_custom_renderer: bool,
}

impl CustomMessageComponent {
    pub fn new(
        message: CustomMessage,
        custom_renderer: Option<MessageRenderer>,
        markdown_theme: Option<MarkdownThemeFactory>,
        output_pad: usize,
    ) -> Self {
        let mut component = Self {
            container: Container::new(),
            message,
            custom_renderer,
            markdown_theme: markdown_theme.unwrap_or_else(default_markdown_theme_factory),
            expanded: false,
            output_pad,
            used_custom_renderer: false,
        };
        component.container.add_child(Box::new(Spacer::new(1)));
        component.rebuild();
        component
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild();
        }
    }

    pub fn set_output_pad(&mut self, output_pad: usize) {
        if self.output_pad != output_pad {
            self.output_pad = output_pad;
            self.rebuild();
        }
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// Whether the registered renderer supplied the component (upstream
    /// `customComponent !== undefined`).
    pub fn used_custom_renderer(&self) -> bool {
        self.used_custom_renderer
    }

    pub fn rebuild(&mut self) {
        self.container.clear();
        self.used_custom_renderer = false;
        self.container.add_child(Box::new(Spacer::new(1)));

        if let Some(renderer) = self.custom_renderer.as_ref() {
            let options = MessageRenderOptions {
                expanded: self.expanded,
                output_pad: self.output_pad,
            };
            let payload = crate::core::extensions_types::message_render_payload(&self.message);
            let style = crate::core::extensions_types::theme_style_fn();
            if let Some(lines) = renderer(&payload, &options, &*style) {
                self.used_custom_renderer = true;
                // The renderer answers themed lines; the component is the
                // presentation adapter's (upstream's renderer returns a
                // Component).
                self.container
                    .add_child(Box::new(Text::new(&lines.join("\n"), 0, 0)));
                return;
            }
        }

        let theme_handle = theme();
        let mut box_component = BoxComponent::new(1, 1);
        let bg_theme = theme_handle.clone();
        box_component.set_bg_fn(Some(Box::new(move |text: &str| {
            bg_theme.bg("customMessageBg", text)
        })));
        let label = theme_handle.fg(
            "customMessageLabel",
            &format!("\u{1b}[1m[{}]\u{1b}[22m", self.message.custom_type),
        );
        box_component.add_child(Box::new(Text::new(&label, 0, 0)));
        box_component.add_child(Box::new(Spacer::new(1)));

        let text = custom_message_text(&self.message);
        box_component.add_child(Box::new(Markdown::new(
            &text,
            0,
            0,
            (self.markdown_theme)(),
            Some(DefaultTextStyle {
                color: Some(Box::new(|text: &str| theme().fg("customMessageText", text))),
                ..Default::default()
            }),
            Default::default(),
        )));
        self.container.add_child(Box::new(box_component));
    }
}

/// The message's text content: image blocks are dropped like upstream's
/// `TextContent` filter, text blocks are joined with newlines.
pub fn custom_message_text(message: &CustomMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            CustomContent::Text(text) => Some(text.as_str()),
            CustomContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Component for CustomMessageComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        self.container.render(width)
    }

    fn invalidate(&mut self) {
        self.rebuild();
    }
}
