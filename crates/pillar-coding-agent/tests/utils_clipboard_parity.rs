//! Parity tests for utils/clipboard.ts (pi v0.84.3): the platform/env driven
//! copy decision tree and the OSC 52 fallback. The clipboard commands are
//! injected, so no real clipboard is touched.

use std::collections::BTreeMap;

use pillar_coding_agent::utils::clipboard::{
    CLIPBOARD_COMMAND_TIMEOUT_MS, ClipboardEnv, ClipboardPlatform, ClipboardRunner,
    MAX_OSC52_ENCODED_LENGTH, copy_to_clipboard_with, is_remote_session, is_wayland_session,
    osc52_sequence, read_clipboard_text_with,
};

#[derive(Default)]
struct FakeRunner {
    /// `(program, args)` of every run, in order.
    calls: Vec<(String, Vec<String>)>,
    /// Programs whose run exits successfully.
    succeed: Vec<String>,
    /// Programs present on PATH.
    present: Vec<String>,
    /// Programs whose read returns stdout.
    read_output: BTreeMap<String, String>,
}

impl FakeRunner {
    fn succeeding(programs: &[&str]) -> Self {
        Self {
            succeed: programs.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        }
    }
}

impl ClipboardRunner for FakeRunner {
    fn run(&mut self, program: &str, args: &[&str], _input: &str) -> bool {
        self.calls.push((
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        ));
        self.succeed.iter().any(|candidate| candidate == program)
    }

    fn exists(&mut self, program: &str) -> bool {
        self.present.iter().any(|candidate| candidate == program)
    }

    fn read(&mut self, program: &str, args: &[&str]) -> Option<String> {
        self.calls.push((
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        ));
        self.read_output.get(program).cloned()
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

fn runs(runner: &FakeRunner) -> Vec<String> {
    runner
        .calls
        .iter()
        .map(|(program, _)| program.clone())
        .collect()
}

#[test]
fn macos_uses_pbcopy_without_osc52() {
    let mut runner = FakeRunner::succeeding(&["pbcopy"]);
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "hello",
        ClipboardPlatform::Darwin,
        &env(&[]),
        &mut runner,
        &mut osc52,
    )
    .expect("copy");
    assert_eq!(runs(&runner), vec!["pbcopy"]);
    assert!(osc52.is_empty(), "no OSC 52 when a native tool worked");
}

#[test]
fn windows_uses_clip() {
    let mut runner = FakeRunner::succeeding(&["clip"]);
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "hello",
        ClipboardPlatform::Win32,
        &env(&[]),
        &mut runner,
        &mut osc52,
    )
    .expect("copy");
    assert_eq!(runs(&runner), vec!["clip"]);
    assert!(osc52.is_empty());
}

#[test]
fn linux_prefers_wl_copy_then_x11_tools() {
    // Wayland with wl-copy available and succeeding.
    let mut runner = FakeRunner::succeeding(&["wl-copy"]);
    runner.present.push("wl-copy".to_string());
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "a",
        ClipboardPlatform::Other,
        &env(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]),
        &mut runner,
        &mut osc52,
    )
    .expect("copy");
    assert_eq!(runs(&runner), vec!["wl-copy"]);
    assert!(osc52.is_empty());

    // wl-copy fails -> xclip is tried, then xsel.
    let mut runner = FakeRunner {
        present: vec!["wl-copy".to_string()],
        succeed: vec!["xsel".to_string()],
        ..Default::default()
    };
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "a",
        ClipboardPlatform::Other,
        &env(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]),
        &mut runner,
        &mut osc52,
    )
    .expect("copy");
    assert_eq!(runs(&runner), vec!["wl-copy", "xclip", "xsel"]);
    assert!(osc52.is_empty(), "xsel succeeded");

    // Every tool failing still succeeds through OSC 52.
    let mut runner = FakeRunner {
        present: vec!["wl-copy".to_string()],
        ..Default::default()
    };
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "a",
        ClipboardPlatform::Other,
        &env(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]),
        &mut runner,
        &mut osc52,
    )
    .expect("osc52 fallback");
    assert_eq!(runs(&runner), vec!["wl-copy", "xclip", "xsel"]);
    assert_eq!(
        String::from_utf8(osc52).unwrap(),
        osc52_sequence("a").unwrap()
    );
}

#[test]
fn linux_x11_only_requires_a_display() {
    let mut runner = FakeRunner::succeeding(&["xclip"]);
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "a",
        ClipboardPlatform::Other,
        &env(&[("DISPLAY", ":0")]),
        &mut runner,
        &mut osc52,
    )
    .expect("copy");
    assert_eq!(runs(&runner), vec!["xclip"]);

    // No display at all: nothing to try, OSC 52 takes over.
    let mut runner = FakeRunner::default();
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "hi",
        ClipboardPlatform::Other,
        &env(&[]),
        &mut runner,
        &mut osc52,
    )
    .expect("osc52 fallback");
    assert!(runs(&runner).is_empty());
    assert_eq!(
        String::from_utf8(osc52).unwrap(),
        osc52_sequence("hi").unwrap()
    );
}

#[test]
fn termux_uses_the_termux_clipboard_tool_first() {
    let mut runner = FakeRunner::succeeding(&["termux-clipboard-set"]);
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "a",
        ClipboardPlatform::Other,
        &env(&[("TERMUX_VERSION", "0.118"), ("DISPLAY", ":0")]),
        &mut runner,
        &mut osc52,
    )
    .expect("copy");
    assert_eq!(runs(&runner), vec!["termux-clipboard-set"]);
    assert!(osc52.is_empty());
}

#[test]
fn remote_sessions_always_emit_osc52() {
    let mut runner = FakeRunner::succeeding(&["pbcopy"]);
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "hello",
        ClipboardPlatform::Darwin,
        &env(&[("SSH_CONNECTION", "1.2.3.4 5555 5.6.7.8 22")]),
        &mut runner,
        &mut osc52,
    )
    .expect("copy");
    // The platform tool still runs, but the terminal gets OSC 52 too.
    assert_eq!(runs(&runner), vec!["pbcopy"]);
    assert_eq!(
        String::from_utf8(osc52).unwrap(),
        osc52_sequence("hello").unwrap()
    );
}

#[test]
fn failures_rise_only_when_osc52_is_unavailable() {
    // Everything fails but the text fits in an OSC 52 payload.
    let mut runner = FakeRunner::default();
    let mut osc52 = Vec::new();
    copy_to_clipboard_with(
        "x",
        ClipboardPlatform::Darwin,
        &env(&[]),
        &mut runner,
        &mut osc52,
    )
    .expect("osc52 saves the copy");

    // Oversized payloads cannot use OSC 52 either.
    let huge = "a".repeat(MAX_OSC52_ENCODED_LENGTH);
    let mut runner = FakeRunner::default();
    let mut osc52 = Vec::new();
    let error = copy_to_clipboard_with(
        &huge,
        ClipboardPlatform::Darwin,
        &env(&[]),
        &mut runner,
        &mut osc52,
    )
    .expect_err("nothing can copy this");
    assert_eq!(error, "Failed to copy to clipboard");
    assert!(osc52.is_empty());
}

#[test]
fn osc52_payload_is_base64_osc52_and_capped() {
    assert_eq!(osc52_sequence("hi").unwrap(), "\x1b]52;c;aGk=\x07");
    assert_eq!(osc52_sequence("").unwrap(), "\x1b]52;c;\x07");
    // 100_000 encoded chars allow 75_000 input bytes.
    assert!(osc52_sequence(&"a".repeat(75_000)).is_some());
    assert!(osc52_sequence(&"a".repeat(75_001)).is_none());
}

#[test]
fn session_and_platform_detection_match_upstream() {
    assert!(!is_remote_session(&env(&[])));
    assert!(is_remote_session(&env(&[("SSH_CONNECTION", "x")])));
    assert!(is_remote_session(&env(&[("SSH_CLIENT", "x")])));
    assert!(is_remote_session(&env(&[("MOSH_CONNECTION", "x")])));

    assert!(!is_wayland_session(&env(&[])));
    assert!(is_wayland_session(&env(&[(
        "WAYLAND_DISPLAY",
        "wayland-1"
    )])));
    assert!(is_wayland_session(&env(&[("XDG_SESSION_TYPE", "wayland")])));
    assert!(!is_wayland_session(&env(&[("XDG_SESSION_TYPE", "x11")])));

    assert_eq!(CLIPBOARD_COMMAND_TIMEOUT_MS, 5_000);
}

#[test]
fn reading_prefers_wayland_then_falls_back() {
    let mut runner = FakeRunner::default();
    runner
        .read_output
        .insert("wl-paste".to_string(), "from-wayland\n".to_string());
    let text = read_clipboard_text_with(
        ClipboardPlatform::Other,
        &env(&[("WAYLAND_DISPLAY", "wayland-0")]),
        &mut runner,
    );
    assert_eq!(text.as_deref(), Some("from-wayland"));

    // xclip when there is no Wayland paste tool.
    let mut runner = FakeRunner::default();
    runner
        .read_output
        .insert("xclip".to_string(), "from-x11".to_string());
    let text = read_clipboard_text_with(
        ClipboardPlatform::Other,
        &env(&[("DISPLAY", ":0")]),
        &mut runner,
    );
    assert_eq!(text.as_deref(), Some("from-x11"));

    // An empty clipboard reads as `None`.
    let mut runner = FakeRunner::default();
    runner
        .read_output
        .insert("pbpaste".to_string(), "\n".to_string());
    assert_eq!(
        read_clipboard_text_with(ClipboardPlatform::Darwin, &env(&[]), &mut runner),
        None
    );
}
