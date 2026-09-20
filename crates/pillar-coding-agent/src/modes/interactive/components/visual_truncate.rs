//! Port of components/visual-truncate.ts: truncate text to a maximum number of
//! *visual* lines (line wrapping included). Shared by tool-execution and
//! bash-execution.

use pillar_tui::components::Text;
use pillar_tui::tui::Component;

/// The rendered lines plus how many were hidden (upstream
/// `VisualTruncateResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualTruncateResult {
    pub visual_lines: Vec<String>,
    pub skipped_count: usize,
}

/// Keep the last `max_visual_lines` rendered lines of `text`.
///
/// `padding_x` is the `Text` component's horizontal padding: use 0 when the
/// result goes into a `Box` (it pads itself) and 1 when it goes into a plain
/// container.
pub fn truncate_to_visual_lines(
    text: &str,
    max_visual_lines: usize,
    width: usize,
    padding_x: usize,
) -> VisualTruncateResult {
    if text.is_empty() {
        return VisualTruncateResult {
            visual_lines: Vec::new(),
            skipped_count: 0,
        };
    }

    let mut temp_text = Text::new(text, padding_x, 0);
    let all_visual_lines: Vec<String> = Component::render(&mut temp_text, width)
        .iter()
        .map(|line| line.to_string())
        .collect();

    if all_visual_lines.len() <= max_visual_lines {
        return VisualTruncateResult {
            visual_lines: all_visual_lines,
            skipped_count: 0,
        };
    }

    let skipped_count = all_visual_lines.len() - max_visual_lines;
    let truncated = all_visual_lines[skipped_count..].to_vec();
    VisualTruncateResult {
        visual_lines: truncated,
        skipped_count,
    }
}
