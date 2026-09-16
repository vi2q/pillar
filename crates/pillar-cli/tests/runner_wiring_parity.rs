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
    let wiring = build_extension_runner(
        &work.to_string_lossy(),
        None,
        None,
        &configured,
    );
    assert!(wiring.errors.is_empty(), "{:?}", wiring.errors);

    let written = std::fs::read_to_string(work.join("output.txt")).expect("fs.write wrote the file");
    assert_eq!(written, "HELLO! (true)");
    // The helper module is not an extension: only the writer has a setup
    // function, and only it registered anything.
    assert!(!wiring.runner.has_handlers("session_start"));
}
