//! Parity tests for trust-manager.ts + pi-manifest.ts (pi v0.84.3): the
//! persisted trust store with nearest-ancestor lookup, option lists,
//! trust-requiring resource detection, and pi manifest parsing.

use std::path::PathBuf;

use pillar_coding_agent::core::trust_manager::{
    ProjectTrustStore, get_project_trust_options, has_trust_requiring_project_resources,
    normalize_cwd, read_pi_manifest,
};

fn temp_agent_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pillar-trust-{}-{name}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// --- normalize_cwd ------------------------------------------------------------------

#[test]
fn normalize_cwd_resolves_dot_segments() {
    assert_eq!(normalize_cwd("/a/b/../c"), "/a/c");
    assert_eq!(normalize_cwd("/a/./b"), "/a/b");
    assert_eq!(normalize_cwd("/"), "/");
    assert_eq!(normalize_cwd("/a/b/"), "/a/b");
}

// --- trust options ---------------------------------------------------------------------

#[test]
fn trust_options_basic_flow() {
    let options = get_project_trust_options("/home/user/proj", false);
    let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["Trust", "Trust parent folder (/home/user)", "Do not trust"]
    );
    // Trust: writes the cwd path.
    assert_eq!(options[0].updates.len(), 1);
    assert_eq!(options[0].updates[0].path, "/home/user/proj");
    assert_eq!(options[0].updates[0].decision, Some(true));
    // Trust parent: sets the parent and clears the cwd entry.
    assert_eq!(options[1].updates.len(), 2);
    assert_eq!(options[1].updates[0].path, "/home/user");
    assert_eq!(options[1].updates[0].decision, Some(true));
    assert_eq!(options[1].updates[1].decision, None);
    assert_eq!(options[1].saved_path.as_deref(), Some("/home/user"));
    // Do not trust: writes false.
    assert_eq!(options[2].updates[0].decision, Some(false));
}

#[test]
fn trust_options_session_only_variants() {
    let options = get_project_trust_options("/home/user/proj", true);
    let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "Trust",
            "Trust parent folder (/home/user)",
            "Trust (this session only)",
            "Do not trust",
            "Do not trust (this session only)"
        ]
    );
    // Session-only options apply no persisted updates.
    assert!(options[2].updates.is_empty());
    assert!(options[4].updates.is_empty());
    assert!(options[2].saved_path.is_none());
}

#[test]
fn root_has_no_parent_trust_option() {
    let options = get_project_trust_options("/", false);
    assert_eq!(options.len(), 2);
}

// --- trust store ------------------------------------------------------------------------

#[test]
fn trust_store_missing_file_returns_none() {
    let store = ProjectTrustStore::new(&temp_agent_dir("missing"));
    assert_eq!(store.get("/some/path"), None);
}

#[test]
fn trust_store_set_get_and_nearest_ancestor() {
    let dir = temp_agent_dir("setget");
    let store = ProjectTrustStore::new(&dir);
    store.set("/home/user/proj-a", Some(true)).unwrap();
    store.set("/home/user/proj-b", Some(false)).unwrap();

    assert_eq!(store.get("/home/user/proj-a"), Some(true));
    assert_eq!(store.get("/home/user/proj-b"), Some(false));
    // No entry, no ancestor entry.
    assert_eq!(store.get("/home/user/other"), None);
    // Nearest ancestor wins.
    assert_eq!(
        store.get_entry("/home/user/proj-a/sub/dir").unwrap().path,
        "/home/user/proj-a"
    );
}

#[test]
fn trust_store_null_decision_clears_entry() {
    let dir = temp_agent_dir("clear");
    let store = ProjectTrustStore::new(&dir);
    store.set("/a/b", Some(true)).unwrap();
    assert_eq!(store.get("/a/b"), Some(true));
    store.set("/a/b", None).unwrap();
    assert_eq!(store.get("/a/b"), None);
}

#[test]
fn trust_store_set_many_applies_atomically() {
    let dir = temp_agent_dir("setmany");
    let store = ProjectTrustStore::new(&dir);
    store
        .set_many(vec![
            pillar_coding_agent::core::trust_manager::ProjectTrustUpdate {
                path: "/a".to_string(),
                decision: Some(true),
            },
            pillar_coding_agent::core::trust_manager::ProjectTrustUpdate {
                path: "/b".to_string(),
                decision: Some(false),
            },
        ])
        .unwrap();
    assert_eq!(store.get("/a"), Some(true));
    assert_eq!(store.get("/b"), Some(false));
}

#[test]
fn trust_store_file_is_sorted_json_with_trailing_newline() {
    let dir = temp_agent_dir("format");
    let store = ProjectTrustStore::new(&dir);
    store.set("/z", Some(true)).unwrap();
    store.set("/a", Some(false)).unwrap();
    let content = std::fs::read_to_string(dir.join("trust.json")).unwrap();
    assert!(content.ends_with("\n}\n"), "{content}");
    let a_index = content.find("\"/a\"").unwrap();
    let z_index = content.find("\"/z\"").unwrap();
    assert!(a_index < z_index, "keys sorted");
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["/a"], serde_json::json!(false));
    assert_eq!(parsed["/z"], serde_json::json!(true));
}

#[test]
fn trust_store_invalid_content_errors_and_get_reports_none() {
    let dir = temp_agent_dir("invalid");
    std::fs::write(dir.join("trust.json"), "not json").unwrap();
    let store = ProjectTrustStore::new(&dir);
    assert_eq!(store.get("/a"), None, "errors surface as no decision");

    // Non-boolean values are rejected on read AND block writes (upstream
    // throws from readTrustFile inside the lock).
    std::fs::write(dir.join("trust.json"), r#"{"a": "yes"}"#).unwrap();
    assert_eq!(store.get("/a"), None);
    assert!(store.set("/a", Some(true)).is_err());
}

// --- trust-requiring resources ---------------------------------------------------------------

#[test]
fn trust_resources_detected_in_project_config_dir() {
    let dir = temp_agent_dir("resources");
    let project = dir.join("proj");
    std::fs::create_dir_all(project.join(".pi").join("skills")).unwrap();
    assert!(has_trust_requiring_project_resources(
        &project.to_string_lossy()
    ));

    // Empty .pi dir does not require trust.
    let project2 = dir.join("proj2");
    std::fs::create_dir_all(project2.join(".pi")).unwrap();
    assert!(!has_trust_requiring_project_resources(
        &project2.to_string_lossy()
    ));
}

#[test]
fn trust_resources_found_in_ancestor_agents_skills() {
    let dir = temp_agent_dir("agents");
    let parent = dir.join("monorepo");
    let child = parent.join("packages").join("app");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::create_dir_all(parent.join(".agents").join("skills")).unwrap();
    assert!(has_trust_requiring_project_resources(
        &child.to_string_lossy()
    ));
}

#[test]
fn user_agents_skills_dir_is_ignored() {
    // Set HOME to the temp dir; the user's ~/.agents/skills must not count.
    let dir = temp_agent_dir("home");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".agents").join("skills")).unwrap();
    // SAFETY: test-only single-threaded env mutation.
    unsafe { std::env::set_var("HOME", &home) };
    // cwd == HOME: the only .agents/skills is the user dir -> not gated.
    assert!(!has_trust_requiring_project_resources(
        &home.to_string_lossy()
    ));
    // But a project .pi resource still gates.
    std::fs::create_dir_all(home.join(".pi").join("prompts")).unwrap();
    assert!(has_trust_requiring_project_resources(
        &home.to_string_lossy()
    ));
    unsafe { std::env::remove_var("HOME") };
}

// --- pi manifest ------------------------------------------------------------------------------

#[test]
fn read_pi_manifest_extracts_string_arrays() {
    let dir = temp_agent_dir("manifest");
    let path = dir.join("package.json");
    std::fs::write(
        &path,
        r#"{
  "name": "pkg",
  "pi": {
    "extensions": ["ext.ts"],
    "skills": ["./skills"],
    "prompts": ["./prompts"],
    "themes": ["./themes"]
  }
}"#,
    )
    .unwrap();
    let manifest = read_pi_manifest(&path).unwrap();
    assert_eq!(manifest.extensions, Some(vec!["ext.ts".to_string()]));
    assert_eq!(manifest.skills, Some(vec!["./skills".to_string()]));
    assert_eq!(manifest.prompts, Some(vec!["./prompts".to_string()]));
    assert_eq!(manifest.themes, Some(vec!["./themes".to_string()]));
}

#[test]
fn read_pi_manifest_rejects_non_string_arrays() {
    let dir = temp_agent_dir("manifest-bad");
    let path = dir.join("package.json");
    std::fs::write(
        &path,
        r#"{"pi": {"extensions": [1, 2], "skills": "not-an-array"}}"#,
    )
    .unwrap();
    let manifest = read_pi_manifest(&path).unwrap();
    assert_eq!(manifest.extensions, None);
    assert_eq!(manifest.skills, None);
}

#[test]
fn read_pi_manifest_missing_or_invalid_returns_none() {
    let dir = temp_agent_dir("manifest-missing");
    // No pi field.
    let path = dir.join("package.json");
    std::fs::write(&path, r#"{"name": "pkg"}"#).unwrap();
    assert!(read_pi_manifest(&path).is_none());
    // Invalid JSON.
    std::fs::write(&path, "nope").unwrap();
    assert!(read_pi_manifest(&path).is_none());
    // Missing file.
    assert!(read_pi_manifest(&dir.join("absent.json")).is_none());
}
