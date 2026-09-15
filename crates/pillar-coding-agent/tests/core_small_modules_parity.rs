//! Parity tests for the small core modules: provider-attribution, timings,
//! source-info, auth-guidance, diagnostics/slash-commands, event-bus, and
//! session-cwd (pi v0.84.3).

use pillar_ai::types::{Model, ModelCost, ModelCostRates, ProviderHeaders};
use pillar_coding_agent::core::auth_guidance;
use pillar_coding_agent::core::diagnostics::{BUILTIN_SLASH_COMMANDS, BuiltinSlashCommand};
use pillar_coding_agent::core::event_bus::{
    SessionCwdIssue, SessionCwdSource, check_session_cwd_exists, create_event_bus,
    format_missing_session_cwd_error, format_missing_session_cwd_prompt,
    get_missing_session_cwd_issue,
};
use pillar_coding_agent::core::provider_attribution::merge_provider_attribution_headers;
use pillar_coding_agent::core::source_info::{
    PathMetadata, SourceOrigin, SourceScope, create_source_info, create_synthetic_source_info,
};

fn model(provider: &str, id: &str, base_url: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "test-api".to_string(),
        provider: provider.to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates::default(),
            tiers: None,
        },
        context_window: 100_000,
        max_tokens: 8_000,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn headers(items: &[(&str, &str)]) -> ProviderHeaders {
    items
        .iter()
        .map(|(k, v)| (k.to_string(), Some(v.to_string())))
        .collect()
}

// --- provider-attribution ------------------------------------------------------

#[test]
fn openrouter_attribution_headers_when_telemetry_enabled() {
    let m = model("openrouter", "kimi", "https://openrouter.ai/api/v1");
    let merged = merge_provider_attribution_headers(&m, true, None, &[]).expect("headers");
    assert!(
        !merged.contains_key("HTTP-Referer"),
        "pillar sends no referer by default: {merged:?}"
    );
    assert_eq!(
        merged.get("X-OpenRouter-Title").unwrap(),
        &Some("pillar".to_string())
    );
    assert_eq!(
        merged.get("X-OpenRouter-Categories").unwrap(),
        &Some("cli-agent".to_string())
    );
}

#[test]
fn openrouter_detection_by_base_url_host() {
    let m = model("other", "m", "https://openrouter.ai/api/v1");
    let merged = merge_provider_attribution_headers(&m, true, None, &[]).unwrap();
    assert!(merged.contains_key("X-OpenRouter-Title"));
}

#[test]
fn nvidia_nim_attribution_header() {
    let m = model("nvidia", "nemotron", "https://integrate.api.nvidia.com/v1");
    let merged = merge_provider_attribution_headers(&m, true, None, &[]).unwrap();
    assert_eq!(
        merged.get("X-BILLING-INVOKE-ORIGIN").unwrap(),
        &Some("Pillar".to_string())
    );
}

#[test]
fn cloudflare_attribution_user_agent() {
    let m = model(
        "cloudflare-workers-ai",
        "kimi",
        "https://api.cloudflare.com",
    );
    let merged = merge_provider_attribution_headers(&m, true, None, &[]).unwrap();
    assert_eq!(
        merged.get("User-Agent").unwrap(),
        &Some("pillar-coding-agent".to_string())
    );
    // Also via the AI gateway host.
    let m = model("x", "m", "https://gateway.ai.cloudflare.com/v1");
    let merged = merge_provider_attribution_headers(&m, true, None, &[]).unwrap();
    assert!(merged.contains_key("User-Agent"));
}

#[test]
fn no_attribution_for_other_providers() {
    let m = model("anthropic", "claude", "https://api.anthropic.com");
    assert!(merge_provider_attribution_headers(&m, true, None, &[]).is_none());
}

#[test]
fn telemetry_disabled_suppresses_attribution() {
    let m = model("openrouter", "kimi", "https://openrouter.ai/api/v1");
    assert!(merge_provider_attribution_headers(&m, false, None, &[]).is_none());
}

#[test]
fn opencode_session_headers() {
    let m = model("opencode", "kimi", "https://opencode.ai/api");
    let merged = merge_provider_attribution_headers(&m, true, Some("sess-1"), &[]).unwrap();
    assert_eq!(
        merged.get("x-opencode-session").unwrap(),
        &Some("sess-1".to_string())
    );
    assert_eq!(
        merged.get("x-opencode-client").unwrap(),
        &Some("pillar".to_string())
    );
}

#[test]
fn opencode_session_headers_only_for_opencode_providers() {
    let m = model("anthropic", "claude", "https://api.anthropic.com");
    assert!(
        merge_provider_attribution_headers(&m, true, Some("sess-1"), &[]).is_none(),
        "session headers suppressed for non-opencode providers without the host"
    );
    // But present when baseUrl matches opencode.ai.
    let m = model("custom", "m", "https://opencode.ai/api");
    let merged = merge_provider_attribution_headers(&m, true, Some("s"), &[]).unwrap();
    assert!(merged.contains_key("x-opencode-session"));
}

#[test]
fn explicit_header_sources_win_over_attribution() {
    let m = model("openrouter", "kimi", "https://openrouter.ai/api/v1");
    let explicit = headers(&[("HTTP-Referer", "https://override.test")]);
    let merged = merge_provider_attribution_headers(&m, true, None, &[&explicit]).unwrap();
    assert_eq!(
        merged.get("HTTP-Referer").unwrap(),
        &Some("https://override.test".to_string())
    );
}

// --- timings ---------------------------------------------------------------------

#[test]
fn timings_noop_when_disabled() {
    // Not asserting env state; just ensure the calls do not panic.
    pillar_coding_agent::core::timings::reset_timings(
        pillar_coding_agent::core::timings::TimingLabel::Main,
    );
    pillar_coding_agent::core::timings::time(
        "t",
        pillar_coding_agent::core::timings::TimingLabel::Main,
    );
    pillar_coding_agent::core::timings::print_timings();
}

// --- source-info ------------------------------------------------------------------

#[test]
fn source_info_from_metadata() {
    let metadata = PathMetadata {
        source: "npm:foo".to_string(),
        scope: SourceScope::Project,
        origin: SourceOrigin::Package,
        base_dir: Some("/proj/node_modules/foo".to_string()),
    };
    let info = create_source_info("/proj/node_modules/foo/ext.js", &metadata);
    assert_eq!(info.path, "/proj/node_modules/foo/ext.js");
    assert_eq!(info.source, "npm:foo");
    assert_eq!(info.scope, SourceScope::Project);
    assert_eq!(info.origin, SourceOrigin::Package);
    assert_eq!(info.base_dir.as_deref(), Some("/proj/node_modules/foo"));
}

#[test]
fn synthetic_source_info_defaults() {
    let info = create_synthetic_source_info(
        "/tmp/runtime.js",
        pillar_coding_agent::core::source_info::SyntheticSourceOptions {
            source: "runtime".to_string(),
            ..Default::default()
        },
    );
    assert_eq!(info.scope, SourceScope::Temporary);
    assert_eq!(info.origin, SourceOrigin::TopLevel);
    assert_eq!(info.source, "runtime");
}

#[test]
fn scope_and_origin_strings_match_upstream() {
    assert_eq!(SourceScope::User.as_str(), "user");
    assert_eq!(SourceScope::Project.as_str(), "project");
    assert_eq!(SourceScope::Temporary.as_str(), "temporary");
    assert_eq!(SourceOrigin::Package.as_str(), "package");
    assert_eq!(SourceOrigin::TopLevel.as_str(), "top-level");
}

// --- auth-guidance -----------------------------------------------------------------

#[test]
fn auth_guidance_messages() {
    let help = auth_guidance::get_provider_login_help();
    assert!(help.starts_with("Use /login to log into a provider via OAuth or API key. See:"));
    assert!(help.contains("docs/providers.md"));
    assert!(help.contains("docs/models.md"));

    assert_eq!(
        auth_guidance::format_no_models_available_message(),
        format!("No models available. {help}")
    );
    assert!(
        auth_guidance::format_no_model_selected_message().starts_with("No model selected.\n\n")
    );
    assert!(
        auth_guidance::format_no_model_selected_message()
            .ends_with("Then use /model to select a model.")
    );

    assert_eq!(
        auth_guidance::format_no_api_key_found_message("anthropic"),
        format!("No API key found for anthropic.\n\n{help}")
    );
    assert_eq!(
        auth_guidance::format_no_api_key_found_message("unknown"),
        format!("No API key found for the selected model.\n\n{help}")
    );
}

// --- slash-commands ------------------------------------------------------------------

#[test]
fn builtin_slash_commands_match_upstream_registry() {
    assert_eq!(BUILTIN_SLASH_COMMANDS.len(), 23);
    let names: Vec<&str> = BUILTIN_SLASH_COMMANDS.iter().map(|c| c.name).collect();
    for expected in [
        "settings",
        "model",
        "tree",
        "thinking",
        "scoped-models",
        "export",
        "import",
        "share",
        "copy",
        "name",
        "session",
        "changelog",
        "hotkeys",
        "fork",
        "clone",
        "trust",
        "login",
        "logout",
        "new",
        "compact",
        "resume",
        "reload",
        "quit",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    let model_cmd: &BuiltinSlashCommand = BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|c| c.name == "model")
        .unwrap();
    assert_eq!(model_cmd.argument_hint, Some("<provider/model>"));
    assert_eq!(model_cmd.description, "Select model (opens selector UI)");
    let quit: &BuiltinSlashCommand = BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|c| c.name == "quit")
        .unwrap();
    assert_eq!(quit.description, "Quit pi");
    assert!(quit.argument_hint.is_none());
}

// --- event-bus -----------------------------------------------------------------------

#[test]
fn event_bus_fans_out_and_clears() {
    let (bus, controller) = create_event_bus();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    controller.on("chat", move |_channel, data| {
        seen2.lock().unwrap().push(data.clone());
    });
    bus.emit("chat", &serde_json::json!({"x": 1}));
    bus.emit("other", &serde_json::json!({"x": 2}));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "only the subscribed channel fires"
    );
    controller.clear();
    bus.emit("chat", &serde_json::json!({"x": 3}));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "cleared handlers do not fire"
    );
}

#[test]
fn event_bus_multiple_handlers_and_late_subscribers() {
    let (bus, controller) = create_event_bus();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count2 = count.clone();
    controller.on("e", move |_c, _d| {
        count2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let count3 = count.clone();
    controller.on("e", move |_c, _d| {
        count3.fetch_add(10, std::sync::atomic::Ordering::SeqCst);
    });
    bus.emit("e", &serde_json::json!(null));
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 11);
}

// --- session-cwd -----------------------------------------------------------------------

struct FixedSession {
    cwd: String,
    file: Option<String>,
}

impl SessionCwdSource for FixedSession {
    fn get_cwd(&self) -> String {
        self.cwd.clone()
    }
    fn get_session_file(&self) -> Option<String> {
        self.file.clone()
    }
}

#[test]
fn missing_session_cwd_detected_only_when_path_gone() {
    // No session file -> no issue.
    let session = FixedSession {
        cwd: "/gone".to_string(),
        file: None,
    };
    assert!(get_missing_session_cwd_issue(&session, "/tmp").is_none());

    // Existing cwd -> no issue.
    let session = FixedSession {
        cwd: std::env::temp_dir().to_string_lossy().to_string(),
        file: Some("/tmp/s.jsonl".to_string()),
    };
    assert!(get_missing_session_cwd_issue(&session, "/tmp").is_none());

    // Missing cwd -> issue.
    let session = FixedSession {
        cwd: "/definitely/not/a/real/dir".to_string(),
        file: Some("/tmp/s.jsonl".to_string()),
    };
    let issue = get_missing_session_cwd_issue(&session, "/tmp").unwrap();
    assert_eq!(issue.session_cwd, "/definitely/not/a/real/dir");
    assert_eq!(issue.fallback_cwd, "/tmp");
    assert_eq!(issue.session_file.as_deref(), Some("/tmp/s.jsonl"));
}

#[test]
fn missing_session_cwd_formatting() {
    let issue = SessionCwdIssue {
        session_file: Some("/tmp/s.jsonl".to_string()),
        session_cwd: "/gone".to_string(),
        fallback_cwd: "/here".to_string(),
    };
    assert_eq!(
        format_missing_session_cwd_error(&issue),
        "Stored session working directory does not exist: /gone\nSession file: /tmp/s.jsonl\nCurrent working directory: /here"
    );
    assert_eq!(
        format_missing_session_cwd_prompt(&issue),
        "cwd from session file does not exist\n/gone\n\ncontinue in current cwd\n/here"
    );
}

#[test]
fn check_session_cwd_exists_returns_error() {
    let session = FixedSession {
        cwd: "/definitely/not/a/real/dir".to_string(),
        file: Some("s.jsonl".to_string()),
    };
    let error = check_session_cwd_exists(&session, "/tmp").unwrap_err();
    assert!(
        error
            .message()
            .starts_with("Stored session working directory does not exist:")
    );
    assert!(error.to_string().contains("/definitely/not/a/real/dir"));

    let session = FixedSession {
        cwd: std::env::temp_dir().to_string_lossy().to_string(),
        file: Some("s.jsonl".to_string()),
    };
    assert!(check_session_cwd_exists(&session, "/tmp").is_ok());
}

// --- provider attribution with a client identity --------------------------------

/// Every overridable protocol value follows `ClientIdentity`, so a pillar
/// build (or `PILLAR_CLIENT_NAME=pillar`) sends pillar branding while the
/// functional headers (`x-opencode-session`, categories) stay as-is.
#[test]
fn client_identity_overrides_the_pillar_protocol_values() {
    use pillar_coding_agent::core::provider_attribution::{
        ClientIdentity, merge_provider_attribution_headers_with,
    };

    let identity = ClientIdentity {
        client_name: "pillar".to_string(),
        referer_url: Some("https://pillar.test".to_string()),
    };

    let openrouter = model("openrouter", "kimi", "https://openrouter.ai/api/v1");
    let merged =
        merge_provider_attribution_headers_with(&openrouter, true, None, &identity, &[]).unwrap();
    assert_eq!(
        merged.get("X-OpenRouter-Title").unwrap(),
        &Some("pillar".to_string())
    );
    assert_eq!(
        merged.get("HTTP-Referer").unwrap(),
        &Some("https://pillar.test".to_string())
    );
    assert_eq!(
        merged.get("X-OpenRouter-Categories").unwrap(),
        &Some("cli-agent".to_string())
    );

    let nvidia = model("nvidia", "nemotron", "https://integrate.api.nvidia.com/v1");
    let merged =
        merge_provider_attribution_headers_with(&nvidia, true, None, &identity, &[]).unwrap();
    assert_eq!(
        merged.get("X-BILLING-INVOKE-ORIGIN").unwrap(),
        &Some("Pillar".to_string())
    );

    let cloudflare = model("cloudflare-workers-ai", "kimi", "https://api.cloudflare.com");
    let merged =
        merge_provider_attribution_headers_with(&cloudflare, true, None, &identity, &[]).unwrap();
    assert_eq!(
        merged.get("User-Agent").unwrap(),
        &Some("pillar-coding-agent".to_string())
    );

    let opencode = model("opencode-go", "omen-alpha", "https://opencode.ai/zen/go/v1");
    let merged =
        merge_provider_attribution_headers_with(&opencode, true, Some("sess-1"), &identity, &[])
            .unwrap();
    assert_eq!(
        merged.get("x-opencode-client").unwrap(),
        &Some("pillar".to_string())
    );
    assert_eq!(
        merged.get("x-opencode-session").unwrap(),
        &Some("sess-1".to_string()),
        "the session id is functional and never renamed"
    );
}

/// An empty referer URL drops the header instead of sending an empty value.
#[test]
fn empty_referer_url_omits_the_header() {
    use pillar_coding_agent::core::provider_attribution::{
        ClientIdentity, merge_provider_attribution_headers_with,
    };

    let identity = ClientIdentity {
        client_name: "pillar".to_string(),
        referer_url: None,
    };
    let m = model("openrouter", "kimi", "https://openrouter.ai/api/v1");
    let merged =
        merge_provider_attribution_headers_with(&m, true, None, &identity, &[]).unwrap();
    assert!(!merged.contains_key("HTTP-Referer"), "{merged:?}");
    assert!(merged.contains_key("X-OpenRouter-Title"));
}
