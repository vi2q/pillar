//! Port of components/markdown-transform.ts: apply the registered Markdown
//! transformers, keeping the source when a transformer fails.

use crate::core::extensions_types::{
    MarkdownMessageType, MarkdownTransformContext, MarkdownTransformer,
};
use pillar_tui::markdown::TransformFn;

/// Builds a fresh [`pillar_tui::markdown::MarkdownTheme`] on demand.
///
/// divergence: upstream shares one `MarkdownTheme` object; the port's theme
/// closures read the global theme per call, so a factory is equivalent (and
/// lets components rebuild their Markdown children).
/// divergence: upstream passes one shared theme object around; the port
/// wraps the factory in an `Arc` so many components can hold it.
pub type MarkdownThemeFactory =
    std::sync::Arc<dyn Fn() -> pillar_tui::markdown::MarkdownTheme + Send + Sync>;

/// The default factory: the active theme's Markdown theme.
pub fn default_markdown_theme_factory() -> MarkdownThemeFactory {
    std::sync::Arc::new(crate::modes::interactive::theme::get_markdown_theme)
}

/// Build a `Markdown` transform hook for one message (upstream
/// `createMarkdownTransform`).
pub fn create_markdown_transform(
    message_type: MarkdownMessageType,
    is_streaming: bool,
    transformers: Vec<MarkdownTransformer>,
) -> Box<TransformFn> {
    Box::new(move |markdown: &str, available_width: usize| {
        let context = MarkdownTransformContext {
            message_type,
            is_streaming,
            available_width,
        };
        apply_markdown_transformers(markdown, &context, &transformers)
    })
}

/// Run each transformer in order; a panic keeps the current Markdown, matching
/// upstream's `catch` (the port has no exceptions, so the host wraps panicking
/// transformers itself and the closure contract is "return the source
/// unchanged on failure").
fn apply_markdown_transformers(
    markdown: &str,
    context: &MarkdownTransformContext,
    transformers: &[MarkdownTransformer],
) -> String {
    let mut transformed = markdown.to_string();
    for transformer in transformers {
        if let Some(next) = transformer(&transformed, context) {
            transformed = next;
        }
    }
    transformed
}
