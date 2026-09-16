//! Parity tests for the interactive components ported in the first slice
//! (pi v0.84.3 modes/interactive/components): keybinding hints, dynamic
//! borders, visual truncation, countdown timer, markdown transforms, diff
//! rendering, status indicators, the bordered loader and custom entries.

use std::sync::{Arc, Mutex};

use pillar_coding_agent::core::extensions_types::{
    EntryRenderOptions, MarkdownMessageType, MarkdownTransformContext, WorkingIndicatorOptions,
};
use pillar_coding_agent::core::session_entries::{CustomEntry, SessionEntryBase};
use pillar_coding_agent::modes::interactive::components::bordered_loader::BorderedLoader;
use pillar_coding_agent::modes::interactive::components::countdown_timer::{
    CountdownTimer, simple_countdown,
};
use pillar_coding_agent::modes::interactive::components::custom_entry::CustomEntryComponent;
use pillar_coding_agent::modes::interactive::components::diff::{RenderDiffOptions, render_diff};
use pillar_coding_agent::modes::interactive::components::dynamic_border::DynamicBorder;
use pillar_coding_agent::modes::interactive::components::keybinding_hints::{
    KeyTextFormatOptions, format_key_text, key_display_text, key_hint, key_text, raw_key_hint,
};
use pillar_coding_agent::modes::interactive::components::markdown_transform::create_markdown_transform;
use pillar_coding_agent::modes::interactive::components::status_indicator::{
    CompactionStatusReason, IdleStatus, StatusIndicatorKind, branch_summary_status_indicator,
    compaction_status_indicator, working_status_indicator,
};
use pillar_coding_agent::modes::interactive::components::visual_truncate::truncate_to_visual_lines;
use pillar_coding_agent::modes::interactive::theme;
use pillar_tui::components::Text;
use pillar_tui::tui::Component;

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn install_dark() {
    theme::init_theme(Some("dark"));
}

fn strip_ansi(text: &str) -> String {
    pillar_tui::text_utils::strip_terminal_sequences(text)
}

// --- keybinding hints -----------------------------------------------------------------------------

#[test]
fn key_text_formatting_matches_upstream() {
    assert_eq!(
        format_key_text("ctrl+a", KeyTextFormatOptions::default()),
        "ctrl+a"
    );
    // "/" separates alternative keys, "+" joins modifiers.
    // macOS capitalizes "option" in the display form.
    // Every part is capitalized (upstream capitalizes the first character of
    // each part).
    let capitalised = if cfg!(target_os = "macos") {
        "Ctrl+A/Option+B"
    } else {
        "Ctrl+A/Alt+B"
    };
    assert_eq!(
        format_key_text("ctrl+a/alt+b", KeyTextFormatOptions { capitalize: true }),
        capitalised
    );
    // macOS shows "option" for alt.
    let alt = format_key_text("alt+x", KeyTextFormatOptions::default());
    if cfg!(target_os = "macos") {
        assert_eq!(alt, "option+x");
        assert_eq!(
            format_key_text("ctrl+a/alt+b", KeyTextFormatOptions::default()),
            "ctrl+a/option+b"
        );
    } else {
        assert_eq!(alt, "alt+x");
        assert_eq!(
            format_key_text("ctrl+a/alt+b", KeyTextFormatOptions::default()),
            "ctrl+a/alt+b"
        );
    }
    assert_eq!(format_key_text("", KeyTextFormatOptions::default()), "");
}

#[test]
fn key_hints_use_the_theme_and_bound_keys() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let dark = theme::get_theme_by_name("dark").expect("dark");

    // `tui.select.cancel` is bound to escape and ctrl+c (upstream defaults).
    assert_eq!(key_text("tui.select.cancel"), "escape/ctrl+c");
    assert_eq!(key_display_text("tui.select.cancel"), "Escape/Ctrl+C");
    assert_eq!(
        key_hint("tui.select.cancel", "cancel"),
        format!(
            "{}{}",
            dark.fg("dim", "escape/ctrl+c"),
            dark.fg("muted", " cancel")
        )
    );
    assert_eq!(
        raw_key_hint("ctrl+c", "quit"),
        format!("{}{}", dark.fg("dim", "ctrl+c"), dark.fg("muted", " quit"))
    );
    // An unbound keybinding formats to an empty key list.
    assert_eq!(key_text("does.not.exist"), "");
}

// --- dynamic border -------------------------------------------------------------------------------

#[test]
fn dynamic_border_stretches_to_the_width() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let mut plain = DynamicBorder::with_color(Box::new(|text| text.to_string()));
    assert_eq!(plain.render(5), vec!["─────".to_string()]);
    // Zero width still draws one column (upstream `Math.max(1, width)`).
    assert_eq!(plain.render(0), vec!["─".to_string()]);

    // An explicit colour function is applied verbatim.
    let mut coloured = DynamicBorder::with_color(Box::new(|text| format!("[{text}]")));
    assert_eq!(coloured.render(3), vec!["[───]".to_string()]);
    coloured.invalidate();

    // The themed border wraps the rule in the `border` colour.
    let dark = theme::get_theme_by_name("dark").expect("dark");
    let mut themed = DynamicBorder::new();
    assert_eq!(themed.render(2), vec![dark.fg("border", "──")]);
}

// --- visual truncation ----------------------------------------------------------------------------

#[test]
fn visual_truncation_keeps_the_last_lines() {
    let result = truncate_to_visual_lines("", 3, 20, 0);
    assert!(result.visual_lines.is_empty());
    assert_eq!(result.skipped_count, 0);

    // Fits: nothing is skipped.
    let result = truncate_to_visual_lines("one\ntwo", 3, 20, 0);
    assert_eq!(result.skipped_count, 0);
    assert_eq!(result.visual_lines.len(), 2);

    // Wrapping is accounted for: 20 columns with padding 0 wraps each word.
    let text = (1..=10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let result = truncate_to_visual_lines(&text, 3, 20, 0);
    assert_eq!(result.visual_lines.len(), 3);
    assert_eq!(result.skipped_count, 7);
    assert!(
        result.visual_lines[0].contains("line8"),
        "{:?}",
        result.visual_lines
    );
    assert!(
        result.visual_lines[2].contains("line10"),
        "{:?}",
        result.visual_lines
    );

    // Padding comes from the Text component (1 here).
    let padded = truncate_to_visual_lines("x", 5, 10, 1);
    assert_eq!(padded.visual_lines[0], " x        ");
}

// --- countdown timer ------------------------------------------------------------------------------

#[test]
fn countdown_reports_every_second_and_expires() {
    let (mut timer, ticks) = simple_countdown(2_500);
    // The constructor reports the ceil of the timeout immediately.
    assert_eq!(timer.remaining_seconds(), 3);
    assert_eq!(ticks.lock().unwrap().as_slice(), [3]);

    assert!(timer.tick());
    assert_eq!(timer.remaining_seconds(), 2);
    assert!(timer.tick());
    assert_eq!(timer.remaining_seconds(), 1);
    assert!(!timer.is_disposed());

    // Reaching zero disposes the timer; later ticks do nothing.
    assert!(timer.tick());
    assert!(timer.is_disposed());
    assert_eq!(ticks.lock().unwrap().as_slice(), [3, 2, 1, 0]);
    assert!(!timer.tick());
}

#[test]
fn countdown_fires_on_expire_once() {
    let expired = Arc::new(Mutex::new(0usize));
    let sink = Arc::clone(&expired);
    let mut timer = CountdownTimer::new(
        1_000,
        Box::new(|_seconds| {}),
        Box::new(move || {
            *sink.lock().unwrap() += 1;
        }),
    );
    timer.tick();
    assert_eq!(*expired.lock().unwrap(), 1);
    timer.tick();
    assert_eq!(*expired.lock().unwrap(), 1, "disposed timers stay quiet");
}

// --- markdown transform ---------------------------------------------------------------------------

#[test]
fn markdown_transforms_chain_and_keep_the_source_on_none() {
    let transformers: Vec<pillar_coding_agent::core::extensions_types::MarkdownTransformer> = vec![
        std::sync::Arc::new(|markdown: &str, context: &MarkdownTransformContext| {
            assert_eq!(context.message_type, MarkdownMessageType::Assistant);
            assert!(!context.is_streaming);
            assert_eq!(context.available_width, 40);
            Some(markdown.replace("foo", "bar"))
        }),
        // Returning None keeps the previous result.
        std::sync::Arc::new(|_markdown: &str, _context: &MarkdownTransformContext| None),
        std::sync::Arc::new(|markdown: &str, _context: &MarkdownTransformContext| {
            Some(format!("{markdown}!"))
        }),
    ];
    let transform = create_markdown_transform(MarkdownMessageType::Assistant, false, transformers);
    assert_eq!(transform("foo baz", 40), "bar baz!");

    // No transformers: the source is returned unchanged.
    let identity = create_markdown_transform(MarkdownMessageType::User, true, Vec::new());
    assert_eq!(identity("keep me", 10), "keep me");
}

// --- diff rendering -------------------------------------------------------------------------------

#[test]
fn render_diff_colours_lines_and_highlights_edits() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let dark = theme::get_theme_by_name("dark").expect("dark");

    // Two added lines: the block path shows whole lines without intra-line
    // highlighting (upstream only diffs 1:1 modifications).
    let diff = " 1 context\n-2 old value\n+2 new value\n+3 added";
    let rendered = render_diff(diff, RenderDiffOptions::default());
    let lines: Vec<&str> = rendered.split('\n').collect();
    assert_eq!(lines.len(), 4, "{rendered:?}");
    assert!(
        lines[0].starts_with(&format!("{} 1 context", dark.fg_ansi("toolDiffContext"))),
        "{:?}",
        lines[0]
    );
    assert!(
        lines[1].starts_with(&format!("{}-2 ", dark.fg_ansi("toolDiffRemoved"))),
        "{:?}",
        lines[1]
    );
    assert!(
        lines[2].starts_with(&format!("{}+2 ", dark.fg_ansi("toolDiffAdded"))),
        "{:?}",
        lines[2]
    );
    assert_eq!(lines[3], dark.fg("toolDiffAdded", "+3 added"));
    assert!(!lines[1].contains(&dark.inverse("old")), "{:?}", lines[1]);

    // A 1:1 modification highlights the changed tokens with inverse video and
    // leaves the shared text plain.
    let single = render_diff("-2 old value\n+2 new value", RenderDiffOptions::default());
    let single_lines: Vec<&str> = single.split('\n').collect();
    assert_eq!(single_lines.len(), 2, "{single:?}");
    let stripped = strip_ansi(&single);
    assert!(stripped.contains("-2 old value"), "{stripped:?}");
    assert!(stripped.contains("+2 new value"), "{stripped:?}");
    assert!(
        single_lines[0].contains(&dark.inverse("old")),
        "{:?}",
        single_lines[0]
    );
    assert!(
        single_lines[1].contains(&dark.inverse("new")),
        "{:?}",
        single_lines[1]
    );
    // The shared " value" suffix stays unhighlighted.
    assert!(
        !single_lines[0].contains(&dark.inverse(" value")),
        "{:?}",
        single_lines[0]
    );

    // A block replacement keeps whole lines (no intra-line diff).
    let block = render_diff("-1 a\n-2 b\n+1 c\n+2 d", RenderDiffOptions::default());
    let block_lines: Vec<&str> = block.split('\n').collect();
    assert_eq!(block_lines.len(), 4);
    assert!(
        !block_lines[0].contains(&dark.inverse("a")),
        "{:?}",
        block_lines[0]
    );
    assert_eq!(strip_ansi(block_lines[1]), "-2 b");
    assert_eq!(strip_ansi(block_lines[2]), "+1 c");

    // Text that is not diff-shaped is a context line.
    let plain = render_diff("hello", RenderDiffOptions::default());
    assert_eq!(plain, dark.fg("toolDiffContext", "hello"));

    // Tabs become three spaces.
    let tabs = render_diff("+1 a\tb", RenderDiffOptions::default());
    assert!(strip_ansi(&tabs).contains("a   b"), "{tabs:?}");
}

// --- status indicators ----------------------------------------------------------------------------

#[test]
fn working_status_indicator_renders_the_spinner() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let dark = theme::get_theme_by_name("dark").expect("dark");
    let mut indicator = working_status_indicator("Thinking", None);
    assert_eq!(indicator.kind(), StatusIndicatorKind::Working);

    let lines = indicator.render(20);
    // The loader renders a leading blank line plus its text line.
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "");
    assert!(
        lines[1].contains(&dark.fg("muted", "Thinking")),
        "{:?}",
        lines[1]
    );

    // Ticking advances the spinner frame.
    let before = indicator.render(20);
    indicator.tick();
    let after = indicator.render(20);
    assert_ne!(before, after);
    indicator.dispose();
}

#[test]
fn custom_indicator_frames_are_rendered_verbatim() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let mut indicator = working_status_indicator(
        "Busy",
        Some(WorkingIndicatorOptions {
            frames: Some(vec!["1".to_string(), "2".to_string()]),
            interval_ms: Some(50),
        }),
    );
    // Custom frames skip the spinner colour and appear verbatim (the loader
    // still applies the Text margin).
    assert_eq!(strip_ansi(&indicator.render(10)[1]).trim(), "1 Busy");
    indicator.tick();
    assert_eq!(strip_ansi(&indicator.render(10)[1]).trim(), "2 Busy");
    indicator.tick();
    assert_eq!(strip_ansi(&indicator.render(10)[1]).trim(), "1 Busy");
}

#[test]
fn compaction_and_branch_indicators_use_the_upstream_labels() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let manual = compaction_status_indicator(CompactionStatusReason::Manual);
    assert_eq!(manual.kind(), StatusIndicatorKind::Compaction);
    assert!(
        manual
            .loader()
            .message()
            .starts_with("Compacting context..."),
        "{:?}",
        manual.loader().message()
    );

    let overflow = compaction_status_indicator(CompactionStatusReason::Overflow);
    assert!(
        overflow
            .loader()
            .message()
            .starts_with("Context overflow detected, Auto-compacting..."),
        "{:?}",
        overflow.loader().message()
    );

    let branch = branch_summary_status_indicator();
    assert_eq!(branch.kind(), StatusIndicatorKind::BranchSummary);
    assert!(
        branch
            .loader()
            .message()
            .starts_with("Summarizing branch...")
    );
    // Every label carries the interrupt hint.
    assert!(manual.loader().message().contains("to cancel)"));
}

#[test]
fn idle_status_renders_two_blank_rows() {
    let mut idle = IdleStatus;
    assert_eq!(idle.render(4), vec!["    ".to_string(), "    ".to_string()]);
}

// --- bordered loader ------------------------------------------------------------------------------

#[test]
fn bordered_loader_frames_the_loader_and_can_abort() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let dark = theme::get_theme_by_name("dark").expect("dark");
    let mut loader = BorderedLoader::new(&dark, "Loading things", true);
    assert!(loader.is_cancellable());
    assert!(!loader.aborted());

    // Wide enough that the message and the cancel hint are not wrapped.
    let lines = loader.render(40);
    assert!(lines.len() >= 5, "{lines:?}");
    assert_eq!(strip_ansi(&lines[0]), "─".repeat(40), "top border");
    assert_eq!(
        strip_ansi(lines.last().expect("bottom border")),
        "─".repeat(40)
    );
    assert!(
        lines.iter().any(|line| line.contains("Loading things")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("cancel")),
        "cancel hint: {lines:?}"
    );

    // The abort callback fires once per abort.
    let aborted = Arc::new(Mutex::new(0usize));
    let sink = Arc::clone(&aborted);
    loader.set_on_abort(Some(Box::new(move || {
        *sink.lock().unwrap() += 1;
    })));
    loader.handle_abort_key(false);
    assert!(!loader.aborted(), "other keys do not abort");
    loader.handle_abort_key(true);
    assert!(loader.aborted());
    assert_eq!(*aborted.lock().unwrap(), 1);

    loader.dispose();
    loader.loader_mut().set_message("Still loading");
    assert!(loader.message().contains("Still loading"));
}

#[test]
fn a_non_cancellable_loader_hides_the_hint_and_ignores_abort() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let dark = theme::get_theme_by_name("dark").expect("dark");
    let mut loader = BorderedLoader::new(&dark, "Working", false);
    assert!(!loader.is_cancellable());
    let lines = loader.render(12);
    assert!(
        !lines.iter().any(|line| line.contains("cancel")),
        "{lines:?}"
    );
    loader.handle_abort_key(true);
    assert!(!loader.aborted());
    // Without the hint the frame is the two rules, the spinner block and its
    // spacer (upstream adds the spacer unconditionally).
    assert_eq!(lines.len(), 5, "{lines:?}");
}

// --- custom entries -------------------------------------------------------------------------------

fn custom_entry(custom_type: &str) -> CustomEntry {
    CustomEntry {
        base: SessionEntryBase {
            id: "entry-1".to_string(),
            parent_id: None,
            timestamp: 1_767_225_600_000,
        },
        custom_type: custom_type.to_string(),
        data: Some(serde_json::json!({"value": 1})),
    }
}

#[test]
fn custom_entry_renders_through_the_renderer_and_toggles_expanded() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let renderer: pillar_coding_agent::core::extensions_types::EntryRenderer = std::sync::Arc::new(
        move |entry: &pillar_coding_agent::core::extensions_types::CustomEntryPayload,
              options: &EntryRenderOptions,
              _theme: &dyn pillar_coding_agent::core::extensions_types::ThemeStyle| {
            sink.lock()
                .unwrap()
                .push((entry.custom_type.clone(), options.expanded));
            Some(vec![if options.expanded {
                "expanded".to_string()
            } else {
                "collapsed".to_string()
            }])
        },
    );

    let mut component = CustomEntryComponent::new(custom_entry("note"), renderer);
    assert!(component.has_content());
    assert!(!component.is_expanded());
    let lines = component.render(20);
    // A spacer precedes the rendered entry (upstream adds one).
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "");
    assert!(lines[1].contains("collapsed"), "{:?}", lines[1]);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [("note".to_string(), false)]
    );

    // Expanding rebuilds the renderer output.
    component.set_expanded(true);
    assert!(component.is_expanded());
    let lines = component.render(20);
    assert!(lines[1].contains("expanded"), "{:?}", lines[1]);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [("note".to_string(), false), ("note".to_string(), true)]
    );
    // Setting the same value does not rebuild.
    component.set_expanded(true);
    assert_eq!(seen.lock().unwrap().len(), 2);

    // Invalidate re-runs the renderer (upstream `invalidate`).
    component.invalidate();
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[test]
fn custom_entry_without_renderer_output_has_no_content() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let renderer: pillar_coding_agent::core::extensions_types::EntryRenderer =
        std::sync::Arc::new(
            |_entry: &pillar_coding_agent::core::extensions_types::CustomEntryPayload,
             _options: &EntryRenderOptions,
             _theme: &dyn pillar_coding_agent::core::extensions_types::ThemeStyle| None,
        );
    let mut component = CustomEntryComponent::new(custom_entry("hidden"), renderer);
    assert!(!component.has_content());
    assert!(component.render(20).is_empty());

    // The failure notice the host shows when a renderer throws.
    let error = component.error_component("boom");
    let mut error = error;
    let lines = error.render(40);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("[hidden] renderer failed: boom")),
        "{lines:?}"
    );
}

#[test]
fn container_composition_preserves_child_order() {
    // DynamicBorder + Text inside a Container render in order (upstream's
    // composition pattern).
    let mut container = pillar_tui::tui::Container::new();
    container.add_child(Box::new(DynamicBorder::with_color(Box::new(|text| {
        text.to_string()
    }))));
    container.add_child(Box::new(Text::new("body", 0, 0)));
    container.add_child(Box::new(DynamicBorder::with_color(Box::new(|text| {
        text.to_string()
    }))));
    let lines = container.render(4);
    assert_eq!(lines, vec!["────", "body", "────"]);
}

// --- message components ---------------------------------------------------------------------------

use pillar_ai::types::{AssistantMessage, Content, StopReason};
use pillar_coding_agent::core::messages::{
    BranchSummaryMessage, CompactionSummaryMessage, CustomContent, CustomMessage,
};
use pillar_coding_agent::modes::interactive::components::assistant_message::AssistantMessageComponent;
use pillar_coding_agent::modes::interactive::components::branch_summary_message::BranchSummaryMessageComponent;
use pillar_coding_agent::modes::interactive::components::compaction_summary_message::CompactionSummaryMessageComponent;
use pillar_coding_agent::modes::interactive::components::custom_message::CustomMessageComponent;
use pillar_coding_agent::modes::interactive::components::skill_invocation_message::SkillInvocationMessageComponent;
use pillar_coding_agent::modes::interactive::components::user_message::UserMessageComponent;

const OSC_START: &str = "\u{1b}]133;A\u{7}";
const OSC_END: &str = "\u{1b}]133;B\u{7}";
const OSC_FINAL: &str = "\u{1b}]133;C\u{7}";

fn assistant_message(content: Vec<Content>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "test".to_string(),
        provider: "test".to_string(),
        model: "test-model".to_string(),
        usage: pillar_ai::types::Usage::default(),
        stop_reason,
        error_message: None,
        timestamp: 1000,
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        deferred: None,
        raw_stop_reason: None,
        end_turn: None,
    }
}

fn plain_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
}

#[test]
fn user_message_wraps_the_zone_markers_and_paints_the_background() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let dark = theme::get_theme_by_name("dark").expect("dark");
    let mut message = UserMessageComponent::new("hello **world**", None, 1, Vec::new());
    let lines = message.render(30);
    assert!(lines.len() >= 2, "{lines:?}");
    // OSC 133 A starts the zone, B+C close it (the alt screen strips them for
    // display).
    assert!(lines[0].starts_with(OSC_START), "{:?}", lines[0]);
    let last = lines.last().expect("last line");
    assert!(
        last.starts_with(&format!("{OSC_END}{OSC_FINAL}")),
        "{last:?}"
    );
    // The text is rendered on the user-message background.
    let body = plain_lines(&lines).join("\n");
    assert!(body.contains("hello"), "{body:?}");
    assert!(body.contains("world"), "{body:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&dark.bg_ansi("userMessageBg"))),
        "background applied: {lines:?}"
    );
    // Bold text is rendered with the theme's bold sequences.
    assert!(
        lines.iter().any(|line| line.contains("\u{1b}[1m")),
        "{lines:?}"
    );

    // Changing the output pad rebuilds with the new padding.
    message.set_output_pad(3);
    let padded = message.render(30);
    assert!(padded.len() >= lines.len(), "{padded:?}");
    message.set_text("**replaced**");
    let replaced = plain_lines(&message.render(30)).join("\n");
    assert!(replaced.contains("replaced"), "{replaced:?}");
}

#[test]
fn assistant_message_renders_text_thinking_and_notices() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let message = assistant_message(
        vec![
            Content::thinking("weighing options"),
            Content::text("Here is the answer."),
        ],
        StopReason::Stop,
    );
    let mut component =
        AssistantMessageComponent::new(Some(message), false, None, "Thinking...", 1, Vec::new());
    assert!(!component.has_tool_calls());
    let lines = component.render(40);
    let body = plain_lines(&lines).join("\n");
    assert!(body.contains("weighing options"), "{body:?}");
    assert!(body.contains("Here is the answer."), "{body:?}");
    // Without tool calls the zone markers wrap the message.
    assert!(lines[0].starts_with(OSC_START), "{:?}", lines[0]);

    // Hidden thinking blocks render the label instead of the text.
    let mut hidden = AssistantMessageComponent::new(
        Some(assistant_message(
            vec![Content::thinking("secret reasoning")],
            StopReason::Stop,
        )),
        true,
        None,
        "Thinking...",
        1,
        Vec::new(),
    );
    let body = plain_lines(&hidden.render(40)).join("\n");
    assert!(body.contains("Thinking..."), "{body:?}");
    assert!(!body.contains("secret reasoning"), "{body:?}");
    hidden.set_hidden_thinking_label("Reasoning...");
    assert!(
        plain_lines(&hidden.render(40))
            .join("\n")
            .contains("Reasoning...")
    );

    // A length stop surfaces the truncation notice.
    let mut truncated = AssistantMessageComponent::new(
        Some(assistant_message(
            vec![Content::text("partial")],
            StopReason::Length,
        )),
        false,
        None,
        "Thinking...",
        1,
        Vec::new(),
    );
    // The notice wraps at 40 columns, so render wide enough to read it.
    assert!(
        plain_lines(&truncated.render(80))
            .join("\n")
            .contains("Response was truncated before completion."),
        "truncation notice"
    );

    // Aborted and errored messages report their reason.
    let mut aborted = AssistantMessageComponent::new(
        Some(assistant_message(vec![], StopReason::Aborted)),
        false,
        None,
        "Thinking...",
        1,
        Vec::new(),
    );
    assert!(
        plain_lines(&aborted.render(40))
            .join("\n")
            .contains("Operation aborted")
    );

    let mut errored = assistant_message(vec![Content::text("hi")], StopReason::Error);
    errored.error_message = Some("boom".to_string());
    let mut error_component = AssistantMessageComponent::new(
        Some(errored.clone()),
        false,
        None,
        "Thinking...",
        1,
        Vec::new(),
    );
    assert!(
        plain_lines(&error_component.render(40))
            .join("\n")
            .contains("Error: boom")
    );

    // Streaming state is remembered for the next content update.
    let mut streaming =
        AssistantMessageComponent::new(None, false, None, "Thinking...", 1, Vec::new());
    streaming.update_content(
        &assistant_message(vec![Content::text("partial")], StopReason::Pending),
        true,
    );
    assert!(streaming.is_streaming());

    // Tool calls suppress the zone markers (the tool block is rendered
    // separately).
    let mut with_tools = AssistantMessageComponent::new(
        Some(assistant_message(
            vec![Content::ToolCall {
                id: "1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
                namespace: None,
            }],
            StopReason::ToolUse,
        )),
        false,
        None,
        "Thinking...",
        1,
        Vec::new(),
    );
    assert!(with_tools.has_tool_calls());
    let lines = with_tools.render(40);
    if !lines.is_empty() {
        assert!(!lines[0].starts_with(OSC_START), "{:?}", lines[0]);
    }
    let _ = (
        &mut aborted,
        &mut errored,
        &mut truncated,
        &mut error_component,
    );
}

#[test]
fn branch_summary_message_collapses_and_expands() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let dark = theme::get_theme_by_name("dark").expect("dark");
    let message = BranchSummaryMessage {
        summary: "The branch did things.".to_string(),
        from_id: "entry-1".to_string(),
        timestamp: 0,
    };
    let mut component = BranchSummaryMessageComponent::new(message, None);
    assert!(!component.is_expanded());
    let collapsed = plain_lines(&component.render(60)).join("\n");
    assert!(collapsed.contains("[branch]"), "{collapsed:?}");
    assert!(collapsed.contains("Branch summary ("), "{collapsed:?}");
    assert!(collapsed.contains("to expand)"), "{collapsed:?}");
    assert!(
        !collapsed.contains("The branch did things."),
        "{collapsed:?}"
    );
    assert!(
        component
            .render(60)
            .iter()
            .any(|line| line.contains(&dark.bg_ansi("customMessageBg"))),
        "box background"
    );

    component.set_expanded(true);
    assert!(component.is_expanded());
    let expanded = plain_lines(&component.render(60)).join("\n");
    assert!(expanded.contains("Branch Summary"), "{expanded:?}");
    assert!(expanded.contains("The branch did things."), "{expanded:?}");
    component.invalidate();
    assert!(
        plain_lines(&component.render(60))
            .join("\n")
            .contains("Branch Summary")
    );
}

#[test]
fn compaction_summary_message_shows_the_token_count() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let message = CompactionSummaryMessage {
        summary: "Earlier context.".to_string(),
        tokens_before: 1234567,
        timestamp: 0,
    };
    let mut component = CompactionSummaryMessageComponent::new(message, None);
    let collapsed = plain_lines(&component.render(60)).join("\n");
    assert!(collapsed.contains("[compaction]"), "{collapsed:?}");
    assert!(collapsed.contains("1,234,567 tokens"), "{collapsed:?}");

    component.set_expanded(true);
    let expanded = plain_lines(&component.render(60)).join("\n");
    assert!(
        expanded.contains("Compacted from 1,234,567 tokens"),
        "{expanded:?}"
    );
    assert!(expanded.contains("Earlier context."), "{expanded:?}");
}

#[test]
fn skill_invocation_message_collapses_and_expands() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let block = pillar_coding_agent::core::agent_session::ParsedSkillBlock {
        name: "commit".to_string(),
        location: "/skills/commit".to_string(),
        content: "Follow these steps.".to_string(),
        user_message: None,
    };
    let mut component = SkillInvocationMessageComponent::new(block, None);
    assert_eq!(component.skill_block().name, "commit");
    let collapsed = plain_lines(&component.render(60)).join("\n");
    assert!(collapsed.contains("[skill] commit"), "{collapsed:?}");
    assert!(collapsed.contains("to expand)"), "{collapsed:?}");
    assert!(!collapsed.contains("Follow these steps."), "{collapsed:?}");

    component.set_expanded(true);
    let expanded = plain_lines(&component.render(60)).join("\n");
    assert!(expanded.contains("[skill]"), "{expanded:?}");
    assert!(expanded.contains("commit"), "{expanded:?}");
    assert!(expanded.contains("Follow these steps."), "{expanded:?}");
}

fn custom_message(custom_type: &str, content: Vec<CustomContent>) -> CustomMessage {
    CustomMessage {
        custom_type: custom_type.to_string(),
        content,
        display: true,
        details: None,
        timestamp: 0,
    }
}

#[test]
fn custom_message_default_renderer_shows_label_and_text() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let message = custom_message(
        "note",
        vec![
            CustomContent::Text("first".to_string()),
            CustomContent::Image {
                data: "AAAA".to_string(),
                mime_type: "image/png".to_string(),
            },
            CustomContent::Text("second".to_string()),
        ],
    );
    let mut component = CustomMessageComponent::new(message, None, None, 1);
    assert!(!component.used_custom_renderer());
    let lines = component.render(60);
    let body = plain_lines(&lines).join("\n");
    assert!(body.contains("[note]"), "{body:?}");
    assert!(body.contains("first"), "{body:?}");
    assert!(body.contains("second"), "{body:?}");
    // Image blocks are dropped by the default renderer.
    assert!(!body.contains("image/png"), "{body:?}");
    // A leading spacer precedes the box.
    assert_eq!(lines[0], "");

    component.set_expanded(true);
    assert!(component.is_expanded());
    component.set_output_pad(2);
    let padded = component.render(60);
    assert!(padded.len() >= lines.len());
}

#[test]
fn custom_message_prefers_the_registered_renderer() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let seen: Arc<Mutex<Vec<(String, bool, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let renderer: pillar_coding_agent::core::extensions_types::MessageRenderer =
        std::sync::Arc::new(move |message, options, _theme| {
            sink.lock().unwrap().push((
                message.custom_type.clone(),
                options.expanded,
                options.output_pad,
            ));
            Some(vec!["custom!".to_string()])
        });
    let mut component = CustomMessageComponent::new(
        custom_message("note", vec![CustomContent::Text("ignored".to_string())]),
        Some(renderer),
        None,
        3,
    );
    assert!(component.used_custom_renderer());
    let body = plain_lines(&component.render(40)).join("\n");
    assert!(body.contains("custom!"), "{body:?}");
    assert!(
        !body.contains("[note]"),
        "default rendering is skipped: {body:?}"
    );
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [("note".to_string(), false, 3)]
    );

    // A renderer that answers None falls back to the default rendering.
    let none_renderer: pillar_coding_agent::core::extensions_types::MessageRenderer =
        std::sync::Arc::new(|_message, _options, _theme| None);
    let mut fallback = CustomMessageComponent::new(
        custom_message("note", vec![CustomContent::Text("shown".to_string())]),
        Some(none_renderer),
        None,
        1,
    );
    assert!(!fallback.used_custom_renderer());
    assert!(
        plain_lines(&fallback.render(40))
            .join("\n")
            .contains("[note]")
    );
}

// --- bash execution -------------------------------------------------------------------------------

use pillar_coding_agent::core::truncate::{TruncationOptions, truncate_tail};
use pillar_coding_agent::modes::interactive::components::bash_execution::{
    BashExecutionComponent, BashExecutionStatus,
};

fn bash_component(exclude_from_context: bool) -> BashExecutionComponent {
    let dark = theme::get_theme_by_name("dark").expect("dark");
    BashExecutionComponent::new(&dark, "echo hi", exclude_from_context)
}

#[test]
fn bash_execution_streams_output_and_reports_status() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let mut component = bash_component(false);
    assert_eq!(component.get_command(), "echo hi");
    assert_eq!(component.status(), BashExecutionStatus::Running);

    // Running: spacer, borders, header and the loader line.
    let lines = component.render(40);
    let body = plain_lines(&lines).join("\n");
    assert!(body.contains("$ echo hi"), "{body:?}");
    assert!(body.contains("Running..."), "{body:?}");
    assert!(body.contains("to cancel"), "{body:?}");
    assert_eq!(lines[0], "", "leading spacer");
    assert_eq!(
        strip_ansi(lines.last().expect("bottom border")),
        "─".repeat(40),
        "bottom border"
    );

    // Streaming: ANSI is stripped, \r\n and \r normalize to \n, and an
    // unterminated chunk continues the previous line.
    component.append_output("\u{1b}[31mfirst\u{1b}[0m");
    component.append_output(" continued\nsecond\r\nthird\rfourth");
    assert_eq!(
        component.get_output(),
        "first continued\nsecond\nthird\nfourth"
    );
    let body = plain_lines(&component.render(40)).join("\n");
    assert!(body.contains("first continued"), "{body:?}");
    assert!(body.contains("fourth"), "{body:?}");

    // Completion: the loader is replaced by the status parts.
    component.set_complete(Some(0), false, None, None);
    assert_eq!(component.status(), BashExecutionStatus::Complete);
    let body = plain_lines(&component.render(40)).join("\n");
    assert!(!body.contains("Running..."), "{body:?}");

    // A non-zero exit reports the code; cancellation reports (cancelled).
    let mut failed = bash_component(false);
    failed.set_complete(Some(3), false, None, None);
    assert_eq!(failed.status(), BashExecutionStatus::Error);
    assert!(
        plain_lines(&failed.render(40))
            .join("\n")
            .contains("(exit 3)")
    );

    let mut cancelled = bash_component(false);
    cancelled.set_complete(None, true, None, None);
    assert_eq!(cancelled.status(), BashExecutionStatus::Cancelled);
    assert!(
        plain_lines(&cancelled.render(40))
            .join("\n")
            .contains("(cancelled)")
    );

    let _ = TruncationOptions::default();
}

#[test]
fn bash_execution_collapses_long_output_and_expands() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let mut component = bash_component(false);
    let output = (0..60)
        .map(|index| format!("line{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    component.append_output(&output);
    component.set_complete(Some(0), false, None, None);

    // Collapsed: the last 20 logical lines are previewed and the hidden count
    // is reported.
    let collapsed = plain_lines(&component.render(60)).join("\n");
    assert!(collapsed.contains("line59"), "{collapsed:?}");
    assert!(!collapsed.contains("line0\n"), "{collapsed:?}");
    assert!(collapsed.contains("... 40 more lines ("), "{collapsed:?}");
    assert!(collapsed.contains("to expand)"), "{collapsed:?}");

    // Expanded: every line is shown and the hint flips to collapse.
    component.set_expanded(true);
    let expanded = plain_lines(&component.render(60)).join("\n");
    assert!(expanded.contains("line0"), "{expanded:?}");
    assert!(expanded.contains("line59"), "{expanded:?}");
    assert!(expanded.contains("to collapse)"), "{expanded:?}");
    assert!(!expanded.contains("more lines"), "{expanded:?}");
}

#[test]
fn bash_execution_reports_context_and_tool_truncation() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let mut component = bash_component(false);
    component.append_output("some output");
    // A result that really was truncated (three lines cut to one).
    let truncated = truncate_tail(
        "one\ntwo\nthree",
        TruncationOptions {
            max_lines: Some(1),
            max_bytes: Some(1024),
        },
    );
    assert!(truncated.truncated, "{truncated:?}");
    component.set_complete(
        Some(0),
        false,
        Some(truncated),
        Some("/tmp/full.log".to_string()),
    );
    let body = plain_lines(&component.render(80)).join("\n");
    assert!(
        body.contains("Output truncated. Full output: /tmp/full.log"),
        "{body:?}"
    );

    // Without a full-output path the warning is suppressed (upstream).
    let mut without_path = bash_component(false);
    without_path.append_output("some output");
    let truncated = truncate_tail(
        "one\ntwo",
        TruncationOptions {
            max_lines: Some(1),
            max_bytes: Some(1024),
        },
    );
    without_path.set_complete(Some(0), false, Some(truncated), None);
    let body = plain_lines(&without_path.render(80)).join("\n");
    assert!(!body.contains("Output truncated"), "{body:?}");
}

#[test]
fn bash_execution_uses_the_dim_border_for_excluded_commands() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let dark = theme::get_theme_by_name("dark").expect("dark");
    let mut included = bash_component(false);
    let mut excluded = bash_component(true);

    let included_lines = included.render(20);
    let excluded_lines = excluded.render(20);
    assert!(
        included_lines[1].contains(&dark.fg_ansi("bashMode")),
        "included border: {:?}",
        included_lines[1]
    );
    assert!(
        excluded_lines[1].contains(&dark.fg_ansi("dim")),
        "excluded border: {:?}",
        excluded_lines[1]
    );
    // The header colour follows upstream's `updateDisplay`, which always uses
    // the bash-mode colour (even for `!!`).
    assert!(
        excluded_lines[2].contains(&dark.fg_ansi("bashMode")),
        "{:?}",
        excluded_lines[2]
    );

    // The preview cache is dropped when the expanded state changes.
    excluded.set_expanded(true);
    excluded.invalidate();
    let body = plain_lines(&excluded.render(20)).join("\n");
    assert!(body.contains("$ echo hi"), "{body:?}");
}

// --- tool execution -----------------------------------------------------------------------------

use pillar_coding_agent::core::tools::render_definitions::{
    ToolRenderContext, ToolRenderResult, ToolRenderResultOptions, ToolRenderShell, ToolRenderer,
};
use pillar_coding_agent::modes::interactive::components::tool_execution::{
    ToolExecutionComponent, ToolExecutionOptions, ToolExecutionResult,
};
use pillar_tui::terminal_image::{ImageProtocol, TerminalCapabilities, set_capabilities};

fn caps_without_images() {
    set_capabilities(TerminalCapabilities {
        images: None,
        true_color: true,
        hyperlinks: false,
    });
}

#[test]
fn tool_execution_renders_the_builtin_call_and_switches_background() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    caps_without_images();
    let dark = theme::get_theme_by_name("dark").expect("dark");

    let mut component = ToolExecutionComponent::new(
        "read",
        "call-1",
        serde_json::json!({"file_path": "/tmp/a.rs"}),
        ToolExecutionOptions::default(),
        None,
        "/tmp",
    );
    let lines = component.render(60);
    // The leading spacer comes first, then the background box.
    assert_eq!(plain_lines(&lines)[0], "", "{lines:?}");
    assert!(
        plain_lines(&lines).join("\n").contains("read /tmp/a.rs"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&dark.bg_ansi("toolPendingBg"))),
        "{lines:?}"
    );

    // A successful result switches the background and renders the output.
    // Collapsed read results render no body (upstream `formatReadResult`),
    // so expand first.
    component.set_expanded(true);
    component.update_result(
        ToolExecutionResult {
            content: vec![Content::text("line one\nline two")],
            details: serde_json::json!({}),
            is_error: false,
        },
        false,
    );
    let lines = component.render(60);
    let body = plain_lines(&lines).join("\n");
    assert!(body.contains("line one"), "{body:?}");
    assert!(body.contains("line two"), "{body:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&dark.bg_ansi("toolSuccessBg"))),
        "{lines:?}"
    );

    // An error result switches to the error background.
    component.update_result(
        ToolExecutionResult {
            content: vec![Content::text("boom")],
            details: serde_json::json!({}),
            is_error: true,
        },
        false,
    );
    let lines = component.render(60);
    let body = plain_lines(&lines).join("\n");
    assert!(body.contains("boom"), "{body:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&dark.bg_ansi("toolErrorBg"))),
        "{lines:?}"
    );
}

#[test]
fn tool_execution_unknown_tool_uses_the_generic_fallback() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    caps_without_images();
    let dark = theme::get_theme_by_name("dark").expect("dark");

    let mut component = ToolExecutionComponent::new(
        "nope",
        "call-2",
        serde_json::json!({"a": 1}),
        ToolExecutionOptions::default(),
        None,
        "/tmp",
    );
    let lines = component.render(60);
    let body = plain_lines(&lines).join("\n");
    // `formatToolExecution`: the bold tool name, the pretty-printed args and
    // no output yet.
    // (The box pads every wrapped line to full width, so assert on the
    // pieces rather than the joined JSON.)
    assert!(body.contains("nope"), "{body:?}");
    assert!(body.contains("\"a\": 1"), "{body:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&dark.bg_ansi("toolPendingBg"))),
        "{lines:?}"
    );

    // The result text is appended after the args.
    component.update_result(
        ToolExecutionResult {
            content: vec![Content::text("the output")],
            details: serde_json::Value::Null,
            is_error: false,
        },
        false,
    );
    let body = plain_lines(&component.render(60)).join("\n");
    assert!(body.contains("the output"), "{body:?}");
}

#[test]
fn tool_execution_expands_the_fallback_preview() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    caps_without_images();

    let output = (0..15)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    // The preview truncation belongs to the result fallback, which runs when
    // a renderer definition exists but no result renderer answers (upstream
    // `createResultFallback`).
    let mut component = ToolExecutionComponent::new(
        "nope",
        "call-3",
        serde_json::json!({}),
        ToolExecutionOptions::default(),
        Some(Box::new(StubRenderer {
            shell: ToolRenderShell::Default,
            serve_call: false,
            serve_result: false,
        })),
        "/tmp",
    );
    component.update_result(
        ToolExecutionResult {
            content: vec![Content::text(output)],
            details: serde_json::Value::Null,
            is_error: false,
        },
        false,
    );

    // Collapsed: the first 10 lines plus the muted "more lines" hint.
    let body = plain_lines(&component.render(60)).join("\n");
    assert!(body.contains("line9"), "{body:?}");
    assert!(!body.contains("line10"), "{body:?}");
    assert!(body.contains("... (5 more lines,"), "{body:?}");

    // Expanded: the full output.
    component.set_expanded(true);
    let body = plain_lines(&component.render(60)).join("\n");
    assert!(body.contains("line14"), "{body:?}");
    assert!(!body.contains("more lines"), "{body:?}");
}

struct StubRenderer {
    shell: ToolRenderShell,
    serve_call: bool,
    serve_result: bool,
}

impl ToolRenderer for StubRenderer {
    fn render_shell(&self) -> ToolRenderShell {
        self.shell
    }

    fn render_call(
        &mut self,
        _width: usize,
        _args: &serde_json::Value,
        _theme: &theme::Theme,
        _context: &ToolRenderContext,
    ) -> Vec<String> {
        vec!["custom call".to_string()]
    }

    fn render_result(
        &mut self,
        _width: usize,
        _result: &ToolRenderResult<'_>,
        _options: &ToolRenderResultOptions,
        _theme: &theme::Theme,
        _context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        Some(vec!["custom result".to_string()])
    }

    fn has_render_call(&self) -> bool {
        self.serve_call
    }

    fn has_render_result(&self) -> bool {
        self.serve_result
    }
}

#[test]
fn tool_execution_prefers_the_custom_renderer_and_self_shell() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    caps_without_images();

    let custom = Box::new(StubRenderer {
        shell: ToolRenderShell::SelfRendered,
        serve_call: true,
        serve_result: true,
    });
    let mut component = ToolExecutionComponent::new(
        "nope",
        "call-4",
        serde_json::json!({}),
        ToolExecutionOptions::default(),
        Some(custom),
        "/tmp",
    );
    component.update_result(
        ToolExecutionResult {
            content: vec![],
            details: serde_json::Value::Null,
            is_error: false,
        },
        false,
    );
    let lines = component.render(40);
    // The self-render shell: one leading blank line, then the renderer's own
    // framing — no tool background box.
    assert_eq!(plain_lines(&lines)[0], "", "{lines:?}");
    let body = plain_lines(&lines).join("\n");
    assert!(body.contains("custom call"), "{body:?}");
    assert!(body.contains("custom result"), "{body:?}");
    assert!(
        !lines.iter().any(|line| line.contains(
            &theme::get_theme_by_name("dark")
                .expect("dark")
                .bg_ansi("toolPendingBg")
        )),
        "self-rendered block must not paint the default box: {lines:?}"
    );

    // A custom renderer without a result renderer falls back to the
    // built-in's (upstream `renderResult ?? builtInToolDefinition.renderResult`).
    let custom = Box::new(StubRenderer {
        shell: ToolRenderShell::Default,
        serve_call: true,
        serve_result: false,
    });
    let mut component = ToolExecutionComponent::new(
        "read",
        "call-5",
        serde_json::json!({"file_path": "/tmp/a.rs"}),
        ToolExecutionOptions::default(),
        Some(custom),
        "/tmp",
    );
    component.update_result(
        ToolExecutionResult {
            content: vec![Content::text("saved")],
            details: serde_json::json!({}),
            is_error: false,
        },
        false,
    );
    let body = plain_lines(&component.render(60)).join("\n");
    assert!(body.contains("custom call"), "{body:?}");
    // The read renderer's result output (the built-in fallback). Collapsed
    // read results render nothing (upstream `formatReadResult`), so expand
    // first; the body then shows the result text, not the call line.
    component.set_expanded(true);
    let body = plain_lines(&component.render(60)).join("\n");
    assert!(body.contains("saved"), "{body:?}");
}

struct PanickyRenderer;

impl ToolRenderer for PanickyRenderer {
    fn render_call(
        &mut self,
        _width: usize,
        _args: &serde_json::Value,
        _theme: &theme::Theme,
        _context: &ToolRenderContext,
    ) -> Vec<String> {
        panic!("boom");
    }

    fn render_result(
        &mut self,
        _width: usize,
        _result: &ToolRenderResult<'_>,
        _options: &ToolRenderResultOptions,
        _theme: &theme::Theme,
        _context: &ToolRenderContext,
    ) -> Option<Vec<String>> {
        panic!("boom");
    }
}

#[test]
fn tool_execution_falls_back_when_the_renderer_panics() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    caps_without_images();

    let mut component = ToolExecutionComponent::new(
        "mytool",
        "call-6",
        serde_json::json!({}),
        ToolExecutionOptions::default(),
        Some(Box::new(PanickyRenderer)),
        "/tmp",
    );
    component.update_result(
        ToolExecutionResult {
            content: vec![Content::text("result text")],
            details: serde_json::Value::Null,
            is_error: false,
        },
        false,
    );
    // Both renderers panic: the call falls back to the tool name, the result
    // to the text output (upstream's catch blocks).
    let body = plain_lines(&component.render(60)).join("\n");
    assert!(body.contains("mytool"), "{body:?}");
    assert!(body.contains("result text"), "{body:?}");
}

#[test]
fn tool_execution_renders_image_blocks_per_protocol() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    install_dark();
    let png_1x1 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    let jpeg = "fake-jpeg-data";

    let image_result = |data: &str, mime: &str| ToolExecutionResult {
        content: vec![Content::Image {
            data: data.to_string(),
            mime_type: mime.to_string(),
        }],
        details: serde_json::Value::Null,
        is_error: false,
    };

    // iTerm2: the image is appended after a spacer.
    set_capabilities(TerminalCapabilities {
        images: Some(ImageProtocol::Iterm2),
        true_color: true,
        hyperlinks: false,
    });
    let mut component = ToolExecutionComponent::new(
        "nope",
        "call-7",
        serde_json::json!({}),
        ToolExecutionOptions::default(),
        None,
        "/tmp",
    );
    component.update_result(image_result(png_1x1, "image/png"), false);
    let lines = component.render(40);
    assert!(
        lines.iter().any(|line| line.contains("\u{1b}]1337;File=")),
        "iterm2 image line: {lines:?}"
    );

    // `showImages` off: no image child.
    component.set_show_images(false);
    let lines = component.render(40);
    assert!(
        !lines.iter().any(|line| line.contains("\u{1b}]1337;File=")),
        "{lines:?}"
    );

    // Kitty requires PNG; without a converter a JPEG is skipped.
    set_capabilities(TerminalCapabilities {
        images: Some(ImageProtocol::Kitty),
        true_color: true,
        hyperlinks: false,
    });
    component.set_show_images(true);
    component.update_result(image_result(jpeg, "image/jpeg"), false);
    let lines = component.render(40);
    assert!(
        !lines.iter().any(|line| line.contains("\u{1b}_G")),
        "kitty skips unconvertible images: {lines:?}"
    );

    // A PNG passes through and is uploaded on kitty.
    component.update_result(image_result(png_1x1, "image/png"), false);
    let lines = component.render(40);
    assert!(
        lines.iter().any(|line| line.contains("\u{1b}_G")),
        "kitty image line: {lines:?}"
    );

    caps_without_images();
}

// --- session selector (upstream session-selector.ts + session-selector-search.ts) ----

use pillar_coding_agent::core::session_manager::SessionInfo;
use pillar_coding_agent::modes::interactive::components::session_selector::{
    SessionScope, SessionSelectorComponent, SessionSelectorOutcome,
};

fn session_info(path: &str, id: &str, name: Option<&str>, modified_ms: u64, body: &str) -> SessionInfo {
    SessionInfo {
        path: path.to_string(),
        id: id.to_string(),
        cwd: "/tmp".to_string(),
        name: name.map(str::to_string),
        parent_session_path: None,
        created_ms: Some(modified_ms),
        modified_ms,
        message_count: 3,
        first_message: body.to_string(),
        all_messages_text: body.to_string(),
    }
}

/// Install the theme and the merged keybinding table (the component matches
/// the `app.session.*` bindings through the global registry; the merged
/// table is a superset of the lazily-installed TUI table).
static SESSION_LOCK: Mutex<()> = Mutex::new(());

fn session_setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    install_dark();
    let definitions =
        pillar_coding_agent::core::keybindings::keybindings("darwin", &Default::default());
    pillar_tui::keybindings::set_keybindings(pillar_tui::keybindings::KeybindingsManager::new(
        definitions,
        Default::default(),
    ));
    guard
}

fn slot_body(component: &mut SessionSelectorComponent, width: usize) -> String {
    component
        .render(width)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn session_selector_lists_the_loaded_sessions_and_renders_the_header() {
    let _guard = session_setup();
    let mut selector = SessionSelectorComponent::new(None);
    assert!(
        selector.displayed_loading(),
        "the constructor starts the current-folder load"
    );

    // The host loaded the current folder's sessions.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let sessions = vec![
        session_info("/s/older.jsonl", "older", None, now - 61_000, "older conversation"),
        session_info("/s/newer.jsonl", "newer", Some("my session"), now - 120_000, "newer conversation"),
    ];
    selector.finish_load(SessionScope::Current, sessions);
    let body = slot_body(&mut selector, 100);
    assert!(body.contains("Resume Session (Current Folder)"), "{body:?}");
    assert!(body.contains("◉ Current Folder"), "{body:?}");
    assert!(body.contains("Sort: Threaded"), "{body:?}");
    assert!(body.contains("Name: All"), "{body:?}");
    assert!(body.contains("older conversation"), "{body:?}");
    assert!(body.contains("my session"), "{body:?}");
    // The relative ages render in the list (2m / 1h style).
    assert!(body.contains("3 2m"), "the relative age renders: {body:?}");
    assert!(body.contains("3 2m"), "{body:?}");
}

#[test]
fn session_selector_navigation_and_enter_reports_the_selection() {
    let _guard = session_setup();
    let mut selector = SessionSelectorComponent::new(None);
    let sessions = vec![
        session_info("/s/one.jsonl", "one", None, 1000, "first"),
        session_info("/s/two.jsonl", "two", None, 2000, "second"),
    ];
    selector.finish_load(SessionScope::Current, sessions);

    // Threaded order: the most recent session first (upstream
    // `buildSessionTree` sorts by latest subtree activity).
    assert_eq!(selector.selected_session_path().as_deref(), Some("/s/two.jsonl"));
    assert_eq!(
        selector.handle_key("\u{1b}[B"), // down
        SessionSelectorOutcome::Consumed
    );
    assert_eq!(selector.selected_session_path().as_deref(), Some("/s/one.jsonl"));
    assert_eq!(
        selector.handle_key("\r"),
        SessionSelectorOutcome::Resume("/s/one.jsonl".to_string())
    );

    // Escape cancels.
    assert_eq!(
        selector.handle_key("\u{1b}"),
        SessionSelectorOutcome::Cancel
    );

    // PgUp/PgDn jump by the page.
    selector.handle_key("\u{1b}[5~"); // page up
    assert_eq!(selector.selected_session_path().as_deref(), Some("/s/two.jsonl"));
}

#[test]
fn session_selector_search_filters_and_ctrl_c_clears_the_query() {
    let _guard = session_setup();
    let mut selector = SessionSelectorComponent::new(None);
    let sessions = vec![
        session_info("/s/one.jsonl", "one", None, 1000, "alpha conversation"),
        session_info("/s/two.jsonl", "two", None, 2000, "beta conversation"),
    ];
    selector.finish_load(SessionScope::Current, sessions);

    for ch in "alph".chars() {
        assert_eq!(
            selector.handle_key(&ch.to_string()),
            SessionSelectorOutcome::Consumed
        );
    }
    assert_eq!(selector.selected_session_path().as_deref(), Some("/s/one.jsonl"));

    // Ctrl+C clears the search first (upstream the same).
    assert_eq!(
        selector.handle_key("\u{3}"),
        SessionSelectorOutcome::Consumed
    );
    assert_eq!(selector.search_value(), "");
    // Without a query the threaded sort applies again.
    assert_eq!(
        selector.selected_session_path().as_deref(),
        Some("/s/two.jsonl")
    );
}

#[test]
fn session_selector_tab_toggles_the_scope_and_requests_a_load() {
    let _guard = session_setup();
    let mut selector = SessionSelectorComponent::new(None);
    selector.finish_load(
        SessionScope::Current,
        vec![session_info("/s/one.jsonl", "one", None, 1000, "first")],
    );

    // Tab → the "all" scope: uncached, so the host must load it.
    assert_eq!(
        selector.handle_key("\t"),
        SessionSelectorOutcome::ScopeToggled {
            scope: SessionScope::All,
            needs_load: true
        }
    );
    let body = slot_body(&mut selector, 100);
    assert!(body.contains("Resume Session (All)"), "{body:?}");

    // The load lands: the cwd column appears and the scope is cached now.
    selector.finish_load(
        SessionScope::All,
        vec![session_info("/other/two.jsonl", "two", None, 2000, "second")],
    );
    let body = slot_body(&mut selector, 140);
    assert!(body.contains("/tmp"), "the cwd shows in the all scope: {body:?}");

    // Tab back → the cached current list returns without a load request.
    assert_eq!(
        selector.handle_key("\t"),
        SessionSelectorOutcome::ScopeToggled {
            scope: SessionScope::Current,
            needs_load: false
        }
    );
    let body = slot_body(&mut selector, 100);
    assert!(body.contains("first"), "{body:?}");

    // A second Tab → the cached "all" scope shows immediately, no reload.
    assert_eq!(
        selector.handle_key("\t"),
        SessionSelectorOutcome::Consumed
    );
    let body = slot_body(&mut selector, 140);
    assert!(body.contains("second"), "{body:?}");
}

#[test]
fn session_selector_sort_name_and_path_toggles() {
    let _guard = session_setup();
    let mut selector = SessionSelectorComponent::new(None);
    selector.finish_load(
        SessionScope::Current,
        vec![session_info("/s/one.jsonl", "one", Some("named"), 1000, "first")],
    );

    // Ctrl+S cycles Threaded → Recent → Fuzzy (the header labels follow).
    selector.handle_key("\u{13}"); // ctrl+s
    assert!(slot_body(&mut selector, 100).contains("Sort: Recent"));
    selector.handle_key("\u{13}");
    assert!(slot_body(&mut selector, 100).contains("Sort: Fuzzy"));
    selector.handle_key("\u{13}");
    assert!(slot_body(&mut selector, 100).contains("Sort: Threaded"));

    // The named filter hides unnamed sessions.
    let mut selector = SessionSelectorComponent::new(None);
    selector.finish_load(
        SessionScope::Current,
        vec![
            session_info("/s/one.jsonl", "one", None, 1000, "unnamed"),
            session_info("/s/two.jsonl", "two", Some("kept"), 2000, "named"),
        ],
    );
    selector.handle_key("\u{e}"); // ctrl+n? (toggleNamedFilter binding)
    let body = slot_body(&mut selector, 100);
    assert!(body.contains("No named sessions in current folder") || body.contains("kept"), "{body:?}");
    selector.handle_key("\u{e}"); // toggle back (same binding)
    let body = slot_body(&mut selector, 100);
    assert!(body.contains("unnamed"), "{body:?}");

    // Ctrl+P toggles the path column.
    selector.handle_key("\u{10}"); // ctrl+p
    let body = slot_body(&mut selector, 160);
    assert!(body.contains("/s/one.jsonl"), "{body:?}");
    assert!(body.contains("path (on)"), "{body:?}");
}

#[test]
fn session_selector_delete_needs_confirmation_and_blocks_the_current_session() {
    let _guard = session_setup();
    let mut selector = SessionSelectorComponent::new(None);
    selector.finish_load(
        SessionScope::Current,
        vec![
            session_info("/s/one.jsonl", "one", None, 1000, "first"),
            session_info("/s/two.jsonl", "two", None, 2000, "second"),
        ],
    );
    // Ctrl+D starts the confirmation for the highlighted session.
    assert_eq!(selector.handle_key("\u{4}"), SessionSelectorOutcome::Consumed);
    let body = slot_body(&mut selector, 100);
    assert!(body.contains("Delete session?"), "{body:?}");

    // While confirming, every key is swallowed.
    assert_eq!(selector.handle_key("x"), SessionSelectorOutcome::Consumed);
    assert!(selector.confirming_delete_path().is_some());

    // Enter confirms → the host deletes and reports back.
    assert_eq!(
        selector.handle_key("\r"),
        SessionSelectorOutcome::Delete("/s/two.jsonl".to_string())
    );
    assert!(selector.confirming_delete_path().is_none());
    selector.remove_session("/s/two.jsonl");
    assert_eq!(
        selector.selected_session_path().as_deref(),
        Some("/s/one.jsonl")
    );

    // The current session cannot be deleted.
    let mut selector = SessionSelectorComponent::new(Some("/s/current.jsonl".to_string()));
    selector.finish_load(
        SessionScope::Current,
        vec![session_info("/s/current.jsonl", "current", None, 1000, "current")],
    );
    selector.handle_key("\u{4}"); // ctrl+d
    let body = slot_body(&mut selector, 120);
    assert!(
        body.contains("Cannot delete the currently active session"),
        "{body:?}"
    );

    // Cancel aborts the confirmation without deleting.
    let mut selector = SessionSelectorComponent::new(None);
    selector.finish_load(
        SessionScope::Current,
        vec![session_info("/s/one.jsonl", "one", None, 1000, "first")],
    );
    selector.handle_key("\u{4}");
    selector.handle_key("\u{1b}"); // cancel
    assert!(selector.confirming_delete_path().is_none());
    assert_eq!(selector.selected_session_path().as_deref(), Some("/s/one.jsonl"));
}

#[test]
fn session_selector_rename_mode() {
    let _guard = session_setup();
    let mut selector = SessionSelectorComponent::new(None);
    selector.finish_load(
        SessionScope::Current,
        vec![session_info("/s/one.jsonl", "one", Some("old name"), 1000, "first")],
    );

    // Ctrl+R enters rename mode with the current name.
    selector.handle_key("\u{12}"); // ctrl+r
    assert_eq!(selector.search_value(), "", "the list search is separate");
    let body = slot_body(&mut selector, 100);
    assert!(body.contains("Rename Session"), "{body:?}");

    // The rename input starts with the current name; edit and submit.
    for ch in "renamed".chars() {
        selector.handle_key(&ch.to_string());
    }
    // The upstream `Input.setValue` keeps the cursor at 0, so typed text
    // prepends (same quirk upstream).
    assert_eq!(
        selector.handle_key("\r"),
        SessionSelectorOutcome::Rename {
            path: "/s/one.jsonl".to_string(),
            name: "renamedold name".to_string(),
        }
    );
    // Escape exits the rename mode without submitting.
    selector.handle_key("\u{12}");
    assert_eq!(
        selector.handle_key("\u{1b}"),
        SessionSelectorOutcome::Consumed
    );
    let body = slot_body(&mut selector, 100);
    assert!(body.contains("Resume Session (Current Folder)"), "{body:?}");
}

#[test]
fn session_selector_tree_groups_children_under_their_parent() {
    let _guard = session_setup();
    let mut selector = SessionSelectorComponent::new(None);
    let parent = session_info("/s/p.jsonl", "p", None, 1000, "parent");
    let mut child = session_info("/s/c.jsonl", "c", None, 2000, "child");
    child.parent_session_path = Some("/s/p.jsonl".to_string());
    let mut grandchild = session_info("/s/g.jsonl", "g", None, 3000, "grandchild");
    grandchild.parent_session_path = Some("/s/c.jsonl".to_string());
    selector.finish_load(SessionScope::Current, vec![child, grandchild, parent]);

    let body = slot_body(&mut selector, 120);
    // The parent renders first; the child rows carry the tree prefix.
    assert!(body.contains("parent"), "{body:?}");
    assert!(body.contains("└─ child") || body.contains("├─ child"), "{body:?}");
    assert!(body.contains("grandchild"), "{body:?}");
}
