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
