//! Entry point of the `pillar` binary (upstream `cli.ts` + the `main.ts`
//! bootstrap).
//!
//! divergence: this crate owns the runtime bootstrap that upstream keeps in
//! `coding-agent/main.ts`, because it is the only layer that can join the
//! coding agent with the Luau extension runtime (docs/rules/01-architecture.md).
//! The package subcommands (`install` / `remove` / `update` / `list`) live in
//! [`pillar_cli::commands`], the `auth` commands in [`pillar_cli::auth`] (parsed
//! before the normal option parser, as upstream does), and the startup
//! migrations in `pillar_coding_agent::migrations`. `pillar config` is not
//! ported yet.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use pillar_coding_agent::cli::args::{Args, DiagnosticKind, Mode, VERSION, parse_args};
use pillar_coding_agent::cli::help::render_help;
use pillar_coding_agent::cli::main::{AppMode, resolve_app_mode};
use pillar_coding_agent::core::agent_session_class::{
    AgentSession, ExtensionBindings, SessionEventMeta,
};
use pillar_coding_agent::core::agent_session_runtime::{
    AgentSessionRuntime, CreateAgentSessionServicesOptions, RuntimeFactoryInput,
    RuntimeFactoryResult, RuntimeHooks, create_agent_session_services,
};
use pillar_coding_agent::core::extensions_runner::ExtensionRunner;
use pillar_coding_agent::core::model_mutation::ScopedModel;
use pillar_coding_agent::core::model_resolver::{
    AuthProviders, Model, ResolveCliModelOptions, ResolverThinkingLevel, resolve_cli_model,
    resolve_model_scope_from_models,
};
use pillar_coding_agent::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use pillar_coding_agent::core::resource_loader::ResourceLoader;
use pillar_coding_agent::core::sdk::{CreateAgentSessionOptions, create_agent_session};
use pillar_coding_agent::core::session_manager::SessionManager;
use pillar_coding_agent::core::settings_manager::SettingsManager;
use pillar_coding_agent::modes::interactive::interactive_mode::{
    InteractiveModeOptions, TuiMode as InteractiveTuiMode,
};
use pillar_coding_agent::modes::interactive::run::{
    InteractiveOutcome, InteractiveRunOptions, run_interactive_process,
};
use pillar_coding_agent::modes::interactive::transcript::TranscriptSettings;
use pillar_coding_agent::modes::print_mode::{PrintModeMode, PrintModeOptions, run_print_mode};
use pillar_coding_agent::modes::rpc::rpc_mode::{
    RpcRuntimeHost, SessionReplacement, run_rpc_mode_with_host,
};

use pillar_cli::effects::EffectBroker;
use pillar_cli::runner::{
    ExtensionHostSlots, ExtensionWiring, build_extension_runner_with_slots,
    extension_command_handler, refresh_extension_data_for,
};
use pillar_cli::trust::{project_extension_dir, resolve_project_trust, stored_project_trust};
use pillar_coding_agent::core::agent_session_class::ExtensionGeneration;
use pillar_coding_agent::core::extensions_types::{ExtensionContextFacts, ExtensionMode};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `pillar auth …` parses its own options (upstream `runAuthCommand` runs
    // before the normal parser, because the subcommand selects the legal
    // options).
    if let Some(code) = pillar_cli::auth::run_auth_command(&args, &agent_dir()).await {
        return ExitCode::from(code as u8);
    }
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

    // `pillar install` / `remove` / `update` / `list`: these do not build a
    // session, so they run before the mode dispatch.
    if let Some(subcommand) = parsed.subcommand.clone() {
        let cwd = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let mut stdout = std::io::stdout();
        return match pillar_cli::commands::run_subcommand(
            &subcommand,
            &cwd,
            &agent_dir(),
            parsed.project_trust_override,
            &mut stdout,
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        };
    }

    let app_mode = resolve_app_mode(
        &parsed,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    // The one-time startup migrations (upstream `runMigrations`); the
    // subcommands above returned before them.
    let migrations = pillar_coding_agent::migrations::run_migrations(
        &std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy(),
        Path::new(&agent_dir()),
    );
    match app_mode {
        AppMode::Print | AppMode::Json => run_print(&parsed, app_mode).await,
        AppMode::Rpc => run_rpc(&parsed).await,
        AppMode::Interactive => run_interactive(&parsed, &migrations).await,
    }
}

/// The agent config directory (`~/.pillar/agent`, `PILLAR_CODING_AGENT_DIR` override).
fn agent_dir() -> String {
    std::env::var("PILLAR_CODING_AGENT_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(|home| format!("{home}/.pillar/agent"))
                .unwrap_or_else(|_| ".pillar/agent".to_string())
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

/// Inputs for building a session: the initial bootstrap supplies defaults,
/// a runtime replacement supplies the services and session manager produced
/// by the replacement flow.
struct SessionBuildInput {
    cwd: String,
    agent_dir: String,
    session_manager: Option<SessionManager>,
    settings_manager: Option<Arc<Mutex<SettingsManager>>>,
    resource_loader: Option<Arc<Mutex<ResourceLoader>>>,
    start_reason: String,
    previous_session_file: Option<String>,
    project_trust_override: Option<bool>,
    /// Whether an unresolved trust decision may prompt on the terminal (the
    /// interactive startup only: later rebuilds run under the TUI).
    prompt_for_trust: bool,
}

/// Create the model runtime over the agent directory config.
///
/// Upstream `ModelRuntime.refresh` reloads the catalog and re-derives which
/// providers have usable auth; the port's `refresh` needs `&mut self` and is
/// offline-only, so the bootstrap runs the availability pass explicitly —
/// without it every provider reads as unauthenticated and no model is ever
/// selected, even when `auth.json` / `models.json` are populated.
async fn create_model_runtime(agent_dir: &str) -> Result<Arc<ModelRuntime>, String> {
    let runtime = ModelRuntime::new(CreateModelRuntimeOptions {
        auth_path: Some(PathBuf::from(agent_dir).join("auth.json")),
        models_path: Some(PathBuf::from(agent_dir).join("models.json")),
        ..Default::default()
    })
    .map_err(|error| format!("failed to create model runtime: {error}"))?;
    runtime
        .refresh_availability(None)
        .await
        .map_err(|error| format!("failed to refresh model availability: {error}"))?;
    Ok(Arc::new(runtime))
}

/// The CLI-selected model, scope, and thinking level (upstream
/// `buildSessionOptions`).
struct CliModelSelection {
    model: Option<Model>,
    scoped_models: Vec<ScopedModel>,
    thinking_level: Option<String>,
}

/// Resolve `--provider` / `--model` (with the `<pattern>:<thinking>`
/// shorthand) and the `--models` scope against the model runtime.
fn resolve_cli_model_selection(
    parsed: &Args,
    model_runtime: &ModelRuntime,
) -> Result<CliModelSelection, String> {
    let all_models = model_runtime.get_models(None);
    let auth = AuthProviders(
        model_runtime
            .get_snapshot()
            .configured_providers
            .into_iter()
            .collect(),
    );

    let mut thinking_level: Option<String> = None;
    let mut model: Option<Model> = None;
    if parsed.model.is_some() {
        let resolved = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: parsed.provider.clone(),
            cli_model: parsed.model.clone(),
            cli_thinking: parsed.thinking.as_deref().map(ResolverThinkingLevel::parse),
            models: &all_models,
            auth: &auth,
        });
        if let Some(warning) = resolved.warning {
            eprintln!("Warning: {warning}");
        }
        if let Some(error) = resolved.error {
            return Err(error);
        }
        model = resolved.model;
        // A `--model <pattern>:<thinking>` shorthand only applies when
        // `--thinking` was not given explicitly.
        if parsed.thinking.is_none() {
            thinking_level = resolved
                .thinking_level
                .map(|level| level.as_str().to_string());
        }
    }

    let mut scoped_models: Vec<ScopedModel> = Vec::new();
    if let Some(patterns) = &parsed.models {
        let scope = resolve_model_scope_from_models(patterns, &all_models);
        for diagnostic in scope.diagnostics {
            eprintln!("Warning: {}", diagnostic.message);
        }
        scoped_models = scope
            .scoped_models
            .into_iter()
            .map(ScopedModel::from)
            .collect();
    }

    Ok(CliModelSelection {
        model,
        scoped_models,
        thinking_level,
    })
}

/// Create the runtime (model runtime, Luau extension runner, session). The
/// returned wiring must outlive the session (it owns the Luau runtime).
async fn build_session_with(
    parsed: &Args,
    model_runtime: Arc<ModelRuntime>,
    input: SessionBuildInput,
) -> Result<(AgentSession, ExtensionWiring), String> {
    let SessionBuildInput {
        cwd,
        agent_dir,
        session_manager,
        settings_manager,
        resource_loader,
        start_reason,
        previous_session_file,
        project_trust_override,
        prompt_for_trust,
    } = input;

    let global_extensions = PathBuf::from(&agent_dir).join("extensions");
    // An untrusted project's extensions are never evaluated: loading one runs
    // arbitrary Luau with `pi.exec` / `pillar.fs` available
    // (docs/ARCHITECTURE-REVIEW-s05c0.md A). The gate result travels into the
    // rebuild inputs too, so `/reload` cannot resurrect them.
    let project_trusted =
        resolve_project_trust(&cwd, &agent_dir, project_trust_override, prompt_for_trust);
    let project_extensions = project_extension_dir(&cwd, project_trusted);
    if project_extensions.is_none()
        && !project_trusted
        && Path::new(&cwd).join(".pillar").join("extensions").is_dir()
    {
        eprintln!(
            "Warning: skipping {cwd}/.pillar/extensions: project not trusted (use --approve, \
             or add the path to {agent_dir}/trust.json)"
        );
    }
    let configured = parsed.extensions.clone().unwrap_or_default();
    // One broker per session build: the extension host callbacks and the
    // session's tool path share its policy and audit trail.
    let broker = EffectBroker::permissive();
    let slots = ExtensionHostSlots::with_broker(&cwd, Arc::clone(&broker));
    let mut wiring = build_extension_runner_with_slots(
        &cwd,
        Some(&global_extensions),
        project_extensions.as_deref(),
        &configured,
        &slots,
    );
    for (path, error) in &wiring.errors {
        eprintln!("Warning: failed to load extension {path}: {error}");
    }
    let rebuild_inputs = wiring.rebuild.clone();
    let extension_runner: Arc<Mutex<ExtensionRunner>> = Arc::new(Mutex::new(wiring.take_runner()));

    let selection = resolve_cli_model_selection(parsed, &model_runtime)?;

    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: cwd.clone(),
        agent_dir: Some(agent_dir),
        model_runtime,
        settings_manager,
        session_manager,
        resource_loader,
        model: selection.model,
        thinking_level: parsed.thinking.clone().or(selection.thinking_level),
        scoped_models: selection.scoped_models,
        tools: parsed.tools.clone(),
        no_tools: no_tools(parsed),
        exclude_tools: parsed.exclude_tools.clone().unwrap_or_default(),
        custom_tools: wiring.custom_tools(),
        extension_runner,
        project_trusted: Some(project_trusted),
        effect_authorizer: Some(broker.authorizer()),
        session_start_event: Some(SessionEventMeta {
            reason: start_reason,
            previous_session_file,
        }),
        command_handler: Some(extension_command_handler(&wiring.runtime)),
        system_prompt_rebuild: None,
        // `/reload`: re-run discovery into a fresh VM, keeping the host slots
        // (the session binding, the `ctx.ui` bridge, the facts and the
        // command snapshot). A generation swap owns every capability derived
        // from the VM — the runner, the command handler and the extension
        // tools — otherwise the old generation keeps running
        // (docs/ARCHITECTURE-REVIEW-s05c0.md C).
        extension_runner_rebuild: {
            let inputs = rebuild_inputs;
            Some(Arc::new(move |flag_values| {
                let mut rebuilt = build_extension_runner_with_slots(
                    &inputs.cwd,
                    inputs.global_dir.as_deref(),
                    inputs.project_dir.as_deref(),
                    &inputs.configured,
                    &inputs.slots,
                );
                for (name, value) in &flag_values {
                    rebuilt.runner.set_flag_value(name, value.clone());
                }
                for (path, error) in &rebuilt.errors {
                    eprintln!("Warning: failed to load extension {path}: {error}");
                }
                // The session applies the generation (command handler, tools,
                // runner) only once the build reported no error.
                Ok(ExtensionGeneration {
                    command_handler: Some(extension_command_handler(&rebuilt.runtime)),
                    tools: rebuilt.custom_tools(),
                    runner: rebuilt.take_runner(),
                })
            }))
        },
        stream_fn: None,
    })
    .await?;
    // Publish the host snapshot after the session installed the rebuilt
    // runner: refreshing inside the factory would read the outgoing runner.
    {
        let session_slot = Arc::clone(&wiring.session_slot);
        let data = Arc::clone(&wiring.data);
        created
            .session
            .set_extension_reload_publish(Arc::new(move || {
                refresh_extension_data_for(&session_slot, &data);
            }));
    }
    if let Some(message) = &created.model_fallback_message {
        eprintln!("Warning: {message}");
    }
    Ok((created.session, wiring))
}

/// `prompt_for_trust` is true only for the interactive startup: a rebuild
/// runs under the TUI, which owns the terminal.
async fn build_session(
    parsed: &Args,
    prompt_for_trust: bool,
) -> Result<(AgentSession, ExtensionWiring), String> {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let agent_dir = agent_dir();
    let model_runtime = create_model_runtime(&agent_dir).await?;
    build_session_with(
        parsed,
        model_runtime,
        SessionBuildInput {
            cwd,
            agent_dir,
            session_manager: None,
            settings_manager: None,
            resource_loader: None,
            start_reason: "startup".to_string(),
            previous_session_file: None,
            project_trust_override: parsed.project_trust_override,
            prompt_for_trust,
        },
    )
    .await
}

/// Build the replacement session for `/resume` (upstream
/// `runtimeHost.switchSession` + `rebindCurrentSession`). A fresh
/// services bundle + model runtime is created for the target session (the
/// same divergence the RPC host documents: the runtime is rebuilt per
/// replacement, so the extension wirings of every replacement stay alive in
/// the caller).
///
/// divergence: upstream prompts for a missing session cwd; the port reports
/// the error and exits instead.
async fn resume_session(
    parsed: &Args,
    current: &Arc<AgentSession>,
    session_path: &str,
) -> Result<(AgentSession, ExtensionWiring), String> {
    let agent_dir = agent_dir();
    let model_runtime = create_model_runtime(&agent_dir).await?;
    // The runtime instance the replacement flows through (upstream one
    // `runtimeHost`); for an in-memory current session there is no previous
    // file (upstream `previousSessionFile` is then undefined too).
    let current_file = current
        .session_manager()
        .lock()
        .expect("session lock")
        .session_file()
        .map(Path::to_path_buf);
    let current_manager = match current_file {
        Some(file) => SessionManager::open(&file, None, None)?,
        None => SessionManager::in_memory(current.cwd(), None)?,
    };
    let mut factory = replacement_factory(parsed, &agent_dir);
    let runtime =
        AgentSessionRuntime::create(&mut factory, current.cwd(), &agent_dir, current_manager)?;
    let previous = runtime
        .session_manager()
        .session_file()
        .map(|path| path.to_string_lossy().to_string());
    let (outcome, runtime) = {
        let mut hooks = RuntimeHooks::default();
        runtime.switch_session(session_path, None, &mut hooks, &mut factory)?
    };
    if outcome.cancelled {
        return Err("resume cancelled".to_string());
    }
    let (services, session_manager, _diagnostics) = runtime.into_parts();
    let settings_manager = Arc::clone(&services.settings_manager);
    let resource_loader = Arc::new(Mutex::new(services.resource_loader));
    build_session_with(
        parsed,
        model_runtime,
        SessionBuildInput {
            cwd: services.cwd.to_string_lossy().to_string(),
            agent_dir: services.agent_dir.to_string_lossy().to_string(),
            session_manager: Some(session_manager),
            settings_manager: Some(settings_manager),
            resource_loader: Some(resource_loader),
            start_reason: "resume".to_string(),
            previous_session_file: previous,
            project_trust_override: parsed.project_trust_override,
            prompt_for_trust: false,
        },
    )
    .await
}

/// A session replacement to install after `/resume`, `/new` or `/fork`
/// (upstream the runtime rebinding): the live session, its extension wiring,
/// and the editor text / status the rebuilt mode starts with.
struct InteractiveReplacement {
    session: AgentSession,
    wiring: Option<ExtensionWiring>,
    initial_editor_text: Option<String>,
    initial_status: Option<String>,
}

/// Build the replacement session for `/new` (upstream
/// `runtimeHost.newSession` + `rebindCurrentSession`). Same shape as
/// [`resume_session`]: a fresh services bundle + model runtime for the new
/// session manager, which the runtime creates through the same factory.
///
/// divergence: upstream keeps the old session's renderer and clears the chat
/// through its diff; the port rebuilds the run loop, so the caller asks the
/// rebuilt run for a cleared first frame (`clear_screen_on_start`).
async fn new_session(
    parsed: &Args,
    current: &Arc<AgentSession>,
) -> Result<(AgentSession, ExtensionWiring), String> {
    let agent_dir = agent_dir();
    let model_runtime = create_model_runtime(&agent_dir).await?;
    // Upstream `newSession` starts from the live session manager: a persisted
    // session creates a new file in the same directory, an in-memory one
    // stays in memory.
    let current_manager = current
        .session_manager()
        .lock()
        .expect("session lock")
        .clone();
    let mut factory = replacement_factory(parsed, &agent_dir);
    let runtime =
        AgentSessionRuntime::create(&mut factory, current.cwd(), &agent_dir, current_manager)?;
    let previous = runtime
        .session_manager()
        .session_file()
        .map(|path| path.to_string_lossy().to_string());
    let (outcome, runtime) = {
        let mut hooks = RuntimeHooks::default();
        runtime.new_session(None, &mut hooks, &mut factory)?
    };
    if outcome.cancelled {
        return Err("new session cancelled".to_string());
    }
    let (services, session_manager, _diagnostics) = runtime.into_parts();
    let settings_manager = Arc::clone(&services.settings_manager);
    let resource_loader = Arc::new(Mutex::new(services.resource_loader));
    build_session_with(
        parsed,
        model_runtime,
        SessionBuildInput {
            cwd: services.cwd.to_string_lossy().to_string(),
            agent_dir: services.agent_dir.to_string_lossy().to_string(),
            session_manager: Some(session_manager),
            settings_manager: Some(settings_manager),
            resource_loader: Some(resource_loader),
            start_reason: "new".to_string(),
            previous_session_file: previous,
            project_trust_override: parsed.project_trust_override,
            prompt_for_trust: false,
        },
    )
    .await
}

/// Rebuild the runtime as a branched session for `/fork` (position `before`)
/// or `/clone` (position `at`), mirroring upstream `runtimeHost.fork`. Returns
/// `None` when a `session_before_fork` handler cancelled.
async fn fork_session(
    parsed: &Args,
    current: &Arc<AgentSession>,
    entry_id: &str,
    position: &str,
) -> Result<Option<(AgentSession, ExtensionWiring)>, String> {
    let agent_dir = agent_dir();
    let model_runtime = create_model_runtime(&agent_dir).await?;
    // Snapshot the live session: a fork branches whatever the session holds,
    // including an in-memory session that has no file yet (upstream mutates
    // the live manager in place).
    let current_manager = current
        .session_manager()
        .lock()
        .expect("session lock")
        .clone();
    let mut factory = replacement_factory(parsed, &agent_dir);
    let runtime =
        AgentSessionRuntime::create(&mut factory, current.cwd(), &agent_dir, current_manager)?;
    let previous = runtime
        .session_manager()
        .session_file()
        .map(|path| path.to_string_lossy().to_string());
    let (outcome, runtime) = {
        let mut hooks = RuntimeHooks::default();
        runtime.fork(entry_id, position, &mut hooks, &mut factory)?
    };
    if outcome.cancelled {
        return Ok(None);
    }
    let (services, session_manager, _diagnostics) = runtime.into_parts();
    let settings_manager = Arc::clone(&services.settings_manager);
    let resource_loader = Arc::new(Mutex::new(services.resource_loader));
    let (session, wiring) = build_session_with(
        parsed,
        model_runtime,
        SessionBuildInput {
            cwd: services.cwd.to_string_lossy().to_string(),
            agent_dir: services.agent_dir.to_string_lossy().to_string(),
            session_manager: Some(session_manager),
            settings_manager: Some(settings_manager),
            resource_loader: Some(resource_loader),
            start_reason: "fork".to_string(),
            previous_session_file: previous,
            project_trust_override: parsed.project_trust_override,
            prompt_for_trust: false,
        },
    )
    .await?;
    Ok(Some((session, wiring)))
}

async fn run_print(parsed: &Args, app_mode: AppMode) -> ExitCode {
    let (session, wiring) = match build_session(parsed, false).await {
        Ok(built) => built,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };

    let mode = if app_mode == AppMode::Json {
        PrintModeMode::Json
    } else {
        PrintModeMode::Text
    };
    let session = Arc::new(session);
    wiring.bind_session(&session);
    // Refresh before `bind_extensions` fires `session_start`, so extensions
    // reading the tool / command lists during it see the real data.
    wiring.refresh_extension_data();
    session
        .bind_extensions(ExtensionBindings {
            ui_context: Some(false),
            mode: Some(mode_label(mode).to_string()),
            on_error: None,
        })
        .await;
    wiring.refresh_extension_data();

    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
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

async fn run_rpc(parsed: &Args) -> ExitCode {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let agent_dir = agent_dir();
    let model_runtime = match create_model_runtime(&agent_dir).await {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    let (session, wiring) = match build_session_with(
        parsed,
        Arc::clone(&model_runtime),
        SessionBuildInput {
            cwd,
            agent_dir,
            session_manager: None,
            settings_manager: None,
            resource_loader: None,
            start_reason: "startup".to_string(),
            previous_session_file: None,
            project_trust_override: parsed.project_trust_override,
            prompt_for_trust: false,
        },
    )
    .await
    {
        Ok(built) => built,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };

    let session = Arc::new(session);
    wiring.bind_session(&session);
    // Refresh before `bind_extensions` fires `session_start`, so extensions
    // reading the tool / command lists during it see the real data.
    wiring.refresh_extension_data();
    session
        .bind_extensions(ExtensionBindings {
            ui_context: Some(false),
            mode: Some("rpc".to_string()),
            on_error: None,
        })
        .await;
    wiring.refresh_extension_data();

    let out: Arc<Mutex<Box<dyn std::io::Write + Send>>> =
        Arc::new(Mutex::new(Box::new(std::io::stdout())));
    let stdin = std::io::stdin();
    let host: Arc<dyn RpcRuntimeHost> = Arc::new(CliRuntimeHost {
        parsed: parsed.clone(),
        model_runtime,
        current: Mutex::new(Some(Arc::clone(&session))),
        wirings: Mutex::new(Vec::new()),
    });
    match run_rpc_mode_with_host(session, stdin.lock(), out, Some(host)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::from(1)
        }
    }
}

/// Run the interactive TUI (upstream `main.ts`'s interactive branch plus the
/// host loop in [`pillar_coding_agent::modes::interactive::run`]).
async fn run_interactive(
    parsed: &Args,
    migrations: &pillar_coding_agent::migrations::MigrationResult,
) -> ExitCode {
    // Upstream shows the deprecation warnings before the TUI takes the screen.
    pillar_coding_agent::migrations::show_deprecation_warnings(&migrations.deprecation_warnings);
    let (session, wiring) = match build_session(parsed, true).await {
        Ok(built) => built,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    let mut session = Arc::new(session);
    wiring.bind_session(&session);
    // The `ctx` facts the extensions see (upstream `bindCore` + the UI
    // context binding below).
    wiring.set_extension_context(ExtensionContextFacts {
        cwd: session
            .session_manager()
            .lock()
            .expect("session")
            .cwd()
            .to_string(),
        mode: ExtensionMode::Tui,
        has_ui: true,
    });
    // Refresh before `bind_extensions` fires `session_start`, so extensions
    // reading the tool / command lists during it see the real data.
    wiring.refresh_extension_data();
    session
        .bind_extensions(ExtensionBindings {
            // Upstream `bindings.uiContext = this.createExtensionUIContext()`:
            // the interactive mode has a dialog-capable UI.
            ui_context: Some(true),
            mode: Some("tui".to_string()),
            on_error: None,
        })
        .await;
    wiring.refresh_extension_data();

    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let transcript = {
        let settings = session.settings_manager().lock().expect("settings lock");
        TranscriptSettings {
            hide_thinking_block: settings.hide_thinking_block(),
            hidden_thinking_label: "Thinking...".to_string(),
            output_pad: settings.output_pad() as usize,
            tool_output_expanded: false,
            show_images: settings.show_images(),
            image_width_cells: settings.image_width_cells() as usize,
            show_cache_miss_notices: settings.show_cache_miss_notices(),
        }
    };

    // divergence: the fullscreen (alt-screen) renderer needs the layout-root
    // bridge, which is not ported; fall back to the regular screen.
    let tui_mode = match parsed.tui_mode {
        Some(pillar_coding_agent::cli::args::TuiMode::Fullscreen) => {
            eprintln!("Warning: fullscreen TUI mode is not ported yet; using the regular screen");
            Some(InteractiveTuiMode::Regular)
        }
        _ => Some(InteractiveTuiMode::Regular),
    };

    let initial_message = prepare_initial_message(parsed, &cwd);
    let mut options = InteractiveRunOptions {
        mode: InteractiveModeOptions {
            tui_mode,
            clear_on_shrink: None,
            show_terminal_progress: None,
            version: Some(VERSION.to_string()),
            on_terminal_title: None,
            on_terminal_progress: None,
            cwd_git_paths: None,
            // Filled in by the run loop (the 2-column picker's history and the
            // terminal height it lays out against).
            agent_dir: None,
            terminal_rows: None,
        },
        transcript,
        markdown_transformers: Vec::new(),
        // The run installs its pump-backed `ctx.ui` bridge here (upstream the
        // mode owns the extension UI context).
        extension_ui: Some(Arc::clone(&wiring.ui_slot)),
        initial_message,
        initial_editor_text: None,
        initial_status: None,
        // The first entry paints into a terminal the user just started the
        // binary in; only a rebuilt (replacement) run clears.
        clear_screen_on_start: false,
        agent_dir: PathBuf::from(agent_dir()),
    };

    // Upstream `runtimeHost.switchSession` rebinds the live session inside
    // one interactive mode instance; the port rebuilds the run loop instead
    // (see the run module's outcome docs).
    let mut wirings: Vec<ExtensionWiring> = Vec::new();
    let mut first_entry = true;
    loop {
        if !first_entry {
            // Every re-entry rebuilds the run loop with a fresh renderer (the
            // port's session-replacement divergence): without this its first
            // frame would be painted below the replaced session's frame
            // instead of erasing it (upstream keeps one renderer and reaches
            // `fullRender(true)` through its diff).
            options.clear_screen_on_start = true;
        }
        first_entry = false;
        let outcome = match run_interactive_process(Arc::clone(&session), options.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("Error: {error}");
                return ExitCode::from(1);
            }
        };
        // `None` when the replacement was cancelled and the same session
        // continues.
        let replacement: Option<InteractiveReplacement> = match outcome {
            InteractiveOutcome::Exit(code) => return ExitCode::from(code as u8),
            InteractiveOutcome::NewSession => match new_session(parsed, &session).await {
                Ok((next, wiring)) => Some(InteractiveReplacement {
                    session: next,
                    wiring: Some(wiring),
                    initial_editor_text: None,
                    // Upstream `handleClearCommand`'s confirmation line.
                    initial_status: Some("✓ New session started".to_string()),
                }),
                Err(error) => {
                    eprintln!("Error: {error}");
                    return ExitCode::from(1);
                }
            },
            InteractiveOutcome::SwitchSession { session_path } => {
                match resume_session(parsed, &session, &session_path).await {
                    Ok((next, wiring)) => Some(InteractiveReplacement {
                        session: next,
                        wiring: Some(wiring),
                        initial_editor_text: None,
                        initial_status: None,
                    }),
                    Err(error) => {
                        eprintln!("Error: {error}");
                        return ExitCode::from(1);
                    }
                }
            }
            InteractiveOutcome::ForkSession {
                entry_id,
                position,
                editor_text,
            } => match fork_session(parsed, &session, &entry_id, &position).await {
                Ok(Some((next, wiring))) => {
                    let status = if position == "at" {
                        "Cloned to new session"
                    } else {
                        "Forked to new session"
                    };
                    Some(InteractiveReplacement {
                        session: next,
                        wiring: Some(wiring),
                        initial_editor_text: editor_text,
                        initial_status: Some(status.to_string()),
                    })
                }
                Ok(None) => None,
                Err(error) => {
                    eprintln!("Error: {error}");
                    return ExitCode::from(1);
                }
            },
        };

        let Some(replacement) = replacement else {
            // Upstream the cancelled fork returns to the same mode instance.
            options.initial_message = None;
            options.initial_editor_text = None;
            options.initial_status = None;
            continue;
        };
        let InteractiveReplacement {
            session: next,
            wiring,
            initial_editor_text,
            initial_status,
        } = replacement;

        let next = Arc::new(next);
        if let Some(wiring) = wiring.as_ref() {
            // The replacement wiring is bound to the replacement session
            // before its extensions react, exactly like the initial startup:
            // otherwise the `@pillar` host callbacks have no session to
            // resolve.
            wiring.bind_session(&next);
            wiring.set_extension_context(ExtensionContextFacts {
                cwd: next
                    .session_manager()
                    .lock()
                    .expect("session")
                    .cwd()
                    .to_string(),
                mode: ExtensionMode::Tui,
                has_ui: true,
            });
        }
        // Upstream `rebindCurrentSession` → `bindCurrentSessionExtensions`.
        next.bind_extensions(ExtensionBindings {
            ui_context: Some(true),
            mode: Some("tui".to_string()),
            on_error: None,
        })
        .await;
        if let Some(wiring) = wiring.as_ref() {
            wiring.refresh_extension_data();
            // The replacement session carries its own wiring: point the run
            // loop at its `ctx.ui` bridge.
            options.extension_ui = Some(Arc::clone(&wiring.ui_slot));
        }
        session = next;
        wirings.extend(wiring);
        options.initial_message = None;
        options.initial_editor_text = initial_editor_text;
        options.initial_status = initial_status;
        // The transcript settings rebuild per entry: a resumed session
        // can live in another cwd with different settings.
        options.transcript = {
            let settings = session.settings_manager().lock().expect("settings lock");
            TranscriptSettings {
                hide_thinking_block: settings.hide_thinking_block(),
                hidden_thinking_label: "Thinking...".to_string(),
                output_pad: settings.output_pad() as usize,
                tool_output_expanded: false,
                show_images: settings.show_images(),
                image_width_cells: settings.image_width_cells() as usize,
                show_cache_miss_notices: settings.show_cache_miss_notices(),
            }
        };
    }
}

/// Runtime host for `--mode rpc` (upstream `runtimeHost`): drives
/// [`AgentSessionRuntime`] for session replacement and rebuilds a live
/// session for the replacement's session manager.
///
/// divergence: the runtime is rebuilt from the current session file for
/// every replacement (upstream keeps one runtime instance), so in-memory
/// sessions cannot be replaced.
struct CliRuntimeHost {
    parsed: Args,
    model_runtime: Arc<ModelRuntime>,
    current: Mutex<Option<Arc<AgentSession>>>,
    /// Keeps every replacement's Luau wiring alive for the process lifetime.
    wirings: Mutex<Vec<ExtensionWiring>>,
}

impl CliRuntimeHost {
    /// Open a runtime over the current session's file.
    fn runtime_for_current(&self) -> Result<AgentSessionRuntime, String> {
        let session = self
            .current
            .lock()
            .expect("current session lock")
            .clone()
            .ok_or_else(|| "no current session".to_string())?;
        let session_file = session
            .session_manager()
            .lock()
            .expect("session lock")
            .session_file()
            .map(Path::to_path_buf);
        let session_file = session_file
            .ok_or_else(|| "session replacement requires a persisted session".to_string())?;
        let agent_dir = agent_dir();
        let session_manager = SessionManager::open(&session_file, None, None)?;
        let mut factory = replacement_factory(&self.parsed, &agent_dir);
        AgentSessionRuntime::create(&mut factory, session.cwd(), &agent_dir, session_manager)
    }

    /// Build the live session for a completed replacement flow.
    async fn build_replacement(
        &self,
        runtime: AgentSessionRuntime,
        start_reason: &str,
        previous_session_file: Option<String>,
    ) -> Result<Arc<AgentSession>, String> {
        let (services, session_manager, _diagnostics) = runtime.into_parts();
        let settings_manager = Arc::clone(&services.settings_manager);
        let resource_loader = Arc::new(Mutex::new(services.resource_loader));
        let (session, wiring) = build_session_with(
            &self.parsed,
            Arc::clone(&self.model_runtime),
            SessionBuildInput {
                cwd: services.cwd.to_string_lossy().to_string(),
                agent_dir: services.agent_dir.to_string_lossy().to_string(),
                session_manager: Some(session_manager),
                settings_manager: Some(settings_manager),
                resource_loader: Some(resource_loader),
                start_reason: start_reason.to_string(),
                previous_session_file,
                project_trust_override: self.parsed.project_trust_override,
                prompt_for_trust: false,
            },
        )
        .await?;
        let session = Arc::new(session);
        wiring.bind_session(&session);
        // Refresh before `bind_extensions` fires `session_start`, so extensions
        // reading the tool / command lists during it see the real data.
        wiring.refresh_extension_data();
        self.wirings.lock().expect("wirings lock").push(wiring);
        *self.current.lock().expect("current session lock") = Some(Arc::clone(&session));
        Ok(session)
    }
}

/// The runtime factory: cwd-bound services for a replacement (the session
/// manager itself comes from the replacement flow).
fn replacement_factory<'a>(
    parsed: &'a Args,
    agent_dir: &'a str,
) -> impl FnMut(RuntimeFactoryInput) -> Result<RuntimeFactoryResult, String> + 'a {
    move |input: RuntimeFactoryInput| {
        // A replacement can land in another cwd, so the trust decision is
        // resolved per cwd (never prompted: the TUI owns the terminal here).
        let project_trusted =
            stored_project_trust(&input.cwd, agent_dir, parsed.project_trust_override);
        let services = create_agent_session_services(
            &input.cwd,
            CreateAgentSessionServicesOptions {
                agent_dir: Some(agent_dir.to_string()),
                additional_extension_paths: parsed.extensions.clone().unwrap_or_default(),
                project_trusted: Some(project_trusted),
                ..Default::default()
            },
        )?;
        Ok(RuntimeFactoryResult {
            services,
            session_manager: input.session_manager,
            session_start_reason: input.session_start_reason,
            previous_session_file: input.previous_session_file,
            diagnostics: Vec::new(),
        })
    }
}

#[async_trait::async_trait]
impl RpcRuntimeHost for CliRuntimeHost {
    async fn new_session(
        &self,
        parent_session: Option<&str>,
    ) -> Result<(SessionReplacement, Option<Arc<AgentSession>>), String> {
        let runtime = self.runtime_for_current()?;
        let previous = runtime
            .session_manager()
            .session_file()
            .map(|path| path.to_string_lossy().to_string());
        let (outcome, runtime) = {
            let mut hooks = RuntimeHooks::default();
            let agent_dir = agent_dir();
            let mut factory = replacement_factory(&self.parsed, &agent_dir);
            runtime.new_session(parent_session, &mut hooks, &mut factory)?
        };
        if outcome.cancelled {
            return Ok((
                SessionReplacement {
                    cancelled: true,
                    selected_text: None,
                },
                None,
            ));
        }
        let session = self.build_replacement(runtime, "new", previous).await?;
        Ok((
            SessionReplacement {
                cancelled: false,
                selected_text: None,
            },
            Some(session),
        ))
    }

    async fn switch_session(
        &self,
        session_path: &str,
    ) -> Result<(SessionReplacement, Option<Arc<AgentSession>>), String> {
        let runtime = self.runtime_for_current()?;
        let previous = runtime
            .session_manager()
            .session_file()
            .map(|path| path.to_string_lossy().to_string());
        let (outcome, runtime) = {
            let mut hooks = RuntimeHooks::default();
            let agent_dir = agent_dir();
            let mut factory = replacement_factory(&self.parsed, &agent_dir);
            runtime.switch_session(session_path, None, &mut hooks, &mut factory)?
        };
        if outcome.cancelled {
            return Ok((
                SessionReplacement {
                    cancelled: true,
                    selected_text: None,
                },
                None,
            ));
        }
        let session = self.build_replacement(runtime, "resume", previous).await?;
        Ok((
            SessionReplacement {
                cancelled: false,
                selected_text: None,
            },
            Some(session),
        ))
    }

    async fn fork(
        &self,
        entry_id: &str,
        position: &str,
    ) -> Result<(SessionReplacement, Option<Arc<AgentSession>>), String> {
        let runtime = self.runtime_for_current()?;
        let previous = runtime
            .session_manager()
            .session_file()
            .map(|path| path.to_string_lossy().to_string());
        let (outcome, runtime) = {
            let mut hooks = RuntimeHooks::default();
            let agent_dir = agent_dir();
            let mut factory = replacement_factory(&self.parsed, &agent_dir);
            runtime.fork(entry_id, position, &mut hooks, &mut factory)?
        };
        if outcome.cancelled {
            return Ok((
                SessionReplacement {
                    cancelled: true,
                    selected_text: outcome.selected_text,
                },
                None,
            ));
        }
        let session = self.build_replacement(runtime, "fork", previous).await?;
        Ok((
            SessionReplacement {
                cancelled: false,
                selected_text: outcome.selected_text,
            },
            Some(session),
        ))
    }
}

fn mode_label(mode: PrintModeMode) -> &'static str {
    match mode {
        PrintModeMode::Text => "print",
        PrintModeMode::Json => "json",
    }
}
