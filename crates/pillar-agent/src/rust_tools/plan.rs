//! The verification planner (design §7): changed paths → packages → targets →
//! explicit, host-approved `cargo test` commands, plus the limits the plan
//! does *not* cover.
//!
//! The planner is pure and metadata-only. It never runs Cargo (design §4,
//! `rs_verify_plan` "Cargoを隠れて起動しない") and never accepts an arbitrary
//! shell string from the model: a configuration id resolves to a
//! host-approved [`Configuration`] (design §7.1), and every argv is built from
//! the catalog.
//!
//! Two rules from §7.2 are encoded as code, not prose:
//!
//! - a package with a library gets `--lib`, because `--test NAME` alone does
//!   not run the package's unit tests; a package without a library does not
//!   get `--lib`;
//! - what was *not* selected is reported as a structured [`Unverified`], so a
//!   green run cannot be read as "everything passed".

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::RustToolError;
use super::metadata::{Configuration, PackageRecord, WorkspaceCatalog, normalize_path};

/// Why the caller is verifying (design §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanGoal {
    /// Validate a change that is about to be made or was just made.
    ValidateChange,
    /// A checkpoint sweep: broader than one focused change.
    Checkpoint,
    /// Gather evidence without asserting a change.
    Investigate,
}

/// How wide to plan (design §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanScope {
    /// The changed packages only.
    Focused,
    /// The changed packages and their workspace reverse dependents.
    Package,
    /// The whole workspace.
    Workspace,
}

/// The planner's request (design §4, example).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRequest {
    pub changed_paths: Vec<String>,
    pub configuration_ids: Vec<String>,
    pub goal: PlanGoal,
    pub scope: PlanScope,
    /// Integration target names to include explicitly (design §7.2).
    #[serde(default)]
    pub requested_targets: Vec<String>,
    /// The metadata digest the caller planned against. When present and
    /// different from the catalog, the plan is refused as stale (design §4).
    #[serde(default)]
    pub metadata_digest: Option<String>,
}

/// What one step's run is intended to cover (design §7.2). Typed, not a free
/// string, so a caller cannot over-read a green run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Coverage {
    UnitTests { package: String },
    BinaryUnitTests { package: String },
    IntegrationTarget { package: String, target: String },
    AllTargets { package: String },
}

/// A dimension the plan does not verify (design §7.2, §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnverifiedKind {
    /// Workspace reverse dependents were not run.
    ReverseDependents,
    /// Doctests are a separate coverage dimension.
    Doctests,
    /// A non-default feature combination was not exercised.
    NonDefaultFeatures,
    /// Other integration / example / bench targets were not selected.
    OtherTargets,
    /// The target triple differs from the host, so a successful build is not
    /// proof that the target's tests ran.
    CrossTarget,
    /// A build script, proc-macro or other shared input changed; static impact
    /// analysis is incomplete.
    BuildScriptOrMacro,
    /// A manifest, lockfile or Cargo configuration changed; the saved metadata
    /// may not describe the new resolution.
    MetadataStale,
    /// A requested target needs features the configuration does not enable.
    RequiredFeatures,
    /// A changed path did not map to a workspace package.
    UnknownOwners,
}

/// One unverified dimension, with the detail the caller needs to judge it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unverified {
    pub kind: UnverifiedKind,
    pub detail: String,
}

/// One explicit run the caller may authorize (design §4, example).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyStep {
    pub step_id: String,
    pub configuration_id: String,
    pub argv: Vec<String>,
    pub covers: Vec<Coverage>,
    pub reason: String,
}

/// A saved plan. `execution_started` is always `false`: the planner never runs
/// anything (design §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyPlan {
    pub plan_id: String,
    pub metadata_digest: String,
    pub configuration_fingerprints: BTreeMap<String, String>,
    pub steps: Vec<VerifyStep>,
    pub unverified: Vec<Unverified>,
    pub notes: Vec<String>,
    pub execution_started: bool,
}

impl VerifyPlan {
    /// A digest over the steps' commands, for the broker's duplicate-start
    /// suppression key (design §10).
    pub fn command_digest(&self) -> u64 {
        let mut canonical = String::new();
        for step in &self.steps {
            canonical.push_str(&step.configuration_id);
            canonical.push('\u{1f}');
            canonical.push_str(&step.argv.join("\u{1e}"));
            canonical.push('\n');
        }
        super::digest64(canonical.as_bytes())
    }
}

/// Plan a focused verification from saved metadata and approved
/// configurations.
pub fn plan(
    catalog: &WorkspaceCatalog,
    configurations: &[Configuration],
    request: &PlanRequest,
) -> Result<VerifyPlan, RustToolError> {
    if request.changed_paths.is_empty() {
        return Err(RustToolError::invalid_request(
            "changed_paths must list at least one path",
        ));
    }
    if request.configuration_ids.is_empty() {
        return Err(RustToolError::invalid_request(
            "configuration_ids is required; the host approves configurations",
        ));
    }
    if let Some(expected) = &request.metadata_digest
        && *expected != catalog.digest_hex()
    {
        return Err(RustToolError::stale_plan(
            "the saved metadata does not match the digest this request was planned against",
        ));
    }

    let mut selected: Vec<&Configuration> = Vec::new();
    for id in &request.configuration_ids {
        let configuration = configurations
            .iter()
            .find(|configuration| configuration.id == *id)
            .ok_or_else(|| {
                RustToolError::configuration_mismatch(format!(
                    "configuration id {id} is not approved by the host"
                ))
            })?;
        selected.push(configuration);
    }

    // 1-3: map changed paths to packages, or to a broad reason.
    let mut package_ids: BTreeSet<String> = BTreeSet::new();
    let mut unverified: Vec<Unverified> = Vec::new();
    let mut broad = false;
    for path in &request.changed_paths {
        match classify_change(catalog, path) {
            Change::Package(package) => {
                package_ids.insert(package.id.clone());
            }
            Change::Broad(kind) => {
                broad = true;
                unverified.push(Unverified {
                    kind,
                    detail: format!("changed path: {}", normalize_path(path)),
                });
            }
            Change::Unknown => {
                unverified.push(Unverified {
                    kind: UnverifiedKind::UnknownOwners,
                    detail: format!("changed path: {}", normalize_path(path)),
                });
            }
        }
    }
    if broad {
        for package in catalog.members() {
            package_ids.insert(package.id.clone());
        }
    }
    if request.scope == PlanScope::Workspace {
        for package in catalog.members() {
            package_ids.insert(package.id.clone());
        }
    }
    if package_ids.is_empty() {
        return Err(RustToolError::invalid_request(
            "no changed path maps to a workspace package; name a member path or a manifest",
        ));
    }

    let packages: Vec<&PackageRecord> = package_ids
        .iter()
        .filter_map(|id| catalog.package_by_id(id))
        .collect();

    // Requested integration targets must exist somewhere among the affected
    // packages (design §4: point it out at plan time).
    let mut missing_targets: Vec<String> = Vec::new();
    for target in &request.requested_targets {
        let found = packages.iter().any(|package| {
            package
                .target(target)
                .is_some_and(|t| t.is_named_test_target())
        });
        if !found {
            missing_targets.push(target.clone());
        }
    }
    if !missing_targets.is_empty() {
        return Err(RustToolError::invalid_request(format!(
            "requested test targets do not exist among the affected packages: {}",
            missing_targets.join(", ")
        )));
    }

    // Scope: add reverse dependents when asked.
    let mut scope_packages = packages.clone();
    if request.scope != PlanScope::Focused {
        let mut extra: BTreeSet<String> = BTreeSet::new();
        for package in &packages {
            for (dependent, _) in catalog.reverse_dependencies(&package.id) {
                extra.insert(dependent.id.clone());
            }
        }
        for id in extra {
            if let Some(package) = catalog.package_by_id(&id)
                && !scope_packages
                    .iter()
                    .any(|existing| existing.id == package.id)
            {
                scope_packages.push(package);
            }
        }
    }

    let mut steps: Vec<VerifyStep> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut configuration_fingerprints = BTreeMap::new();
    for configuration in &selected {
        configuration_fingerprints.insert(
            configuration.id.clone(),
            format!("{:016x}", configuration.fingerprint()),
        );
        for package in &scope_packages {
            let (argv, covers, applicable_targets) = build_step(
                configuration,
                package,
                &request.requested_targets,
                &mut notes,
            )?;
            for target in &request.requested_targets {
                if package
                    .target(target)
                    .is_some_and(|t| t.is_named_test_target())
                    && !applicable_targets.contains(target)
                {
                    unverified.push(Unverified {
                        kind: UnverifiedKind::RequiredFeatures,
                        detail: format!(
                            "target {target} in {} needs features configuration {} does not enable",
                            package.name, configuration.id
                        ),
                    });
                }
            }
            steps.push(VerifyStep {
                step_id: format!("s{}", steps.len() + 1),
                configuration_id: configuration.id.clone(),
                argv,
                covers: covers.clone(),
                reason: step_reason(package, &covers),
            });
        }
    }

    // Unverified dimensions (design §7.2, §7.3).
    let doctest_packages: BTreeSet<String> = scope_packages
        .iter()
        .filter(|package| package.library().is_some())
        .map(|package| package.name.clone())
        .collect();
    if !doctest_packages.is_empty() {
        unverified.push(Unverified {
            kind: UnverifiedKind::Doctests,
            detail: format!(
                "no step ran doctests for: {}",
                doctest_packages.into_iter().collect::<Vec<_>>().join(", ")
            ),
        });
    }
    for configuration in &selected {
        if !configuration.all_features {
            let featureless: Vec<&str> = scope_packages
                .iter()
                .filter(|package| package.has_features())
                .map(|package| package.name.as_str())
                .collect();
            if !featureless.is_empty() {
                unverified.push(Unverified {
                    kind: UnverifiedKind::NonDefaultFeatures,
                    detail: format!(
                        "configuration {} did not enable all features of: {}",
                        configuration.id,
                        featureless.join(", ")
                    ),
                });
            }
        }
        if configuration.is_cross_target() {
            unverified.push(Unverified {
                kind: UnverifiedKind::CrossTarget,
                detail: format!(
                    "configuration {} targets {} while the host is {}; a successful build is not a test run",
                    configuration.id,
                    configuration.target_triple.as_deref().unwrap_or("?"),
                    configuration.host_triple.as_deref().unwrap_or("?")
                ),
            });
        }
        if !configuration.features.is_empty() || configuration.no_default_features {
            unverified.push(Unverified {
                kind: UnverifiedKind::NonDefaultFeatures,
                detail: format!(
                    "configuration {} exercises only features [{}] with default-features={}",
                    configuration.id,
                    configuration.features.join(", "),
                    !configuration.no_default_features
                ),
            });
        }
    }
    if request.scope == PlanScope::Focused {
        for package in &packages {
            let dependents = catalog.reverse_dependencies(&package.id);
            if !dependents.is_empty() {
                unverified.push(Unverified {
                    kind: UnverifiedKind::ReverseDependents,
                    detail: format!(
                        "{} reverse dependents of {} were not run: {}",
                        dependents.len(),
                        package.name,
                        dependents
                            .iter()
                            .map(|(dependent, _)| dependent.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
        }
    }
    for package in &scope_packages {
        let other: Vec<&str> = package
            .integration_tests()
            .map(|target| target.name.as_str())
            .filter(|name| {
                !request
                    .requested_targets
                    .iter()
                    .any(|requested| requested == name)
            })
            .collect();
        if !other.is_empty() {
            unverified.push(Unverified {
                kind: UnverifiedKind::OtherTargets,
                detail: format!(
                    "{} integration targets of {} were not selected: {}",
                    other.len(),
                    package.name,
                    other.join(", ")
                ),
            });
        }
    }

    // Stable, de-duplicated unverified list.
    unverified.sort_by(|a, b| (a.kind, &a.detail).cmp(&(b.kind, &b.detail)));
    unverified.dedup();
    notes.sort();
    notes.dedup();

    let plan_id = {
        let mut canonical = String::new();
        canonical.push_str(&catalog.digest_hex());
        for (id, fingerprint) in &configuration_fingerprints {
            canonical.push('\u{1f}');
            canonical.push_str(id);
            canonical.push('=');
            canonical.push_str(fingerprint);
        }
        for step in &steps {
            canonical.push('\u{1f}');
            canonical.push_str(&step.argv.join("\u{1e}"));
        }
        format!("vp{:016x}", super::digest64(canonical.as_bytes()))
    };

    Ok(VerifyPlan {
        plan_id,
        metadata_digest: catalog.digest_hex(),
        configuration_fingerprints,
        steps,
        unverified,
        notes,
        execution_started: false,
    })
}

/// How one changed path is classified (design §7.1 step 3).
enum Change<'a> {
    Package(&'a PackageRecord),
    Broad(UnverifiedKind),
    Unknown,
}

fn classify_change<'a>(catalog: &'a WorkspaceCatalog, path: &str) -> Change<'a> {
    let normalized = normalize_path(path);
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    if file_name == "Cargo.toml" {
        return Change::Broad(UnverifiedKind::MetadataStale);
    }
    if file_name == "Cargo.lock" {
        return Change::Broad(UnverifiedKind::MetadataStale);
    }
    if normalized.contains("/.cargo/") {
        return Change::Broad(UnverifiedKind::MetadataStale);
    }
    match catalog.package_for_path(&normalized) {
        Some(package) => {
            if file_name == "build.rs" {
                // A build script's outputs are a broad, hard-to-trace input
                // (design §7.2): static impact analysis is incomplete.
                Change::Broad(UnverifiedKind::BuildScriptOrMacro)
            } else {
                Change::Package(package)
            }
        }
        None => Change::Unknown,
    }
}

fn build_step(
    configuration: &Configuration,
    package: &PackageRecord,
    requested_targets: &[String],
    notes: &mut Vec<String>,
) -> Result<(Vec<String>, Vec<Coverage>, BTreeSet<String>), RustToolError> {
    let mut argv = vec![
        "cargo".to_string(),
        "test".to_string(),
        "--locked".to_string(),
        "-p".to_string(),
        package.name.clone(),
    ];
    if configuration.no_default_features {
        argv.push("--no-default-features".to_string());
    }
    if configuration.all_features {
        argv.push("--all-features".to_string());
    } else if !configuration.features.is_empty() {
        argv.push("--features".to_string());
        argv.push(configuration.features.join(","));
    }
    if configuration.profile.as_deref() == Some("release") {
        argv.push("--release".to_string());
    }

    let mut covers = Vec::new();
    let mut applicable_targets = BTreeSet::new();
    if package.library().is_some() {
        argv.push("--lib".to_string());
        covers.push(Coverage::UnitTests {
            package: package.name.clone(),
        });
    }
    let binaries: Vec<&str> = package
        .testable_binaries()
        .map(|target| target.name.as_str())
        .collect();
    if !binaries.is_empty() {
        argv.push("--bins".to_string());
        covers.push(Coverage::BinaryUnitTests {
            package: package.name.clone(),
        });
    }
    for target_name in requested_targets {
        let Some(target) = package
            .target(target_name)
            .filter(|target| target.is_named_test_target())
        else {
            continue;
        };
        if !features_satisfied(configuration, target) {
            continue;
        }
        argv.push("--test".to_string());
        argv.push(target_name.clone());
        applicable_targets.insert(target_name.clone());
        covers.push(Coverage::IntegrationTarget {
            package: package.name.clone(),
            target: target_name.clone(),
        });
    }
    if covers.is_empty() {
        argv.push("--all-targets".to_string());
        covers.push(Coverage::AllTargets {
            package: package.name.clone(),
        });
        notes.push(format!(
            "{} has no unit-test target; --all-targets selects every target of the package",
            package.name
        ));
    }
    argv.push("--message-format=json".to_string());

    Ok((argv, covers, applicable_targets))
}

/// Whether a configuration's requested feature set satisfies a target's
/// `required-features`. `--all-features` satisfies everything; otherwise every
/// required feature must be requested explicitly (a default feature is not
/// assumed, to stay conservative).
fn features_satisfied(
    configuration: &Configuration,
    target: &super::metadata::TargetRecord,
) -> bool {
    if target.required_features.is_empty() || configuration.all_features {
        return true;
    }
    target.required_features.iter().all(|feature| {
        configuration
            .features
            .iter()
            .any(|enabled| enabled == feature)
    })
}

fn step_reason(package: &PackageRecord, covers: &[Coverage]) -> String {
    let unit = covers.iter().any(|coverage| {
        matches!(
            coverage,
            Coverage::UnitTests { .. } | Coverage::BinaryUnitTests { .. }
        )
    });
    let integration = covers
        .iter()
        .any(|coverage| matches!(coverage, Coverage::IntegrationTarget { .. }));
    match (unit, integration) {
        (true, true) => format!(
            "{}: unit tests and the explicitly related integration targets",
            package.name
        ),
        (true, false) => format!("{}: unit tests", package.name),
        (false, true) => format!("{}: the requested integration targets", package.name),
        (false, false) => format!("{}: every target (no unit-test target)", package.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_gate_a_required_feature_target() {
        let target = super::super::metadata::TargetRecord {
            name: "contract".to_string(),
            kind: vec!["test".to_string()],
            crate_types: vec![],
            src_path: None,
            edition: None,
            doctest: false,
            test: true,
            doc: false,
            required_features: vec!["extra".to_string()],
        };
        let mut configuration = Configuration {
            id: "default".to_string(),
            toolchain: None,
            host_triple: None,
            target_triple: None,
            profile: None,
            packages: vec![],
            features: vec![],
            all_features: false,
            no_default_features: false,
            selected_targets: vec![],
            cargo_config_digest: None,
            lock_digest: None,
            rustflags_digest: None,
            env_digest: None,
        };
        assert!(!features_satisfied(&configuration, &target));
        configuration.features = vec!["extra".to_string()];
        assert!(features_satisfied(&configuration, &target));
        configuration.features.clear();
        configuration.all_features = true;
        assert!(features_satisfied(&configuration, &target));
    }
}
