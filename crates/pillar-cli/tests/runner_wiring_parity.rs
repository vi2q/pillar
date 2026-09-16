//! Verifies the host wiring that joins the coding agent to the Luau
//! extension runtime: a configured `.luau` file is discovered, loaded, and
//! bridged into an `ExtensionRunner` the session can bind to.

use pillar_cli::runner::build_extension_runner;

fn temp_dir(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pillar-cli-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn build_extension_runner_loads_configured_luau_extensions() {
    let dir = temp_dir("ext");
    std::fs::write(
        dir.join("greet.luau"),
        r#"
        local pillar = require("@pillar")
        pillar.register_command("hello", { description = "Say hello" })
        pillar.on("session_start", function(event)
            return nil
        end)
        return nil
        "#,
    )
    .unwrap();

    let configured = vec![dir.to_string_lossy().to_string()];
    let mut wiring = build_extension_runner("", None, None, &configured);

    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);
    assert!(
        wiring.runner.has_handlers("session_start"),
        "extension handler bridged"
    );
    assert!(
        wiring
            .runner
            .registered_commands()
            .iter()
            .any(|command| command.name == "hello"),
        "extension command registered"
    );
    // The runtime must stay alive for the runner's bridges.
    assert!(std::sync::Arc::strong_count(&wiring.runtime) >= 1);
}

#[test]
fn build_extension_runner_without_paths_has_no_extensions() {
    let wiring = build_extension_runner("", None, None, &[]);
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);
    assert!(!wiring.runner.has_handlers("session_start"));
}

/// A registered Luau tool becomes a callable agent tool through the CLI
/// wiring (upstream the runner adding extension tools to the session).
#[tokio::test]
async fn configured_extensions_contribute_callable_tools() {
    let dir = temp_dir("tools");
    std::fs::write(
        dir.join("tools.luau"),
        r#"
        local pillar = require("@pillar")
        pillar.register_tool({
            name = "shout",
            label = "Shout",
            description = "Uppercase text",
            parameters = pillar.schema.object({ text = pillar.schema.string() }),
            execute = function(tool_call_id, params)
                return { content = { { type = "text", text = string.upper(params.text) } } }
            end,
        })
        return nil
        "#,
    )
    .unwrap();

    let configured = vec![dir.to_string_lossy().to_string()];
    let wiring = build_extension_runner("", None, None, &configured);
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);

    let tools = wiring.custom_tools();
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0].tool.name, "shout");
    assert_eq!(tools[0].label, "Shout");
    assert_eq!(
        tools[0].tool.parameters["properties"]["text"]["type"],
        serde_json::json!("string")
    );

    let result = (tools[0].execute)(
        "call-1".to_string(),
        serde_json::json!({ "text": "hi" }),
        None,
        None,
    )
    .await
    .expect("tool executes");
    assert_eq!(result.content[0], pillar_ai::types::Content::text("HI"));
}

/// Extensions can use the bounded `pillar.fs` API and share code through
/// `require("@ext/<name>")`; paths resolve against the session cwd.
#[test]
fn configured_extensions_can_use_fs_and_require_each_other() {
    let dir = temp_dir("fs");
    let work = temp_dir("fs-work");
    std::fs::write(
        dir.join("helper.luau"),
        r#"
        return { shout = function(text) return string.upper(text) end }
        "#,
    )
    .unwrap();
    std::fs::write(
        dir.join("writer.luau"),
        r#"
        local pillar = require("@pillar")
        local helper = require("@ext/helper")
        local existing = pillar.fs.read("input.txt")
        pillar.fs.write("output.txt", helper.shout(existing) .. "! (" .. tostring(pillar.fs.exists("input.txt")) .. ")")
        return nil
        "#,
    )
    .unwrap();
    std::fs::write(work.join("input.txt"), "hello").unwrap();

    let configured = vec![dir.to_string_lossy().to_string()];
    let wiring = build_extension_runner(&work.to_string_lossy(), None, None, &configured);
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);

    let written =
        std::fs::read_to_string(work.join("output.txt")).expect("fs.write wrote the file");
    assert_eq!(written, "HELLO! (true)");
    // The helper module is not an extension: only the writer has a setup
    // function, and only it registered anything.
    assert!(!wiring.runner.has_handlers("session_start"));
}

/// Extension renderers registered in Luau reach the runner the session
/// binds to: the message/entry renderers produce components and the
/// markdown transformer rewrites source.
#[test]
fn configured_extensions_register_renderers() {
    use pillar_coding_agent::core::extensions_types::{
        EntryRenderOptions, MarkdownMessageType, MarkdownTransformContext, MessageRenderOptions,
    };
    use pillar_coding_agent::core::messages::{CustomContent, CustomMessage};
    use pillar_coding_agent::core::session_entries::{CustomEntry, SessionEntryBase};
    use pillar_coding_agent::modes::interactive::theme;

    theme::init_theme(Some("dark"));

    let dir = temp_dir("renderers");
    std::fs::write(
        dir.join("renderers.luau"),
        r#"
        local pillar = require("@pillar")
        pillar.register_message_renderer("notice", function(message, options)
            return {
                lines = {
                    { { text = "NOTICE ", style = "accent" }, { text = message.details.title } },
                    "pad=" .. tostring(options.outputPad),
                },
            }
        end)
        pillar.register_entry_renderer("widget", function(entry)
            return "widget " .. tostring(entry.data.value)
        end)
        pillar.register_markdown_transformer(function(markdown, context)
            if context.messageType ~= "user" then return nil end
            return "> " .. markdown
        end)
        return nil
        "#,
    )
    .unwrap();

    let configured = vec![dir.to_string_lossy().to_string()];
    let wiring = build_extension_runner("", None, None, &configured);
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);

    let active = theme::theme();
    let message = CustomMessage {
        custom_type: "notice".to_string(),
        content: vec![CustomContent::Text("body".to_string())],
        display: true,
        details: Some(serde_json::json!({ "title": "deployed" })),
        timestamp: 0,
    };
    let renderer = wiring
        .runner
        .get_message_renderer("notice")
        .expect("message renderer registered");
    let rendered = renderer(
        &message,
        &MessageRenderOptions {
            expanded: false,
            output_pad: 4,
        },
        &active,
    )
    .expect("rendered lines")
    .join("\n");
    assert!(rendered.contains("NOTICE"), "{rendered:?}");
    assert!(rendered.contains("deployed"), "{rendered:?}");
    assert!(rendered.contains("pad=4"), "{rendered:?}");

    let entry = CustomEntry {
        base: SessionEntryBase {
            id: "e1".to_string(),
            ..Default::default()
        },
        custom_type: "widget".to_string(),
        data: Some(serde_json::json!({ "value": 11 })),
    };
    let entry_renderer = wiring
        .runner
        .get_entry_renderer("widget")
        .expect("entry renderer registered");
    let rendered = entry_renderer(&entry, &EntryRenderOptions { expanded: false }, &active)
        .expect("rendered lines")
        .join("\n");
    assert!(rendered.contains("widget 11"), "{rendered:?}");

    let transformers = wiring.runner.get_markdown_transformers();
    assert_eq!(transformers.len(), 1);
    let user = MarkdownTransformContext {
        message_type: MarkdownMessageType::User,
        is_streaming: false,
        available_width: 80,
    };
    assert_eq!(transformers[0]("hi", &user), Some("> hi".to_string()));
}

/// `ctx` reaches the handlers with the host facts, and `ctx.ui` forwards to
/// whatever bridge the interactive run installs.
#[test]
fn configured_extensions_receive_ctx_and_forward_ui_requests() {
    use std::sync::{Arc, Mutex};

    use pillar_coding_agent::core::extensions_types::ExtensionUiRequest;

    let dir = temp_dir("ctx");
    std::fs::write(
        dir.join("ctx.luau"),
        r#"
        local pillar = require("@pillar")
        pillar.on("session_start", function(event, ctx)
            print("CTX: " .. ctx.cwd .. " " .. ctx.mode .. " " .. tostring(ctx.hasUI))
            ctx.ui.notify("hello from ctx", "warning")
            ctx.ui.set_status("probe", "1")
            ctx.ui.set_editor_text("draft")
            return nil
        end)
        return nil
        "#,
    )
    .unwrap();

    let configured = vec![dir.to_string_lossy().to_string()];
    let wiring = build_extension_runner("/tmp/pillar-ctx-cwd", None, None, &configured);
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);

    // Before the run installs a bridge, ui requests are queued for replay
    // (`session_start` fires before the run loop starts).
    wiring
        .runner
        .emit(&serde_json::json!({ "type": "session_start" }));
    assert_eq!(
        wiring.ui_slot.lock().unwrap().pending.len(),
        3,
        "requests are queued until the mode installs its bridge"
    );

    let seen: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    wiring.ui_slot.lock().unwrap().bridge = Some(Arc::new(move |request| {
        sink.lock().unwrap().push(request);
        Ok(())
    }));
    wiring
        .runner
        .emit(&serde_json::json!({ "type": "session_start" }));

    let calls = seen.lock().unwrap();
    let ops: Vec<&str> = calls.iter().map(|call| call.op.as_str()).collect();
    assert_eq!(ops, vec!["notify", "set_status", "set_editor_text"]);
    assert_eq!(
        calls[0].args,
        serde_json::json!({ "message": "hello from ctx", "type": "warning" })
    );
    assert_eq!(
        calls[1].args,
        serde_json::json!({ "key": "probe", "text": "1" })
    );
    assert_eq!(calls[2].args, serde_json::json!({ "text": "draft" }));
}

/// A rebuild (`/reload`) re-discovers the extensions into a fresh VM while
/// keeping the host slots: the session binding, the `ctx.ui` bridge, the
/// `ctx` facts and the flag values survive.
#[test]
fn rebuild_re_discovers_extensions_into_a_fresh_vm() {
    use pillar_cli::runner::{ExtensionHostSlots, build_extension_runner_with_slots};

    let dir = temp_dir("rebuild");
    std::fs::write(
        dir.join("one.luau"),
        r#"
        local pillar = require("@pillar")
        pillar.register_command("alpha", { description = "first" })
        pillar.register_flag("probe", { type = "string" })
        return nil
        "#,
    )
    .unwrap();

    let configured = vec![dir.to_string_lossy().to_string()];
    let slots = ExtensionHostSlots::new("/tmp/pillar-rebuild-cwd");
    let mut wiring = build_extension_runner_with_slots(
        "/tmp/pillar-rebuild-cwd",
        None,
        None,
        &configured,
        &slots,
    );
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);
    let names = |runner: &mut pillar_coding_agent::core::extensions_runner::ExtensionRunner| {
        runner
            .registered_commands()
            .iter()
            .map(|command| command.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&mut wiring.runner), vec!["alpha".to_string()]);

    // Edit the extension, then rebuild like `/reload` does.
    std::fs::write(
        dir.join("one.luau"),
        r#"
        local pillar = require("@pillar")
        pillar.register_command("beta", { description = "second" })
        return nil
        "#,
    )
    .unwrap();
    let mut flags = std::collections::BTreeMap::new();
    flags.insert("probe".to_string(), serde_json::json!("on"));
    let mut rebuilt = wiring.rebuild(&flags);

    assert_eq!(names(&mut rebuilt.runner), vec!["beta".to_string()]);
    assert_eq!(
        rebuilt.runner.flag_values().get("probe"),
        Some(&serde_json::json!("on"))
    );
    // The host slots are the same cells, so an installed `ctx.ui` bridge and
    // the bound session keep working after the rebuild.
    assert!(std::sync::Arc::ptr_eq(&wiring.ui_slot, &rebuilt.ui_slot));
    assert!(std::sync::Arc::ptr_eq(
        &wiring.session_slot,
        &rebuilt.session_slot
    ));
    assert!(std::sync::Arc::ptr_eq(&wiring.context, &rebuilt.context));
    assert!(std::sync::Arc::ptr_eq(&wiring.data, &rebuilt.data));
}

/// An extension command reaches its Luau handler with the argument text and
/// `ctx` (the host handler the session installs).
#[test]
fn extension_commands_run_through_the_host_handler() {
    use pillar_cli::runner::extension_command_handler;

    let dir = temp_dir("command");
    std::fs::write(
        dir.join("cmd.luau"),
        r#"
        local pillar = require("@pillar")
        pillar.register_command("probe", {
            description = "Probe command",
            handler = function(args, ctx)
                local text = "args=" .. args .. "|idle=" .. tostring(ctx.isIdle()) .. "|mode=" .. ctx.mode
                pillar.fs.write("command-ran.txt", text)
                ctx.ui.notify("probe ran", "info")
            end,
        })
        return nil
        "#,
    )
    .unwrap();

    let work = temp_dir("command-work");
    let configured = vec![dir.to_string_lossy().to_string()];
    let wiring = build_extension_runner(&work.to_string_lossy(), None, None, &configured);
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);

    let handler = extension_command_handler(&wiring.runtime);
    // Unknown commands answer false so the session prompts instead.
    assert!(!handler("nope", "").unwrap());
    assert!(handler("probe", "one two").unwrap());
    // The handler saw the argument text and `ctx` (no session bound in this
    // test, so the facts are the defaults: idle, print mode).
    assert_eq!(
        std::fs::read_to_string(work.join("command-ran.txt")).unwrap(),
        "args=one two|idle=true|mode=print"
    );
}
