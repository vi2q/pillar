//! Vendor-neutral telemetry contracts and reference adapters. Port of pi
//! `packages/telemetry` (pi v0.84.3, commit `56700d4`).
//!
//! divergence: the upstream TypeScript schema-inference layer
//! (`defineTelemetrySchema`, `createTypedSpanStarter`, conditional types) is
//! compile-time machinery with no runtime behavior; Rust callers express
//! closed-set guarantees with enums and pass attribute maps directly. Span
//! handles are owned (`Arc`-based) so futures are `'static`, which the
//! upstream promise-based callbacks already effectively require. The
//! conformance cases live in `tests/conformance.rs`.

#![forbid(unsafe_code)]
pub mod memory;
pub mod noop;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

pub use memory::{InMemoryTelemetryContext, RecordedTelemetryEvent, RecordedTelemetrySpan};
pub use noop::{NoopTelemetryContext, NoopTelemetrySpan};

pub type SpanAttributes = BTreeMap<String, AttributeValue>;

#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Bool(bool),
    StringArray(Vec<String>),
    NumberArray(Vec<f64>),
    BoolArray(Vec<bool>),
}

impl From<&str> for AttributeValue {
    fn from(value: &str) -> Self {
        AttributeValue::String(value.to_owned())
    }
}

impl From<String> for AttributeValue {
    fn from(value: String) -> Self {
        AttributeValue::String(value)
    }
}

impl From<f64> for AttributeValue {
    fn from(value: f64) -> Self {
        AttributeValue::Number(value)
    }
}

impl From<i64> for AttributeValue {
    fn from(value: i64) -> Self {
        AttributeValue::Number(value as f64)
    }
}

impl From<u64> for AttributeValue {
    fn from(value: u64) -> Self {
        AttributeValue::Number(value as f64)
    }
}

impl From<bool> for AttributeValue {
    fn from(value: bool) -> Self {
        AttributeValue::Bool(value)
    }
}

impl From<Vec<String>> for AttributeValue {
    fn from(value: Vec<String>) -> Self {
        AttributeValue::StringArray(value)
    }
}

impl From<Vec<f64>> for AttributeValue {
    fn from(value: Vec<f64>) -> Self {
        AttributeValue::NumberArray(value)
    }
}

impl From<Vec<bool>> for AttributeValue {
    fn from(value: Vec<bool>) -> Self {
        AttributeValue::BoolArray(value)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: SpanAttributes,
}

impl SpanOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            attributes: BTreeMap::new(),
        }
    }

    pub fn with_attribute(
        mut self,
        name: impl Into<String>,
        value: impl Into<AttributeValue>,
    ) -> Self {
        self.attributes.insert(name.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpanError {
    None,
    Some { name: String, message: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpanStatus {
    Ok,
    Error { error: SpanError },
}

impl SpanStatus {
    pub fn error(name: impl Into<String>, message: impl Into<String>) -> Self {
        SpanStatus::Error {
            error: SpanError::Some {
                name: name.into(),
                message: message.into(),
            },
        }
    }

    pub fn error_without_details() -> Self {
        SpanStatus::Error {
            error: SpanError::None,
        }
    }
}

/// Owned span handle. Recording calls are inert after settlement.
#[derive(Clone)]
pub struct SpanHandle {
    inner: Arc<dyn SpanHandleInner>,
}

pub trait SpanHandleInner: Send + Sync {
    fn add_event(&self, name: &str, attributes: SpanAttributes);
    fn set_attributes(&self, attributes: SpanAttributes);
    fn set_status(&self, status: SpanStatus);
    fn start_child_erased(&self, options: SpanOptions, callback: ChildFn) -> ChildFuture;
}

impl SpanHandle {
    pub fn add_event(&self, name: impl Into<String>, attributes: SpanAttributes) {
        self.inner.add_event(&name.into(), attributes);
    }

    pub fn set_attributes(&self, attributes: SpanAttributes) {
        self.inner.set_attributes(attributes);
    }

    pub fn set_status(&self, status: SpanStatus) {
        self.inner.set_status(status);
    }

    /// Starts a child span with a typed callback; the child settles before
    /// its value is returned.
    pub async fn start_child<T, F, Fut>(&self, options: SpanOptions, callback: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(SpanHandle, SpanStarter) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let boxed = self
            .inner
            .start_child_erased(
                options,
                ChildFn(Box::new(move |child, starter| {
                    Box::pin(async move {
                        let value: T = callback(child, starter).await;
                        Box::new(value) as Box<dyn std::any::Any + Send>
                    })
                })),
            )
            .await;
        match boxed.downcast::<T>() {
            Ok(value) => *value,
            Err(_) => panic!("child span callback returned the wrong type"),
        }
    }
}

impl std::fmt::Debug for SpanHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanHandle").finish()
    }
}

/// Starter for child spans obtained from a context.
#[derive(Clone)]
pub struct SpanStarter {
    inner: Arc<dyn SpanStarterInner>,
}

pub trait SpanStarterInner: Send + Sync {
    fn start_erased(&self, options: SpanOptions, callback: ChildFn) -> ChildFuture;
}

impl SpanStarter {
    /// Starts a root-level span (no parent), typed.
    pub async fn start_span<T, F, Fut>(&self, options: SpanOptions, callback: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(SpanHandle, SpanStarter) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let boxed = self
            .inner
            .start_erased(
                options,
                ChildFn(Box::new(move |child, starter| {
                    Box::pin(async move {
                        let value: T = callback(child, starter).await;
                        Box::new(value) as Box<dyn std::any::Any + Send>
                    })
                })),
            )
            .await;
        match boxed.downcast::<T>() {
            Ok(value) => *value,
            Err(_) => panic!("span callback returned the wrong type"),
        }
    }
}

impl std::fmt::Debug for SpanStarter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanStarter").finish()
    }
}

/// Erased callback used internally by [`SpanHandle::start_child`] and
/// [`SpanStarter::start_span`].
pub struct ChildFn(pub Box<dyn FnOnce(SpanHandle, SpanStarter) -> ChildFuture + Send>);

pub type ChildFuture =
    std::pin::Pin<Box<dyn Future<Output = Box<dyn std::any::Any + Send>> + Send>>;

/// Context surface for starting spans; mirrors upstream `TelemetryContext`.
pub trait TelemetryContext: Send + Sync {
    /// Returns a starter for root-level spans under this context.
    fn starter(&self) -> SpanStarter;

    /// Typed entry point: run `callback` inside a span on this context.
    fn start_span<T, F, Fut>(
        &self,
        options: SpanOptions,
        callback: F,
    ) -> impl Future<Output = T> + Send
    where
        T: Send + 'static,
        F: FnOnce(SpanHandle, SpanStarter) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        async move { self.starter().start_span(options, callback).await }
    }
}

/// Build a `SpanAttributes` map from `(name, value)` pairs.
#[macro_export]
macro_rules! attributes {
    ($($name:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map: ::std::collections::BTreeMap<::std::string::String, $crate::AttributeValue> =
            ::std::collections::BTreeMap::new();
        $(
            map.insert(::std::string::String::from($name), ::std::convert::Into::into($value));
        )*
        map
    }};
}
