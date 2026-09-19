//! The saved Cargo workspace catalog (design §3, §7.1).
//!
//! `cargo metadata --format-version 1` is parsed into a typed catalog once and
//! read from then on: the planner resolves changed paths to packages and
//! targets without touching Cargo again (design §4: `rs_verify_plan` "Cargoを
//! 隠れて起動しない"). The `Configuration` records are host-approved recipes,
//! not model-supplied shell strings (design §7.1).
//!
//! This module is deliberately OS-free: it reads a saved JSON document and
//! computes over it. Producing the document (running Cargo, following
//! workspaces) is a host adapter behind an effect gate (design §9, §11).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::RustToolError;

/// One target of a package, as `cargo metadata` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRecord {
    pub name: String,
    #[serde(default)]
    pub kind: Vec<String>,
    #[serde(default)]
    pub crate_types: Vec<String>,
    #[serde(default)]
    pub src_path: Option<String>,
    #[serde(default)]
    pub edition: Option<String>,
    #[serde(default)]
    pub doctest: bool,
    #[serde(default)]
    pub test: bool,
    #[serde(default)]
    pub doc: bool,
    /// `cargo metadata` uses the kebab-case key `required-features`.
    #[serde(default, rename = "required-features")]
    pub required_features: Vec<String>,
}

impl TargetRecord {
    fn has_kind(&self, kind: &str) -> bool {
        self.kind.iter().any(|candidate| candidate == kind)
    }

    /// A library target (including proc-macro and the cdylib/staticlib forms).
    pub fn is_library(&self) -> bool {
        self.has_kind("lib")
            || self.has_kind("proc-macro")
            || self.has_kind("cdylib")
            || self.has_kind("staticlib")
            || self.has_kind("dylib")
            || self.crate_types.iter().any(|kind| {
                matches!(
                    kind.as_str(),
                    "lib" | "rlib" | "dylib" | "proc-macro" | "cdylib" | "staticlib"
                )
            })
    }

    /// A `[[test]]` integration target.
    pub fn is_integration_test(&self) -> bool {
        self.has_kind("test")
    }

    /// A binary target (bin), which can carry `#[test]` unit tests.
    pub fn is_binary(&self) -> bool {
        self.has_kind("bin")
    }

    pub fn is_example(&self) -> bool {
        self.has_kind("example")
    }

    pub fn is_benchmark(&self) -> bool {
        self.has_kind("bench")
    }

    pub fn is_build_script(&self) -> bool {
        self.has_kind("custom-build")
    }

    pub fn is_proc_macro(&self) -> bool {
        self.has_kind("proc-macro")
    }

    /// Whether this target's unit tests are selected by `cargo test`.
    pub fn has_unit_tests(&self) -> bool {
        self.test && (self.is_library() || self.is_binary())
    }

    /// Whether the target can be selected with `--test <name>`.
    pub fn is_named_test_target(&self) -> bool {
        self.is_integration_test()
    }
}

/// A package's dependency edge, as `cargo metadata` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyRecord {
    pub name: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub req: Option<String>,
    /// `null` for a normal dependency, otherwise `"dev"` or `"build"`.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub rename: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub target: Option<String>,
}

impl DependencyRecord {
    /// Normal, dev or build. Cargo's `kind` is `None` for a normal edge.
    pub fn kind_name(&self) -> &str {
        self.kind.as_deref().unwrap_or("normal")
    }
}

/// One workspace package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub manifest_path: Option<String>,
    #[serde(default)]
    pub targets: Vec<TargetRecord>,
    #[serde(default)]
    pub dependencies: Vec<DependencyRecord>,
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,
}

impl PackageRecord {
    /// The directory that owns this package's manifest.
    pub fn manifest_dir(&self) -> Option<String> {
        self.manifest_path.as_deref().and_then(manifest_dir)
    }

    /// The library target, if any.
    pub fn library(&self) -> Option<&TargetRecord> {
        self.targets.iter().find(|target| target.is_library())
    }

    /// Integration (`[[test]]`) targets, by name.
    pub fn integration_tests(&self) -> impl Iterator<Item = &TargetRecord> {
        self.targets
            .iter()
            .filter(|target| target.is_integration_test())
    }

    /// Binary targets that can carry unit tests.
    pub fn testable_binaries(&self) -> impl Iterator<Item = &TargetRecord> {
        self.targets
            .iter()
            .filter(|target| target.is_binary() && target.test)
    }

    /// A named target of any kind.
    pub fn target(&self, name: &str) -> Option<&TargetRecord> {
        self.targets.iter().find(|target| target.name == name)
    }

    /// Whether the package declares at least one feature a build can turn on.
    pub fn has_features(&self) -> bool {
        !self.features.is_empty()
    }

    /// Whether the package's source tree is a build script or proc-macro
    /// producer, whose changes have a wider, less predictable impact.
    pub fn has_build_script_or_proc_macro(&self) -> bool {
        self.targets
            .iter()
            .any(|target| target.is_build_script() || target.is_proc_macro())
    }
}

/// The manifest directory of a manifest path (`/a/b/Cargo.toml` → `/a/b`).
pub fn manifest_dir(manifest_path: &str) -> Option<String> {
    let normalized = normalize_path(manifest_path);
    match normalized.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => Some(dir.to_string()),
        // An absolute manifest at the filesystem root owns "/".
        Some(_) => Some("/".to_string()),
        None if normalized == "Cargo.toml" => Some(String::new()),
        None => None,
    }
}

/// Normalize a path for comparison: backslashes become `/`, and a trailing
/// slash is dropped. Identity is still host-owned (design §3); this is only a
/// lexical helper for matching a saved absolute path against a workspace root.
pub fn normalize_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Whether `path` is `dir` or lives under it, lexically.
pub fn path_is_under(path: &str, dir: &str) -> bool {
    let path = normalize_path(path);
    let dir = normalize_path(dir);
    if dir == "/" {
        return path.starts_with('/');
    }
    path == dir || path.starts_with(&format!("{dir}/"))
}

/// A host-approved build configuration (design §3, "構成キー").
///
/// The values that are sensitive or environment-specific (Cargo config,
/// rustflags, wrapper, runner, build environment) are held as opaque digests:
/// the host compares them, and the model sees only the fingerprint. The host
/// and target triples are kept distinct because build scripts and proc-macros
/// run on the host while the output targets the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Configuration {
    pub id: String,
    #[serde(default)]
    pub toolchain: Option<String>,
    #[serde(default)]
    pub host_triple: Option<String>,
    #[serde(default)]
    pub target_triple: Option<String>,
    /// `dev`, `test`, `release`, or a custom profile name.
    #[serde(default)]
    pub profile: Option<String>,
    /// Selected packages; empty means the workspace default members.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Features requested on the command line (not the resolved set).
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub all_features: bool,
    #[serde(default)]
    pub no_default_features: bool,
    /// Named targets the configuration restricts to; empty means none.
    #[serde(default)]
    pub selected_targets: Vec<String>,
    #[serde(default)]
    pub cargo_config_digest: Option<String>,
    #[serde(default)]
    pub lock_digest: Option<String>,
    #[serde(default)]
    pub rustflags_digest: Option<String>,
    #[serde(default)]
    pub env_digest: Option<String>,
}

impl Configuration {
    /// A stable fingerprint of every field that can change a build.
    ///
    /// It is an identity hint for detecting a changed configuration between
    /// planning and execution; it is not proof that two builds are input-equal
    /// (build scripts and proc-macros can read anything; design §3).
    pub fn fingerprint(&self) -> u64 {
        let mut canonical = String::new();
        canonical.push_str(&self.id);
        canonical.push('\u{1f}');
        canonical.push_str(self.toolchain.as_deref().unwrap_or(""));
        canonical.push('\u{1f}');
        canonical.push_str(self.host_triple.as_deref().unwrap_or(""));
        canonical.push('\u{1f}');
        canonical.push_str(self.target_triple.as_deref().unwrap_or(""));
        canonical.push('\u{1f}');
        canonical.push_str(self.profile.as_deref().unwrap_or(""));
        canonical.push('\u{1f}');
        canonical.push_str(&self.packages.join("\u{1e}"));
        canonical.push('\u{1f}');
        canonical.push_str(&self.features.join("\u{1e}"));
        canonical.push('\u{1f}');
        canonical.push_str(if self.all_features { "1" } else { "0" });
        canonical.push('\u{1f}');
        canonical.push_str(if self.no_default_features { "1" } else { "0" });
        canonical.push('\u{1f}');
        canonical.push_str(&self.selected_targets.join("\u{1e}"));
        canonical.push('\u{1f}');
        canonical.push_str(self.cargo_config_digest.as_deref().unwrap_or(""));
        canonical.push('\u{1f}');
        canonical.push_str(self.lock_digest.as_deref().unwrap_or(""));
        canonical.push('\u{1f}');
        canonical.push_str(self.rustflags_digest.as_deref().unwrap_or(""));
        canonical.push('\u{1f}');
        canonical.push_str(self.env_digest.as_deref().unwrap_or(""));
        super::digest64(canonical.as_bytes())
    }

    /// Whether the configuration's target differs from the host.
    pub fn is_cross_target(&self) -> bool {
        match (&self.host_triple, &self.target_triple) {
            (Some(host), Some(target)) => host != target,
            _ => false,
        }
    }
}

/// The raw `cargo metadata` document, reduced to the fields the catalog uses.
#[derive(Debug, Clone, Deserialize)]
struct RawMetadata {
    packages: Vec<PackageRecord>,
    workspace_members: Vec<String>,
    #[serde(default)]
    workspace_default_members: Vec<String>,
    #[serde(default)]
    workspace_root: Option<String>,
    #[serde(default)]
    target_directory: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

/// A saved workspace: its packages, the workspace members, and the workspace
/// root, with the digest of the document used for staleness checks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspaceCatalog {
    packages: Vec<PackageRecord>,
    members: Vec<String>,
    default_members: Vec<String>,
    workspace_root: Option<String>,
    target_directory: Option<String>,
    digest: u64,
    /// Extra host metadata carried by `cargo metadata`'s `metadata` field,
    /// passed through untouched for a host that stores a generation there.
    metadata: Option<serde_json::Value>,
}

impl WorkspaceCatalog {
    /// Parse a saved `cargo metadata --format-version 1` document.
    pub fn from_json(json: &str) -> Result<Self, RustToolError> {
        let raw: RawMetadata = serde_json::from_str(json).map_err(|error| {
            RustToolError::metadata_unavailable(format!("invalid metadata: {error}"))
        })?;
        Ok(Self::from_raw(raw, super::digest64(json.as_bytes())))
    }

    fn from_raw(raw: RawMetadata, digest: u64) -> Self {
        let members = raw.workspace_members.clone();
        let default_members = if raw.workspace_default_members.is_empty() {
            raw.workspace_members.clone()
        } else {
            raw.workspace_default_members.clone()
        };
        Self {
            packages: raw.packages,
            members,
            default_members,
            workspace_root: raw.workspace_root,
            target_directory: raw.target_directory,
            digest,
            metadata: raw.metadata,
        }
    }

    /// The digest of the metadata document. The planner stamps it into a plan
    /// so a run can refuse to execute against different metadata (design §4).
    pub fn digest(&self) -> u64 {
        self.digest
    }

    pub fn digest_hex(&self) -> String {
        format!("{:016x}", self.digest)
    }

    pub fn workspace_root(&self) -> Option<&str> {
        self.workspace_root.as_deref()
    }

    pub fn target_directory(&self) -> Option<&str> {
        self.target_directory.as_deref()
    }

    pub fn metadata(&self) -> Option<&serde_json::Value> {
        self.metadata.as_ref()
    }

    /// Every workspace member package.
    pub fn members(&self) -> impl Iterator<Item = &PackageRecord> {
        self.members.iter().filter_map(|id| self.package_by_id(id))
    }

    /// The workspace default members (all members when Cargo does not list a
    /// narrower set).
    pub fn default_members(&self) -> impl Iterator<Item = &PackageRecord> {
        self.default_members
            .iter()
            .filter_map(|id| self.package_by_id(id))
    }

    pub fn package_by_id(&self, id: &str) -> Option<&PackageRecord> {
        self.packages.iter().find(|package| package.id == id)
    }

    pub fn package_by_name(&self, name: &str) -> Option<&PackageRecord> {
        self.packages
            .iter()
            .find(|package| package.name == name && self.is_member(&package.id))
    }

    fn is_member(&self, id: &str) -> bool {
        self.members.iter().any(|member| member == id)
    }

    /// The workspace member whose manifest directory is the longest prefix of
    /// `path` (design §7.1 step 1). An unmapped path returns `None`; the
    /// caller reports it as an unknown owner rather than assuming no impact.
    ///
    /// Both an absolute path and a workspace-relative path are accepted: the
    /// plan request's `changed_paths` are usually relative to the workspace
    /// root, while a diagnostic span may be absolute.
    pub fn package_for_path(&self, path: &str) -> Option<&PackageRecord> {
        let root = self.workspace_root.as_deref().map(normalize_path);
        let mut best: Option<(usize, &PackageRecord)> = None;
        for package in self.members() {
            let Some(dir) = package.manifest_dir() else {
                continue;
            };
            let matches = path_is_under(path, &dir)
                || root.as_deref().is_some_and(|root| {
                    if normalize_path(&dir) == root {
                        !path.starts_with('/')
                    } else if let Some(relative) = dir.strip_prefix(&format!("{root}/")) {
                        path_is_under(path, relative)
                    } else {
                        false
                    }
                });
            if matches {
                let length = normalize_path(&dir).len();
                if best.is_none_or(|(best_len, _)| length > best_len) {
                    best = Some((length, package));
                }
            }
        }
        best.map(|(_, package)| package)
    }

    /// A member package's direct reverse dependencies (design §7.1 step 4).
    ///
    /// Only workspace members are returned: a dependency outside the workspace
    /// is not something this plan can test. The edge kind is kept so the caller
    /// can say whether the dependent is a normal, dev or build user. Cargo's
    /// `name` is the dependency's package name (the local alias is `rename`),
    /// so matching on `name` follows renames to the real package.
    pub fn reverse_dependencies(
        &self,
        package_id: &str,
    ) -> Vec<(&PackageRecord, &DependencyRecord)> {
        let Some(target) = self.package_by_id(package_id) else {
            return Vec::new();
        };
        let mut dependents = Vec::new();
        for package in self.members() {
            for dependency in &package.dependencies {
                if dependency.name == target.name {
                    dependents.push((package, dependency));
                }
            }
        }
        dependents.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        dependents
    }

    /// The target kinds present anywhere in the workspace, for reporting.
    pub fn target_kinds(&self) -> BTreeSet<String> {
        self.packages
            .iter()
            .flat_map(|package| package.targets.iter())
            .flat_map(|target| target.kind.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_dir_strips_the_file_name() {
        assert_eq!(manifest_dir("/a/b/Cargo.toml").as_deref(), Some("/a/b"));
        assert_eq!(manifest_dir("Cargo.toml").as_deref(), Some(""));
        assert_eq!(manifest_dir("/Cargo.toml").as_deref(), Some("/"));
    }

    #[test]
    fn path_prefixes_are_lexical_and_separator_agnostic() {
        assert!(path_is_under("/a/b/c.rs", "/a/b"));
        assert!(path_is_under("C:/a/b/c.rs", "C:/a/b"));
        assert!(!path_is_under("/a/bc/c.rs", "/a/b"));
        assert!(path_is_under("/anything", "/"));
    }
}
