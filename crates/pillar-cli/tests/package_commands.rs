//! The `pillar <command>` package forms: they are the explicit, user-approved
//! install / remove / update path, so they must reach the package manager with
//! the right scope and respect the project trust decision.

use std::path::{Path, PathBuf};

use pillar_cli::commands::run_subcommand;
use pillar_coding_agent::cli::args::{Subcommand, SubcommandArgs, parse_args};

fn temp_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pillar-cli-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn command(name: &str, source: Option<&str>, local: bool) -> SubcommandArgs {
    let mut args = vec![name.to_string()];
    if let Some(source) = source {
        args.push(source.to_string());
    }
    if local {
        args.push("-l".to_string());
    }
    parse_args(&args)
        .subcommand
        .expect("the first argument is a command")
}

fn run(args: &SubcommandArgs, cwd: &Path, agent_dir: &Path, trust: Option<bool>) -> String {
    let mut out: Vec<u8> = Vec::new();
    run_subcommand(
        args,
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        trust,
        &mut out,
    )
    .expect("the command succeeds");
    String::from_utf8(out).expect("utf-8 output")
}

fn settings_text(agent_dir: &Path) -> String {
    std::fs::read_to_string(agent_dir.join("settings.json")).unwrap_or_default()
}

#[test]
fn install_list_and_remove_a_user_source() {
    let cwd = temp_dir("cmd-cwd");
    let agent_dir = temp_dir("cmd-agent");
    let source = temp_dir("cmd-source");

    let installed = run(
        &command("install", Some(&source.to_string_lossy()), false),
        &cwd,
        &agent_dir,
        None,
    );
    assert!(installed.contains("Installed"), "{installed}");
    assert!(
        settings_text(&agent_dir).contains(&source.to_string_lossy().to_string()),
        "the source is persisted: {}",
        settings_text(&agent_dir)
    );

    let listed = run(&command("list", None, false), &cwd, &agent_dir, None);
    assert!(listed.contains("user"), "{listed}");
    assert!(listed.contains("installed"), "{listed}");

    let removed = run(
        &command("remove", Some(&source.to_string_lossy()), false),
        &cwd,
        &agent_dir,
        None,
    );
    assert!(removed.contains("Removed"), "{removed}");
    assert!(
        !settings_text(&agent_dir).contains(&source.to_string_lossy().to_string()),
        "the source is gone from settings: {}",
        settings_text(&agent_dir)
    );
    assert!(
        run(&command("list", None, false), &cwd, &agent_dir, None).contains("No package sources"),
        "nothing is left to list"
    );
}

/// A project-scope install follows the same trust decision as project
/// extensions: an untrusted project cannot add a source, `--approve` can.
#[test]
fn a_project_install_respects_the_trust_decision() {
    let cwd = temp_dir("cmd-trust-cwd");
    let agent_dir = temp_dir("cmd-trust-agent");
    let source = temp_dir("cmd-trust-source");
    let install = command("install", Some(&source.to_string_lossy()), true);

    let mut out: Vec<u8> = Vec::new();
    let error = run_subcommand(
        &install,
        &cwd.to_string_lossy(),
        &agent_dir.to_string_lossy(),
        Some(false),
        &mut out,
    )
    .expect_err("an untrusted project must not install");
    assert!(error.contains("trust"), "{error}");

    run(&install, &cwd, &agent_dir, Some(true));
    assert!(settings_text(&cwd.join(".pillar")).contains("packages"));
}

#[test]
fn config_reports_that_it_is_not_ported() {
    let args = command("config", None, false);
    let mut out: Vec<u8> = Vec::new();
    let error = run_subcommand(
        &args,
        &temp_dir("cmd-x-cwd").to_string_lossy(),
        &temp_dir("cmd-x-agent").to_string_lossy(),
        None,
        &mut out,
    )
    .expect_err("not ported");
    assert!(error.contains("not ported"), "{error}");
}

/// `--help` on a command prints its usage instead of running it.
#[test]
fn a_command_help_does_not_run_it() {
    let mut args = command("install", Some("npm:whatever"), false);
    args.help = true;
    let agent_dir = temp_dir("cmd-help-agent");
    let printed = run(&args, &temp_dir("cmd-help-cwd"), &agent_dir, None);
    assert!(printed.contains("Usage: pillar install"), "{printed}");
    assert!(
        settings_text(&agent_dir).is_empty(),
        "help must not install anything"
    );
}

#[test]
fn a_command_is_only_recognized_as_the_first_argument() {
    // `install` after an option is a message, not a command.
    let parsed = parse_args(&[
        "--model".to_string(),
        "claude".to_string(),
        "install".to_string(),
        "foo".to_string(),
    ]);
    assert!(parsed.subcommand.is_none());
    assert_eq!(
        parsed.messages,
        vec!["install".to_string(), "foo".to_string()]
    );

    let parsed = parse_args(&["install".to_string()]);
    assert_eq!(
        parsed.subcommand.map(|args| args.command),
        Some(Subcommand::Install)
    );
    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("requires a <source>")),
        "{:?}",
        parsed.diagnostics
    );

    // `update` without a source updates everything.
    let parsed = parse_args(&["update".to_string()]);
    let args = parsed.subcommand.expect("command");
    assert_eq!(args.command, Subcommand::Update);
    assert!(args.source.is_none());
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}
