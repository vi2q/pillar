//! Port of packages/tui/src/components: Text, Spacer, Box, and
//! TruncatedText (pi v0.84.3) — the simple render components over the
//! text-utils core.
//!
//! divergence: upstream components implement a `Component` trait with
//! `invalidate` hooks against a shared TUI render loop; the port models
//! components as plain structs with `render(width) -> Vec<String>` and
//! internal caches.

type BgFn = Box<dyn Fn(&str) -> String + Send + Sync>;

use crate::text_utils::{
    apply_background_to_line, truncate_to_width, visible_width, wrap_text_with_ansi,
};

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
    cache: Option<(String, usize, Vec<String>)>,
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

    pub fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((cached_text, cached_width, lines)) = &self.cache {
            if cached_text == &self.text && *cached_width == width {
                return lines.clone();
            }
        }
        if self.text.trim().is_empty() {
            self.cache = Some((self.text.clone(), width, Vec::new()));
            return Vec::new();
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

        self.cache = Some((self.text.clone(), width, result.clone()));
        if result.is_empty() {
            vec![String::new()]
        } else {
            result
        }
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

    pub fn render(&self, _width: usize) -> Vec<String> {
        vec![String::new(); self.lines]
    }
}

// ============================================================================
// Box
// ============================================================================}

/// A container applying padding and background to children (upstream
/// `Box`). Children are function-driven since the port's components have
/// heterogeneous types: each child is a closure receiving the content
/// width.
type ChildRender = Box<dyn Fn(usize) -> Vec<String> + Send>;

pub struct BoxComponent {
    children: Vec<Box<dyn Fn(usize) -> Vec<String> + Send>>,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Option<BgFn>,
    cache: Option<CacheEntry>,
}

struct CacheEntry {
    child_lines: Vec<String>,
    width: usize,
    bg_sample: Option<String>,
    lines: Vec<String>,
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

    /// Add a child as a render closure (upstream `addChild` with a
    /// `Component` instance).
    pub fn add_child(&mut self, child: ChildRender) {
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

    pub fn render(&mut self, width: usize) -> Vec<String> {
        if self.children.is_empty() {
            return Vec::new();
        }
        let content_width = (width.saturating_sub(self.padding_x * 2)).max(1);
        let left_pad = " ".repeat(self.padding_x);

        let mut child_lines: Vec<String> = Vec::new();
        for child in &self.children {
            for line in child(content_width) {
                child_lines.push(format!("{left_pad}{line}"));
            }
        }
        if child_lines.is_empty() {
            return Vec::new();
        }

        let bg_sample = self.bg_fn.as_ref().map(|bg_fn| bg_fn("test"));

        if let Some(cache) = &self.cache {
            if cache.width == width
                && cache.bg_sample == bg_sample
                && cache.child_lines == child_lines
            {
                return cache.lines.clone();
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

        self.cache = Some(CacheEntry {
            child_lines,
            width,
            bg_sample,
            lines: result.clone(),
        });
        result
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

    pub fn render(&self, width: usize) -> Vec<String> {
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
        result
    }
}
