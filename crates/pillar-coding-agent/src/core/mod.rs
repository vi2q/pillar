//! Port of packages/coding-agent/src/core (pi v0.84.3).

pub mod agent_session;
pub mod agent_session_class;
pub mod agent_session_runtime;
pub mod auth_guidance;
pub mod auth_storage;
pub mod bash_executor;
pub mod cache_stats;
pub mod client_transcript;
pub mod compaction;
pub mod diagnostics;
pub mod effects;
pub mod event_bus;
pub mod exec;
/// Native host adapters for the Rust workflow tools (design
/// docs/RUST-TOOLING-DESIGN.md §2, §9). Native-only: the embedding profiles
/// have no process or filesystem capability.
#[cfg(not(target_arch = "wasm32"))]
pub mod rust_host;
/// The optional rust-analyzer semantic provider (design §6, stage R2).
/// Native-only: it drives a server process.
#[cfg(not(target_arch = "wasm32"))]
pub mod rust_analyzer;
pub mod export_html;
pub mod extensions_loader;
pub mod extensions_luau;
pub mod extensions_runner;
pub mod extensions_types;
pub mod extras;
pub mod footer_data_provider;
pub mod http_dispatcher;
pub mod keybindings;
pub mod messages;
pub mod model_config;
pub mod model_mutation;
pub mod model_registry;
pub mod model_resolver;
pub mod model_runtime;
pub mod package_manager;
pub mod prompt_templates;
pub mod provider_attribution;
pub mod provider_composer;
pub mod remote_catalog_provider;
pub mod resolve_config_value;
pub mod resource_loader;
pub mod runtime_credentials;
pub mod sdk;
pub mod session_entries;
pub mod session_manager;
pub mod session_support;
pub mod settings_manager;
pub mod skills;
pub mod slash_commands;
pub mod source_info;
pub mod system_prompt;
pub mod timings;
pub mod tools;
pub mod truncate;
pub mod trust_manager;
pub mod usage_totals;
