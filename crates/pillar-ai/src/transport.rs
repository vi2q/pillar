//! Port of packages/ai/src/types.ts fetch plumbing (pi v0.84.3) — the
//! injectable HTTP transport boundary for provider requests.
//!
//! divergence: upstream threads a `fetch` function (and WHATWG `Response`
//! objects) through `StreamOptions.fetch`; the Rust port uses a `FetchFn`
//! trait returning a streaming `FetchResponse`. Providers receive an
//! `Arc<dyn FetchFn>` and use the reqwest-backed default when the caller
//! supplies none. The `Transport` selection type ("sse" | "websocket" |
//! "websocket-cached" | "auto") lives in `types::Transport`.

use std::pin::Pin;

use async_trait::async_trait;
use futures::StreamExt;

use crate::error::AiError;

/// Streamed response body chunks.
pub type ByteStream = Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>, AiError>> + Send>>;

/// An outgoing provider HTTP request (upstream: `fetch(url, RequestInit)`).
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub method: String,
    pub url: String,
    /// Header name/value pairs, sent in order.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl FetchRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: Some(body),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// A provider HTTP response. The body streams; callers that only need the
/// full payload use [`FetchResponse::text`] / [`FetchResponse::json`].
pub struct FetchResponse {
    pub status: u16,
    /// Header name/value pairs as received (names lowercased by the default
    /// transport, matching upstream `Headers` iteration).
    pub headers: Vec<(String, String)>,
    pub body: ByteStream,
}

impl FetchResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Drain the body into a single string (upstream `response.text()`).
    pub async fn text(self) -> Result<String, AiError> {
        let mut bytes = Vec::new();
        let mut stream = self.body;
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        String::from_utf8(bytes)
            .map_err(|error| AiError::Other(format!("fetch: response body is not UTF-8: {error}")))
    }

    /// Drain the body and parse it as JSON (upstream `response.json()`).
    pub async fn json(self) -> Result<serde_json::Value, AiError> {
        let text = self.text().await?;
        serde_json::from_str(&text)
            .map_err(|error| AiError::Other(format!("fetch: invalid JSON response body: {error}")))
    }
}

/// Injectable transport for provider HTTP requests (upstream `FetchFunction`).
/// Object-safe so callers can inject fakes; providers fall back to
/// [`ReqwestFetch`] when no implementation is supplied.
#[async_trait]
pub trait FetchFn: Send + Sync {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, AiError>;
}

/// Shared handle passed through provider options.
pub type SharedFetchFn = std::sync::Arc<dyn FetchFn>;

/// Default [`FetchFn`] implementation backed by reqwest (rustls, streaming).
pub struct ReqwestFetch {
    client: reqwest::Client,
}

impl ReqwestFetch {
    pub fn new() -> Result<Self, AiError> {
        let client = reqwest::Client::builder().build().map_err(|error| {
            AiError::Other(format!("fetch: failed to build HTTP client: {error}"))
        })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl FetchFn for ReqwestFetch {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, AiError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|error| AiError::Other(format!("fetch: invalid HTTP method: {error}")))?;
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| AiError::Other(format!("fetch: request failed: {error}")))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                Ok((
                    name.as_str().to_string(),
                    value
                        .to_str()
                        .map_err(|error| {
                            AiError::Other(format!("fetch: response header is not UTF-8: {error}"))
                        })?
                        .to_string(),
                ))
            })
            .collect::<Result<Vec<_>, AiError>>()?;
        let body = response
            .bytes_stream()
            .map(|chunk| {
                chunk.map(|bytes| bytes.to_vec()).map_err(|error| {
                    AiError::Other(format!("fetch: response stream failed: {error}"))
                })
            })
            .boxed();
        Ok(FetchResponse {
            status,
            headers,
            body,
        })
    }
}

/// Upstream `headersToRecord`: response headers as a plain name/value map
/// (later duplicates overwrite earlier ones, matching `Headers.entries()`).
pub fn headers_to_record(
    headers: &[(String, String)],
) -> std::collections::BTreeMap<String, String> {
    headers.iter().cloned().collect()
}
