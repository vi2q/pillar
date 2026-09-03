//! Ports of small pi v0.84.3 core modules not yet covered:
//! - experimental.ts (`PI_EXPERIMENTAL` flag and strict tool sampling)
//! - telemetry.ts (`isInstallTelemetryEnabled`)
//! - radius.ts (`RADIUS_PROVIDER_ID`)
//! - output-guard.ts (stdout takeover state machine; the TUI-process
//!   write plumbing itself is host-side, the port tracks takeover state
//!   and the retry/exit contracts)

/// The experimental features env var name (upstream reads
/// `process.env.PI_EXPERIMENTAL`).
pub const PI_EXPERIMENTAL_ENV: &str = "PI_EXPERIMENTAL";

/// Whether experimental features are enabled (upstream
/// `areExperimentalFeaturesEnabled`): `PI_EXPERIMENTAL === "1"`.
pub fn are_experimental_features_enabled(env_value: Option<&str>) -> bool {
    env_value == Some("1")
}

/// The strict tool sampling hint (upstream `PREFER_STRICT_TOOL_SAMPLING`).
pub const PREFER_STRICT_TOOL_SAMPLING: &str = r#"{"type":"json_schema","strict":"prefer"}"#;

/// The structured-tool-sampling option when experimental features are on
/// (upstream `getExperimentalToolSampling`).
pub fn get_experimental_tool_sampling(env_value: Option<&str>) -> Option<&'static str> {
    if are_experimental_features_enabled(env_value) {
        Some(PREFER_STRICT_TOOL_SAMPLING)
    } else {
        None
    }
}

/// The telemetry env var name (upstream `process.env.PI_TELEMETRY`).
pub const PI_TELEMETRY_ENV: &str = "PI_TELEMETRY";

/// Truthy env flag parsing shared by telemetry and offline checks
/// (upstream `isTruthyEnvFlag`).
pub fn is_truthy_env_flag(value: Option<&str>) -> bool {
    match value {
        Some(value) => {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        }
        None => false,
    }
}

/// Whether install telemetry is enabled (upstream
/// `isInstallTelemetryEnabled`): the env var overrides the setting when
/// present.
pub fn is_install_telemetry_enabled(settings_enabled: bool, telemetry_env: Option<&str>) -> bool {
    match telemetry_env {
        Some(_) => is_truthy_env_flag(telemetry_env),
        None => settings_enabled,
    }
}

/// The Radius gateway provider id (upstream `RADIUS_PROVIDER_ID`).
pub const RADIUS_PROVIDER_ID: &str = "radius";

// ============================================================================
// output-guard.ts
// ============================================================================

/// Retry delay for raw stdout writes that hit backpressure errors
/// (upstream `RAW_STDOUT_RETRY_DELAY_MS`).
pub const RAW_STDOUT_RETRY_DELAY_MS: u64 = 10;

/// Whether a write error is a retriable backpressure error (upstream the
/// ENOBUFS / EAGAIN / EWOULDBLOCK check).
pub fn is_backpressure_error(code: &str) -> bool {
    matches!(code, "ENOBUFS" | "EAGAIN" | "EWOULDBLOCK")
}

/// Stdout takeover state (upstream the module-level
/// `stdoutTakeoverState`): the TUI redirects stdout writes to stderr so
/// raw TUI output owns stdout.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StdoutGuard {
    taken_over: bool,
    /// Bytes written to raw stdout since takeover (host bookkeeping).
    write_queue_len: usize,
}

impl StdoutGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take over stdout (upstream `takeOverStdout`): idempotent.
    pub fn take_over(&mut self) {
        if self.taken_over {
            return;
        }
        self.taken_over = true;
    }

    /// Restore stdout (upstream `restoreStdout`): idempotent, resets the
    /// write queue.
    pub fn restore(&mut self) {
        if !self.taken_over {
            return;
        }
        self.taken_over = false;
        self.write_queue_len = 0;
    }

    /// Whether stdout is currently taken over (upstream
    /// `isStdoutTakenOver`).
    pub fn is_taken_over(&self) -> bool {
        self.taken_over
    }

    /// Queue a raw stdout write; empty writes are dropped (upstream
    /// `writeRawStdout`).
    pub fn write_raw(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.write_queue_len += 1;
    }

    /// Number of queued writes (host-side backpressure bookkeeping).
    pub fn write_queue_len(&self) -> usize {
        self.write_queue_len
    }
}
