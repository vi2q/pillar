//! Port of components/branch-summary-message.ts: a branch summary with
//! collapsed/expanded state.

use crate::core::messages::BranchSummaryMessage;
use crate::modes::interactive::components::keybinding_hints::key_text;
use crate::modes::interactive::components::markdown_transform::{
    MarkdownThemeFactory, default_markdown_theme_factory,
};
use crate::modes::interactive::theme::theme;
use pillar_tui::components::{BoxComponent, Spacer, Text};
use pillar_tui::markdown::{DefaultTextStyle, Markdown};
use pillar_tui::tui::Component;

/// A branch summary message (upstream `BranchSummaryMessageComponent`).
pub struct BranchSummaryMessageComponent {
    box_component: BoxComponent,
    expanded: bool,
    message: BranchSummaryMessage,
    markdown_theme: MarkdownThemeFactory,
}

impl BranchSummaryMessageComponent {
    pub fn new(message: BranchSummaryMessage, markdown_theme: Option<MarkdownThemeFactory>) -> Self {
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
        let label = theme_handle.fg(
            "customMessageLabel",
            "\u{1b}[1m[branch]\u{1b}[22m",
        );
        self.box_component.add_child(Box::new(Text::new(&label, 0, 0)));
        self.box_component.add_child(Box::new(Spacer::new(1)));

        if self.expanded {
            let header = "**Branch Summary**\n\n";
            self.box_component.add_child(Box::new(Markdown::new(
                &format!("{header}{}", self.message.summary),
                0,
                0,
                (self.markdown_theme)(),
                Some(DefaultTextStyle {
                    color: Some(Box::new(|text: &str| {
                        theme().fg("customMessageText", text)
                    })),
                    ..Default::default()
                }),
                Default::default(),
            )));
        } else {
            let hint = format!(
                "{}{}{}",
                theme_handle.fg("customMessageText", "Branch summary ("),
                theme_handle.fg("dim", &key_text("app.tools.expand")),
                theme_handle.fg("customMessageText", " to expand)")
            );
            self.box_component.add_child(Box::new(Text::new(&hint, 0, 0)));
        }
    }
}

impl Component for BranchSummaryMessageComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.box_component.render(width)
    }

    fn invalidate(&mut self) {
        self.update_display();
    }
}
