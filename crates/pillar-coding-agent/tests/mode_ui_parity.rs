//! Parity tests for the interactive mode's status/notice UI (pi v0.84.3
//! `interactive-mode.ts`): the status indicator container, the chat notices
//! and the pending-messages display.

use std::sync::Mutex;

use pillar_coding_agent::core::extensions_types::WorkingIndicatorOptions;
use pillar_coding_agent::modes::interactive::components::status_indicator::StatusIndicatorKind;
use pillar_coding_agent::modes::interactive::mode_ui::{PendingMessagesUi, QueueMode, StatusUi};
use pillar_coding_agent::modes::interactive::theme;
use pillar_coding_agent::modes::interactive::transcript::{
    InteractiveTranscript, TranscriptSettings,
};
use pillar_tui::tui::{Component as _, TuiMode};

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn install_dark() {
    theme::init_theme(Some("dark"));
}

fn strip_ansi(text: &str) -> String {
    pillar_tui::text_utils::strip_terminal_sequences(text)
}

fn plain(container: &mut pillar_tui::tui::Container, width: usize) -> String {
    container
        .render(width)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n")
}

// --- status indicator management ----------------------------------------------------------

#[test]
fn status_ui_replaces_and_clears_indicators() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();

    let mut status_ui = StatusUi::new(TuiMode::Regular, true);
    status_ui.show_working_status();
    assert_eq!(status_ui.active_kind(), Some(StatusIndicatorKind::Working));
    let body = plain(&mut status_ui.status, 40);
    assert!(body.contains("Working..."), "{body:?}");

    // A working message override rewrites the active indicator.
    status_ui.set_working_message(Some("Compiling…".to_string()));
    let body = plain(&mut status_ui.status, 40);
    assert!(body.contains("Compiling…"), "{body:?}");
    assert!(!body.contains("Working..."), "{body:?}");

    // clearStatusIndicator(kind) only clears matching indicators.
    status_ui.clear_status_indicator(Some(StatusIndicatorKind::Compaction));
    assert_eq!(status_ui.active_kind(), Some(StatusIndicatorKind::Working));

    // A regular TUI with clear-on-shrink keeps an idle status filler.
    status_ui.clear_status_indicator(None);
    assert!(status_ui.active_kind().is_none());
    // The idle status renders two blank full-width lines.
    let body = plain(&mut status_ui.status, 40);
    assert_eq!(
        body,
        format!("{}\n{}", " ".repeat(40), " ".repeat(40)),
        "{body:?}"
    );
}

#[test]
fn status_ui_idle_filler_follows_clear_on_shrink_and_mode() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();

    // Without clear-on-shrink the status container empties.
    let mut status_ui = StatusUi::new(TuiMode::Regular, false);
    status_ui.show_working_status();
    status_ui.clear_status_indicator(None);
    assert!(status_ui.status.render(40).is_empty());

    // Alt-screen TUIs never keep the idle filler.
    let mut status_ui = StatusUi::new(TuiMode::Fullscreen, true);
    status_ui.show_working_status();
    status_ui.clear_status_indicator(None);
    assert!(status_ui.status.render(40).is_empty());
}

#[test]
fn status_ui_working_visibility_and_indicator_options() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();

    let mut status_ui = StatusUi::new(TuiMode::Regular, false);
    // Not streaming: nothing shows.
    status_ui.set_working_visible(true, false);
    assert!(status_ui.active_kind().is_none());

    // Streaming shows the working indicator.
    status_ui.set_working_visible(true, true);
    assert_eq!(status_ui.active_kind(), Some(StatusIndicatorKind::Working));

    // A compaction indicator is active: hiding the working indicator must
    // NOT clear it (upstream `clearStatusIndicator("working")` guards on
    // the kind).
    status_ui.show_status_indicator(
        pillar_coding_agent::modes::interactive::components::status_indicator::compaction_status_indicator(
            pillar_coding_agent::modes::interactive::components::status_indicator::CompactionStatusReason::Overflow,
        ),
    );
    status_ui.set_working_visible(false, false);
    assert_eq!(
        status_ui.active_kind(),
        Some(StatusIndicatorKind::Compaction)
    );

    // The indicator options flow into the loader.
    status_ui.show_working_status();
    status_ui.set_working_indicator(Some(WorkingIndicatorOptions {
        frames: Some(vec!["a".to_string(), "b".to_string()]),
        interval_ms: Some(50),
    }));
    let body = plain(&mut status_ui.status, 40);
    assert!(body.contains("Working..."), "{body:?}");
}

// --- pending messages ---------------------------------------------------------------------

#[test]
fn pending_messages_render_steering_and_follow_up() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();

    let mut pending = PendingMessagesUi::new();
    pending.update_display(&[], &[]);
    assert!(
        pending.container.render(60).is_empty(),
        "no queued messages"
    );

    pending.queue_compaction_message("queued after compaction".to_string(), QueueMode::Steer);
    pending.update_display(&["steer one".to_string()], &["follow one".to_string()]);
    let body = plain(&mut pending.container, 60);
    assert!(body.contains("Steering: steer one"), "{body:?}");
    assert!(
        body.contains("Steering: queued after compaction"),
        "{body:?}"
    );
    assert!(body.contains("Follow-up: follow one"), "{body:?}");
    assert!(body.contains("to edit all queued messages"), "{body:?}");

    let (steering, follow_up) = pending.all_queued_messages(&[], &[]);
    assert_eq!(steering, vec!["queued after compaction".to_string()]);
    assert!(follow_up.is_empty());

    let taken = pending.take_compaction_queue();
    assert_eq!(
        taken,
        vec![("queued after compaction".to_string(), QueueMode::Steer)]
    );
    let (steering, follow_up) = pending.all_queued_messages(&[], &[]);
    assert!(steering.is_empty() && follow_up.is_empty());
}

// --- chat notices -------------------------------------------------------------------------

fn make_transcript() -> InteractiveTranscript {
    InteractiveTranscript::new(TranscriptSettings::default(), None, Vec::new(), "/tmp")
}

#[test]
fn show_status_rewrites_in_place_while_it_is_last() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();

    let mut transcript = make_transcript();
    transcript.show_status("first status");
    assert_eq!(transcript.chat.len(), 2);
    let body = plain(&mut transcript.chat, 50);
    assert!(body.contains("first status"), "{body:?}");

    // A second status while the first is still last rewrites in place.
    transcript.show_status("second status");
    assert_eq!(transcript.chat.len(), 2, "{:?}", transcript.chat.len());
    let body = plain(&mut transcript.chat, 50);
    assert!(body.contains("second status"), "{body:?}");
    assert!(!body.contains("first status"), "{body:?}");

    // After other content is appended the next status appends a new pair.
    transcript.show_error("boom");
    assert_eq!(transcript.chat.len(), 4, "{:?}", transcript.chat.len());
    transcript.show_status("third status");
    assert_eq!(transcript.chat.len(), 6, "{:?}", transcript.chat.len());
    let body = plain(&mut transcript.chat, 50);
    assert!(body.contains("Error: boom"), "{body:?}");
    assert!(body.contains("third status"), "{body:?}");
    // The rewrite tracking follows the newest pair.
    transcript.show_status("fourth status");
    assert_eq!(transcript.chat.len(), 6, "{:?}", transcript.chat.len());
    let body = plain(&mut transcript.chat, 50);
    assert!(body.contains("fourth status"), "{body:?}");
    assert!(!body.contains("third status"), "{body:?}");
}

#[test]
fn show_warning_uses_the_warning_colour_prefix() {
    let _guard = THEME_LOCK.lock().expect("lock");
    install_dark();

    let mut transcript = make_transcript();
    transcript.show_warning("careful");
    let body = plain(&mut transcript.chat, 50);
    assert!(body.contains("Warning: careful"), "{body:?}");
}
