//! Port of packages/coding-agent/src/core/source-info.ts (pi v0.84.3):
//! provenance metadata for loaded resources (extensions, skills, prompts,
//! themes). `PathMetadata` (upstream lives in package-manager.ts) is
//! re-declared here as a structural equivalent until the package manager
//! itself is ported.

/// Where a resource came from on disk (upstream `PathMetadata`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathMetadata {
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub base_dir: Option<String>,
}

/// Upstream `SourceScope`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceScope {
    User,
    Project,
    #[default]
    Temporary,
}

impl SourceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceScope::User => "user",
            SourceScope::Project => "project",
            SourceScope::Temporary => "temporary",
        }
    }
}

/// Upstream `SourceOrigin`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceOrigin {
    Package,
    #[default]
    TopLevel,
}

impl SourceOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceOrigin::Package => "package",
            SourceOrigin::TopLevel => "top-level",
        }
    }
}

/// Provenance of a loaded resource (upstream `SourceInfo`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub base_dir: Option<String>,
}

/// Serialize a `SourceInfo` (upstream passes the object straight to the
/// wire; camelCase `baseDir`).
pub fn source_info_to_json(info: &SourceInfo) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "path".to_string(),
        serde_json::Value::String(info.path.clone()),
    );
    obj.insert(
        "source".to_string(),
        serde_json::Value::String(info.source.clone()),
    );
    obj.insert(
        "scope".to_string(),
        serde_json::Value::String(info.scope.as_str().to_string()),
    );
    obj.insert(
        "origin".to_string(),
        serde_json::Value::String(info.origin.as_str().to_string()),
    );
    if let Some(base_dir) = &info.base_dir {
        obj.insert(
            "baseDir".to_string(),
            serde_json::Value::String(base_dir.clone()),
        );
    }
    serde_json::Value::Object(obj)
}

/// Build a `SourceInfo` from package-manager path metadata (upstream
/// `createSourceInfo`).
pub fn create_source_info(path: &str, metadata: &PathMetadata) -> SourceInfo {
    SourceInfo {
        path: path.to_string(),
        source: metadata.source.clone(),
        scope: metadata.scope,
        origin: metadata.origin,
        base_dir: metadata.base_dir.clone(),
    }
}

/// Build a synthetic `SourceInfo` for runtime-constructed resources
/// (upstream `createSyntheticSourceInfo`).
pub fn create_synthetic_source_info(path: &str, options: SyntheticSourceOptions) -> SourceInfo {
    SourceInfo {
        path: path.to_string(),
        source: options.source,
        scope: options.scope.unwrap_or(SourceScope::Temporary),
        origin: options.origin.unwrap_or(SourceOrigin::TopLevel),
        base_dir: options.base_dir,
    }
}

/// Options for `create_synthetic_source_info`.
#[derive(Debug, Clone, Default)]
pub struct SyntheticSourceOptions {
    pub source: String,
    pub scope: Option<SourceScope>,
    pub origin: Option<SourceOrigin>,
    pub base_dir: Option<String>,
}
