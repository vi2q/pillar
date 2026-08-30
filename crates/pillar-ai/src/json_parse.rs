//! Port of packages/ai/src/utils/json-parse.ts (pi v0.84.3).
//!
//! Repairs malformed JSON string literals (raw control characters, invalid
//! escapes) and parses potentially incomplete JSON during streaming,
//! returning the partially-parsed value.
//!
//! divergence: upstream delegates partial parsing to the `partial-json`
//! package. The port implements a small recursive scanner that closes open
//! strings/containers; exotic partial-json edge cases (e.g. `+Infinity`)
//! fall back to an empty object just like upstream's final catch.

use serde_json::{Map, Value};

const VALID_JSON_ESCAPES: [char; 9] = ['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

fn is_control_character(c: char) -> bool {
    (c as u32) <= 0x1f
}

fn escape_control_character(c: char) -> String {
    match c {
        '\u{0008}' => "\\b".to_string(),
        '\u{000c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        other => format!("\\u{:04x}", other as u32),
    }
}

/// Repairs malformed JSON string literals by escaping raw control characters
/// inside strings and doubling backslashes before invalid escape characters.
pub fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::new();
    let mut in_string = false;

    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];

        if !in_string {
            repaired.push(c);
            if c == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        if c == '"' {
            repaired.push(c);
            in_string = false;
            index += 1;
            continue;
        }

        if c == '\\' {
            let next = chars.get(index + 1).copied();
            let Some(next) = next else {
                repaired.push_str("\\\\");
                index += 1;
                continue;
            };

            if next == 'u' {
                let unicode_digits: String = chars
                    .get(index + 2..index + 6)
                    .unwrap_or(&[])
                    .iter()
                    .collect();
                if unicode_digits.len() == 4
                    && unicode_digits.chars().all(|d| d.is_ascii_hexdigit())
                {
                    repaired.push_str(&format!("\\u{unicode_digits}"));
                    index += 6;
                    continue;
                }
            }

            if VALID_JSON_ESCAPES.contains(&next) {
                repaired.push('\\');
                repaired.push(next);
                index += 2;
                continue;
            }

            repaired.push_str("\\\\");
            index += 1;
            continue;
        }

        if is_control_character(c) {
            // escape_control_character returns "\\X" sequences of 2 chars;
            // push them individually.
            for esc in escape_control_character(c).chars() {
                repaired.push(esc);
            }
            index += 1;
            continue;
        }
        repaired.push(c);
        index += 1;
    }

    repaired
}

fn parse_json(json: &str) -> Option<Value> {
    serde_json::from_str(json).ok()
}

/// Parse JSON, falling back to [`repair_json`] when the raw parse fails.
pub fn parse_json_with_repair(json: &str) -> Option<Value> {
    parse_json(json).or_else(|| {
        let repaired = repair_json(json);
        if repaired != json {
            parse_json(&repaired)
        } else {
            None
        }
    })
}

/// Close unterminated strings/containers of a truncated JSON document.
fn close_partial_json(json: &str) -> Option<String> {
    let chars: Vec<char> = json.chars().collect();
    let mut in_string = false;
    let mut escaped = false;
    let mut stack: Vec<char> = Vec::new();

    for &c in &chars {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => stack.push(c),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }

    if stack.is_empty() && !in_string {
        return None;
    }

    let mut repaired: String = chars.into_iter().collect();
    if in_string {
        // A dangling escape before the cut would make the closing quote an
        // escaped quote; drop it first.
        if escaped {
            repaired.pop();
        }
        repaired.push('"');
    }
    // Trim a dangling separator or incomplete key/value.
    let trimmed = repaired.trim_end();
    if trimmed.ends_with(',') {
        repaired = trimmed.trim_end_matches(',').to_string();
    } else if trimmed.ends_with(':') {
        repaired = format!("{trimmed}null");
    }
    for open in stack.into_iter().rev() {
        repaired.push(if open == '{' { '}' } else { ']' });
    }
    Some(repaired)
}

fn partial_parse(json: &str) -> Option<Value> {
    let closed = close_partial_json(json)?;
    parse_json(&closed)
}

/// Attempts to parse potentially incomplete JSON during streaming. Always
/// returns a valid value, even if the JSON is incomplete (empty object on
/// total failure, matching upstream).
pub fn parse_streaming_json(partial_json: Option<&str>) -> Value {
    let Some(partial_json) = partial_json else {
        return Value::Object(Map::new());
    };
    if partial_json.trim().is_empty() {
        return Value::Object(Map::new());
    }

    if let Some(value) = parse_json_with_repair(partial_json) {
        return value;
    }
    if let Some(value) = partial_parse(partial_json) {
        return value;
    }
    if let Some(repaired) = partial_parse(&repair_json(partial_json)) {
        return repaired;
    }
    Value::Object(Map::new())
}
