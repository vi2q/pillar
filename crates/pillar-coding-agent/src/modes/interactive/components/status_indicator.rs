//! Port of components/status-indicator.ts: the transcript's spinner rows.

use crate::core::extensions_types::WorkingIndicatorOptions;
use crate::modes::interactive::theme::theme;
use crate::modes::interactive::components::countdown_timer::CountdownTimer;
use crate::modes::interactive::components::keybinding_hints::key_text;
use pillar_tui::loaders::{Loader, LoaderIndicatorOptions};
use pillar_tui::tui::Component;

/// Which spinner row this is (upstream `StatusIndicatorKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusIndicatorKind {
    Working,
    Retry,
    Compaction,
    BranchSummary,
}

/// A [`Loader`] tagged with its kind (upstream `StatusIndicator`).
pub struct StatusIndicator {
    kind: StatusIndicatorKind,
    loader: Loader,
}

impl StatusIndicator {
    pub fn new(
        kind: StatusIndicatorKind,
        spinner_color: Box<dyn Fn(&str) -> String + Send>,
        message_color: Box<dyn Fn(&str) -> String + Send>,
        message: &str,
        indicator: Option<WorkingIndicatorOptions>,
    ) -> Self {
        let loader = Loader::new(
            spinner_color,
            message_color,
            message,
            indicator.map(loader_indicator_options),
        );
        Self { kind, loader }
    }

    pub fn kind(&self) -> StatusIndicatorKind {
        self.kind
    }

    pub fn loader(&self) -> &Loader {
        &self.loader
    }

    pub fn loader_mut(&mut self) -> &mut Loader {
        &mut self.loader
    }

    /// Stop the spinner (upstream `dispose`).
    pub fn dispose(&mut self) {
        self.loader.stop();
    }

    /// Re-render the message (upstream the inherited `setMessage`).
    pub fn set_message(&mut self, message: &str) {
        self.loader.set_message(message);
    }

    /// Advance the spinner (host-driven; upstream's interval).
    pub fn tick(&mut self) -> bool {
        self.loader.tick()
    }
}

impl Component for StatusIndicator {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.loader.render(width)
    }
}

/// Map the extension-facing indicator options onto the TUI loader's (upstream
/// passes the object straight through).
pub fn loader_indicator_options(options: WorkingIndicatorOptions) -> LoaderIndicatorOptions {
    LoaderIndicatorOptions {
        frames: options.frames,
        interval_ms: options.interval_ms,
    }
}

/// The generic working spinner (upstream `WorkingStatusIndicator`).
pub fn working_status_indicator(
    message: &str,
    indicator: Option<WorkingIndicatorOptions>,
) -> StatusIndicator {
    StatusIndicator::new(
        StatusIndicatorKind::Working,
        Box::new(|spinner| theme().fg("accent", spinner)),
        Box::new(|text| theme().fg("muted", text)),
        message,
        indicator,
    )
}

fn retry_message(attempt: usize, max_attempts: usize, seconds: i64) -> String {
    format!(
        "Retrying ({attempt}/{max_attempts}) in {seconds}s... ({} to cancel)",
        key_text("app.interrupt")
    )
}

/// The auto-retry countdown spinner (upstream `RetryStatusIndicator`).
///
/// divergence: upstream's countdown callback closes over the indicator and
/// rewrites its message each second; the port stores the countdown next to the
/// indicator and rewrites the message from `tick` (Rust cannot re-enter the
/// indicator from its own callback).
pub struct RetryStatusIndicator {
    indicator: StatusIndicator,
    countdown: Option<CountdownTimer>,
    attempt: usize,
    max_attempts: usize,
    displayed_seconds: i64,
}

impl RetryStatusIndicator {
    pub fn new(attempt: usize, max_attempts: usize, delay_ms: u64) -> Self {
        let seconds = delay_ms.div_ceil(1000) as i64;
        let message = retry_message(attempt, max_attempts, seconds);
        let indicator = StatusIndicator::new(
            StatusIndicatorKind::Retry,
            Box::new(|spinner| theme().fg("warning", spinner)),
            Box::new(|text| theme().fg("muted", text)),
            &message,
            None,
        );
        let countdown = CountdownTimer::new(
            delay_ms,
            Box::new(|_seconds| {}),
            Box::new(|| {}),
        );
        Self {
            indicator,
            countdown: Some(countdown),
            attempt,
            max_attempts,
            displayed_seconds: seconds,
        }
    }

    pub fn indicator(&self) -> &StatusIndicator {
        &self.indicator
    }

    pub fn indicator_mut(&mut self) -> &mut StatusIndicator {
        &mut self.indicator
    }

    /// Advance the spinner and the countdown, refreshing the message
    /// (host-driven). Returns whether a render is needed.
    pub fn tick(&mut self) -> bool {
        let spinner = self.indicator.tick();
        let countdown = self
            .countdown
            .as_mut()
            .map(|countdown| countdown.tick())
            .unwrap_or(false);
        if let Some(seconds) = self.seconds_remaining() {
            if seconds != self.displayed_seconds {
                self.displayed_seconds = seconds;
                self.indicator.set_message(&retry_message(
                    self.attempt,
                    self.max_attempts,
                    seconds,
                ));
            }
        }
        spinner || countdown
    }

    pub fn seconds_remaining(&self) -> Option<i64> {
        self.countdown
            .as_ref()
            .map(|countdown| countdown.remaining_seconds())
    }

    pub fn dispose(&mut self) {
        if let Some(countdown) = self.countdown.as_mut() {
            countdown.dispose();
        }
        self.countdown = None;
        self.indicator.dispose();
    }
}

impl Component for RetryStatusIndicator {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.indicator.render(width)
    }
}

/// Why a compaction spinner is shown (upstream `CompactionStatusReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStatusReason {
    Manual,
    Threshold,
    Overflow,
}

/// The compaction spinner (upstream `CompactionStatusIndicator`).
pub fn compaction_status_indicator(reason: CompactionStatusReason) -> StatusIndicator {
    let cancel_hint = format!("({} to cancel)", key_text("app.interrupt"));
    let label = match reason {
        CompactionStatusReason::Manual => format!("Compacting context... {cancel_hint}"),
        CompactionStatusReason::Overflow => {
            format!("Context overflow detected, Auto-compacting... {cancel_hint}")
        }
        CompactionStatusReason::Threshold => format!("Auto-compacting... {cancel_hint}"),
    };
    StatusIndicator::new(
        StatusIndicatorKind::Compaction,
        Box::new(|spinner| theme().fg("accent", spinner)),
        Box::new(|text| theme().fg("muted", text)),
        &label,
        None,
    )
}

/// The branch-summary spinner (upstream `BranchSummaryStatusIndicator`).
pub fn branch_summary_status_indicator() -> StatusIndicator {
    StatusIndicator::new(
        StatusIndicatorKind::BranchSummary,
        Box::new(|spinner| theme().fg("accent", spinner)),
        Box::new(|text| theme().fg("muted", text)),
        &format!(
            "Summarizing branch... ({} to cancel)",
            key_text("app.interrupt")
        ),
        None,
    )
}

/// Two blank rows shown when nothing is running (upstream `IdleStatus`).
pub struct IdleStatus;

impl Component for IdleStatus {
    fn render(&mut self, width: usize) -> Vec<String> {
        let empty_line = " ".repeat(width);
        vec![empty_line.clone(), empty_line]
    }
}
