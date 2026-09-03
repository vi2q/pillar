//! Parity tests for small pi v0.84.3 core modules: experimental.ts,
//! telemetry.ts, radius.ts, and output-guard.ts.

use pillar_coding_agent::core::extras::{
    PI_EXPERIMENTAL_ENV, PI_TELEMETRY_ENV, PREFER_STRICT_TOOL_SAMPLING, RADIUS_PROVIDER_ID,
    RAW_STDOUT_RETRY_DELAY_MS, StdoutGuard, are_experimental_features_enabled,
    get_experimental_tool_sampling, is_backpressure_error, is_install_telemetry_enabled,
    is_truthy_env_flag,
};

// --- experimental ---------------------------------------------------------------------

#[test]
fn experimental_flag_env_var() {
    assert_eq!(PI_EXPERIMENTAL_ENV, "PI_EXPERIMENTAL");
    assert!(are_experimental_features_enabled(Some("1")));
    assert!(!are_experimental_features_enabled(Some("true")));
    assert!(!are_experimental_features_enabled(Some("0")));
    assert!(!are_experimental_features_enabled(None));
}

#[test]
fn experimental_tool_sampling_only_when_enabled() {
    assert_eq!(
        get_experimental_tool_sampling(Some("1")),
        Some(PREFER_STRICT_TOOL_SAMPLING)
    );
    assert!(get_experimental_tool_sampling(None).is_none());
    assert_eq!(
        PREFER_STRICT_TOOL_SAMPLING,
        r#"{"type":"json_schema","strict":"prefer"}"#
    );
}

// --- telemetry ---------------------------------------------------------------------------

#[test]
fn telemetry_env_overrides_setting() {
    assert_eq!(PI_TELEMETRY_ENV, "PI_TELEMETRY");
    // Env present: env decides (even when the setting says enabled).
    assert!(!is_install_telemetry_enabled(true, Some("0")));
    assert!(!is_install_telemetry_enabled(true, Some("false")));
    assert!(is_install_telemetry_enabled(false, Some("1")));
    assert!(is_install_telemetry_enabled(false, Some("yes")));
    assert!(is_install_telemetry_enabled(false, Some("TRUE")));
    // Env absent: the setting decides.
    assert!(is_install_telemetry_enabled(true, None));
    assert!(!is_install_telemetry_enabled(false, None));
}

#[test]
fn truthy_env_flag_parsing() {
    assert!(is_truthy_env_flag(Some("1")));
    assert!(is_truthy_env_flag(Some("true")));
    assert!(is_truthy_env_flag(Some("YES")));
    assert!(!is_truthy_env_flag(Some("")));
    assert!(!is_truthy_env_flag(Some("0")));
    assert!(!is_truthy_env_flag(Some("no")));
    assert!(!is_truthy_env_flag(None));
}

// --- radius --------------------------------------------------------------------------------

#[test]
fn radius_provider_id() {
    assert_eq!(RADIUS_PROVIDER_ID, "radius");
}

// --- output guard ----------------------------------------------------------------------------

#[test]
fn stdout_takeover_idempotent_and_restorable() {
    let mut guard = StdoutGuard::new();
    assert!(!guard.is_taken_over());
    guard.take_over();
    assert!(guard.is_taken_over());
    // Second takeover is a no-op.
    guard.take_over();
    assert!(guard.is_taken_over());
    guard.restore();
    assert!(!guard.is_taken_over());
    // Restore without takeover is a no-op.
    guard.restore();
    assert!(!guard.is_taken_over());
}

#[test]
fn stdout_guard_empty_writes_dropped() {
    let mut guard = StdoutGuard::new();
    guard.take_over();
    guard.write_raw("");
    assert_eq!(guard.write_queue_len(), 0);
    guard.write_raw("chunk");
    assert_eq!(guard.write_queue_len(), 1);
    guard.restore();
    assert_eq!(guard.write_queue_len(), 0);
}

#[test]
fn backpressure_error_codes_retriable() {
    assert!(is_backpressure_error("ENOBUFS"));
    assert!(is_backpressure_error("EAGAIN"));
    assert!(is_backpressure_error("EWOULDBLOCK"));
    assert!(!is_backpressure_error("EPIPE"));
    assert!(!is_backpressure_error("EBADF"));
    assert_eq!(RAW_STDOUT_RETRY_DELAY_MS, 10);
}
