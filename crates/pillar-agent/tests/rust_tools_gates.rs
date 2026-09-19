//! Offline gates for the Rust tooling core (`docs/RUST-TOOLING-DESIGN.md`
//! §5, §7, §12). Every assertion is a deterministic check over saved metadata
//! and captured Cargo JSON: no Cargo, no filesystem, no network.
//!
//! The cases mirror the design's mandatory fixture table for R1:
//!
//! - E0277 with primary/secondary/children and a multipart suggestion is
//!   normalized without losing the related text or the proposal group;
//! - a build that finished successfully but ran no tests is *not* a test pass,
//!   and `running 0 tests` is not a pass either;
//! - non-JSON, unknown reasons and malformed JSON are kept and make the
//!   collection partial instead of being dropped;
//! - registry / sysroot / macro-virtual spans are never editable workspace
//!   targets;
//! - `--lib` is selected only for a package that has a library, a requested
//!   integration target is validated at plan time, and what the plan does not
//!   cover is reported structurally.

#![cfg(feature = "rust-tools")]

use pillar_agent::rust_tools::diagnostic::{DiagnosticCollector, SourceBinding};
use pillar_agent::rust_tools::metadata::{Configuration, TargetRecord, WorkspaceCatalog};
use pillar_agent::rust_tools::plan::{Coverage, PlanGoal, PlanRequest, PlanScope, plan};
use pillar_agent::rust_tools::{CollectionLimits, RustToolErrorCode, SourcePolicy, UnverifiedKind};

const REPO_ROOT: &str = "/ws";
const CORE: &str = "path+file:///ws/crates/example-core#example-core@0.1.0";
const CLI: &str = "path+file:///ws/crates/example-cli#example-cli@0.1.0";

fn metadata_json() -> String {
    format!(
        r#"{{
  "packages": [
    {{
      "id": "{CORE}",
      "name": "example-core",
      "version": "0.1.0",
      "manifest_path": "/ws/crates/example-core/Cargo.toml",
      "targets": [
        {{"name": "example_core", "kind": ["lib"], "crate_types": ["lib"],
          "src_path": "/ws/crates/example-core/src/lib.rs", "edition": "2024",
          "doc": true, "doctest": true, "test": true}},
        {{"name": "contract", "kind": ["test"], "crate_types": ["bin"],
          "src_path": "/ws/crates/example-core/tests/contract.rs", "edition": "2024",
          "doc": false, "doctest": false, "test": true}},
        {{"name": "gated", "kind": ["test"], "crate_types": ["bin"],
          "src_path": "/ws/crates/example-core/tests/gated.rs", "edition": "2024",
          "doc": false, "doctest": false, "test": true, "required-features": ["extra"]}}
      ],
      "dependencies": [],
      "features": {{"default": [], "extra": []}}
    }},
    {{
      "id": "{CLI}",
      "name": "example-cli",
      "version": "0.1.0",
      "manifest_path": "/ws/crates/example-cli/Cargo.toml",
      "targets": [
        {{"name": "example-cli", "kind": ["bin"], "crate_types": ["bin"],
          "src_path": "/ws/crates/example-cli/src/main.rs", "edition": "2024",
          "doc": false, "doctest": false, "test": true}}
      ],
      "dependencies": [
        {{"name": "example-core", "source": null, "req": "^0.1.0",
          "kind": null, "rename": null, "optional": false, "target": null}}
      ],
      "features": {{}}
    }}
  ],
  "workspace_members": ["{CORE}", "{CLI}"],
  "workspace_default_members": ["{CORE}", "{CLI}"],
  "workspace_root": "{REPO_ROOT}",
  "target_directory": "{REPO_ROOT}/target",
  "metadata": null
}}"#
    )
}

fn catalog() -> WorkspaceCatalog {
    WorkspaceCatalog::from_json(&metadata_json()).expect("metadata parses")
}

fn configuration(id: &str, features: &[&str]) -> Configuration {
    Configuration {
        id: id.to_string(),
        toolchain: Some("stable".to_string()),
        host_triple: Some("x86_64-unknown-linux-gnu".to_string()),
        target_triple: Some("x86_64-unknown-linux-gnu".to_string()),
        profile: Some("test".to_string()),
        packages: Vec::new(),
        features: features.iter().map(|feature| feature.to_string()).collect(),
        all_features: false,
        no_default_features: false,
        selected_targets: Vec::new(),
        cargo_config_digest: None,
        lock_digest: Some("lock-digest".to_string()),
        rustflags_digest: None,
        env_digest: None,
    }
}

fn request(paths: &[&str], configurations: &[&str]) -> PlanRequest {
    PlanRequest {
        changed_paths: paths.iter().map(|path| path.to_string()).collect(),
        configuration_ids: configurations.iter().map(|id| id.to_string()).collect(),
        goal: PlanGoal::ValidateChange,
        scope: PlanScope::Focused,
        requested_targets: Vec::new(),
        metadata_digest: None,
    }
}

fn compiler_message(message: &str) -> String {
    format!(
        r#"{{"reason":"compiler-message","package_id":"{CORE}","target":{{"name":"example_core"}},"message":{message}}}"#
    )
}

// --- metadata (§7.1) -------------------------------------------------------

#[test]
fn changed_paths_resolve_to_the_owning_package_and_reverse_dependencies() {
    let catalog = catalog();
    let core = catalog
        .package_for_path("crates/example-core/src/lib.rs")
        .expect("relative path maps to example-core");
    assert_eq!(core.name, "example-core");

    let cli = catalog
        .package_for_path("/ws/crates/example-cli/src/main.rs")
        .expect("absolute path maps to example-cli");
    assert_eq!(cli.name, "example-cli");

    assert!(
        catalog.package_for_path("docs/notes.md").is_none(),
        "a path outside every member is unmapped, not silently ignored"
    );

    let dependents = catalog.reverse_dependencies(CORE);
    assert_eq!(dependents.len(), 1);
    assert_eq!(dependents[0].0.name, "example-cli");
    assert_eq!(dependents[0].1.kind_name(), "normal");
}

#[test]
fn target_kinds_are_classified_from_the_metadata() {
    let catalog = catalog();
    let core = catalog.package_by_name("example-core").expect("package");
    assert!(core.library().is_some());
    assert_eq!(core.integration_tests().count(), 2);
    assert!(core.has_features());

    let cli = catalog.package_by_name("example-cli").expect("package");
    assert!(cli.library().is_none());
    assert_eq!(cli.testable_binaries().count(), 1);
}

// --- diagnostics (§5) ------------------------------------------------------

#[test]
fn a_diagnostic_keeps_primary_secondary_children_and_a_multipart_suggestion() {
    let message = r#"{
      "rendered": "error[E0277]: ...",
      "code": {"code": "E0277", "explanation": null},
      "level": "error",
      "message": "the trait bound `Foo: Bar` is not satisfied",
      "spans": [
        {"file_name": "/ws/crates/example-core/src/lib.rs",
         "byte_start": 10, "byte_end": 13,
         "line_start": 2, "line_end": 2, "column_start": 1, "column_end": 4,
         "is_primary": true, "label": "required by this bound",
         "suggested_replacement": "impl Bar for Foo {}",
         "suggestion_applicability": "MachineApplicable",
         "text": [{"text": "let x = foo();", "highlight_start": 9, "highlight_end": 12}]},
        {"file_name": "/ws/crates/example-core/src/lib.rs",
         "byte_start": 30, "byte_end": 33,
         "line_start": 4, "line_end": 4, "column_start": 1, "column_end": 4,
         "is_primary": false, "label": "also here",
         "suggested_replacement": "let y = 0;",
         "suggestion_applicability": "MachineApplicable"}
      ],
      "children": [
        {"rendered": null, "children": [], "code": null, "level": "note",
         "message": "required by a bound in `Bar`", "spans": []}
      ]
    }"#;
    let mut collector = DiagnosticCollector::new("run1", CollectionLimits::default());
    assert!(collector.push_line(&compiler_message(message)));
    let run = collector.finish(&SourcePolicy::new(REPO_ROOT));

    assert_eq!(run.diagnostics.len(), 1);
    let diagnostic = &run.diagnostics[0];
    assert_eq!(diagnostic.code.as_deref(), Some("E0277"));
    assert!(diagnostic.is_error());
    assert_eq!(diagnostic.spans.len(), 2);
    assert_eq!(diagnostic.children.len(), 1);
    assert_eq!(
        diagnostic.children[0].message,
        "required by a bound in `Bar`"
    );
    assert_eq!(
        diagnostic.spans[0].binding,
        SourceBinding::Workspace {
            path: "/ws/crates/example-core/src/lib.rs".to_string()
        }
    );
    // The byte range is half-open and 0-based; the line/column fields are kept
    // as display coordinates, not converted.
    assert_eq!(diagnostic.spans[0].byte_range(), (10, 13));

    assert_eq!(diagnostic.suggestions.len(), 1);
    let suggestion = &diagnostic.suggestions[0];
    assert_eq!(suggestion.replacements.len(), 2);
    assert!(suggestion.is_machine_applicable());
    assert!(!suggestion.has_insertions());
}

#[test]
fn build_finished_success_is_not_test_success() {
    let mut collector = DiagnosticCollector::new("run1", CollectionLimits::default());
    collector.push_line(r#"{"reason":"build-finished","success":true}"#);
    let run = collector.finish(&SourcePolicy::new(REPO_ROOT));

    assert_eq!(
        run.build_status,
        pillar_agent::rust_tools::BuildStatus::Succeeded
    );
    assert_eq!(
        run.test_status,
        pillar_agent::rust_tools::TestStatus::NotRun
    );
    assert!(!run.test_evidence.parsed);
}

#[test]
fn a_zero_selected_test_run_is_not_a_pass() {
    let mut collector = DiagnosticCollector::new("run1", CollectionLimits::default());
    collector.push_line("running 0 tests");
    let run = collector.finish(&SourcePolicy::new(REPO_ROOT));

    assert!(run.test_evidence.zero_selected);
    assert!(!run.test_evidence.parsed);
    assert_eq!(
        run.test_status,
        pillar_agent::rust_tools::TestStatus::Unknown
    );
}

#[test]
fn a_failed_libtest_summary_marks_the_test_phase_failed() {
    let mut collector = DiagnosticCollector::new("run1", CollectionLimits::default());
    collector.push_line(
        "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s",
    );
    let run = collector.finish(&SourcePolicy::new(REPO_ROOT));

    assert!(run.test_evidence.parsed);
    assert_eq!(
        run.test_status,
        pillar_agent::rust_tools::TestStatus::Failed
    );
}

#[test]
fn a_passing_libtest_summary_marks_the_test_phase_passed() {
    let mut collector = DiagnosticCollector::new("run1", CollectionLimits::default());
    collector.push_line(
        "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s",
    );
    let run = collector.finish(&SourcePolicy::new(REPO_ROOT));

    assert!(run.test_evidence.parsed);
    assert_eq!(
        run.test_status,
        pillar_agent::rust_tools::TestStatus::Passed
    );
}

#[test]
fn non_json_unknown_reasons_and_malformed_lines_are_kept_and_partial() {
    let mut collector = DiagnosticCollector::new("run1", CollectionLimits::default());
    collector.push_line("warning: build script printed this"); // non-JSON
    collector.push_line(r#"{"reason":"future-thing","detail":1}"#); // unknown reason
    collector.push_line(r#"{"reason":"compiler-message","message":42}"#); // malformed shape
    let run = collector.finish(&SourcePolicy::new(REPO_ROOT));

    assert_eq!(run.unstructured.len(), 1);
    assert_eq!(run.other_messages.len(), 1);
    assert_eq!(run.other_messages[0].reason, "future-thing");
    assert_eq!(run.parse_errors.len(), 1);
    assert!(
        !run.collection.is_complete(),
        "foreign output is not a clean run"
    );
}

#[test]
fn registry_sysroot_and_macro_virtual_spans_are_not_editable() {
    let policy = SourcePolicy {
        workspace_root: Some(REPO_ROOT.to_string()),
        registry_roots: vec!["/home/u/.cargo/registry".to_string()],
        sysroot_roots: vec!["/rust/sysroot".to_string()],
    };
    let message = |file: &str| {
        format!(
            r#"{{"reason":"compiler-message","package_id":"{CORE}","target":{{"name":"example_core"}},"message":{{"code":null,"level":"error","message":"x","spans":[{{"file_name":"{file}","byte_start":1,"byte_end":2,"is_primary":true}}]}}}}"#
        )
    };
    let mut collector = DiagnosticCollector::new("run1", CollectionLimits::default());
    collector.push_line(&message("/ws/crates/example-core/src/lib.rs"));
    collector.push_line(&message("/home/u/.cargo/registry/src/crate-1.0/src/lib.rs"));
    collector.push_line(&message("/rust/sysroot/lib/rustlib/src/std.rs"));
    collector.push_line(&message("<vec macros>"));
    collector.push_line(&message("/tmp/other.rs"));
    let run = collector.finish(&policy);

    let bindings: Vec<&SourceBinding> = run
        .diagnostics
        .iter()
        .map(|diagnostic| &diagnostic.spans[0].binding)
        .collect();
    assert!(bindings[0].is_editable_candidate());
    assert!(matches!(
        bindings[1],
        SourceBinding::RegistryOrSysroot { .. }
    ));
    assert!(matches!(
        bindings[2],
        SourceBinding::RegistryOrSysroot { .. }
    ));
    assert!(matches!(bindings[3], SourceBinding::MacroVirtual { .. }));
    assert!(matches!(bindings[4], SourceBinding::Unmapped { .. }));
    assert_eq!(run.editable_spans().count(), 1);
}

#[test]
fn oversized_lines_are_dropped_without_stopping_the_collection() {
    let limits = CollectionLimits {
        max_line_bytes: 64,
        ..CollectionLimits::default()
    };
    let mut collector = DiagnosticCollector::new("run1", limits);
    let long = "x".repeat(128);
    assert!(!collector.push_line(&long));
    // The next line is still collected.
    assert!(collector.push_line(r#"{"reason":"build-finished","success":true}"#));
    let run = collector.finish(&SourcePolicy::new(REPO_ROOT));

    assert!(!run.collection.is_complete());
    assert_eq!(run.parse_errors.len(), 1);
    assert_eq!(
        run.build_status,
        pillar_agent::rust_tools::BuildStatus::Succeeded
    );
}

// --- planning (§7) ---------------------------------------------------------

#[test]
fn a_library_package_gets_lib_and_the_requested_integration_target() {
    let catalog = catalog();
    let configuration = configuration("native-default", &[]);
    let mut request = request(&["crates/example-core/src/lib.rs"], &["native-default"]);
    request.requested_targets = vec!["contract".to_string()];

    let plan = plan(&catalog, &[configuration], &request).expect("plan");
    assert_eq!(plan.steps.len(), 1);
    let step = &plan.steps[0];
    assert_eq!(
        step.argv,
        vec![
            "cargo",
            "test",
            "--locked",
            "-p",
            "example-core",
            "--lib",
            "--test",
            "contract",
            "--message-format=json"
        ]
    );
    assert!(step.covers.contains(&Coverage::UnitTests {
        package: "example-core".to_string()
    }));
    assert!(step.covers.contains(&Coverage::IntegrationTarget {
        package: "example-core".to_string(),
        target: "contract".to_string()
    }));
    assert!(!plan.execution_started, "the planner never runs anything");
    assert!(!plan.metadata_digest.is_empty());

    // The dimensions the green run does not cover are named.
    assert!(
        plan.unverified
            .iter()
            .any(|item| item.kind == UnverifiedKind::Doctests)
    );
    assert!(
        plan.unverified
            .iter()
            .any(|item| item.kind == UnverifiedKind::ReverseDependents)
    );
    assert!(
        plan.unverified
            .iter()
            .any(|item| item.kind == UnverifiedKind::OtherTargets),
        "the unselected `gated` target is reported"
    );
}

#[test]
fn a_package_without_a_library_does_not_get_lib() {
    let catalog = catalog();
    let configuration = configuration("native-default", &[]);
    let request = request(&["crates/example-cli/src/main.rs"], &["native-default"]);

    let plan = plan(&catalog, &[configuration], &request).expect("plan");
    assert_eq!(plan.steps.len(), 1);
    assert!(plan.steps[0].argv.contains(&"--bins".to_string()));
    assert!(
        !plan.steps[0].argv.contains(&"--lib".to_string()),
        "a package without a library must not be given --lib"
    );
    assert!(plan.steps[0].covers.contains(&Coverage::BinaryUnitTests {
        package: "example-cli".to_string()
    }));
}

#[test]
fn an_unknown_configuration_id_is_refused() {
    let catalog = catalog();
    let request = request(&["crates/example-core/src/lib.rs"], &["not-approved"]);
    let error = plan(&catalog, &[configuration("native-default", &[])], &request)
        .expect_err("unknown configuration");
    assert_eq!(error.code, RustToolErrorCode::ConfigurationMismatch);
}

#[test]
fn a_requested_target_that_does_not_exist_is_refused_at_plan_time() {
    let catalog = catalog();
    let mut request = request(&["crates/example-core/src/lib.rs"], &["native-default"]);
    request.requested_targets = vec!["nope".to_string()];
    let error = plan(&catalog, &[configuration("native-default", &[])], &request)
        .expect_err("unknown target");
    assert_eq!(error.code, RustToolErrorCode::InvalidRequest);
}

#[test]
fn a_manifest_change_broadens_the_plan_and_reports_stale_metadata() {
    let catalog = catalog();
    let request = request(&["Cargo.toml"], &["native-default"]);
    let plan = plan(&catalog, &[configuration("native-default", &[])], &request).expect("plan");

    assert_eq!(plan.steps.len(), 2, "both members are planned");
    assert!(
        plan.unverified
            .iter()
            .any(|item| item.kind == UnverifiedKind::MetadataStale)
    );
}

#[test]
fn a_metadata_digest_mismatch_is_a_stale_plan() {
    let catalog = catalog();
    let mut request = request(&["crates/example-core/src/lib.rs"], &["native-default"]);
    request.metadata_digest = Some("0000000000000000".to_string());
    let error =
        plan(&catalog, &[configuration("native-default", &[])], &request).expect_err("stale");
    assert_eq!(error.code, RustToolErrorCode::StalePlan);
}

#[test]
fn a_cross_target_configuration_is_reported_as_unverified() {
    let catalog = catalog();
    let mut cross = configuration("wasm", &[]);
    cross.target_triple = Some("wasm32-unknown-unknown".to_string());
    let request = request(&["crates/example-core/src/lib.rs"], &["wasm"]);
    let plan = plan(&catalog, &[cross], &request).expect("plan");

    let detail = plan
        .unverified
        .iter()
        .find(|item| item.kind == UnverifiedKind::CrossTarget)
        .expect("cross-target is not silently a test run");
    assert!(detail.detail.contains("wasm32-unknown-unknown"));
}

#[test]
fn a_required_feature_target_is_skipped_and_reported() {
    let catalog = catalog();
    let configuration = configuration("native-default", &[]);
    let mut request = request(&["crates/example-core/src/lib.rs"], &["native-default"]);
    request.requested_targets = vec!["gated".to_string()];
    let plan = plan(&catalog, &[configuration], &request).expect("plan");

    assert!(
        !plan.steps[0].argv.contains(&"gated".to_string()),
        "a target whose required features are not enabled is not scheduled"
    );
    assert!(
        plan.unverified
            .iter()
            .any(|item| item.kind == UnverifiedKind::RequiredFeatures)
    );
}

#[test]
fn an_enabled_required_feature_target_is_scheduled() {
    let catalog = catalog();
    let target = TargetRecord {
        name: "gated".to_string(),
        kind: vec!["test".to_string()],
        crate_types: vec![],
        src_path: None,
        edition: None,
        doctest: false,
        test: true,
        doc: false,
        required_features: vec!["extra".to_string()],
    };
    assert!(target.is_named_test_target());

    let configuration = configuration("native-default", &["extra"]);
    let mut request = request(&["crates/example-core/src/lib.rs"], &["native-default"]);
    request.requested_targets = vec!["gated".to_string()];
    let plan = plan(&catalog, &[configuration], &request).expect("plan");

    assert!(plan.steps[0].argv.contains(&"gated".to_string()));
    assert!(
        !plan
            .unverified
            .iter()
            .any(|item| item.kind == UnverifiedKind::RequiredFeatures)
    );
}

#[test]
fn no_changed_path_that_maps_is_an_invalid_request() {
    let catalog = catalog();
    let error = plan(
        &catalog,
        &[configuration("native-default", &[])],
        &request(&["docs/readme.md"], &["native-default"]),
    )
    .expect_err("no mapped package");
    assert_eq!(error.code, RustToolErrorCode::InvalidRequest);
}
