//! The four `rs_*` tools and their handlers (design §4).
//!
//! The handlers are pure over the [`host`](super::host) ports: they plan from
//! saved metadata, start an authorized step through the broker, page a run's
//! output, and normalize diagnostics — but they never spawn Cargo or read a
//! file themselves. `rs_run` re-checks the workspace and configuration at
//! execution time and refuses a plan whose metadata changed (design §4,
//! "変化していれば `stale_plan`"). `rs_diagnostics` never starts a build.
//!
//! The registration is opt-in, exactly like the experimental adapter:
//! [`RustToolkit`] creates the tools and nothing else does.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::contract::{ContractRequest, ContractResponse, ContractService};
use super::diagnostic::{
    BuildStatus, CollectedRun, CollectionState, Diagnostic, DiagnosticCollector, SourceBinding,
    TestStatus,
};
use super::error::RustToolError;
use super::host::{
    CargoJobBroker, OutputStream, OwnerId, RunId, RunRecord, SourceSlice, SourceSnapshotPort,
    StartRequest, WorkspaceCatalogPort,
};
use super::plan::{Coverage, PlanRequest, VerifyPlan, plan};
use super::suggestion::{ApplyRequest, SuggestionNotice, SuggestionReceipt, SuggestionService};
use super::{RustToolLimits, SourcePolicy, command_digest};
use crate::types::{AgentTool, AgentToolResult, ToolExecuteError};
use pillar_ai::types::{Content, Tool};

/// Bounded plan registry. A missing plan means the caller must re-plan, never
/// that the old command is safe (design §4).
#[derive(Debug)]
pub struct PlanRegistry {
    plans: HashMap<String, VerifyPlan>,
    order: VecDeque<String>,
    capacity: usize,
}

impl PlanRegistry {
    pub fn new(capacity: usize) -> Self {
        Self {
            plans: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn insert(&mut self, plan: VerifyPlan) {
        while self.plans.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.plans.remove(&evicted);
            } else {
                break;
            }
        }
        self.order.push_back(plan.plan_id.clone());
        self.plans.insert(plan.plan_id.clone(), plan);
    }

    pub fn get(&self, plan_id: &str) -> Option<&VerifyPlan> {
        self.plans.get(plan_id)
    }
}

/// Bounded diagnostic cache. One normalization per run; eviction reports the
/// run as lost rather than re-fetching different bytes under the same id
/// (design §6.2).
#[derive(Debug)]
pub struct DiagnosticStore {
    runs: HashMap<String, CollectedRun>,
    order: VecDeque<String>,
    capacity: usize,
}

impl DiagnosticStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            runs: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn get(&self, run_id: &str) -> Option<CollectedRun> {
        self.runs.get(run_id).cloned()
    }

    fn insert(&mut self, run_id: String, run: CollectedRun) {
        while self.runs.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.runs.remove(&evicted);
            } else {
                break;
            }
        }
        self.order.push_back(run_id.clone());
        self.runs.insert(run_id, run);
    }
}

/// Per-session state behind the Rust workflow tools.
pub struct RustToolkit {
    catalog: Arc<dyn WorkspaceCatalogPort>,
    broker: Arc<dyn CargoJobBroker>,
    sources: Arc<dyn SourceSnapshotPort>,
    owner: OwnerId,
    limits: RustToolLimits,
    plans: Arc<Mutex<PlanRegistry>>,
    diagnostics: Arc<Mutex<DiagnosticStore>>,
    suggestions: Option<Arc<SuggestionService>>,
    contract: Option<Arc<ContractService>>,
}

impl RustToolkit {
    pub fn new(
        catalog: Arc<dyn WorkspaceCatalogPort>,
        broker: Arc<dyn CargoJobBroker>,
        sources: Arc<dyn SourceSnapshotPort>,
        owner: OwnerId,
        limits: RustToolLimits,
    ) -> Self {
        let capacity = limits.max_live_runs;
        Self {
            catalog,
            broker,
            sources,
            owner,
            limits,
            plans: Arc::new(Mutex::new(PlanRegistry::new(capacity))),
            diagnostics: Arc::new(Mutex::new(DiagnosticStore::new(capacity))),
            suggestions: None,
            contract: None,
        }
    }

    /// Add the optional type/trait contract provider (design §6, stage R2).
    /// Without it, `rs_contract` is not registered.
    pub fn with_contract(mut self, service: Arc<ContractService>) -> Self {
        self.contract = Some(service);
        self
    }

    /// Add suggestion registration/application (design §8, stage R3). Without
    /// a strict conditional store, `rs_diagnostics` returns no suggestion ids
    /// and `rs_apply_suggestion` is not registered.
    pub fn with_suggestions(mut self, service: Arc<SuggestionService>) -> Self {
        self.suggestions = Some(service);
        self
    }

    /// Every Rust workflow tool, in registration order.
    pub fn tools(&self) -> Vec<AgentTool> {
        let mut tools = vec![self.verify_plan_tool(), self.run_tool(), self.job_tool()];
        if self.suggestions.is_some() {
            tools.push(self.apply_suggestion_tool());
        }
        if self.contract.is_some() {
            tools.push(self.contract_tool());
        }
        tools.push(self.diagnostics_tool());
        tools
    }

    fn verify_plan_tool(&self) -> AgentTool {
        let (catalog, plans, limits) = (
            Arc::clone(&self.catalog),
            Arc::clone(&self.plans),
            self.limits.clone(),
        );
        AgentTool {
            tool: Tool {
                name: "rs_verify_plan".to_string(),
                description: rs_verify_plan_description(),
                parameters: rs_verify_plan_parameters_json(),
                constrained_sampling: None,
            },
            label: "rs_verify_plan".to_string(),
            prepare_arguments: None,
            execute: Arc::new(move |_id, args, signal, _on_update| {
                let (catalog, plans, limits) =
                    (Arc::clone(&catalog), Arc::clone(&plans), limits.clone());
                Box::pin(async move {
                    abort_guard(signal.as_ref())?;
                    let request: PlanRequest = serde_json::from_value(args).map_err(|error| {
                        ToolExecuteError(format!("rs_verify_plan input: {error}"))
                    })?;
                    let plan = verify_plan_impl(catalog, plans, limits, request)
                        .await
                        .map_err(tool_error)?;
                    Ok(plan_result(&plan))
                })
            }),
            execution_mode: None,
        }
    }

    fn run_tool(&self) -> AgentTool {
        let (catalog, plans, broker, owner) = (
            Arc::clone(&self.catalog),
            Arc::clone(&self.plans),
            Arc::clone(&self.broker),
            self.owner.clone(),
        );
        AgentTool {
            tool: Tool {
                name: "rs_run".to_string(),
                description: rs_run_description(),
                parameters: rs_run_parameters_json(),
                constrained_sampling: None,
            },
            label: "rs_run".to_string(),
            prepare_arguments: None,
            execute: Arc::new(move |_id, args, signal, _on_update| {
                let (catalog, plans, broker, owner) = (
                    Arc::clone(&catalog),
                    Arc::clone(&plans),
                    Arc::clone(&broker),
                    owner.clone(),
                );
                Box::pin(async move {
                    abort_guard(signal.as_ref())?;
                    let request: RunRequest = serde_json::from_value(args)
                        .map_err(|error| ToolExecuteError(format!("rs_run input: {error}")))?;
                    let record = run_step_impl(catalog, plans, broker, owner, request)
                        .await
                        .map_err(tool_error)?;
                    Ok(run_result(&record))
                })
            }),
            execution_mode: None,
        }
    }

    fn job_tool(&self) -> AgentTool {
        let (broker, owner, limits) = (
            Arc::clone(&self.broker),
            self.owner.clone(),
            self.limits.clone(),
        );
        AgentTool {
            tool: Tool {
                name: "rs_job".to_string(),
                description: rs_job_description(),
                parameters: rs_job_parameters_json(),
                constrained_sampling: None,
            },
            label: "rs_job".to_string(),
            prepare_arguments: None,
            execute: Arc::new(move |_id, args, signal, _on_update| {
                let (broker, owner, limits) = (Arc::clone(&broker), owner.clone(), limits.clone());
                Box::pin(async move {
                    abort_guard(signal.as_ref())?;
                    let request: JobRequest = serde_json::from_value(args)
                        .map_err(|error| ToolExecuteError(format!("rs_job input: {error}")))?;
                    let outcome = job_impl(broker, owner, limits, request)
                        .await
                        .map_err(tool_error)?;
                    Ok(job_result(&outcome))
                })
            }),
            execution_mode: None,
        }
    }

    fn apply_suggestion_tool(&self) -> AgentTool {
        let suggestions = self
            .suggestions
            .clone()
            .expect("only registered when a suggestion service exists");
        let owner = self.owner.clone();
        AgentTool {
            tool: Tool {
                name: "rs_apply_suggestion".to_string(),
                description: rs_apply_suggestion_description(),
                parameters: rs_apply_suggestion_parameters_json(),
                constrained_sampling: None,
            },
            label: "rs_apply_suggestion".to_string(),
            prepare_arguments: None,
            execute: Arc::new(move |_id, args, signal, _on_update| {
                let suggestions = Arc::clone(&suggestions);
                let owner = owner.clone();
                Box::pin(async move {
                    abort_guard(signal.as_ref())?;
                    let request: ApplyRequest = serde_json::from_value(args).map_err(|error| {
                        ToolExecuteError(format!("rs_apply_suggestion input: {error}"))
                    })?;
                    let receipt = suggestions
                        .apply(&owner, &request)
                        .await
                        .map_err(tool_error)?;
                    Ok(apply_result(&receipt))
                })
            }),
            execution_mode: None,
        }
    }

    fn contract_tool(&self) -> AgentTool {
        let contract = self
            .contract
            .clone()
            .expect("only registered when a contract service exists");
        AgentTool {
            tool: Tool {
                name: "rs_contract".to_string(),
                description: rs_contract_description(),
                parameters: rs_contract_parameters_json(),
                constrained_sampling: None,
            },
            label: "rs_contract".to_string(),
            prepare_arguments: None,
            execute: Arc::new(move |_id, args, signal, _on_update| {
                let contract = Arc::clone(&contract);
                Box::pin(async move {
                    abort_guard(signal.as_ref())?;
                    let request: ContractRequest = serde_json::from_value(args)
                        .map_err(|error| ToolExecuteError(format!("rs_contract input: {error}")))?;
                    let response = contract.contract(&request).await.map_err(tool_error)?;
                    Ok(contract_result(&response))
                })
            }),
            execution_mode: None,
        }
    }

    fn diagnostics_tool(&self) -> AgentTool {
        let (catalog, broker, sources, diagnostics, suggestions, owner, limits) = (
            Arc::clone(&self.catalog),
            Arc::clone(&self.broker),
            Arc::clone(&self.sources),
            Arc::clone(&self.diagnostics),
            self.suggestions.clone(),
            self.owner.clone(),
            self.limits.clone(),
        );
        AgentTool {
            tool: Tool {
                name: "rs_diagnostics".to_string(),
                description: rs_diagnostics_description(),
                parameters: rs_diagnostics_parameters_json(),
                constrained_sampling: None,
            },
            label: "rs_diagnostics".to_string(),
            prepare_arguments: None,
            execute: Arc::new(move |_id, args, signal, _on_update| {
                let (catalog, broker, sources, diagnostics, suggestions, owner, limits) = (
                    Arc::clone(&catalog),
                    Arc::clone(&broker),
                    Arc::clone(&sources),
                    Arc::clone(&diagnostics),
                    suggestions.clone(),
                    owner.clone(),
                    limits.clone(),
                );
                Box::pin(async move {
                    abort_guard(signal.as_ref())?;
                    let request: DiagnosticsRequest =
                        serde_json::from_value(args).map_err(|error| {
                            ToolExecuteError(format!("rs_diagnostics input: {error}"))
                        })?;
                    let response = diagnostics_impl(
                        catalog,
                        broker,
                        sources,
                        diagnostics,
                        suggestions,
                        owner,
                        limits,
                        request,
                    )
                    .await
                    .map_err(tool_error)?;
                    Ok(diagnostics_result(&response))
                })
            }),
            execution_mode: None,
        }
    }
}

fn abort_guard(signal: Option<&crate::AbortSignal>) -> Result<(), ToolExecuteError> {
    if signal.is_some_and(|signal| signal.is_aborted()) {
        return Err(ToolExecuteError("Operation aborted".to_string()));
    }
    Ok(())
}

// --- handlers --------------------------------------------------------------

async fn verify_plan_impl(
    catalog: Arc<dyn WorkspaceCatalogPort>,
    plans: Arc<Mutex<PlanRegistry>>,
    _limits: RustToolLimits,
    request: PlanRequest,
) -> Result<VerifyPlan, RustToolError> {
    let metadata = catalog.catalog()?;
    if metadata.workspace_root().is_none() {
        return Err(RustToolError::metadata_unavailable(
            "the saved metadata has no workspace root",
        ));
    }
    let configurations = catalog.configurations()?;
    let plan = plan(&metadata, &configurations, &request)?;
    plans.lock().expect("plans lock").insert(plan.clone());
    Ok(plan)
}

async fn run_step_impl(
    catalog: Arc<dyn WorkspaceCatalogPort>,
    plans: Arc<Mutex<PlanRegistry>>,
    broker: Arc<dyn CargoJobBroker>,
    owner: OwnerId,
    request: RunRequest,
) -> Result<RunRecord, RustToolError> {
    let plan = {
        let plans = plans.lock().expect("plans lock");
        plans.get(&request.plan_id).cloned().ok_or_else(|| {
            RustToolError::invalid_request("unknown plan_id; make a new plan with rs_verify_plan")
        })?
    };
    let step = plan
        .steps
        .iter()
        .find(|step| step.step_id == request.step_id)
        .cloned()
        .ok_or_else(|| RustToolError::invalid_request("unknown step_id in this plan"))?;

    // Re-check the workspace and configuration at execution time (design §4):
    // the planning-time digest is not trusted on its own.
    let metadata = catalog.catalog()?;
    if metadata.digest_hex() != plan.metadata_digest {
        return Err(RustToolError::stale_plan(
            "the workspace metadata changed since this plan was made",
        ));
    }
    let expected_fingerprint = plan
        .configuration_fingerprints
        .get(&step.configuration_id)
        .ok_or_else(|| {
            RustToolError::configuration_mismatch("the plan does not name this configuration")
        })?;
    let configurations = catalog.configurations()?;
    let configuration = configurations
        .iter()
        .find(|configuration| configuration.id == step.configuration_id)
        .ok_or_else(|| {
            RustToolError::configuration_mismatch("the configuration is no longer approved")
        })?;
    let observed = format!("{:016x}", configuration.fingerprint());
    if *expected_fingerprint != observed {
        return Err(RustToolError::stale_plan(
            "the configuration changed since this plan was made",
        ));
    }

    let start = StartRequest {
        owner,
        plan_id: plan.plan_id.clone(),
        step_id: step.step_id.clone(),
        request_id: crate::rust_tools::host::RequestId::new(request.request_id),
        configuration_id: step.configuration_id.clone(),
        metadata_digest: plan.metadata_digest.clone(),
        configuration_fingerprint: observed,
        command_digest: command_digest(&step.argv),
        argv: step.argv,
    };
    broker.start(start).await
}

async fn job_impl(
    broker: Arc<dyn CargoJobBroker>,
    owner: OwnerId,
    limits: RustToolLimits,
    request: JobRequest,
) -> Result<JobOutcome, RustToolError> {
    let run_id = RunId::new(request.run_id);
    match request.action {
        JobAction::Status => {
            let record = broker.status(&owner, &run_id).await?;
            Ok(JobOutcome {
                record,
                output: None,
            })
        }
        JobAction::Cancel => {
            let record = broker.cancel(&owner, &run_id).await?;
            Ok(JobOutcome {
                record,
                output: None,
            })
        }
        JobAction::Output => {
            let stream = request.stream.unwrap_or(OutputStream::Stdout);
            let limit = request.limit_bytes.unwrap_or(limits.max_source_bytes);
            let page = broker
                .output(&owner, &run_id, stream, request.offset, limit)
                .await?;
            let record = broker.status(&owner, &run_id).await?;
            Ok(JobOutcome {
                record,
                output: Some(page),
            })
        }
    }
}

// The ports are distinct host capabilities; bundling them into a struct would
// only move the list. The handler is internal and called from one place.
#[allow(clippy::too_many_arguments)]
async fn diagnostics_impl(
    catalog: Arc<dyn WorkspaceCatalogPort>,
    broker: Arc<dyn CargoJobBroker>,
    sources: Arc<dyn SourceSnapshotPort>,
    store: Arc<Mutex<DiagnosticStore>>,
    suggestions: Option<Arc<SuggestionService>>,
    owner: OwnerId,
    limits: RustToolLimits,
    request: DiagnosticsRequest,
) -> Result<DiagnosticsResponse, RustToolError> {
    let run_id = request.run_id.clone();
    let cached = store.lock().expect("diagnostics lock").get(&run_id);
    let run = match cached {
        Some(run) => run,
        None => {
            let metadata = catalog.catalog()?;
            let policy = match metadata.workspace_root() {
                Some(root) => SourcePolicy::new(root),
                None => SourcePolicy::default(),
            };
            let raw = broker.raw(&owner, &RunId::new(run_id.clone())).await?;
            let mut collector = DiagnosticCollector::new(run_id.clone(), limits.collection.clone());
            for line in String::from_utf8_lossy(&raw.stdout).lines() {
                collector.push_line(line);
            }
            for line in String::from_utf8_lossy(&raw.stderr).lines() {
                collector.push_line(line);
            }
            let mut run = collector.finish(&policy);
            if raw.truncated && run.collection.is_complete() {
                run.collection = CollectionState::Partial {
                    reason: "the broker did not retain the whole run".to_string(),
                };
            }
            store
                .lock()
                .expect("diagnostics lock")
                .insert(run_id.clone(), run.clone());
            run
        }
    };

    // Register each proposal group once (design §8.1). Registration is
    // idempotent by run id, so a repeated rs_diagnostics call keeps the same
    // suggestion ids.
    let suggestion_notices = match &suggestions {
        Some(service) => service.register_run(&owner, &run).await?,
        None => Vec::new(),
    };

    let selected = select_diagnostics(&run, &request.diagnostic_ids);
    let max_diagnostics = request
        .budget
        .max_diagnostics
        .unwrap_or(limits.max_diagnostics_per_response);
    let diagnostics_omitted = selected.len().saturating_sub(max_diagnostics);
    let mut source_budget = request
        .budget
        .source_bytes
        .unwrap_or(limits.max_source_bytes);
    let mut source_bytes_used = 0usize;
    let mut truncated_sources = false;
    let want_source = request.include.is_empty()
        || request
            .include
            .iter()
            .any(|item| item == "primary_source" || item == "constraint_sources");

    let mut entries: Vec<DiagnosticEntry> = Vec::new();
    for diagnostic in selected.into_iter().take(max_diagnostics) {
        let mut source = None;
        if want_source
            && let Some(span) = primary_workspace_span(diagnostic)
            && span.line_start > 0
        {
            let slice = sources.read_range(&span.file_name, span.line_start, 1)?;
            if slice.text.len() <= source_budget {
                source_budget -= slice.text.len();
                source_bytes_used += slice.text.len();
                source = Some(slice);
            } else {
                truncated_sources = true;
            }
        }
        entries.push(DiagnosticEntry {
            diagnostic: diagnostic.clone(),
            source,
            suggestions: suggestion_notices
                .iter()
                .filter(|notice| notice.diagnostic_id == diagnostic.id)
                .cloned()
                .collect(),
        });
    }

    Ok(DiagnosticsResponse {
        run_id,
        build_status: run.build_status,
        test_status: run.test_status,
        collection: run.collection.clone(),
        diagnostics: entries,
        diagnostics_omitted,
        source_bytes_used,
        truncated_sources,
    })
}

fn select_diagnostics<'a>(run: &'a CollectedRun, ids: &[String]) -> Vec<&'a Diagnostic> {
    if ids.is_empty() {
        return run.diagnostics.iter().collect();
    }
    let mut selected = Vec::new();
    for id in ids {
        if let Some(diagnostic) = find_diagnostic(&run.diagnostics, id) {
            selected.push(diagnostic);
        }
    }
    selected
}

fn find_diagnostic<'a>(diagnostics: &'a [Diagnostic], id: &str) -> Option<&'a Diagnostic> {
    for diagnostic in diagnostics {
        if diagnostic.id == id {
            return Some(diagnostic);
        }
        if let Some(found) = find_diagnostic(&diagnostic.children, id) {
            return Some(found);
        }
    }
    None
}

fn primary_workspace_span(diagnostic: &Diagnostic) -> Option<&super::diagnostic::DiagnosticSpan> {
    diagnostic
        .spans
        .iter()
        .find(|span| span.is_primary && matches!(span.binding, SourceBinding::Workspace { .. }))
        .or_else(|| {
            diagnostic
                .spans
                .iter()
                .find(|span| matches!(span.binding, SourceBinding::Workspace { .. }))
        })
}

// --- request / response shapes ---------------------------------------------

/// `rs_run` arguments (design §4, example).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    pub plan_id: String,
    pub step_id: String,
    pub request_id: String,
}

/// `rs_job` actions (design §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobAction {
    Status,
    Output,
    Cancel,
}

/// `rs_job` arguments (design §4, example).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRequest {
    pub run_id: String,
    pub action: JobAction,
    #[serde(default)]
    pub stream: Option<OutputStream>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit_bytes: Option<usize>,
}

/// `rs_diagnostics` arguments (design §5.2, example).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticsRequest {
    pub run_id: String,
    #[serde(default)]
    pub diagnostic_ids: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub budget: DiagnosticBudget,
}

/// Per-call budgets. `output_tokens` is accepted but advisory: no tokenizer is
/// assumed, so a byte cap is the hard one (design §8.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticBudget {
    #[serde(default)]
    pub output_tokens: Option<usize>,
    #[serde(default)]
    pub source_bytes: Option<usize>,
    #[serde(default)]
    pub max_diagnostics: Option<usize>,
}

/// The `rs_job` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobOutcome {
    pub record: RunRecord,
    pub output: Option<super::host::OutputPage>,
}

/// A diagnostic plus the source text the caller is allowed to see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticEntry {
    pub diagnostic: Diagnostic,
    pub source: Option<SourceSlice>,
    /// Proposal groups registered for this diagnostic, when a suggestion
    /// service is configured (design §8.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<SuggestionNotice>,
}

/// The `rs_diagnostics` result. Completely distinct from a new build: the run
/// was already produced (design §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticsResponse {
    pub run_id: String,
    pub build_status: BuildStatus,
    pub test_status: TestStatus,
    pub collection: CollectionState,
    pub diagnostics: Vec<DiagnosticEntry>,
    pub diagnostics_omitted: usize,
    pub source_bytes_used: usize,
    pub truncated_sources: bool,
}

// --- rendering -------------------------------------------------------------

fn plan_result(plan: &VerifyPlan) -> AgentToolResult {
    let mut content = format!(
        "[rs_verify_plan {} steps={} metadata={}]\n",
        plan.plan_id,
        plan.steps.len(),
        plan.metadata_digest
    );
    for step in &plan.steps {
        content.push_str(&format!(
            "{} [{}] {}\n",
            step.step_id,
            step.configuration_id,
            step.argv.join(" ")
        ));
        for coverage in &step.covers {
            content.push_str(&format!("  covers {}\n", coverage_label(coverage)));
        }
    }
    for item in &plan.unverified {
        content.push_str(&format!(
            "unverified {}: {}\n",
            item.kind.as_str(),
            item.detail
        ));
    }
    for note in &plan.notes {
        content.push_str(&format!("note: {note}\n"));
    }
    AgentToolResult {
        content: vec![Content::text(content.trim_end().to_string())],
        details: serde_json::to_value(plan).unwrap_or(Value::Null),
        ..Default::default()
    }
}

fn run_result(record: &RunRecord) -> AgentToolResult {
    let content = format!(
        "[rs_run {} {} build={} test={}]",
        record.run_id.as_str(),
        record.state.as_str(),
        record.build_status.as_str(),
        record.test_status.as_str()
    );
    AgentToolResult {
        content: vec![Content::text(content)],
        details: serde_json::to_value(record).unwrap_or(Value::Null),
        ..Default::default()
    }
}

fn job_result(outcome: &JobOutcome) -> AgentToolResult {
    let mut content = format!(
        "[rs_job {} {} build={} test={}",
        outcome.record.run_id.as_str(),
        outcome.record.state.as_str(),
        outcome.record.build_status.as_str(),
        outcome.record.test_status.as_str()
    );
    if let Some(output) = &outcome.output {
        if output.expired {
            content.push_str(" expired=true");
        }
        if output.truncated {
            content.push_str(" truncated=true");
        }
        content.push(']');
        if !output.bytes.is_empty() {
            content.push('\n');
            content.push_str(&String::from_utf8_lossy(&output.bytes));
        }
    } else {
        content.push(']');
    }
    AgentToolResult {
        content: vec![Content::text(content)],
        details: serde_json::to_value(outcome).unwrap_or(Value::Null),
        ..Default::default()
    }
}

fn diagnostics_result(response: &DiagnosticsResponse) -> AgentToolResult {
    let mut content = format!(
        "[rs_diagnostics {} build={} test={}",
        response.run_id,
        response.build_status.as_str(),
        response.test_status.as_str()
    );
    if !response.collection.is_complete() {
        content.push_str(" partial=true");
    }
    if response.diagnostics_omitted > 0 {
        content.push_str(&format!(
            " diagnostics_omitted={}",
            response.diagnostics_omitted
        ));
    }
    if response.truncated_sources {
        content.push_str(" sources_truncated=true");
    }
    content.push_str("]\n");
    for entry in &response.diagnostics {
        let diagnostic = &entry.diagnostic;
        content.push_str(&format!(
            "{} {} {}",
            diagnostic.id,
            diagnostic.level.as_str(),
            diagnostic.message
        ));
        if let Some(code) = &diagnostic.code {
            content.push_str(&format!(" [{code}]"));
        }
        if let Some(span) = primary_workspace_span(diagnostic) {
            content.push_str(&format!(
                " at {}:{}:{}",
                span.file_name, span.line_start, span.column_start
            ));
        }
        for notice in &entry.suggestions {
            content.push_str(&format!(
                " suggestion {} applicable={}",
                notice.suggestion_id, notice.applicable
            ));
            if let Some(reason) = &notice.reason {
                content.push_str(&format!(" ({reason})"));
            }
        }
        content.push('\n');
        if let Some(source) = &entry.source {
            for line in source.text.lines() {
                content.push_str(&format!("    {line}\n"));
            }
        }
    }
    AgentToolResult {
        content: vec![Content::text(content.trim_end().to_string())],
        details: serde_json::to_value(response).unwrap_or(Value::Null),
        ..Default::default()
    }
}

fn coverage_label(coverage: &Coverage) -> String {
    match coverage {
        Coverage::UnitTests { package } => format!("unit_tests:{package}"),
        Coverage::BinaryUnitTests { package } => format!("binary_unit_tests:{package}"),
        Coverage::IntegrationTarget { package, target } => {
            format!("integration_target:{package}:{target}")
        }
        Coverage::AllTargets { package } => format!("all_targets:{package}"),
    }
}

fn apply_result(receipt: &SuggestionReceipt) -> AgentToolResult {
    let content = format!(
        "[rs_apply_suggestion {} applied path={} revision={} edits={} bytes={} validated=false]",
        receipt.suggestion_id,
        receipt.path,
        receipt.revision.generation,
        receipt.edits_applied,
        receipt.bytes_written
    );
    AgentToolResult {
        content: vec![Content::text(content)],
        details: serde_json::to_value(receipt).unwrap_or(Value::Null),
        ..Default::default()
    }
}

fn contract_result(response: &ContractResponse) -> AgentToolResult {
    let mut content = format!(
        "[rs_contract {} {}",
        response.position_ref,
        if response.availability.is_available() {
            "available"
        } else {
            "unavailable"
        }
    );
    if response.fallback_to_source {
        content.push_str(" fallback=source");
    }
    if response.slice.truncated {
        content.push_str(" truncated=true");
    }
    content.push_str("]\n");
    if let Some(declaration) = &response.slice.declaration {
        content.push_str(&format!("{} {}", declaration.kind, declaration.name));
        if let Some(signature) = &declaration.signature {
            content.push_str(&format!(" {signature}"));
        }
        content.push_str(&format!(
            " ({})\n",
            provenance_label(declaration.provenance)
        ));
        for bound in declaration
            .where_clauses
            .iter()
            .chain(declaration.generics.iter())
        {
            content.push_str(&format!("  {bound}\n"));
        }
    }
    for item in &response.slice.types {
        content.push_str(&format!("type {}", item.name));
        if let Some(definition) = &item.definition {
            content.push_str(&format!(" = {definition}"));
        }
        content.push('\n');
    }
    for candidate in &response.slice.impls {
        content.push_str(&format!(
            "impl {}{}\n",
            candidate.text,
            if candidate.selected {
                " (selected)"
            } else {
                ""
            }
        ));
    }
    for unresolved in &response.slice.unresolved {
        content.push_str(&format!(
            "unresolved {}: {}\n",
            unresolved.what, unresolved.reason
        ));
    }
    if let Some(excerpt) = &response.slice.source_excerpt {
        content.push_str("source:\n");
        content.push_str(excerpt);
        content.push('\n');
    }
    AgentToolResult {
        content: vec![Content::text(content.trim_end().to_string())],
        details: serde_json::to_value(response).unwrap_or(Value::Null),
        ..Default::default()
    }
}

fn provenance_label(provenance: super::contract::Provenance) -> &'static str {
    match provenance {
        super::contract::Provenance::Declared => "declared",
        super::contract::Provenance::Inferred => "inferred",
    }
}

fn tool_error(error: RustToolError) -> ToolExecuteError {
    let mut message = format!("rs tool failed ({}) : {}", error.code, error.message);
    if let Some(repair) = error.repair {
        message.push_str(&format!(" — {repair}"));
    }
    ToolExecuteError(message)
}

// --- schemas ---------------------------------------------------------------

pub fn rs_verify_plan_description() -> String {
    "Plan which Cargo tests to run for a change, from saved workspace metadata, without running \
     Cargo. Returns step ids and the exact cargo argv the host will authorize, plus what the plan \
     does not cover. Pass the changed paths and the configuration ids the host approved."
        .to_string()
}

pub fn rs_run_description() -> String {
    "Start one step from a saved rs_verify_plan. The host re-checks the workspace and the \
     configuration; a plan made against different metadata is refused as stale. Reuse the same \
     requestId only to retry this exact call."
        .to_string()
}

pub fn rs_job_description() -> String {
    "Inspect a run started by rs_run: its state and phase (`status`), a page of retained stdout or \
     stderr (`output`), or cancellation (`cancel`). Cancellation acceptance is not proof the \
     process stopped; read the returned state."
        .to_string()
}

pub fn rs_diagnostics_description() -> String {
    "Read the normalized diagnostics of a run that already happened, with source locations and the \
     relevant source lines. Never starts a new build. An empty diagnosticIds selects every \
     diagnostic; `collection` says whether the whole run was observed."
        .to_string()
}

pub fn rs_verify_plan_parameters_json() -> Value {
    json!({
        "type": "object",
        "properties": {
            "changed_paths": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Paths changed, relative to the workspace root or absolute"
            },
            "configuration_ids": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Host-approved configuration ids to plan for"
            },
            "goal": {
                "type": "string",
                "enum": ["validate_change", "checkpoint", "investigate"],
                "description": "Why the verification is being planned"
            },
            "scope": {
                "type": "string",
                "enum": ["focused", "package", "workspace"],
                "description": "How wide to plan"
            },
            "requested_targets": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Integration test target names to include"
            },
            "metadata_digest": {
                "type": "string",
                "description": "The metadata digest the caller planned against; a mismatch is refused"
            }
        },
        "required": ["changed_paths", "configuration_ids", "goal", "scope"]
    })
}

pub fn rs_run_parameters_json() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plan_id": {"type": "string"},
            "step_id": {"type": "string"},
            "request_id": {
                "type": "string",
                "description": "Your id for this start; repeat it only to retry this exact call"
            }
        },
        "required": ["plan_id", "step_id", "request_id"]
    })
}

pub fn rs_job_parameters_json() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string"},
            "action": {"type": "string", "enum": ["status", "output", "cancel"]},
            "stream": {"type": "string", "enum": ["stdout", "stderr"]},
            "offset": {"type": "integer", "description": "Byte offset for action=output"},
            "limit_bytes": {"type": "integer", "description": "Maximum bytes for action=output"}
        },
        "required": ["run_id", "action"]
    })
}

pub fn rs_diagnostics_parameters_json() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string"},
            "diagnostic_ids": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Diagnostic ids to fetch; empty means all"
            },
            "include": {
                "type": "array",
                "items": {"type": "string"},
                "description": "What to include, e.g. primary_source, constraint_sources, suggestions"
            },
            "budget": {
                "type": "object",
                "properties": {
                    "output_tokens": {"type": "integer"},
                    "source_bytes": {"type": "integer"},
                    "max_diagnostics": {"type": "integer"}
                }
            }
        },
        "required": ["run_id"]
    })
}

pub fn rs_apply_suggestion_description() -> String {
    "Apply one registered compiler suggestion exactly once. Only a proposal that is entirely \
     MachineApplicable on a single workspace file with a strict host can be applied; anything else \
     is preview-only and is refused. Publication is not validation: run rs_verify_plan/rs_run \
     afterwards. Repeat the same operationId only to retry this exact call."
        .to_string()
}

pub fn rs_apply_suggestion_parameters_json() -> Value {
    json!({
        "type": "object",
        "properties": {
            "suggestion_id": {"type": "string"},
            "operation_id": {
                "type": "string",
                "description": "Your id for this application; repeat it only to retry this exact call"
            },
            "expected_preview_digest": {
                "type": "string",
                "description": "The previewDigest you saw; a mismatch is refused"
            }
        },
        "required": ["suggestion_id", "operation_id"]
    })
}

pub fn rs_contract_description() -> String {
    "Read the type/trait contract at a versioned position reference: the declaration signature, \
     generics and where-clauses, the referenced type definitions and related impl candidates. \
     Selected and candidate impls are distinguished, declared and inferred types are distinguished, \
     and anything unresolved or cut by the budget is reported. When no semantic provider is \
     available this falls back to the raw source line and says so."
        .to_string()
}

pub fn rs_contract_parameters_json() -> Value {
    json!({
        "type": "object",
        "properties": {
            "position_ref": {
                "type": "string",
                "description": "A versioned source position reference from a previous tool result"
            },
            "include": {
                "type": "array",
                "items": {"type": "string"},
                "description": "signature, where_clauses, type_definitions, related_impls, cfg"
            },
            "budget": {
                "type": "object",
                "properties": {
                    "max_nodes": {"type": "integer"},
                    "max_depth": {"type": "integer"},
                    "output_tokens": {"type": "integer"}
                }
            }
        },
        "required": ["position_ref"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_registry_evicts_the_oldest_plan() {
        let mut registry = PlanRegistry::new(1);
        let plan_a = VerifyPlan {
            plan_id: "a".to_string(),
            metadata_digest: "m".to_string(),
            configuration_fingerprints: Default::default(),
            steps: Vec::new(),
            unverified: Vec::new(),
            notes: Vec::new(),
            execution_started: false,
        };
        let mut plan_b = plan_a.clone();
        plan_b.plan_id = "b".to_string();
        registry.insert(plan_a);
        registry.insert(plan_b);
        assert!(registry.get("a").is_none());
        assert!(registry.get("b").is_some());
    }

    #[test]
    fn the_quoted_schemas_require_what_the_handlers_need() {
        let plan = rs_verify_plan_parameters_json();
        assert_eq!(
            plan["required"],
            json!(["changed_paths", "configuration_ids", "goal", "scope"])
        );
        let run = rs_run_parameters_json();
        assert_eq!(run["required"], json!(["plan_id", "step_id", "request_id"]));
        let job = rs_job_parameters_json();
        assert_eq!(job["required"], json!(["run_id", "action"]));
        let diagnostics = rs_diagnostics_parameters_json();
        assert_eq!(diagnostics["required"], json!(["run_id"]));
        let apply = rs_apply_suggestion_parameters_json();
        assert_eq!(apply["required"], json!(["suggestion_id", "operation_id"]));
        let contract = rs_contract_parameters_json();
        assert_eq!(contract["required"], json!(["position_ref"]));
    }
}
