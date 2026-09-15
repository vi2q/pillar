//! Integration tests for the `pillar` binary (upstream cli.ts entry behavior).

use std::process::Command;

fn pillar_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pillar")
}

#[test]
fn binary_version_prints_pinned_pi_version() {
    let output = Command::new(pillar_bin())
        .arg("--version")
        .output()
        .expect("run pillar");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn binary_help_prints_usage() {
    let output = Command::new(pillar_bin())
        .arg("--help")
        .output()
        .expect("run pillar");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("pillar - AI coding assistant"));
    assert!(stdout.contains("Usage:"));
}

#[test]
fn binary_reports_unknown_short_option() {
    let output = Command::new(pillar_bin())
        .arg("-z")
        .output()
        .expect("run pillar");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Unknown option: -z"));
}

#[test]
fn binary_rejects_file_args_in_rpc_mode() {
    let output = Command::new(pillar_bin())
        .args(["--mode", "rpc", "@prompt.md"])
        .output()
        .expect("run pillar");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("@file arguments are not supported in RPC mode")
    );
}

#[test]
fn binary_print_mode_without_a_model_fails_cleanly() {
    // Isolate the agent dir so no real credentials/settings are read.
    let dir = std::env::temp_dir().join(format!(
        "pillar-cli-print-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let output = Command::new(pillar_bin())
        .args(["-p", "hi"])
        .env("HOME", &dir)
        .env("PILLAR_CODING_AGENT_DIR", &dir)
        .output()
        .expect("run pillar");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Error:"), "{stderr}");
    // Must fail through the model/auth path, not panic.
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// Write a configured agent dir with a custom provider whose base URL points
/// at a closed port (so the run fails at the request, before any network
/// work).
fn configured_agent_dir(label: &str, with_default_settings: bool) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pillar-cli-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("models.json"),
        r#"{
  "providers": {
    "fakeprov": {
      "name": "Fake Provider",
      "baseUrl": "http://127.0.0.1:9/v1",
      "api": "openai-completions",
      "models": [{ "id": "fake-model", "name": "Fake Model" }]
    }
  }
}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("auth.json"),
        r#"{ "fakeprov": { "type": "api_key", "key": "test-key" } }"#,
    )
    .unwrap();
    if with_default_settings {
        std::fs::write(
            dir.join("settings.json"),
            r#"{ "defaultProvider": "fakeprov", "defaultModel": "fake-model" }"#,
        )
        .unwrap();
    }
    dir
}

/// The default model from settings.json resolves and the custom provider
/// streams: the run must fail at the request, not at model selection. This is
/// the path that was broken while `refresh_availability` was never called and
/// config-only providers got no API implementation.
#[test]
fn binary_uses_the_configured_default_model_and_custom_provider() {
    let dir = configured_agent_dir("configured", true);
    let output = Command::new(pillar_bin())
        .args(["-p", "hi"])
        .env("HOME", &dir)
        .env("PILLAR_CODING_AGENT_DIR", &dir)
        .output()
        .expect("run pillar");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("No models available"), "{stderr}");
    assert!(!stderr.contains("No model selected"), "{stderr}");
    assert!(!stderr.contains("no API implementation"), "{stderr}");
    assert!(stderr.contains("fetch:"), "{stderr}");
}

/// `--model <pattern>` selects a configured model even without a saved
/// default.
#[test]
fn binary_model_flag_selects_a_configured_model() {
    let dir = configured_agent_dir("model-flag", false);
    let output = Command::new(pillar_bin())
        .args(["-p", "hi", "--model", "fake-model"])
        .env("HOME", &dir)
        .env("PILLAR_CODING_AGENT_DIR", &dir)
        .output()
        .expect("run pillar");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("No models available"), "{stderr}");
    assert!(!stderr.contains("No model selected"), "{stderr}");
    assert!(!stderr.contains("no API implementation"), "{stderr}");
    assert!(stderr.contains("fetch:"), "{stderr}");
}
