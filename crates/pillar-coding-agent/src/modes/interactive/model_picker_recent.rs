//! Recent-model history for the interactive 2-column model picker.
//!
//! Not an upstream component: this ports the history side of the user's
//! `pi-model-picker` extension (<https://github.com/vi2q/pi-model-picker>),
//! which keeps the last four switched-to models in
//! `<agent dir>/model-picker-recent.json`:
//!
//! ```json
//! { "recent": [{ "provider": "opencode-go", "id": "omen-alpha" }] }
//! ```
//!
//! The file shape is the extension's, so a pi install and the port share one
//! history. A failed read or write is never fatal (upstream wraps both in
//! `try`/`catch` and keeps an empty history).

use std::path::{Path, PathBuf};

/// Upstream `MAX_RECENT`: the history is hardcoded to four entries.
pub const MAX_RECENT: usize = 4;

/// The history file name inside the agent directory (upstream `RECENT_FILE`).
pub const RECENT_FILE_NAME: &str = "model-picker-recent.json";

/// One history entry (upstream `RecentEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentEntry {
    pub provider: String,
    pub id: String,
}

/// The picker's recent-model history (upstream the module-level `recents`).
#[derive(Debug, Clone)]
pub struct RecentModels {
    /// The history file; `None` when the host has no agent directory, in which
    /// case recording is a no-op (a test or an embedding host must not write a
    /// stray file into the working directory).
    path: Option<PathBuf>,
    entries: Vec<RecentEntry>,
}

impl RecentModels {
    /// The history file inside `agent_dir`.
    pub fn path_in(agent_dir: &Path) -> PathBuf {
        agent_dir.join(RECENT_FILE_NAME)
    }

    /// Load the history from `agent_dir` (an unreadable or malformed file
    /// reads as empty, like upstream).
    pub fn load(agent_dir: &Path) -> Self {
        let path = Self::path_in(agent_dir);
        let entries = read_entries(&path);
        Self {
            path: Some(path),
            entries,
        }
    }

    /// A history that is never read from or written to (a host without an
    /// agent directory).
    pub fn disabled() -> Self {
        Self {
            path: None,
            entries: Vec::new(),
        }
    }

    /// An in-memory history with an explicit path (tests).
    pub fn in_memory(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            entries: Vec::new(),
        }
    }

    /// The entries, newest first.
    pub fn entries(&self) -> &[RecentEntry] {
        &self.entries
    }

    /// Upstream `recordRecent`: move the model to the front, drop duplicates
    /// and cap the history, then persist.
    pub fn record(&mut self, provider: &str, id: &str) {
        self.entries
            .retain(|entry| !(entry.provider == provider && entry.id == id));
        self.entries.insert(
            0,
            RecentEntry {
                provider: provider.to_string(),
                id: id.to_string(),
            },
        );
        self.entries.truncate(MAX_RECENT);
        self.save();
    }

    /// Persist the history (upstream `saveRecents`; failures are ignored).
    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let payload = serde_json::json!({
            "recent": self
                .entries
                .iter()
                .map(|entry| serde_json::json!({ "provider": entry.provider, "id": entry.id }))
                .collect::<Vec<_>>(),
        });
        let json = match serde_json::to_string_pretty(&payload) {
            Ok(json) => json,
            Err(_) => return,
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // A failed history write must never take the picker down (upstream).
        let _ = std::fs::write(path, json);
    }
}

/// Read the history file, ignoring anything malformed (upstream `loadRecents`).
fn read_entries(path: &Path) -> Vec<RecentEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let Some(recent) = value.get("recent").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    recent
        .iter()
        .filter_map(|entry| {
            let provider = entry.get("provider")?.as_str()?;
            let id = entry.get("id")?.as_str()?;
            Some(RecentEntry {
                provider: provider.to_string(),
                id: id.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pillar-recent-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn records_newest_first_without_duplicates_and_caps_at_four() {
        let dir = temp_dir("record");
        let mut recent = RecentModels::load(&dir);
        for id in ["a", "b", "c", "d", "e"] {
            recent.record("p", id);
        }
        let ids: Vec<&str> = recent.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["e", "d", "c", "b"], "newest first, capped");

        // Re-recording an entry moves it to the front instead of duplicating.
        recent.record("p", "c");
        let ids: Vec<&str> = recent.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "e", "d", "b"]);

        // The same id under another provider is a different entry.
        recent.record("q", "c");
        assert_eq!(recent.entries().len(), 4);
        assert_eq!(
            (
                recent.entries()[0].provider.as_str(),
                recent.entries()[0].id.as_str()
            ),
            ("q", "c")
        );
    }

    #[test]
    fn round_trips_through_the_agent_directory() {
        let dir = temp_dir("round-trip");
        let mut recent = RecentModels::load(&dir);
        recent.record("opencode-go", "omen-alpha");
        recent.record("anthropic", "claude-opus-5");

        let reloaded = RecentModels::load(&dir);
        assert_eq!(
            reloaded.entries(),
            &[
                RecentEntry {
                    provider: "anthropic".to_string(),
                    id: "claude-opus-5".to_string()
                },
                RecentEntry {
                    provider: "opencode-go".to_string(),
                    id: "omen-alpha".to_string()
                }
            ]
        );
        // The extension's file shape (shared with a pi install).
        let raw = std::fs::read_to_string(RecentModels::path_in(&dir)).expect("history file");
        assert!(raw.contains("\"recent\""), "{raw}");
        assert!(raw.contains("\"provider\": \"anthropic\""), "{raw}");
    }

    #[test]
    fn disabled_history_never_touches_the_filesystem() {
        let mut recent = RecentModels::disabled();
        recent.record("p", "a");
        // The entry is tracked in memory but nothing is written.
        assert_eq!(recent.entries().len(), 1);
        assert!(recent.path.is_none());
    }

    #[test]
    fn malformed_or_missing_history_reads_as_empty() {
        let dir = temp_dir("malformed");
        assert!(
            RecentModels::load(&dir).entries().is_empty(),
            "missing file"
        );

        std::fs::write(RecentModels::path_in(&dir), "{ not json").unwrap();
        assert!(RecentModels::load(&dir).entries().is_empty(), "bad json");

        std::fs::write(RecentModels::path_in(&dir), "{\"recent\": 3}").unwrap();
        assert!(RecentModels::load(&dir).entries().is_empty(), "bad shape");

        // Entries missing provider/id are dropped, valid ones survive.
        std::fs::write(
            RecentModels::path_in(&dir),
            "{\"recent\":[{\"provider\":\"p\"},{\"provider\":\"p\",\"id\":\"a\"}]}",
        )
        .unwrap();
        let loaded = RecentModels::load(&dir);
        assert_eq!(loaded.entries().len(), 1);
        assert_eq!(loaded.entries()[0].id, "a");
    }
}
