//! Port of components/markdown-transform.ts: apply the registered Markdown
//! transformers, keeping the source when a transformer fails.

use crate::core::extensions_types::{
    MarkdownMessageType, MarkdownTransformContext, MarkdownTransformer,
};
use pillar_tui::markdown::TransformFn;

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
