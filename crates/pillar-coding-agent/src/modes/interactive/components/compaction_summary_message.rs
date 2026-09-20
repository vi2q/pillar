//! Port of components/compaction-summary-message.ts: a compaction summary with
//! collapsed/expanded state.

use crate::core::messages::CompactionSummaryMessage;
use crate::modes::interactive::components::keybinding_hints::key_text;
use crate::modes::interactive::components::markdown_transform::{
    MarkdownThemeFactory, default_markdown_theme_factory,
};
use crate::modes::interactive::theme::theme;
use pillar_tui::components::{BoxComponent, Spacer, Text};
use pillar_tui::markdown::{DefaultTextStyle, Markdown};
use pillar_tui::tui::{Component, RenderLines};

/// A compaction summary message (upstream `CompactionSummaryMessageComponent`).
pub struct CompactionSummaryMessageComponent {
    box_component: BoxComponent,
    expanded: bool,
    message: CompactionSummaryMessage,
    markdown_theme: MarkdownThemeFactory,
}

/// Group the token count the way upstream's `toLocaleString` does.
fn format_tokens(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

impl CompactionSummaryMessageComponent {
    pub fn new(
        message: CompactionSummaryMessage,
        markdown_theme: Option<MarkdownThemeFactory>,
    ) -> Self {
        let mut component = Self {
            box_component: BoxComponent::new(1, 1),
            expanded: false,
            message,
            markdown_theme: markdown_theme.unwrap_or_else(default_markdown_theme_factory),
        };
        let theme_handle = theme();
        component
            .box_component
            .set_bg_fn(Some(Box::new(move |text: &str| {
                theme_handle.bg("customMessageBg", text)
            })));
        component.update_display();
        component
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    fn update_display(&mut self) {
        self.box_component.clear();
        let theme_handle = theme();
        let token_str = format_tokens(self.message.tokens_before);
        let label = theme_handle.fg("customMessageLabel", "\u{1b}[1m[compaction]\u{1b}[22m");
        self.box_component
            .add_child(Box::new(Text::new(&label, 0, 0)));
        self.box_component.add_child(Box::new(Spacer::new(1)));

        if self.expanded {
            let header = format!("**Compacted from {token_str} tokens**\n\n");
            self.box_component.add_child(Box::new(Markdown::new(
                &format!("{header}{}", self.message.summary),
                0,
                0,
                (self.markdown_theme)(),
                Some(DefaultTextStyle {
                    color: Some(Box::new(|text: &str| theme().fg("customMessageText", text))),
                    ..Default::default()
                }),
                Default::default(),
            )));
        } else {
            let hint = format!(
                "{}{}{}",
                theme_handle.fg(
                    "customMessageText",
                    &format!("Compacted from {token_str} tokens (")
                ),
                theme_handle.fg("dim", &key_text("app.tools.expand")),
                theme_handle.fg("customMessageText", " to expand)")
            );
            self.box_component
                .add_child(Box::new(Text::new(&hint, 0, 0)));
        }
    }
}

impl Component for CompactionSummaryMessageComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        self.box_component.render(width)
    }

    fn invalidate(&mut self) {
        self.update_display();
    }
}
