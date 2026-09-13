//! Port of packages/coding-agent/src/modes/interactive/model-search.ts
//! (pi v0.84.3): the search text used when filtering models.

/// A model entry the selector can filter (upstream `ModelSearchItem`).
pub struct ModelSearchItem<'a> {
    pub id: &'a str,
    pub provider: &'a str,
    pub name: Option<&'a str>,
}

/// Search text for a model list: the bare id leads (upstream
/// `getModelSearchText`).
pub fn model_search_text(item: &ModelSearchItem<'_>) -> String {
    let name = match item.name {
        Some(name) => format!(" {name}"),
        None => String::new(),
    };
    format!(
        "{} {} {}/{} {} {}{}",
        item.id, item.provider, item.provider, item.id, item.provider, item.id, name
    )
}

/// Search text for the `/model` selector (upstream
/// `getModelSelectorSearchText`): the provider leads so exact
/// provider-prefixed queries rank before proxy-provider ids such as
/// `openrouter/openai/gpt-5`.
pub fn model_selector_search_text(item: &ModelSearchItem<'_>) -> String {
    let name = match item.name {
        Some(name) => format!(" {name}"),
        None => String::new(),
    };
    format!(
        "{} {}/{} {} {}{}",
        item.provider, item.provider, item.id, item.provider, item.id, name
    )
}
