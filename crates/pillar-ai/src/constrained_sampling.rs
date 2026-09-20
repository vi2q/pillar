//! Port of packages/ai/src/api/constrained-sampling.ts (pi v0.84.3).
//!
//! Shared constrained-sampling support: strict JSON-schema conversion for
//! providers with structured-output modes, and grammar (lark/regex) tool
//! input buffering. Schemas are `serde_json::Value` (upstream: typebox /
//! plain JSON Schema objects).
//!
//! divergence: upstream mutates a `structuredClone` of the schema and
//! preserves JSON object key insertion order; `serde_json::Value` maps are
//! ordered (BTreeMap by default), so the generated `required` arrays are in
//! key order rather than source order.

use serde_json::{Map, Value, json};

use crate::error::AiError;
use crate::types::{ConstrainedSamplingConfig, ConstrainedStrictness, Tool};

/// A schema that cannot be safely converted to the strict subset. The
/// message is the raw reason (upstream: `UnsupportedStrictJsonSchemaError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedStrictJsonSchemaError(pub String);

impl std::fmt::Display for UnsupportedStrictJsonSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn as_schema_object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn is_structured_schema(schema: &Value) -> bool {
    let Some(map) = as_schema_object(schema) else {
        return false;
    };
    let types = match &map.get("type") {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    types.iter().any(|text| text == "object" || text == "array")
        || map.contains_key("properties")
        || map.contains_key("items")
}

fn schema_allows_null(schema: &Value) -> bool {
    let Some(map) = as_schema_object(schema) else {
        return false;
    };
    match map.get("type") {
        Some(Value::String(text)) if text == "null" => return true,
        Some(Value::Array(items))
            if items
                .iter()
                .any(|item| item == &Value::String("null".to_string())) =>
        {
            return true;
        }
        _ => {}
    }
    if map.get("const").is_some_and(Value::is_null) {
        return true;
    }
    if let Some(Value::Array(items)) = map.get("enum")
        && items.iter().any(Value::is_null)
    {
        return true;
    }
    match map.get("anyOf") {
        Some(Value::Array(variants)) => variants.iter().any(schema_allows_null),
        _ => false,
    }
}

fn unsupported(reason: impl Into<String>) -> UnsupportedStrictJsonSchemaError {
    UnsupportedStrictJsonSchemaError(reason.into())
}

fn make_json_schema_node_strict(
    schema: &mut Value,
) -> Result<(), UnsupportedStrictJsonSchemaError> {
    let is_object = schema.is_object();
    if !is_object {
        return Err(unsupported("boolean schemas are unsupported"));
    }
    let map = schema.as_object_mut().expect("checked object above");
    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if map.contains_key(*key) {
            return Err(unsupported(format!("{key} schemas are unsupported")));
        }
    }

    if let Some(Value::Array(any_of)) = map.get_mut("anyOf") {
        if any_of.is_empty() {
            return Err(unsupported("anyOf must contain at least one schema"));
        }
        for variant in any_of {
            if is_structured_schema(variant) {
                return Err(unsupported("object and array unions are unsupported"));
            }
            make_json_schema_node_strict(variant)?;
        }
    }

    if let Some(items) = map.get_mut("items") {
        if items.is_array() {
            return Err(unsupported("tuple schemas are unsupported"));
        }
        make_json_schema_node_strict(items)?;
    }

    let type_is_object = map.get("type") == Some(&Value::String("object".to_string()));
    if map.contains_key("properties") && !type_is_object {
        return Err(unsupported("properties require type object"));
    }
    if !type_is_object {
        return Ok(());
    }
    match map.get("additionalProperties") {
        Some(value) if !value.is_boolean() || value == &Value::Bool(true) => {
            return Err(unsupported(
                "schema-valued or true additionalProperties is unsupported",
            ));
        }
        _ => {}
    }
    if let Some(properties) = map.get("properties")
        && !properties.is_object()
    {
        return Err(unsupported("object properties must be a schema map"));
    }
    if let Some(required) = map.get("required") {
        let valid = required
            .as_array()
            .is_some_and(|keys| keys.iter().all(|key| key.is_string()));
        if !valid {
            return Err(unsupported("object required must be a string array"));
        }
    }

    let properties = match map.get_mut("properties") {
        Some(Value::Object(properties)) => std::mem::take(properties),
        _ => Map::new(),
    };
    let required: Vec<String> = match map.get("required") {
        Some(Value::Array(keys)) => keys
            .iter()
            .filter_map(|key| key.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let property_names: Vec<String> = properties.keys().cloned().collect();
    if required.iter().any(|key| !property_names.contains(key)) {
        return Err(unsupported("required contains an unknown property"));
    }

    let mut strict_properties = Map::new();
    for (key, property) in properties {
        let mut strict_property = property.clone();
        make_json_schema_node_strict(&mut strict_property)?;
        if !required.contains(&key) && !schema_allows_null(&strict_property) {
            strict_property = json!({ "anyOf": [strict_property, { "type": "null" }] });
        }
        strict_properties.insert(key, strict_property);
    }
    map.insert("properties".to_string(), Value::Object(strict_properties));
    map.insert(
        "required".to_string(),
        Value::Array(property_names.into_iter().map(Value::String).collect()),
    );
    map.insert("additionalProperties".to_string(), Value::Bool(false));
    Ok(())
}

/// Convert a tool schema to the strict subset expected by provider
/// constrained sampling. Returns a converted clone; the input is unchanged
/// (upstream: `structuredClone` then mutate).
pub fn make_strict_json_schema(schema: &Value) -> Result<Value, UnsupportedStrictJsonSchemaError> {
    let mut cloned = schema.clone();
    if !cloned.is_object() {
        return Err(unsupported("root schema must have type object"));
    }
    make_json_schema_node_strict(&mut cloned)?;
    if cloned.get("type") != Some(&Value::String("object".to_string())) {
        return Err(unsupported("root schema must have type object"));
    }
    Ok(cloned)
}

/// Tool parameters as sent to the provider: strict-converted when the
/// provider supports strict mode and the tool opted in.
pub fn get_json_schema_tool_parameters(
    tool: &Tool,
    strict: Option<bool>,
) -> Result<Value, UnsupportedStrictJsonSchemaError> {
    if strict == Some(true) {
        make_strict_json_schema(&tool.parameters)
    } else {
        Ok(tool.parameters.clone())
    }
}

/// Grammar-constrained tool input descriptor (upstream
/// `GrammarConstrainedSampling`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarConstrainedSampling {
    pub format: &'static str,
    pub definition: String,
    pub input_property: String,
}

/// Streaming buffer that re-wraps a grammar tool's raw text input into the
/// JSON `{"<property>":"..."}` argument delta stream.
#[derive(Debug, Clone, Default)]
pub struct GrammarToolInputJsonBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

pub fn get_grammar_tool_input(
    tool_name: &str,
    arguments: &Value,
    input_property: &str,
) -> Result<String, AiError> {
    let input = arguments
        .get(input_property)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AiError::Other(format!(
                "Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string."
            ))
        })?;
    Ok(input.to_string())
}

/// Append the next raw input chunk to the buffer, returning the JSON
/// argument delta to emit (or `None` when nothing changed).
pub fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputJsonBuffer,
    input_property: &str,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, AiError> {
    if buffer.closed {
        if close && next_input == buffer.input {
            return Ok(None);
        }
        return Err(AiError::Other(format!(
            "grammar tool input for property \"{input_property}\" changed after it was closed"
        )));
    }
    if !next_input.starts_with(&buffer.input) {
        return Err(AiError::Other(format!(
            "grammar tool input for property \"{input_property}\" changed non-monotonically"
        )));
    }

    let input_delta = &next_input[buffer.input.len()..];
    if !close && input_delta.is_empty() {
        return Ok(None);
    }

    let mut delta = String::new();
    if !buffer.started {
        let key = serde_json::to_string(input_property).map_err(|error| {
            AiError::Other(format!("grammar: failed to encode property name: {error}"))
        })?;
        delta.push('{');
        delta.push_str(&key);
        delta.push_str(":\"");
        buffer.started = true;
    }
    let escaped = serde_json::to_string(input_delta).map_err(|error| {
        AiError::Other(format!("grammar: failed to encode input delta: {error}"))
    })?;
    delta.push_str(&escaped[1..escaped.len() - 1]);
    buffer.input = next_input.to_string();

    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta))
}

fn infer_grammar_input_property(tool: &Tool) -> Result<String, AiError> {
    let schema = &tool.parameters;
    if schema.get("type") != Some(&Value::String("object".to_string())) {
        return Err(AiError::Other(
            "grammar constrained sampling requires an object parameter schema".to_string(),
        ));
    }
    let required = schema.get("required").and_then(Value::as_array);
    let input_property = match required {
        Some(keys) if keys.len() == 1 && keys[0].is_string() => {
            keys[0].as_str().expect("checked string").to_string()
        }
        _ => {
            return Err(AiError::Other(
                "grammar constrained sampling requires exactly one required string property"
                    .to_string(),
            ));
        }
    };
    let property = schema
        .get("properties")
        .and_then(|properties| properties.get(&input_property));
    let Some(property) = property else {
        return Err(AiError::Other(format!(
            "grammar constrained sampling requires a properties entry for {input_property}"
        )));
    };
    if property.get("type") != Some(&Value::String("string".to_string())) {
        return Err(AiError::Other(format!(
            "grammar constrained sampling property {input_property} must have type string"
        )));
    }
    Ok(input_property)
}

/// Resolve JSON-schema strict sampling for a tool: `Some(true)` when the
/// schema converts cleanly, `None` when strict mode is unsupported or the
/// schema only needs the non-strict path. `strict: require` tools fail when
/// strict mode is unavailable or the schema cannot convert.
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, AiError> {
    let Some(ConstrainedSamplingConfig::JsonSchema { strict }) = &tool.constrained_sampling else {
        return Ok(None);
    };

    if supports_strict_mode {
        return match make_strict_json_schema(&tool.parameters) {
            Ok(_) => Ok(Some(true)),
            Err(error) => {
                if *strict == ConstrainedStrictness::Require {
                    return Err(AiError::Other(format!(
                        "Tool \"{}\" requires JSON-schema constrained sampling, but {error}.",
                        tool.name
                    )));
                }
                Ok(None)
            }
        };
    }
    if *strict == ConstrainedStrictness::Require {
        return Err(AiError::Other(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        )));
    }
    Ok(None)
}

/// Resolve grammar constrained sampling for a tool, preferring the lark
/// variant. Returns `None` when the provider lacks grammar tools or the tool
/// opted out; fails when the tool opted in but no supported variant exists.
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_openai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, AiError> {
    let Some(ConstrainedSamplingConfig::Grammar { variants }) = &tool.constrained_sampling else {
        return Ok(None);
    };
    if !supports_openai_grammar_tools {
        return Ok(None);
    }

    let lark_definition = variants.get("openai_lark");
    let regex_definition = variants.get("openai_regex");
    let has_lark = lark_definition.is_some_and(|definition| !definition.trim().is_empty());
    let has_regex = regex_definition.is_some_and(|definition| !definition.trim().is_empty());
    if !has_lark && !has_regex {
        return Err(AiError::Other(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
            tool.name
        )));
    }

    let (format, definition) = if has_lark {
        (
            "lark",
            lark_definition.expect("checked lark variant").clone(),
        )
    } else {
        (
            "regex",
            regex_definition.expect("checked regex variant").clone(),
        )
    };
    let input_property = infer_grammar_input_property(tool).map_err(|error| {
        AiError::Other(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {}.",
            tool.name,
            error_message(&error)
        ))
    })?;
    Ok(Some(GrammarConstrainedSampling {
        format,
        definition,
        input_property,
    }))
}

/// Map tool names to their grammar input property for all grammar tools.
pub fn create_grammar_tool_input_properties(
    tools: Option<&[Tool]>,
    supports_openai_grammar_tools: bool,
) -> std::collections::BTreeMap<String, String> {
    let mut properties = std::collections::BTreeMap::new();
    for tool in tools.unwrap_or(&[]) {
        if let Ok(Some(grammar)) =
            resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)
        {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    properties
}

/// `AiError` has no structured variants; extract the message for composing.
fn error_message(error: &AiError) -> String {
    error.to_string()
}
