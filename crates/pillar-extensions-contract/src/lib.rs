//! The extension contract: the types a Luau extension host and the coding
//! agent share, with no dependency on the coding agent, the TUI, or the CLI.
//!
//! Why a separate crate (docs/DEVELOPMENT-STRATEGY.md §4/§5): the VM
//! (`pillar-extensions`) and the coding agent both need these shapes, and a
//! minimal embedding profile (LMPC) must be able to take the VM without the
//! coding agent's terminal layer. `crates/pillar-cli/tests/dependency_profiles.rs`
//! asserts that this crate's resolved graph contains no presentation, shell,
//! or VM crate.
//!
//! `pillar-coding-agent::core::extensions_types` re-exports what still belongs
//! to it (the session-event payloads and the renderer aliases, which name the
//! session and message models), so the moved types keep their old paths.

use std::sync::Arc;

// ============================================================================
// Markdown transform, render options, the `ctx` facts, and the `ctx.ui` bridge
// ============================================================================

/// Which kind of message a Markdown transformer is rendering (upstream
/// `MarkdownTransformContext["messageType"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownMessageType {
    User,
    Assistant,
    AssistantThinking,
}

impl MarkdownMessageType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::AssistantThinking => "assistant-thinking",
        }
    }
}

/// Context handed to a Markdown transformer (upstream
/// `MarkdownTransformContext`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownTransformContext {
    pub message_type: MarkdownMessageType,
    pub is_streaming: bool,
    pub available_width: usize,
}

/// Rewrites Markdown source before rendering (upstream `MarkdownTransformer`).
/// Returning `None` keeps the current source, mirroring upstream's
/// `typeof transformed === "string"` check.
///
/// divergence: upstream passes the transformer list by reference; the port
/// holds it in an `Arc` so components can rebuild their children.
pub type MarkdownTransformer =
    std::sync::Arc<dyn Fn(&str, &MarkdownTransformContext) -> Option<String> + Send + Sync>;

/// Options for rendering a custom session entry (upstream
/// `EntryRenderOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryRenderOptions {
    pub expanded: bool,
}

/// The lines a custom renderer answers, already styled with the active theme.
///
/// divergence: upstream's renderer returns a live `Component`; the port keeps
/// the extension contract presentation-neutral (the presentation adapter wraps
/// these lines in its own component), so the contract names no TUI type and
/// the VM crate does not depend on the terminal layer.
pub type RenderedLines = Vec<String>;

/// Which run mode an extension context describes (upstream
/// `ExtensionMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionMode {
    Tui,
    Rpc,
    Json,
    Print,
}

impl ExtensionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Rpc => "rpc",
            Self::Json => "json",
            Self::Print => "print",
        }
    }
}

/// The host facts an extension context carries (upstream `ExtensionContext`'s
/// `cwd` / `mode` / `hasUI`). The host updates them when the mode changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionContextFacts {
    pub cwd: String,
    pub mode: ExtensionMode,
    /// Whether dialog-capable UI is available (upstream `hasUI`).
    pub has_ui: bool,
}

impl Default for ExtensionContextFacts {
    fn default() -> Self {
        Self {
            cwd: String::new(),
            mode: ExtensionMode::Print,
            has_ui: false,
        }
    }
}

/// One `ctx.ui.*` request: the operation name (snake_case of the upstream
/// method, e.g. `set_status`) plus its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionUiRequest {
    pub op: String,
    pub args: serde_json::Value,
}

/// Host bridge for `ctx.ui` (upstream `ExtensionUIContext`). The port's UI
/// state lives on the pump thread, so a request is only *queued*: the sender
/// never blocks and never touches mode locks (an extension handler runs with
/// the Luau runtime locked, and the transcript's extension renderers lock the
/// same runtime while holding transcript state).
pub type ExtensionUiFn =
    std::sync::Arc<dyn Fn(ExtensionUiRequest) -> Result<(), String> + Send + Sync>;

/// The `ctx.ui` bridge state: the pump-backed sender the interactive run
/// installs plus the requests that arrived before it existed (extensions
/// commonly touch the UI from `session_start`, which fires before the run
/// loop starts).
#[derive(Default)]
pub struct ExtensionUiState {
    /// The pump-backed sender.
    pub bridge: Option<ExtensionUiFn>,
    /// The pump-backed request/answer bridge (dialogs).
    pub ask: Option<ExtensionUiAskFn>,
    /// The pump-backed `ctx.ui.custom` installer.
    pub custom: Option<ExtensionCustomFn>,
    /// Requests queued before [`ExtensionUiState::bridge`] was installed.
    pub pending: Vec<ExtensionUiRequest>,
    /// Custom components queued before [`ExtensionUiState::custom`] was
    /// installed.
    pub pending_custom: Vec<ExtensionCustomSurface>,
}

impl ExtensionUiState {
    /// Hand a request to the bridge, queueing it until one exists (the caller
    /// holds the slot lock; the bridge itself only sends on a channel, so it
    /// never re-enters the slot).
    pub fn dispatch(&mut self, request: ExtensionUiRequest) -> bool {
        match self.bridge.clone() {
            Some(bridge) => bridge(request).is_ok(),
            None => {
                // A pathological extension cannot queue without bound.
                if self.pending.len() < 256 {
                    self.pending.push(request);
                }
                false
            }
        }
    }

    /// Mount one `ctx.ui.custom` surface, queueing it until the interactive
    /// run installs its installer.
    pub fn install_custom(&mut self, surface: ExtensionCustomSurface) -> bool {
        match self.custom.clone() {
            Some(install) => install(surface).is_ok(),
            None => {
                if self.pending_custom.len() < 256 {
                    self.pending_custom.push(surface);
                }
                false
            }
        }
    }
}

/// One event the interactive mode sends to a `ctx.ui.custom` render loop
/// (upstream the component's own `handleInput` / the TUI's resize).
pub enum ExtensionCustomEvent {
    /// A key sequence for the component.
    Input(String),
    /// The width the pump renders at.
    Resize(usize),
    /// The pump closed the component (session shutdown or a cancelled ask).
    Close,
}

/// The shared surface of one `ctx.ui.custom` component: the extension thread
/// renders into `lines` (upstream the factory's returned `Component` renders
/// on the main thread; the port splits the two, so the pump reads the last
/// painted frame).
pub struct ExtensionCustomSurface {
    /// The last frame the extension painted.
    pub lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Bumped on every paint so the pump can repaint without a channel.
    pub revision: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Set when the render loop gave up; a queued mount is then skipped.
    pub closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The pump's events.
    pub events: ExtensionCustomEvents,
}

impl std::fmt::Debug for ExtensionCustomSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionCustomSurface")
            .field(
                "revision",
                &self.revision.load(std::sync::atomic::Ordering::SeqCst),
            )
            .field(
                "closed",
                &self.closed.load(std::sync::atomic::Ordering::SeqCst),
            )
            .finish()
    }
}

/// The pump's end of a custom surface. The only owner of the event sender is
/// the pump (the queued mount or the mounted component), so dropping the last
/// handle tells the render loop that the component is gone.
#[derive(Clone)]
pub struct ExtensionCustomEvents(std::sync::Arc<ExtensionCustomEventsInner>);

struct ExtensionCustomEventsInner(std::sync::mpsc::Sender<ExtensionCustomEvent>);

impl Drop for ExtensionCustomEventsInner {
    fn drop(&mut self) {
        let _ = self.0.send(ExtensionCustomEvent::Close);
    }
}

impl ExtensionCustomEvents {
    pub fn new(sender: std::sync::mpsc::Sender<ExtensionCustomEvent>) -> Self {
        Self(std::sync::Arc::new(ExtensionCustomEventsInner(sender)))
    }

    /// Send one event; `false` when the render loop is gone.
    pub fn send(&self, event: ExtensionCustomEvent) -> bool {
        self.0.0.send(event).is_ok()
    }
}

/// Host installer for `ctx.ui.custom` (upstream the mode mounting the
/// factory's component in the editor slot).
pub type ExtensionCustomFn =
    std::sync::Arc<dyn Fn(ExtensionCustomSurface) -> Result<(), String> + Send + Sync>;

/// Host bridge for the `ctx.ui` methods that answer a value (upstream
/// `confirm` / `select` / `input` / `editor`): the caller blocks until the
/// user answers.
pub type ExtensionUiAskFn =
    std::sync::Arc<dyn Fn(ExtensionUiRequest) -> Result<serde_json::Value, String> + Send + Sync>;

/// The slot the interactive run fills with its pump-backed UI bridge
/// (upstream the mode owns `ctx.ui` directly).
pub type ExtensionUiSlot = std::sync::Arc<std::sync::Mutex<ExtensionUiState>>;

/// Host callback answering the extension context facts (upstream the live
/// `ExtensionContext` fields).
pub type ExtensionContextFn = std::sync::Arc<dyn Fn() -> ExtensionContextFacts + Send + Sync>;

/// Working-indicator animation options (upstream `WorkingIndicatorOptions`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingIndicatorOptions {
    /// Animation frames; an empty list hides the indicator.
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds.
    pub interval_ms: Option<u64>,
}

/// Options for rendering a custom message (upstream `MessageRenderOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageRenderOptions {
    pub expanded: bool,
    /// Horizontal padding from the `outputPad` setting.
    pub output_pad: usize,
}

// ============================================================================
// exec.ts shapes (the host implements the spawn; core::exec owns it)
// ============================================================================

/// Options for one command execution (upstream `ExecOptions`).
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Cancels the command; an aborted signal kills it before it starts.
    pub signal: Option<pillar_agent::abort::AbortSignal>,
    /// Timeout in milliseconds; `None` or `0` means no timeout.
    pub timeout_ms: Option<u64>,
    /// Working directory override (upstream `options.cwd`, default: the
    /// caller's cwd).
    pub cwd: Option<String>,
}

/// Result of one command execution (upstream `ExecResult`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub killed: bool,
    /// Whether the captured output hit the host's budget: the command ran to the
    /// end (its output was still drained), but only the first
    /// `MAX_CAPTURED_BYTES` of each stream are here. This is the port's own
    /// guard — a command cannot make the host allocate without bound — so it is
    /// additive to upstream's shape.
    #[serde(default)]
    pub truncated: bool,
}

impl ExecResult {
    /// The failure shape upstream produces when the command cannot be spawned.
    pub fn spawn_failure(error: impl std::fmt::Display) -> Self {
        Self {
            stdout: String::new(),
            stderr: error.to_string(),
            code: -1,
            killed: false,
            truncated: false,
        }
    }
}

// ============================================================================
// Renderer contract (the presentation theme stays out of the VM)
// ============================================================================

/// Styles text with a theme foreground colour name (`text`, `dim`, `accent`,
/// `success`, `error`, …). Unknown names pass the text through unstyled.
///
/// The host implements this (usually as a closure over its live theme), so the
/// renderer contract never names a presentation type.
pub trait ThemeStyle: Send + Sync {
    fn fg(&self, name: &str, text: &str) -> String;
}

impl<F> ThemeStyle for F
where
    F: Fn(&str, &str) -> String + Send + Sync,
{
    fn fg(&self, name: &str, text: &str) -> String {
        self(name, text)
    }
}

/// A shared [`ThemeStyle`] (what a renderer closure is handed).
pub type ThemeStyleFn = Arc<dyn ThemeStyle>;

/// The payload handed to a custom-message renderer (upstream the `CustomMessage`
/// object). The coding agent converts its message model into this shape.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomMessagePayload {
    /// The custom type the renderer registered under.
    pub custom_type: String,
    /// The content parts (`[{ "type": "text", "text": ... }]`).
    pub content: serde_json::Value,
    pub display: bool,
    /// Opaque renderer state.
    pub details: serde_json::Value,
    pub timestamp: i64,
}

impl CustomMessagePayload {
    /// The Lua/JSON view of the payload (absent top-level keys are dropped so
    /// `nil` stays distinguishable from an empty table).
    pub fn to_json(&self) -> serde_json::Value {
        without_absent_keys(serde_json::json!({
            "customType": self.custom_type,
            "content": self.content,
            "display": self.display,
            "details": self.details,
            "timestamp": self.timestamp,
        }))
    }
}

/// The payload handed to a custom-entry renderer (upstream the `CustomEntry`).
#[derive(Debug, Clone, PartialEq)]
pub struct CustomEntryPayload {
    pub custom_type: String,
    pub id: String,
    /// Opaque renderer state.
    pub data: serde_json::Value,
}

impl CustomEntryPayload {
    pub fn to_json(&self) -> serde_json::Value {
        without_absent_keys(serde_json::json!({
            "customType": self.custom_type,
            "id": self.id,
            "data": self.data,
        }))
    }
}

/// Drop absent (`null`) top-level keys: the Lua boundary turns JSON `null` into
/// an empty table, so an extension could not tell "no data" from "empty data"
/// (`if entry.data == nil`).
pub fn without_absent_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(entries) => serde_json::Value::Object(
            entries
                .into_iter()
                .filter(|(_, value)| !value.is_null())
                .collect(),
        ),
        other => other,
    }
}

/// Renders a custom session entry (upstream `EntryRenderer`). The renderer
/// answers themed lines, or `None` to skip the entry.
pub type EntryRenderer = Arc<
    dyn Fn(
            &CustomEntryPayload,
            &EntryRenderOptions,
            &dyn ThemeStyle,
        ) -> Option<RenderedLines>
        + Send
        + Sync,
>;

/// Renders a custom message (upstream `MessageRenderer`); `None` falls back to
/// the default message rendering.
pub type MessageRenderer = Arc<
    dyn Fn(
            &CustomMessagePayload,
            &MessageRenderOptions,
            &dyn ThemeStyle,
        ) -> Option<RenderedLines>
        + Send
        + Sync,
>;

// ============================================================================
// `ctx.ui.theme`: the host owns the live theme, the VM sees a snapshot
// ============================================================================

/// The theme snapshot `ctx.ui.theme` exposes (upstream the live `Theme`
/// object). The host builds it from its presentation theme; the VM never
/// names a presentation type.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ThemeSnapshot {
    /// The active theme's name (`None` when it has none).
    pub name: Option<String>,
    /// "dark" | "light" | "" (no theme initialized).
    pub mode: String,
    /// Foreground colours by theme key.
    pub fg_colors: std::collections::BTreeMap<String, String>,
    /// Background colours by theme key.
    pub bg_colors: std::collections::BTreeMap<String, String>,
}

/// One entry of `ctx.ui.theme.getAllThemes()`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ThemeInfo {
    pub name: String,
    pub path: String,
}

/// The host's theme provider for `ctx.ui.theme`: the live snapshot and the
/// installed themes. Without one, the styling helpers answer plain text.
pub trait ThemeProvider: Send + Sync {
    /// `None` while no theme is initialized.
    fn snapshot(&self) -> Option<ThemeSnapshot>;
    /// The installed themes (`getAllThemes`).
    fn list(&self) -> Vec<ThemeInfo>;
}

// ============================================================================
// Diagnostics (skills / resources / extensions share the shape)
// ============================================================================

/// Diagnostic produced while loading resources (upstream `ResourceDiagnostic`
/// subset; source metadata lives with the host).
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceDiagnostic {
    Warning { message: String, path: String },
    Collision { message: String, path: String },
}

mod runner;
pub use runner::*;

// ============================================================================
// The Luau loader contract (the VM implements it, the host drives it)
// ============================================================================

/// The outcome of loading one extension file (upstream the module loader's
/// return): the extension, `Ok(None)` for "not an extension", or the error.
pub type LoadOutcome = Result<Option<crate::HostExtension>, String>;

/// A host-provided module loader (upstream the JS module import + factory
/// invocation).
pub type ModuleLoader<'a> = &'a mut dyn FnMut(&str) -> LoadOutcome;

/// A loader for Luau extension files, injected by the host (the Luau runtime
/// crate implements it). Mirrors [`ModuleLoader`] with an object-safe surface
/// so hosts can swap runtimes.
pub trait LuauExtensionLoader: Send + Sync {
    /// Load one extension file: type-check, run setup, and bridge to a
    /// runner-shaped [`HostExtension`]. `Ok(None)` means "not an extension";
    /// errors are per-path.
    fn load_extension(&self, path: &str) -> LoadOutcome;
}

impl<F> LuauExtensionLoader for F
where
    F: Fn(&str) -> LoadOutcome + Send + Sync,
{
    fn load_extension(&self, path: &str) -> LoadOutcome {
        self(path)
    }
}
