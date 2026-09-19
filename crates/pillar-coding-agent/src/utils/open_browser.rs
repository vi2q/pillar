//! Port of packages/coding-agent/src/utils/open-browser.ts (pi v0.84.3):
//! open a URL (or file) in the platform browser/default handler.
//!
//! The launcher is *never* run through a shell. On Windows `cmd /c start` is
//! deliberately avoided: cmd.exe re-parses metacharacters (`&`, `|`, `^`, …)
//! before `start` runs, which would make an attacker-controlled URL
//! injectable.
//!
//! divergences:
//! - the launcher spawn is `#[cfg]`-gated off `wasm32` (there is no process
//!   spawn there); the guest platform is decided by
//!   [`OpenBrowserPlatform::native`] otherwise.
//! - [`launcher_command`] exposes the platform→command decision upstream
//!   hides inside `openBrowser`, so the decision tree is testable without
//!   launching a real browser.

/// The platform family the launcher branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenBrowserPlatform {
    Darwin,
    Win32,
    Other,
}

impl OpenBrowserPlatform {
    /// The platform of the running process.
    pub fn native() -> Self {
        if cfg!(target_os = "macos") {
            OpenBrowserPlatform::Darwin
        } else if cfg!(target_os = "windows") {
            OpenBrowserPlatform::Win32
        } else {
            OpenBrowserPlatform::Other
        }
    }
}

/// The launcher program and its argv for `target` (upstream's platform
/// ternary). `target` is always a single argv entry — never shell-parsed.
pub fn launcher_command(platform: OpenBrowserPlatform, target: &str) -> (String, Vec<String>) {
    match platform {
        OpenBrowserPlatform::Darwin => ("open".to_string(), vec![target.to_string()]),
        OpenBrowserPlatform::Win32 => (
            "rundll32".to_string(),
            vec![
                "url.dll,FileProtocolHandler".to_string(),
                target.to_string(),
            ],
        ),
        OpenBrowserPlatform::Other => ("xdg-open".to_string(), vec![target.to_string()]),
    }
}

/// Open `target` in the platform browser/default handler (upstream
/// `openBrowser`).
///
/// Best-effort: launcher failures (for example a missing `xdg-open`) are
/// swallowed. Callers still show the target to the user, so a launcher
/// failure must not become a process crash.
pub fn open_browser(target: &str) {
    let (program, args) = launcher_command(OpenBrowserPlatform::native(), target);
    let _ = spawn_detached(&program, &args);
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_detached(program: &str, args: &[String]) -> bool {
    use std::process::{Command, Stdio};

    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(target_arch = "wasm32")]
fn spawn_detached(_program: &str, _args: &[String]) -> bool {
    // No process spawn on the guest; opening the target is the host's job.
    false
}
