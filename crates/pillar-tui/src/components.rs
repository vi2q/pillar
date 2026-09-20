//! Port of packages/tui/src/components: Text, Spacer, Box, and
//! TruncatedText (pi v0.84.3) — the simple render components over the
//! text-utils core.
//!
//! divergence: upstream components implement a `Component` trait with
//! `invalidate` hooks against a shared TUI render loop; the port models
//! components as plain structs with `render(width) -> Vec<String>` and
//! internal caches.

type BgFn = Box<dyn Fn(&str) -> String + Send + Sync>;

use std::sync::Arc;

use crate::text_utils::{
    apply_background_to_line, truncate_to_width, visible_width, wrap_text_with_ansi,
};
use crate::tui::{Component, RenderLines, empty_lines, render_lines};

// ============================================================================
// Text
// ============================================================================}

/// Multi-line text with word wrapping (upstream `Text`).
#[derive(Default)]
pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Option<BgFn>,
    cache: Option<(String, usize, RenderLines)>,
}

impl Text {
    pub fn new(text: &str, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
            bg_fn: None,
            cache: None,
        }
    }

    pub fn with_bg(
        text: &str,
        padding_x: usize,
        padding_y: usize,
        bg_fn: Box<dyn Fn(&str) -> String + Send + Sync>,
    ) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
            bg_fn: Some(bg_fn),
            cache: None,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cache = None;
    }

    pub fn set_bg_fn(&mut self, bg_fn: Option<BgFn>) {
        self.bg_fn = bg_fn;
        self.cache = None;
    }

    pub fn invalidate(&mut self) {
        self.cache = None;
    }

    pub fn render(&mut self, width: usize) -> RenderLines {
        if let Some((cached_text, cached_width, lines)) = &self.cache
            && cached_text == &self.text
            && *cached_width == width
        {
            return Arc::clone(lines);
        }
        if self.text.trim().is_empty() {
            let empty: RenderLines = render_lines(Vec::new());
            self.cache = Some((self.text.clone(), width, Arc::clone(&empty)));
            return empty;
        }

        // Tabs become 3 spaces.
        let normalized_text = self.text.replace('\t', "   ");
        // Reduce margins when necessary so content and padding fit.
        let padding_x = self.padding_x.min((width.saturating_sub(1)) / 2);
        let content_width = (width.saturating_sub(padding_x * 2)).max(1);
        let wrapped_lines = wrap_text_with_ansi(&normalized_text, content_width);

        let left_margin = " ".repeat(padding_x);
        let right_margin = " ".repeat(padding_x);
        let mut content_lines: Vec<String> = Vec::new();
        for line in wrapped_lines {
            let line_with_margins = format!("{left_margin}{line}{right_margin}");
            match &self.bg_fn {
                Some(bg_fn) => {
                    content_lines.push(apply_background_to_line(
                        &line_with_margins,
                        width,
                        bg_fn.as_ref(),
                    ));
                }
                None => {
                    let visible_len = visible_width(&line_with_margins);
                    let padding_needed = width.saturating_sub(visible_len);
                    content_lines
                        .push(format!("{line_with_margins}{}", " ".repeat(padding_needed)));
                }
            }
        }

        let empty_line = " ".repeat(width);
        let mut result = Vec::new();
        for _ in 0..self.padding_y {
            let line = match &self.bg_fn {
                Some(bg_fn) => apply_background_to_line(&empty_line, width, bg_fn.as_ref()),
                None => empty_line.clone(),
            };
            result.push(line);
        }
        result.extend(content_lines);
        for _ in 0..self.padding_y {
            let line = match &self.bg_fn {
                Some(bg_fn) => apply_background_to_line(&empty_line, width, bg_fn.as_ref()),
                None => empty_line.clone(),
            };
            result.push(line);
        }

        let lines = if result.is_empty() {
            render_lines(vec![String::new()])
        } else {
            render_lines(result)
        };
        self.cache = Some((self.text.clone(), width, Arc::clone(&lines)));
        lines
    }
}

// ============================================================================
// Spacer
// ============================================================================}

/// Renders empty lines (upstream `Spacer`).
#[derive(Debug, Clone, Default)]
pub struct Spacer {
    lines: usize,
}

impl Spacer {
    pub fn new(lines: usize) -> Self {
        Self { lines }
    }

    pub fn set_lines(&mut self, lines: usize) {
        self.lines = lines;
    }

    pub fn render(&self, _width: usize) -> RenderLines {
        render_lines(vec![String::new(); self.lines])
    }
}

// ============================================================================
// Box
// ============================================================================}

/// A container applying padding and background to children (upstream
/// `Box`).
pub struct BoxComponent {
    children: Vec<Box<dyn Component>>,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Option<BgFn>,
    cache: Option<CacheEntry>,
}

struct CacheEntry {
    child_frames: Vec<RenderLines>,
    width: usize,
    bg_sample: Option<String>,
    lines: RenderLines,
}

impl BoxComponent {
    pub fn new(padding_x: usize, padding_y: usize) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg_fn: None,
            cache: None,
        }
    }

    /// Add a child component (upstream `addChild`).
    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
        self.cache = None;
    }

    pub fn clear(&mut self) {
        self.children.clear();
        self.cache = None;
    }

    /// Set the background; the cache detects bg changes by sampling.
    pub fn set_bg_fn(&mut self, bg_fn: Option<BgFn>) {
        self.bg_fn = bg_fn;
    }

    pub fn invalidate(&mut self) {
        self.cache = None;
    }

    fn apply_bg(&self, line: &str, width: usize) -> String {
        let vis_len = visible_width(line);
        let pad_needed = width.saturating_sub(vis_len);
        let padded = format!("{line}{}", " ".repeat(pad_needed));
        match &self.bg_fn {
            Some(bg_fn) => apply_background_to_line(&padded, width, bg_fn.as_ref()),
            None => padded,
        }
    }

    pub fn render(&mut self, width: usize) -> RenderLines {
        if self.children.is_empty() {
            return empty_lines();
        }
        let content_width = (width.saturating_sub(self.padding_x * 2)).max(1);
        let left_pad = " ".repeat(self.padding_x);

        // The children's frames are compared by pointer below: a child that
        // did not change hands back the same `Arc`, so the background/padding
        // pass is skipped without re-wrapping anything.
        let mut child_frames: Vec<RenderLines> = Vec::with_capacity(self.children.len());
        for child in self.children.iter_mut() {
            child_frames.push(child.render(content_width));
        }
        if child_frames.iter().all(|lines| lines.is_empty()) {
            return empty_lines();
        }

        let bg_sample = self.bg_fn.as_ref().map(|bg_fn| bg_fn("test"));

        if let Some(cache) = &self.cache {
            let same_children = cache.child_frames.len() == child_frames.len()
                && cache
                    .child_frames
                    .iter()
                    .zip(child_frames.iter())
                    .all(|(old, new)| Arc::ptr_eq(old, new));
            if cache.width == width && cache.bg_sample == bg_sample && same_children {
                return Arc::clone(&cache.lines);
            }
        }

        let mut child_lines: Vec<String> = Vec::new();
        for lines in &child_frames {
            for line in lines.iter() {
                child_lines.push(format!("{left_pad}{line}"));
            }
        }

        let mut result: Vec<String> = Vec::new();
        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }
        for line in &child_lines {
            result.push(self.apply_bg(line, width));
        }
        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }

        let lines = render_lines(result);
        self.cache = Some(CacheEntry {
            child_frames,
            width,
            bg_sample,
            lines: Arc::clone(&lines),
        });
        lines
    }
}

// ============================================================================
// TruncatedText
// ============================================================================}

/// Single-line text truncated to the viewport width (upstream
/// `TruncatedText`).
#[derive(Debug, Clone, Default)]
pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
}

impl TruncatedText {
    pub fn new(text: &str, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
        }
    }

    pub fn render(&self, width: usize) -> RenderLines {
        let mut result: Vec<String> = Vec::new();
        let empty_line = " ".repeat(width);
        for _ in 0..self.padding_y {
            result.push(empty_line.clone());
        }

        let available_width = (width.saturating_sub(self.padding_x * 2)).max(1);
        // Take only the first line (stop at newline).
        let single_line_text = self.text.split('\n').next().unwrap_or("");
        let display_text = truncate_to_width(single_line_text, available_width, "...", false);

        let left_padding = " ".repeat(self.padding_x);
        let right_padding = " ".repeat(self.padding_x);
        let line_with_padding = format!("{left_padding}{display_text}{right_padding}");
        let line_visible_width = visible_width(&line_with_padding);
        let padding_needed = width.saturating_sub(line_visible_width);
        result.push(format!("{line_with_padding}{}", " ".repeat(padding_needed)));

        for _ in 0..self.padding_y {
            result.push(empty_line.clone());
        }
        render_lines(result)
    }
}

// ============================================================================
// Image (upstream components/image.ts)
// ============================================================================

/// Fallback styling for terminals without image support (upstream
/// `ImageTheme`).
pub type ImageTheme = Box<dyn Fn(&str) -> String + Send>;

/// Image options (upstream `ImageOptions`).
#[derive(Debug, Clone, Default)]
pub struct ImageOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub filename: Option<String>,
    /// Kitty image ID to reuse (animations/updates).
    pub image_id: Option<u32>,
}

/// Renders an image through the terminal's image protocol, falling back to a
/// styled placeholder (upstream `Image`).
pub struct Image {
    base64_data: String,
    mime_type: String,
    dimensions: crate::terminal_image::ImageDimensions,
    theme: ImageTheme,
    options: ImageOptions,
    image_id: Option<u32>,
    cached_lines: Option<RenderLines>,
    cached_width: Option<usize>,
}

impl Image {
    pub fn new(
        base64_data: &str,
        mime_type: &str,
        theme: ImageTheme,
        options: ImageOptions,
        dimensions: Option<crate::terminal_image::ImageDimensions>,
    ) -> Self {
        let dimensions = dimensions
            .or_else(|| crate::terminal_image::get_image_dimensions(base64_data, mime_type))
            .unwrap_or(crate::terminal_image::ImageDimensions {
                width_px: 800,
                height_px: 600,
            });
        let image_id = options.image_id;
        Self {
            base64_data: base64_data.to_string(),
            mime_type: mime_type.to_string(),
            dimensions,
            theme,
            options,
            image_id,
            cached_lines: None,
            cached_width: None,
        }
    }

    /// The Kitty image ID in use, if any (upstream `getImageId`).
    pub fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    /// Drop cached lines (upstream `invalidate`).
    pub fn invalidate(&mut self) {
        self.cached_lines = None;
        self.cached_width = None;
    }

    pub fn render(&mut self, width: usize) -> RenderLines {
        if let (Some(lines), Some(cached_width)) = (&self.cached_lines, self.cached_width)
            && cached_width == width
        {
            return Arc::clone(lines);
        }

        let max_width = width
            .saturating_sub(2)
            .max(1)
            .min(self.options.max_width_cells.unwrap_or(60));
        let cell_dimensions = crate::terminal_image::get_cell_dimensions();
        let default_max_height = ((max_width * cell_dimensions.width_px as usize)
            .div_ceil(cell_dimensions.height_px.max(1) as usize))
        .max(1);
        let max_height = self.options.max_height_cells.unwrap_or(default_max_height);

        let capabilities = crate::terminal_image::get_capabilities();
        let fallback = || {
            let text = crate::terminal_image::image_fallback(
                &self.mime_type,
                Some(self.dimensions),
                self.options.filename.as_deref(),
                crate::terminal_image::get_capabilities().hyperlinks,
            );
            vec![truncate_to_width(&(self.theme)(&text), width, "", false)]
        };

        let lines = match capabilities.images {
            None => fallback(),
            Some(protocol) => {
                if protocol == crate::terminal_image::ImageProtocol::Kitty
                    && self.image_id.is_none()
                {
                    self.image_id = Some(crate::terminal_image::allocate_image_id());
                }
                let rendered = crate::terminal_image::render_image(
                    &self.base64_data,
                    self.dimensions,
                    crate::terminal_image::ImageRenderOptions {
                        max_width_cells: Some(max_width),
                        max_height_cells: Some(max_height),
                        image_id: self.image_id,
                        move_cursor: Some(false),
                        ..Default::default()
                    },
                );
                match rendered {
                    Some(rendered) => {
                        if rendered.image_id.is_some() {
                            self.image_id = rendered.image_id;
                        }
                        if protocol == crate::terminal_image::ImageProtocol::Kitty {
                            // C=1 keeps the cursor in place.
                            let mut lines = vec![rendered.sequence];
                            for _ in 0..rendered.rows.saturating_sub(1) {
                                lines.push(String::new());
                            }
                            lines
                        } else {
                            // iTerm2: the first rows are blank; the last line
                            // moves up, draws, and leaves the cursor at the
                            // image's bottom row.
                            let row_offset = rendered.rows.saturating_sub(1);
                            let mut lines = vec![String::new(); row_offset];
                            let move_up = if row_offset > 0 {
                                format!("\u{1b}[{row_offset}A")
                            } else {
                                String::new()
                            };
                            lines.push(move_up + &rendered.sequence);
                            lines
                        }
                    }
                    None => fallback(),
                }
            }
        };

        let lines = render_lines(lines);
        self.cached_lines = Some(Arc::clone(&lines));
        self.cached_width = Some(width);
        lines
    }
}

// ============================================================================
// Component impls (upstream these classes implement `Component`)
// ============================================================================

impl Component for Text {
    fn render(&mut self, width: usize) -> RenderLines {
        Text::render(self, width)
    }

    fn invalidate(&mut self) {
        Text::invalidate(self);
    }
}

impl Component for Spacer {
    fn render(&mut self, width: usize) -> RenderLines {
        Spacer::render(self, width)
    }
}

impl Component for BoxComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        BoxComponent::render(self, width)
    }

    fn invalidate(&mut self) {
        BoxComponent::invalidate(self);
    }
}

impl Component for TruncatedText {
    fn render(&mut self, width: usize) -> RenderLines {
        TruncatedText::render(self, width)
    }
}

impl Component for Image {
    fn render(&mut self, width: usize) -> RenderLines {
        Image::render(self, width)
    }

    fn invalidate(&mut self) {
        Image::invalidate(self);
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}
