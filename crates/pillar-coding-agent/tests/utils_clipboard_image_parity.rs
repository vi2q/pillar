//! Parity tests for utils/clipboard-image.ts (pi v0.84.3): the mime helpers
//! and the platform image-reading tree (injected runner, no real clipboard).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pillar_coding_agent::utils::clipboard::{ClipboardEnv, ClipboardPlatform};
use pillar_coding_agent::utils::clipboard_image::{
    ClipboardImage, ClipboardImageRunner, SUPPORTED_IMAGE_MIME_TYPES, base_mime_type,
    extension_for_image_mime_type, is_supported_image_mime_type, is_wsl_with,
    read_clipboard_image_with, select_preferred_image_mime_type,
};

#[derive(Default)]
struct FakeRunner {
    /// Successful stdout keyed by `"program arg1 arg2"`.
    outputs: BTreeMap<String, Vec<u8>>,
    /// Files readable through `read_file`.
    files: BTreeMap<PathBuf, Vec<u8>>,
    /// Every command run, in order.
    calls: Vec<String>,
    /// Paths passed to `remove_file`.
    removed: Vec<PathBuf>,
    /// Path returned by `temp_file`.
    temp_file: PathBuf,
}

impl FakeRunner {
    fn with_output(mut self, key: &str, bytes: &[u8]) -> Self {
        self.outputs.insert(key.to_string(), bytes.to_vec());
        self
    }
}

impl ClipboardImageRunner for FakeRunner {
    fn run(&mut self, program: &str, args: &[&str]) -> Option<Vec<u8>> {
        let key = if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        };
        self.calls.push(key.clone());
        self.outputs.get(&key).cloned()
    }

    fn read_file(&mut self, path: &Path) -> Option<Vec<u8>> {
        self.files.get(path).cloned()
    }

    fn remove_file(&mut self, path: &Path) {
        self.removed.push(path.to_path_buf());
        self.files.remove(path);
    }

    fn temp_file(&mut self, _prefix: &str) -> PathBuf {
        if self.temp_file.as_os_str().is_empty() {
            PathBuf::from("/tmp/pi-wsl-clip-test.png")
        } else {
            self.temp_file.clone()
        }
    }
}

fn env(pairs: &[(&str, &str)]) -> ClipboardEnv {
    ClipboardEnv::new(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

#[test]
fn mime_helpers_match_upstream() {
    assert_eq!(base_mime_type("image/PNG; charset=binary"), "image/png");
    assert_eq!(extension_for_image_mime_type("image/png"), Some("png"));
    assert_eq!(extension_for_image_mime_type("image/jpeg"), Some("jpg"));
    assert_eq!(extension_for_image_mime_type("image/webp"), Some("webp"));
    assert_eq!(extension_for_image_mime_type("image/gif"), Some("gif"));
    assert_eq!(extension_for_image_mime_type("image/bmp"), None);
    assert_eq!(extension_for_image_mime_type("IMAGE/PNG"), Some("png"));

    assert!(is_supported_image_mime_type("image/png;charset=binary"));
    assert!(!is_supported_image_mime_type("image/bmp"));
    assert_eq!(SUPPORTED_IMAGE_MIME_TYPES.len(), 4);
}

#[test]
fn preferred_mime_type_uses_upstream_precedence() {
    let types = vec![
        "text/plain".to_string(),
        "image/gif".to_string(),
        "image/jpeg".to_string(),
    ];
    // png > jpeg > webp > gif, regardless of list order.
    assert_eq!(
        select_preferred_image_mime_type(&types).as_deref(),
        Some("image/jpeg")
    );

    let other_image = vec!["image/bmp".to_string(), "text/html".to_string()];
    assert_eq!(
        select_preferred_image_mime_type(&other_image).as_deref(),
        Some("image/bmp")
    );

    assert_eq!(
        select_preferred_image_mime_type(&["text/plain".to_string()]),
        None
    );
    assert_eq!(select_preferred_image_mime_type(&[]), None);
    // Upstream keeps the original spelling of the matched entry.
    assert_eq!(
        select_preferred_image_mime_type(&["image/png;charset=binary".to_string()]).as_deref(),
        Some("image/png;charset=binary")
    );
}

#[test]
fn wsl_detection_checks_env_then_proc_version() {
    assert!(is_wsl_with(&env(&[("WSL_DISTRO_NAME", "Ubuntu")]), || None));
    assert!(is_wsl_with(&env(&[("WSLENV", "x")]), || None));
    assert!(is_wsl_with(&env(&[]), || Some(
        "Linux version 5.15.0-microsoft-standard-WSL2".to_string()
    )));
    assert!(is_wsl_with(&env(&[]), || Some("WSL2".to_string())));
    assert!(!is_wsl_with(&env(&[]), || Some(
        "Linux version 6.5.0-15-generic".to_string()
    )));
    assert!(!is_wsl_with(&env(&[]), || None));
}

#[test]
fn termux_never_reads_clipboard_images() {
    let mut runner = FakeRunner::default();
    let image = read_clipboard_image_with(
        ClipboardPlatform::Other,
        &env(&[
            ("TERMUX_VERSION", "0.118"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ]),
        &mut runner,
    );
    assert_eq!(image, None);
    assert!(runner.calls.is_empty());
}

#[test]
fn wayland_reads_the_preferred_type_from_wl_paste() {
    let mut runner = FakeRunner::default()
        .with_output(
            "wl-paste --list-types",
            b"image/gif\nimage/png\ntext/plain\n",
        )
        .with_output("wl-paste --type image/png --no-newline", b"PNGBYTES");
    let image = read_clipboard_image_with(
        ClipboardPlatform::Other,
        &env(&[("WAYLAND_DISPLAY", "wayland-0")]),
        &mut runner,
    )
    .expect("image");
    assert_eq!(
        image,
        ClipboardImage {
            bytes: b"PNGBYTES".to_vec(),
            mime_type: "image/png".to_string(),
        }
    );
    assert_eq!(
        runner.calls,
        vec![
            "wl-paste --list-types".to_string(),
            "wl-paste --type image/png --no-newline".to_string(),
        ]
    );
}

#[test]
fn xclip_is_tried_when_wl_paste_fails() {
    let mut runner = FakeRunner::default()
        .with_output(
            "xclip -selection clipboard -t TARGETS -o",
            b"image/jpeg\nimage/png\n",
        )
        .with_output("xclip -selection clipboard -t image/png -o", b"XCLIPPNG");
    let image = read_clipboard_image_with(
        ClipboardPlatform::Other,
        &env(&[("WAYLAND_DISPLAY", "wayland-0")]),
        &mut runner,
    )
    .expect("image");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(image.bytes, b"XCLIPPNG".to_vec());
    // wl-paste failed first, then the preferred xclip type was used.
    assert_eq!(runner.calls[0], "wl-paste --list-types");
    assert!(
        runner
            .calls
            .contains(&"xclip -selection clipboard -t image/png -o".to_string())
    );
}

#[test]
fn unsupported_formats_yield_no_image_without_photon() {
    // xclip offers only BMP; upstream converts it with Photon, the port has
    // no converter so the read yields nothing.
    let mut runner = FakeRunner::default()
        .with_output("xclip -selection clipboard -t TARGETS -o", b"image/bmp\n");
    let image = read_clipboard_image_with(
        ClipboardPlatform::Other,
        &env(&[("DISPLAY", ":0")]),
        &mut runner,
    );
    assert_eq!(image, None);
}

#[test]
fn wsl_falls_back_to_powershell_and_cleans_up() {
    let temp = PathBuf::from("/tmp/pi-wsl-clip-test.png");
    let mut runner = FakeRunner {
        temp_file: temp.clone(),
        ..Default::default()
    };
    runner.files.insert(
        PathBuf::from("/proc/version"),
        b"Linux version 5.15.0-microsoft-standard-WSL2".to_vec(),
    );
    runner.files.insert(temp.clone(), b"POWERSHELLPNG".to_vec());
    runner.outputs.insert(
        "wslpath -w /tmp/pi-wsl-clip-test.png".to_string(),
        b"C:\\Users\\vie\\AppData\\Local\\Temp\\pi-wsl-clip-test.png\n".to_vec(),
    );
    runner.outputs.insert(
        format!(
            "powershell.exe -NoProfile -Command {}",
            [
                "Add-Type -AssemblyName System.Windows.Forms",
                "Add-Type -AssemblyName System.Drawing",
                "$path = 'C:\\Users\\vie\\AppData\\Local\\Temp\\pi-wsl-clip-test.png'",
                "$img = [System.Windows.Forms.Clipboard]::GetImage()",
                "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else { Write-Output 'empty' }",
            ]
            .join("; ")
        ),
        b"ok\n".to_vec(),
    );

    let image = read_clipboard_image_with(
        ClipboardPlatform::Other,
        &env(&[("WSL_DISTRO_NAME", "Ubuntu")]),
        &mut runner,
    )
    .expect("image");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(image.bytes, b"POWERSHELLPNG".to_vec());
    // The temp file was removed in the `finally` equivalent.
    assert_eq!(runner.removed, vec![temp.clone()]);
}

#[test]
fn non_linux_platforms_have_no_image_source_without_the_native_addon() {
    let mut runner = FakeRunner::default();
    for platform in [ClipboardPlatform::Darwin, ClipboardPlatform::Win32] {
        let image = read_clipboard_image_with(platform, &env(&[]), &mut runner);
        assert_eq!(image, None);
    }
    assert!(runner.calls.is_empty());
}
