//! Parity tests for the tool render layer (pi v0.84.3): the per-tool
//! `format*Call` / `format*Result` helpers and the renderer registry
//! (upstream `createAllToolDefinitions`' renderers).

use std::sync::Mutex;

use pillar_ai::types::Content;
use pillar_coding_agent::core::tools::render_definitions::{
    BashToolRenderer, EditToolRenderer, GrepToolRenderer, LsToolRenderer, ReadToolRenderer,
    ToolRenderContext, ToolRenderResult, ToolRenderResultOptions, ToolRenderShell, ToolRenderer,
    WriteToolRenderer, create_all_tool_renderers, create_tool_renderer, format_compact_read_call,
    format_duration, format_edit_call, format_edit_result, format_find_call, format_find_result,
    format_grep_call, format_grep_result, format_ls_call, format_ls_result, format_read_call,
    format_read_line_range, format_read_result, format_shell_call, format_write_call,
    format_write_result, get_compact_read_classification,
};
use pillar_coding_agent::modes::interactive::theme;

static THEME_LOCK: Mutex<()> = Mutex::new(());

fn dark() -> theme::Theme {
    theme::init_theme(Some("dark"));
    theme::get_theme_by_name("dark").expect("dark theme")
}

fn strip(text: &str) -> String {
    pillar_tui::text_utils::strip_terminal_sequences(text)
}

fn text_content(text: &str) -> Vec<Content> {
    vec![Content::text(text)]
}

fn options(expanded: bool) -> ToolRenderResultOptions {
    ToolRenderResultOptions {
        expanded,
        is_partial: false,
    }
}

fn context(args: serde_json::Value, cwd: &str) -> ToolRenderContext {
    ToolRenderContext {
        args,
        tool_call_id: "call-1".to_string(),
        cwd: cwd.to_string(),
        execution_started: false,
        args_complete: true,
        is_partial: false,
        expanded: false,
        show_images: true,
        is_error: false,
    }
}

// --- read -----------------------------------------------------------------------------------------

#[test]
fn read_call_and_line_range() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();

    // No offset/limit → no range suffix.
    assert_eq!(
        format_read_line_range(&serde_json::json!({"path": "a.rs"}), &theme_handle),
        ""
    );
    let range = format_read_line_range(
        &serde_json::json!({"path": "a.rs", "offset": 10, "limit": 5}),
        &theme_handle,
    );
    assert_eq!(strip(&range), ":10-14");
    assert!(
        range.contains(&theme_handle.fg_ansi("warning")),
        "{range:?}"
    );

    let call = format_read_call(
        &serde_json::json!({"file_path": "/tmp/a.rs"}),
        &theme_handle,
        "/tmp",
    );
    assert!(
        strip(&call).starts_with("read /tmp/a.rs"),
        "{:?}",
        strip(&call)
    );
    // A nullish path has no file to name; a non-string one is an invalid
    // argument.
    let missing = format_read_call(&serde_json::json!({}), &theme_handle, "/tmp");
    assert_eq!(strip(&missing), "read ...");
    let invalid = format_read_call(&serde_json::json!({"file_path": 42}), &theme_handle, "/tmp");
    assert!(
        strip(&invalid).contains("[invalid arg]"),
        "{:?}",
        strip(&invalid)
    );
}

#[test]
fn read_result_previews_and_reports_truncation() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let args = serde_json::json!({"file_path": "/tmp/a.txt"});

    // Collapsed without an error renders nothing (the call line is enough).
    let details = serde_json::json!({});
    let result = ToolRenderResult {
        content: &text_content("one\ntwo"),
        details: &details,
        is_error: false,
    };
    assert_eq!(
        format_read_result(&args, &result, &options(false), &theme_handle, true, false),
        ""
    );

    // Expanded renders every line after a blank line.
    let body = format_read_result(&args, &result, &options(true), &theme_handle, true, false);
    assert!(strip(&body).starts_with("\none\n"), "{:?}", strip(&body));
    assert!(strip(&body).contains("two"), "{:?}", strip(&body));

    // Collapsed with more than ten lines previews ten and hints.
    let long: String = (1..=15)
        .map(|index| format!("line{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let long_content = text_content(&long);
    let result = ToolRenderResult {
        content: &long_content,
        details: &details,
        is_error: false,
    };
    let body = format_read_result(&args, &result, &options(false), &theme_handle, true, true);
    assert!(strip(&body).contains("line1"), "{:?}", strip(&body));
    assert!(
        strip(&body).contains("... (5 more lines,"),
        "{:?}",
        strip(&body)
    );
    assert!(strip(&body).contains("to expand)"), "{:?}", strip(&body));

    // The truncation footer follows the details.
    let truncation = serde_json::json!({
        "truncation": {
            "truncated": true,
            "truncatedBy": "lines",
            "outputLines": 2000,
            "totalLines": 5000,
            "maxLines": 2000,
        }
    });
    let result = ToolRenderResult {
        content: &text_content("partial"),
        details: &truncation,
        is_error: true,
    };
    let body = format_read_result(&args, &result, &options(true), &theme_handle, true, true);
    assert!(
        strip(&body).contains("[Truncated: showing 2000 of 5000 lines (2000 line limit)]"),
        "{:?}",
        strip(&body)
    );

    // The first-line-exceeds-limit case reports the byte limit.
    let first_line = serde_json::json!({
        "truncation": {
            "truncated": true,
            "firstLineExceedsLimit": true,
            "maxBytes": 51200,
        }
    });
    let result = ToolRenderResult {
        content: &text_content("x"),
        details: &first_line,
        is_error: true,
    };
    let body = format_read_result(&args, &result, &options(true), &theme_handle, true, true);
    assert!(
        strip(&body).contains("[First line exceeds 50.0KB limit]"),
        "{:?}",
        strip(&body)
    );
}

#[test]
fn compact_read_classification_matches_upstream() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let cwd = "/work/project";

    // A skill file collapses to its directory name.
    let classification = get_compact_read_classification(
        &serde_json::json!({"file_path": "/skills/commit/SKILL.md"}),
        cwd,
    )
    .expect("skill classification");
    assert_eq!(classification.kind, "skill");
    assert_eq!(classification.label, "commit");
    let call = format_compact_read_call(&classification, &serde_json::json!({}), &theme_handle);
    assert!(
        strip(&call).starts_with("[skill] commit"),
        "{:?}",
        strip(&call)
    );

    // Resource files inside the cwd get the relative label.
    let classification = get_compact_read_classification(
        &serde_json::json!({"path": "/work/project/AGENTS.md"}),
        cwd,
    )
    .expect("resource classification");
    assert_eq!(classification.kind, "resource");
    assert_eq!(classification.label, "AGENTS.md");
    let call = format_compact_read_call(&classification, &serde_json::json!({}), &theme_handle);
    assert!(
        strip(&call).starts_with("read resource AGENTS.md"),
        "{:?}",
        strip(&call)
    );

    // Ordinary files have no compact form.
    assert!(
        get_compact_read_classification(
            &serde_json::json!({"path": "/work/project/src/main.rs"}),
            cwd
        )
        .is_none()
    );
    // Missing/empty paths classify to nothing.
    assert!(get_compact_read_classification(&serde_json::json!({}), cwd).is_none());
    assert!(get_compact_read_classification(&serde_json::json!({"path": ""}), cwd).is_none());
}

#[test]
fn read_renderer_prefers_the_compact_call_when_collapsed() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let mut renderer = ReadToolRenderer;
    let args = serde_json::json!({"file_path": "/work/project/AGENTS.md"});

    let collapsed = renderer.render_call(
        80,
        &args,
        &theme_handle,
        &context(args.clone(), "/work/project"),
    );
    assert!(
        strip(&collapsed[0]).starts_with("read resource"),
        "{:?}",
        collapsed
    );

    let mut expanded_context = context(args.clone(), "/work/project");
    expanded_context.expanded = true;
    let expanded = renderer.render_call(80, &args, &theme_handle, &expanded_context);
    assert!(
        strip(&expanded[0]).starts_with("read /work/project/AGENTS.md"),
        "{:?}",
        expanded
    );
}

// --- write ----------------------------------------------------------------------------------------

#[test]
fn write_call_previews_content_and_flags_bad_arguments() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let content: String = (1..=12)
        .map(|index| format!("line{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let call = format_write_call(
        &serde_json::json!({"path": "/tmp/out.txt", "content": content}),
        &options(false),
        &theme_handle,
        "/tmp",
    );
    assert!(
        strip(&call).starts_with("write /tmp/out.txt\n\nline1"),
        "{:?}",
        strip(&call)
    );
    assert!(
        strip(&call).contains("... (2 more lines, 12 total,"),
        "{:?}",
        strip(&call)
    );

    let expanded = format_write_call(
        &serde_json::json!({"path": "/tmp/out.txt", "content": "only\n"}),
        &options(true),
        &theme_handle,
        "/tmp",
    );
    assert!(strip(&expanded).contains("only"), "{:?}", strip(&expanded));
    assert!(
        !strip(&expanded).contains("more lines"),
        "{:?}",
        strip(&expanded)
    );

    // A nullish content argument shows only the path.
    let missing = format_write_call(
        &serde_json::json!({"path": "/tmp/out.txt"}),
        &options(false),
        &theme_handle,
        "/tmp",
    );
    assert_eq!(strip(&missing), "write /tmp/out.txt");
    // A non-string content argument is called out.
    let invalid = format_write_call(
        &serde_json::json!({"path": "/tmp/out.txt", "content": 5}),
        &options(false),
        &theme_handle,
        "/tmp",
    );
    assert!(
        strip(&invalid).contains("[invalid content arg - expected string]"),
        "{:?}",
        strip(&invalid)
    );
}

#[test]
fn write_result_only_renders_errors() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let details = serde_json::json!({});
    let content = text_content("Successfully wrote 3 bytes");

    let ok = ToolRenderResult {
        content: &content,
        details: &details,
        is_error: false,
    };
    assert_eq!(format_write_result(&ok, &theme_handle), None);

    let failed = ToolRenderResult {
        content: &content,
        details: &details,
        is_error: true,
    };
    let rendered = format_write_result(&failed, &theme_handle).expect("error text");
    assert_eq!(strip(&rendered), "\nSuccessfully wrote 3 bytes");
    assert!(rendered.contains(&theme_handle.fg_ansi("error")));

    // An error with no text renders nothing.
    let empty: Vec<Content> = Vec::new();
    let failed = ToolRenderResult {
        content: &empty,
        details: &details,
        is_error: true,
    };
    assert_eq!(format_write_result(&failed, &theme_handle), None);
}

// --- grep / find / ls -----------------------------------------------------------------------------

#[test]
fn grep_and_find_calls_render_pattern_and_scope() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();

    let grep = format_grep_call(
        &serde_json::json!({"pattern": "needle", "path": "/tmp", "glob": "*.rs", "limit": 20}),
        &theme_handle,
    );
    let plain = strip(&grep);
    assert!(plain.starts_with("grep /needle/ in /tmp"), "{plain:?}");
    assert!(plain.contains(" (*.rs)"), "{plain:?}");
    assert!(plain.contains(" limit 20"), "{plain:?}");

    // An empty path defaults to ".".
    let grep = format_grep_call(
        &serde_json::json!({"pattern": "x", "path": ""}),
        &theme_handle,
    );
    assert!(strip(&grep).contains(" in ."), "{:?}", strip(&grep));
    // A nullish pattern coerces to "" (upstream `str`), so it renders `//`;
    // only a non-string argument is an invalid argument.
    let grep = format_grep_call(&serde_json::json!({}), &theme_handle);
    assert!(strip(&grep).starts_with("grep //"), "{:?}", strip(&grep));
    let grep = format_grep_call(&serde_json::json!({"pattern": 7}), &theme_handle);
    assert!(strip(&grep).contains("[invalid arg]"), "{:?}", strip(&grep));

    let find = format_find_call(
        &serde_json::json!({"pattern": "*.rs", "path": "/tmp", "limit": 5}),
        &theme_handle,
    );
    let plain = strip(&find);
    assert!(plain.starts_with("find *.rs in /tmp"), "{plain:?}");
    assert!(plain.contains(" (limit 5)"), "{plain:?}");

    let ls = format_ls_call(&serde_json::json!({"limit": 3}), &theme_handle, "/tmp");
    assert!(strip(&ls).starts_with("ls ."), "{:?}", strip(&ls));
    assert!(strip(&ls).contains(" (limit 3)"), "{:?}", strip(&ls));
}

#[test]
fn result_previews_share_the_same_shape() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let details = serde_json::json!({});
    let long: String = (1..=25)
        .map(|index| format!("hit{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let content = text_content(&long);
    let result = ToolRenderResult {
        content: &content,
        details: &details,
        is_error: false,
    };

    // grep previews 15, find and ls preview 20 (upstream).
    let grep = strip(&format_grep_result(
        &result,
        &options(false),
        &theme_handle,
        true,
    ));
    assert!(grep.contains("... (10 more lines,"), "{grep:?}");
    let find = strip(&format_find_result(
        &result,
        &options(false),
        &theme_handle,
        true,
    ));
    assert!(find.contains("... (5 more lines,"), "{find:?}");
    let ls = strip(&format_ls_result(
        &result,
        &options(false),
        &theme_handle,
        true,
    ));
    assert!(ls.contains("... (5 more lines,"), "{ls:?}");
    // Expanded shows everything.
    let expanded = strip(&format_ls_result(
        &result,
        &options(true),
        &theme_handle,
        true,
    ));
    assert!(!expanded.contains("more lines"), "{expanded:?}");
    assert!(expanded.contains("hit25"), "{expanded:?}");
}

#[test]
fn limit_warnings_name_the_reached_limit() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let content = text_content("one");

    let grep_details = serde_json::json!({
        "matchLimitReached": 100,
        "truncation": {"truncated": true, "maxBytes": 51200},
        "linesTruncated": true,
    });
    let result = ToolRenderResult {
        content: &content,
        details: &grep_details,
        is_error: false,
    };
    let text = strip(&format_grep_result(
        &result,
        &options(false),
        &theme_handle,
        true,
    ));
    assert!(
        text.contains("[Truncated: 100 matches limit, 50.0KB limit, some lines truncated]"),
        "{text:?}"
    );

    let find_details = serde_json::json!({"resultLimitReached": 200});
    let result = ToolRenderResult {
        content: &content,
        details: &find_details,
        is_error: false,
    };
    let text = strip(&format_find_result(
        &result,
        &options(false),
        &theme_handle,
        true,
    ));
    assert!(text.contains("[Truncated: 200 results limit]"), "{text:?}");

    let ls_details = serde_json::json!({"entryLimitReached": 500});
    let result = ToolRenderResult {
        content: &content,
        details: &ls_details,
        is_error: false,
    };
    let text = strip(&format_ls_result(
        &result,
        &options(false),
        &theme_handle,
        true,
    ));
    assert!(text.contains("[Truncated: 500 entries limit]"), "{text:?}");
}

// --- edit -----------------------------------------------------------------------------------------

#[test]
fn edit_call_and_result_render_the_diff() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let args = serde_json::json!({"file_path": "/tmp/a.rs"});
    let call = format_edit_call(&args, &theme_handle, "/tmp");
    assert!(
        strip(&call).starts_with("edit /tmp/a.rs"),
        "{:?}",
        strip(&call)
    );

    let details = serde_json::json!({"diff": " 1 context\n-2 old\n+2 new"});
    let content = text_content("Successfully replaced 1 block(s) in /tmp/a.rs.");
    let result = ToolRenderResult {
        content: &content,
        details: &details,
        is_error: false,
    };
    let rendered = format_edit_result(&args, &result, &theme_handle, false).expect("diff");
    let plain = strip(&rendered);
    assert!(plain.contains("-2 old"), "{plain:?}");
    assert!(plain.contains("+2 new"), "{plain:?}");
    assert!(plain.contains("context"), "{plain:?}");

    // Errors render the error text instead of a diff.
    let failing = ToolRenderResult {
        content: &text_content("old_text not found"),
        details: &serde_json::json!({}),
        is_error: true,
    };
    let rendered = format_edit_result(&args, &failing, &theme_handle, true).expect("error");
    assert_eq!(strip(&rendered), "old_text not found");
    assert!(rendered.contains(&theme_handle.fg_ansi("error")));

    // No diff and no error → nothing.
    let empty = ToolRenderResult {
        content: &text_content("done"),
        details: &serde_json::json!({}),
        is_error: false,
    };
    assert_eq!(
        format_edit_result(&args, &empty, &theme_handle, false),
        None
    );
}

// --- bash -----------------------------------------------------------------------------------------

#[test]
fn shell_call_and_duration_formatting() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let call = format_shell_call(
        &serde_json::json!({"command": "ls -la"}),
        "!",
        &theme_handle,
    );
    assert!(strip(&call).starts_with("! ls -la"), "{:?}", strip(&call));

    let with_timeout = format_shell_call(
        &serde_json::json!({"command": "sleep 1", "timeout": 5}),
        "!",
        &theme_handle,
    );
    assert!(
        strip(&with_timeout).contains("(timeout 5s)"),
        "{:?}",
        strip(&with_timeout)
    );

    // A nullish or empty command shows the placeholder; a non-string one is
    // an invalid argument.
    let empty = format_shell_call(&serde_json::json!({}), "!", &theme_handle);
    assert!(strip(&empty).ends_with("! ..."), "{:?}", strip(&empty));
    let empty = format_shell_call(&serde_json::json!({"command": ""}), "!", &theme_handle);
    assert!(strip(&empty).ends_with("! ..."), "{:?}", strip(&empty));
    let invalid = format_shell_call(&serde_json::json!({"command": 5}), "!", &theme_handle);
    assert!(
        strip(&invalid).contains("[invalid arg]"),
        "{:?}",
        strip(&invalid)
    );

    assert_eq!(format_duration(0.0), "0.0s");
    assert_eq!(format_duration(1234.0), "1.2s");
    assert_eq!(format_duration(5600.0), "5.6s");
}

#[test]
fn bash_renderer_tracks_timings_and_truncation() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let mut renderer = BashToolRenderer::new("!");
    let args = serde_json::json!({"command": "ls"});

    let mut partial_context = context(args.clone(), "/tmp");
    partial_context.execution_started = true;
    partial_context.is_partial = true;
    renderer.render_call(80, &args, &theme_handle, &partial_context);
    assert!(renderer.started_at().is_some(), "start time recorded");

    let details = serde_json::json!({
        "truncation": {"truncated": true, "truncatedBy": "lines", "outputLines": 2000, "totalLines": 4000},
        "fullOutputPath": "/tmp/full.log",
    });
    let content = text_content("output line");
    let result = ToolRenderResult {
        content: &content,
        details: &details,
        is_error: false,
    };
    let partial = renderer
        .render_result(
            80,
            &result,
            &ToolRenderResultOptions {
                expanded: true,
                is_partial: true,
            },
            &theme_handle,
            &partial_context,
        )
        .expect("partial result");
    let text = partial.join("\n");
    assert!(strip(&text).contains("Elapsed "), "{:?}", strip(&text));
    assert!(renderer.ended_at().is_none(), "still running");

    let finished = renderer
        .render_result(80, &result, &options(true), &theme_handle, &partial_context)
        .expect("finished result");
    let text = strip(&finished.join("\n"));
    assert!(text.contains("Took "), "{text:?}");
    assert!(renderer.ended_at().is_some(), "finished");
    assert!(
        text.contains("[Full output: /tmp/full.log. Truncated: showing 2000 of 4000 lines]"),
        "{text:?}"
    );

    // A collapsed partial result previews the tail and caches per width.
    renderer.invalidate();
    let long: String = (1..=30)
        .map(|index| format!("out{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let long_content = text_content(&long);
    let result = ToolRenderResult {
        content: &long_content,
        details: &serde_json::json!({}),
        is_error: false,
    };
    let collapsed = renderer
        .render_result(
            80,
            &result,
            &ToolRenderResultOptions {
                expanded: false,
                is_partial: true,
            },
            &theme_handle,
            &partial_context,
        )
        .expect("collapsed");
    let text = strip(&collapsed.join("\n"));
    assert!(text.contains("earlier lines,"), "{text:?}");
    assert!(text.contains("out30"), "{text:?}");
}

// --- registry -------------------------------------------------------------------------------------

#[test]
fn renderer_registry_covers_the_built_in_tools() {
    let _guard = THEME_LOCK.lock().expect("theme lock");
    let theme_handle = dark();
    let renderers = create_all_tool_renderers("/tmp");
    let names: Vec<&str> = renderers.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        vec!["bash", "edit", "find", "grep", "ls", "read", "write"]
    );

    // Unknown tools have no renderer (the component then uses its fallback).
    assert!(create_tool_renderer("nope", "/tmp").is_none());
    // Every built-in renderer answers a call line. Only edit frames itself
    // (upstream edit.ts `renderShell: "self"`); the rest use the default
    // background box.
    for (name, mut renderer) in renderers {
        let expected_shell = if name == "edit" {
            ToolRenderShell::SelfRendered
        } else {
            ToolRenderShell::Default
        };
        assert_eq!(renderer.render_shell(), expected_shell);
        let args = serde_json::json!({
            "command": "ls",
            "path": "/tmp",
            "file_path": "/tmp/a.txt",
            "pattern": "x",
            "content": "hi",
        });
        let rendered =
            renderer.render_call(80, &args, &theme_handle, &context(args.clone(), "/tmp"));
        assert!(!rendered.is_empty(), "{name} rendered nothing");
    }

    // Purpose-built renderers are constructible directly (used by hosts that
    // know the tool).
    let mut grep = GrepToolRenderer;
    let mut write = WriteToolRenderer;
    let mut edit = EditToolRenderer;
    let mut ls = LsToolRenderer;
    let mut read = ReadToolRenderer;
    for renderer in [
        &mut grep as &mut dyn ToolRenderer,
        &mut write,
        &mut edit,
        &mut ls,
        &mut read,
    ] {
        renderer.invalidate();
    }
    let _ = theme_handle;
}
