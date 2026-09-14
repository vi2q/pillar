//! Port of components/bash-execution.ts: the `!` bash execution block with
//! streaming output, preview truncation and a running loader.
//!
//! divergence: upstream rebuilds a container of children on every update and
//! caches the collapsed preview inside a closure component; the port composes
//! the same lines in `render` (holding the loader owned so it can be ticked
//! and stopped) and caches the preview per width in a field.

use crate::core::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationOptions, TruncationResult, strip_ansi,
    truncate_tail,
};
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::{key_hint, key_text};
use crate::modes::interactive::components::visual_truncate::truncate_to_visual_lines;
use crate::modes::interactive::theme::{Theme, theme};
use pillar_tui::components::Text;
use pillar_tui::loaders::Loader;
use pillar_tui::tui::Component;

/// Preview line limit when collapsed (upstream `PREVIEW_LINES`).
const PREVIEW_LINES: usize = 20;

/// The execution status (upstream the `status` union).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BashExecutionStatus {
    #[default]
    Running,
    Complete,
    Cancelled,
    Error,
}

/// A bash command block (upstream `BashExecutionComponent`).
pub struct BashExecutionComponent {
    command: String,
    output_lines: Vec<String>,
    status: BashExecutionStatus,
    exit_code: Option<i32>,
    loader: Loader,
    truncation_result: Option<TruncationResult>,
    full_output_path: Option<String>,
    expanded: bool,
    exclude_from_context: bool,
    cached_preview: Option<(usize, Vec<String>)>,
}

impl BashExecutionComponent {
    pub fn new(theme_obj: &Theme, command: &str, exclude_from_context: bool) -> Self {
        // Excluded-from-context commands (the `!!` prefix) use the dim border.
        let color_key = if exclude_from_context { "dim" } else { "bashMode" };
        let loader_theme = theme_obj.clone();
        let message_theme = theme_obj.clone();
        let loader = Loader::new(
            Box::new(move |spinner: &str| loader_theme.fg(color_key, spinner)),
            Box::new(move |text: &str| message_theme.fg("muted", text)),
            &format!(
                "Running... ({} to cancel)",
                key_text("tui.select.cancel")
            ),
            None,
        );
        Self {
            command: command.to_string(),
            output_lines: Vec::new(),
            status: BashExecutionStatus::Running,
            exit_code: None,
            loader,
            truncation_result: None,
            full_output_path: None,
            expanded: false,
            exclude_from_context,
            cached_preview: None,
        }
    }

    /// Expand (full output) or collapse (preview) the block.
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.cached_preview = None;
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    pub fn status(&self) -> BashExecutionStatus {
        self.status
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// The loader inside the block (the host advances it with `tick`).
    pub fn loader_mut(&mut self) -> &mut Loader {
        &mut self.loader
    }

    /// Append a streamed chunk: ANSI stripped, line endings normalized, an
    /// unterminated trailing line continued (upstream `appendOutput`).
    pub fn append_output(&mut self, chunk: &str) {
        let clean = strip_ansi(chunk).replace("\r\n", "\n").replace('\r', "\n");
        let new_lines: Vec<String> = clean.split('\n').map(str::to_string).collect();
        if !self.output_lines.is_empty() && !new_lines.is_empty() {
            if let Some(last) = self.output_lines.last_mut() {
                last.push_str(&new_lines[0]);
            }
            self.output_lines.extend(new_lines[1..].iter().cloned());
        } else {
            self.output_lines.extend(new_lines);
        }
        self.cached_preview = None;
    }

    /// Mark the command finished (upstream `setComplete`).
    pub fn set_complete(
        &mut self,
        exit_code: Option<i32>,
        cancelled: bool,
        truncation_result: Option<TruncationResult>,
        full_output_path: Option<String>,
    ) {
        self.exit_code = exit_code;
        self.status = if cancelled {
            BashExecutionStatus::Cancelled
        } else if matches!(exit_code, Some(code) if code != 0) {
            BashExecutionStatus::Error
        } else {
            BashExecutionStatus::Complete
        };
        self.truncation_result = truncation_result;
        self.full_output_path = full_output_path;
        self.loader.stop();
        self.cached_preview = None;
    }

    /// The raw output (upstream `getOutput`).
    pub fn get_output(&self) -> String {
        self.output_lines.join("\n")
    }

    /// The executed command (upstream `getCommand`).
    pub fn get_command(&self) -> &str {
        &self.command
    }

    /// The output after the context truncation limits (upstream's
    /// `contextTruncation`).
    fn context_truncation(&self) -> TruncationResult {
        truncate_tail(
            &self.output_lines.join("\n"),
            TruncationOptions {
                max_lines: Some(DEFAULT_MAX_LINES),
                max_bytes: Some(DEFAULT_MAX_BYTES),
            },
        )
    }

    fn border(&self, width: usize) -> Vec<String> {
        let color_key = if self.exclude_from_context {
            "dim"
        } else {
            "bashMode"
        };
        let theme_handle = theme();
        DynamicBorder::with_color(Box::new(move |text: &str| {
            theme_handle.fg(color_key, text)
        }))
        .render(width)
    }
}

impl Component for BashExecutionComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme_handle = theme();
        let mut lines: Vec<String> = Vec::new();
        // Spacer + top border.
        lines.push(String::new());
        lines.extend(self.border(width));

        // Upstream's `updateDisplay` always colours the header with `bashMode`
        // (only the constructor uses the dim colour for `!!`), which the port
        // keeps for parity.
        let header = format!(
            "$ {}",
            self.command
        );
        let header_text = theme_handle.fg("bashMode", &theme_handle.bold(&header));
        let mut header_component = Text::new(&header_text, 1, 0);
        lines.extend(header_component.render(width));

        // Output.
        let context_truncation = self.context_truncation();
        let available_lines: Vec<String> = if context_truncation.content.is_empty() {
            Vec::new()
        } else {
            context_truncation
                .content
                .split('\n')
                .map(str::to_string)
                .collect()
        };
        let preview_logical_lines: Vec<String> = if available_lines.len() > PREVIEW_LINES {
            available_lines[available_lines.len() - PREVIEW_LINES..].to_vec()
        } else {
            available_lines.clone()
        };
        let hidden_line_count = available_lines.len().saturating_sub(preview_logical_lines.len());

        if !available_lines.is_empty() {
            if self.expanded {
                let display_text = available_lines
                    .iter()
                    .map(|line| theme_handle.fg("muted", line))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut text = Text::new(&format!("\n{display_text}"), 1, 0);
                lines.extend(text.render(width));
            } else {
                // Width-aware preview truncation, cached per width (upstream
                // caches inside the closure component).
                let styled = preview_logical_lines
                    .iter()
                    .map(|line| theme_handle.fg("muted", line))
                    .collect::<Vec<_>>()
                    .join("\n");
                let styled_input = format!("\n{styled}");
                let cached = match &self.cached_preview {
                    Some((cached_width, cached_lines)) if *cached_width == width => {
                        cached_lines.clone()
                    }
                    _ => {
                        let result = truncate_to_visual_lines(&styled_input, PREVIEW_LINES, width, 1);
                        self.cached_preview = Some((width, result.visual_lines.clone()));
                        result.visual_lines
                    }
                };
                lines.extend(cached);
            }
        }

        // Loader or status.
        if self.status == BashExecutionStatus::Running {
            lines.extend(self.loader.render(width));
        } else {
            let mut status_parts: Vec<String> = Vec::new();
            if hidden_line_count > 0 {
                if self.expanded {
                    status_parts.push(format!(
                        "{}{}{}",
                        theme_handle.fg("muted", "("),
                        key_hint("app.tools.expand", "to collapse"),
                        theme_handle.fg("muted", ")")
                    ));
                } else {
                    status_parts.push(format!(
                        "{}{}{}",
                        theme_handle.fg("muted", &format!("... {hidden_line_count} more lines (")),
                        key_hint("app.tools.expand", "to expand"),
                        theme_handle.fg("muted", ")")
                    ));
                }
            }

            match self.status {
                BashExecutionStatus::Cancelled => {
                    status_parts.push(theme_handle.fg("warning", "(cancelled)"));
                }
                BashExecutionStatus::Error => {
                    status_parts.push(theme_handle.fg(
                        "error",
                        &format!("(exit {})", self.exit_code.unwrap_or_default()),
                    ));
                }
                _ => {}
            }

            let was_truncated = self
                .truncation_result
                .as_ref()
                .map(|result| result.truncated)
                .unwrap_or(false)
                || context_truncation.truncated;
            if was_truncated {
                if let Some(full_output_path) = self.full_output_path.as_deref() {
                    status_parts.push(theme_handle.fg(
                        "warning",
                        &format!("Output truncated. Full output: {full_output_path}"),
                    ));
                }
            }

            if !status_parts.is_empty() {
                let mut text = Text::new(&format!("\n{}", status_parts.join("\n")), 1, 0);
                lines.extend(text.render(width));
            }
        }

        lines.extend(self.border(width));
        lines
    }

    fn invalidate(&mut self) {
        self.cached_preview = None;
    }
}
