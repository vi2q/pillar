//! Port of components/skill-invocation-message.ts: a skill block with
//! collapsed/expanded state.

use crate::core::agent_session::ParsedSkillBlock;
use crate::modes::interactive::components::keybinding_hints::key_text;
use crate::modes::interactive::components::markdown_transform::{
    MarkdownThemeFactory, default_markdown_theme_factory,
};
use crate::modes::interactive::theme::theme;
use pillar_tui::components::{BoxComponent, Text};
use pillar_tui::markdown::{DefaultTextStyle, Markdown};
use pillar_tui::tui::Component;

/// A skill invocation message (upstream `SkillInvocationMessageComponent`).
pub struct SkillInvocationMessageComponent {
    box_component: BoxComponent,
    expanded: bool,
    skill_block: ParsedSkillBlock,
    markdown_theme: MarkdownThemeFactory,
}

impl SkillInvocationMessageComponent {
    pub fn new(skill_block: ParsedSkillBlock, markdown_theme: Option<MarkdownThemeFactory>) -> Self {
        let mut component = Self {
            box_component: BoxComponent::new(1, 1),
            expanded: false,
            skill_block,
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

    pub fn skill_block(&self) -> &ParsedSkillBlock {
        &self.skill_block
    }

    fn update_display(&mut self) {
        self.box_component.clear();
        let theme_handle = theme();
        if self.expanded {
            let label = theme_handle.fg("customMessageLabel", "\u{1b}[1m[skill]\u{1b}[22m");
            self.box_component.add_child(Box::new(Text::new(&label, 0, 0)));
            let header = format!("**{}**\n\n", self.skill_block.name);
            self.box_component.add_child(Box::new(Markdown::new(
                &format!("{header}{}", self.skill_block.content),
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
            let line = format!(
                "{}{}{}",
                theme_handle.fg("customMessageLabel", "\u{1b}[1m[skill]\u{1b}[22m "),
                theme_handle.fg("customMessageText", &self.skill_block.name),
                theme_handle.fg(
                    "dim",
                    &format!(" ({} to expand)", key_text("app.tools.expand"))
                )
            );
            self.box_component.add_child(Box::new(Text::new(&line, 0, 0)));
        }
    }
}

impl Component for SkillInvocationMessageComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.box_component.render(width)
    }

    fn invalidate(&mut self) {
        self.update_display();
    }
}
