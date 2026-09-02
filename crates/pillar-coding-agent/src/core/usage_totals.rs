//! Port of packages/coding-agent/src/core/usage-totals.ts,
//! session-export.ts, experimental.ts, and settings-diagnostics.ts (pi
//! v0.84.3): usage accounting grouped by model, JSONL session export,
//! experimental feature flags, and settings diagnostics shaping.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use pillar_ai::types::Usage;
use serde_json::Value;

use crate::core::session_entries::SessionEntry as Entry;
use crate::core::session_manager::{CURRENT_SESSION_VERSION, SessionHeader, entry_to_json};

// ============================================================================
// usage-totals.ts
// ============================================================================

/// Cumulative usage totals (upstream `UsageTotals`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl UsageTotals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one usage record (upstream `addUsageToTotals`).
    pub fn add(&mut self, usage: &Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.cost += usage.cost.total;
    }
}

/// One breakdown bucket (upstream `UsageCostBreakdownEntry`).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageCostBreakdownEntry {
    pub key: String,
    pub cost: f64,
    pub tokens: u64,
}

/// Group attributable assistant usage by model and all other usage into a
/// separate "Tools/summaries" bucket (upstream `getUsageCostBreakdown`).
/// Results are sorted by cost descending and empty buckets are dropped.
pub fn get_usage_cost_breakdown(entries: &[Entry]) -> Vec<UsageCostBreakdownEntry> {
    let mut totals_by_key: BTreeMap<String, UsageTotals> = BTreeMap::new();

    for entry in entries {
        let (key, usage): (String, &Usage) = match entry {
            Entry::Message(message_entry) => match &message_entry.message {
                crate::core::messages::CodingAgentMessage::Base(
                    pillar_ai::types::Message::Assistant(assistant),
                ) => (
                    format!(
                        "{}/{}",
                        assistant.provider,
                        assistant
                            .response_model
                            .as_ref()
                            .unwrap_or(&assistant.model)
                    ),
                    &assistant.usage,
                ),
                crate::core::messages::CodingAgentMessage::Base(
                    pillar_ai::types::Message::ToolResult(result),
                ) if result.usage.is_some() => (
                    "Tools/summaries".to_string(),
                    result.usage.as_ref().expect("checked"),
                ),
                _ => continue,
            },
            Entry::BranchSummary(branch) if branch.usage.is_some() => (
                "Tools/summaries".to_string(),
                branch.usage.as_ref().expect("checked"),
            ),
            Entry::Compaction(compaction) if compaction.usage.is_some() => (
                "Tools/summaries".to_string(),
                compaction.usage.as_ref().expect("checked"),
            ),
            _ => continue,
        };

        totals_by_key.entry(key).or_default().add(usage);
    }

    let mut breakdown: Vec<UsageCostBreakdownEntry> = totals_by_key
        .into_iter()
        .map(|(key, totals)| UsageCostBreakdownEntry {
            tokens: totals.input + totals.output + totals.cache_read + totals.cache_write,
            cost: totals.cost,
            key,
        })
        .filter(|entry| entry.cost > 0.0 || entry.tokens > 0)
        .collect();
    breakdown.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    breakdown
}

// ============================================================================
// session-export.ts
// ============================================================================

/// Write the current session branch and optional trailing export-only
/// entries as JSONL (upstream `exportSessionToJsonl`). Returns the file
/// path written.
/// Trailing entry factory (upstream `createTrailingEntries`).
pub type TrailingEntriesFn = dyn Fn(Option<&str>) -> Vec<Value>;

pub fn export_session_to_jsonl(
    session_id: &str,
    cwd: &str,
    branch: &[Entry],
    output_path: Option<&Path>,
    create_trailing_entries: Option<&TrailingEntriesFn>,
) -> Result<PathBuf, String> {
    let file_path = output_path.map(Path::to_path_buf).unwrap_or_else(|| {
        let timestamp = iso_now();
        PathBuf::from(format!(
            "session-{}.jsonl",
            timestamp.replace([':', '.'], "-")
        ))
    });
    if let Some(dir) = file_path.parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            fs::create_dir_all(dir).map_err(|e| format!("Failed to create export dir: {e}"))?;
        }
    }

    let timestamp = iso_now();
    let header = SessionHeader {
        r#type: "session".to_string(),
        version: Some(CURRENT_SESSION_VERSION),
        id: session_id.to_string(),
        timestamp,
        cwd: cwd.to_string(),
        parent_session: None,
    };
    let mut lines = vec![serde_json::to_string(&header).unwrap_or_default()];

    let mut parent_id: Option<String> = None;
    for entry in branch {
        let mut json = entry_to_json(entry);
        if let Some(object) = json.as_object_mut() {
            object.insert(
                "parentId".to_string(),
                parent_id
                    .clone()
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        lines.push(serde_json::to_string(&json).unwrap_or_default());
        parent_id = Some(entry.id().to_string());
    }
    if let Some(create_trailing_entries) = create_trailing_entries {
        for entry in create_trailing_entries(parent_id.as_deref()) {
            lines.push(serde_json::to_string(&entry).unwrap_or_default());
        }
    }

    let mut content = lines.join("\n");
    content.push('\n');
    fs::write(&file_path, content).map_err(|e| format!("Failed to write export: {e}"))?;
    Ok(file_path)
}

fn iso_now() -> String {
    let now_ms = pillar_ai::models::now_ms();
    let secs = (now_ms / 1000) as i64;
    let millis = (now_ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ============================================================================
// experimental.ts
// ============================================================================

/// Upstream `PREFER_STRICT_TOOL_SAMPLING`.
pub fn experimental_tool_sampling() -> Option<serde_json::Value> {
    are_experimental_features_enabled()
        .then(|| serde_json::json!({"type": "json_schema", "strict": "prefer"}))
}

/// Upstream `areExperimentalFeaturesEnabled` (`PI_EXPERIMENTAL === "1"`).
pub fn are_experimental_features_enabled() -> bool {
    std::env::var("PI_EXPERIMENTAL").ok().as_deref() == Some("1")
}

// ============================================================================
// settings-diagnostics.ts
// ============================================================================

/// A runtime diagnostic (upstream `AgentSessionRuntimeDiagnostic` warning
/// shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDiagnostic {
    pub message: String,
}

/// Map settings-manager errors to warning diagnostics (upstream
/// `collectSettingsDiagnostics`).
pub fn collect_settings_diagnostics(
    errors: Vec<crate::core::settings_manager::SettingsError>,
) -> Vec<RuntimeDiagnostic> {
    errors
        .into_iter()
        .map(|error| RuntimeDiagnostic {
            message: match &error.path {
                Some(path) => format!(
                    "Invalid settings file {}: {}",
                    path.display(),
                    error.message
                ),
                None => format!(
                    "Invalid {} settings: {}",
                    match error.scope {
                        crate::core::settings_manager::SettingsScope::Global => "global",
                        crate::core::settings_manager::SettingsScope::Project => "project",
                    },
                    error.message
                ),
            },
        })
        .collect()
}

/// Remove duplicate type/message diagnostics preserving first occurrences
/// (upstream `deduplicateDiagnostics`).
pub fn deduplicate_diagnostics(diagnostics: &[RuntimeDiagnostic]) -> Vec<RuntimeDiagnostic> {
    let mut seen = std::collections::BTreeSet::new();
    diagnostics
        .iter()
        .filter(|diagnostic| seen.insert(diagnostic.message.clone()))
        .cloned()
        .collect()
}
