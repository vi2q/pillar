//! Tool-argument validation: the shared core's guarantee that a declared
//! argument schema is *enforced* before `execute` runs.
//!
//! A tool declares a JSON-Schema for its arguments (`Tool::parameters`, the
//! same typebox-shaped object the provider APIs receive). This module turns
//! that declaration into a check the loop runs on every call, the way upstream
//! does in `packages/ai/src/utils/validation.ts` (typebox `Compile` + cache,
//! `Value.Convert`, then `Check`, throwing
//! `Validation failed for tool "…":\n  - path: message\n\nReceived arguments:\n…`).
//!
//! Three properties matter and are tested:
//!
//! - **Fail-closed.** A schema this module cannot interpret (an unsupported
//!   keyword, a malformed facet) is a validation failure, not a pass. A tool
//!   whose schema we do not understand never runs.
//! - **Coercion first, like upstream.** Models routinely send `"5"` for a
//!   number or `null` for an optional property. Optional `null`s are dropped
//!   and primitives are coerced by the declared type before the check, so the
//!   arguments `execute` sees are the arguments that were validated (and what
//!   the authorization hook saw).
//! - **Compiled once per schema.** The parsed schema is cached by its own JSON
//!   text, so a tool called in a tight loop does not re-parse it.
//!
//! ## Supported schema subset
//!
//! `type` (string or array of types), `enum`, `const`, `minimum`, `maximum`,
//! `exclusiveMinimum`, `exclusiveMaximum`, `minLength`, `maxLength`, `pattern`,
//! `minItems`, `maxItems`, `items` (schema or tuple), `properties`, `required`,
//! `additionalProperties` (bool or schema), `anyOf`, `oneOf`, `allOf`, and the
//! boolean forms `true` / `false`.
//!
//! Ignored as annotations: `$schema`, `$id`, `$comment`, `title`,
//! `description`, `default`, `examples`, `deprecated`, `readOnly`,
//! `writeOnly`, `format`, and any `x-…` extension key.
//!
//! Everything else — `$ref`, `patternProperties`, `not`, `if`/`then`/`else`,
//! `dependentRequired`, … — is a compile error. That is deliberate: silently
//! ignoring a keyword we do not implement is how a schema stops being a
//! guarantee.
//!
//! divergence: JSON-Schema `pattern` is ECMA-262 regular expression syntax and
//! this port compiles it with the `regex` crate, which lacks look-around and
//! backreferences. Such a pattern is reported as an invalid schema rather than
//! being approximated.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};

/// The JSON types a schema can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonType {
    Object,
    Array,
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

impl JsonType {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "object" => Self::Object,
            "array" => Self::Array,
            "string" => Self::String,
            "number" => Self::Number,
            "integer" => Self::Integer,
            "boolean" => Self::Boolean,
            "null" => Self::Null,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Null => "null",
        }
    }

    fn matches(self, value: &Value) -> bool {
        match self {
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            Self::Integer => {
                value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
            }
            Self::Boolean => value.is_boolean(),
            Self::Null => value.is_null(),
        }
    }
}

/// What `items` says about an array's elements.
#[derive(Debug, Default)]
enum Items {
    /// No `items` (or a bare `true`): any element is acceptable.
    #[default]
    Any,
    /// One schema for every element.
    Each(Arc<Compiled>),
    /// One schema per position (JSON-Schema tuple form).
    Positional(Vec<Arc<Compiled>>),
}

/// What `additionalProperties` says about undeclared keys.
#[derive(Debug, Default)]
enum Additional {
    /// Unset, or `true`.
    #[default]
    Allowed,
    /// `false`: an undeclared key is a validation failure.
    Forbidden,
    /// A schema every undeclared value must satisfy.
    Schema(Arc<Compiled>),
}

/// A parsed, ready-to-run schema.
#[derive(Debug, Default)]
struct Compiled {
    /// `false` as a schema: no value is acceptable.
    never: bool,
    types: Option<Vec<JsonType>>,
    values: Option<Vec<Value>>,
    constant: Option<Value>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    exclusive_minimum: Option<f64>,
    exclusive_maximum: Option<f64>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    pattern: Option<regex::Regex>,
    pattern_source: Option<String>,
    min_items: Option<usize>,
    max_items: Option<usize>,
    items: Items,
    properties: BTreeMap<String, Arc<Compiled>>,
    required: Vec<String>,
    additional: Additional,
    any_of: Vec<Arc<Compiled>>,
    one_of: Vec<Arc<Compiled>>,
    all_of: Vec<Arc<Compiled>>,
}

/// Keywords that carry no constraint for us and are therefore ignored.
const ANNOTATIONS: [&str; 9] = [
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
    "deprecated",
    "format",
];

fn compile(schema: &Value) -> Result<Arc<Compiled>, String> {
    match schema {
        Value::Bool(true) => Ok(Arc::new(Compiled::default())),
        Value::Bool(false) => Ok(Arc::new(Compiled {
            never: true,
            ..Compiled::default()
        })),
        Value::Object(object) => compile_object(object),
        other => Err(format!(
            "a schema must be an object or a boolean, not {}",
            type_name(other)
        )),
    }
}

fn compile_object(object: &Map<String, Value>) -> Result<Arc<Compiled>, String> {
    let mut compiled = Compiled::default();
    for (keyword, value) in object {
        match keyword.as_str() {
            "type" => compiled.types = Some(parse_types(value)?),
            "enum" => {
                let values = value.as_array().ok_or("`enum` must be an array")?;
                if values.is_empty() {
                    return Err("`enum` must not be empty".to_owned());
                }
                compiled.values = Some(values.clone());
            }
            "const" => compiled.constant = Some(value.clone()),
            "minimum" => compiled.minimum = Some(number(keyword, value)?),
            "maximum" => compiled.maximum = Some(number(keyword, value)?),
            "exclusiveMinimum" => compiled.exclusive_minimum = Some(number(keyword, value)?),
            "exclusiveMaximum" => compiled.exclusive_maximum = Some(number(keyword, value)?),
            "minLength" => compiled.min_length = Some(count(keyword, value)?),
            "maxLength" => compiled.max_length = Some(count(keyword, value)?),
            "pattern" => {
                let source = value
                    .as_str()
                    .ok_or("`pattern` must be a string")?
                    .to_owned();
                let regex = regex::Regex::new(&source).map_err(|error| {
                    format!("`pattern` is not a usable regular expression: {error}")
                })?;
                compiled.pattern = Some(regex);
                compiled.pattern_source = Some(source);
            }
            "minItems" => compiled.min_items = Some(count(keyword, value)?),
            "maxItems" => compiled.max_items = Some(count(keyword, value)?),
            "items" => {
                compiled.items = match value {
                    Value::Array(items) => {
                        Items::Positional(items.iter().map(compile).collect::<Result<Vec<_>, _>>()?)
                    }
                    other => Items::Each(compile(other)?),
                };
            }
            "properties" => {
                let properties = value.as_object().ok_or("`properties` must be an object")?;
                for (name, property) in properties {
                    compiled.properties.insert(name.clone(), compile(property)?);
                }
            }
            "required" => {
                let required = value.as_array().ok_or("`required` must be an array")?;
                for name in required {
                    compiled.required.push(
                        name.as_str()
                            .ok_or("`required` must be an array of property names")?
                            .to_owned(),
                    );
                }
            }
            "additionalProperties" => {
                compiled.additional = match value {
                    Value::Bool(true) => Additional::Allowed,
                    Value::Bool(false) => Additional::Forbidden,
                    other => Additional::Schema(compile(other)?),
                };
            }
            "anyOf" | "oneOf" | "allOf" => {
                let variants = value
                    .as_array()
                    .ok_or_else(|| format!("`{keyword}` must be an array"))?;
                if variants.is_empty() {
                    return Err(format!("`{keyword}` must not be empty"));
                }
                let parsed = variants
                    .iter()
                    .map(compile)
                    .collect::<Result<Vec<_>, _>>()?;
                match keyword.as_str() {
                    "anyOf" => compiled.any_of = parsed,
                    "oneOf" => compiled.one_of = parsed,
                    _ => compiled.all_of = parsed,
                }
            }
            annotation if ANNOTATIONS.contains(&annotation) || annotation.starts_with("x-") => {}
            unsupported => {
                return Err(format!(
                    "the schema keyword `{unsupported}` is not implemented (fail-closed: the \
                     arguments cannot be checked against it)"
                ));
            }
        }
    }
    Ok(Arc::new(compiled))
}

fn parse_types(value: &Value) -> Result<Vec<JsonType>, String> {
    let names: Vec<&Value> = match value {
        Value::String(_) => vec![value],
        Value::Array(names) => {
            if names.is_empty() {
                return Err("`type` must not be empty".to_owned());
            }
            names.iter().collect()
        }
        _ => return Err("`type` must be a string or an array of strings".to_owned()),
    };
    names
        .into_iter()
        .map(|name| {
            let name = name
                .as_str()
                .ok_or("`type` must be a string or an array of strings")?;
            JsonType::parse(name).ok_or_else(|| format!("`type` names an unknown type `{name}`"))
        })
        .collect()
}

fn number(keyword: &str, value: &Value) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("`{keyword}` must be a number"))
}

fn count(keyword: &str, value: &Value) -> Result<usize, String> {
    value
        .as_u64()
        .map(|count| count as usize)
        .ok_or_else(|| format!("`{keyword}` must be a non-negative whole number"))
}

/// Validate (and coerce, like upstream) a tool call's arguments against the
/// schema the tool declared.
///
/// `Ok` is the argument object `execute` should receive — coerced, with
/// optional `null`s dropped. `Err` is the `"  - path: message"` lines the
/// caller embeds in the upstream-shaped message, joined by newlines.
pub fn validate_tool_arguments(schema: &Value, arguments: &Value) -> Result<Value, String> {
    let compiled = match compiled(schema) {
        Ok(compiled) => compiled,
        Err(error) => return Err(format!("  - schema: {error}")),
    };

    let mut arguments = arguments.clone();
    normalize_optional_nulls(&mut arguments, &compiled);
    arguments = coerce(arguments, &compiled);

    let mut errors = Vec::new();
    check(&arguments, &compiled, "", &mut errors, &compiled);
    if errors.is_empty() {
        return Ok(arguments);
    }
    Err(errors
        .iter()
        .map(|error| format!("  - {error}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// The compiled form of `schema`, cached by the schema's own JSON text (a tool
/// set is small and stable, so this is bounded by the number of distinct tool
/// schemas a process sees).
type CompiledCache = HashMap<String, Result<Arc<Compiled>, String>>;

fn compiled(schema: &Value) -> Result<Arc<Compiled>, String> {
    static CACHE: OnceLock<Mutex<CompiledCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = serde_json::to_string(schema).unwrap_or_else(|_| format!("{schema:?}"));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = cache.get(&key) {
        return cached.clone();
    }
    let result = compile(schema);
    // A schema that fails to compile is cached too: the error is deterministic
    // and recompiling it on every call would be pure cost.
    cache.insert(key, result.clone());
    result
}

/// Upstream `normalizeOptionalNulls`: an explicit `null` for an *optional*
/// property that the property schema does not accept is dropped rather than
/// coerced, so `{"value": null}` behaves like an omitted `value`.
fn normalize_optional_nulls(value: &mut Value, schema: &Compiled) {
    match value {
        Value::Array(values) => match &schema.items {
            Items::Each(item) => {
                for value in values.iter_mut() {
                    normalize_optional_nulls(value, item);
                }
            }
            Items::Positional(items) => {
                for (index, value) in values.iter_mut().enumerate() {
                    if let Some(item) = items.get(index) {
                        normalize_optional_nulls(value, item);
                    }
                }
            }
            Items::Any => {}
        },
        Value::Object(object) => {
            for (name, property) in &schema.properties {
                let Some(current) = object.get_mut(name) else {
                    continue;
                };
                if current.is_null()
                    && !schema.required.iter().any(|required| required == name)
                    && !accepts_null(property)
                {
                    object.remove(name);
                } else {
                    normalize_optional_nulls(current, property);
                }
            }
        }
        _ => {}
    }
}

fn accepts_null(schema: &Compiled) -> bool {
    !schema.never
        && schema.types.as_ref().is_some_and(|types| {
            types
                .iter()
                .any(|declared| *declared == JsonType::Null || declared.matches(&Value::Null))
        })
}

/// Upstream `coerceWithJsonSchema` / typebox `Value.Convert`: make the common
/// model mistakes (`"5"` for a number, `"true"` for a boolean, `null` for a
/// scalar) into the declared type before checking.
fn coerce(value: Value, schema: &Compiled) -> Value {
    let mut value = value;

    for nested in &schema.all_of {
        value = coerce(value, nested);
    }

    for variants in [&schema.any_of, &schema.one_of] {
        if !variants.is_empty() && !variants.iter().any(|variant| is_valid(&value, variant)) {
            for variant in variants {
                let candidate = coerce(value.clone(), variant);
                if is_valid(&candidate, variant) {
                    value = candidate;
                    break;
                }
            }
        }
    }

    if let Some(types) = &schema.types {
        let declared_matches =
            types.len() > 1 && types.iter().any(|declared| declared.matches(&value));
        if !types.iter().any(|declared| declared.matches(&value)) && !declared_matches {
            for declared in types {
                let candidate = coerce_primitive(&value, *declared);
                if candidate != value {
                    value = candidate;
                    break;
                }
            }
        }
    }

    let wants_object = wants(schema, JsonType::Object);
    let wants_array = wants(schema, JsonType::Array);

    if wants_object && let Value::Object(object) = &mut value {
        let declared: Vec<String> = schema.properties.keys().cloned().collect();
        for (name, property) in &schema.properties {
            if let Some(current) = object.remove(name) {
                let coerced = coerce(current, property);
                object.insert(name.clone(), coerced);
            }
        }
        if let Additional::Schema(additional) = &schema.additional {
            for (name, current) in std::mem::take(object) {
                if declared.contains(&name) {
                    continue;
                }
                let coerced = coerce(current, additional);
                object.insert(name, coerced);
            }
        }
    }

    if wants_array && let Value::Array(values) = value {
        let mut values = values;
        match &schema.items {
            Items::Each(item) => {
                values = values
                    .into_iter()
                    .map(|value| coerce(value, item))
                    .collect();
            }
            Items::Positional(items) => {
                for (index, item) in items.iter().enumerate() {
                    if let Some(value) = values.get_mut(index).map(std::mem::take) {
                        values[index] = coerce(value, item);
                    }
                }
            }
            Items::Any => {}
        }
        value = Value::Array(values);
    }

    value
}

/// Whether the schema accepts the declared type at all (no `type` means any).
fn wants(schema: &Compiled, declared: JsonType) -> bool {
    match &schema.types {
        Some(types) => types.contains(&declared),
        None => false,
    }
}

fn coerce_primitive(value: &Value, declared: JsonType) -> Value {
    match declared {
        JsonType::Number => match value {
            Value::Null => Value::from(0),
            Value::String(text) if !text.trim().is_empty() => match text.trim().parse::<f64>() {
                Ok(parsed) if parsed.is_finite() => Value::from(parsed),
                _ => value.clone(),
            },
            Value::Bool(true) => Value::from(1),
            Value::Bool(false) => Value::from(0),
            other => other.clone(),
        },
        JsonType::Integer => match value {
            Value::Null => Value::from(0),
            Value::String(text) if !text.trim().is_empty() => match text.trim().parse::<i64>() {
                Ok(parsed) => Value::from(parsed),
                _ => value.clone(),
            },
            Value::Bool(true) => Value::from(1),
            Value::Bool(false) => Value::from(0),
            other => other.clone(),
        },
        JsonType::Boolean => match value {
            Value::Null => Value::from(false),
            Value::String(text) if text == "true" => Value::from(true),
            Value::String(text) if text == "false" => Value::from(false),
            Value::Number(number) if number.as_f64() == Some(1.0) => Value::from(true),
            Value::Number(number) if number.as_f64() == Some(0.0) => Value::from(false),
            other => other.clone(),
        },
        JsonType::String => match value {
            Value::Null => Value::from(""),
            Value::Number(number) => Value::from(number.to_string()),
            Value::Bool(flag) => Value::from(flag.to_string()),
            other => other.clone(),
        },
        JsonType::Null => match value {
            Value::String(text) if text.is_empty() => Value::Null,
            Value::Number(number) if number.as_f64() == Some(0.0) => Value::Null,
            Value::Bool(false) => Value::Null,
            other => other.clone(),
        },
        JsonType::Object | JsonType::Array => value.clone(),
    }
}

/// Collect the ways `value` fails `schema` (upstream `validator.Errors`).
/// `root` is the full compiled schema, used for `anyOf`/`oneOf` presence checks
/// to stay cheap; `path` is the location shown in the message (empty at the
/// root, where upstream prints `root`).
fn check(value: &Value, schema: &Compiled, path: &str, errors: &mut Vec<String>, root: &Compiled) {
    let _ = root;
    let shown = || {
        if path.is_empty() {
            "root".to_owned()
        } else {
            path.to_owned()
        }
    };

    if schema.never {
        errors.push(format!("{}: this value is not allowed here", shown()));
        return;
    }

    for nested in &schema.all_of {
        check(value, nested, path, errors, nested);
    }

    if !schema.any_of.is_empty() && !schema.any_of.iter().any(|variant| is_valid(value, variant)) {
        errors.push(format!(
            "{}: must match at least one of the {} allowed schemas",
            shown(),
            schema.any_of.len()
        ));
    }

    if !schema.one_of.is_empty() {
        let matching = schema
            .one_of
            .iter()
            .filter(|variant| is_valid(value, variant))
            .count();
        if matching != 1 {
            errors.push(format!(
                "{}: must match exactly one of the {} allowed schemas (matched {matching})",
                shown(),
                schema.one_of.len()
            ));
        }
    }

    if let Some(constant) = &schema.constant
        && value != constant
    {
        errors.push(format!(
            "{}: must be {}",
            shown(),
            serde_json::to_string(constant).unwrap_or_default()
        ));
    }

    if let Some(values) = &schema.values
        && !values.contains(value)
    {
        errors.push(format!(
            "{}: must be one of {}",
            shown(),
            serde_json::to_string(values).unwrap_or_default()
        ));
    }

    if let Some(types) = &schema.types
        && !types.iter().any(|declared| declared.matches(value))
    {
        let expected: Vec<&str> = types.iter().map(|declared| declared.name()).collect();
        errors.push(format!(
            "{}: expected {} (found {})",
            shown(),
            expected.join(" or "),
            type_name(value)
        ));
        // The remaining facets are all type-specific; reporting them against a
        // value of the wrong type would be noise.
        return;
    }

    if let Some(number) = value.as_f64() {
        if let Some(minimum) = schema.minimum
            && number < minimum
        {
            errors.push(format!("{}: must be >= {minimum}", shown()));
        }
        if let Some(maximum) = schema.maximum
            && number > maximum
        {
            errors.push(format!("{}: must be <= {maximum}", shown()));
        }
        if let Some(minimum) = schema.exclusive_minimum
            && number <= minimum
        {
            errors.push(format!("{}: must be > {minimum}", shown()));
        }
        if let Some(maximum) = schema.exclusive_maximum
            && number >= maximum
        {
            errors.push(format!("{}: must be < {maximum}", shown()));
        }
    }

    if let Some(text) = value.as_str() {
        if let Some(minimum) = schema.min_length
            && text.chars().count() < minimum
        {
            errors.push(format!(
                "{}: must NOT have fewer than {minimum} characters",
                shown()
            ));
        }
        if let Some(maximum) = schema.max_length
            && text.chars().count() > maximum
        {
            errors.push(format!(
                "{}: must NOT have more than {maximum} characters",
                shown()
            ));
        }
        if let Some(pattern) = &schema.pattern
            && !pattern.is_match(text)
        {
            errors.push(format!(
                "{}: must match the pattern \"{}\"",
                shown(),
                schema.pattern_source.as_deref().unwrap_or_default()
            ));
        }
    }

    if let Some(values) = value.as_array() {
        if let Some(minimum) = schema.min_items
            && values.len() < minimum
        {
            errors.push(format!(
                "{}: must NOT have fewer than {minimum} items",
                shown()
            ));
        }
        if let Some(maximum) = schema.max_items
            && values.len() > maximum
        {
            errors.push(format!(
                "{}: must NOT have more than {maximum} items",
                shown()
            ));
        }
        match &schema.items {
            Items::Each(item) => {
                for (index, value) in values.iter().enumerate() {
                    let child = child_path(path, &index.to_string());
                    check(value, item, &child, errors, item);
                }
            }
            Items::Positional(items) => {
                for (index, value) in values.iter().enumerate() {
                    if let Some(item) = items.get(index) {
                        let child = child_path(path, &index.to_string());
                        check(value, item, &child, errors, item);
                    }
                }
            }
            Items::Any => {}
        }
    }

    if let Some(object) = value.as_object() {
        for name in &schema.required {
            if !object.contains_key(name) {
                // Upstream reports a missing property at the property's own
                // path (`value`, not `root.value`).
                errors.push(format!(
                    "{}: expected a required property",
                    child_path(path, name)
                ));
            }
        }
        for (name, value) in object {
            let declared = schema.properties.get(name);
            let child = child_path(path, name);
            match (declared, &schema.additional) {
                (Some(property), _) => check(value, property, &child, errors, property),
                (None, Additional::Forbidden) => {
                    errors.push(format!("{child}: unexpected property"))
                }
                (None, Additional::Schema(additional)) => {
                    check(value, additional, &child, errors, additional)
                }
                (None, Additional::Allowed) => {}
            }
        }
    }
}

fn child_path(path: &str, child: &str) -> String {
    if path.is_empty() {
        child.to_owned()
    } else {
        format!("{path}.{child}")
    }
}

/// Whether `value` satisfies `schema` outright (the `anyOf`/`oneOf` probe).
fn is_valid(value: &Value, schema: &Compiled) -> bool {
    let mut errors = Vec::new();
    check(value, schema, "", &mut errors, schema);
    errors.is_empty()
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn validate(schema: Value, arguments: Value) -> Result<Value, String> {
        validate_tool_arguments(&schema, &arguments)
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "count": {"type": "number", "minimum": 0},
                "mode": {"type": "string", "enum": ["fast", "slow"]},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "nested": {
                    "type": "object",
                    "properties": {"flag": {"type": "boolean"}},
                    "required": ["flag"],
                    "additionalProperties": false
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    #[test]
    fn accepts_a_conforming_object() {
        let arguments = json!({"name": "x", "count": 1, "mode": "fast", "tags": ["a"]});
        assert_eq!(validate(schema(), arguments.clone()).unwrap(), arguments);
    }

    #[test]
    fn rejects_a_missing_required_property() {
        let error = validate(schema(), json!({})).unwrap_err();
        assert_eq!(error, "  - name: expected a required property");
    }

    #[test]
    fn rejects_a_type_mismatch_with_the_path() {
        let error = validate(schema(), json!({"name": "x", "count": "not a number"})).unwrap_err();
        assert_eq!(error, "  - count: expected number (found string)");
    }

    #[test]
    fn rejects_nested_enum_and_range_violations() {
        let error = validate(
            schema(),
            json!({"name": "x", "count": -1, "mode": "medium"}),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "  - count: must be >= 0\n  - mode: must be one of [\"fast\",\"slow\"]"
        );
    }

    #[test]
    fn rejects_a_nested_violation_and_an_undeclared_key() {
        let error = validate(
            schema(),
            json!({"name": "x", "nested": {"flag": true}, "extra": 1}),
        )
        .unwrap_err();
        assert_eq!(error, "  - extra: unexpected property");

        let error = validate(schema(), json!({"name": "x", "nested": {}})).unwrap_err();
        assert_eq!(error, "  - nested.flag: expected a required property");
    }

    #[test]
    fn coerces_like_upstream_before_checking() {
        let validated = validate(schema(), json!({"name": 7, "count": "3"})).unwrap();
        assert_eq!(validated, json!({"name": "7", "count": 3.0}));
    }

    #[test]
    fn drops_an_optional_null_instead_of_coercing_it() {
        // `name` is required, so its null is coerced to ""; the optional
        // `count` null is dropped (upstream `normalizeOptionalNulls`).
        let validated = validate(schema(), json!({"name": null, "count": null})).unwrap();
        assert_eq!(validated, json!({"name": ""}));
    }

    #[test]
    fn a_schema_we_cannot_check_is_a_failure_not_a_pass() {
        let unsupported = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "patternProperties": {"^x": {"type": "string"}}
        });
        let error = validate(unsupported, json!({"name": "x"})).unwrap_err();
        assert!(
            error.contains("`patternProperties` is not implemented"),
            "{error}"
        );

        let malformed = json!({"type": "objct"});
        let error = validate(malformed, json!({})).unwrap_err();
        assert!(error.contains("unknown type `objct`"), "{error}");
    }

    #[test]
    fn union_and_tuple_forms_work() {
        let union = json!({
            "type": "object",
            "properties": {"value": {"anyOf": [{"type": "string"}, {"type": "number"}]}},
            "required": ["value"]
        });
        assert_eq!(
            validate(union.clone(), json!({"value": "text"})).unwrap(),
            json!({"value": "text"})
        );
        assert_eq!(
            validate(union.clone(), json!({"value": 5})).unwrap(),
            json!({"value": 5})
        );
        // Upstream coerces into a union member when one accepts the value.
        assert_eq!(
            validate(union.clone(), json!({"value": true})).unwrap(),
            json!({"value": "true"})
        );
        // …and refuses when no member can.
        assert!(validate(union, json!({"value": {}})).is_err());

        let tuple = json!({"type": "array", "items": [{"type": "string"}, {"type": "number"}]});
        assert!(validate(tuple.clone(), json!(["a", 1])).is_ok());
        assert!(validate(tuple, json!(["a", "b"])).is_err());
    }

    #[test]
    fn the_boolean_forms_are_honored() {
        assert!(validate(json!(true), json!({"anything": 1})).is_ok());
        assert!(validate(json!(false), json!({"anything": 1})).is_err());
    }
}
