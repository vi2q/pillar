//! Port of packages/tui/src/components/markdown.ts (pi v0.84.3):
//! terminal markdown renderer over pulldown-cmark (standing in for the
//! upstream `marked` parser), with LaTeX extension, lists, blockquotes,
//! code blocks, and width-aware tables.
//!
//! divergences: the token stream comes from pulldown-cmark rather than
//! marked, so the LaTeX inline/block extensions are handled by a
//! pre-pass splitting `$$…$$` / `\[…\]` / `$…$` regions out of the
//! source before parsing (upstream registers marked tokenizer
//! extensions). Streamed partial-closing-fence trimming is applied to
//! the source text for the same effect. Strikethrough uses pulldown's
//! GFM option instead of the strict tokenizer. Image lines and OSC-8
//! hyperlink capabilities stay host-side (links render with the URL
//! fallback path).

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use std::sync::Arc;

use crate::latex::{RenderLatexOptions, render_latex};
use crate::text_utils::{apply_background_to_line, visible_width, wrap_text_with_ansi};
use crate::tui::{RenderLines, render_lines};

/// Styling applied to all text unless overridden (upstream
/// `DefaultTextStyle`). Background color is applied at the padding
/// stage, not here.
/// Foreground color hook type.
pub type ColorFn = dyn Fn(&str) -> String + Send;

#[derive(Default)]
pub struct DefaultTextStyle {
    pub color: Option<Box<ColorFn>>,
    pub bg_color: Option<Box<ColorFn>>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

/// Syntax highlighter hook type.
pub type HighlightCodeFn = dyn Fn(&str, Option<&str>) -> Vec<String> + Send;

/// Source transform hook type.
pub type TransformFn = dyn Fn(&str, usize) -> String + Send;

/// Theme functions for markdown elements (upstream `MarkdownTheme`).
pub struct MarkdownTheme {
    pub heading: Box<dyn Fn(&str) -> String + Send>,
    pub link: Box<dyn Fn(&str) -> String + Send>,
    pub link_url: Box<dyn Fn(&str) -> String + Send>,
    pub code: Box<dyn Fn(&str) -> String + Send>,
    pub code_block: Box<dyn Fn(&str) -> String + Send>,
    pub code_block_border: Box<dyn Fn(&str) -> String + Send>,
    pub quote: Box<dyn Fn(&str) -> String + Send>,
    pub quote_border: Box<dyn Fn(&str) -> String + Send>,
    pub hr: Box<dyn Fn(&str) -> String + Send>,
    pub list_bullet: Box<dyn Fn(&str) -> String + Send>,
    pub bold: Box<dyn Fn(&str) -> String + Send>,
    pub italic: Box<dyn Fn(&str) -> String + Send>,
    pub strikethrough: Box<dyn Fn(&str) -> String + Send>,
    pub underline: Box<dyn Fn(&str) -> String + Send>,
    /// Syntax highlighter for code blocks; falls back to `code_block`.
    pub highlight_code: Option<Box<HighlightCodeFn>>,
    /// Prefix applied to each rendered code block line (default "  ").
    pub code_block_indent: Option<String>,
}

/// Renderer options (upstream `MarkdownOptions`).
#[derive(Default)]
pub struct MarkdownOptions {
    pub preserve_ordered_list_markers: bool,
    pub preserve_backslash_escapes: bool,
    pub transform: Option<Box<TransformFn>>,
    pub render_latex: bool,
}

/// Markdown component (upstream `Markdown`). Keeps (text, width) cache
/// semantics.
pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    theme: MarkdownTheme,
    default_text_style: Option<DefaultTextStyle>,
    options: MarkdownOptions,
    cached_text: Option<String>,
    cached_width: Option<usize>,
    cached_lines: Option<RenderLines>,
}

/// Inline style application context (upstream `InlineStyleContext`).
struct StyleContext<'a> {
    apply_text: Box<dyn Fn(&str) -> String + 'a>,
    style_prefix: String,
}

impl Markdown {
    pub fn new(
        text: &str,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_text_style: Option<DefaultTextStyle>,
        options: MarkdownOptions,
    ) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
            theme,
            default_text_style,
            options,
            cached_text: None,
            cached_width: None,
            cached_lines: None,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.invalidate();
    }

    pub fn invalidate(&mut self) {
        self.cached_text = None;
        self.cached_width = None;
        self.cached_lines = None;
    }

    fn apply_default_style(&self, text: &str) -> String {
        let Some(style) = &self.default_text_style else {
            return text.to_string();
        };
        let mut styled = text.to_string();
        if let Some(color) = &style.color {
            styled = color(&styled);
        }
        if style.bold {
            styled = (self.theme.bold)(&styled);
        }
        if style.italic {
            styled = (self.theme.italic)(&styled);
        }
        if style.strikethrough {
            styled = (self.theme.strikethrough)(&styled);
        }
        if style.underline {
            styled = (self.theme.underline)(&styled);
        }
        styled
    }

    /// Extract the ANSI prefix a style fn emits before its content via a
    /// NUL sentinel (upstream `getStylePrefix`).
    fn style_prefix(f: &dyn Fn(&str) -> String) -> String {
        let sentinel = "\u{0}";
        let styled = f(sentinel);
        styled
            .find(sentinel)
            .map(|index| styled[..index].to_string())
            .unwrap_or_default()
    }

    fn default_style_prefix(&self) -> String {
        let Some(style) = &self.default_text_style else {
            return String::new();
        };
        let sentinel = "\u{0}";
        let mut styled = sentinel.to_string();
        if let Some(color) = &style.color {
            styled = color(&styled);
        }
        if style.bold {
            styled = (self.theme.bold)(&styled);
        }
        if style.italic {
            styled = (self.theme.italic)(&styled);
        }
        if style.strikethrough {
            styled = (self.theme.strikethrough)(&styled);
        }
        if style.underline {
            styled = (self.theme.underline)(&styled);
        }
        styled
            .find(sentinel)
            .map(|index| styled[..index].to_string())
            .unwrap_or_default()
    }

    fn default_context(&self) -> StyleContext<'_> {
        StyleContext {
            apply_text: Box::new(|text: &str| self.apply_default_style(text)),
            style_prefix: self.default_style_prefix(),
        }
    }

    pub fn render(&mut self, width: usize) -> RenderLines {
        if let (Some(cached_lines), Some(cached_text), Some(cached_width)) =
            (&self.cached_lines, &self.cached_text, self.cached_width)
            && *cached_text == self.text
            && cached_width == width
        {
            return Arc::clone(cached_lines);
        }

        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let text = match &self.options.transform {
            Some(transform) => transform(&self.text, content_width),
            None => self.text.clone(),
        };

        if text.trim().is_empty() {
            self.cached_text = Some(self.text.clone());
            self.cached_width = Some(width);
            let empty = render_lines(Vec::new());
            self.cached_lines = Some(Arc::clone(&empty));
            return empty;
        }

        // Tabs → 3 spaces for consistent rendering (upstream).
        let normalized_text = text.replace('\t', "   ");

        let trimmed = trim_partial_closing_fences(&normalized_text);
        let events: Vec<Event> = Parser::new_ext(
            &trimmed,
            Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
        )
        .collect();
        let blocks = group_blocks(events);

        let rendered_lines: Vec<String> = {
            let mut rendered: Vec<String> = Vec::new();
            let context = self.default_context();
            for (index, block) in blocks.iter().enumerate() {
                let next_type = blocks.get(index + 1).map(|b| b.next_kind.as_str());
                let token_lines = self.render_block(block, content_width, next_type, &context);
                rendered.extend(token_lines);
            }
            rendered
        };

        // Wrap lines (no padding, no background yet).
        let mut wrapped_lines: Vec<String> = Vec::new();
        for line in &rendered_lines {
            wrapped_lines.extend(wrap_text_with_ansi(line, content_width));
        }

        // Margins + background.
        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        let bg_fn = self
            .default_text_style
            .as_ref()
            .and_then(|s| s.bg_color.as_ref());
        let mut content_lines: Vec<String> = Vec::new();
        for line in &wrapped_lines {
            let line_with_margins = format!("{left_margin}{line}{right_margin}");
            match bg_fn {
                Some(bg) => content_lines.push(apply_background_to_line(
                    &line_with_margins,
                    width,
                    bg.as_ref(),
                )),
                None => {
                    let visible_len = visible_width(&line_with_margins);
                    let padding = width.saturating_sub(visible_len);
                    content_lines.push(format!("{line_with_margins}{}", " ".repeat(padding)));
                }
            }
        }

        let empty_line = " ".repeat(width);
        let empty_lines: Vec<String> = (0..self.padding_y)
            .map(|_| match bg_fn {
                Some(bg) => apply_background_to_line(&empty_line, width, bg.as_ref()),
                None => empty_line.clone(),
            })
            .collect();

        let mut result = empty_lines;
        result.extend(content_lines);
        result.extend((0..self.padding_y).map(|_| match bg_fn {
            Some(bg) => apply_background_to_line(&empty_line, width, bg.as_ref()),
            None => empty_line.clone(),
        }));

        if result.is_empty() {
            result.push(String::new());
        }
        let lines = render_lines(result);
        self.cached_text = Some(self.text.clone());
        self.cached_width = Some(width);
        self.cached_lines = Some(Arc::clone(&lines));
        lines
    }

    fn render_block(
        &self,
        block: &Block,
        width: usize,
        next_type: Option<&str>,
        context: &StyleContext<'_>,
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        match &block.kind {
            BlockKind::Heading(depth, inline) => {
                let heading_prefix = format!("{} ", "#".repeat(*depth as usize));
                let heading_style = |text: &str| {
                    if *depth >= 2 {
                        (self.theme.heading)(&(self.theme.bold)(text))
                    } else {
                        (self.theme.heading)(&(self.theme.bold)(&(self.theme.underline)(text)))
                    }
                };
                let heading_context = StyleContext {
                    apply_text: Box::new(heading_style),
                    style_prefix: Self::style_prefix(&heading_style),
                };
                let heading_text = self.render_inline(inline, &heading_context);
                if *depth >= 3 {
                    lines.push(format!("{}{heading_text}", heading_style(&heading_prefix)));
                } else {
                    lines.push(heading_text);
                }
                if let Some(next) = next_type
                    && next != "space"
                {
                    lines.push(String::new());
                }
            }
            BlockKind::Paragraph(inline) => {
                lines.push(self.render_inline(inline, context));
                if let Some(next) = next_type
                    && next != "list"
                    && next != "space"
                {
                    lines.push(String::new());
                }
            }
            BlockKind::LatexBlock { text, raw, pending } => {
                let rendered = if !*pending && self.options.render_latex {
                    render_latex(text, RenderLatexOptions { display: true })
                        .unwrap_or_else(|| raw.trim().to_string())
                } else {
                    raw.trim().to_string()
                };
                for line in rendered.split('\n') {
                    lines.push(self.apply_default_style(line));
                }
                if let Some(next) = next_type
                    && next != "space"
                {
                    lines.push(String::new());
                }
            }
            BlockKind::Code { lang, text } => {
                let indent = self
                    .theme
                    .code_block_indent
                    .clone()
                    .unwrap_or_else(|| "  ".to_string());
                lines.push((self.theme.code_block_border)(&format!(
                    "```{}",
                    lang.clone().unwrap_or_default()
                )));
                match &self.theme.highlight_code {
                    Some(highlight) => {
                        for hl_line in highlight(text, lang.as_deref()) {
                            lines.push(format!("{indent}{hl_line}"));
                        }
                    }
                    None => {
                        for code_line in text.split('\n') {
                            lines.push(format!("{indent}{}", (self.theme.code_block)(code_line)));
                        }
                    }
                }
                lines.push((self.theme.code_block_border)("```"));
                if let Some(next) = next_type
                    && next != "space"
                {
                    lines.push(String::new());
                }
            }
            BlockKind::Blockquote(quote_blocks) => {
                let quote_style = |text: &str| (self.theme.quote)(&(self.theme.italic)(text));
                let quote_style_prefix = Self::style_prefix(&quote_style);
                let apply_quote_style = |line: &str| {
                    if quote_style_prefix.is_empty() {
                        return quote_style(line);
                    }
                    let re_applied =
                        line.replace("\u{1b}[0m", &format!("\u{1b}[0m{quote_style_prefix}"));
                    quote_style(&re_applied)
                };
                let quote_content_width = (width).saturating_sub(2).max(1);
                let inner_context = StyleContext {
                    apply_text: Box::new(|text: &str| text.to_string()),
                    style_prefix: quote_style_prefix.clone(),
                };
                let mut rendered_quote_lines: Vec<String> = Vec::new();
                for (index, quote_block) in quote_blocks.iter().enumerate() {
                    let next = quote_blocks.get(index + 1).map(|b| b.next_kind.as_str());
                    rendered_quote_lines.extend(self.render_block(
                        quote_block,
                        quote_content_width,
                        next,
                        &inner_context,
                    ));
                }
                while rendered_quote_lines
                    .last()
                    .map(String::is_empty)
                    .unwrap_or(false)
                {
                    rendered_quote_lines.pop();
                }
                for quote_line in &rendered_quote_lines {
                    let styled = apply_quote_style(quote_line);
                    for wrapped in wrap_text_with_ansi(&styled, quote_content_width) {
                        lines.push(format!("{}{wrapped}", (self.theme.quote_border)("│ ")));
                    }
                }
                if let Some(next) = next_type
                    && next != "space"
                {
                    lines.push(String::new());
                }
            }
            BlockKind::List(list) => {
                lines.extend(self.render_list(list, 0, width, context));
            }
            BlockKind::Table(table) => {
                lines.extend(self.render_table(table, width, next_type, context));
            }
            BlockKind::Rule => {
                lines.push((self.theme.hr)(&"─".repeat(width.min(80))));
                if let Some(next) = next_type
                    && next != "space"
                {
                    lines.push(String::new());
                }
            }
            BlockKind::Html(raw) => {
                lines.push(self.apply_default_style(raw.trim()));
            }
            BlockKind::Paragraphish(raw) => {
                lines.push(self.apply_default_style(raw.trim()));
            }
        }
        lines
    }

    fn render_inline(&self, inline: &[Inline], context: &StyleContext<'_>) -> String {
        let apply = &context.apply_text;
        let apply_with_newlines =
            |text: &str| text.split('\n').map(apply).collect::<Vec<_>>().join("\n");
        let mut result = String::new();
        for token in inline {
            match token {
                Inline::Latex { text, raw, pending } => {
                    let rendered = if !*pending && self.options.render_latex {
                        render_latex(text, RenderLatexOptions { display: false })
                            .unwrap_or_else(|| raw.clone())
                    } else {
                        raw.clone()
                    };
                    result.push_str(&apply_with_newlines(&rendered));
                }
                Inline::Escape(raw, text) => {
                    result.push_str(&apply_with_newlines(
                        if self.options.preserve_backslash_escapes {
                            raw
                        } else {
                            text
                        },
                    ));
                }
                Inline::Text(text) => result.push_str(&apply_with_newlines(text)),
                Inline::Strong(content) => {
                    let bold_content = self.render_inline(content, context);
                    result.push_str(&(self.theme.bold)(&bold_content));
                    result.push_str(&context.style_prefix);
                }
                Inline::Em(content) => {
                    let italic_content = self.render_inline(content, context);
                    result.push_str(&(self.theme.italic)(&italic_content));
                    result.push_str(&context.style_prefix);
                }
                Inline::Code(text) => {
                    result.push_str(&(self.theme.code)(text));
                    result.push_str(&context.style_prefix);
                }
                Inline::Link { text, href } => {
                    let link_text = self.render_inline(text, context);
                    let styled_link = (self.theme.link)(&(self.theme.underline)(&link_text));
                    // Hyperlink capability stays host-side: always use the
                    // URL-fallback path.
                    let href_for_comparison = href.strip_prefix("mailto:").unwrap_or(href);
                    if text_plain(text) == *href || text_plain(text) == href_for_comparison {
                        result.push_str(&styled_link);
                    } else {
                        result.push_str(&styled_link);
                        result.push_str(&(self.theme.link_url)(&format!(" ({href})")));
                    }
                    result.push_str(&context.style_prefix);
                }
                Inline::Del(content) => {
                    let del_content = self.render_inline(content, context);
                    result.push_str(&(self.theme.strikethrough)(&del_content));
                    result.push_str(&context.style_prefix);
                }
                Inline::SoftBreak | Inline::HardBreak => result.push('\n'),
            }
        }
        while !context.style_prefix.is_empty() && result.ends_with(&context.style_prefix) {
            result.truncate(result.len() - context.style_prefix.len());
        }
        result
    }

    fn render_list(
        &self,
        list: &ListBlock,
        depth: usize,
        width: usize,
        context: &StyleContext<'_>,
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let indent = "    ".repeat(depth);
        let start_number = list.start;

        for (index, item) in list.items.iter().enumerate() {
            let is_last = index == list.items.len() - 1;
            let bullet = if list.ordered {
                if self.options.preserve_ordered_list_markers {
                    item.raw_marker
                        .clone()
                        .unwrap_or_else(|| format!("{}. ", start_number + index))
                } else {
                    format!("{}. ", start_number + index)
                }
            } else if self.options.preserve_ordered_list_markers {
                item.raw_marker.clone().unwrap_or_else(|| "- ".to_string())
            } else {
                "- ".to_string()
            };
            let task_marker = match item.task {
                Some(checked) => if checked { "[x] " } else { "[ ] " }.to_string(),
                None => String::new(),
            };
            let marker = format!("{bullet}{task_marker}");
            let first_prefix = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let continuation_prefix = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            let item_width = width.saturating_sub(visible_width(&first_prefix)).max(1);
            let mut rendered_any_line = false;

            for item_token in &item.blocks {
                if let BlockKind::List(nested) = &item_token.kind {
                    lines.extend(self.render_list(nested, depth + 1, width, context));
                    rendered_any_line = true;
                    continue;
                }
                let item_lines = self.render_block(item_token, item_width, None, context);
                for line in item_lines {
                    for wrapped in wrap_text_with_ansi(&line, item_width) {
                        let line_prefix = if rendered_any_line {
                            continuation_prefix.clone()
                        } else {
                            first_prefix.clone()
                        };
                        lines.push(format!("{line_prefix}{wrapped}"));
                        rendered_any_line = true;
                    }
                }
            }

            if !rendered_any_line {
                lines.push(first_prefix);
            }

            if list.loose && !is_last {
                lines.push(String::new());
            }
        }
        lines
    }

    fn longest_word_width(text: &str, max_width: Option<usize>) -> usize {
        let mut longest = 0;
        for word in text.split_whitespace() {
            longest = longest.max(visible_width(word));
        }
        match max_width {
            Some(max) => longest.min(max),
            None => longest,
        }
    }

    fn wrap_cell_text(text: &str, max_width: usize, style_prefix: &str) -> Vec<String> {
        let lines = wrap_text_with_ansi(text, max_width.max(1));
        let last = lines.len().saturating_sub(1);
        lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let style_reset = if index < last {
                    "\u{1b}[22;23;24;25;27;28;29;39m"
                } else {
                    ""
                };
                format!("{line}{style_reset}{style_prefix}")
            })
            .collect()
    }

    fn render_table(
        &self,
        table: &TableBlock,
        available_width: usize,
        next_type: Option<&str>,
        context: &StyleContext<'_>,
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let num_cols = table.header.len();
        if num_cols == 0 {
            return lines;
        }

        let border_overhead = 3 * num_cols + 1;
        let Some(available_for_cells) = available_width.checked_sub(border_overhead) else {
            return lines;
        };
        if available_for_cells < num_cols {
            let mut fallback = wrap_text_with_ansi(&table.raw, available_width);
            if let Some(next) = next_type
                && next != "space"
            {
                fallback.push(String::new());
            }
            return fallback;
        }

        let max_unbroken_word_width = 30;

        let mut natural_widths = vec![0usize; num_cols];
        let mut min_word_widths = vec![1usize; num_cols];
        for (index, cell) in table.header.iter().enumerate() {
            let text = self.render_inline(cell, context);
            natural_widths[index] = visible_width(&text);
            min_word_widths[index] =
                Self::longest_word_width(&text, Some(max_unbroken_word_width)).max(1);
        }
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate().take(num_cols) {
                let text = self.render_inline(cell, context);
                natural_widths[index] = natural_widths[index].max(visible_width(&text));
                min_word_widths[index] = min_word_widths[index].max(Self::longest_word_width(
                    &text,
                    Some(max_unbroken_word_width),
                ));
            }
        }

        let mut min_column_widths = min_word_widths.clone();
        let mut min_cells_width: usize = min_column_widths.iter().sum();

        if min_cells_width > available_for_cells {
            min_column_widths = vec![1usize; num_cols];
            let remaining = available_for_cells - num_cols;
            if remaining > 0 {
                let total_weight: usize = min_word_widths.iter().map(|w| w.saturating_sub(1)).sum();
                let growth: Vec<usize> = min_word_widths
                    .iter()
                    .map(|w| {
                        let weight = w.saturating_sub(1);
                        (weight * remaining).checked_div(total_weight).unwrap_or(0)
                    })
                    .collect();
                for index in 0..num_cols {
                    min_column_widths[index] += growth[index];
                }
                let allocated: usize = growth.iter().sum();
                let mut leftover = remaining - allocated;
                for width in min_column_widths.iter_mut() {
                    if leftover == 0 {
                        break;
                    }
                    *width += 1;
                    leftover -= 1;
                }
            }
            min_cells_width = min_column_widths.iter().sum();
        }

        let total_natural_width: usize = natural_widths.iter().sum::<usize>() + border_overhead;
        let column_widths: Vec<usize> = if total_natural_width <= available_width {
            natural_widths
                .iter()
                .zip(&min_column_widths)
                .map(|(natural, min)| *natural.max(min))
                .collect()
        } else {
            let total_grow_potential: usize = natural_widths
                .iter()
                .zip(&min_column_widths)
                .map(|(natural, min)| natural.saturating_sub(*min))
                .sum();
            let extra_width = available_for_cells.saturating_sub(min_cells_width);
            let mut widths: Vec<usize> = min_column_widths
                .iter()
                .zip(&natural_widths)
                .map(|(min_width, natural)| {
                    let delta = natural.saturating_sub(*min_width);
                    let grow = (delta * extra_width)
                        .checked_div(total_grow_potential)
                        .unwrap_or(0);
                    min_width + grow
                })
                .collect();
            let mut remaining = available_for_cells.saturating_sub(widths.iter().sum::<usize>());
            while remaining > 0 {
                let mut grew = false;
                for index in 0..num_cols {
                    if remaining == 0 {
                        break;
                    }
                    if widths[index] < natural_widths[index] {
                        widths[index] += 1;
                        remaining -= 1;
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
            widths
        };

        let top_border_cells: Vec<String> = column_widths.iter().map(|w| "─".repeat(*w)).collect();
        lines.push(format!("┌─{}─┐", top_border_cells.join("─┬─")));

        let header_cell_lines: Vec<Vec<String>> = table
            .header
            .iter()
            .zip(&column_widths)
            .map(|(cell, width)| {
                let text = self.render_inline(cell, context);
                Self::wrap_cell_text(&text, *width, &context.style_prefix)
            })
            .collect();
        let header_line_count = header_cell_lines.iter().map(Vec::len).max().unwrap_or(0);
        for line_index in 0..header_line_count {
            let row_parts: Vec<String> = header_cell_lines
                .iter()
                .zip(&column_widths)
                .map(|(cell_lines, width)| {
                    let text = cell_lines.get(line_index).cloned().unwrap_or_default();
                    let padded = format!(
                        "{text}{}",
                        " ".repeat(width.saturating_sub(visible_width(&text)))
                    );
                    (self.theme.bold)(&padded)
                })
                .collect();
            lines.push(format!("│ {} │", row_parts.join(" │ ")));
        }

        let separator_cells: Vec<String> = column_widths.iter().map(|w| "─".repeat(*w)).collect();
        let separator_line = format!("├─{}─┤", separator_cells.join("─┼─"));
        lines.push(separator_line.clone());

        for (row_index, row) in table.rows.iter().enumerate() {
            let row_cell_lines: Vec<Vec<String>> = row
                .iter()
                .zip(&column_widths)
                .map(|(cell, width)| {
                    let text = self.render_inline(cell, context);
                    Self::wrap_cell_text(&text, *width, &context.style_prefix)
                })
                .collect();
            let row_line_count = row_cell_lines.iter().map(Vec::len).max().unwrap_or(0);
            for line_index in 0..row_line_count {
                let row_parts: Vec<String> = row_cell_lines
                    .iter()
                    .zip(&column_widths)
                    .map(|(cell_lines, width)| {
                        let text = cell_lines.get(line_index).cloned().unwrap_or_default();
                        format!(
                            "{text}{}",
                            " ".repeat(width.saturating_sub(visible_width(&text)))
                        )
                    })
                    .collect();
                lines.push(format!("│ {} │", row_parts.join(" │ ")));
            }
            if row_index < table.rows.len() - 1 {
                lines.push(separator_line.clone());
            }
        }

        let bottom_border_cells: Vec<String> =
            column_widths.iter().map(|w| "─".repeat(*w)).collect();
        lines.push(format!("└─{}─┘", bottom_border_cells.join("─┴─")));

        if let Some(next) = next_type
            && next != "space"
        {
            lines.push(String::new());
        }
        lines
    }
}

fn text_plain(inline: &[Inline]) -> String {
    inline
        .iter()
        .map(|token| match token {
            Inline::Text(text) => text.clone(),
            Inline::Code(text) => text.clone(),
            Inline::Escape(_, text) => text.clone(),
            Inline::Latex { raw, .. } => raw.clone(),
            Inline::Link { text, .. } => text_plain(text),
            Inline::Strong(content) | Inline::Em(content) | Inline::Del(content) => {
                text_plain(content)
            }
            Inline::SoftBreak | Inline::HardBreak => "\n".to_string(),
        })
        .collect()
}

// ============================================================================
// pulldown-cmark event grouping
// ============================================================================

/// A block-level element (upstream marked block tokens).
enum BlockKind {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    #[allow(dead_code)]
    LatexBlock {
        text: String,
        raw: String,
        pending: bool,
    },
    Code {
        lang: Option<String>,
        text: String,
    },
    Blockquote(Vec<Block>),
    List(ListBlock),
    Table(TableBlock),
    Rule,
    Html(String),
    Paragraphish(String),
}

/// A block with the rendered "next token type" hint (upstream
/// `nextTokenType`).
struct Block {
    kind: BlockKind,
    next_kind: String,
}

struct ListItemBlock {
    raw_marker: Option<String>,
    task: Option<bool>,
    blocks: Vec<Block>,
}

struct ListBlock {
    ordered: bool,
    loose: bool,
    start: usize,
    items: Vec<ListItemBlock>,
}

struct TableBlock {
    raw: String,
    header: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
}

/// Inline-level element (upstream marked inline tokens).
enum Inline {
    Text(String),
    Code(String),
    #[allow(dead_code)]
    Escape(String, String),
    Strong(Vec<Inline>),
    Em(Vec<Inline>),
    Del(Vec<Inline>),
    Link {
        text: Vec<Inline>,
        href: String,
    },
    #[allow(dead_code)]
    Latex {
        text: String,
        raw: String,
        pending: bool,
    },
    SoftBreak,
    HardBreak,
}

/// Group a pulldown-cmark event stream into blocks with inline runs.
fn group_blocks(events: Vec<Event>) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut iter = events.into_iter().peekable();
    while let Some(event) = iter.next() {
        let mut next_kind = String::new();
        let kind = build_block(event, &mut iter, &mut next_kind);
        blocks.push(Block { kind, next_kind });
    }
    blocks
}

fn collect_inline_until(
    iter: &mut std::iter::Peekable<std::vec::IntoIter<Event>>,
    end: TagEnd,
) -> Vec<Inline> {
    let mut inline: Vec<Inline> = Vec::new();
    loop {
        let Some(event) = iter.next() else {
            return inline;
        };
        match event {
            Event::End(tag) if tag == end => return inline,
            Event::Text(text) => inline.push(Inline::Text(text.to_string())),
            Event::Code(text) => inline.push(Inline::Code(text.to_string())),
            Event::SoftBreak => inline.push(Inline::SoftBreak),
            Event::HardBreak => inline.push(Inline::HardBreak),
            Event::Start(tag) => match tag {
                Tag::Strong => {
                    let content = collect_inline_until(iter, TagEnd::Strong);
                    inline.push(Inline::Strong(content));
                }
                Tag::Emphasis => {
                    let content = collect_inline_until(iter, TagEnd::Emphasis);
                    inline.push(Inline::Em(content));
                }
                Tag::Strikethrough => {
                    let content = collect_inline_until(iter, TagEnd::Strikethrough);
                    inline.push(Inline::Del(content));
                }
                Tag::Link { dest_url, .. } => {
                    let content = collect_inline_until(iter, TagEnd::Link);
                    inline.push(Inline::Link {
                        text: content,
                        href: dest_url.to_string(),
                    });
                }
                Tag::Paragraph => {
                    let content = collect_inline_until(iter, TagEnd::Paragraph);
                    inline.extend(content);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn build_block(
    event: Event,
    iter: &mut std::iter::Peekable<std::vec::IntoIter<Event>>,
    next_kind: &mut String,
) -> BlockKind {
    match event {
        Event::Start(Tag::Heading { level, .. }) => {
            let depth = match level {
                HeadingLevel::H1 => 1,
                HeadingLevel::H2 => 2,
                HeadingLevel::H3 => 3,
                HeadingLevel::H4 => 4,
                HeadingLevel::H5 => 5,
                HeadingLevel::H6 => 6,
            };
            let inline = collect_inline_until(iter, TagEnd::Heading(level));
            BlockKind::Heading(depth, inline)
        }
        Event::Start(Tag::Paragraph) => {
            let inline = collect_inline_until(iter, TagEnd::Paragraph);
            BlockKind::Paragraph(inline)
        }
        Event::Start(Tag::CodeBlock(kind)) => {
            let lang = match &kind {
                CodeBlockKind::Fenced(info) => {
                    let info = info.split(',').next().unwrap_or("").trim().to_string();
                    (!info.is_empty()).then_some(info)
                }
                _ => None,
            };
            let mut text = String::new();
            loop {
                match iter.next() {
                    Some(Event::Text(t)) => text.push_str(&t),
                    Some(Event::End(TagEnd::CodeBlock)) => break,
                    _ => break,
                }
            }
            if text.ends_with('\n') {
                text.truncate(text.len() - 1);
            }
            BlockKind::Code { lang, text }
        }
        Event::Start(Tag::BlockQuote(_)) => {
            let mut inner: Vec<Block> = Vec::new();
            while let Some(event) = iter.next() {
                match event {
                    Event::End(TagEnd::BlockQuote(_)) => break,
                    other => {
                        let mut inner_next = String::new();
                        let kind = build_block(other, iter, &mut inner_next);
                        inner.push(Block {
                            kind,
                            next_kind: inner_next,
                        });
                    }
                }
            }
            BlockKind::Blockquote(inner)
        }
        Event::Start(Tag::List(start)) => {
            let mut list = ListBlock {
                ordered: start.is_some(),
                loose: false,
                start: start.unwrap_or(1) as usize,
                items: Vec::new(),
            };
            let mut saw_paragraph = false;
            while let Some(event) = iter.next() {
                match event {
                    Event::End(TagEnd::List(_)) => break,
                    Event::Start(Tag::Item) => {
                        let mut item_blocks: Vec<Block> = Vec::new();
                        let mut raw_marker: Option<String> = None;
                        let mut task: Option<bool> = None;
                        let mut first_text: Option<String> = None;
                        while let Some(item_event) = iter.next() {
                            match item_event {
                                Event::End(TagEnd::Item) => break,
                                Event::TaskListMarker(checked) => task = Some(checked),
                                Event::Text(text) => {
                                    if first_text.is_none() {
                                        first_text = Some(text.to_string());
                                    }
                                    // Capture into first paragraph inline.
                                    if let Some(last) = item_blocks.last_mut()
                                        && let BlockKind::Paragraph(inline) = &mut last.kind
                                    {
                                        inline.push(Inline::Text(text.to_string()));
                                        continue;
                                    }
                                    let inner_next = String::new();
                                    let kind =
                                        BlockKind::Paragraph(vec![Inline::Text(text.to_string())]);
                                    item_blocks.push(Block {
                                        kind,
                                        next_kind: inner_next,
                                    });
                                }
                                Event::Code(text) => {
                                    // Tight list items emit bare `Event::Code`
                                    // without a Paragraph start (upstream
                                    // handles inline code spans here too).
                                    if let Some(last) = item_blocks.last_mut()
                                        && let BlockKind::Paragraph(inline) = &mut last.kind
                                    {
                                        inline.push(Inline::Code(text.to_string()));
                                        continue;
                                    }
                                    let inner_next = String::new();
                                    let kind =
                                        BlockKind::Paragraph(vec![Inline::Code(text.to_string())]);
                                    item_blocks.push(Block {
                                        kind,
                                        next_kind: inner_next,
                                    });
                                }
                                Event::Start(Tag::Paragraph) => {
                                    saw_paragraph = true;
                                    let inline = collect_inline_until(iter, TagEnd::Paragraph);
                                    let inner_next = String::new();
                                    let kind = BlockKind::Paragraph(inline);
                                    item_blocks.push(Block {
                                        kind,
                                        next_kind: inner_next,
                                    });
                                }
                                Event::Start(Tag::List(_)) => {
                                    let mut inner_next = String::new();
                                    let kind = build_block(
                                        Event::Start(Tag::List(None)),
                                        iter,
                                        &mut inner_next,
                                    );
                                    // build_block consumed List(None); correct ordering:
                                    let mut list2 = match kind {
                                        BlockKind::List(l) => l,
                                        other => {
                                            item_blocks.push(Block {
                                                kind: other,
                                                next_kind: String::new(),
                                            });
                                            continue;
                                        }
                                    };
                                    list2.ordered = false;
                                    // Re-detect ordered from the original start value:
                                    let _ = &mut list2;
                                    let _ = raw_marker.take();
                                    item_blocks.push(Block {
                                        kind: BlockKind::List(list2),
                                        next_kind: inner_next,
                                    });
                                }
                                Event::Start(tag) => {
                                    let mut inner_next = String::new();
                                    let kind =
                                        build_block(Event::Start(tag), iter, &mut inner_next);
                                    item_blocks.push(Block {
                                        kind,
                                        next_kind: inner_next,
                                    });
                                }
                                Event::Rule => {
                                    item_blocks.push(Block {
                                        kind: BlockKind::Rule,
                                        next_kind: String::new(),
                                    });
                                }
                                _ => {}
                            }
                        }
                        if let Some(first) = first_text
                            && !item_blocks.is_empty()
                        {
                            raw_marker = ordered_marker_from(&first)
                                .or_else(|| unordered_marker_from(&first));
                        }
                        list.items.push(ListItemBlock {
                            raw_marker,
                            task,
                            blocks: item_blocks,
                        });
                    }
                    _ => {}
                }
            }
            list.loose = saw_paragraph;
            BlockKind::List(list)
        }
        Event::Start(Tag::Table(alignments)) => {
            let mut raw = String::new();
            let mut header: Vec<Vec<Inline>> = Vec::new();
            let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
            let mut current_row: Vec<Vec<Inline>> = Vec::new();
            let mut current_cell: Vec<Inline> = Vec::new();
            let mut _in_header = true;
            while let Some(event) = iter.next() {
                match event {
                    Event::End(TagEnd::Table) => break,
                    Event::Start(Tag::TableHead) => _in_header = true,
                    Event::End(TagEnd::TableHead) => {
                        if !current_cell.is_empty() {
                            current_row.push(std::mem::take(&mut current_cell));
                        }
                        header = std::mem::take(&mut current_row);
                        _in_header = false;
                    }
                    Event::Start(Tag::TableRow) => {
                        current_row = Vec::new();
                        current_cell = Vec::new();
                    }
                    Event::End(TagEnd::TableRow) => {
                        if !current_cell.is_empty() {
                            current_row.push(std::mem::take(&mut current_cell));
                        }
                        rows.push(std::mem::take(&mut current_row));
                    }
                    Event::Start(Tag::TableCell) => current_cell = Vec::new(),
                    Event::End(TagEnd::TableCell) => {
                        current_row.push(std::mem::take(&mut current_cell));
                    }
                    Event::Text(t) => {
                        raw.push_str(&t);
                        current_cell.push(Inline::Text(t.to_string()));
                    }
                    Event::Code(t) => current_cell.push(Inline::Code(t.to_string())),
                    Event::Start(Tag::Strong) => {
                        let content = collect_inline_until(iter, TagEnd::Strong);
                        current_cell.push(Inline::Strong(content));
                    }
                    Event::Start(Tag::Emphasis) => {
                        let content = collect_inline_until(iter, TagEnd::Emphasis);
                        current_cell.push(Inline::Em(content));
                    }
                    Event::SoftBreak => current_cell.push(Inline::SoftBreak),
                    _ => {}
                }
            }
            let _ = alignments;
            BlockKind::Table(TableBlock { raw, header, rows })
        }
        Event::Rule => BlockKind::Rule,
        Event::Start(Tag::HtmlBlock) => {
            let mut raw = String::new();
            loop {
                match iter.next() {
                    Some(Event::Html(t)) => raw.push_str(&t),
                    Some(Event::Text(t)) => raw.push_str(&t),
                    Some(Event::End(TagEnd::HtmlBlock)) => break,
                    _ => break,
                }
            }
            BlockKind::Html(raw)
        }
        other => {
            let _ = next_kind;
            BlockKind::Paragraphish(match &other {
                Event::Text(t) => t.to_string(),
                _ => String::new(),
            })
        }
    }
}

fn ordered_marker_from(text: &str) -> Option<String> {
    let trimmed = text.trim_start_matches(' ');
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    let rest = &trimmed[digits.len()..];
    if !digits.is_empty() && digits.len() <= 9 && (rest.starts_with('.') || rest.starts_with(')')) {
        Some(format!("{digits}{} ", &rest[..1]))
    } else {
        None
    }
}

fn unordered_marker_from(text: &str) -> Option<String> {
    let trimmed = text.trim_start_matches(' ');
    let mut chars = trimmed.chars();
    match chars.next() {
        Some('-' | '+' | '*') => Some(format!("{} ", trimmed.chars().next().unwrap())),
        _ => None,
    }
}

/// Trim a streamed partial closing fence from the final code block so
/// code blocks do not shrink/flicker while streaming (upstream
/// `trimPartialClosingFences`, applied at the source level here): the
/// last line must be a nonempty run of fence characters shorter than 3
/// that matches an earlier opening fence in the same source, and the
/// source must end mid-line.
fn trim_partial_closing_fences(source: &str) -> String {
    let Some(last_line) = source.lines().last() else {
        return source.to_string();
    };
    let trimmed = last_line.trim_start_matches(' ');
    if trimmed.is_empty() || trimmed.len() >= 3 {
        return source.to_string();
    }
    let first = match trimmed.chars().next() {
        Some(c @ ('`' | '~')) => c,
        _ => return source.to_string(),
    };
    if !trimmed.chars().all(|c| c == first) {
        return source.to_string();
    }
    let marker_char = first;
    // An earlier opening fence of the same character must exist.
    let opening = String::from(marker_char).repeat(3);
    let has_earlier_fence = source
        .lines()
        .rev()
        .skip(1)
        .any(|line| line.trim_start_matches(' ').starts_with(&opening));
    if !has_earlier_fence {
        return source.to_string();
    }
    // Only trim when the source ends without a newline (streaming edge).
    if source.ends_with('\n') {
        return source.to_string();
    }
    let mut result = source.to_string();
    result.truncate(result.len() - last_line.len());
    result.trim_end_matches('\n').to_string()
}

impl crate::tui::Component for Markdown {
    fn render(&mut self, width: usize) -> RenderLines {
        Markdown::render(self, width)
    }
}
