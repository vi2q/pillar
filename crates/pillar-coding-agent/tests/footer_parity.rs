//! Parity tests for the footer port (pi v0.84.3 components/footer.ts).

use std::sync::{Arc, Mutex};

use pillar_agent::{Agent, AgentOptions, AgentState, AgentThinkingLevel, FauxModelRef};
use pillar_ai::types::{Content, StopReason, Usage, UsageCost};
use pillar_coding_agent::core::agent_session_class::{AgentSession, AgentSessionConfig};
use pillar_coding_agent::core::footer_data_provider::FooterDataProvider;
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::core::resource_loader::{GitPaths, ResourceLoader, ResourceLoaderOptions};
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use pillar_coding_agent::modes::interactive::components::footer::{
    FooterComponent, format_cwd_for_footer, format_tokens,
};
use pillar_coding_agent::modes::interactive::theme;
use pillar_tui::tui::Component as _;

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn strip_ansi(text: &str) -> String {
    pillar_tui::text_utils::strip_terminal_sequences(text)
}

// --- formatTokens -------------------------------------------------------------------------

#[test]
fn format_tokens_matches_upstream() {
    assert_eq!(format_tokens(0), "0");
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(1000), "1.0k");
    assert_eq!(format_tokens(9500), "9.5k");
    assert_eq!(format_tokens(9999), "10.0k");
    assert_eq!(format_tokens(150_000), "150k");
    assert_eq!(format_tokens(999_999), "1000k");
    assert_eq!(format_tokens(1_500_000), "1.5M");
    assert_eq!(format_tokens(9_999_999), "10.0M");
    assert_eq!(format_tokens(12_345_678), "12M");
    assert_eq!(format_tokens(123_456_789), "123M");
}

// --- formatCwdForFooter -------------------------------------------------------------------

#[test]
fn format_cwd_for_footer_matches_upstream() {
    // No home: the cwd passes through.
    assert_eq!(format_cwd_for_footer("/home/u/p", None), "/home/u/p");

    // Inside home: ~ prefix (Node `path.relative` shape, POSIX separator).
    assert_eq!(format_cwd_for_footer("/home/u/p", Some("/home/u")), "~/p");
    // Exactly home: `~`.
    assert_eq!(format_cwd_for_footer("/home/u", Some("/home/u")), "~");
    assert_eq!(
        format_cwd_for_footer("/home/u/sub/deep", Some("/home/u")),
        "~/sub/deep"
    );

    // Outside home: unchanged.
    assert_eq!(format_cwd_for_footer("/etc", Some("/home/u")), "/etc");
    // Sibling of home (upstream `..` / `../...` is not inside).
    assert_eq!(
        format_cwd_for_footer("/home/u2", Some("/home/u")),
        "/home/u2"
    );
    // Parent of home.
    assert_eq!(format_cwd_for_footer("/home", Some("/home/u")), "/home");
    assert_eq!(format_cwd_for_footer("/home/v", Some("/home/u")), "/home/v");
}

// --- footer rendering ---------------------------------------------------------------------

fn footer_session(provider: &str, model_id: &str, reasoning: bool) -> Arc<AgentSession> {
    let model = FauxModelRef {
        id: model_id.to_string(),
        name: model_id.to_string(),
        api: "anthropic-messages".to_string(),
        provider: provider.to_string(),
        base_url: String::new(),
        reasoning,
        input: vec!["text".to_string()],
        cost: UsageCost::default(),
        context_window: 200_000,
        max_tokens: 8_000,
    };

    let stream_fn = pillar_agent::StreamFn::new(|_context, _options| async {
        unreachable!("footer tests never prompt the agent")
    });
    let mut options = AgentOptions::new(stream_fn);
    options.initial_state = Some(AgentState {
        system_prompt: "Test".to_string(),
        model,
        thinking_level: AgentThinkingLevel::Medium,
        tools: Vec::new(),
        messages: Vec::new(),
        is_streaming: false,
        streaming_message: None,
        pending_tool_calls: Default::default(),
        error_message: None,
    });
    let agent = Arc::new(Agent::new(options));
    let session_manager = Arc::new(std::sync::Mutex::new(
        SessionManager::in_memory("/tmp/pillar-footer-cwd", None).expect("in-memory session"),
    ));
    let settings_manager = Arc::new(std::sync::Mutex::new(SettingsManager::in_memory(
        serde_json::json!({}),
        SettingsManagerCreateOptions {
            project_trusted: Some(true),
        },
    )));
    let resource_loader = Arc::new(std::sync::Mutex::new(ResourceLoader::new(
        "",
        ResourceLoaderOptions {
            agent_dir: "/tmp/pillar-footer-agent-dir".to_string(),
            no_skills: true,
            no_prompt_templates: true,
            no_themes: true,
            no_context_files: true,
            ..Default::default()
        },
        Arc::clone(&settings_manager),
    )));
    let dir = std::env::temp_dir().join(format!("pillar-footer-runtime-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let models_path = dir.join("models.json");
    std::fs::write(&models_path, "{}").expect("write models");
    let runtime = pillar_coding_agent::core::model_runtime::ModelRuntime::new(
        pillar_coding_agent::core::model_runtime::CreateModelRuntimeOptions {
            models_path: Some(models_path),
            models_store: Some(Arc::new(
                pillar_coding_agent::core::auth_storage::InMemoryCodingAgentModelsStore::new(),
            )),
            ..Default::default()
        },
    )
    .expect("runtime");

    Arc::new(AgentSession::new(AgentSessionConfig::new(
        agent,
        session_manager,
        settings_manager,
        String::new(),
        resource_loader,
        Arc::new(runtime),
        Arc::new(std::sync::Mutex::new(
            pillar_coding_agent::core::extensions_runner::ExtensionRunner::new(Vec::new()),
        )),
    )))
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost {
            total: cost,
            ..UsageCost::default()
        },
    }
}

fn assistant_entry(usage: Usage) -> CodingAgentMessage {
    CodingAgentMessage::Base(pillar_ai::types::Message::Assistant(Box::new(
        pillar_ai::types::AssistantMessage {
            content: vec![Content::text("hi")],
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            response_model: None,
            usage,
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            response_id: None,
            diagnostics: Vec::new(),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        },
    )))
}

fn make_footer(session: &Arc<AgentSession>) -> FooterComponent {
    let mut footer = FooterComponent::new(Arc::clone(session), FooterDataProvider::new(), None);
    // Upstream reads the env at render time; the port at construction.
    footer.set_home_dir(None);
    footer
}

#[test]
fn footer_renders_pwd_and_stats_with_right_aligned_model() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark_helper();
    let session = footer_session("anthropic", "claude-sonnet-4-5", true);
    session
        .session_manager()
        .lock()
        .expect("lock")
        .append_message(assistant_entry(usage(1000, 2000, 500, 0, 0.0123)))
        .expect("append");

    let mut footer = make_footer(&session);
    let lines = footer.render(80);
    assert_eq!(lines.len(), 2, "{lines:?}");

    let pwd = strip_ansi(&lines[0]);
    assert_eq!(pwd, "/tmp/pillar-footer-cwd", "{pwd:?}");

    let stats = strip_ansi(&lines[1]);
    assert!(stats.contains("\u{2191}1.0k"), "{stats:?}");
    assert!(stats.contains("\u{2193}2.0k"), "{stats:?}");
    assert!(stats.contains("R500"), "{stats:?}");
    assert!(stats.contains("CH33.3%"), "{stats:?}");
    assert!(stats.contains("$0.012"), "{stats:?}");
    // An assistant responded with usage, so the context percentage is
    // known: 3500/200000 = 1.8%, plus the auto indicator.
    assert!(stats.contains("1.8%/200k (auto)"), "{stats:?}");
    // The model is right-aligned with the thinking indicator.
    assert!(
        stats.ends_with("claude-sonnet-4-5 \u{2022} medium"),
        "{stats:?}"
    );
}

fn install_dark_helper() {
    theme::init_theme(Some("dark"));
}

#[test]
fn footer_truncates_the_right_side_when_space_is_tight() {
    let session = footer_session("anthropic", "claude-sonnet-4-5", true);
    session
        .session_manager()
        .lock()
        .expect("lock")
        .append_message(assistant_entry(usage(1000, 2000, 500, 0, 0.0123)))
        .expect("append");

    let mut footer = make_footer(&session);
    let lines = footer.render(50);
    let stats = strip_ansi(&lines[1]);
    // The stats alone exceed the budget for stats + padding + model, so
    // the right side disappears entirely (upstream `availableForRight`).
    assert_eq!(
        stats, "\u{2191}1.0k \u{2193}2.0k R500 CH33.3% $0.012 1.8%/200k (auto)",
        "{stats:?}"
    );
}

#[test]
fn footer_prepends_the_provider_when_several_are_available() {
    let session = footer_session("anthropic", "claude-sonnet-4-5", true);
    let mut footer = make_footer(&session);
    footer.footer_data().set_available_provider_count(2);
    let lines = footer.render(80);
    let stats = strip_ansi(&lines[1]);
    assert!(stats.contains("(anthropic) claude-sonnet-4-5"), "{stats:?}");
}

#[test]
fn footer_appends_the_git_branch_and_session_name() {
    let session = footer_session("anthropic", "claude-sonnet-4-5", false);
    // A temp "repo" whose HEAD file names the branch (upstream resolves
    // through git paths).
    let temp = std::env::temp_dir().join("pillar-footer-repo");
    let _ = std::fs::create_dir_all(&temp);
    let head = temp.join("HEAD");
    std::fs::write(&head, "ref: refs/heads/main").expect("write HEAD");
    let git_paths = GitPaths {
        repo_dir: temp.clone(),
        common_git_dir: temp.clone(),
        head_path: head,
    };
    let mut footer = FooterComponent::new(
        Arc::clone(&session),
        FooterDataProvider::new(),
        Some(git_paths),
    );
    footer.set_home_dir(None);

    let lines = footer.render(80);
    let pwd = strip_ansi(&lines[0]);
    assert_eq!(pwd, "/tmp/pillar-footer-cwd (main)", "{pwd:?}");

    // Session names are appended after a bullet.
    session
        .session_manager()
        .lock()
        .expect("lock")
        .append_session_info("my session")
        .expect("append");
    let lines = footer.render(80);
    let pwd = strip_ansi(&lines[0]);
    assert_eq!(
        pwd, "/tmp/pillar-footer-cwd (main) \u{2022} my session",
        "{pwd:?}"
    );
}

#[test]
fn footer_renders_extension_statuses_on_one_sorted_line() {
    let session = footer_session("anthropic", "claude-sonnet-4-5", true);
    let mut footer = make_footer(&session);
    footer
        .footer_data()
        .set_extension_status("b", Some("status b"));
    footer
        .footer_data()
        .set_extension_status("a", Some("status\na  x"));
    let lines = footer.render(80);
    assert_eq!(lines.len(), 3, "{lines:?}");
    // Sorted by key (the provider's BTreeMap is already sorted) and
    // sanitized (control characters become spaces, spaces collapse).
    let status = strip_ansi(&lines[2]);
    assert_eq!(status, "status a x status b", "{status:?}");
}
