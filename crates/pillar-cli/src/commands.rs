//! The `pillar <command>` forms: package install / remove / update / list.
//!
//! These are the explicit, user-approved package operations
//! (docs/ARCHITECTURE-REVIEW-s05c0.md 0/6): resolving a session never installs
//! anything, and every fetch or delete here goes through the effect broker,
//! whose audit trail records what the command line approved.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pillar_coding_agent::cli::args::{Subcommand, SubcommandArgs};
use pillar_coding_agent::core::package_manager::{
    DefaultPackageManager, ParsedSource, parse_source,
};
use pillar_coding_agent::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};

use crate::effects::EffectBroker;
use crate::self_update;
use crate::trust::stored_project_trust;

/// Run one `pillar <command>` invocation, writing user-facing output to `out`.
pub fn run_subcommand(
    args: &SubcommandArgs,
    cwd: &str,
    agent_dir: &str,
    trust_override: Option<bool>,
    out: &mut dyn Write,
) -> Result<(), String> {
    run_subcommand_with(
        args,
        cwd,
        agent_dir,
        trust_override,
        out,
        &self_update::spawn,
    )
}

/// [`run_subcommand`] with an injected process runner, so the self-update path
/// (`pillar update self` / `pi`) is testable without invoking cargo.
#[allow(clippy::too_many_arguments)]
pub fn run_subcommand_with(
    args: &SubcommandArgs,
    cwd: &str,
    agent_dir: &str,
    trust_override: Option<bool>,
    out: &mut dyn Write,
    command_runner: &self_update::CommandRunner<'_>,
) -> Result<(), String> {
    if args.help {
        return writeln!(out, "{}", help_for(args.command)).map_err(|error| error.to_string());
    }
    match args.command {
        Subcommand::Config => return Err("`pillar config` is not ported yet".to_string()),
        // `pillar auth …` parses its own options and runs from the CLI entry
        // point ([`crate::auth::run_auth_command`]) before the option parser.
        Subcommand::Auth => {
            return Err("`pillar auth` runs before subcommand dispatch".to_string());
        }
        _ => {}
    }

    // `pillar update self` / `pillar update pillar` name the CLI itself, not
    // a package (the help line documents the `self|pillar` forms). Same path
    // as `pillar --update`.
    if args.command == Subcommand::Update
        && self_update::is_self_update_source(args.source.as_deref())
    {
        return self_update::run_checked(command_runner, out, self_update::offline_from_env());
    }

    // The user's trust decision gates a project-scope (`-l`) install exactly as
    // it gates project extensions.
    let project_trusted = stored_project_trust(cwd, agent_dir, trust_override);
    let settings = Arc::new(Mutex::new(SettingsManager::create(
        cwd,
        Path::new(agent_dir),
        SettingsManagerCreateOptions {
            project_trusted: Some(project_trusted),
        },
    )));
    let mut manager = DefaultPackageManager::new(cwd, Path::new(agent_dir), Arc::clone(&settings));
    // The command line is the approval: the broker records the intent and lets
    // it through (there is no interactive policy engine yet).
    manager.set_effect_authorizer(EffectBroker::permissive().authorizer());

    let scope_note = if args.local { " (project)" } else { "" };
    match args.command {
        Subcommand::Install => {
            let source = args.source.as_deref().expect("the parser requires it");
            manager.install_and_persist(source, args.local)?;
            writeln!(out, "Installed {source}{scope_note}").map_err(|error| error.to_string())?;
        }
        Subcommand::Remove => {
            let source = args.source.as_deref().expect("the parser requires it");
            let removed = manager.remove_and_persist(source, args.local)?;
            if removed {
                writeln!(out, "Removed {source}{scope_note}").map_err(|error| error.to_string())?;
            } else {
                writeln!(
                    out,
                    "Removed the installation; {source} was not in settings"
                )
                .map_err(|error| error.to_string())?;
            }
        }
        Subcommand::Update => {
            manager.update(args.source.as_deref())?;
            match &args.source {
                Some(source) => writeln!(out, "Updated {source}{scope_note}")
                    .map_err(|error| error.to_string())?,
                None => writeln!(out, "Updated the configured sources")
                    .map_err(|error| error.to_string())?,
            }
        }
        Subcommand::List => {
            let configured = configured_sources(&settings);
            if configured.is_empty() {
                writeln!(out, "No package sources configured")
                    .map_err(|error| error.to_string())?;
            }
            for (source, project) in configured {
                let installed = is_installed(&manager, &source, project);
                writeln!(
                    out,
                    "{source}\t{}\t{}",
                    if project { "project" } else { "user" },
                    if installed {
                        "installed"
                    } else {
                        "not installed"
                    }
                )
                .map_err(|error| error.to_string())?;
            }
        }
        Subcommand::Config | Subcommand::Auth => unreachable!("returned above"),
    }
    Ok(())
}

/// The `<source> [-l]` help lines the top-level help points at.
pub fn help_for(command: Subcommand) -> String {
    match command {
        Subcommand::Install => {
            "Usage: pillar install <source> [-l]\n\nInstall an extension source and add it to settings.\n  -l, --local   install into the project (.pillar) instead of the user directory".to_string()
        }
        Subcommand::Remove => {
            "Usage: pillar remove <source> [-l]\n\nRemove an installed extension source (alias: uninstall).".to_string()
        }
        Subcommand::Update => {
            "Usage: pillar update [<source>|self|pillar] [-l]\n\nUpdate one configured source, or every source when none is given. `self` / `pillar` reinstall the pillar CLI itself from git (same as `pillar --update`).".to_string()
        }
        Subcommand::List => {
            "Usage: pillar list\n\nList the configured package sources and whether they are installed.".to_string()
        }
        Subcommand::Config => "pillar config is not ported yet.".to_string(),
        Subcommand::Auth => "Usage: pillar auth <command>\n\nPrint credentials or check provider readiness. See `pillar auth --help` for the commands.".to_string(),
    }
}

/// The `packages` entries of the global and project settings.
fn configured_sources(settings: &Arc<Mutex<SettingsManager>>) -> Vec<(String, bool)> {
    let settings = settings.lock().expect("settings lock");
    let mut sources = Vec::new();
    for (value, project) in [
        (settings.global_settings(), false),
        (settings.project_settings(), true),
    ] {
        let Some(packages) = value.get("packages").and_then(|value| value.as_array()) else {
            continue;
        };
        for entry in packages {
            let source = match entry {
                serde_json::Value::String(source) => source.clone(),
                other => other
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            };
            if !source.is_empty() {
                sources.push((source, project));
            }
        }
    }
    sources
}

/// Whether the source's install directory exists (a local path is "installed"
/// when the path is there).
fn is_installed(manager: &DefaultPackageManager, source: &str, project: bool) -> bool {
    use pillar_coding_agent::core::package_manager::SourceScope;
    let scope = if project {
        SourceScope::Project
    } else {
        SourceScope::User
    };
    match parse_source(source) {
        ParsedSource::Npm(npm) => manager.get_npm_install_path(&npm, scope).exists(),
        ParsedSource::Git(git) => manager
            .get_git_install_path(&git, scope)
            .map(|path| path.exists())
            .unwrap_or(false),
        ParsedSource::Local(path) => Path::new(&path).exists(),
    }
}
