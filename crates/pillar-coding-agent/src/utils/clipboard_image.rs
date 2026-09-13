//! Port of packages/coding-agent/src/utils/clipboard-image.ts (pi v0.84.3):
//! read an image from the system clipboard.
//!
//! divergences:
//! - the `@mariozechner/clipboard` native addon is not ported, so platforms
//!   other than Linux have no clipboard-image source and macOS/Windows
//!   return `None` (upstream reads them through the addon).
//! - Photon (`photon.ts`) is not ported, so formats outside
//!   [`SUPPORTED_IMAGE_MIME_TYPES`] (e.g. BMP pasted via WSLg) return `None`
//!   instead of being converted to PNG.
//! - image reads run without the upstream timeouts (`spawnSync` `timeout`);
//!   the process is only reaped on exit.
//! - `clipboard.ts` and this module share `is_wayland_session`, declared here
//!   the way upstream declares it in this file.

use std::path::{Path, PathBuf};

use crate::utils::clipboard::{ClipboardEnv, ClipboardPlatform};

/// Whether the session is a Wayland session (upstream `isWaylandSession`).
pub fn is_wayland_session(env: &ClipboardEnv) -> bool {
    env.has("WAYLAND_DISPLAY") || env.get("XDG_SESSION_TYPE") == Some("wayland")
}

/// An image read from the clipboard (upstream `ClipboardImage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// Image formats pi sends to the model unchanged (upstream
/// `SUPPORTED_IMAGE_MIME_TYPES`).
pub const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Strip parameters and case from a mime type (upstream `baseMimeType`).
pub fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_lowercase()
}

/// File extension for a supported image mime type (upstream
/// `extensionForImageMimeType`).
pub fn extension_for_image_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

/// Whether the mime type is sent to the model as-is (upstream
/// `isSupportedImageMimeType`).
pub fn is_supported_image_mime_type(mime_type: &str) -> bool {
    let base = base_mime_type(mime_type);
    SUPPORTED_IMAGE_MIME_TYPES.contains(&base.as_str())
}

/// Pick the best available image mime type (upstream
/// `selectPreferredImageMimeType`): upstream's preference order first, then
/// any other `image/*` entry, preserving the original spelling.
pub fn select_preferred_image_mime_type(mime_types: &[String]) -> Option<String> {
    let normalized: Vec<(String, String)> = mime_types
        .iter()
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty())
        .map(|raw| (raw.to_string(), base_mime_type(raw)))
        .collect();

    for preferred in SUPPORTED_IMAGE_MIME_TYPES {
        if let Some((raw, _)) = normalized
            .iter()
            .find(|(_, base)| base == &preferred.to_string())
        {
            return Some(raw.clone());
        }
    }

    normalized
        .iter()
        .find(|(_, base)| base.starts_with("image/"))
        .map(|(raw, _)| raw.clone())
}

/// Whether the environment looks like WSL (upstream `isWSL`): the WSL env
/// markers, or `/proc/version` mentioning Microsoft/WSL.
pub fn is_wsl_with(env: &ClipboardEnv, read_proc_version: impl FnOnce() -> Option<String>) -> bool {
    if env.has("WSL_DISTRO_NAME") || env.has("WSLENV") {
        return true;
    }
    let Some(release) = read_proc_version() else {
        return false;
    };
    let lower = release.to_lowercase();
    lower.contains("microsoft") || lower.contains("wsl")
}

/// Runs the clipboard-image commands (upstream `spawnSync` + `fs`), injectable
/// so the platform tree can be tested without a real clipboard.
pub trait ClipboardImageRunner {
    /// Run `program` with `args`; return stdout when it exits successfully.
    fn run(&mut self, program: &str, args: &[&str]) -> Option<Vec<u8>>;

    /// Read a file (upstream `readFileSync`), `None` on error.
    fn read_file(&mut self, path: &Path) -> Option<Vec<u8>>;

    /// Delete a file, ignoring errors (upstream `unlinkSync` in `finally`).
    fn remove_file(&mut self, path: &Path);

    /// A fresh temporary file path for the WSL PowerShell handoff.
    fn temp_file(&mut self, prefix: &str) -> PathBuf;
}

/// The process-backed runner used outside tests.
#[derive(Debug, Default)]
pub struct ProcessClipboardImageRunner;

impl ClipboardImageRunner for ProcessClipboardImageRunner {
    fn run(&mut self, program: &str, args: &[&str]) -> Option<Vec<u8>> {
        use std::process::Command;
        let output = Command::new(program).args(args).output().ok()?;
        if !output.status.success() {
            return None;
        }
        Some(output.stdout)
    }

    fn read_file(&mut self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    fn remove_file(&mut self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    fn temp_file(&mut self, prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}.png", pillar_ai::uuid::uuidv7()))
    }
}

fn non_empty_bytes(bytes: Option<Vec<u8>>) -> Option<Vec<u8>> {
    match bytes {
        Some(bytes) if !bytes.is_empty() => Some(bytes),
        _ => None,
    }
}

fn split_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// `wl-paste` path (upstream `readClipboardImageViaWlPaste`).
fn read_via_wl_paste(runner: &mut dyn ClipboardImageRunner) -> Option<ClipboardImage> {
    let list = non_empty_bytes(runner.run("wl-paste", &["--list-types"]))?;
    let types = split_lines(&list);
    let selected = select_preferred_image_mime_type(&types)?;
    let data = non_empty_bytes(runner.run("wl-paste", &["--type", &selected, "--no-newline"]))?;
    Some(ClipboardImage {
        bytes: data,
        mime_type: base_mime_type(&selected),
    })
}

/// PowerShell path for WSL (upstream `readClipboardImageViaPowerShell`):
/// Windows screenshots never reach the Linux clipboard, so save the Windows
/// clipboard image to a temp file from PowerShell and read it back.
fn read_via_powershell(runner: &mut dyn ClipboardImageRunner) -> Option<ClipboardImage> {
    let tmp_file = runner.temp_file("pi-wsl-clip");
    let result = (|| {
        let win_path = String::from_utf8_lossy(&non_empty_bytes(
            runner.run("wslpath", &["-w", &tmp_file.to_string_lossy()]),
        )?)
        .trim()
        .to_string();
        if win_path.is_empty() {
            return None;
        }

        let ps_quoted_win_path = win_path.replace('\'', "''");
        let ps_script = [
            "Add-Type -AssemblyName System.Windows.Forms",
            "Add-Type -AssemblyName System.Drawing",
            &format!("$path = '{ps_quoted_win_path}'"),
            "$img = [System.Windows.Forms.Clipboard]::GetImage()",
            "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else { Write-Output 'empty' }",
        ]
        .join("; ");

        let output =
            non_empty_bytes(runner.run("powershell.exe", &["-NoProfile", "-Command", &ps_script]))?;
        if String::from_utf8_lossy(&output).trim() != "ok" {
            return None;
        }

        let bytes = non_empty_bytes(runner.read_file(&tmp_file))?;
        Some(ClipboardImage {
            bytes,
            mime_type: "image/png".to_string(),
        })
    })();
    runner.remove_file(&tmp_file);
    result
}

/// `xclip` path (upstream `readClipboardImageViaXclip`).
fn read_via_xclip(runner: &mut dyn ClipboardImageRunner) -> Option<ClipboardImage> {
    let candidate_types = runner
        .run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .map(|stdout| split_lines(&stdout))
        .unwrap_or_default();

    let preferred = select_preferred_image_mime_type(&candidate_types);
    let mut try_types: Vec<String> = Vec::new();
    if let Some(preferred) = preferred {
        try_types.push(preferred);
    }
    try_types.extend(
        SUPPORTED_IMAGE_MIME_TYPES
            .iter()
            .map(|mime| mime.to_string()),
    );

    for mime_type in try_types {
        let data = non_empty_bytes(runner.run(
            "xclip",
            &["-selection", "clipboard", "-t", &mime_type, "-o"],
        ));
        if let Some(data) = data {
            return Some(ClipboardImage {
                bytes: data,
                mime_type: base_mime_type(&mime_type),
            });
        }
    }
    None
}

/// Read an image from the clipboard with explicit platform/env/runner inputs.
pub fn read_clipboard_image_with(
    platform: ClipboardPlatform,
    env: &ClipboardEnv,
    runner: &mut dyn ClipboardImageRunner,
) -> Option<ClipboardImage> {
    if env.has("TERMUX_VERSION") {
        return None;
    }

    let mut image: Option<ClipboardImage> = None;

    if platform == ClipboardPlatform::Other {
        let wsl = is_wsl_with(env, || {
            runner
                .read_file(Path::new("/proc/version"))
                .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        });
        let wayland = is_wayland_session(env);

        if wayland || wsl {
            image = read_via_wl_paste(runner).or_else(|| read_via_xclip(runner));
        }
        if image.is_none() && wsl {
            image = read_via_powershell(runner);
        }
        if image.is_none() && !wayland {
            // The native addon is not ported (see the module note).
            image = read_via_xclip(runner);
        }
    }
    // Non-Linux platforms read through the native addon upstream; without it
    // there is no source, so `image` stays `None`.

    let image = image?;
    if !is_supported_image_mime_type(&image.mime_type) {
        // Photon conversion is not ported: unsupported formats yield nothing.
        return None;
    }
    Some(image)
}

/// Read an image from the clipboard using the running process's platform and
/// environment.
pub fn read_clipboard_image() -> Option<ClipboardImage> {
    let env = ClipboardEnv::from_process();
    read_clipboard_image_with(
        ClipboardPlatform::native(),
        &env,
        &mut ProcessClipboardImageRunner,
    )
}
