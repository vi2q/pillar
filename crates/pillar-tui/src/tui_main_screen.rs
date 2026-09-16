//! Port of packages/tui/src/tui-main-screen.ts (pi v0.84.3): the
//! [`TuiMainScreen`] renderer — differential rendering into the terminal's
//! main screen and scrollback.
//!
//! Reuses the already-ported algorithm pieces: [`crate::main_screen`]
//! (`decide_render`, `changed_range`, `is_append_start`, the kitty-image
//! helpers), [`crate::editor_autocomplete::BoundedTerminalWriter`] (upstream
//! declares it in this module), and [`crate::overlay`] compositing.
//!
//! divergences:
//! - upstream writes a crash file on width overflow and optional debug logs
//!   (`PILLAR_DEBUG_REDRAW`, `PILLAR_TUI_DEBUG`); the port returns `Err` with the same
//!   message and writes no files.
//! - frames are built as bounded chunks and written afterwards (Rust borrow
//!   rules; the chunking itself matches upstream).

use crate::editor_autocomplete::BoundedTerminalWriter;
use crate::main_screen::{
    MainScreenRenderState, RenderDecision, changed_range, decide_render,
    delete_changed_kitty_images, delete_kitty_images, expand_changed_range_for_kitty_images,
    extract_kitty_image_ids, get_kitty_image_reserved_rows,
};
use crate::overlay::{
    ResolvedOverlayLayout, apply_line_resets, composite_overlays, extract_cursor_position,
    prepare_overlay,
};
use crate::process_terminal::Terminal;
use crate::terminal_image::is_image_line;
use crate::text_utils::visible_width;
use crate::tui::{TuiBase, TuiMode, TuiStopOptions};

const SYNC_START: &str = "\u{1b}[?2026h";
const SYNC_END: &str = "\u{1b}[?2026l";
const CLEAR_SCREEN: &str = "\u{1b}[2J\u{1b}[H\u{1b}[3J";

/// Render state captured/restored across renderer swaps (upstream
/// `TuiMainScreenRenderState`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiMainScreenRenderState {
    pub previous_lines: Vec<String>,
    pub previous_width: usize,
    pub previous_height: usize,
    pub cursor_row: usize,
    pub hardware_cursor_row: usize,
    pub max_lines_rendered: usize,
    pub previous_viewport_top: usize,
}

/// Renders into the main screen with differential updates (upstream
/// `TuiMainScreen`).
pub struct TuiMainScreen {
    base: TuiBase,
    previous_lines: Vec<String>,
    previous_kitty_image_ids: Vec<u32>,
    previous_width: usize,
    previous_height: usize,
    cursor_row: usize,
    hardware_cursor_row: usize,
    max_lines_rendered: usize,
    previous_viewport_top: usize,
}

impl TuiMainScreen {
    pub fn new(terminal: Box<dyn Terminal>) -> Self {
        Self {
            base: TuiBase::new(terminal, TuiMode::Regular),
            previous_lines: Vec::new(),
            previous_kitty_image_ids: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            cursor_row: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 0,
            previous_viewport_top: 0,
        }
    }

    pub fn base(&self) -> &TuiBase {
        &self.base
    }

    pub fn base_mut(&mut self) -> &mut TuiBase {
        &mut self.base
    }

    pub fn capture_render_state(&self) -> TuiMainScreenRenderState {
        TuiMainScreenRenderState {
            previous_lines: self.previous_lines.clone(),
            previous_width: self.previous_width,
            previous_height: self.previous_height,
            cursor_row: self.cursor_row,
            hardware_cursor_row: self.hardware_cursor_row,
            max_lines_rendered: self.max_lines_rendered,
            previous_viewport_top: self.previous_viewport_top,
        }
    }

    pub fn restore_render_state(&mut self, state: &TuiMainScreenRenderState) {
        // Image lines are dropped: kitty placements do not survive a renderer
        // swap (upstream clears the tracked ids for the same reason).
        self.previous_lines = state
            .previous_lines
            .iter()
            .map(|line| {
                if is_image_line(line) {
                    String::new()
                } else {
                    line.clone()
                }
            })
            .collect();
        self.previous_kitty_image_ids = Vec::new();
        self.previous_width = state.previous_width;
        self.previous_height = state.previous_height;
        self.cursor_row = state.cursor_row;
        self.hardware_cursor_row = state.hardware_cursor_row;
        self.max_lines_rendered = state.max_lines_rendered;
        self.previous_viewport_top = state.previous_viewport_top;
    }

    /// Drop all incremental render state (upstream `resetRenderState`).
    pub fn reset_render_state(&mut self) {
        self.previous_lines.clear();
        self.previous_width = 0;
        self.previous_height = 0;
        self.cursor_row = 0;
        self.hardware_cursor_row = 0;
        self.max_lines_rendered = 0;
        self.previous_viewport_top = 0;
    }

    /// Leave the cursor after the rendered content (upstream
    /// `beforeTerminalStop`).
    pub fn before_terminal_stop(&mut self, options: TuiStopOptions) {
        if options.preserve_screen || self.previous_lines.is_empty() {
            return;
        }
        let target_row = self.previous_lines.len();
        let line_diff = target_row as isize - self.hardware_cursor_row as isize;
        let mut buffer = String::from(" ");
        if line_diff > 0 {
            buffer.push_str(&format!("\u{1b}[{line_diff}B"));
        } else if line_diff < 0 {
            buffer.push_str(&format!("\u{1b}[{}A", -line_diff));
        }
        buffer.push_str("\r\n");
        self.base.terminal_mut().write(&buffer);
    }

    pub fn start(&mut self) {
        self.base.start();
    }

    pub fn stop(&mut self, options: TuiStopOptions) {
        self.before_terminal_stop(options);
        self.base.stop(options);
    }

    fn collect_kitty_image_ids(&self, lines: &[String]) -> Vec<u32> {
        let mut ids = Vec::new();
        for line in lines {
            for id in extract_kitty_image_ids(line) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids
    }

    fn render_state(&self, has_overlays: bool) -> MainScreenRenderState {
        MainScreenRenderState {
            previous_lines: self.previous_lines.clone(),
            previous_width: self.previous_width,
            previous_height: self.previous_height,
            max_lines_rendered: self.max_lines_rendered,
            previous_viewport_top: self.previous_viewport_top,
            has_overlays,
            clear_on_shrink: self.base.clear_on_shrink(),
            is_termux: is_termux_session(),
        }
    }

    /// Component tree plus overlays (upstream the overlay step in `doRender`).
    fn compose(&mut self, width: usize, height: usize) -> Vec<String> {
        let mut lines = self.base.render(width);
        if !self.base.has_overlay_entries() {
            return lines;
        }
        let ids = self.base.overlay_ids();
        let mut overlays: Vec<(Vec<String>, ResolvedOverlayLayout)> = Vec::new();
        for id in ids {
            let options = self.base.overlay_options(id).cloned().unwrap_or_default();
            if !options.is_visible(width, height) {
                continue;
            }
            let Some(component) = self.base.overlay_component_mut(id) else {
                continue;
            };
            let overlay_lines = component.render(width);
            overlays.push(prepare_overlay(
                Some(&options),
                overlay_lines,
                width,
                height,
            ));
        }
        lines = composite_overlays(lines, &mut overlays, width, height);
        lines
    }

    /// Write a bounded chunk batch to the terminal (upstream flushes the
    /// bounded writer straight to stdout).
    fn write_chunks(&mut self, chunks: Vec<String>) {
        for chunk in chunks {
            self.base.terminal_mut().write(&chunk);
        }
    }

    /// Draw one frame (upstream `doRender`). Returns an error when a rendered
    /// line exceeds the terminal width, after restoring the terminal.
    pub fn do_render(&mut self) -> Result<(), String> {
        if self.base.is_stopped() {
            return Ok(());
        }
        let width = self.base.terminal().columns();
        let height = self.base.terminal().rows();
        // Width/height changes are classified by `decide_render`; the local
        // flag drives only the viewport recomputation below.
        let height_changed = self.previous_height != 0 && self.previous_height != height;

        let previous_buffer_length = if self.previous_height > 0 {
            self.previous_viewport_top + self.previous_height
        } else {
            height
        };
        let mut prev_viewport_top = if height_changed {
            previous_buffer_length.saturating_sub(height)
        } else {
            self.previous_viewport_top
        };
        let mut viewport_top = prev_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;

        let mut new_lines = self.compose(width, height);
        let cursor_pos = extract_cursor_position(&mut new_lines, height);
        new_lines = apply_line_resets(new_lines);

        let decision = decide_render(
            &self.render_state(self.base.has_overlay_entries()),
            &new_lines,
            width,
            height,
        );

        // First render / width / height / clearOnShrink: redraw everything.
        if matches!(
            decision,
            RenderDecision::FirstRender
                | RenderDecision::WidthChanged
                | RenderDecision::HeightChanged
                | RenderDecision::ClearOnShrink
        ) {
            let clear = !matches!(decision, RenderDecision::FirstRender);
            self.full_render(&new_lines, width, height, clear, cursor_pos);
            return Ok(());
        }

        // Compute the precise changed range (the decision tree classifies the
        // frame, the paths below need the indices).
        let (first, last, appended) = changed_range(&self.previous_lines, &new_lines);
        let (first_changed, last_changed) = match (first, last) {
            (Some(first), Some(last)) => {
                let (expanded_first, expanded_last) = expand_changed_range_for_kitty_images(
                    first,
                    last,
                    &self.previous_lines,
                    &new_lines,
                );
                (Some(expanded_first), Some(expanded_last))
            }
            _ => (None, None),
        };
        let append_start = appended
            && first_changed == Some(self.previous_lines.len())
            && first_changed.is_some_and(|first| first > 0);

        // No changes: still reposition the hardware cursor.
        let Some(first_changed) = first_changed else {
            self.position_hardware_cursor(cursor_pos, new_lines.len());
            self.previous_viewport_top = prev_viewport_top;
            self.previous_height = height;
            return Ok(());
        };
        let last_changed = last_changed.unwrap_or(first_changed);

        // Only deletions: clear the extra lines without scrolling.
        if first_changed >= new_lines.len() {
            if self.previous_lines.len() > new_lines.len() {
                let target_row = new_lines.len().saturating_sub(1);
                if target_row < prev_viewport_top {
                    self.full_render(&new_lines, width, height, true, cursor_pos);
                    return Ok(());
                }
                let extra_lines = self.previous_lines.len() - new_lines.len();
                if extra_lines > height {
                    self.full_render(&new_lines, width, height, true, cursor_pos);
                    return Ok(());
                }

                let mut chunks: Vec<String> = Vec::new();
                let mut output =
                    BoundedTerminalWriter::new(|data: &str| chunks.push(data.to_string()));
                output.append(SYNC_START);
                output.append(&delete_changed_kitty_images(
                    first_changed,
                    last_changed,
                    &self.previous_lines,
                ));
                let line_diff = compute_line_diff(
                    target_row,
                    viewport_top,
                    hardware_cursor_row,
                    prev_viewport_top,
                );
                if line_diff > 0 {
                    output.append(&format!("\u{1b}[{line_diff}B"));
                } else if line_diff < 0 {
                    output.append(&format!("\u{1b}[{}A", -line_diff));
                }
                output.append("\r");
                let clear_start_offset = if new_lines.is_empty() { 0 } else { 1 };
                if extra_lines > 0 && clear_start_offset > 0 {
                    output.append(&format!("\u{1b}[{clear_start_offset}B"));
                }
                for index in 0..extra_lines {
                    output.append("\r\u{1b}[2K");
                    if index < extra_lines - 1 {
                        output.append("\u{1b}[1B");
                    }
                }
                let move_back = extra_lines - 1 + clear_start_offset;
                if move_back > 0 {
                    output.append(&format!("\u{1b}[{move_back}A"));
                }
                output.append(SYNC_END);
                output.flush();
                self.write_chunks(chunks);

                self.cursor_row = target_row;
                self.hardware_cursor_row = target_row;
            }
            self.position_hardware_cursor(cursor_pos, new_lines.len());
            self.previous_lines = new_lines;
            self.previous_kitty_image_ids = self.collect_kitty_image_ids(&self.previous_lines);
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = prev_viewport_top;
            return Ok(());
        }

        // Differential render from the first changed line to the last.
        let mut chunks: Vec<String> = Vec::new();
        let mut output = BoundedTerminalWriter::new(|data: &str| chunks.push(data.to_string()));
        output.append(SYNC_START);
        output.append(&delete_changed_kitty_images(
            first_changed,
            last_changed,
            &self.previous_lines,
        ));

        let prev_viewport_bottom = prev_viewport_top + height - 1;
        let move_target_row = if append_start {
            first_changed - 1
        } else {
            first_changed
        };
        if move_target_row > prev_viewport_bottom {
            let current_screen_row = hardware_cursor_row
                .saturating_sub(prev_viewport_top)
                .min(height - 1);
            let move_to_bottom = height - 1 - current_screen_row;
            if move_to_bottom > 0 {
                output.append(&format!("\u{1b}[{move_to_bottom}B"));
            }
            let scroll = move_target_row - prev_viewport_bottom;
            for _ in 0..scroll {
                output.append("\r\n");
            }
            prev_viewport_top += scroll;
            viewport_top += scroll;
            hardware_cursor_row = move_target_row;
        }

        let line_diff = compute_line_diff(
            move_target_row,
            viewport_top,
            hardware_cursor_row,
            prev_viewport_top,
        );
        if line_diff > 0 {
            output.append(&format!("\u{1b}[{line_diff}B"));
        } else if line_diff < 0 {
            output.append(&format!("\u{1b}[{}A", -line_diff));
        }
        output.append(if append_start { "\r\n" } else { "\r" });

        let render_end = last_changed.min(new_lines.len() - 1);
        let mut index = first_changed;
        while index <= render_end {
            if index > first_changed {
                output.append("\r\n");
            }
            let line = &new_lines[index];
            let image_rows = if is_image_line(line) {
                get_kitty_image_reserved_rows(&new_lines, index, render_end)
            } else {
                1
            };
            if image_rows > 1 {
                let image_start_screen_row = index as isize - viewport_top as isize;
                if image_start_screen_row < 0
                    || image_start_screen_row as usize + image_rows > height
                {
                    // Pre-clearing would scroll: fall back to a full redraw.
                    self.full_render(&new_lines, width, height, true, cursor_pos);
                    return Ok(());
                }
                output.append("\u{1b}[2K");
                for _ in 1..image_rows {
                    output.append("\r\n\u{1b}[2K");
                }
                output.append(&format!("\u{1b}[{}A", image_rows - 1));
                output.append(line);
                output.append(&format!("\u{1b}[{}B", image_rows - 1));
                index += image_rows;
                continue;
            }

            output.append("\u{1b}[2K");
            if !is_image_line(line) && visible_width(line) > width {
                // Restore the terminal before surfacing the fault
                // (upstream throws after `stop()`).
                let error = format!(
                    "Rendered line {index} exceeds terminal width ({} > {width}).\n\n\
                     This is likely caused by a custom TUI component not truncating its output.\n\
                     Use visibleWidth() to measure and truncateToWidth() to truncate lines.",
                    visible_width(line)
                );
                self.stop(TuiStopOptions::default());
                return Err(error);
            }
            output.append(line);
            index += 1;
        }

        // Clear lines that no longer exist and move back to the content end.
        let mut final_cursor_row = render_end;
        if self.previous_lines.len() > new_lines.len() {
            if render_end < new_lines.len() - 1 {
                let move_down = new_lines.len() - 1 - render_end;
                output.append(&format!("\u{1b}[{move_down}B"));
                final_cursor_row = new_lines.len() - 1;
            }
            let extra_lines = self.previous_lines.len() - new_lines.len();
            for _ in new_lines.len()..self.previous_lines.len() {
                output.append("\r\n\u{1b}[2K");
            }
            output.append(&format!("\u{1b}[{extra_lines}A"));
        }
        output.append(SYNC_END);
        output.flush();
        self.write_chunks(chunks);

        self.cursor_row = new_lines.len().saturating_sub(1);
        self.hardware_cursor_row = final_cursor_row;
        self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
        self.previous_viewport_top = prev_viewport_top.max(
            (final_cursor_row + 1).saturating_sub(height),
        );
        self.position_hardware_cursor(cursor_pos, new_lines.len());

        self.previous_lines = new_lines;
        self.previous_kitty_image_ids = self.collect_kitty_image_ids(&self.previous_lines);
        self.previous_width = width;
        self.previous_height = height;
        Ok(())
    }

    fn full_render(
        &mut self,
        new_lines: &[String],
        width: usize,
        height: usize,
        clear: bool,
        cursor_pos: Option<(usize, usize)>,
    ) {
        self.base.note_full_redraw();
        let mut chunks: Vec<String> = Vec::new();
        let mut output = BoundedTerminalWriter::new(|data: &str| chunks.push(data.to_string()));
        output.append(SYNC_START);
        if clear {
            for id in self.previous_kitty_image_ids.clone() {
                output.append(&delete_kitty_images(&[id]));
            }
            output.append(CLEAR_SCREEN);
        }
        let mut index = 0;
        while index < new_lines.len() {
            if index > 0 {
                output.append("\r\n");
            }
            let line = &new_lines[index];
            let image_rows = if is_image_line(line) {
                get_kitty_image_reserved_rows(new_lines, index, new_lines.len().saturating_sub(1))
            } else {
                1
            };
            if image_rows > 1 && image_rows <= height {
                for _ in 1..image_rows {
                    output.append("\r\n");
                }
                output.append(&format!("\u{1b}[{}A", image_rows - 1));
                output.append(line);
                output.append(&format!("\u{1b}[{}B", image_rows - 1));
                index += image_rows;
                continue;
            }
            output.append(line);
            index += 1;
        }
        output.append(SYNC_END);
        output.flush();
        self.write_chunks(chunks);

        self.cursor_row = new_lines.len().saturating_sub(1);
        self.hardware_cursor_row = self.cursor_row;
        self.max_lines_rendered = if clear {
            new_lines.len()
        } else {
            self.max_lines_rendered.max(new_lines.len())
        };
        let buffer_length = height.max(new_lines.len());
        self.previous_viewport_top = buffer_length.saturating_sub(height);
        self.position_hardware_cursor(cursor_pos, new_lines.len());
        self.previous_lines = new_lines.to_vec();
        self.previous_kitty_image_ids = self.collect_kitty_image_ids(new_lines);
        self.previous_width = width;
        self.previous_height = height;
    }

    /// Show the hardware cursor at the marker position (upstream
    /// `positionHardwareCursor`).
    fn position_hardware_cursor(&mut self, cursor_pos: Option<(usize, usize)>, total_lines: usize) {
        let Some((row, col)) = cursor_pos else {
            self.base.terminal_mut().hide_cursor();
            return;
        };
        if total_lines == 0 {
            self.base.terminal_mut().hide_cursor();
            return;
        }
        let target_row = row.min(total_lines - 1);
        let target_col = col;

        let row_delta = target_row as isize - self.hardware_cursor_row as isize;
        let mut buffer = String::new();
        if row_delta > 0 {
            buffer.push_str(&format!("\u{1b}[{row_delta}B"));
        } else if row_delta < 0 {
            buffer.push_str(&format!("\u{1b}[{}A", -row_delta));
        }
        // Absolute column, 1-indexed.
        buffer.push_str(&format!("\u{1b}[{}G", target_col + 1));
        if !buffer.is_empty() {
            self.base.terminal_mut().write(&buffer);
        }
        self.hardware_cursor_row = target_row;
        if self.base.show_hardware_cursor() {
            self.base.terminal_mut().show_cursor();
        } else {
            self.base.terminal_mut().hide_cursor();
        }
    }
}

/// Cursor movement between the tracked viewport and the target row
/// (upstream `computeLineDiff`).
fn compute_line_diff(
    target_row: usize,
    viewport_top: usize,
    hardware_cursor_row: usize,
    prev_viewport_top: usize,
) -> isize {
    let current_screen_row = hardware_cursor_row as isize - prev_viewport_top as isize;
    let target_screen_row = target_row as isize - viewport_top as isize;
    target_screen_row - current_screen_row
}

/// Whether the session runs in Termux (upstream `isTermuxSession`).
pub fn is_termux_session() -> bool {
    std::env::var("TERMUX_VERSION").is_ok()
}
