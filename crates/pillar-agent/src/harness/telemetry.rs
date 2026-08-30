//! Port of packages/agent/src/harness/telemetry.ts (pi v0.84.3) — the
//! AI-request and harness telemetry schemas plus the typed span starters.
//!
//! divergence: upstream's schema layer is TypeScript `as const satisfies`
//! objects whose type-level machinery (`TelemetrySchemaSpanName`,
//! `ExactTelemetryAttributes`, …) enforces attribute sets at compile time.
//! The port keeps the schema **data** (serde-serializable, same key order
//! as upstream via struct field order) and enforces closed sets with
//! enums; `start_ai_span`/`start_harness_span` take enums and build the
//! attribute maps, so unknown/missing attributes are type errors here too.
//! The markdown documentation renderer (upstream
//! `scripts/generate-telemetry-docs.ts`) is ported as
//! [`render_agent_telemetry_schema_markdown`].

use std::collections::BTreeMap;

use pillar_telemetry::{AttributeValue, SpanHandle, SpanOptions, SpanStatus, TelemetryContext};
use serde::Serialize;

/// Upstream `TelemetryAttributeDefinition` (the subset the schemas use).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryAttributeDefinition {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<&'static [&'static str]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cardinality: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
    pub description: &'static str,
}

impl TelemetryAttributeDefinition {
    /// Upstream `required: true` string attribute without a values list.
    pub const fn required_string(description: &'static str) -> Self {
        Self {
            kind: "string",
            required: true,
            values: None,
            cardinality: None,
            sensitive: false,
            description,
        }
    }

    /// Upstream `required: false` string attribute.
    pub const fn optional_string(description: &'static str) -> Self {
        Self {
            kind: "string",
            required: false,
            values: None,
            cardinality: None,
            sensitive: false,
            description,
        }
    }

    /// Upstream `required: true` boolean attribute.
    pub const fn required_bool(description: &'static str) -> Self {
        Self {
            kind: "boolean",
            required: true,
            values: None,
            cardinality: None,
            sensitive: false,
            description,
        }
    }

    /// Upstream optional boolean attribute.
    pub const fn optional_bool(description: &'static str) -> Self {
        Self {
            kind: "boolean",
            required: false,
            values: None,
            cardinality: None,
            sensitive: false,
            description,
        }
    }

    /// Upstream number attribute.
    pub const fn number(required: bool, description: &'static str) -> Self {
        Self {
            kind: "number",
            required,
            values: None,
            cardinality: None,
            sensitive: false,
            description,
        }
    }
}

/// Upstream `TelemetrySchemaDefinition["spans"][name].parents`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TelemetryParentDefinition {
    /// `{ kind: "any" }`.
    Any,
    /// `{ kind: "root_or_external" }`.
    RootOrExternal,
    /// `{ kind: "spans", spans: [...] }`.
    Spans { spans: &'static [&'static str] },
}

/// Upstream `span.status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySpanStatus {
    pub default: &'static str,
    pub error_when: &'static str,
}

/// One span in a schema (upstream span definition shape).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySpanDefinition {
    pub description: &'static str,
    pub parents: TelemetryParentDefinition,
    pub start_attributes: BTreeMap<&'static str, TelemetryAttributeDefinition>,
    pub end_attributes: BTreeMap<&'static str, TelemetryAttributeDefinition>,
    pub status: TelemetrySpanStatus,
}

/// Upstream `TelemetrySchemaDefinition`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TelemetrySchemaDefinition {
    pub version: u32,
    pub spans: BTreeMap<&'static str, TelemetrySpanDefinition>,
}

/// Upstream `AI_TELEMETRY_SCHEMA`.
pub fn ai_telemetry_schema() -> TelemetrySchemaDefinition {
    let mut spans = BTreeMap::new();
    let mut start = BTreeMap::new();
    start.insert(
        "pi.ai.operation",
        TelemetryAttributeDefinition {
            kind: "string",
            required: true,
            values: Some(&["stream", "fetch_deferred", "cancel_deferred", "generate_images"]),
            cardinality: None,
            sensitive: false,
            description: "Logical provider operation",
        },
    );
    start.insert(
        "pi.ai.provider",
        TelemetryAttributeDefinition::required_string("Selected provider id"),
    );
    start.insert(
        "pi.ai.model",
        TelemetryAttributeDefinition::required_string("Requested model id"),
    );
    start.insert(
        "pi.ai.api",
        TelemetryAttributeDefinition::required_string("Provider API id"),
    );
    start.insert(
        "pi.ai.streaming",
        TelemetryAttributeDefinition::required_bool("Whether this operation returns a stream"),
    );
    start.insert(
        "pi.ai.deferred",
        TelemetryAttributeDefinition::optional_bool(
            "Whether the operation requests or participates in deferred execution",
        ),
    );
    let mut end = BTreeMap::new();
    end.insert(
        "pi.ai.response.model",
        TelemetryAttributeDefinition::optional_string("Concrete response model"),
    );
    end.insert(
        "pi.ai.response.id",
        TelemetryAttributeDefinition {
            kind: "string",
            required: false,
            values: None,
            cardinality: Some("high"),
            sensitive: false,
            description: "Provider response id",
        },
    );
    end.insert(
        "pi.ai.response.stop_reason",
        TelemetryAttributeDefinition {
            kind: "string",
            required: false,
            values: Some(&["stop", "length", "tool_use", "error", "aborted", "deferred"]),
            cardinality: None,
            sensitive: false,
            description: "Normalized terminal response reason",
        },
    );
    end.insert(
        "pi.ai.http.status_code",
        TelemetryAttributeDefinition::number(false, "Final HTTP status"),
    );
    end.insert(
        "pi.ai.usage.input_tokens",
        TelemetryAttributeDefinition::number(false, "Reported input tokens"),
    );
    end.insert(
        "pi.ai.usage.output_tokens",
        TelemetryAttributeDefinition::number(false, "Reported output tokens"),
    );
    end.insert(
        "pi.ai.usage.cache_read_tokens",
        TelemetryAttributeDefinition::number(false, "Reported cache-read tokens"),
    );
    end.insert(
        "pi.ai.usage.cache_write_tokens",
        TelemetryAttributeDefinition::number(false, "Reported cache-write tokens"),
    );
    end.insert(
        "pi.ai.usage.reasoning_tokens",
        TelemetryAttributeDefinition::number(false, "Reported reasoning tokens"),
    );
    end.insert(
        "pi.ai.usage.total_tokens",
        TelemetryAttributeDefinition::number(false, "Reported total tokens"),
    );
    end.insert(
        "pi.ai.usage.cost",
        TelemetryAttributeDefinition::number(false, "Reported total cost"),
    );
    end.insert(
        "pi.ai.stream.chunk_count",
        TelemetryAttributeDefinition::number(false, "Streamed update chunk count"),
    );
    end.insert(
        "pi.ai.stream.time_to_first_chunk_ms",
        TelemetryAttributeDefinition::number(false, "Elapsed milliseconds to first update chunk"),
    );
    end.insert(
        "pi.ai.error.type",
        TelemetryAttributeDefinition {
            kind: "string",
            required: false,
            values: None,
            cardinality: Some("low"),
            sensitive: false,
            description: "Provider or transport error class",
        },
    );
    spans.insert(
        "pi.ai.request",
        TelemetrySpanDefinition {
            description: "One logical request to an AI provider",
            parents: TelemetryParentDefinition::Any,
            start_attributes: start,
            end_attributes: end,
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "The operation throws or returns an error result",
            },
        },
    );
    TelemetrySchemaDefinition { version: 1, spans }
}

/// Upstream `HOOK_NAMES`.
pub const HOOK_NAMES: &[&str] = &[
    "before_run",
    "before_resume",
    "before_run_end",
    "transform_context",
    "before_request",
    "before_payload",
    "after_response",
    "before_tool",
    "after_tool",
    "before_compaction",
    "before_navigation",
];

/// Upstream `EVENT_TYPES`.
pub const EVENT_TYPES: &[&str] = &[
    "run_start",
    "run_resume",
    "run_suspend",
    "run_abort",
    "run_end",
    "fault",
    "handler_error",
    "turn_start",
    "turn_end",
    "retry_scheduled",
    "retry_start",
    "retry_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_start",
    "tool_update",
    "tool_end",
    "entry_added",
    "write_pending",
    "queue_update",
    "fact_update",
    "config_update",
    "compaction_start",
    "compaction_end",
    "navigation_start",
    "navigation_end",
    "lane_created",
    "usage",
];

/// Upstream `HARNESS_TELEMETRY_SCHEMA`.
pub fn harness_telemetry_schema() -> TelemetrySchemaDefinition {
    let operation_start_attributes = |kind: &'static str, kind_values: &'static [&'static str]| {
        let mut start = BTreeMap::new();
        start.insert(
            "pi.session.id",
            TelemetryAttributeDefinition {
                kind: "string",
                required: true,
                values: None,
                cardinality: Some("high"),
                sensitive: false,
                description: "Session id",
            },
        );
        start.insert(
            "pi.lane.name",
            TelemetryAttributeDefinition {
                kind: "string",
                required: true,
                values: None,
                cardinality: Some("high"),
                sensitive: false,
                description: "Lane name",
            },
        );
        start.insert(
            "pi.operation.id",
            TelemetryAttributeDefinition {
                kind: "string",
                required: true,
                values: None,
                cardinality: Some("high"),
                sensitive: false,
                description: "Durable operation id",
            },
        );
        start.insert(
            "pi.operation.recovery",
            TelemetryAttributeDefinition::required_bool(
                "Whether this invocation resumes durable work",
            ),
        );
        start.insert(
            "pi.operation.kind",
            TelemetryAttributeDefinition {
                kind: "string",
                required: true,
                values: Some(kind_values),
                cardinality: None,
                sensitive: false,
                description: kind,
            },
        );
        start
    };

    let operation_error_attributes = |mut end: BTreeMap<&'static str, TelemetryAttributeDefinition>| {
        end.insert(
            "pi.error.code",
            TelemetryAttributeDefinition {
                kind: "string",
                required: false,
                values: None,
                cardinality: Some("low"),
                sensitive: false,
                description: "Stable operation error code",
            },
        );
        end.insert(
            "pi.error.type",
            TelemetryAttributeDefinition {
                kind: "string",
                required: false,
                values: None,
                cardinality: Some("low"),
                sensitive: false,
                description: "Low-cardinality operation error class",
            },
        );
        end
    };

    let mut spans = BTreeMap::new();

    spans.insert(
        "pi.harness.run",
        TelemetrySpanDefinition {
            description: "One admitted in-process run invocation",
            parents: TelemetryParentDefinition::RootOrExternal,
            start_attributes: operation_start_attributes("Run operation kind", &["run"]),
            end_attributes: operation_error_attributes(BTreeMap::from([(
                "pi.operation.outcome",
                TelemetryAttributeDefinition {
                    kind: "string",
                    required: false,
                    values: Some(&["completed", "aborted", "failed", "suspended"]),
                    cardinality: None,
                    sensitive: false,
                    description: "Run invocation outcome",
                },
            )])),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "The run fails or throws",
            },
        },
    );

    spans.insert(
        "pi.harness.compaction",
        TelemetrySpanDefinition {
            description: "One admitted in-process manual compaction invocation",
            parents: TelemetryParentDefinition::RootOrExternal,
            start_attributes: operation_start_attributes("Compaction operation kind", &["compaction"]),
            end_attributes: operation_error_attributes(BTreeMap::from([(
                "pi.operation.outcome",
                TelemetryAttributeDefinition {
                    kind: "string",
                    required: false,
                    values: Some(&["completed", "declined", "aborted", "failed"]),
                    cardinality: None,
                    sensitive: false,
                    description: "Compaction invocation outcome",
                },
            )])),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "The compaction fails or throws",
            },
        },
    );

    spans.insert(
        "pi.harness.navigation",
        TelemetrySpanDefinition {
            description: "One admitted in-process navigation invocation",
            parents: TelemetryParentDefinition::RootOrExternal,
            start_attributes: operation_start_attributes("Navigation operation kind", &["navigation"]),
            end_attributes: operation_error_attributes(BTreeMap::from([(
                "pi.operation.outcome",
                TelemetryAttributeDefinition {
                    kind: "string",
                    required: false,
                    values: Some(&["completed", "declined", "aborted", "failed"]),
                    cardinality: None,
                    sensitive: false,
                    description: "Navigation invocation outcome",
                },
            )])),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "The navigation fails or throws",
            },
        },
    );

    spans.insert(
        "pi.harness.checkpoint",
        TelemetrySpanDefinition {
            description: "One run checkpoint",
            parents: TelemetryParentDefinition::Spans {
                spans: &["pi.harness.run"],
            },
            start_attributes: BTreeMap::from([
                (
                    "pi.lane.name",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Lane name",
                    },
                ),
                (
                    "pi.operation.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Durable operation id",
                    },
                ),
                (
                    "pi.checkpoint.kind",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: Some(&["normal", "failure_drain", "abort_reconcile"]),
                        cardinality: None,
                        sensitive: false,
                        description: "Checkpoint purpose",
                    },
                ),
            ]),
            end_attributes: BTreeMap::new(),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "Checkpoint work throws",
            },
        },
    );

    spans.insert(
        "pi.harness.turn",
        TelemetrySpanDefinition {
            description: "One assistant response and its tool batch",
            parents: TelemetryParentDefinition::Spans {
                spans: &["pi.harness.run"],
            },
            start_attributes: BTreeMap::from([
                (
                    "pi.lane.name",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Lane name",
                    },
                ),
                (
                    "pi.operation.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Durable operation id",
                    },
                ),
                (
                    "pi.turn.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Invocation-local turn id",
                    },
                ),
            ]),
            end_attributes: BTreeMap::new(),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "Turn work throws",
            },
        },
    );

    spans.insert(
        "pi.harness.step",
        TelemetrySpanDefinition {
            description: "One durable retry attempt",
            parents: TelemetryParentDefinition::Spans {
                spans: &[
                    "pi.harness.turn",
                    "pi.harness.checkpoint",
                    "pi.harness.compaction",
                    "pi.harness.navigation",
                ],
            },
            start_attributes: BTreeMap::from([
                (
                    "pi.lane.name",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Lane name",
                    },
                ),
                (
                    "pi.operation.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Durable operation id",
                    },
                ),
                (
                    "pi.step.kind",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: Some(&["assistant", "compaction", "branch_summary"]),
                        cardinality: None,
                        sensitive: false,
                        description: "Retryable step kind",
                    },
                ),
                (
                    "pi.step.attempt",
                    TelemetryAttributeDefinition::number(true, "One-based durable attempt number"),
                ),
                (
                    "pi.compaction.reason",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: false,
                        values: Some(&["manual", "threshold", "overflow"]),
                        cardinality: None,
                        sensitive: false,
                        description: "Compaction trigger",
                    },
                ),
            ]),
            end_attributes: BTreeMap::from([(
                "pi.step.outcome",
                TelemetryAttributeDefinition {
                    kind: "string",
                    required: false,
                    values: Some(&["succeeded", "retry", "failed", "aborted", "deferred", "overflow"]),
                    cardinality: None,
                    sensitive: false,
                    description: "Attempt outcome",
                },
            )]),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "The attempt retries, fails, or throws",
            },
        },
    );

    spans.insert(
        "pi.harness.tool",
        TelemetrySpanDefinition {
            description: "One raw phase-2 tool execution",
            parents: TelemetryParentDefinition::Spans {
                spans: &["pi.harness.turn", "pi.harness.run"],
            },
            start_attributes: BTreeMap::from([
                (
                    "pi.lane.name",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Lane name",
                    },
                ),
                (
                    "pi.operation.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Durable operation id",
                    },
                ),
                (
                    "pi.turn.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: false,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Invocation-local live turn id",
                    },
                ),
                (
                    "pi.tool.name",
                    TelemetryAttributeDefinition::required_string("Tool name"),
                ),
                (
                    "pi.tool.call_id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Tool call id",
                    },
                ),
                (
                    "pi.tool.replay",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: Some(&["never", "safe"]),
                        cardinality: None,
                        sensitive: false,
                        description: "Declared replay policy",
                    },
                ),
                (
                    "pi.tool.recovery",
                    TelemetryAttributeDefinition::required_bool("Whether this is recovery execution"),
                ),
            ]),
            end_attributes: BTreeMap::from([(
                "pi.tool.is_error",
                TelemetryAttributeDefinition::optional_bool(
                    "Whether raw phase-2 execution returned an error",
                ),
            )]),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "Raw phase-2 execution returns an error",
            },
        },
    );

    spans.insert(
        "pi.harness.hook",
        TelemetrySpanDefinition {
            description: "One registered hook handler invocation",
            parents: TelemetryParentDefinition::Any,
            start_attributes: BTreeMap::from([
                (
                    "pi.lane.name",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Lane name",
                    },
                ),
                (
                    "pi.operation.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: false,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Durable operation id when accepted",
                    },
                ),
                (
                    "pi.hook.name",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: Some(HOOK_NAMES),
                        cardinality: None,
                        sensitive: false,
                        description: "Hook name",
                    },
                ),
                (
                    "pi.hook.registration_id",
                    TelemetryAttributeDefinition::optional_string("Stable hook registration id"),
                ),
            ]),
            end_attributes: BTreeMap::from([(
                "pi.hook.outcome",
                TelemetryAttributeDefinition {
                    kind: "string",
                    required: false,
                    values: Some(&["completed", "skipped", "blocked", "failed"]),
                    cardinality: None,
                    sensitive: false,
                    description: "Handler outcome",
                },
            )]),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "The handler throws",
            },
        },
    );

    spans.insert(
        "pi.harness.sleep",
        TelemetrySpanDefinition {
            description: "One retry delay",
            parents: TelemetryParentDefinition::Spans {
                spans: &["pi.harness.step", "pi.harness.run"],
            },
            start_attributes: BTreeMap::from([
                (
                    "pi.operation.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Durable operation id",
                    },
                ),
                (
                    "pi.sleep.delay_ms",
                    TelemetryAttributeDefinition::number(true, "Requested delay in milliseconds"),
                ),
            ]),
            end_attributes: BTreeMap::from([(
                "pi.sleep.outcome",
                TelemetryAttributeDefinition {
                    kind: "string",
                    required: false,
                    values: Some(&["elapsed", "aborted"]),
                    cardinality: None,
                    sensitive: false,
                    description: "Delay outcome",
                },
            )]),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "Sleep work throws",
            },
        },
    );

    spans.insert(
        "pi.harness.event_handler",
        TelemetrySpanDefinition {
            description: "One passive event listener invocation",
            parents: TelemetryParentDefinition::Any,
            start_attributes: BTreeMap::from([
                (
                    "pi.event.type",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: Some(EVENT_TYPES),
                        cardinality: Some("low"),
                        sensitive: false,
                        description: "Delivered harness event type",
                    },
                ),
                (
                    "pi.lane.name",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: false,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Lane name for lane-scoped events",
                    },
                ),
            ]),
            end_attributes: BTreeMap::new(),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "The listener throws",
            },
        },
    );

    spans.insert(
        "pi.session.write",
        TelemetrySpanDefinition {
            description: "One committed session mutation",
            parents: TelemetryParentDefinition::Any,
            start_attributes: BTreeMap::from([
                (
                    "pi.lane.name",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Lane name",
                    },
                ),
                (
                    "pi.operation.id",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: false,
                        values: None,
                        cardinality: Some("high"),
                        sensitive: false,
                        description: "Durable operation id when accepted",
                    },
                ),
                (
                    "pi.session.mutation",
                    TelemetryAttributeDefinition {
                        kind: "string",
                        required: true,
                        values: Some(&["entry", "record", "lane", "fact"]),
                        cardinality: None,
                        sensitive: false,
                        description: "Session mutation kind",
                    },
                ),
                (
                    "pi.session.item_type",
                    TelemetryAttributeDefinition::optional_string(
                        "Entry, record, lane, or fact subtype",
                    ),
                ),
            ]),
            end_attributes: BTreeMap::from([(
                "pi.session.seq",
                TelemetryAttributeDefinition::number(false, "Committed session sequence when exposed"),
            )]),
            status: TelemetrySpanStatus {
                default: "ok",
                error_when: "Storage rejects the mutation",
            },
        },
    );

    TelemetrySchemaDefinition { version: 1, spans }
}

/// Upstream `AGENT_TELEMETRY_SCHEMAS`.
pub fn agent_telemetry_schemas() -> Vec<TelemetrySchemaDefinition> {
    vec![ai_telemetry_schema(), harness_telemetry_schema()]
}

// --- Typed span starters ---------------------------------------------------

/// Upstream `AiSpanStartAttributes` operation values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiOperation {
    Stream,
    FetchDeferred,
    CancelDeferred,
    GenerateImages,
}

impl AiOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::FetchDeferred => "fetch_deferred",
            Self::CancelDeferred => "cancel_deferred",
            Self::GenerateImages => "generate_images",
        }
    }
}

/// Upstream `pi.ai.response.stop_reason` value set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiResponseStopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

impl AiResponseStopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolUse => "tool_use",
            Self::Error => "error",
            Self::Aborted => "aborted",
            Self::Deferred => "deferred",
        }
    }
}

/// Required start attributes for a `pi.ai.request` span (upstream
/// `AiSpanStartAttributes<"pi.ai.request">`; optional `deferred` is set
/// separately).
#[derive(Debug, Clone, PartialEq)]
pub struct AiSpanStartAttributes {
    pub operation: AiOperation,
    pub provider: String,
    pub model: String,
    pub api: String,
    pub streaming: bool,
    /// Optional `pi.ai.deferred`.
    pub deferred: Option<bool>,
}

/// End attribute enrichment for a `pi.ai.request` span (upstream
/// `AiSpanEndAttributes`; all optional).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AiSpanEndAttributes {
    pub response_model: Option<String>,
    pub response_id: Option<String>,
    pub response_stop_reason: Option<AiResponseStopReason>,
    pub http_status_code: Option<f64>,
    pub usage_input_tokens: Option<f64>,
    pub usage_output_tokens: Option<f64>,
    pub usage_cache_read_tokens: Option<f64>,
    pub usage_cache_write_tokens: Option<f64>,
    pub usage_reasoning_tokens: Option<f64>,
    pub usage_total_tokens: Option<f64>,
    pub usage_cost: Option<f64>,
    pub stream_chunk_count: Option<f64>,
    pub stream_time_to_first_chunk_ms: Option<f64>,
    pub error_type: Option<String>,
}

impl AiSpanEndAttributes {
    /// Merge into the span handle (upstream `span.setAttributes({...})`).
    pub fn set_on(&self, span: &SpanHandle) {
        let mut attributes = BTreeMap::new();
        if let Some(value) = &self.response_model {
            attributes.insert("pi.ai.response.model".to_owned(), value.as_str().into());
        }
        if let Some(value) = &self.response_id {
            attributes.insert("pi.ai.response.id".to_owned(), value.as_str().into());
        }
        if let Some(value) = self.response_stop_reason {
            attributes.insert(
                "pi.ai.response.stop_reason".to_owned(),
                value.as_str().into(),
            );
        }
        if let Some(value) = self.http_status_code {
            attributes.insert("pi.ai.http.status_code".to_owned(), value.into());
        }
        for (key, value) in [
            ("pi.ai.usage.input_tokens", self.usage_input_tokens),
            ("pi.ai.usage.output_tokens", self.usage_output_tokens),
            ("pi.ai.usage.cache_read_tokens", self.usage_cache_read_tokens),
            ("pi.ai.usage.cache_write_tokens", self.usage_cache_write_tokens),
            ("pi.ai.usage.reasoning_tokens", self.usage_reasoning_tokens),
            ("pi.ai.usage.total_tokens", self.usage_total_tokens),
            ("pi.ai.usage.cost", self.usage_cost),
            ("pi.ai.stream.chunk_count", self.stream_chunk_count),
            ("pi.ai.stream.time_to_first_chunk_ms", self.stream_time_to_first_chunk_ms),
        ] {
            if let Some(value) = value {
                attributes.insert(key.to_owned(), value.into());
            }
        }
        if let Some(value) = &self.error_type {
            attributes.insert("pi.ai.error.type".to_owned(), value.as_str().into());
        }
        span.set_attributes(attributes);
    }
}

/// Upstream `startAiSpan`: run `callback` inside a `pi.ai.request` span.
pub async fn start_ai_span<T, F, Fut>(
    telemetry_context: &dyn TelemetryContext,
    attributes: AiSpanStartAttributes,
    callback: F,
) -> T
where
    T: Send + 'static,
    F: FnOnce(SpanHandle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
{
    let AiSpanStartAttributes {
        operation,
        provider,
        model,
        api,
        streaming,
        deferred,
    } = attributes;
    let mut options = SpanOptions::new("pi.ai.request")
        .with_attribute("pi.ai.operation", operation.as_str())
        .with_attribute("pi.ai.provider", provider.as_str())
        .with_attribute("pi.ai.model", model.as_str())
        .with_attribute("pi.ai.api", api.as_str())
        .with_attribute("pi.ai.streaming", streaming);
    if let Some(deferred) = deferred {
        options = options.with_attribute("pi.ai.deferred", deferred);
    }
    let _ = std::marker::PhantomData::<fn() -> SpanStatus>;
    telemetry_context.start_span(options, |span| async move { callback(span).await }).await
}

#[allow(dead_code)] // phantom-marker witness for the status vocabulary
fn _status_witness(_: std::marker::PhantomData<fn() -> SpanStatus>) {}
