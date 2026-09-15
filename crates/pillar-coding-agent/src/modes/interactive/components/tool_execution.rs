//! Port of components/tool-execution.ts: the transcript block for a tool
//! call — renderer-composed call/result lines, background colour by state
//! (pending/success/error), optional image blocks, and the generic fallback
//! when the tool has no renderer.
//!
//! divergence: upstream's `renderCall` / `renderResult` return TUI
//! components that are stored as children and re-added on every update; the
//! port's renderers (see `core/tools/render_definitions.rs`) answer styled
//! lines with state held in the renderer, so the component composes those
//! lines into a single pass-through child of the content box (or the
//! self-render container). The theme is read through the global `theme()`
//! when lines are built (upstream's inline closures read the live module
//! global too), the display rebuild runs lazily in `render` (upstream runs
//! `updateDisplay` eagerly on every mutation — visible output is the same),
//! `ui.requestRender()` is host-driven and omitted, extension renderers
//! arrive as pre-adapted `ToolRenderer`s (the `ExtensionUIContext` types are
//! not ported yet), and kitty's PNG conversion goes through an injectable
//! converter (upstream's async Photon path; see `utils/image-convert.ts`).

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use pillar_ai::types::Content;
use pillar_tui::components::{BoxComponent, Image, ImageOptions, Spacer, Text};
use pillar_tui::terminal_image::{ImageProtocol, get_capabilities};
use pillar_tui::tui::{Component, Container};

use crate::core::tools::render_definitions::{
    ToolRenderContext, ToolRenderResult, ToolRenderResultOptions, ToolRenderShell, ToolRenderer,
    create_tool_renderer,
};
use crate::core::tools::render_utils::{ToolResultBlock, get_text_output};
use crate::modes::interactive::components::keybinding_hints::key_hint;
use crate::modes::interactive::theme::theme;

/// Preview line limit of the generic result fallback (upstream
/// `FALLBACK_PREVIEW_LINES`).
const FALLBACK_PREVIEW_LINES: usize = 10;

/// Injected PNG conversion (upstream `convertToPng`): base64 image data and
/// mime type in, converted base64 PNG out. `None` (or a `None` answer) means
/// "no conversion available" — kitty then skips the image.
pub type PngConverter = Box<dyn Fn(&str, &str) -> Option<(String, String)> + Send>;

/// Options (upstream `ToolExecutionOptions`).
#[derive(Debug, Clone, Default)]
pub struct ToolExecutionOptions {
    pub show_images: Option<bool>,
    pub image_width_cells: Option<usize>,
}

/// The result being rendered (upstream the anonymous `{ content, details,
/// isError }` shape).
#[derive(Debug, Clone, Default)]
pub struct ToolExecutionResult {
    pub content: Vec<Content>,
    pub details: serde_json::Value,
    pub is_error: bool,
}

/// Pass-through child rendering stored lines verbatim (the port stand-in for
/// upstream's renderer-provided child components).
///
/// Upstream hands these strings to a `Text` child, which splits on newlines
/// and word-wraps every line to the available width; a multi-line string
/// stored as one element therefore has to be wrapped here too (otherwise the
/// frame contains a line wider than the terminal and the renderer aborts with
/// "Rendered line N exceeds terminal width").
struct StaticLines(Vec<String>);

impl Component for StaticLines {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for stored in &self.0 {
            lines.extend(pillar_tui::text_utils::wrap_text_with_ansi(
                stored,
                width.max(1),
            ));
        }
        lines
    }
}

/// A tool execution block (upstream `ToolExecutionComponent`).
pub struct ToolExecutionComponent {
    spacer: Spacer,
    content_box: BoxComponent,
    content_text: Text,
    self_render_container: Container,
    /// Whether the constructor picked `contentBox` (a renderer is present)
    /// or `contentText` (generic fallback).
    uses_content_box: bool,
    custom_renderer: Option<Box<dyn ToolRenderer>>,
    builtin_renderer: Option<Box<dyn ToolRenderer>>,
    tool_name: String,
    tool_call_id: String,
    args: serde_json::Value,
    expanded: bool,
    show_images: bool,
    image_width_cells: usize,
    is_partial: bool,
    cwd: String,
    execution_started: bool,
    args_complete: bool,
    result: Option<ToolExecutionResult>,
    converted_images: HashMap<usize, (String, String)>,
    hide_component: bool,
    image_components: Vec<Image>,
    image_spacers: Vec<Spacer>,
    png_converter: Option<PngConverter>,
    /// The display is rebuilt lazily: `update_display` runs in `render` when
    /// the state changed or the width moved (see the module divergence note).
    dirty: bool,
    rendered_width: usize,
}

impl ToolExecutionComponent {
    pub fn new(
        tool_name: &str,
        tool_call_id: &str,
        args: serde_json::Value,
        options: ToolExecutionOptions,
        custom_renderer: Option<Box<dyn ToolRenderer>>,
        cwd: &str,
    ) -> Self {
        let builtin_renderer = create_tool_renderer(tool_name, cwd);
        let uses_content_box = builtin_renderer.is_some() || custom_renderer.is_some();

        // Upstream always creates all three shell variants and appends the
        // one matching the current shell.
        let mut content_box = BoxComponent::new(1, 1);
        content_box.set_bg_fn(Some(background_fn("toolPendingBg")));
        let content_text = Text::new("", 1, 1);
        let self_render_container = Container::new();

        let mut component = Self {
            spacer: Spacer::new(1),
            content_box,
            content_text,
            self_render_container,
            uses_content_box,
            custom_renderer,
            builtin_renderer,
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            args,
            expanded: false,
            show_images: options.show_images.unwrap_or(true),
            image_width_cells: options.image_width_cells.unwrap_or(60),
            is_partial: true,
            cwd: cwd.to_string(),
            execution_started: false,
            args_complete: false,
            result: None,
            converted_images: HashMap::new(),
            hide_component: false,
            image_components: Vec::new(),
            image_spacers: Vec::new(),
            png_converter: None,
            dirty: true,
            rendered_width: 0,
        };
        component.update_display(0);
        component
    }

    /// The kitty PNG converter (upstream imports `convertToPng` from
    /// `utils/image-convert.ts`); hosts inject it when a decoder exists.
    pub fn set_png_converter(&mut self, converter: Option<PngConverter>) {
        self.png_converter = converter;
    }

    // --- renderer selection (upstream hasRendererDefinition /
    // getRenderShell / getCallRenderer / getResultRenderer) ---

    fn has_renderer_definition(&self) -> bool {
        self.builtin_renderer.is_some() || self.custom_renderer.is_some()
    }

    /// Upstream's custom-wins shell selection; the port's renderers always
    /// declare a shell, so there is no `undefined` fallback to the built-in's
    /// shell.
    fn get_render_shell(&self) -> ToolRenderShell {
        if let Some(custom) = &self.custom_renderer {
            return custom.render_shell();
        }
        if let Some(builtin) = &self.builtin_renderer {
            return builtin.render_shell();
        }
        ToolRenderShell::Default
    }

    /// The renderer serving the call (upstream
    /// `toolDefinition.renderCall ?? builtInToolDefinition.renderCall`).
    fn call_renderer(&mut self) -> Option<&mut Box<dyn ToolRenderer>> {
        if self
            .custom_renderer
            .as_ref()
            .is_some_and(|r| r.has_render_call())
        {
            return self.custom_renderer.as_mut();
        }
        if self
            .builtin_renderer
            .as_ref()
            .is_some_and(|r| r.has_render_call())
        {
            return self.builtin_renderer.as_mut();
        }
        None
    }

    /// The renderer serving the result (upstream
    /// `toolDefinition.renderResult ?? builtInToolDefinition.renderResult`).
    fn result_renderer(&mut self) -> Option<&mut Box<dyn ToolRenderer>> {
        if self
            .custom_renderer
            .as_ref()
            .is_some_and(|r| r.has_render_result())
        {
            return self.custom_renderer.as_mut();
        }
        if self
            .builtin_renderer
            .as_ref()
            .is_some_and(|r| r.has_render_result())
        {
            return self.builtin_renderer.as_mut();
        }
        None
    }

    fn get_render_context(&self) -> ToolRenderContext {
        ToolRenderContext {
            args: self.args.clone(),
            tool_call_id: self.tool_call_id.clone(),
            cwd: self.cwd.clone(),
            execution_started: self.execution_started,
            args_complete: self.args_complete,
            is_partial: self.is_partial,
            expanded: self.expanded,
            show_images: self.show_images,
            is_error: self.result.as_ref().is_some_and(|r| r.is_error),
        }
    }

    // --- fallback lines (upstream createCallFallback /
    // createResultFallback) ---

    fn call_fallback_lines(&self) -> Vec<String> {
        let theme_obj = theme();
        vec![theme_obj.fg("toolTitle", &theme_obj.bold(&self.tool_name))]
    }

    fn result_fallback_lines(&self) -> Option<Vec<String>> {
        let output = self.text_output();
        if output.is_empty() {
            return None;
        }

        let lines: Vec<&str> = output.split('\n').collect();
        let display_count = if self.expanded {
            lines.len()
        } else {
            lines.len().min(FALLBACK_PREVIEW_LINES)
        };
        let remaining = lines.len() - display_count;
        let theme_obj = theme();
        let mut text = lines[..display_count]
            .iter()
            .map(|line| theme_obj.fg("toolOutput", line))
            .collect::<Vec<_>>()
            .join("\n");
        if remaining > 0 {
            text += &format!(
                "{} {}{}",
                theme_obj.fg("muted", &format!("\n... ({remaining} more lines,")),
                key_hint("app.tools.expand", "to expand"),
                theme_obj.fg("muted", ")")
            );
        }
        Some(vec![text])
    }

    // --- mutators (upstream updateArgs / markExecutionStarted /
    // setArgsComplete / updateResult / setExpanded / setShowImages /
    // setImageWidthCells) ---

    pub fn update_args(&mut self, args: serde_json::Value) {
        self.args = args;
        self.dirty = true;
    }

    pub fn mark_execution_started(&mut self) {
        self.execution_started = true;
        self.dirty = true;
    }

    pub fn set_args_complete(&mut self) {
        self.args_complete = true;
        self.dirty = true;
    }

    pub fn update_result(&mut self, result: ToolExecutionResult, is_partial: bool) {
        self.result = Some(result);
        self.is_partial = is_partial;
        self.dirty = true;
        self.maybe_convert_images_for_kitty();
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.dirty = true;
    }

    pub fn set_show_images(&mut self, show: bool) {
        self.show_images = show;
        self.dirty = true;
    }

    pub fn set_image_width_cells(&mut self, width: usize) {
        self.image_width_cells = width.max(1);
        self.dirty = true;
    }

    /// Async in upstream; the port's converter is synchronous.
    fn maybe_convert_images_for_kitty(&mut self) {
        if self.png_converter.is_none() {
            return;
        }
        if get_capabilities().images != Some(ImageProtocol::Kitty) {
            return;
        }
        let Some(result) = &self.result else {
            return;
        };
        let image_blocks: Vec<(usize, String, String)> = result
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Image { data, mime_type } => Some((data.clone(), mime_type.clone())),
                _ => None,
            })
            .enumerate()
            .map(|(index, (data, mime_type))| (index, data, mime_type))
            .collect();
        for (index, data, mime_type) in image_blocks {
            if data.is_empty() || mime_type.is_empty() || mime_type == "image/png" {
                continue;
            }
            if self.converted_images.contains_key(&index) {
                continue;
            }
            let converter = self.png_converter.as_ref().expect("checked");
            if let Some(converted) = converter(&data, &mime_type) {
                self.converted_images.insert(index, converted);
                self.dirty = true;
            }
        }
    }

    // --- display assembly (upstream updateDisplay) ---

    #[allow(unused_assignments)] // mirrors upstream's hasContent bookkeeping
    fn update_display(&mut self, width: usize) {
        let bg_key = self.bg_key();
        let has_renderer = self.has_renderer_definition();

        let mut has_content = false;
        self.hide_component = false;

        let theme_obj = theme();
        let args = self.args.clone();
        let context = self.get_render_context();

        if has_renderer {
            if self.get_render_shell() == ToolRenderShell::SelfRendered {
                // A plain Container has no bg function (upstream
                // `instanceof Box`).
                self.self_render_container.clear();
            } else {
                self.content_box.set_bg_fn(Some(background_fn(bg_key)));
                self.content_box.clear();
            }

            // Call renderer.
            let call_lines = self.render_call_lines(width, &theme_obj, &args, &context);
            match call_lines {
                Some(lines) => {
                    self.push_render_child(lines);
                    has_content = true;
                }
                None => {
                    self.push_render_child(self.call_fallback_lines());
                    has_content = true;
                }
            }

            // Result renderer.
            if let Some(result) = self.result.clone() {
                match self.render_result_lines(width, &theme_obj, &args, &result, &context) {
                    Some(Some(lines)) => {
                        self.push_render_child(lines);
                        has_content = true;
                    }
                    Some(None) => {}
                    None => {
                        if let Some(lines) = self.result_fallback_lines() {
                            self.push_render_child(lines);
                            has_content = true;
                        }
                    }
                }
            }
        } else {
            self.content_text.set_bg_fn(Some(background_fn(bg_key)));
            self.content_text.set_text(&self.format_tool_execution());
            has_content = true;
        }

        // Images (upstream removes its image children and rebuilds them).
        self.image_components.clear();
        self.image_spacers.clear();

        let capabilities = get_capabilities();
        if let Some(result) = &self.result {
            let image_blocks: Vec<(&String, &String)> = result
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Image { data, mime_type } => Some((data, mime_type)),
                    _ => None,
                })
                .collect();
            for (index, (data, mime_type)) in image_blocks.into_iter().enumerate() {
                if capabilities.images.is_some()
                    && self.show_images
                    && !data.is_empty()
                    && !mime_type.is_empty()
                {
                    let (data, mime_type) = match self.converted_images.get(&index) {
                        Some(converted) => (converted.0.clone(), converted.1.clone()),
                        None => (data.clone(), mime_type.clone()),
                    };
                    if capabilities.images == Some(ImageProtocol::Kitty) && mime_type != "image/png"
                    {
                        continue;
                    }

                    self.image_spacers.push(Spacer::new(1));
                    self.image_components.push(Image::new(
                        &data,
                        &mime_type,
                        Box::new(|text: &str| theme().fg("toolOutput", text)),
                        ImageOptions {
                            max_width_cells: Some(self.image_width_cells),
                            ..ImageOptions::default()
                        },
                        None,
                    ));
                }
            }
        }

        if has_renderer && !has_content && self.image_components.is_empty() {
            self.hide_component = true;
        }
        self.dirty = false;
        self.rendered_width = width;
    }

    /// The background colour key (upstream's `bgFn` selection).
    fn bg_key(&self) -> &'static str {
        if self.is_partial {
            "toolPendingBg"
        } else if self.result.as_ref().is_some_and(|r| r.is_error) {
            "toolErrorBg"
        } else {
            "toolSuccessBg"
        }
    }

    /// Run the call renderer, catching panics like upstream's `try/catch`.
    fn render_call_lines(
        &mut self,
        width: usize,
        theme_obj: &crate::modes::interactive::theme::Theme,
        args: &serde_json::Value,
        context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        let renderer = self.call_renderer()?;
        // AssertUnwindSafe: renderer state may be mutated mid-panic; the
        // caller then falls back like upstream's catch block.
        catch_unwind(AssertUnwindSafe(|| {
            renderer.render_call(width, args, theme_obj, context)
        }))
        .ok()
    }

    /// `Some(Some(lines))` / `Some(None)` / `None` mirrors upstream's
    /// "renderer answered / renderer answered `undefined` / no renderer".
    fn render_result_lines(
        &mut self,
        width: usize,
        theme_obj: &crate::modes::interactive::theme::Theme,
        _args: &serde_json::Value,
        result: &ToolExecutionResult,
        context: &ToolRenderContext,
    ) -> Option<Option<Vec<String>>> {
        let options = ToolRenderResultOptions {
            expanded: self.expanded,
            is_partial: self.is_partial,
        };
        let renderer = self.result_renderer()?;
        let render_result = ToolRenderResult {
            content: &result.content,
            details: &result.details,
            is_error: result.is_error,
        };
        catch_unwind(AssertUnwindSafe(|| {
            renderer.render_result(width, &render_result, &options, theme_obj, context)
        }))
        .ok()
    }

    /// Feed rendered lines into the active shell container.
    fn push_render_child(&mut self, lines: Vec<String>) {
        if self.get_render_shell() == ToolRenderShell::SelfRendered {
            self.self_render_container
                .add_child(Box::new(StaticLines(lines)));
        } else {
            self.content_box.add_child(Box::new(StaticLines(lines)));
        }
    }

    /// The sanitized result text (upstream `getTextOutput`).
    fn text_output(&self) -> String {
        let Some(result) = &self.result else {
            return String::new();
        };
        let blocks: Vec<ToolResultBlock> = result
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text, .. } => Some(ToolResultBlock::Text(text.clone())),
                Content::Image { data, mime_type } => Some(ToolResultBlock::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                }),
                _ => None,
            })
            .collect();
        get_text_output(&blocks, self.show_images)
    }

    /// The no-renderer fallback body (upstream `formatToolExecution`).
    fn format_tool_execution(&self) -> String {
        let theme_obj = theme();
        let mut text = theme_obj.fg("toolTitle", &theme_obj.bold(&self.tool_name));
        // JSON.stringify answers undefined for `undefined` args; `null`
        // stringifies to "null".
        if !self.args.is_null() {
            let content = serde_json::to_string_pretty(&self.args).unwrap_or_default();
            if !content.is_empty() {
                text += &format!("\n\n{content}");
            }
        }
        let output = self.text_output();
        if !output.is_empty() {
            text += &format!("\n{output}");
        }
        text
    }

    /// The image blocks with their spacers, rendered at full width.
    fn image_lines(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for i in 0..self.image_components.len() {
            lines.extend(self.image_spacers[i].render(width));
            lines.extend(self.image_components[i].render(width));
        }
        lines
    }
}

/// A background closure reading the live global theme (upstream's inline
/// `theme.bg(key, text)` closures).
fn background_fn(key: &'static str) -> Box<dyn Fn(&str) -> String + Send + Sync> {
    Box::new(move |text: &str| theme().bg(key, text))
}

impl Component for ToolExecutionComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        if self.dirty || self.rendered_width != width {
            self.update_display(width);
        }

        if self.hide_component {
            return Vec::new();
        }

        if self.has_renderer_definition()
            && self.get_render_shell() == ToolRenderShell::SelfRendered
        {
            let content_lines = self.self_render_container.render(width);
            if content_lines.is_empty() && self.image_components.is_empty() {
                return Vec::new();
            }

            let mut lines = Vec::new();
            if !content_lines.is_empty() {
                lines.push(String::new());
                lines.extend(content_lines);
            }
            lines.extend(self.image_lines(width));
            return lines;
        }

        // Upstream `super.render(width)`: the container's children — the
        // leading spacer, the content box (or text), then the images.
        let mut lines = self.spacer.render(width);
        if self.uses_content_box {
            lines.extend(self.content_box.render(width));
        } else {
            lines.extend(self.content_text.render(width));
        }
        lines.extend(self.image_lines(width));
        lines
    }

    fn invalidate(&mut self) {
        self.dirty = true;
        self.content_box.invalidate();
        self.content_text.invalidate();
        self.self_render_container.invalidate();
    }
}
