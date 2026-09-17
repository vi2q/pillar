//! The production build scenario, driven end to end against the real bundled
//! `extensions/build.luau` and *real* child processes.
//!
//! This is the build half of the path the policy review asks for
//! (docs/POLICY-REVIEW-sb39f.md: 調査→変更→build/preview→観測→結果照合→中断/再開)
//! and the ledger's `build-with-progress-interrupt-resume` scenario: a plan of
//! steps runs, progress is reported per step, a failing step is not success, an
//! aborted call kills the running process, and the next run resumes from what is
//! already on disk.
//!
//! The commands are real (`sh -c …` in a temp directory), so the interruption
//! test really kills a process rather than pretending to. No model is involved:
//! the tool is called directly, which is what makes the scenario deterministic.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pillar_agent::AbortSignal;
use pillar_extensions::runtime::{ExecHost, ExtensionRuntime, HostApi};
use pillar_extensions_contract::{ExecOptions, ExecResult};

/// A temp working directory plus the hosts the extension runs against.
struct Fixture {
    dir: PathBuf,
    runtime: ExtensionRuntime,
    /// Every command the exec host actually spawned (the resume test asserts a
    /// skipped step does not run).
    commands: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pillar-build-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let commands: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let runtime = ExtensionRuntime::new();
        runtime.set_host_api(HostApi {
            fs: {
                let cwd = dir.clone();
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
                            Ok(metadata) => Ok(serde_json::json!({
                                "type": if metadata.is_dir() { "directory" } else { "file" },
                                "size": metadata.len(),
                                "modified_ms": 0,
                            })),
                            Err(_) => Ok(serde_json::Value::Null),
                        },
                        other => Err(format!("unsupported op {other}")),
                    }
                }))
            },
            ..Default::default()
        });
        runtime.set_exec_host(real_exec(Arc::clone(&commands), dir.clone()));

        let mut fixture = Self {
            dir,
            runtime,
            commands,
        };
        let source = std::fs::read_to_string(extension_path()).expect("extensions/build.luau");
        fixture
            .runtime
            .load_extension("/ext/build.luau", &source)
            .expect("the build extension loads");
        fixture
    }

    /// A step plan: `steps` is the JSON array the tool's schema takes.
    fn call(&mut self, args: serde_json::Value, signal: Option<AbortSignal>) -> serde_json::Value {
        self.runtime
            .call_tool("build", "call-1", args, signal, None)
            .expect("the build tool returns a result")
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.join(relative)
    }

    fn commands_run(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

fn extension_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../extensions/build.luau")
        .canonicalize()
        .expect("extensions/build.luau exists")
}

/// A real exec host: spawn the command, and while it runs watch the abort
/// signal, killing the process when it fires. This is the host half of "an
/// interrupt stops the build" — the guest cannot kill its own child.
fn real_exec(commands: Arc<Mutex<Vec<String>>>, cwd: PathBuf) -> ExecHost {
    Arc::new(move |command: &str, args: &[String], options: &ExecOptions| {
        commands
            .lock()
            .unwrap()
            .push(format!("{command} {}", args.join(" ")));
        let directory = options.cwd.clone().map_or_else(|| cwd.clone(), PathBuf::from);
        let mut child = match std::process::Command::new(command)
            .args(args)
            .current_dir(&directory)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => return ExecResult::spawn_failure(&error.to_string()),
        };

        let mut killed = false;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => return ExecResult::spawn_failure(&error.to_string()),
            }
            if options.signal.as_ref().is_some_and(|s| s.is_aborted()) {
                let _ = child.kill();
                killed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        match child.wait_with_output() {
            Ok(output) => ExecResult {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                code: output.status.code().unwrap_or(-1),
                killed,
                truncated: false,
            },
            Err(error) => ExecResult::spawn_failure(&error.to_string()),
        }
    })
}

fn step(name: &str, script: &str, artifact: Option<&str>) -> serde_json::Value {
    let mut step = serde_json::json!({
        "name": name,
        "command": "sh",
        "args": ["-c", script],
    });
    if let Some(artifact) = artifact {
        step["artifact"] = serde_json::Value::String(artifact.to_owned());
    }
    step
}

fn statuses(result: &serde_json::Value) -> Vec<(String, String)> {
    result["details"]["results"]
        .as_array()
        .expect("the tool reports per-step results")
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap_or_default().to_owned(),
                entry["status"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[test]
fn a_build_reports_progress_and_a_result_to_match() {
    let mut fixture = Fixture::new();
    let updates: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&updates);
    let result = fixture
        .runtime
        .call_tool(
            "build",
            "call-1",
            serde_json::json!({
                "steps": [
                    step("prepare", "printf one > one.txt", Some("one.txt")),
                    step("compile", "printf two > two.txt", Some("two.txt")),
                    step("package", "cat one.txt two.txt > bundle.txt", Some("bundle.txt")),
                ],
                "log": "build.log",
            }),
            None,
            Some(Arc::new(move |partial| {
                sink.lock().unwrap().push(serde_json::json!({
                    "content": partial.content,
                    "details": partial.details,
                }));
            })),
        )
        .expect("the build returns");

    // The result is something the caller can match against the plan: every
    // step's status, and the counts.
    assert_eq!(
        statuses(&result),
        vec![
            ("prepare".to_owned(), "ok".to_owned()),
            ("compile".to_owned(), "ok".to_owned()),
            ("package".to_owned(), "ok".to_owned()),
        ]
    );
    assert_eq!(result["details"]["completed"], 3);
    assert_eq!(result["details"]["skipped"], 0);
    assert_eq!(result["details"]["aborted"], false);

    // Progress is reported once per step, in order, while the build runs.
    let updates = updates.lock().unwrap();
    let steps: Vec<i64> = updates
        .iter()
        .map(|update| update["details"]["step"].as_i64().unwrap_or_default())
        .collect();
    assert_eq!(steps, vec![1, 2, 3], "one update per step, in order");
    assert_eq!(updates[2]["details"]["completed"], 3);

    // The log is where the caller can read what the steps actually printed.
    let log = std::fs::read_to_string(fixture.path("build.log")).expect("the log was written");
    assert!(log.contains("prepare") && log.contains("compile"), "{log}");
    let bundle = std::fs::read_to_string(fixture.path("bundle.txt")).unwrap();
    assert_eq!(bundle, "onetwo");
}

#[test]
fn a_failing_step_stops_the_build_and_is_not_success() {
    let mut fixture = Fixture::new();
    let result = fixture.call(
        serde_json::json!({
            "steps": [
                step("prepare", "printf one > one.txt", Some("one.txt")),
                step("compile", "echo 'undefined symbol: x' >&2; exit 2", None),
                step("package", "printf three > three.txt", Some("three.txt")),
            ],
        }),
        None,
    );

    assert_eq!(
        statuses(&result),
        vec![
            ("prepare".to_owned(), "ok".to_owned()),
            ("compile".to_owned(), "failed".to_owned()),
        ],
        "the build stops at the failure and does not run what follows"
    );
    assert_eq!(result["details"]["results"][1]["code"], 2);
    assert!(
        result["details"]["results"][1]["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("undefined symbol"),
        "the failure carries its output"
    );
    assert_eq!(result["details"]["completed"], 1);
    assert!(!fixture.path("three.txt").exists());
    assert!(
        !fixture.commands_run().iter().any(|line| line.contains("three")),
        "the step after the failure never ran"
    );
}

#[test]
fn a_resumed_build_skips_the_steps_whose_artifact_exists() {
    let mut fixture = Fixture::new();
    let plan = serde_json::json!({
        "steps": [
            step("prepare", "printf one > one.txt", Some("one.txt")),
            step("compile", "printf two > two.txt", Some("two.txt")),
        ],
    });
    let first = fixture.call(plan.clone(), None);
    assert_eq!(first["details"]["completed"], 2);
    let spawns_after_first = fixture.commands_run().len();

    let mut resume = plan;
    resume["resume"] = serde_json::Value::Bool(true);
    let second = fixture.call(resume, None);

    assert_eq!(
        statuses(&second),
        vec![
            ("prepare".to_owned(), "skipped".to_owned()),
            ("compile".to_owned(), "skipped".to_owned()),
        ]
    );
    assert_eq!(second["details"]["skipped"], 2);
    assert_eq!(second["details"]["completed"], 0);
    assert_eq!(
        fixture.commands_run().len(),
        spawns_after_first,
        "a skipped step does not spawn a process"
    );
}

#[test]
fn an_interrupted_build_kills_the_process_and_reports_what_ran() {
    let mut fixture = Fixture::new();
    let signal = AbortSignal::new();
    let killer = signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        killer.abort();
    });

    let started = Instant::now();
    let result = fixture.call(
        serde_json::json!({
            "steps": [
                step("prepare", "printf one > one.txt", Some("one.txt")),
                // Long enough that only the abort can end it. `exec` replaces
                // the shell, because killing the shell would leave the child it
                // started holding the output pipes (docs/TASKS.md).
                step("compile", "exec sleep 30", Some("two.txt")),
                step("package", "printf three > three.txt", Some("three.txt")),
            ],
            "log": "interrupted.log",
        }),
        Some(signal),
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(20),
        "the abort ended the build instead of waiting for `sleep 30`: {elapsed:?}"
    );
    let statuses = statuses(&result);
    assert_eq!(
        statuses,
        vec![
            ("prepare".to_owned(), "ok".to_owned()),
            ("compile".to_owned(), "interrupted".to_owned()),
        ],
        "the running step is reported as interrupted, and nothing after it runs"
    );
    assert_eq!(result["details"]["aborted"], true);
    assert_eq!(result["details"]["completed"], 1);
    assert!(
        result["details"]["results"][1]["code"].as_i64().unwrap_or_default() != 0,
        "an interrupted step does not carry a success exit code"
    );
    // The log survives the interruption: that is the moment it is needed.
    let log = std::fs::read_to_string(fixture.path("interrupted.log")).unwrap();
    assert!(log.contains("prepare"), "{log}");
    assert!(!fixture.path("three.txt").exists());
}

#[test]
fn a_build_after_an_interrupt_resumes_from_the_artifacts() {
    let mut fixture = Fixture::new();
    let signal = AbortSignal::new();
    let killer = signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        killer.abort();
    });
    let plan = serde_json::json!({
        "steps": [
            step("prepare", "printf one > one.txt", Some("one.txt")),
            step("compile", "exec sleep 30", Some("two.txt")),
        ],
    });
    let interrupted = fixture.call(plan.clone(), Some(signal));
    assert_eq!(interrupted["details"]["aborted"], true);
    assert!(fixture.path("one.txt").exists());
    assert!(!fixture.path("two.txt").exists());

    // The restart does not repeat the work that survived: `prepare` is skipped
    // because its artifact is on disk, and only `compile` runs.
    let spawns_before = fixture.commands_run().len();
    let mut resume = plan;
    resume["resume"] = serde_json::Value::Bool(true);
    // The unfinished step finishes quickly the second time: what is under test
    // is that `prepare` is *skipped* from its artifact, not the sleep.
    resume["steps"][1] = step("compile", "printf two > two.txt", Some("two.txt"));
    let resumed = fixture.call(resume, None);

    assert_eq!(
        statuses(&resumed),
        vec![
            ("prepare".to_owned(), "skipped".to_owned()),
            ("compile".to_owned(), "ok".to_owned()),
        ]
    );
    assert_eq!(resumed["details"]["completed"], 1);
    assert_eq!(resumed["details"]["skipped"], 1);
    assert_eq!(
        fixture.commands_run().len(),
        spawns_before + 1,
        "exactly the unfinished step ran again"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.path("two.txt")).unwrap(),
        "two"
    );
}

#[test]
fn the_plan_is_validated_before_anything_runs() {
    let mut fixture = Fixture::new();
    let error = fixture
        .runtime
        .call_tool(
            "build",
            "call-1",
            // `steps` is required, and a step's items are typed.
            serde_json::json!({ "steps": [{ "name": "prepare" }] }),
            None,
            None,
        )
        .expect_err("invalid arguments are refused");
    assert!(error.contains("Validation failed"), "{error}");
    assert!(
        fixture.commands_run().is_empty(),
        "nothing ran: the schema gate refused the plan first"
    );

    assert_eq!(
        fixture
            .runtime
            .call_tool("build", "call-1", serde_json::json!({}), None, None)
            .expect_err("a missing plan is refused")
            .contains("Validation failed"),
        true
    );
    assert!(fixture.commands_run().is_empty());
}
