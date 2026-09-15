//! Port of packages/coding-agent/src/utils/changelog.ts (pi v0.84.3):
//! parse CHANGELOG.md entries and normalize their relative links to
//! tag-pinned GitHub source links.
//!
//! divergences:
//! - upstream re-exports `getChangelogPath` from `config.ts`, which resolves
//!   paths relative to the JS package layout (Bun binary / dist / src). The
//!   port has no package dir, so [`changelog_path`] honours
//!   `PILLAR_CHANGELOG_PATH`, then a `CHANGELOG.md` next to the executable, then
//!   `./CHANGELOG.md`.
//! - the `regex` crate has no lookahead, so the legacy-repo pattern consumes
//!   its `(?=/|$)` separator into a capture group.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// One changelog section (upstream `ChangelogEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub content: String,
}

/// The upstream repository the changelog links point at.
const GITHUB_REPO: &str = "earendil-works/pi";
/// Changelog links are relative to this package inside the repository.
const CHANGELOG_LINK_BASE_PATH: &str = "packages/coding-agent";

static LEGACY_REPO_RE: LazyLock<Regex> = LazyLock::new(|| {
    // upstream: ^https://github\.com/(?:badlogic|earendil-works)/pi-mono(?=/|$)
    Regex::new(r"^https://github\.com/(?:badlogic|earendil-works)/pi-mono(/|$)")
        .expect("legacy repo regex")
});
static URL_SCHEME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*:").expect("url scheme regex"));
static INLINE_MARKDOWN_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(!?\[[^\]\n]+\]\()([^\s)]+)((?:\s+[^)]*)?\))").expect("inline link regex")
});
static VERSION_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"##\s+\[?(\d+)\.(\d+)\.(\d+)\]?").expect("version regex"));

fn entry_version(entry: &ChangelogEntry) -> String {
    format!("{}.{}.{}", entry.major, entry.minor, entry.patch)
}

fn normalize_tag(version: &str) -> String {
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

/// Split a link target into fragment / path / query (upstream
/// `splitLocalTarget`).
fn split_local_target(target: &str) -> (String, String, String) {
    let (before_hash, fragment) = match target.find('#') {
        Some(index) => (&target[..index], &target[index..]),
        None => (target, ""),
    };
    match before_hash.find('?') {
        Some(index) => (
            fragment.to_string(),
            before_hash[..index].to_string(),
            before_hash[index..].to_string(),
        ),
        None => (fragment.to_string(), before_hash.to_string(), String::new()),
    }
}

/// `path.posix.normalize`: collapse `.`/`..` segments, keep a trailing
/// slash, and answer `.` for an empty result.
fn posix_normalize(input: &str) -> String {
    if input.is_empty() {
        return ".".to_string();
    }
    let trailing_slash = input.ends_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in input.split('/') {
        match segment {
            "" | "." => {}
            ".." => match segments.last() {
                Some(last) if *last != ".." => {
                    segments.pop();
                }
                _ => segments.push(".."),
            },
            other => segments.push(other),
        }
    }
    let mut normalized = segments.join("/");
    if normalized.is_empty() {
        normalized = ".".to_string();
    }
    if trailing_slash && normalized != "." {
        normalized.push('/');
    }
    normalized
}

/// Resolve a changelog-relative path to a repository path, rejecting targets
/// that escape the repository (upstream `resolveRepositoryPath`).
fn resolve_repository_path(target_path: &str) -> Option<String> {
    let normalized_target = target_path.replace('\\', "/");
    let joined = if let Some(stripped) = normalized_target.strip_prefix('/') {
        posix_normalize(stripped.trim_start_matches('/'))
    } else {
        posix_normalize(&format!("{CHANGELOG_LINK_BASE_PATH}/{normalized_target}"))
    };
    if joined == "." || joined.starts_with("../") || joined == ".." {
        return None;
    }
    Some(joined)
}

/// Whether a link target is a directory (upstream `isDirectoryTarget`).
fn is_directory_target(original_path: &str, repository_path: &str) -> bool {
    if original_path.ends_with('/') {
        return true;
    }
    let basename = repository_path
        .rsplit('/')
        .next()
        .unwrap_or(repository_path);
    !basename.contains('.')
}

/// Normalize a single link target (upstream `normalizeChangelogLinkTarget`).
fn normalize_changelog_link_target(target: &str, tag: &str) -> String {
    let repo_url = format!("https://github.com/{GITHUB_REPO}");
    let mut canonical_target = LEGACY_REPO_RE
        .replace(target, |captures: &regex::Captures<'_>| {
            format!("{repo_url}{}", &captures[1])
        })
        .to_string();

    for route in ["blob", "tree"] {
        for branch in ["main", "master"] {
            let floating_ref_prefix = format!("{repo_url}/{route}/{branch}/");
            if let Some(rest) = canonical_target.strip_prefix(&floating_ref_prefix) {
                canonical_target = format!("{repo_url}/{route}/{tag}/{rest}");
            }
        }
    }

    if canonical_target.starts_with('#')
        || canonical_target.starts_with("//")
        || URL_SCHEME_RE.is_match(&canonical_target)
    {
        return canonical_target;
    }

    let (fragment, path_part, query) = split_local_target(&canonical_target);
    if path_part.is_empty() {
        return canonical_target;
    }
    let Some(repository_path) = resolve_repository_path(&path_part) else {
        return canonical_target;
    };

    let route = if is_directory_target(&path_part, &repository_path) {
        "tree"
    } else {
        "blob"
    };
    format!(
        "https://github.com/{GITHUB_REPO}/{route}/{tag}/{}",
        percent_encode_path(&repository_path)
    ) + &query
        + &fragment
}

/// `encodeURI` for a repository path: percent-encode everything outside the
/// unreserved set plus the path separators `encodeURI` keeps.
fn percent_encode_path(value: &str) -> String {
    const UNRESERVED: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if UNRESERVED.contains(&byte) || b";/?:@&=+$,#".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Rewrite package-relative links in a changelog section to tag-pinned GitHub
/// source links (upstream `normalizeChangelogLinks`).
pub fn normalize_changelog_links(markdown: &str, version: &str) -> String {
    normalize_changelog_links_with(markdown, &normalize_tag(version))
}

/// As [`normalize_changelog_links`] but taking an entry.
pub fn normalize_changelog_links_for_entry(markdown: &str, entry: &ChangelogEntry) -> String {
    normalize_changelog_links_with(markdown, &normalize_tag(&entry_version(entry)))
}

fn normalize_changelog_links_with(markdown: &str, tag: &str) -> String {
    INLINE_MARKDOWN_LINK_RE
        .replace_all(markdown, |captures: &regex::Captures<'_>| {
            let prefix = &captures[1];
            let target = &captures[2];
            let suffix = &captures[3];
            format!(
                "{prefix}{}{suffix}",
                normalize_changelog_link_target(target, tag)
            )
        })
        .to_string()
}

/// Parse changelog sections from markdown content (upstream `parseChangelog`
/// minus the file read).
pub fn parse_changelog_content(content: &str) -> Vec<ChangelogEntry> {
    let mut entries = Vec::new();
    let mut current_lines: Vec<String> = Vec::new();
    let mut current_version: Option<(u64, u64, u64)> = None;

    for line in content.split('\n') {
        if line.starts_with("## ") {
            if let Some((major, minor, patch)) = current_version {
                if !current_lines.is_empty() {
                    entries.push(ChangelogEntry {
                        major,
                        minor,
                        patch,
                        content: current_lines.join("\n").trim().to_string(),
                    });
                }
            }

            match VERSION_HEADER_RE.captures(line) {
                Some(captures) => {
                    current_version = Some((
                        captures[1].parse().unwrap_or(0),
                        captures[2].parse().unwrap_or(0),
                        captures[3].parse().unwrap_or(0),
                    ));
                    current_lines = vec![line.to_string()];
                }
                None => {
                    current_version = None;
                    current_lines = Vec::new();
                }
            }
        } else if current_version.is_some() {
            current_lines.push(line.to_string());
        }
    }

    if let Some((major, minor, patch)) = current_version {
        if !current_lines.is_empty() {
            entries.push(ChangelogEntry {
                major,
                minor,
                patch,
                content: current_lines.join("\n").trim().to_string(),
            });
        }
    }

    entries
}

/// Parse changelog sections from a file (upstream `parseChangelog`): a missing
/// or unreadable file yields no entries.
pub fn parse_changelog(changelog_path: &Path) -> Vec<ChangelogEntry> {
    match std::fs::read_to_string(changelog_path) {
        Ok(content) => parse_changelog_content(&content),
        Err(_) => Vec::new(),
    }
}

/// Upstream `compareVersions`: -1, 0, or 1.
pub fn compare_versions(v1: &ChangelogEntry, v2: &ChangelogEntry) -> i32 {
    if v1.major != v2.major {
        return if v1.major < v2.major { -1 } else { 1 };
    }
    if v1.minor != v2.minor {
        return if v1.minor < v2.minor { -1 } else { 1 };
    }
    if v1.patch == v2.patch {
        0
    } else if v1.patch < v2.patch {
        -1
    } else {
        1
    }
}

/// Entries strictly newer than `last_version` (upstream `getNewEntries`).
pub fn get_new_entries(entries: &[ChangelogEntry], last_version: &str) -> Vec<ChangelogEntry> {
    let parts: Vec<u64> = last_version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect();
    let last = ChangelogEntry {
        major: parts.first().copied().unwrap_or(0),
        minor: parts.get(1).copied().unwrap_or(0),
        patch: parts.get(2).copied().unwrap_or(0),
        content: String::new(),
    };
    entries
        .iter()
        .filter(|entry| compare_versions(entry, &last) > 0)
        .cloned()
        .collect()
}

/// Path of the bundled CHANGELOG.md (upstream `getChangelogPath`, adapted to
/// a binary without a JS package layout).
pub fn changelog_path() -> PathBuf {
    if let Ok(configured) = std::env::var("PILLAR_CHANGELOG_PATH") {
        if !configured.trim().is_empty() {
            return PathBuf::from(configured);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("CHANGELOG.md");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("CHANGELOG.md")
}
