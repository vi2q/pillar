//! Port of packages/agent/src/harness/session/state.ts (pi v0.84.3).
//!
//! `SessionState` is the shared mutation core: consecutive-sequence
//! validation, id uniqueness, lane leaf tracking, open-operation tracking,
//! stats accumulation, and the durable log.

use std::collections::{BTreeMap, BTreeSet};

use super::types::{
    BranchBounds, Entry, EntryOrder, EntryQuery, ForkOptions, ForkPosition, LanePointer,
    LaneRecord, LogItem, OperationOutcome, RecordPayload, RecordQuery, SessionError,
    SessionErrorCode, SessionStats,
};

/// One pending change applied through [`SessionState::apply_mutation`]
/// (upstream `SessionMutation`).
#[derive(Debug, Clone)]
pub enum SessionMutation {
    Entry {
        lane: Option<String>,
        entry: Entry,
    },
    Record {
        record: LaneRecord,
    },
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    Name {
        seq: u64,
        name: Option<String>,
    },
    Label {
        seq: u64,
        target_id: String,
        label: Option<String>,
    },
}

fn invalid(message: &str) -> SessionError {
    SessionError::new(
        SessionErrorCode::InvalidEntry,
        format!("Invalid session mutation: {message}"),
    )
}

fn assert_valid_limit(limit: Option<usize>) -> Result<(), SessionError> {
    if let Some(limit) = limit {
        if limit == 0 {
            return Err(SessionError::new(
                SessionErrorCode::InvalidQuery,
                "limit must be a positive integer",
            ));
        }
    }
    Ok(())
}

fn assert_valid_cursor(_after_seq: Option<u64>) -> Result<(), SessionError> {
    // u64 cannot be negative; upstream rejects negative cursors, which the
    // port's u64 type makes unrepresentable.
    Ok(())
}

fn ordered<'a, T>(
    items: &'a [T],
    order: Option<EntryOrder>,
) -> Box<dyn Iterator<Item = &'a T> + 'a> {
    if order == Some(EntryOrder::OldestFirst) {
        Box::new(items.iter())
    } else {
        Box::new(items.iter().rev())
    }
}

/// Shared session mutation core (upstream `SessionState`).
#[derive(Debug, Default)]
pub struct SessionState {
    sequence: u64,
    used_ids: BTreeSet<String>,
    entries: Vec<Entry>,
    entries_by_id: BTreeMap<String, Entry>,
    records: Vec<LaneRecord>,
    /// lane -> (record id -> operation_started record), insertion ordered
    /// via the records vec ordering.
    open_operations_by_lane: BTreeMap<String, Vec<String>>,
    open_operations_by_id: BTreeMap<String, LaneRecord>,
    /// lane name -> leaf entry id; `main` always exists.
    lanes: BTreeMap<String, Option<String>>,
    log: Vec<LogItem>,
    stats: SessionStats,
    name: Option<String>,
    labels: BTreeMap<String, String>,
}

impl SessionState {
    pub fn new() -> Self {
        let mut lanes = BTreeMap::new();
        lanes.insert("main".to_owned(), None);
        Self {
            lanes,
            ..Default::default()
        }
    }

    pub fn next_sequence(&self) -> u64 {
        self.sequence + 1
    }

    pub fn get_lanes(&self) -> Vec<LanePointer> {
        self.lanes
            .iter()
            .map(|(lane, leaf_id)| LanePointer {
                lane: lane.clone(),
                leaf_id: leaf_id.clone(),
            })
            .collect()
    }

    pub fn require_lane(&self, lane: &str) -> Result<Option<String>, SessionError> {
        self.lanes.get(lane).cloned().ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::InvalidLane,
                format!("Lane not found: {lane}"),
            )
        })
    }

    pub fn validate_new_lane(&self, lane: &str) -> Result<(), SessionError> {
        if self.lanes.contains_key(lane) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Lane already exists: {lane}"),
            ));
        }
        Ok(())
    }

    pub fn validate_target(&self, target_id: Option<&str>) -> Result<(), SessionError> {
        if let Some(target_id) = target_id {
            if !self.entries_by_id.contains_key(target_id) {
                return Err(SessionError::new(
                    SessionErrorCode::NotFound,
                    format!("Entry not found: {target_id}"),
                ));
            }
        }
        Ok(())
    }

    pub fn validate_unused_id(&self, id: &str) -> Result<(), SessionError> {
        if self.used_ids.contains(id) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session id already exists: {id}"),
            ));
        }
        Ok(())
    }

    /// Apply one mutation. `seq` must equal `next_sequence` (upstream
    /// consecutive-sequence invariant).
    pub fn apply_mutation(&mut self, mutation: SessionMutation) -> Result<(), SessionError> {
        let seq = match &mutation {
            SessionMutation::Entry { entry, .. } => entry.seq,
            SessionMutation::Record { record } => record.seq,
            SessionMutation::Lane { seq, .. }
            | SessionMutation::Name { seq, .. }
            | SessionMutation::Label { seq, .. } => *seq,
        };
        if seq != self.sequence + 1 {
            return Err(invalid(&format!("has non-consecutive seq {seq}")));
        }

        match mutation {
            SessionMutation::Entry { lane, entry } => {
                if self.used_ids.contains(&entry.id) {
                    return Err(invalid(&format!("contains duplicate id {}", entry.id)));
                }
                if let Some(lane) = &lane {
                    let leaf_id = self.lanes.get(lane);
                    let Some(leaf_id) = leaf_id else {
                        return Err(invalid(&format!("references missing lane {lane}")));
                    };
                    if entry.parent_id != *leaf_id {
                        return Err(invalid("does not chain to the lane leaf"));
                    }
                }
                if let Some(parent_id) = &entry.parent_id {
                    if !self.entries_by_id.contains_key(parent_id) {
                        return Err(invalid(&format!("references missing parent {parent_id}")));
                    }
                }
                self.sequence = seq;
                self.used_ids.insert(entry.id.clone());
                if matches!(entry.payload, super::types::EntryPayload::Message { .. }) {
                    self.stats.message_count += 1;
                }
                self.entries_by_id.insert(entry.id.clone(), entry.clone());
                self.entries.push(entry.clone());
                if let Some(lane) = &lane {
                    self.lanes.insert(lane.clone(), Some(entry.id.clone()));
                }
                self.log.push(LogItem::Entry { seq, entry });
            }
            SessionMutation::Record { record } => {
                if !self.lanes.contains_key(&record.lane) {
                    return Err(invalid(&format!("references missing lane {}", record.lane)));
                }
                if self.used_ids.contains(&record.id) {
                    return Err(invalid(&format!("contains duplicate id {}", record.id)));
                }
                self.sequence = seq;
                self.used_ids.insert(record.id.clone());
                match &record.payload {
                    RecordPayload::OperationStarted { .. } => {
                        self.open_operations_by_lane
                            .entry(record.lane.clone())
                            .or_default()
                            .push(record.id.clone());
                        self.open_operations_by_id
                            .insert(record.id.clone(), record.clone());
                    }
                    RecordPayload::OperationFinished { run_id, .. } => {
                        if let Some(open_ids) = self.open_operations_by_lane.get_mut(&record.lane) {
                            open_ids.retain(|id| id != run_id);
                        }
                        self.open_operations_by_id.remove(run_id);
                    }
                    _ => {}
                }
                self.records.push(record.clone());
                if let RecordPayload::UsageRecord { usage, .. } = &record.payload {
                    self.stats.cached_tokens += usage.cache_read as f64;
                    self.stats.uncached_tokens += (usage.input + usage.cache_write) as f64;
                    self.stats.total_tokens += usage.total_tokens as f64;
                    self.stats.cost_total += usage.cost.total;
                }
                self.log.push(LogItem::Record { seq, record });
            }
            SessionMutation::Lane { seq, lane, leaf_id } => {
                if let Some(leaf_id) = &leaf_id {
                    if !self.entries_by_id.contains_key(leaf_id) {
                        return Err(invalid(&format!(
                            "references missing lane target {leaf_id}"
                        )));
                    }
                }
                self.sequence = seq;
                self.lanes.insert(lane.clone(), leaf_id.clone());
                self.log.push(LogItem::Lane { seq, lane, leaf_id });
            }
            SessionMutation::Name { seq, name } => {
                self.sequence = seq;
                self.name = name.clone();
                self.log.push(LogItem::Name { seq, name });
            }
            SessionMutation::Label {
                seq,
                target_id,
                label,
            } => {
                if !self.entries_by_id.contains_key(&target_id) {
                    return Err(invalid(&format!(
                        "references missing label target {target_id}"
                    )));
                }
                self.sequence = seq;
                match &label {
                    Some(label) => {
                        self.labels.insert(target_id.clone(), label.clone());
                    }
                    None => {
                        self.labels.remove(&target_id);
                    }
                }
                self.log.push(LogItem::Label {
                    seq,
                    target_id,
                    label,
                });
            }
        }
        Ok(())
    }

    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        self.entries_by_id.get(id)
    }

    pub fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.after_seq)?;
        let mut results = Vec::new();
        for entry in ordered(&self.entries, query.order) {
            if !self.matches_entry_query(entry, query) {
                continue;
            }
            results.push(entry.clone());
            if results.len() == query.limit.unwrap_or(usize::MAX) {
                break;
            }
        }
        Ok(results)
    }

    pub fn find_entries_on_branch(
        &self,
        start: &str,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.after_seq)?;
        let mut results = Vec::new();
        let path = self.walk_to_root(Some(start), bounds)?;
        if query.order == Some(EntryOrder::OldestFirst) {
            for entry in path.iter().rev() {
                let reached_bound = Some(&entry.id) == bounds.stop_at_id.as_ref()
                    || Some(entry.kind.as_str()) == bounds.stop_at_kind.as_deref();
                if self.matches_entry_query(entry, query) {
                    results.push(entry.clone());
                }
                if reached_bound || results.len() == query.limit.unwrap_or(usize::MAX) {
                    break;
                }
            }
        } else {
            for entry in &path {
                if self.matches_entry_query(entry, query) {
                    results.push(entry.clone());
                }
                if results.len() == query.limit.unwrap_or(usize::MAX) {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.after_seq)?;
        let mut results = Vec::new();
        for record in ordered(&self.records, query.order) {
            if !self.matches_record_query(record, query) {
                continue;
            }
            results.push(record.clone());
            if results.len() == query.limit.unwrap_or(usize::MAX) {
                break;
            }
        }
        Ok(results)
    }

    /// Unfinished operation starts, newest first (upstream
    /// `findOpenOperations`).
    pub fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LaneRecord>, SessionError> {
        assert_valid_limit(limit)?;
        let open_ids = self.open_operations_by_lane.get(lane);
        let Some(open_ids) = open_ids else {
            return Ok(Vec::new());
        };
        let open_operations = open_ids
            .iter()
            .rev()
            .filter_map(|id| self.open_operations_by_id.get(id))
            .cloned()
            .collect::<Vec<_>>();
        Ok(match limit {
            Some(limit) => open_operations.into_iter().take(limit).collect(),
            None => open_operations,
        })
    }

    pub fn get_log(
        &self,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Vec<LogItem>, SessionError> {
        assert_valid_limit(limit)?;
        assert_valid_cursor(after_seq)?;
        let mut results = Vec::new();
        for item in &self.log {
            if let Some(after_seq) = after_seq {
                if item.seq() <= after_seq {
                    continue;
                }
            }
            results.push(item.clone());
            if results.len() == limit.unwrap_or(usize::MAX) {
                break;
            }
        }
        Ok(results)
    }

    pub fn get_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(String::as_str)
    }

    pub fn get_stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Build fork mutations copying this session (upstream
    /// `createForkMutations`). Sequence numbers restart at 1.
    pub fn create_fork_mutations(
        &self,
        options: &ForkOptions,
    ) -> Result<Vec<SessionMutation>, SessionError> {
        let (copied_entries, fork_lanes): (Vec<Entry>, Vec<LanePointer>) = match options {
            ForkOptions::Tree => {
                let query = EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                };
                (self.find_entries(&query)?, self.get_lanes())
            }
            ForkOptions::Branch { entry_id, position } => {
                let selected_entry_id = entry_id.clone().unwrap_or_else(|| {
                    self.require_lane("main").ok().flatten().unwrap_or_default()
                });
                let mut target_id: Option<String> = None;
                if !selected_entry_id.is_empty() {
                    let entry = self.get_entry(&selected_entry_id);
                    let Some(entry) = entry else {
                        return Err(SessionError::new(
                            SessionErrorCode::InvalidForkTarget,
                            format!("Fork target is not a message entry: {selected_entry_id}"),
                        ));
                    };
                    if entry.kind != "message" {
                        return Err(SessionError::new(
                            SessionErrorCode::InvalidForkTarget,
                            format!("Fork target is not a message entry: {selected_entry_id}"),
                        ));
                    }
                    let default_position = if entry_id.is_none() {
                        ForkPosition::At
                    } else {
                        ForkPosition::Before
                    };
                    let position = position.unwrap_or(default_position);
                    target_id = Some(match position {
                        ForkPosition::At => entry.id.clone(),
                        ForkPosition::Before => entry.parent_id.clone().unwrap_or_default(),
                    });
                }
                let target_id = target_id.filter(|id| !id.is_empty());
                let copied = match &target_id {
                    Some(target_id) => {
                        let query = EntryQuery {
                            order: Some(EntryOrder::OldestFirst),
                            ..Default::default()
                        };
                        self.find_entries_on_branch(target_id, &query, &BranchBounds::default())?
                    }
                    None => Vec::new(),
                };
                (
                    copied,
                    vec![LanePointer {
                        lane: "main".to_owned(),
                        leaf_id: target_id,
                    }],
                )
            }
        };

        let mut mutations = Vec::new();
        let mut sequence = 1u64;
        for mut source_entry in copied_entries.clone() {
            source_entry.seq = sequence;
            sequence += 1;
            mutations.push(SessionMutation::Entry {
                lane: None,
                entry: source_entry,
            });
        }
        for pointer in fork_lanes {
            mutations.push(SessionMutation::Lane {
                seq: sequence,
                lane: pointer.lane,
                leaf_id: pointer.leaf_id,
            });
            sequence += 1;
        }
        if let Some(name) = &self.name {
            mutations.push(SessionMutation::Name {
                seq: sequence,
                name: Some(name.clone()),
            });
            sequence += 1;
        }
        for entry in &copied_entries {
            if let Some(label) = self.labels.get(&entry.id) {
                mutations.push(SessionMutation::Label {
                    seq: sequence,
                    target_id: entry.id.clone(),
                    label: Some(label.clone()),
                });
                sequence += 1;
            }
        }
        Ok(mutations)
    }

    /// Walk from `start` toward the root (upstream `walkToRoot`). Returns
    /// entries leaf-first.
    fn walk_to_root(
        &self,
        start: Option<&str>,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        let Some(start) = start else {
            return Ok(Vec::new());
        };
        let mut visited = BTreeSet::new();
        let mut current = self.entries_by_id.get(start).cloned().ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::NotFound,
                format!("Entry not found: {start}"),
            )
        })?;
        let mut path = Vec::new();
        while !current.id.is_empty() {
            if visited.contains(&current.id) {
                return Err(SessionError::new(
                    SessionErrorCode::InvalidEntry,
                    format!("Session branch contains a cycle at {}", current.id),
                ));
            }
            visited.insert(current.id.clone());
            let stop = Some(&current.id) == bounds.stop_at_id.as_ref()
                || Some(current.kind.as_str()) == bounds.stop_at_kind.as_deref()
                || current.parent_id.is_none();
            path.push(current.clone());
            if stop {
                break;
            }
            let parent_id = current.parent_id.clone().expect("non-root has parent");
            current = self.entries_by_id.get(&parent_id).cloned().ok_or_else(|| {
                SessionError::new(
                    SessionErrorCode::InvalidEntry,
                    format!("Entry not found: {parent_id}"),
                )
            })?;
        }
        Ok(path)
    }

    fn matches_entry_query(&self, entry: &Entry, query: &EntryQuery) -> bool {
        if query.kind.as_deref().is_some_and(|kind| entry.kind != kind) {
            return false;
        }
        if query
            .custom_type
            .as_deref()
            .is_some_and(|custom_type| match &entry.payload {
                super::types::EntryPayload::Custom {
                    custom_type: ct, ..
                } => ct != custom_type,
                _ => true,
            })
        {
            return false;
        }
        if let Some(after_seq) = query.after_seq {
            // Upstream cursor semantics: oldestFirst uses seq > afterSeq,
            // newestFirst uses seq < afterSeq.
            if query.order == Some(EntryOrder::OldestFirst) {
                if entry.seq <= after_seq {
                    return false;
                }
            } else if entry.seq >= after_seq {
                return false;
            }
        }
        true
    }

    fn matches_record_query(&self, record: &LaneRecord, query: &RecordQuery) -> bool {
        if query
            .lane
            .as_deref()
            .is_some_and(|lane| record.lane != lane)
        {
            return false;
        }
        if query
            .kind
            .as_deref()
            .is_some_and(|kind| record.kind != kind)
        {
            return false;
        }
        if let Some(run_id) = &query.run_id {
            let record_run_id = match &record.payload {
                RecordPayload::OperationStarted { .. } => Some(&record.id),
                RecordPayload::AbortRequested { run_id }
                | RecordPayload::OperationFinished { run_id, .. }
                | RecordPayload::StepAttempt { run_id, .. }
                | RecordPayload::ToolStarted { run_id, .. }
                | RecordPayload::WriteDeferred { run_id, .. } => Some(run_id),
                RecordPayload::QueueEnqueued { run_id, .. }
                | RecordPayload::QueueCancelled { run_id, .. }
                | RecordPayload::UsageRecord { run_id, .. } => run_id.as_ref(),
            };
            if record_run_id != Some(run_id) {
                return false;
            }
        }
        if let Some(operation_kind) = &query.operation_kind {
            let matches = match &record.payload {
                RecordPayload::OperationStarted { intent, .. } => {
                    intent_kind_str(intent) == operation_kind.as_str()
                }
                _ => false,
            };
            if !matches {
                return false;
            }
        }
        if let Some(after_seq) = query.after_seq {
            if record.seq <= after_seq {
                return false;
            }
        }
        true
    }
}

fn intent_kind_str(intent: &super::types::OperationIntent) -> &'static str {
    match intent {
        super::types::OperationIntent::Run { .. } => "run",
        super::types::OperationIntent::Compaction { .. } => "compaction",
        super::types::OperationIntent::Navigation { .. } => "navigation",
    }
}

// OperationOutcome referenced for the finished-record matching shape.
#[allow(dead_code)]
fn _outcome_witness(outcome: OperationOutcome) -> OperationOutcome {
    outcome
}
