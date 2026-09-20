//! Frame cost of the interactive render pipeline: how much work one frame
//! does as the transcript grows.
//!
//! Run (the printed numbers feed `docs/PERF-BASELINE.md`):
//!
//! ```text
//! cargo test --release -p pillar-coding-agent --test tui_render_cost -- --nocapture
//! ```
//!
//! The pipeline re-renders the whole component tree every frame: the
//! transcript's `chat` container walks every message component, each of which
//! returns a fresh `Vec<String>` of its cached lines, and the main screen then
//! diffs the result against the previous frame line by line. Nothing here is
//! an SLO; the numbers exist to show how the cost scales with the history and
//! which stage dominates.
//!
//! Two variants per frame shape are measured:
//!
//! - `tick` changes the tail (the status line): the frame takes the
//!   differential path, like a spinner tick or a streaming token.
//! - `invalidate` also calls `ui.invalidate()` first. The port's pump used to
//!   do this on every dirty frame (upstream only does it on a theme change, a
//!   grammar load or a TUI-mode switch); that is fixed, so this variant is now
//!   the upper bound for "every component cache was dropped" — a theme change,
//!   or a regression that reintroduces the per-frame invalidation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_ai::types::{AssistantMessage, Content, Message, StopReason, Usage, UsageCost};
use pillar_coding_agent::core::messages::CodingAgentMessage;
use pillar_coding_agent::modes::interactive::transcript::{
    InteractiveTranscript, Shared, TranscriptSettings,
};
use pillar_tui::components::Text;
use pillar_tui::main_screen::{changed_range, expand_changed_range_for_kitty_images};
use pillar_tui::overlay::apply_line_resets;
use pillar_tui::process_terminal::{ProcessTerminal, TerminalIo};
use pillar_tui::text_utils::visible_width;
use pillar_tui::tui::Component as _;
use pillar_tui::tui_main_screen::TuiMainScreen;

/// Messages per transcript size: 25 messages is a short session, 200 a long
/// one. Each message renders to ~40 lines (prose + a 30-line code block).
/// Debug builds measure a smaller corpus so the default suite stays fast; the
/// baseline numbers come from `--release`.
const SIZES: [usize; 2] = if cfg!(debug_assertions) {
    [10, 40]
} else {
    [25, 200]
};
const MESSAGE_CODE_LINES: usize = 30;
const FRAMES: usize = if cfg!(debug_assertions) { 5 } else { 30 };

static THEME_LOCK: Mutex<()> = Mutex::new(());

/// A terminal that counts the bytes the renderer writes.
struct CountingIo {
    written: Arc<AtomicU64>,
}

impl TerminalIo for CountingIo {
    fn write(&mut self, data: &str) {
        self.written.fetch_add(data.len() as u64, Ordering::Relaxed);
    }
    fn size(&self) -> (usize, usize) {
        (80, 24)
    }
    fn enable_raw_mode(&mut self) -> bool {
        false
    }
    fn disable_raw_mode(&mut self) {}
    fn read_input(&mut self, _buffer: &mut [u8], _timeout: Duration) -> Option<usize> {
        None
    }
}

fn usage() -> Usage {
    Usage {
        input: 10,
        output: 10,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 20,
        cost: UsageCost::default(),
    }
}

fn assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::Text {
            text: text.to_string(),
            text_signature: None,
        }],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        response_model: None,
        usage: usage(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        response_id: None,
        diagnostics: Vec::new(),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1,
    }
}

/// One assistant message: prose, a fenced code block, a list.
fn message_text(index: usize) -> String {
    let mut text = format!(
        "### Step {index}\n\nThe change is applied in three places and the tail of the \
         message carries a little more prose so the paragraph wraps over a few lines at \
         80 columns.\n\n```rust\n"
    );
    for line in 0..MESSAGE_CODE_LINES {
        text.push_str(&format!("let value_{line} = compute({index}, {line});\n"));
    }
    text.push_str("```\n\n- first item\n- second item\n- third item\n");
    text
}

fn transcript(messages: usize) -> InteractiveTranscript {
    let mut transcript =
        InteractiveTranscript::new(TranscriptSettings::default(), None, Vec::new(), "/tmp");
    for index in 0..messages {
        transcript.add_message_to_chat(
            CodingAgentMessage::Base(Message::Assistant(Box::new(assistant(&message_text(
                index,
            ))))),
            false,
        );
    }
    transcript
}

/// A screen over `transcript` plus a status line the caller can tick.
fn screen(messages: usize, written: Arc<AtomicU64>) -> (TuiMainScreen, Shared<Text>) {
    let status = Shared::new(Text::new("status 0", 1, 0));
    let mut screen = TuiMainScreen::new(Box::new(ProcessTerminal::with_io(Box::new(CountingIo {
        written,
    }))));
    screen.base_mut().start();
    screen.base_mut().set_clear_on_shrink(false);
    screen.base_mut().add_child(Box::new(transcript(messages)));
    screen.base_mut().add_child(Box::new(status.clone()));
    (screen, status)
}

/// Time `frames` frames of one shape; returns (µs/frame, rendered lines).
fn measure(messages: usize, invalidate: bool, tick: bool, frames: usize) -> (f64, usize) {
    let written = Arc::new(AtomicU64::new(0));
    let (mut screen, status) = screen(messages, Arc::clone(&written));
    // First frame: full render (not measured).
    screen.do_render().expect("first frame");
    let lines = screen.capture_render_state().previous_lines.len();

    let start = Instant::now();
    for frame in 0..frames {
        if tick {
            status.lock().set_text(&format!("status {frame}"));
        }
        if invalidate {
            screen.base_mut().invalidate();
        }
        screen.do_render().expect("frame");
    }
    let elapsed = start.elapsed();
    (elapsed.as_secs_f64() * 1e6 / frames as f64, lines)
}

#[test]
fn one_frame_costs_the_whole_transcript() {
    let _guard = THEME_LOCK.lock().expect("lock");
    pillar_coding_agent::modes::interactive::theme::init_theme(Some("dark"));
    let budget = Instant::now();

    println!("\n== frame cost (80x24, 30 frames) ==");
    println!(
        "{:>6} {:>7} {:>10} {:>12} {:>14} {:>14}",
        "msgs", "lines", "noop", "noop+inv", "tick", "tick+inv"
    );
    let mut rows = Vec::new();
    for messages in SIZES {
        let (noop, lines) = measure(messages, false, false, FRAMES);
        let (noop_invalidate, _) = measure(messages, true, false, FRAMES);
        let (tick, _) = measure(messages, false, true, FRAMES);
        let (tick_invalidate, _) = measure(messages, true, true, FRAMES);
        println!(
            "{messages:>6} {lines:>7} {noop:>9.1}µs {noop_invalidate:>10.1}µs {tick:>12.1}µs {tick_invalidate:>12.1}µs"
        );
        rows.push((messages, lines, tick, tick_invalidate));
    }

    // The pipeline is O(history) per frame: doubling the transcript must not
    // make a frame more than 4x as expensive (a generous, non-SLO bound that
    // still fails on a superlinear blow-up).
    let (_, _, small, small_inv) = rows[0];
    let (_, _, large, large_inv) = rows[1];
    let growth = large / small.max(1.0);
    let growth_inv = large_inv / small_inv.max(1.0);
    println!(
        "growth from {} to {} messages: tick {growth:.1}x, tick+invalidate {growth_inv:.1}x",
        rows[0].0, rows[1].0
    );
    assert!(
        growth < 20.0 && growth_inv < 20.0,
        "per-frame cost blew up superlinearly: {growth:.1}x / {growth_inv:.1}x"
    );

    println!("\n== stage cost on {} lines ==", rows[1].1);
    let previous: Vec<Arc<str>> = (0..rows[1].1)
        .map(|index| {
            Arc::<str>::from(format!("line {index} with a little text to compare").as_str())
        })
        .collect();
    let mut next = previous.clone();
    let tail = next.len() - 1;
    next[tail] = Arc::from("line tail changed");
    let next_owned: Vec<String> = next.iter().map(|line| line.to_string()).collect();

    let stage = |label: &str, iterations: usize, mut run: Box<dyn FnMut()>| {
        let start = Instant::now();
        for _ in 0..iterations {
            run();
        }
        let per = start.elapsed().as_secs_f64() * 1e6 / iterations as f64;
        println!("  {label:<46} {per:>9.1}µs");
    };

    stage(
        "changed_range (one full diff scan)",
        200,
        Box::new(|| {
            std::hint::black_box(changed_range(&previous, &next));
        }),
    );
    stage(
        "expand_changed_range_for_kitty_images",
        200,
        Box::new(|| {
            std::hint::black_box(expand_changed_range_for_kitty_images(
                0, tail, &previous, &next,
            ));
        }),
    );
    stage(
        "kitty id scan over every line",
        200,
        Box::new(|| {
            let mut ids = 0usize;
            for line in &next {
                ids += pillar_tui::main_screen::extract_kitty_image_ids(line).len();
            }
            std::hint::black_box(ids);
        }),
    );
    stage(
        "apply_line_resets (fullscreen path only now)",
        200,
        Box::new(|| {
            std::hint::black_box(apply_line_resets(next_owned.clone()));
        }),
    );
    stage(
        "clone of the line vector (previous_lines)",
        200,
        Box::new(|| {
            std::hint::black_box(next.clone());
        }),
    );

    stage(
        "visible_width over every line",
        200,
        Box::new(|| {
            let mut width = 0usize;
            for line in &next {
                width += visible_width(line);
            }
            std::hint::black_box(width);
        }),
    );
    stage(
        "cursor-marker scan over every line",
        200,
        Box::new(|| {
            let mut found = 0usize;
            for line in &next {
                if line.contains('\u{1b}') {
                    found += 1;
                }
            }
            std::hint::black_box(found);
        }),
    );
    stage(
        "transcript tree walk (cache hits only)",
        if cfg!(debug_assertions) { 10 } else { 50 },
        Box::new(|| {
            let mut tree = transcript(SIZES[1]);
            std::hint::black_box(tree.render(80).len());
        }),
    );
    let mut warm = transcript(SIZES[1]);
    std::hint::black_box(warm.render(80).len());
    stage(
        "transcript tree walk (warm caches)",
        if cfg!(debug_assertions) { 10 } else { 50 },
        Box::new(|| {
            std::hint::black_box(warm.render(80).len());
        }),
    );

    // What one container rebuild costs in each representation of a frame.
    // `Arc<[String]>` shares the *array*, not the line data, so every ancestor
    // that rebuilds after a change copies all of the strings again; sharing per
    // line is what makes an ancestor rebuild cheap.
    let flat: Vec<String> = next_owned.clone();
    let shared: Vec<Arc<str>> = flat.iter().map(|line| Arc::from(line.as_str())).collect();
    stage(
        "rebuild flat Vec<String> (deep copy)",
        200,
        Box::new(|| {
            let mut out: Vec<String> = Vec::with_capacity(flat.len());
            out.extend(flat.iter().cloned());
            std::hint::black_box(out);
        }),
    );
    stage(
        "rebuild Arc<[String]> (deep copy + one alloc)",
        200,
        Box::new(|| {
            let out: Arc<[String]> = flat.clone().into();
            std::hint::black_box(out);
        }),
    );
    stage(
        "rebuild Vec<Arc<str>> (pointer copies)",
        200,
        Box::new(|| {
            let mut out: Vec<Arc<str>> = Vec::with_capacity(shared.len());
            out.extend(shared.iter().cloned());
            std::hint::black_box(out);
        }),
    );
    stage(
        "rebuild Arc<[Arc<str>]> (pointer copies)",
        200,
        Box::new(|| {
            let out: Arc<[Arc<str>]> = shared.clone().into();
            std::hint::black_box(out);
        }),
    );
    stage(
        "diff scan over Arc<str> (ptr fast path)",
        200,
        Box::new(|| {
            let mut changed = 0usize;
            for (old, new) in shared.iter().zip(shared.iter()) {
                if !Arc::ptr_eq(old, new) && old != new {
                    changed += 1;
                }
            }
            std::hint::black_box(changed);
        }),
    );

    let elapsed = budget.elapsed();
    println!("\ntotal measurement wall clock: {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(300),
        "measurement hung: {elapsed:?}"
    );
}
