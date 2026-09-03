//! Parity tests for model-registry.ts (pi v0.84.3): the sync facade over
//! a host-provided runtime — auth resolution shaping, compat fallback,
//! error mapping, and provider display names.

use std::collections::BTreeMap;

use pillar_ai::types::Model;
use pillar_coding_agent::core::model_registry::{
    AuthStatus, ModelRegistry, ModelRuntimeFacade, ResolvedRequestAuth, ResolvedRuntimeAuth,
};

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-completions".to_string(),
        provider: provider.to_string(),
        base_url: "https://example.com".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: Default::default(),
        context_window: 100_000,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// A configurable fake runtime.
struct FakeRuntime {
    auth: Result<Option<ResolvedRuntimeAuth>, String>,
    compat_auth_header: bool,
    compat_headers: Option<BTreeMap<String, String>>,
    display_names: BTreeMap<String, String>,
}

impl ModelRuntimeFacade for FakeRuntime {
    fn find(&self, _provider: &str, _model_id: &str) -> Option<Model> {
        None
    }
    fn all(&self) -> Vec<Model> {
        Vec::new()
    }
    fn available(&self) -> Vec<Model> {
        Vec::new()
    }
    fn has_configured_auth(&self, _provider: &str) -> bool {
        false
    }
    fn auth_status(&self, _provider: &str) -> AuthStatus {
        AuthStatus::default()
    }
    fn get_auth(&self, _model: &Model) -> Result<Option<ResolvedRuntimeAuth>, String> {
        self.auth.clone()
    }
    fn compatibility_request_config(
        &self,
        _model: &Model,
    ) -> (bool, Option<BTreeMap<String, String>>) {
        (self.compat_auth_header, self.compat_headers.clone())
    }
    fn provider_display_name(&self, provider: &str) -> Option<String> {
        self.display_names.get(provider).cloned()
    }
    fn is_using_oauth(&self, _provider: &str) -> bool {
        false
    }
}

#[test]
fn resolved_auth_passes_through() {
    let runtime = FakeRuntime {
        auth: Ok(Some(ResolvedRuntimeAuth {
            api_key: Some("sk-test".to_string()),
            headers: None,
            base_url: Some("https://proxy.example.com".to_string()),
            env: None,
        })),
        compat_auth_header: false,
        compat_headers: None,
        display_names: BTreeMap::new(),
    };
    let registry = ModelRegistry::new(&runtime);
    let resolved = registry.get_api_key_and_headers(&model("openai", "gpt-test"));
    match resolved {
        ResolvedRequestAuth::Ok {
            api_key, base_url, ..
        } => {
            assert_eq!(api_key.as_deref(), Some("sk-test"));
            assert_eq!(base_url.as_deref(), Some("https://proxy.example.com"));
        }
        other => panic!("expected ok, got {other:?}"),
    }
}

#[test]
fn no_auth_with_compat_headers_succeeds() {
    let mut headers = BTreeMap::new();
    headers.insert("x-custom".to_string(), "value".to_string());
    let runtime = FakeRuntime {
        auth: Ok(None),
        compat_auth_header: false,
        compat_headers: Some(headers.clone()),
        display_names: BTreeMap::new(),
    };
    let registry = ModelRegistry::new(&runtime);
    match registry.get_api_key_and_headers(&model("openai", "gpt-test")) {
        ResolvedRequestAuth::Ok {
            api_key,
            headers: resolved_headers,
            ..
        } => {
            assert_eq!(api_key, None);
            assert_eq!(resolved_headers, Some(headers));
        }
        other => panic!("expected ok, got {other:?}"),
    }
}

#[test]
fn no_auth_with_auth_header_requirement_fails() {
    let runtime = FakeRuntime {
        auth: Ok(None),
        compat_auth_header: true,
        compat_headers: None,
        display_names: BTreeMap::new(),
    };
    let registry = ModelRegistry::new(&runtime);
    let resolved = registry.get_api_key_and_headers(&model("openai", "gpt-test"));
    assert_eq!(resolved.error(), Some("No API key found for \"openai\""));
}

#[test]
fn auth_header_cause_error_maps_to_no_api_key() {
    let runtime = FakeRuntime {
        auth: Err("authHeader requires a resolved API key".to_string()),
        compat_auth_header: false,
        compat_headers: None,
        display_names: BTreeMap::new(),
    };
    let registry = ModelRegistry::new(&runtime);
    let resolved = registry.get_api_key_and_headers(&model("anthropic", "claude-test"));
    assert_eq!(resolved.error(), Some("No API key found for \"anthropic\""));
}

#[test]
fn other_auth_errors_pass_through() {
    let runtime = FakeRuntime {
        auth: Err("network down".to_string()),
        compat_auth_header: false,
        compat_headers: None,
        display_names: BTreeMap::new(),
    };
    let registry = ModelRegistry::new(&runtime);
    let resolved = registry.get_api_key_and_headers(&model("openai", "gpt-test"));
    assert_eq!(resolved.error(), Some("network down"));
}

#[test]
fn provider_display_name_falls_back_to_id() {
    let mut display_names = BTreeMap::new();
    display_names.insert("openai".to_string(), "OpenAI".to_string());
    let runtime = FakeRuntime {
        auth: Ok(None),
        compat_auth_header: false,
        compat_headers: None,
        display_names,
    };
    let registry = ModelRegistry::new(&runtime);
    assert_eq!(registry.provider_display_name("openai"), "OpenAI");
    assert_eq!(
        registry.provider_display_name("unknown-provider"),
        "unknown-provider"
    );
}

#[test]
fn auth_status_shape() {
    let status = AuthStatus {
        configured: true,
        source: Some("stored".to_string()),
        label: Some("API key".to_string()),
    };
    assert!(status.configured);
    assert_eq!(status.source.as_deref(), Some("stored"));
    assert!(!AuthStatus::default().configured);
}
