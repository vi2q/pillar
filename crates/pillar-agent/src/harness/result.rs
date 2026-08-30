//! Port of packages/agent/src/harness/result.ts (pi v0.84.3).
//!
//! Tagged error plumbing shared by the harness modules.
//!
//! divergence: upstream `Result<T, E>` maps to [`std::result::Result`]; the
//! `Result.ok`/`Result.err` helpers and `matchError` have no Rust
//! counterpart (exhaustive `match` on the error type instead). The
//! `TaggedError` class factory maps to the [`TaggedErrorValue`] trait:
//! implementors carry the upstream class name as their tag and reproduce
//! the upstream `toJSON()` shape.

/// Upstream `TaggedErrorValue`: an error value carrying a stable string tag
/// plus its payload fields as JSON (upstream `toJSON`).
pub trait TaggedErrorValue: std::error::Error {
    /// The upstream error tag (`_tag`), equal to the class name.
    fn tag(&self) -> &'static str;

    /// Upstream `toJSON()`: `{ _tag, message, ...payload }`.
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "_tag": self.tag(),
            "message": self.to_string(),
        })
    }
}
