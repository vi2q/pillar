//! `pillar --update` / `pillar update self`: reinstall the CLI from its
//! canonical git source.
//!
//! pillar is a Rust binary; the documented install (README.md) is
//! `cargo install --git https://github.com/vi2q/pillar pillar-cli`. Rather than
//! invent a download / stage / activate pipeline, the self-updater shells out
//! to that same command with `--force`. `cargo install` builds first and only
//! replaces the installed binary after a successful build, so a failed update
//! leaves the running binary in place.
//!
//! divergence from pi: pi checks `https://pi.dev/api/latest-version` and can
//! stage/verify/activate a managed release. The port has no release endpoint
//! (no published artifacts yet) and always rebuilds the checked-out `main`, so
//! it reports progress instead of an "already current" state.

use std::io::Write;

use pillar_coding_agent::cli::args::APP_NAME;

/// The canonical git source. Keep in sync with README.md / the install docs.
pub const GIT_URL: &str = "https://github.com/vi2q/pillar";
/// The installed package (the binary it provides is `pillar`).
pub const PACKAGE: &str = "pillar-cli";

/// A process runner: `(program, args) -> exit code`, or a spawn error.
pub type CommandRunner<'a> = dyn Fn(&str, &[String]) -> Result<i32, String> + 'a;

/// The cargo invocation the self-updater runs.
pub fn cargo_install_args() -> Vec<String> {
    vec![
        "install".to_string(),
        "--git".to_string(),
        GIT_URL.to_string(),
        PACKAGE.to_string(),
        "--force".to_string(),
    ]
}

/// Whether `source` names the CLI itself rather than a package
/// (`pillar update self` / `pillar update pillar`).
pub fn is_self_update_source(source: Option<&str>) -> bool {
    matches!(source, Some("self") | Some("pillar"))
}

/// Whether offline mode is set through the environment.
pub fn offline_from_env() -> bool {
    matches!(
        std::env::var("PILLAR_OFFLINE").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Spawn cargo, inheriting stdio so the build output streams live. The exit
/// code is normalized (`None` for a signal death becomes `-1`).
pub fn spawn(program: &str, args: &[String]) -> Result<i32, String> {
    std::process::Command::new(program)
        .args(args)
        .status()
        .map(|status| status.code().unwrap_or(-1))
        .map_err(|error| format!("could not run `{program}`: {error}"))
}

/// The testable core: `runner` lets a test exercise the success / failure /
/// offline paths without invoking cargo. Production passes [`spawn`].
pub fn run_checked(
    runner: &CommandRunner<'_>,
    out: &mut dyn Write,
    offline: bool,
) -> Result<(), String> {
    if offline {
        return Err(format!(
            "refusing to update {APP_NAME} in offline mode (unset PILLAR_OFFLINE / drop --offline)"
        ));
    }
    writeln!(out, "Updating {APP_NAME} from {GIT_URL} (cargo install --git … --force)…")
        .map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())?;
    let code = runner("cargo", &cargo_install_args())?;
    if code != 0 {
        return Err(format!("`cargo install` failed (exit {code})"));
    }
    writeln!(
        out,
        "Updated {APP_NAME} — run `{APP_NAME} --version` to confirm."
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Run the self-update with the real process runner.
pub fn run(out: &mut dyn Write, offline: bool) -> Result<(), String> {
    run_checked(&spawn, out, offline)
}
