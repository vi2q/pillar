//! Port of packages/coding-agent/src/core/http-dispatcher.ts (pi v0.84.3),
//! the settings-related half: the HTTP idle-timeout setting parsing and
//! formatting shared by the settings manager. The dispatcher's transport
//! wiring is Node-specific and not ported.

/// Default HTTP idle timeout (upstream `DEFAULT_HTTP_IDLE_TIMEOUT_MS`).
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;

/// Idle-timeout UI choices (upstream `HTTP_IDLE_TIMEOUT_CHOICES`).
pub const HTTP_IDLE_TIMEOUT_CHOICES: [(&str, u64); 6] = [
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("10 min", 600_000),
    ("disabled", 0),
];

/// Parse the `httpIdleTimeoutMs` setting: "disabled" -> 0, finite
/// non-negative numbers floor to milliseconds, everything else is None
/// (upstream `parseHttpIdleTimeoutMs`).
pub fn parse_http_idle_timeout_ms(value: Option<&serde_json::Value>) -> Option<u64> {
    match value {
        Some(serde_json::Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            if trimmed.is_empty() {
                return None;
            }
            parse_http_idle_timeout_ms(Some(&serde_json::json!(trimmed.parse::<f64>().ok()?)))
        }
        Some(serde_json::Value::Number(number)) => {
            let value = number.as_f64()?;
            if !value.is_finite() || value < 0.0 {
                return None;
            }
            Some(value.floor() as u64)
        }
        _ => None,
    }
}

/// Format an idle timeout for display (upstream `formatHttpIdleTimeoutMs`).
pub fn format_http_idle_timeout_ms(timeout_ms: u64) -> String {
    for (label, value) in HTTP_IDLE_TIMEOUT_CHOICES {
        if value == timeout_ms {
            return label.to_string();
        }
    }
    format!("{} sec", timeout_ms / 1000)
}
