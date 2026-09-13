//! Port of packages/coding-agent/src/utils/clipboard.ts (pi v0.84.3):
//! copy text to the system clipboard, with the same preference order
//! (platform tools first, OSC 52 as the fallback).
//!
//! divergences:
//! - the `@mariozechner/clipboard` native addon (`clipboard-native.ts`) is
//!   not ported, so the native fast path is always skipped. Upstream already
//!   skips it on Linux and on Termux; on macOS/Windows the platform tools
//!   below are used instead.
//! - the platform decision tree is driven through an injectable
//!   [`ClipboardRunner`] plus explicit platform/env inputs, so it can be
//!   tested without touching a real clipboard.
//! - `isWaylandSession` lives in `clipboard-image.ts` upstream; it is defined
//!   here until that module lands.

use std::collections::BTreeMap;
use std::io::Write;

/// Largest accepted base64-encoded OSC 52 payload (upstream
/// `MAX_OSC52_ENCODED_LENGTH`).
pub const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// Clipboard command timeout in milliseconds (upstream `timeout: 5000`).
pub const CLIPBOARD_COMMAND_TIMEOUT_MS: u64 = 5_000;

/// The platform family the clipboard logic branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPlatform {
    Darwin,
    Win32,
    Other,
}

impl ClipboardPlatform {
    /// The platform of the running process.
    pub fn native() -> Self {
        if cfg!(target_os = "macos") {
            ClipboardPlatform::Darwin
        } else if cfg!(target_os = "windows") {
            ClipboardPlatform::Win32
        } else {
            ClipboardPlatform::Other
        }
    }
}

/// Environment lookups the clipboard logic needs, injectable for tests.
#[derive(Debug, Clone, Default)]
pub struct ClipboardEnv {
    values: BTreeMap<String, String>,
}

impl ClipboardEnv {
    pub fn new(values: BTreeMap<String, String>) -> Self {
        Self { values }
    }

    /// The process environment.
    pub fn from_process() -> Self {
        Self {
            values: std::env::vars().collect(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some_and(|value| !value.is_empty())
    }
}

/// Whether the session runs over SSH/Mosh (upstream `isRemoteSession`):
/// the terminal clipboard is unreachable, so OSC 52 is preferred.
pub fn is_remote_session(env: &ClipboardEnv) -> bool {
    env.has("SSH_CONNECTION") || env.has("SSH_CLIENT") || env.has("MOSH_CONNECTION")
}

/// Whether the session is a Wayland session (upstream `isWaylandSession`).
pub fn is_wayland_session(env: &ClipboardEnv) -> bool {
    env.has("WAYLAND_DISPLAY") || env.get("XDG_SESSION_TYPE") == Some("wayland")
}

/// The OSC 52 escape sequence for `text`, or `None` when the encoded payload
/// exceeds [`MAX_OSC52_ENCODED_LENGTH`].
pub fn osc52_sequence(text: &str) -> Option<String> {
    let encoded = base64_encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return None;
    }
    Some(format!("\x1b]52;c;{encoded}\x07"))
}

/// Write the OSC 52 sequence to `sink`; returns false when the payload is too
/// large or the write fails.
pub fn write_osc52(sink: &mut impl Write, text: &str) -> bool {
    let Some(sequence) = osc52_sequence(text) else {
        return false;
    };
    sink.write_all(sequence.as_bytes()).is_ok()
}

/// Runs the platform clipboard commands (upstream `execSync` / `spawn`).
pub trait ClipboardRunner {
    /// Run `program` with `args`, feeding `input` on stdin. Returns true when
    /// the command exits successfully.
    fn run(&mut self, program: &str, args: &[&str], input: &str) -> bool;

    /// Whether `program` is available on PATH (upstream `which wl-copy`).
    fn exists(&mut self, program: &str) -> bool;

    /// Run `program` and return its stdout when it exits successfully
    /// (upstream `execFileSync`), otherwise `None`.
    fn read(&mut self, program: &str, args: &[&str]) -> Option<String>;
}

/// The process-backed runner used outside tests.
#[derive(Debug, Default)]
pub struct ProcessClipboardRunner;

impl ClipboardRunner for ProcessClipboardRunner {
    fn run(&mut self, program: &str, args: &[&str], input: &str) -> bool {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let mut child = match Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return false,
        };
        if let Some(mut stdin) = child.stdin.take() {
            // EPIPE when the tool exits early is expected; the exit status is
            // what decides success (upstream ignores `stdin` errors).
            let _ = stdin.write_all(input.as_bytes());
            let _ = stdin.flush();
        }
        child.wait().map(|status| status.success()).unwrap_or(false)
    }

    fn exists(&mut self, program: &str) -> bool {
        use std::process::{Command, Stdio};
        Command::new("which")
            .arg(program)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn read(&mut self, program: &str, args: &[&str]) -> Option<String> {
        use std::process::Command;
        let output = Command::new(program).args(args).output().ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok()
    }
}

/// Copy text through the X11 tools (upstream `copyToX11Clipboard`): xclip
/// first, then xsel.
fn copy_to_x11_clipboard(runner: &mut dyn ClipboardRunner, text: &str) -> bool {
    if runner.run("xclip", &["-selection", "clipboard"], text) {
        return true;
    }
    runner.run("xsel", &["--clipboard", "--input"], text)
}

/// Copy `text` to the clipboard with the platform/env/runner inputs supplied.
///
/// When `remote` is true, or no platform tool succeeded, the OSC 52 fallback
/// is written to `osc52_sink`. Returns an error when nothing worked
/// (upstream `throw new Error("Failed to copy to clipboard")`).
pub fn copy_to_clipboard_with(
    text: &str,
    platform: ClipboardPlatform,
    env: &ClipboardEnv,
    runner: &mut dyn ClipboardRunner,
    osc52_sink: &mut impl Write,
) -> Result<(), String> {
    // The native addon fast path is not ported (see the module divergence
    // note), so `copied` starts false.
    let mut copied = false;
    let remote = is_remote_session(env);

    if !copied {
        match platform {
            ClipboardPlatform::Darwin => {
                copied = runner.run("pbcopy", &[], text);
            }
            ClipboardPlatform::Win32 => {
                copied = runner.run("clip", &[], text);
            }
            ClipboardPlatform::Other => {
                if env.has("TERMUX_VERSION") {
                    copied = runner.run("termux-clipboard-set", &[], text);
                }
                if !copied {
                    let has_wayland_display = env.has("WAYLAND_DISPLAY");
                    let has_x11_display = env.has("DISPLAY");
                    if is_wayland_session(env) && has_wayland_display {
                        // `wl-copy` daemonizes; the exit status decides success
                        // so a failure falls through to xclip/xsel or OSC 52.
                        if runner.exists("wl-copy") {
                            if runner.run("wl-copy", &[], text) {
                                copied = true;
                            } else if has_x11_display {
                                copied = copy_to_x11_clipboard(runner, text);
                            }
                        } else if has_x11_display {
                            copied = copy_to_x11_clipboard(runner, text);
                        }
                    } else if has_x11_display {
                        copied = copy_to_x11_clipboard(runner, text);
                    }
                }
            }
        }
    }

    if remote || !copied {
        let osc52_copied = write_osc52(osc52_sink, text);
        copied = copied || osc52_copied;
    }

    if !copied {
        return Err("Failed to copy to clipboard".to_string());
    }
    Ok(())
}

/// Copy text to the clipboard using the running process's platform and
/// environment, with OSC 52 written to `osc52_sink`.
pub fn copy_to_clipboard(text: &str, osc52_sink: &mut impl Write) -> Result<(), String> {
    let env = ClipboardEnv::from_process();
    let mut runner = ProcessClipboardRunner;
    copy_to_clipboard_with(
        text,
        ClipboardPlatform::native(),
        &env,
        &mut runner,
        osc52_sink,
    )
}

/// Read plain text from the system clipboard (upstream `readClipboardText`).
/// Returns `None` when no clipboard tool is available or the clipboard is
/// empty.
pub fn read_clipboard_text() -> Option<String> {
    let env = ClipboardEnv::from_process();
    read_clipboard_text_with(
        ClipboardPlatform::native(),
        &env,
        &mut ProcessClipboardRunner,
    )
}

/// Read the clipboard with explicit platform/env/runner inputs.
pub fn read_clipboard_text_with(
    platform: ClipboardPlatform,
    env: &ClipboardEnv,
    runner: &mut dyn ClipboardRunner,
) -> Option<String> {
    let read = |runner: &mut dyn ClipboardRunner, program: &str, args: &[&str]| -> Option<String> {
        let output = runner.read(program, args)?;
        let text = output.trim_end_matches('\n').to_string();
        if text.is_empty() { None } else { Some(text) }
    };

    if platform == ClipboardPlatform::Other
        && is_wayland_session(env)
        && env.has("WAYLAND_DISPLAY")
        && let Some(text) = read(runner, "wl-paste", &["--no-newline", "--type", "text"])
    {
        return Some(text);
    }

    // No native addon in the port; fall back to platform tools.
    let candidates: &[(&str, &[&str])] = match platform {
        ClipboardPlatform::Darwin => &[("pbpaste", &[])],
        ClipboardPlatform::Win32 => &[("powershell", &["-NoProfile", "-Command", "Get-Clipboard"])],
        ClipboardPlatform::Other => &[
            ("wl-paste", &["--no-newline", "--type", "text"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ],
    };
    for (program, args) in candidates {
        if let Some(text) = read(runner, program, args) {
            return Some(text);
        }
    }
    None
}

/// Minimal base64 encoder (standard alphabet, padded) so the clipboard
/// module needs no extra dependency.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
