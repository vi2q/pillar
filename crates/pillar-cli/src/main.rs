//! Entry point of the `pillar` binary (upstream `cli.ts` + the `main.ts`
//! bootstrap).
//!
//! divergence: this crate owns the runtime bootstrap that upstream keeps in
//! `coding-agent/main.ts`, because it is the only layer that can join the
//! coding agent with the Luau extension runtime (docs/rules/01-architecture.md).
//! The package/auth/update subcommands, migrations, trust prompts, and the
//! interactive/rpc modes are not ported yet.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use pillar_coding_agent::cli::args::{Args, DiagnosticKind, Mode, VERSION, parse_args};
use pillar_coding_agent::cli::help::render_help;
use pillar_coding_agent::cli::main::{AppMode, resolve_app_mode};
use pillar_coding_agent::core::agent_session_class::{ExtensionBindings, SessionEventMeta};
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pillar_coding_agent::core::sdk::{CreateAgentSessionOptions, create_agent_session};
use pillar_coding_agent::modes::print_mode::{PrintModeMode, PrintModeOptions, run_print_mode};

use pillar_cli::runner::build_extension_runner;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = parse_args(&args);

    let mut has_error = false;
    for diagnostic in &parsed.diagnostics {
        match diagnostic.kind {
            DiagnosticKind::Error => {
                eprintln!("Error: {}", diagnostic.message);
                has_error = true;
            }
            DiagnosticKind::Warning => eprintln!("Warning: {}", diagnostic.message),
        }
    }
    if has_error {
        return ExitCode::from(1);
    }

    if parsed.version == Some(true) {
        println!("{VERSION}");
        return ExitCode::SUCCESS;
    }

    if parsed.help == Some(true) {
        print!("{}", render_help(&[]));
        return ExitCode::SUCCESS;
    }

    if parsed.mode == Some(Mode::Rpc) && !parsed.file_args.is_empty() {
        eprintln!("Error: @file arguments are not supported in RPC mode");
        return ExitCode::from(1);
    }

    let app_mode = resolve_app_mode(
        &parsed,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    match app_mode {
        AppMode::Print | AppMode::Json => run_print(&parsed, app_mode).await,
        AppMode::Rpc => {
            eprintln!("Error: `rpc` mode is not ported yet");
            ExitCode::from(1)
        }
        AppMode::Interactive => {
            eprintln!("Error: `interactive` mode is not ported yet");
            ExitCode::from(1)
        }
    }
}

/// The agent config directory (`~/.pi/agent`, `PI_CODING_AGENT_DIR` override).
fn agent_dir() -> String {
    std::env::var("PI_CODING_AGENT_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(|home| format!("{home}/.pi/agent"))
                .unwrap_or_else(|_| ".pi/agent".to_string())
        })
}

/// Upstream `noTools` from `--no-tools` / `--no-builtin-tools`.
fn no_tools(parsed: &Args) -> Option<String> {
    if parsed.no_tools == Some(true) {
        Some("all".to_string())
    } else if parsed.no_builtin_tools == Some(true) {
        Some("builtin".to_string())
    } else {
        None
    }
}

/// Build the initial message from `@file` arguments and the first message
/// (upstream `prepareInitialMessage`, simplified: file contents are inlined
/// verbatim without the per-file annotation block).
fn prepare_initial_message(parsed: &Args, cwd: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for file in &parsed.file_args {
        let path = Path::new(file);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(cwd).join(path)
        };
        match std::fs::read_to_string(&resolved) {
            Ok(content) => parts.push(format!("@{file}\n{content}")),
            Err(error) => parts.push(format!("@{file}\n[failed to read: {error}]")),
        }
    }
    if !parsed.messages.is_empty() {
        parts.push(parsed.messages[0].clone());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

async fn run_print(parsed: &Args, app_mode: AppMode) -> ExitCode {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let agent_dir = agent_dir();

    let model_runtime = match ModelRuntime::new(CreateModelRuntimeOptions {
        auth_path: Some(PathBuf::from(&agent_dir).join("auth.json")),
        models_path: Some(PathBuf::from(&agent_dir).join("models.json")),
        ..Default::default()
    }) {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => {
            eprintln!("Error: failed to create model runtime: {error}");
            return ExitCode::from(1);
        }
    };

    let global_extensions = PathBuf::from(&agent_dir).join("extensions");
    let project_extensions = PathBuf::from(&cwd).join(".pi").join("extensions");
    let configured = parsed.extensions.clone().unwrap_or_default();
    let wiring = build_extension_runner(
        &cwd,
        Some(&global_extensions),
        Some(&project_extensions),
        &configured,
    );
    for (path, error) in &wiring.errors {
        eprintln!("Warning: failed to load extension {path}: {error}");
    }
    let extension_runner: Arc<Mutex<ExtensionRunner>> = Arc::new(Mutex::new(wiring.runner));
    // Keep the Luau runtime alive for the runner's bridges.
    let _runtime = wiring.runtime;

    let created = match create_agent_session(CreateAgentSessionOptions {
        cwd: cwd.clone(),
        agent_dir: Some(agent_dir),
        model_runtime,
        settings_manager: None,
        session_manager: None,
        resource_loader: None,
        model: None,
        thinking_level: parsed.thinking.clone(),
        scoped_models: Vec::new(),
        tools: parsed.tools.clone(),
        no_tools: no_tools(parsed),
        exclude_tools: parsed.exclude_tools.clone().unwrap_or_default(),
        custom_tools: Vec::new(),
        extension_runner,
        session_start_event: Some(SessionEventMeta {
            reason: "startup".to_string(),
            previous_session_file: None,
        }),
        system_prompt_rebuild: None,
        extension_runner_rebuild: None,
        stream_fn: None,
    })
    .await
    {
        Ok(created) => created,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    if let Some(message) = &created.model_fallback_message {
        eprintln!("Warning: {message}");
    }

    let session = created.session;
    let mode = if app_mode == AppMode::Json {
        PrintModeMode::Json
    } else {
        PrintModeMode::Text
    };
    session
        .bind_extensions(ExtensionBindings {
            ui_context: Some(false),
            mode: Some(mode_label(mode).to_string()),
            on_error: None,
        })
        .await;

    let options = PrintModeOptions {
        mode,
        messages: parsed.messages.iter().skip(1).cloned().collect(),
        initial_message: prepare_initial_message(parsed, &cwd),
        initial_images: None,
    };
    let mut stdout = std::io::stdout();
    match run_print_mode(&session, options, &mut stdout).await {
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::from(1)
        }
    }
}

fn mode_label(mode: PrintModeMode) -> &'static str {
    match mode {
        PrintModeMode::Text => "print",
        PrintModeMode::Json => "json",
    }
}
