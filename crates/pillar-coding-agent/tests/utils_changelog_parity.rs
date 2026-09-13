//! Parity tests for utils/changelog.ts (pi v0.84.3, upstream
//! test/changelog.test.ts) plus the parser/version helpers.

use std::io::Write;

use pillar_coding_agent::utils::changelog::{
    ChangelogEntry, compare_versions, get_new_entries, normalize_changelog_links,
    normalize_changelog_links_for_entry, parse_changelog, parse_changelog_content,
};

fn entry(major: u64, minor: u64, patch: u64) -> ChangelogEntry {
    ChangelogEntry {
        major,
        minor,
        patch,
        content: String::new(),
    }
}

#[test]
fn rewrites_package_relative_links_to_tag_pinned_source_links() {
    let markdown = [
        "[Project Trust](README.md#project-trust)",
        "[Extensions](docs/extensions.md#project_trust)",
        "[Examples](examples/extensions/)",
        "[Root README](../../README.md#supply-chain-hardening)",
    ]
    .join("\n");

    assert_eq!(
        normalize_changelog_links_for_entry(&markdown, &entry(0, 79, 0)),
        [
            "[Project Trust](https://github.com/earendil-works/pi/blob/v0.79.0/packages/coding-agent/README.md#project-trust)",
            "[Extensions](https://github.com/earendil-works/pi/blob/v0.79.0/packages/coding-agent/docs/extensions.md#project_trust)",
            "[Examples](https://github.com/earendil-works/pi/tree/v0.79.0/packages/coding-agent/examples/extensions/)",
            "[Root README](https://github.com/earendil-works/pi/blob/v0.79.0/README.md#supply-chain-hardening)",
        ]
        .join("\n")
    );
}

#[test]
fn canonicalizes_legacy_repository_urls_and_keeps_external_links() {
    let markdown = [
        "[#5167](https://github.com/earendil-works/pi-mono/pull/5167)",
        "[#4163](https://github.com/badlogic/pi-mono/issues/4163)",
        "[Agent README](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md)",
        "[External](https://example.com/docs)",
        "[Local anchor](#settings)",
    ]
    .join("\n");

    assert_eq!(
        normalize_changelog_links(&markdown, "0.79.0"),
        [
            "[#5167](https://github.com/earendil-works/pi/pull/5167)",
            "[#4163](https://github.com/earendil-works/pi/issues/4163)",
            "[Agent README](https://github.com/earendil-works/pi/blob/v0.79.0/packages/agent/README.md)",
            "[External](https://example.com/docs)",
            "[Local anchor](#settings)",
        ]
        .join("\n")
    );
}

#[test]
fn a_v_prefix_is_not_doubled_and_query_fragments_survive() {
    // Already-tagged versions stay as-is.
    assert_eq!(
        normalize_changelog_links("[Doc](docs/a.md)", "v1.0.0"),
        "[Doc](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/a.md)"
    );
    // Query strings and fragments are appended after the encoded path.
    assert_eq!(
        normalize_changelog_links("[Doc](docs/a.md?plain=1#top)", "0.1.0"),
        "[Doc](https://github.com/earendil-works/pi/blob/v0.1.0/packages/coding-agent/docs/a.md?plain=1#top)"
    );
    // Targets that escape the repository are left alone.
    assert_eq!(
        normalize_changelog_links("[Up](../../../../etc/passwd)", "0.1.0"),
        "[Up](../../../../etc/passwd)"
    );
    // Absolute repository paths resolve from the repository root.
    assert_eq!(
        normalize_changelog_links("[Root](/README.md)", "0.1.0"),
        "[Root](https://github.com/earendil-works/pi/blob/v0.1.0/README.md)"
    );
    // Directory targets without an extension use the tree route.
    assert_eq!(
        normalize_changelog_links("[Dir](docs/guides)", "0.1.0"),
        "[Dir](https://github.com/earendil-works/pi/tree/v0.1.0/packages/coding-agent/docs/guides)"
    );
}

#[test]
fn parses_version_sections_and_ignores_other_headers() {
    let content = [
        "# Changelog",
        "",
        "Intro text.",
        "",
        "## [0.80.0] - 2026-01-02",
        "### Added",
        "- thing",
        "",
        "## 0.79.0",
        "- older thing",
        "",
        "## Unreleased",
        "- unversioned",
    ]
    .join("\n");

    let entries = parse_changelog_content(&content);
    assert_eq!(entries.len(), 2);
    assert_eq!(
        (entries[0].major, entries[0].minor, entries[0].patch),
        (0, 80, 0)
    );
    assert!(entries[0].content.starts_with("## [0.80.0] - 2026-01-02"));
    assert!(entries[0].content.contains("- thing"));
    assert!(!entries[0].content.contains("older thing"));
    assert_eq!(
        (entries[1].major, entries[1].minor, entries[1].patch),
        (0, 79, 0)
    );
    assert!(entries[1].content.contains("- older thing"));
    // The "## Unreleased" section is dropped (no version match).
    assert!(
        !entries
            .iter()
            .any(|entry| entry.content.contains("unversioned"))
    );
}

#[test]
fn missing_changelog_files_parse_to_no_entries() {
    let path = std::env::temp_dir().join(format!(
        "pillar-changelog-missing-{}.md",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    assert!(parse_changelog(&path).is_empty());

    let dir = std::env::temp_dir().join(format!("pillar-changelog-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("CHANGELOG.md");
    let mut handle = std::fs::File::create(&file).expect("create changelog");
    handle
        .write_all(b"## [1.2.3]\n- note\n")
        .expect("write changelog");
    let entries = parse_changelog(&file);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (entries[0].major, entries[0].minor, entries[0].patch),
        (1, 2, 3)
    );
}

#[test]
fn version_comparison_and_new_entry_filtering() {
    assert_eq!(compare_versions(&entry(0, 79, 0), &entry(0, 80, 0)), -1);
    assert_eq!(compare_versions(&entry(1, 0, 0), &entry(0, 99, 9)), 1);
    assert_eq!(compare_versions(&entry(0, 79, 1), &entry(0, 79, 1)), 0);

    let entries = vec![entry(0, 81, 0), entry(0, 80, 0), entry(0, 79, 5)];
    let newer = get_new_entries(&entries, "0.79.5");
    assert_eq!(newer.len(), 2);
    assert_eq!(newer[0].minor, 81);
    assert_eq!(newer[1].minor, 80);

    // A malformed last version behaves like 0.0.0 (upstream `|| 0`).
    assert_eq!(get_new_entries(&entries, "x.y.z").len(), 3);
}
