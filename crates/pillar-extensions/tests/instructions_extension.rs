//! The bundled `extensions/instructions.luau` (the Luau port of pi's
//! `pi-instructions-ext`): the file layer (templates, tidy) and the event
//! hooks (session pointer, per-turn tag, staleness, post-compact pointer).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pillar_coding_agent::core::extensions_types::{ExtensionContextFacts, ExtensionMode};
use pillar_extensions::runtime::{ExtensionRuntime, HostApi};

/// A temp working directory plus the scripted session the host callbacks
/// answer from.
struct Fixture {
    dir: PathBuf,
    runtime: ExtensionRuntime,
    /// Session entries `ctx.sessionManager.getEntries()` answers.
    entries: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pillar-instructions-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let entries: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));

        let runtime = ExtensionRuntime::new();
        let cwd = dir.clone();
        runtime.set_host_api(HostApi {
            fs: {
                let cwd = cwd.clone();
                Some(Arc::new(move |op, path, content| {
                    let resolved = cwd.join(path);
                    match op {
                        "read" => match std::fs::read_to_string(&resolved) {
                            Ok(text) => Ok(serde_json::Value::String(text)),
                            Err(_) => Ok(serde_json::Value::Null),
                        },
                        "write" => {
                            if let Some(parent) = resolved.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            std::fs::write(&resolved, content.unwrap_or_default())
                                .map(|_| serde_json::Value::Bool(true))
                                .map_err(|error| error.to_string())
                        }
                        "exists" => Ok(serde_json::Value::Bool(resolved.exists())),
                        "stat" => match std::fs::metadata(&resolved) {
                            Ok(metadata) => {
                                let modified_ms = metadata
                                    .modified()
                                    .ok()
                                    .and_then(|time| {
                                        time.duration_since(std::time::UNIX_EPOCH).ok()
                                    })
                                    .map(|duration| duration.as_millis() as u64)
                                    .unwrap_or(0);
                                Ok(serde_json::json!({
                                    "type": if metadata.is_dir() { "directory" } else { "file" },
                                    "size": metadata.len(),
                                    "modified_ms": modified_ms,
                                }))
                            }
                            Err(_) => Ok(serde_json::Value::Null),
                        },
                        other => Err(format!("unsupported op {other}")),
                    }
                }))
            },
            session_id: Some(Arc::new(|| {
                Some("0199a1b2-3456-7abc-8def-0123456789ab".to_string())
            })),
            session_entries: {
                let entries = Arc::clone(&entries);
                Some(Arc::new(move || {
                    serde_json::Value::Array(entries.lock().unwrap().clone())
                }))
            },
            context: Some(Arc::new(|| ExtensionContextFacts {
                cwd: "/tmp/instructions".to_string(),
                mode: ExtensionMode::Tui,
                has_ui: true,
            })),
            ui: Some(Arc::new(|_request| Ok(()))),
            ..Default::default()
        });
        let mut fixture = Self {
            dir,
            runtime,
            entries,
        };
        fixture.load();
        fixture
    }

    fn load(&mut self) {
        let source = std::fs::read_to_string(extension_path()).expect("extension source");
        self.runtime
            .type_check("instructions.luau", &source)
            .unwrap();
        let loaded = self
            .runtime
            .load_extension("instructions.luau", &source)
            .unwrap();
        self.runtime.run_setup(&loaded).unwrap();
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).expect("file")
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// Emit an event; `None` when the handler answered nothing.
    fn emit(&mut self, event: &str) -> Option<serde_json::Value> {
        match self
            .runtime
            .dispatch(event, serde_json::json!({ "type": event }))
            .expect("dispatch")
        {
            pillar_extensions::runtime::HandlerOutcome::Table(table) => Some(table),
            pillar_extensions::runtime::HandlerOutcome::None => None,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    /// Emit an event whose handler must answer a table.
    fn emit_table(&mut self, event: &str) -> serde_json::Value {
        self.emit(event).expect("handler answered a table")
    }

    /// The custom messages the extension sent (`pillar.send_message`).
    fn messages(&self) -> Vec<serde_json::Value> {
        self.runtime.registry().messages.clone()
    }

    /// Whether the verify walkthrough prompt was sent (`send_user_message`
    /// records the raw string; `send_message` records the table).
    fn sent_walkthrough(&self) -> bool {
        self.messages().iter().any(|message| {
            let text = match message {
                serde_json::Value::String(text) => Some(text.as_str()),
                other => other.get("content").and_then(serde_json::Value::as_str),
            };
            text.unwrap_or_default().contains("Walk through every item")
        })
    }

    /// The messages with a custom type, oldest first.
    fn messages_of(&self, custom_type: &str) -> Vec<serde_json::Value> {
        self.messages()
            .into_iter()
            .filter(|message| message["customType"] == serde_json::json!(custom_type))
            .collect()
    }
}

fn extension_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../extensions/instructions.luau")
        .canonicalize()
        .expect("extensions/instructions.luau exists")
}

/// `tasks_init` creates both templates and never overwrites an existing file.
#[test]
fn tasks_init_creates_the_templates_once() {
    let mut fixture = Fixture::new();
    assert!(!fixture.path("docs/TASKS.md").exists());

    let result = fixture
        .runtime
        .call_tool("tasks_init", "call-1", serde_json::json!({}))
        .expect("tasks_init");
    let text = result["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("created docs/TASKS.md (skeleton)"), "{text}");
    assert!(
        text.contains("created docs/RULES.md (recording rules)"),
        "{text}"
    );
    assert_eq!(
        result["details"],
        serde_json::json!({ "createdTasks": true, "createdRules": true })
    );
    assert_eq!(
        fixture.read("docs/TASKS.md"),
        "# TASKS\n\n<!-- Work-instruction record. See docs/RULES.md for conventions. -->\n\n## Active\n\n## Completed\n"
    );
    assert!(
        fixture
            .read("docs/RULES.md")
            .starts_with("# TASKS recording rules")
    );

    // A second call reports the existing files and leaves them alone.
    fixture.write("docs/TASKS.md", "hand written\n");
    let result = fixture
        .runtime
        .call_tool("tasks_init", "call-2", serde_json::json!({}))
        .expect("tasks_init");
    assert_eq!(
        result["details"],
        serde_json::json!({ "createdTasks": false, "createdRules": false })
    );
    assert_eq!(fixture.read("docs/TASKS.md"), "hand written\n");
}

/// `tasks_tidy` normalizes checkbox syntax, indentation and confirm prefixes
/// while preserving order, trailing continuations and unrecognized lines.
#[test]
fn tasks_tidy_normalizes_without_reordering() {
    let mut fixture = Fixture::new();
    fixture.write(
        "docs/TASKS.md",
        "# TASKS\n\nIntro prose stays.\n\n## Active\n\n* [X] Done item\n    * [ ] Nested child\n      - [ ] confirm(user): check the visual result\n+ [ ] Another item\n\nSome free-form line.\n\n## Completed\n\n- [x] Closed\n",
    );
    let result = fixture
        .runtime
        .call_tool("tasks_tidy", "call-1", serde_json::json!({}))
        .expect("tasks_tidy");
    let text = result["content"][0]["text"].as_str().expect("text");
    assert!(
        text.starts_with("Tidied docs/TASKS.md: 5 item(s)"),
        "{text}"
    );
    assert_eq!(result["details"]["status"], serde_json::json!("changed"));
    // Continuation lines travel with their item and stay untouched; the
    // nested checkbox item gets the normalized confirm prefix.
    assert_eq!(
        fixture.read("docs/TASKS.md"),
        "# TASKS\n\nIntro prose stays.\n\n## Active\n\n- [x] Done item\n  - [ ] Nested child\n    - [ ] Confirm (user): check the visual result\n- [ ] Another item\n\nSome free-form line.\n\n## Completed\n\n- [x] Closed\n"
    );

    // Idempotent: a second tidy changes nothing.
    let result = fixture
        .runtime
        .call_tool("tasks_tidy", "call-2", serde_json::json!({}))
        .expect("tasks_tidy");
    assert_eq!(result["details"]["status"], serde_json::json!("unchanged"));

    // A file without checklist items is left as is.
    fixture.write("docs/TASKS.md", "# TASKS\n\nnothing to do\n");
    let result = fixture
        .runtime
        .call_tool("tasks_tidy", "call-3", serde_json::json!({}))
        .expect("tasks_tidy");
    assert_eq!(result["details"]["status"], serde_json::json!("no-items"));
    assert_eq!(fixture.read("docs/TASKS.md"), "# TASKS\n\nnothing to do\n");

    // A missing file is reported, not created.
    std::fs::remove_file(fixture.path("docs/TASKS.md")).unwrap();
    let result = fixture
        .runtime
        .call_tool("tasks_tidy", "call-4", serde_json::json!({}))
        .expect("tasks_tidy");
    assert_eq!(result["details"]["status"], serde_json::json!("missing"));
}

/// `session_start` injects the pointer once (a second emit dedups through the
/// session entries) and records the staleness baseline.
#[test]
fn session_start_injects_the_pointer_once() {
    let mut fixture = Fixture::new();
    fixture.emit("session_start");
    let messages = fixture.messages_of("instructions:init");
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(
        messages[0]["customType"],
        serde_json::json!("instructions:init")
    );
    assert_eq!(messages[0]["display"], serde_json::json!(true));
    let content = messages[0]["content"].as_str().expect("content");
    // No files yet: the pointer asks for tasks_init.
    assert!(content.contains("call the tasks_init tool"), "{content}");
    assert!(content.contains("(s89ab)"), "{content}");

    // The session records the pointer; a re-fire (/reload) stays quiet.
    fixture.entries.lock().unwrap().push(serde_json::json!({
        "type": "custom_message",
        "customType": "instructions:init",
    }));
    fixture.emit("session_start");
    assert_eq!(fixture.messages_of("instructions:init").len(), 1);
}

/// Once both files exist the pointer switches to the "read the rules" form,
/// and the per-turn tag appears (with the staleness variant when the file
/// changed on disk behind the agent's back).
#[test]
fn pointer_and_per_turn_tag_track_file_state() {
    let mut fixture = Fixture::new();
    fixture.emit("session_start");
    // Only TASKS.md exists: still the setup pointer.
    fixture.write("docs/TASKS.md", "# TASKS\n");
    fixture.write("docs/RULES.md", "# rules\n");
    fixture.emit("session_start");
    let content = fixture
        .messages_of("instructions:init")
        .last()
        .and_then(|message| message["content"].as_str().map(str::to_string))
        .expect("pointer");
    assert!(
        content.contains("Start by reading docs/RULES.md"),
        "{content}"
    );
    assert!(content.contains("(s89ab)"), "{content}");

    // The first turn establishes the baseline and answers the plain tag.
    let outcome = fixture.emit_table("before_agent_start");
    assert_eq!(
        outcome["message"]["customType"],
        serde_json::json!("instructions:auto-tag")
    );
    assert_eq!(
        outcome["message"]["content"],
        serde_json::json!("Auto-message: Please update docs/TASKS.md as needed.")
    );
    assert_eq!(outcome["message"]["display"], serde_json::json!(false));

    // An external edit (a parallel session) makes the next turn warn.
    fixture.write("docs/TASKS.md", "# TASKS\n\n- [ ] external edit\n");
    let outcome = fixture.emit_table("before_agent_start");
    let stale = outcome["message"]["content"].as_str().unwrap();
    assert!(stale.contains("has changed on disk"), "{stale}");
    // One-shot: the following turn is back to the plain tag.
    let outcome = fixture.emit_table("before_agent_start");
    assert_eq!(
        outcome["message"]["content"],
        serde_json::json!("Auto-message: Please update docs/TASKS.md as needed.")
    );

    // A rules change takes priority over the tasks tag.
    fixture.emit("turn_end");
    fixture.write("docs/RULES.md", "# rules\n\n- [ ] new convention\n");
    let outcome = fixture.emit_table("before_agent_start");
    let rules = outcome["message"]["content"].as_str().unwrap();
    assert!(
        rules.contains("docs/RULES.md has changed on disk"),
        "{rules}"
    );
}

/// `session_compact` re-injects the pointer only when the tasks file exists.
#[test]
fn session_compact_reinjects_the_pointer() {
    let mut fixture = Fixture::new();
    fixture.emit("session_compact");
    assert!(fixture.messages_of("instructions:post-compact").is_empty());

    fixture.write("docs/TASKS.md", "# TASKS\n");
    fixture.emit("session_compact");
    let messages = fixture.messages_of("instructions:post-compact");
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0]["customType"],
        serde_json::json!("instructions:post-compact")
    );
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("re-read docs/TASKS.md")
    );
    assert_eq!(messages[0]["display"], serde_json::json!(false));
}

/// The ported commands run with `(args, ctx)` and see the host facts.
#[test]
fn commands_execute_with_ctx() {
    let mut fixture = Fixture::new();
    assert!(fixture.runtime.call_command("tasks-info", "").unwrap());
    assert!(fixture.runtime.call_command("tasks-init", "").unwrap());

    // `tasks-init` created the templates (nothing existed yet) and notified.
    assert!(fixture.path("docs/TASKS.md").exists());
    assert!(fixture.path("docs/RULES.md").exists());

    // An unknown command answers false (the session then prompts instead).
    assert!(!fixture.runtime.call_command("nope", "").unwrap());

    // `tasks-verify` refuses when the tasks file is missing…
    std::fs::remove_file(fixture.path("docs/TASKS.md")).unwrap();
    assert!(fixture.runtime.call_command("tasks-verify", "").unwrap());
    assert!(!fixture.sent_walkthrough());

    // …and sends the walkthrough message (a user message) when it exists.
    fixture.write("docs/TASKS.md", "# TASKS\n");
    assert!(fixture.runtime.call_command("tasks-verify", "").unwrap());
    assert!(fixture.sent_walkthrough());
}
