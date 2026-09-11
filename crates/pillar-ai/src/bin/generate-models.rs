//! Port of packages/ai/scripts/generate-models.ts (pi v0.84.3).
//!
//! Generates the built-in model catalog from live provider catalogs:
//! - models.dev api.json (primary source)
//! - OpenRouter /v1/models (xAI and other providers)
//! - Vercel AI Gateway /models
//!
//! Outputs:
//! - `src/models_data/<provider>.json` — per-provider model groups keyed by
//!   API (upstream `src/providers/data/<provider>.json`, gitignored there;
//!   committed here so builds stay offline-reproducible)
//! - `src/models_data/.manifest.json` — structure manifest with SHA-256s
//! - `src/models_generated.rs` — a committed aggregator exposing `MODELS`
//!
//! Run: `cargo run -p pillar-ai --features generate-models --bin generate-models`
//!

//! divergence: the upstream TS generator also emits `.models.ts` shards; the
//! Rust port collapses the shards into one generated file since JSON data is
//! embedded via `include_str!`. Live fetch failures are fatal (upstream
//! `--strict` is the default here).

#![allow(clippy::type_complexity, clippy::neg_cmp_op_on_partial_ord)]
#![allow(clippy::too_many_lines, clippy::similar_names)]
#![allow(
    clippy::collapsible_if,
    clippy::redundant_clone,
    clippy::field_reassign_with_default
)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Generated-source constants (upstream generate-models.ts constants)
// ---------------------------------------------------------------------------

const KIMI_K3_MAX_TOKENS: u64 = 131_072;
const KIMI_K3_COST: [(&str, f64); 4] = [
    ("input", 3.0),
    ("output", 15.0),
    ("cacheRead", 0.3),
    ("cacheWrite", 0.0),
];

const KIMI_CODING_IMPLIED_COSTS: &[(&str, [(&str, f64); 4])] = &[
    ("k3", KIMI_K3_COST),
    (
        "kimi-for-coding",
        [
            ("input", 0.95),
            ("output", 4.0),
            ("cacheRead", 0.19),
            ("cacheWrite", 0.0),
        ],
    ),
    (
        "kimi-for-coding-highspeed",
        [
            ("input", 1.9),
            ("output", 8.0),
            ("cacheRead", 0.38),
            ("cacheWrite", 0.0),
        ],
    ),
    (
        "kimi-k2-thinking",
        [
            ("input", 0.6),
            ("output", 2.5),
            ("cacheRead", 0.15),
            ("cacheWrite", 0.0),
        ],
    ),
];

const OPENROUTER_KIMI_K3_MODEL_IDS: [&str; 2] = ["moonshotai/kimi-k3", "~moonshotai/kimi-latest"];

const BEDROCK_INFERENCE_PROFILE_ONLY_MODEL_IDS: [&str; 1] = ["anthropic.claude-opus-5"];
const MODELS_DEV_OPENAI_UNSUPPORTED_MODEL_IDS: [&str; 1] = ["gpt-5.6"];

const OPENAI_TOOL_SEARCH_MODEL_IDS: [&str; 7] = [
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.4-pro",
    "gpt-5.5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];
const OPENAI_CODEX_ADDITIONAL_TOOLS_MODEL_IDS: [&str; 3] =
    ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];

const OPENAI_LONG_CONTEXT_INPUT_THRESHOLD: u64 = 272_000;
const OPENAI_SHORT_CONTEXT_CAPPED_MODEL_IDS: [&str; 5] = [
    "gpt-5.4",
    "gpt-5.5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];
const OPENAI_LONG_CONTEXT_PRICING_MODEL_IDS: [&str; 7] = [
    "gpt-5.4",
    "gpt-5.4-pro",
    "gpt-5.5",
    "gpt-5.5-pro",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

const OPENAI_GPT_56_STANDARD_COSTS: &[(&str, [(&str, f64); 4])] = &[
    (
        "gpt-5.6-luna",
        [
            ("input", 0.2),
            ("output", 1.2),
            ("cacheRead", 0.02),
            ("cacheWrite", 0.25),
        ],
    ),
    (
        "gpt-5.6-terra",
        [
            ("input", 2.0),
            ("output", 12.0),
            ("cacheRead", 0.2),
            ("cacheWrite", 2.5),
        ],
    ),
];

const OPENAI_RESPONSES_NONE_REASONING_MODELS: [&str; 10] = [
    "gpt-5.1",
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.4-nano",
    "gpt-5.5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

const XAI_BUILTIN_EXCLUDED_MODEL_IDS: [&str; 5] = [
    "grok-3",
    "grok-3-fast",
    "grok-4.20-0309-non-reasoning",
    "grok-4.20-0309-reasoning",
    "grok-code-fast-1",
];

const OPENCODE_OPENAI_COMPLETIONS_LONG_CACHE_RETENTION_UNSUPPORTED: &[&str] = &[
    "opencode:deepseek-v4-flash",
    "opencode:deepseek-v4-pro",
    "opencode:kimi-k2.5",
    "opencode:kimi-k2.6",
    "opencode:minimax-m2.7",
    "opencode-go:kimi-k2.6",
];

const QWEN_TOKEN_PLAN_REASONING_EFFORT_UNSUPPORTED: [&str; 8] = [
    "MiniMax-M2.5",
    "deepseek-v3.2",
    "kimi-k2.5",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "qwen3.6-flash",
    "qwen3.6-plus",
    "qwen3.7-max",
];

const QWEN_TOKEN_PLAN_EXCLUDED_MODEL_IDS: [&str; 1] = ["qwen3.8-max-preview"];
const QWEN_TOKEN_PLAN_PROVIDER_IDS: [&str; 3] = [
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
];

const QWEN_TOKEN_PLAN_INDIVIDUAL_MODEL_IDS: [&str; 8] = [
    "deepseek-v4-flash-0731",
    "deepseek-v4-pro",
    "deepseek-v4-pro-0813",
    "glm-5.2",
    "qwen3.6-flash",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.8-max",
];

const ZAI_TOOL_STREAM_UNSUPPORTED_MODELS: [&str; 4] =
    ["glm-4.5", "glm-4.5-air", "glm-4.5-flash", "glm-4.5v"];

const NVIDIA_NIM_UNSUPPORTED_MODELS: [&str; 18] = [
    "abacusai/dracarys-llama-3.1-70b-instruct",
    "bytedance/seed-oss-36b-instruct",
    "deepseek-ai/deepseek-v4-flash",
    "deepseek-ai/deepseek-v4-pro",
    "google/gemma-2-2b-it",
    "google/gemma-3n-e2b-it",
    "google/gemma-3n-e4b-it",
    "google/gemma-4-31b-it",
    "meta/llama-3.2-1b-instruct",
    "meta/llama-4-maverick-17b-128e-instruct",
    "microsoft/phi-4-mini-instruct",
    "minimaxai/minimax-m2.7",
    "mistralai/mistral-nemotron",
    "nvidia/nemotron-mini-4b-instruct",
    "qwen/qwen3-next-80b-a3b-instruct",
    "qwen/qwen3.5-397b-a17b",
    "sarvamai/sarvam-m",
    "upstage/solar-10.7b-instruct",
];

const EAGER_TOOL_INPUT_STREAMING_UNSUPPORTED_ANTHROPIC: [&str; 3] = [
    "github-copilot:claude-haiku-4.5",
    "github-copilot:claude-sonnet-4",
    "github-copilot:claude-sonnet-4.5",
];

const ANTHROPIC_ALLOWED_FALLBACK_MODELS: &[(&str, [&str; 2])] = &[
    ("claude-fable-5", ["claude-opus-4-8", "claude-opus-5"]),
    ("claude-opus-5", ["claude-opus-4-8", "claude-opus-4-8"]),
];

const COPILOT_STATIC_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];

const NVIDIA_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";
const NVIDIA_HEADERS: [(&str, &str); 1] = [("NVCF-POLL-SECONDS", "3600")];

const CLOUDFLARE_WORKERS_AI_BASE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1";
const CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat";
const CLOUDFLARE_AI_GATEWAY_OPENAI_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai";
const CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL: &str = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic";

const TOGETHER_BASE_URL: &str = "https://api.together.ai/v1";
const AI_GATEWAY_MODELS_URL: &str = "https://ai-gateway.vercel.sh/v1";
const AI_GATEWAY_BASE_URL: &str = "https://ai-gateway.vercel.sh";

const TOGETHER_REASONING_ONLY_MODELS: [&str; 2] =
    ["deepseek-ai/DeepSeek-R1", "MiniMaxAI/MiniMax-M2.7"];
const TOGETHER_REASONING_EFFORT_MODELS: [&str; 2] = ["openai/gpt-oss-20b", "openai/gpt-oss-120b"];
const TOGETHER_TOGGLE_REASONING_EFFORT_MODELS: [&str; 1] = ["deepseek-ai/DeepSeek-V4-Pro"];

const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

// ---------------------------------------------------------------------------
// Model record (serialized to JSON; shape mirrors upstream Model)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ModelRec {
    id: String,
    name: String,
    api: String,
    provider: String,
    base_url: String,
    reasoning: bool,
    thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    input: Vec<String>,
    cost: Cost,
    context_window: u64,
    max_tokens: u64,
    headers: Option<Vec<(String, String)>>,
    compat: Option<Value>,
}

#[derive(Clone, Debug, Default)]
struct Cost {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    tiers: Option<Vec<CostTier>>,
}

#[derive(Clone, Debug)]
struct CostTier {
    input_tokens_above: u64,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

impl Cost {
    fn to_json(&self) -> Value {
        let mut object = json!({
            "input": self.input,
            "output": self.output,
            "cacheRead": self.cache_read,
            "cacheWrite": self.cache_write,
        });
        if let Some(tiers) = &self.tiers {
            object["tiers"] = json!(
                tiers
                    .iter()
                    .map(|tier| json!({
                        "inputTokensAbove": tier.input_tokens_above,
                        "input": tier.input,
                        "output": tier.output,
                        "cacheRead": tier.cache_read,
                        "cacheWrite": tier.cache_write,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        object
    }
}

impl ModelRec {
    fn new(id: &str, name: &str, api: &str, provider: &str, base_url: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            api: api.to_string(),
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec!["text".to_string()],
            cost: Cost::default(),
            context_window: 4096,
            max_tokens: 4096,
            headers: None,
            compat: None,
        }
    }

    fn to_json(&self) -> Value {
        let mut object = json!({
            "id": self.id,
            "name": self.name,
            "api": self.api,
            "provider": self.provider,
            "baseUrl": self.base_url,
            "reasoning": self.reasoning,
            "input": self.input,
            "cost": self.cost.to_json(),
            "contextWindow": self.context_window,
            "maxTokens": self.max_tokens,
        });
        if let Some(map) = &self.thinking_level_map {
            object["thinkingLevelMap"] = json!(map);
        }
        if let Some(headers) = &self.headers {
            object["headers"] = json!(headers.iter().cloned().collect::<BTreeMap<_, _>>());
        }
        if let Some(compat) = &self.compat {
            object["compat"] = compat.clone();
        }
        object
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn round_cost(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn cost_of(model: &Value) -> Cost {
    Cost {
        input: model
            .pointer("/cost/input")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        output: model
            .pointer("/cost/output")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        cache_read: model
            .pointer("/cost/cache_read")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        cache_write: model
            .pointer("/cost/cache_write")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        tiers: None,
    }
}

fn input_of(model: &Value) -> Vec<String> {
    let image = model
        .pointer("/modalities/input")
        .and_then(Value::as_array)
        .map(|items| items.iter().any(|item| item.as_str() == Some("image")))
        .unwrap_or(false);
    if image {
        vec!["text".to_string(), "image".to_string()]
    } else {
        vec!["text".to_string()]
    }
}

/// Upstream `getEffortThinkingLevelMap`.
fn effort_thinking_level_map(options: &[Value]) -> Option<BTreeMap<String, Option<String>>> {
    let mut effort_values = BTreeSet::new();
    for option in options {
        if option.get("type").and_then(Value::as_str) == Some("effort") {
            if let Some(values) = option.get("values").and_then(Value::as_array) {
                for value in values {
                    if let Some(value) = value.as_str() {
                        effort_values.insert(value.to_string());
                    }
                }
            }
        }
    }
    if effort_values.is_empty() {
        return None;
    }
    let has_known = THINKING_LEVELS
        .iter()
        .any(|level| effort_values.contains(*level));
    if !has_known && !effort_values.contains("none") {
        return None;
    }
    let mut map = BTreeMap::new();
    map.insert(
        "off".to_string(),
        effort_values.contains("none").then(|| "none".to_string()),
    );
    for level in THINKING_LEVELS {
        if level == "off" {
            continue;
        }
        map.insert(
            level.to_string(),
            effort_values.contains(level).then(|| level.to_string()),
        );
    }
    Some(map)
}

/// Upstream `getOpenRouterThinkingLevelMap`.
fn openrouter_thinking_level_map(
    reasoning: Option<&Value>,
) -> Option<BTreeMap<String, Option<String>>> {
    let reasoning = reasoning?;
    let supported_efforts: Vec<String> = reasoning
        .get("supported_efforts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mandatory = reasoning
        .get("mandatory")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if supported_efforts.is_empty() {
        return mandatory.then(|| {
            let mut map = BTreeMap::new();
            map.insert("off".to_string(), None);
            map
        });
    }
    let options: Vec<Value> = vec![json!({ "type": "effort", "values": supported_efforts })];
    let mut map = effort_thinking_level_map(&options)?;
    map.insert(
        "off".to_string(),
        if mandatory {
            None
        } else {
            Some("none".to_string())
        },
    );
    Some(map)
}

fn merge_thinking_level_map(model: &mut ModelRec, map: BTreeMap<String, Option<String>>) {
    let entry = model.thinking_level_map.get_or_insert_with(BTreeMap::new);
    for (key, value) in map {
        entry.insert(key, value);
    }
}

fn merge_compat(model: &mut ModelRec, compat: Value) {
    let merged = match model.compat.take() {
        Some(existing) if existing.as_object().is_some() && compat.as_object().is_some() => {
            let mut merged = existing.as_object().unwrap().clone();
            for (key, value) in compat.as_object().unwrap() {
                merged.insert(key.clone(), value.clone());
            }
            Value::Object(merged)
        }
        _ => compat,
    };
    model.compat = (merged.as_object().map(|object| !object.is_empty()))
        .unwrap_or(false)
        .then_some(merged);
}

fn level_map(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
    entries
        .iter()
        .map(|(key, value)| (key.to_string(), value.map(str::to_string)))
        .collect()
}

fn with_open_ai_long_context_pricing(mut cost: Cost) -> Cost {
    cost.tiers = Some(vec![CostTier {
        input_tokens_above: OPENAI_LONG_CONTEXT_INPUT_THRESHOLD,
        input: round_cost(cost.input * 2.0),
        output: round_cost(cost.output * 1.5),
        cache_read: round_cost(cost.cache_read * 2.0),
        cache_write: round_cost(cost.cache_write * 2.0),
    }]);
    cost
}

fn cost_from_entries(entries: [(&str, f64); 4]) -> Cost {
    let get = |name: &str| {
        entries
            .iter()
            .find(|(key, _)| *key == name)
            .map_or(0.0, |(_, value)| *value)
    };
    Cost {
        input: get("input"),
        output: get("output"),
        cache_read: get("cacheRead"),
        cache_write: get("cacheWrite"),
        tiers: None,
    }
}

// ---------------------------------------------------------------------------
// models.dev processing (upstream loadModelsDevData)
// ---------------------------------------------------------------------------

fn get_bedrock_base_url(model_id: &str) -> &'static str {
    if model_id.starts_with("eu.") {
        "https://bedrock-runtime.eu-central-1.amazonaws.com"
    } else {
        "https://bedrock-runtime.us-east-1.amazonaws.com"
    }
}

fn normalize_nvidia_model_id(model_id: &str) -> String {
    model_id.to_lowercase().replace('_', ".")
}

fn get_models_dev_cost(cost: &Value) -> Cost {
    let mut base = Cost {
        input: cost.get("input").and_then(Value::as_f64).unwrap_or(0.0),
        output: cost.get("output").and_then(Value::as_f64).unwrap_or(0.0),
        cache_read: cost
            .get("cache_read")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        cache_write: cost
            .get("cache_write")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        tiers: None,
    };
    let tiers: Vec<CostTier> = cost
        .get("tiers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|tier| {
                    let context = tier.get("tier")?;
                    if context.get("type").and_then(Value::as_str) != Some("context") {
                        return None;
                    }
                    let size = context.get("size")?.as_u64()?;
                    Some(CostTier {
                        input_tokens_above: size,
                        input: tier.get("input").and_then(Value::as_f64).unwrap_or(0.0),
                        output: tier.get("output").and_then(Value::as_f64).unwrap_or(0.0),
                        cache_read: tier
                            .get("cache_read")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0),
                        cache_write: tier
                            .get("cache_write")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if !tiers.is_empty() {
        base.tiers = Some(tiers);
    }
    base
}

/// Upstream `detectOpenAICompletionsCompat` (URL/provider heuristics).
fn detect_openai_completions_compat(provider: &str, base_url: &str, model_id: &str) -> Value {
    let is_zai = provider == "zai"
        || provider == "zai-coding-cn"
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot = provider == "moonshotai"
        || provider == "moonshotai-cn"
        || base_url.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare_workers_ai =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_ai_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_together_reasoning_only =
        is_together && TOGETHER_REASONING_ONLY_MODELS.contains(&model_id);
    let is_deep_seek = provider == "deepseek" || base_url.to_lowercase().contains("deepseek.com");

    let is_non_standard = is_nvidia
        || provider == "cerebras"
        || base_url.contains("cerebras.ai")
        || provider == "xai"
        || base_url.contains("api.x.ai")
        || is_together
        || base_url.contains("chutes.ai")
        || is_deep_seek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers_ai
        || is_cloudflare_ai_gateway
        || is_ant_ling;

    let use_max_tokens = base_url.contains("chutes.ai")
        || is_deep_seek
        || is_moonshot
        || is_cloudflare_ai_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;

    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_openrouter_developer_role_model =
        is_openrouter && (model_id.starts_with("anthropic/") || model_id.starts_with("openai/"));
    let cache_control_format = if provider == "openrouter"
        && (model_id.starts_with("anthropic/") || model_id.starts_with("~anthropic/"))
    {
        Some("anthropic")
    } else {
        None
    };

    let thinking_format = if is_deep_seek {
        "deepseek"
    } else if is_zai {
        "zai"
    } else if is_together && !is_together_reasoning_only {
        "together"
    } else if is_ant_ling {
        "ant-ling"
    } else if is_openrouter {
        "openrouter"
    } else {
        "openai"
    };

    json!({
        "supportsStore": !is_non_standard,
        "supportsDeveloperRole": is_openrouter_developer_role_model || (!is_non_standard && !is_openrouter),
        "supportsReasoningEffort": !is_grok && !is_zai && !is_moonshot && !is_together && !is_cloudflare_ai_gateway && !is_nvidia && !is_ant_ling,
        "supportsUsageInStreaming": true,
        "supportsFinishReason": true,
        "maxTokensField": if use_max_tokens { "max_tokens" } else { "max_completion_tokens" },
        "requiresToolResultName": false,
        "requiresAssistantAfterToolResult": false,
        "requiresThinkingAsText": false,
        "requiresReasoningContentOnAssistantMessages": is_deep_seek,
        "thinkingFormat": thinking_format,
        "zaiToolStream": false,
        "supportsStrictMode": !is_moonshot && !is_together && !is_cloudflare_ai_gateway && !is_nvidia,
        "sendSessionAffinityHeaders": false,
        "supportsLongCacheRetention": !(is_together || is_cloudflare_workers_ai || is_cloudflare_ai_gateway || is_nvidia || is_ant_ling),
        "cacheControlFormat": cache_control_format,
    })
}

/// Upstream `openAICompletionsCompatDelta`: keep only non-default fields.
fn openai_completions_compat_delta(compat: &Value) -> Value {
    let defaults = json!({
        "supportsStore": true,
        "supportsDeveloperRole": true,
        "supportsReasoningEffort": true,
        "supportsUsageInStreaming": true,
        "supportsFinishReason": true,
        "maxTokensField": "max_completion_tokens",
        "requiresToolResultName": false,
        "requiresAssistantAfterToolResult": false,
        "requiresThinkingAsText": false,
        "requiresReasoningContentOnAssistantMessages": false,
        "thinkingFormat": "openai",
        "zaiToolStream": false,
        "supportsStrictMode": true,
        "supportsOpenAIGrammarTools": false,
        "sendSessionAffinityHeaders": false,
        "supportsLongCacheRetention": true,
    });
    let mut delta = serde_json::Map::new();
    if let (Some(compat), Some(defaults)) = (compat.as_object(), defaults.as_object()) {
        for (key, value) in compat {
            if defaults.get(key) == Some(value) {
                continue;
            }
            delta.insert(key.clone(), value.clone());
        }
    }
    Value::Object(delta)
}

/// Upstream `getAnthropicMessagesCompat`.
fn get_anthropic_messages_compat(provider: &str, model_id: &str) -> Option<Value> {
    let mut compat = serde_json::Map::new();
    let key = format!("{provider}:{model_id}");
    if EAGER_TOOL_INPUT_STREAMING_UNSUPPORTED_ANTHROPIC.contains(&key.as_str()) {
        compat.insert("supportsEagerToolInputStreaming".to_string(), json!(false));
    }
    if provider == "xiaomi" || provider.starts_with("xiaomi-token-plan-") {
        compat.insert("allowEmptySignature".to_string(), json!(true));
    }
    (!compat.is_empty()).then_some(Value::Object(compat))
}

fn supports_open_ai_xhigh(model_id: &str) -> bool {
    ["gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5", "gpt-5.6"]
        .iter()
        .any(|needle| model_id.contains(needle))
}

fn supports_open_ai_max(model: &ModelRec) -> bool {
    model.id.contains("gpt-5.6")
        && matches!(
            model.api.as_str(),
            "openai-responses"
                | "azure-openai-responses"
                | "openai-codex-responses"
                | "openai-completions"
        )
}

fn is_gemini_3_pro_model(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    lower.contains("gemini-3-pro")
        || lower.contains("gemini-3.0-pro")
        || lower.contains("gemini-3.1-pro")
        || lower.contains("gemini-3.5-pro")
        || (lower.starts_with("gemini-3") && lower.contains("-pro"))
}

fn is_gemini_3_flash_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    (id.starts_with("gemini-3") && id.contains("-flash"))
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
}

fn is_gemma_4_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    id.contains("gemma-4") || id.contains("gemma4")
}

fn is_anthropic_adaptive_thinking_model(model_id: &str) -> bool {
    model_id.contains("opus-4-6")
        || model_id.contains("opus-4.6")
        || model_id.contains("opus-4-7")
        || model_id.contains("opus-4.7")
        || model_id.contains("opus-4-8")
        || model_id.contains("opus-4.8")
        || model_id.contains("opus-5")
        || model_id.contains("opus.5")
        || model_id.contains("sonnet-4-6")
        || model_id.contains("sonnet-4.6")
        || model_id.contains("sonnet-5")
        || model_id.contains("sonnet.5")
        || model_id.contains("fable-5")
}

fn is_anthropic_temperature_unsupported_model(model_id: &str) -> bool {
    model_id.contains("opus-4-7")
        || model_id.contains("opus-4.7")
        || model_id.contains("opus-4-8")
        || model_id.contains("opus-4.8")
        || model_id.contains("opus-5")
        || model_id.contains("opus.5")
}

fn is_google_thinking_api(api: &str) -> bool {
    api == "google-generative-ai" || api == "google-vertex"
}

/// Upstream `applyOpenAICompletionsCompatMetadata`.
fn apply_openai_completions_compat_metadata(model: &mut ModelRec) {
    if model.api != "openai-completions" {
        return;
    }
    let detected = openai_completions_compat_delta(&detect_openai_completions_compat(
        &model.provider,
        &model.base_url,
        &model.id,
    ));
    let merged = match model.compat.take() {
        Some(existing) if existing.as_object().is_some() => {
            let mut merged = detected.as_object().cloned().unwrap_or_default();
            for (key, value) in existing.as_object().unwrap() {
                merged.insert(key.clone(), value.clone());
            }
            Value::Object(merged)
        }
        _ => detected,
    };
    model.compat = (!merged
        .as_object()
        .map(|object| object.is_empty())
        .unwrap_or(false))
    .then_some(merged);
}

/// Upstream `applyAnthropicMessagesCompatMetadata`.
fn apply_anthropic_messages_compat_metadata(model: &mut ModelRec) {
    if model.api != "anthropic-messages" {
        return;
    }
    if let Some(compat) = get_anthropic_messages_compat(&model.provider, &model.id) {
        merge_compat(model, compat);
    }
}

/// Upstream `applyStrictToolCompatMetadata`.
fn apply_strict_tool_compat_metadata(model: &mut ModelRec) {
    if (model.provider == "openai" || model.provider == "cloudflare-ai-gateway")
        && model.api == "openai-responses"
    {
        merge_compat(model, json!({ "supportsStrictMode": true }));
    } else if model.provider == "anthropic" && model.api == "anthropic-messages" {
        merge_compat(model, json!({ "supportsStrictTools": true }));
    }
}

/// Upstream `applyOpenAIGrammarToolCompatMetadata`.
fn apply_openai_grammar_tool_compat_metadata(model: &mut ModelRec) {
    const PROVIDERS: [&str; 6] = [
        "openai",
        "openai-codex",
        "azure-openai-responses",
        "github-copilot",
        "opencode",
        "cloudflare-ai-gateway",
    ];
    const APIS: [&str; 3] = [
        "openai-responses",
        "azure-openai-responses",
        "openai-codex-responses",
    ];
    if !APIS.contains(&model.api.as_str()) || !PROVIDERS.contains(&model.provider.as_str()) {
        return;
    }
    // OpenAI rejects `type: "custom"` tools for pre-GPT-5 models.
    let Some(dash_index) = model.id.find("gpt-") else {
        return;
    };
    let rest = &model.id[dash_index + 4..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let Ok(major) = digits.parse::<u32>() else {
        return;
    };
    if major < 5 {
        return;
    }
    merge_compat(model, json!({ "supportsOpenAIGrammarTools": true }));
}

/// Upstream `applyOpenAIToolSearchMetadata`.
fn apply_openai_tool_search_metadata(model: &mut ModelRec) {
    let is_openai_responses = model.provider == "openai" && model.api == "openai-responses";
    let is_openai_codex = model.provider == "openai-codex" && model.api == "openai-codex-responses";
    if !(is_openai_responses || is_openai_codex)
        || !OPENAI_TOOL_SEARCH_MODEL_IDS.contains(&model.id.as_str())
    {
        return;
    }
    let supports_additional_tools = (is_openai_responses)
        || (is_openai_codex
            && OPENAI_CODEX_ADDITIONAL_TOOLS_MODEL_IDS.contains(&model.id.as_str()));
    let compat = json!({
        "supportsAdditionalTools": supports_additional_tools,
        "supportsToolSearch": true,
    });
    // Only keep supportsAdditionalTools when true.
    let compat = if supports_additional_tools {
        compat
    } else {
        json!({ "supportsToolSearch": true })
    };
    merge_compat(model, compat);
}

/// Upstream `applyOpenAIExplicitPromptCacheMetadata`.
fn apply_openai_explicit_prompt_cache_metadata(model: &mut ModelRec) {
    if model.provider != "openai" || model.api != "openai-responses" {
        return;
    }
    if !(model.cost.cache_write > 0.0) {
        return;
    }
    merge_compat(model, json!({ "supportsExplicitPromptCacheMode": true }));
}

/// Upstream `applyThinkingLevelMetadata`.
fn apply_thinking_level_metadata(model: &mut ModelRec) {
    if (model.api == "openai-responses" || model.api == "azure-openai-responses")
        && model.id.starts_with("gpt-5")
    {
        merge_thinking_level_map(model, level_map(&[("off", None)]));
    }
    if model.provider == "github-copilot" && model.id.starts_with("gpt-5") {
        merge_thinking_level_map(model, level_map(&[("minimal", Some("low"))]));
    }
    if model.api == "openai-responses"
        && model.provider == "openai"
        && OPENAI_RESPONSES_NONE_REASONING_MODELS.contains(&model.id.as_str())
    {
        merge_thinking_level_map(model, level_map(&[("off", Some("none"))]));
    }
    if model.provider == "xai"
        && model.api == "openai-responses"
        && model.thinking_level_map.is_none()
    {
        merge_thinking_level_map(model, level_map(&[("off", None), ("minimal", None)]));
    }
    if supports_open_ai_xhigh(&model.id) {
        merge_thinking_level_map(model, level_map(&[("xhigh", Some("xhigh"))]));
    }
    if supports_open_ai_max(model) {
        merge_thinking_level_map(model, level_map(&[("max", Some("max"))]));
    }
    if model.provider == "openai" && model.id == "gpt-5.5" {
        merge_thinking_level_map(model, level_map(&[("minimal", None)]));
    }
    if model.id.ends_with("gpt-5.5-pro") {
        merge_thinking_level_map(
            model,
            level_map(&[("off", None), ("minimal", None), ("low", None)]),
        );
    }
    if model.id.contains("opus-4-6")
        || model.id.contains("opus-4.6")
        || model.id.contains("sonnet-4-6")
        || model_id_contains(&model.id, &["sonnet-4.6"])
    {
        merge_thinking_level_map(model, level_map(&[("max", Some("max"))]));
    }
    if model_id_contains(
        &model.id,
        &[
            "opus-4-7", "opus-4.7", "opus-4-8", "opus-4.8", "opus-5", "opus.5", "sonnet-5",
            "sonnet.5",
        ],
    ) {
        merge_thinking_level_map(
            model,
            level_map(&[("xhigh", Some("xhigh")), ("max", Some("max"))]),
        );
    }
    if model.id.contains("fable-5") {
        merge_thinking_level_map(
            model,
            level_map(&[
                ("off", None),
                ("xhigh", Some("xhigh")),
                ("max", Some("max")),
            ]),
        );
    }
    if model.api == "anthropic-messages" && is_anthropic_adaptive_thinking_model(&model.id) {
        merge_compat(model, json!({ "forceAdaptiveThinking": true }));
    }
    if model.api == "anthropic-messages" && is_anthropic_temperature_unsupported_model(&model.id) {
        merge_compat(model, json!({ "supportsTemperature": false }));
    }
    if model.api == "openai-completions" && model.id.contains("deepseek-v4") {
        let map = if model.provider == "openrouter" {
            level_map(&[
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("high")),
                ("xhigh", Some("xhigh")),
                ("max", None),
            ])
        } else if (model.provider == "deepseek"
            || model.provider == "opencode"
            || model.provider == "opencode-go")
            && model.id.contains("deepseek-v4-flash")
        {
            level_map(&[
                ("minimal", None),
                ("low", Some("low")),
                ("medium", None),
                ("high", Some("high")),
                ("max", None),
            ])
        } else {
            level_map(&[
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("high")),
                ("max", Some("max")),
            ])
        };
        merge_thinking_level_map(model, map);
    }
    if is_google_thinking_api(&model.api) && is_gemini_3_pro_model(&model.id) {
        merge_thinking_level_map(
            model,
            level_map(&[
                ("off", None),
                ("minimal", None),
                ("low", Some("LOW")),
                ("medium", None),
                ("high", Some("HIGH")),
            ]),
        );
    }
    if is_google_thinking_api(&model.api) && is_gemini_3_flash_model(&model.id) {
        merge_thinking_level_map(model, level_map(&[("off", None)]));
    }
    if is_google_thinking_api(&model.api) && is_gemma_4_model(&model.id) {
        merge_thinking_level_map(
            model,
            level_map(&[
                ("off", None),
                ("minimal", Some("MINIMAL")),
                ("low", None),
                ("medium", None),
                ("high", Some("HIGH")),
            ]),
        );
    }
    if model.provider == "groq" && model.id == "qwen/qwen3.6-27b" {
        merge_thinking_level_map(
            model,
            level_map(&[
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("default")),
            ]),
        );
    }
    if model.provider == "openai-codex" && supports_open_ai_xhigh(&model.id) {
        merge_thinking_level_map(model, level_map(&[("minimal", Some("low"))]));
    }
    if (model.provider == "moonshotai" || model.provider == "moonshotai-cn")
        && (model.id == "kimi-k2.7-code" || model.id == "kimi-k2.7-code-highspeed")
    {
        merge_thinking_level_map(model, level_map(&[("off", None)]));
    }
    if model.provider == "openrouter" && model.id.starts_with("inception/mercury-2") {
        merge_thinking_level_map(model, level_map(&[("off", None)]));
    }
    if model.provider == "openrouter" && model.id == "z-ai/glm-5.2" {
        merge_thinking_level_map(model, level_map(&[("xhigh", Some("xhigh"))]));
    }
    if model.provider == "fireworks" && model.id.contains("glm-5p2") {
        merge_thinking_level_map(
            model,
            level_map(&[
                ("off", Some("none")),
                ("minimal", None),
                ("low", Some("high")),
                ("medium", Some("high")),
                ("max", Some("max")),
            ]),
        );
    }
    if model.provider == "opencode-go" && model.id == "glm-5.2" {
        merge_thinking_level_map(
            model,
            level_map(&[
                ("off", None),
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("high")),
                ("max", Some("max")),
            ]),
        );
    }
    if model.provider == "opencode-go" && model.id == "kimi-k2.6" {
        merge_thinking_level_map(
            model,
            level_map(&[("minimal", None), ("low", None), ("medium", None)]),
        );
    }
    if model.provider == "opencode" && model.id == "grok-build-0.1" {
        merge_thinking_level_map(
            model,
            level_map(&[
                ("off", None),
                ("minimal", None),
                ("low", None),
                ("medium", None),
            ]),
        );
    }
    if model.provider == "ant-ling" && model.reasoning {
        merge_thinking_level_map(
            model,
            level_map(&[
                ("off", None),
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("high")),
                ("xhigh", Some("xhigh")),
            ]),
        );
    }
    if model.provider == "github-copilot" {
        let override_map = match model.id.as_str() {
            "claude-opus-4.7" => Some(level_map(&[("minimal", Some("low"))])),
            "claude-opus-4.8" => Some(level_map(&[("minimal", Some("low"))])),
            "claude-opus-5" => Some(level_map(&[("minimal", Some("low"))])),
            "claude-sonnet-4.6" => {
                Some(level_map(&[("minimal", Some("low")), ("max", Some("max"))]))
            }
            _ => None,
        };
        if let Some(map) = override_map {
            merge_thinking_level_map(model, map);
        }
    }
}

fn model_id_contains(model_id: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| model_id.contains(needle))
}

/// Upstream `applyAnthropicAllowedFallbackModelMetadata`.
fn apply_anthropic_allowed_fallback_model_metadata(models: &mut [ModelRec]) {
    for (model_id, fallback_ids) in ANTHROPIC_ALLOWED_FALLBACK_MODELS {
        let mut allowed = Vec::new();
        for fallback_id in fallback_ids {
            if let Some(fallback) = models
                .iter()
                .find(|m| m.id == *fallback_id && m.id != *model_id)
            {
                allowed.push(json!({
                    "provider": fallback.provider,
                    "model": fallback.id,
                    "cost": fallback.cost.to_json(),
                }));
            }
        }
        let Some(model) = models.iter_mut().find(|m| m.id == *model_id) else {
            continue;
        };
        if !allowed.is_empty() {
            merge_compat(model, json!({ "allowedFallbackModels": allowed }));
        }
    }
}

// ---------------------------------------------------------------------------
// Source loaders
// ---------------------------------------------------------------------------

fn http_get_json(url: &str) -> Result<Value, String> {
    let response = reqwest::blocking::Client::builder()
        .user_agent("pi-generate-models")
        .build()
        .map_err(|error| error.to_string())?
        .get(url)
        .send()
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("{url} returned {}", response.status()));
    }
    response
        .text()
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .ok_or_else(|| "invalid JSON".to_string())
}

fn process_zai_models(data: &Value) -> Vec<ModelRec> {
    let variants = [
        (
            "zai-coding-plan",
            "zai",
            "https://api.z.ai/api/coding/paas/v4",
        ),
        (
            "zhipuai-coding-plan",
            "zai-coding-cn",
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ),
    ];
    let mut models = Vec::new();
    for (source, provider, base_url) in variants {
        let Some(entries) = data
            .get(source)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec =
                ModelRec::new(model_id, model_id, "openai-completions", provider, base_url);
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            let reference_cost = data
                .get("zai")
                .and_then(|zai| zai.pointer(&format!("/models/{model_id}/cost")))
                .and_then(Value::as_object)
                .map(|cost| json!(cost))
                .unwrap_or_else(|| json!(model.get("cost").cloned().unwrap_or(Value::Null)));
            let cost = if reference_cost.is_null() {
                Cost::default()
            } else {
                cost_of(&reference_cost)
            };
            rec.cost = Cost {
                cache_write: cost.cache_write,
                ..cost.clone()
            };
            let thinking_level_map = model
                .get("reasoning_options")
                .and_then(Value::as_array)
                .and_then(|options| effort_thinking_level_map(options));
            let is_glm52 = model_id == "glm-5.2" || model_id == "glm-5.2-highspeed";
            let supports_reasoning_effort = thinking_level_map.is_some();
            let mut compat = json!({
                "supportsDeveloperRole": false,
                "thinkingFormat": "zai",
            });
            if supports_reasoning_effort {
                compat["supportsReasoningEffort"] = json!(true);
            }
            if !ZAI_TOOL_STREAM_UNSUPPORTED_MODELS.contains(&model_id.as_str()) {
                compat["zaiToolStream"] = json!(true);
            }
            rec.compat = Some(compat);
            if let Some(map) = thinking_level_map {
                let mut map = map;
                if is_glm52 {
                    map.insert("off".to_string(), Some("none".to_string()));
                }
                rec.thinking_level_map = Some(map);
            }
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            models.push(rec);
        }
    }
    models
}

fn process_baseten_models(provider: Option<&Value>) -> Vec<ModelRec> {
    let Some(provider) = provider else {
        return Vec::new();
    };
    let base_url = "https://api.baseten.co/v1";
    let mut models = Vec::new();
    let Some(entries) = provider.get("models").and_then(Value::as_object) else {
        return models;
    };
    for (model_id, model) in entries {
        if model.get("status").and_then(Value::as_str) == Some("deprecated") {
            continue;
        }
        let reasoning = model
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let reasoning_options = model
            .get("reasoning_options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let is_glm52 = model_id == "zai-org/GLM-5.2" || model_id == "zai-org/GLM-5.2-Fast";
        let supports_toggle = reasoning_options
            .iter()
            .any(|option| option.get("type").and_then(Value::as_str) == Some("toggle"))
            || is_glm52;
        let supports_effort = reasoning_options
            .iter()
            .any(|option| option.get("type").and_then(Value::as_str) == Some("effort"))
            || is_glm52;
        let mut compat = json!({
            "supportsStore": false,
            "supportsDeveloperRole": false,
            "supportsUsageInStreaming": true,
            "maxTokensField": "max_tokens",
            "supportsStrictMode": true,
            "supportsLongCacheRetention": false,
        });
        if supports_toggle && supports_effort {
            compat["supportsReasoningEffort"] = json!(true);
            compat["thinkingFormat"] = json!("baseten");
            compat["chatTemplateArgs"] =
                json!({ "enable_thinking": { "$var": "thinking.enabled" } });
        } else if supports_toggle {
            compat["thinkingFormat"] = json!("baseten");
            compat["chatTemplateArgs"] =
                json!({ "enable_thinking": { "$var": "thinking.enabled" } });
        } else if supports_effort {
            compat["supportsReasoningEffort"] = json!(true);
            compat["thinkingFormat"] = json!("openai");
        }
        let thinking_level_map = if is_glm52 {
            Some(level_map(&[
                ("off", Some("none")),
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("high")),
                ("xhigh", None),
                ("max", Some("max")),
            ]))
        } else if supports_toggle {
            Some(level_map(&[
                ("off", Some("off")),
                ("minimal", None),
                ("low", None),
                ("medium", None),
                ("high", Some("high")),
                ("xhigh", None),
                ("max", None),
            ]))
        } else {
            effort_thinking_level_map(&reasoning_options)
        };

        let mut rec = ModelRec::new(
            model_id,
            model_id,
            "openai-completions",
            "baseten",
            base_url,
        );
        rec.reasoning = reasoning;
        if let Some(map) = thinking_level_map {
            rec.thinking_level_map = Some(map);
        }
        rec.input = input_of(model);
        rec.cost = cost_of(model);
        rec.compat = Some(compat);
        rec.context_window = model
            .pointer("/limit/context")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        rec.max_tokens = model
            .pointer("/limit/output")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        models.push(rec);
    }
    models
}

fn process_fireworks_models(provider: Option<&Value>) -> Vec<ModelRec> {
    let Some(provider) = provider else {
        return Vec::new();
    };
    let mut models = Vec::new();
    let Some(entries) = provider.get("models").and_then(Value::as_object) else {
        return models;
    };
    let anthropic_compat = json!({
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false,
        "thinkingFormat": "string-thinking",
    });
    let kimi_k3_compat = json!({
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": true,
        "thinkingFormat": "openai",
        "supportsLongCacheRetention": false,
    });
    let openai_compat = json!({
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": true,
        "thinkingFormat": "openai",
    });
    for (model_id, model) in entries {
        if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        if model.get("status").and_then(Value::as_str) == Some("deprecated") {
            continue;
        }
        let mut rec = ModelRec::new(
            model_id,
            model_id,
            "openai-completions",
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
        );
        rec.reasoning = model
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        rec.input = input_of(model);
        rec.cost = cost_of(model);
        rec.context_window = model
            .pointer("/limit/context")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        rec.max_tokens = model
            .pointer("/limit/output")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        if model_id.contains("kimi-k3") {
            rec.compat = Some(kimi_k3_compat.clone());
        } else if model_id.contains("kimi-k2")
            || model_id.contains("glm")
            || model_id.contains("deepseek")
            || model_id.contains("qwen")
        {
            rec.compat = Some(openai_compat.clone());
        } else {
            rec.api = "anthropic-messages".to_string();
            rec.base_url = "https://api.fireworks.ai/inference".to_string();
            rec.compat = Some(anthropic_compat.clone());
        }
        models.push(rec);
    }
    models
}

fn load_models_dev_data() -> Result<Vec<ModelRec>, String> {
    println!("Fetching models from models.dev API...");
    let data = http_get_json("https://models.dev/api.json")?;
    let mut models: Vec<ModelRec> = Vec::new();

    let mut record = BTreeMap::new();
    let mut record_options = |provider: &str, id: &str, model: &Value| {
        if let Some(options) = model.get("reasoning_options").and_then(Value::as_array) {
            record.insert(format!("{provider}:{id}"), options.clone());
        }
    };

    // Amazon Bedrock
    if let Some(entries) = data
        .get("amazon-bedrock")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if BEDROCK_INFERENCE_PROFILE_ONLY_MODEL_IDS.contains(&model_id.as_str()) {
                continue;
            }
            if model_id.starts_with("ai21.jamba")
                || model_id.starts_with("mistral.mistral-7b-instruct-v0")
            {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "bedrock-converse-stream",
                "amazon-bedrock",
                get_bedrock_base_url(model_id),
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("amazon-bedrock", model_id, model);
            models.push(rec);
        }
    }

    // Anthropic
    if let Some(entries) = data
        .get("anthropic")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "anthropic-messages",
                "anthropic",
                "https://api.anthropic.com",
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("anthropic", model_id, model);
            models.push(rec);
        }
    }

    // Google
    if let Some(entries) = data
        .get("google")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let source = match model_id.as_str() {
                "gemini-flash-latest" => data
                    .pointer("/google/models/gemini-3.5-flash")
                    .unwrap_or(model),
                "gemini-flash-lite-latest" => data
                    .pointer("/google/models/gemini-3.1-flash-lite")
                    .unwrap_or(model),
                _ => model,
            };
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "google-generative-ai",
                "google",
                "https://generativelanguage.googleapis.com/v1beta",
            );
            rec.reasoning = source
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(source);
            rec.cost = cost_of(source);
            rec.context_window = source
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = source
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("google", model_id, source);
            models.push(rec);
        }
    }

    // Google Vertex
    if let Some(entries) = data
        .get("google-vertex")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let source = match model_id.as_str() {
                "gemini-flash-latest" => data
                    .pointer("/google-vertex/models/gemini-3.5-flash")
                    .unwrap_or(model),
                _ => model,
            };
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "google-vertex",
                "google-vertex",
                "https://aiplatform.googleapis.com/v1",
            );
            rec.reasoning = source
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(source);
            rec.cost = cost_of(source);
            rec.context_window = source
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = source
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("google-vertex", model_id, source);
            models.push(rec);
        }
    }

    // OpenAI
    if let Some(entries) = data
        .get("openai")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if MODELS_DEV_OPENAI_UNSUPPORTED_MODEL_IDS.contains(&model_id.as_str()) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-responses",
                "openai",
                "https://api.openai.com/v1",
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("openai", model_id, model);
            models.push(rec);
        }
    }

    // Groq
    if let Some(entries) = data
        .get("groq")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-completions",
                "groq",
                "https://api.groq.com/openai/v1",
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("groq", model_id, model);
            models.push(rec);
        }
    }

    // Cerebras
    if let Some(entries) = data
        .get("cerebras")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-completions",
                "cerebras",
                "https://api.cerebras.ai/v1",
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("cerebras", model_id, model);
            models.push(rec);
        }
    }

    // Cloudflare Workers AI
    if let Some(entries) = data
        .get("cloudflare-workers-ai")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-completions",
                "cloudflare-workers-ai",
                CLOUDFLARE_WORKERS_AI_BASE_URL,
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("cloudflare-workers-ai", model_id, model);
            models.push(rec);
        }
    }

    // Cloudflare AI Gateway (passthrough prefixes)
    if let Some(entries) = data
        .get("cloudflare-ai-gateway")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (prefixed_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let Some((upstream, native_id)) = prefixed_id.split_once('/') else {
                continue;
            };
            let (api, base_url, id) = match upstream {
                "openai" => (
                    "openai-responses",
                    CLOUDFLARE_AI_GATEWAY_OPENAI_BASE_URL,
                    native_id.to_string(),
                ),
                "anthropic" => (
                    "anthropic-messages",
                    CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL,
                    native_id.to_string(),
                ),
                "workers-ai" => (
                    "openai-completions",
                    CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL,
                    prefixed_id.clone(),
                ),
                _ => continue,
            };
            let mut rec = ModelRec::new(
                &id,
                model.get("name").and_then(Value::as_str).unwrap_or(&id),
                api,
                "cloudflare-ai-gateway",
                base_url,
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            if upstream == "anthropic" || upstream == "workers-ai" {
                rec.compat = Some(json!({ "sendSessionAffinityHeaders": true }));
            }
            record_options("cloudflare-ai-gateway", &id, model);
            models.push(rec);
        }
        // Mirror Workers AI passthroughs under the workers-ai/ prefix.
        if let Some(worker_entries) = data
            .get("cloudflare-workers-ai")
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        {
            for (model_id, model) in worker_entries {
                if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                let id = format!("workers-ai/{model_id}");
                if models
                    .iter()
                    .any(|m| m.provider == "cloudflare-ai-gateway" && m.id == id)
                {
                    continue;
                }
                let mut rec = ModelRec::new(
                    &id,
                    model.get("name").and_then(Value::as_str).unwrap_or(&id),
                    "openai-completions",
                    "cloudflare-ai-gateway",
                    CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL,
                );
                rec.reasoning = model
                    .get("reasoning")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                rec.input = input_of(model);
                rec.cost = cost_of(model);
                rec.context_window = model
                    .pointer("/limit/context")
                    .and_then(Value::as_u64)
                    .unwrap_or(4096);
                rec.max_tokens = model
                    .pointer("/limit/output")
                    .and_then(Value::as_u64)
                    .unwrap_or(4096);
                rec.compat = Some(json!({ "sendSessionAffinityHeaders": true }));
                models.push(rec);
            }
        }
    }

    // xAI
    if let Some(entries) = data
        .get("xai")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-responses",
                "xai",
                "https://api.x.ai/v1",
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.compat = Some(json!({ "supportsLongCacheRetention": false }));
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("xai", model_id, model);
            models.push(rec);
        }
    }

    models.extend(process_zai_models(&data));

    // Mistral
    if let Some(entries) = data
        .get("mistral")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "mistral-conversations",
                "mistral",
                "https://api.mistral.ai",
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            let mut cost = cost_of(model);
            if cost.cache_read == 0.0 {
                let input = model
                    .pointer("/cost/input")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                if input != 0.0 {
                    cost.cache_read = round_cost(input * 0.1);
                }
            }
            rec.cost = cost;
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("mistral", model_id, model);
            models.push(rec);
        }
    }

    // Hugging Face
    if let Some(entries) = data
        .get("huggingface")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-completions",
                "huggingface",
                "https://router.huggingface.co/v1",
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.compat = Some(json!({ "supportsDeveloperRole": false }));
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("huggingface", model_id, model);
            models.push(rec);
        }
    }

    models.extend(process_fireworks_models(data.get("fireworks-ai")));

    // NVIDIA NIM
    if let Some(entries) = data
        .get("nvidia")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let text_in = model
                .pointer("/modalities/input")
                .and_then(Value::as_array)
                .map(|items| items.iter().any(|item| item.as_str() == Some("text")))
                .unwrap_or(false);
            let text_out = model
                .pointer("/modalities/output")
                .and_then(Value::as_array)
                .map(|items| items.iter().any(|item| item.as_str() == Some("text")))
                .unwrap_or(false);
            if !text_in || !text_out {
                continue;
            }
            let normalized = normalize_nvidia_model_id(model_id);
            let live_model_id = model_id.clone();
            if NVIDIA_NIM_UNSUPPORTED_MODELS.contains(&live_model_id.as_str())
                || NVIDIA_NIM_UNSUPPORTED_MODELS.contains(&normalized.as_str())
            {
                continue;
            }
            let mut rec = ModelRec::new(
                &live_model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&live_model_id),
                "openai-completions",
                "nvidia",
                NVIDIA_BASE_URL,
            );
            rec.headers = Some(
                NVIDIA_HEADERS
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.compat = Some(json!({
                "supportsStore": false,
                "supportsDeveloperRole": false,
                "supportsReasoningEffort": false,
                "maxTokensField": "max_tokens",
                "supportsStrictMode": false,
                "supportsLongCacheRetention": false,
            }));
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("nvidia", &live_model_id, model);
            models.push(rec);
        }
    }

    // Together AI
    let together_provider = data
        .get("together")
        .or_else(|| data.get("togetherai"))
        .or_else(|| data.get("together-ai"));
    if let Some(entries) = together_provider
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if model.get("status").and_then(Value::as_str) == Some("deprecated") {
                continue;
            }
            let reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let thinking_level_map = if !reasoning {
                None
            } else if TOGETHER_REASONING_EFFORT_MODELS.contains(&model_id.as_str()) {
                Some(level_map(&[
                    ("off", None),
                    ("minimal", None),
                    ("low", Some("low")),
                    ("medium", Some("medium")),
                    ("high", Some("high")),
                ]))
            } else if TOGETHER_TOGGLE_REASONING_EFFORT_MODELS.contains(&model_id.as_str()) {
                Some(level_map(&[
                    ("minimal", None),
                    ("low", None),
                    ("medium", None),
                    ("high", Some("high")),
                    ("xhigh", None),
                ]))
            } else if TOGETHER_REASONING_ONLY_MODELS.contains(&model_id.as_str()) {
                Some(level_map(&[
                    ("off", None),
                    ("minimal", None),
                    ("low", None),
                    ("medium", None),
                ]))
            } else {
                Some(level_map(&[
                    ("minimal", None),
                    ("low", None),
                    ("medium", None),
                ]))
            };
            let mut compat = json!({
                "supportsStore": false,
                "supportsDeveloperRole": false,
                "supportsReasoningEffort": false,
                "maxTokensField": "max_tokens",
                "supportsStrictMode": false,
                "supportsLongCacheRetention": false,
            });
            if !reasoning {
                // base compat
            } else if TOGETHER_REASONING_EFFORT_MODELS.contains(&model_id.as_str()) {
                compat["supportsReasoningEffort"] = json!(true);
                compat["thinkingFormat"] = json!("openai");
            } else if TOGETHER_TOGGLE_REASONING_EFFORT_MODELS.contains(&model_id.as_str()) {
                compat["supportsReasoningEffort"] = json!(true);
                compat["thinkingFormat"] = json!("together");
            } else if !TOGETHER_REASONING_ONLY_MODELS.contains(&model_id.as_str()) {
                compat["thinkingFormat"] = json!("together");
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-completions",
                "together",
                TOGETHER_BASE_URL,
            );
            rec.reasoning = reasoning;
            if let Some(map) = thinking_level_map {
                rec.thinking_level_map = Some(map);
            }
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.compat = Some(compat);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("together", model_id, model);
            models.push(rec);
        }
    }

    models.extend(process_baseten_models(data.get("baseten")));

    // OpenCode (Zen and Go)
    let opencode_variants = [
        ("opencode", "opencode", "https://opencode.ai/zen"),
        ("opencode-go", "opencode-go", "https://opencode.ai/zen/go"),
    ];
    for (key, provider, base_path) in opencode_variants {
        let Some(entries) = data
            .get(key)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if model.get("status").and_then(Value::as_str) == Some("deprecated") {
                continue;
            }
            let npm = model
                .pointer("/provider/npm")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut api = "openai-completions".to_string();
            let mut base_url = format!("{base_path}/v1");
            let mut compat: Option<Value> = None;
            match npm {
                "@ai-sdk/openai" => {
                    api = "openai-responses".to_string();
                    compat = Some(json!({ "sessionAffinityFormat": "openai-nosession" }));
                }
                "@ai-sdk/anthropic" => {
                    api = "anthropic-messages".to_string();
                    base_url = base_path.to_string();
                }
                "@ai-sdk/google" => {
                    api = "google-generative-ai".to_string();
                }
                "@ai-sdk/alibaba" => {
                    compat = Some(json!({ "cacheControlFormat": "anthropic" }));
                }
                _ => {}
            }
            if provider == "opencode" && model_id == "grok-build-0.1" {
                let mut object = compat.take().unwrap_or_else(|| json!({}));
                object["supportsReasoningEffort"] = json!(false);
                compat = Some(object);
            }
            if (provider == "opencode" || provider == "opencode-go") && model_id == "kimi-k2.6" {
                let mut object = compat.take().unwrap_or_else(|| json!({}));
                object["thinkingFormat"] = json!("deepseek");
                object["supportsReasoningEffort"] = json!(false);
                compat = Some(object);
            }
            if provider == "opencode-go" {
                if model_id == "minimax-m2.7" {
                    api = "openai-completions".to_string();
                    base_url = format!("{base_path}/v1");
                }
                if model_id == "qwen3.5-plus" || model_id == "qwen3.6-plus" {
                    api = "openai-completions".to_string();
                    base_url = format!("{base_path}/v1");
                    let mut object = compat.take().unwrap_or_else(|| json!({}));
                    object["thinkingFormat"] = json!("qwen");
                    compat = Some(object);
                }
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                &api,
                provider,
                &base_url,
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.compat = compat;
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            if api == "openai-completions"
                && OPENCODE_OPENAI_COMPLETIONS_LONG_CACHE_RETENTION_UNSUPPORTED
                    .contains(&format!("{provider}:{model_id}").as_str())
            {
                let mut object = rec.compat.take().unwrap_or_else(|| json!({}));
                object["supportsLongCacheRetention"] = json!(false);
                rec.compat = Some(object);
            }
            record_options(provider, model_id, model);
            models.push(rec);
        }
    }

    // GitHub Copilot
    if let Some(entries) = data
        .get("github-copilot")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if model.get("status").and_then(Value::as_str) == Some("deprecated") {
                continue;
            }
            let is_copilot_claude = model_id.starts_with("claude-haiku-4")
                || model_id.starts_with("claude-sonnet-4")
                || model_id.starts_with("claude-opus-4")
                || model_id.starts_with("claude-haiku-5")
                || model_id.starts_with("claude-sonnet-5")
                || model_id.starts_with("claude-opus-5");
            let needs_responses_api = model_id.starts_with("grok-")
                || model_id.starts_with("gpt-5")
                || model_id.starts_with("oswe")
                || model_id.starts_with("mai-");
            let api = if is_copilot_claude {
                "anthropic-messages"
            } else if needs_responses_api {
                "openai-responses"
            } else {
                "openai-completions"
            };
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                api,
                "github-copilot",
                "https://api.individual.githubcopilot.com",
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = get_models_dev_cost(model.get("cost").unwrap_or(&Value::Null));
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(128_000);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(8192);
            rec.headers = Some(
                COPILOT_STATIC_HEADERS
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
            );
            if let Some(compat) = get_anthropic_messages_compat("github-copilot", model_id) {
                if api == "anthropic-messages" {
                    rec.compat = Some(compat);
                }
            }
            if api == "openai-completions" {
                rec.compat = Some(json!({
                    "supportsStore": false,
                    "supportsDeveloperRole": false,
                    "supportsReasoningEffort": false,
                }));
            }
            record_options("github-copilot", model_id, model);
            models.push(rec);
        }
    }

    // MiniMax (Anthropic-compatible)
    let minimax_variants = [
        ("minimax", "minimax", "https://api.minimax.io/anthropic"),
        (
            "minimax-cn",
            "minimax-cn",
            "https://api.minimaxi.com/anthropic",
        ),
    ];
    for (key, provider, base_url) in minimax_variants {
        let Some(entries) = data
            .get(key)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "anthropic-messages",
                provider,
                base_url,
            );
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options(provider, model_id, model);
            models.push(rec);
        }
    }

    // Kimi For Coding
    if let Some(entries) = data
        .get("kimi-for-coding")
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
    {
        let has_canonical = entries.contains_key("kimi-for-coding");
        let kimi_aliases = ["k2p5", "k2p6", "k2p7"];
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if kimi_aliases.contains(&model_id.as_str()) && has_canonical {
                continue;
            }
            let normalized_id = if kimi_aliases.contains(&model_id.as_str()) {
                "kimi-for-coding"
            } else {
                model_id.as_str()
            };
            let normalized_name = if kimi_aliases.contains(&model_id.as_str()) {
                "Kimi For Coding"
            } else {
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(normalized_id)
            };
            let is_kimi_k3 = normalized_id == "k3";
            let allow_empty_signature = is_kimi_k3 || normalized_id == "kimi-for-coding";
            let implied = KIMI_CODING_IMPLIED_COSTS
                .iter()
                .find(|(id, _)| *id == normalized_id)
                .map(|(_, cost)| cost_from_entries(*cost));
            let mut compat = json!({ "forceAdaptiveThinking": true });
            if allow_empty_signature {
                compat["allowEmptySignature"] = json!(true);
            }
            let mut rec = ModelRec::new(
                normalized_id,
                normalized_name,
                "anthropic-messages",
                "kimi-coding",
                "https://api.kimi.com/coding",
            );
            rec.compat = Some(compat);
            rec.reasoning = is_kimi_k3
                || model
                    .get("reasoning")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            rec.input = input_of(model);
            let mut cost = cost_of(model);
            if let Some(implied) = implied {
                if cost.input == 0.0 {
                    cost.input = implied.input;
                }
                if cost.output == 0.0 {
                    cost.output = implied.output;
                }
                if cost.cache_read == 0.0 {
                    cost.cache_read = implied.cache_read;
                }
                if cost.cache_write == 0.0 {
                    cost.cache_write = implied.cache_write;
                }
            }
            rec.cost = cost;
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options("kimi-coding", normalized_id, model);
            models.push(rec);
        }
    }

    // Moonshot AI
    let moonshot_variants = [
        ("moonshotai", "moonshotai", "https://api.moonshot.ai/v1"),
        (
            "moonshotai-cn",
            "moonshotai-cn",
            "https://api.moonshot.cn/v1",
        ),
    ];
    for (key, provider, base_url) in moonshot_variants {
        let Some(entries) = data
            .get(key)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let is_kimi_k3 = model_id == "kimi-k3";
            let mut compat = json!({
                "supportsStore": false,
                "supportsDeveloperRole": false,
                "supportsReasoningEffort": false,
                "maxTokensField": "max_tokens",
                "supportsStrictMode": false,
                "thinkingFormat": "deepseek",
            });
            if is_kimi_k3 {
                compat["requiresReasoningContentOnAssistantMessages"] = json!(true);
                compat["deferredToolsMode"] = json!("kimi");
                compat["thinkingFormat"] = json!("openai");
                compat["supportsReasoningEffort"] = json!(true);
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-completions",
                provider,
                base_url,
            );
            rec.reasoning = is_kimi_k3
                || model
                    .get("reasoning")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            rec.input = input_of(model);
            let mut cost = cost_of(model);
            if is_kimi_k3 {
                let implied = cost_from_entries(KIMI_K3_COST);
                if cost.input == 0.0 {
                    cost.input = implied.input;
                }
                if cost.output == 0.0 {
                    cost.output = implied.output;
                }
                if cost.cache_read == 0.0 {
                    cost.cache_read = implied.cache_read;
                }
                if cost.cache_write == 0.0 {
                    cost.cache_write = implied.cache_write;
                }
            }
            rec.cost = cost;
            rec.compat = Some(compat);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options(provider, model_id, model);
            models.push(rec);
        }
    }

    // Xiaomi MiMo
    let xiaomi_compat = json!({
        "requiresReasoningContentOnAssistantMessages": true,
        "thinkingFormat": "deepseek",
    });
    let xiaomi_variants = [
        ("xiaomi", "xiaomi", "https://api.xiaomimimo.com/v1"),
        (
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-cn",
            "https://token-plan-cn.xiaomimimo.com/v1",
        ),
        (
            "xiaomi-token-plan-ams",
            "xiaomi-token-plan-ams",
            "https://token-plan-ams.xiaomimimo.com/v1",
        ),
        (
            "xiaomi-token-plan-sgp",
            "xiaomi-token-plan-sgp",
            "https://token-plan-sgp.xiaomimimo.com/v1",
        ),
    ];
    for (source, provider, base_url) in xiaomi_variants {
        let Some(entries) = data
            .get(source)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if model.get("status").and_then(Value::as_str) == Some("deprecated") {
                continue;
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-completions",
                provider,
                base_url,
            );
            rec.compat = Some(xiaomi_compat.clone());
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options(provider, model_id, model);
            models.push(rec);
        }
    }

    // Alibaba Cloud Model Studio Token Plan (qwen-token-plan[-cn|individual])
    let qwen_token_plan_compat = json!({
        "thinkingFormat": "qwen",
        "supportsDeveloperRole": false,
        "supportsStore": false,
        "supportsReasoningEffort": true,
    });
    let qwen_variants: [(&str, &str, &str, Option<&[&str]>); 3] = [
        (
            "alibaba-token-plan",
            "qwen-token-plan",
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            None,
        ),
        (
            "alibaba-token-plan",
            "qwen-token-plan-individual",
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            Some(&QWEN_TOKEN_PLAN_INDIVIDUAL_MODEL_IDS),
        ),
        (
            "alibaba-token-plan-cn",
            "qwen-token-plan-cn",
            "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
            None,
        ),
    ];
    for (source, provider, base_url, model_ids) in qwen_variants {
        let Some(entries) = data
            .get(source)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (model_id, model) in entries {
            if model.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if QWEN_TOKEN_PLAN_EXCLUDED_MODEL_IDS.contains(&model_id.as_str()) {
                continue;
            }
            if let Some(model_ids) = model_ids {
                if !model_ids.contains(&model_id.as_str()) {
                    continue;
                }
            }
            let supports_effort =
                !QWEN_TOKEN_PLAN_REASONING_EFFORT_UNSUPPORTED.contains(&model_id.as_str());
            let mut compat = qwen_token_plan_compat.clone();
            if !supports_effort {
                compat["supportsReasoningEffort"] = json!(false);
            }
            let mut rec = ModelRec::new(
                model_id,
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id),
                "openai-completions",
                provider,
                base_url,
            );
            rec.compat = Some(compat);
            if supports_effort {
                rec.thinking_level_map = Some(if model_id == "qwen3.8-max" {
                    level_map(&[
                        ("minimal", None),
                        ("low", Some("low")),
                        ("medium", Some("medium")),
                        ("high", None),
                        ("xhigh", Some("xhigh")),
                        ("max", None),
                    ])
                } else {
                    level_map(&[
                        ("minimal", None),
                        ("low", None),
                        ("medium", None),
                        ("high", Some("high")),
                        ("xhigh", None),
                        ("max", Some("max")),
                    ])
                });
            }
            rec.reasoning = model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rec.input = input_of(model);
            rec.cost = cost_of(model);
            rec.context_window = model
                .pointer("/limit/context")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            rec.max_tokens = model
                .pointer("/limit/output")
                .and_then(Value::as_u64)
                .unwrap_or(4096);
            record_options(provider, model_id, model);
            models.push(rec);
        }
    }

    Ok(models)
}

fn fetch_open_router_models() -> Result<Vec<ModelRec>, String> {
    println!("Fetching models from OpenRouter API...");
    let data = http_get_json("https://openrouter.ai/api/v1/models")?;
    let mut models = Vec::new();
    let Some(items) = data.get("data").and_then(Value::as_array) else {
        return Ok(models);
    };
    for model in items {
        let supported_parameters: Vec<String> = model
            .get("supported_parameters")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !supported_parameters
            .iter()
            .any(|parameter| parameter == "tools")
        {
            continue;
        }
        let input = if model
            .pointer("/architecture/modality")
            .and_then(Value::as_str)
            .map(|modality| modality.contains("image"))
            .unwrap_or(false)
        {
            vec!["text".to_string(), "image".to_string()]
        } else {
            vec!["text".to_string()]
        };
        let parse_cost = |pointer: &str| -> f64 {
            model
                .pointer(pointer)
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(0.0)
                * 1_000_000.0
        };
        let thinking_level_map = openrouter_thinking_level_map(model.get("reasoning"));
        let mut rec = ModelRec::new(
            model.get("id").and_then(Value::as_str).unwrap_or(""),
            model.get("name").and_then(Value::as_str).unwrap_or(""),
            "openai-completions",
            "openrouter",
            "https://openrouter.ai/api/v1",
        );
        rec.reasoning = supported_parameters
            .iter()
            .any(|parameter| parameter == "reasoning");
        if let Some(map) = thinking_level_map {
            rec.thinking_level_map = Some(map);
        }
        rec.input = input;
        rec.cost = Cost {
            input: round_cost(parse_cost("/pricing/prompt")),
            output: round_cost(parse_cost("/pricing/completion")),
            cache_read: round_cost(parse_cost("/pricing/input_cache_read")),
            cache_write: round_cost(parse_cost("/pricing/input_cache_write")),
            tiers: None,
        };
        rec.context_window = model
            .pointer("/top_provider/context_length")
            .and_then(Value::as_u64)
            .or_else(|| model.get("context_length").and_then(Value::as_u64))
            .unwrap_or(4096);
        rec.max_tokens = model
            .pointer("/top_provider/max_completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        models.push(rec);
    }
    Ok(models)
}

fn fetch_ai_gateway_models() -> Result<Vec<ModelRec>, String> {
    println!("Fetching models from Vercel AI Gateway API...");
    let data = http_get_json(&format!("{AI_GATEWAY_MODELS_URL}/models"))?;
    let mut models = Vec::new();
    let Some(items) = data.get("data").and_then(Value::as_array) else {
        return Ok(models);
    };
    for model in items {
        let tags: Vec<String> = model
            .get("tags")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !tags.iter().any(|tag| tag == "tool-use") {
            continue;
        }
        let to_number = |pointer: &str| -> f64 {
            model
                .pointer(pointer)
                .map(|value| {
                    value
                        .as_f64()
                        .or_else(|| value.as_str().and_then(|value| value.parse::<f64>().ok()))
                        .unwrap_or(0.0)
                })
                .unwrap_or(0.0)
        };
        let input = if tags.iter().any(|tag| tag == "vision") {
            vec!["text".to_string(), "image".to_string()]
        } else {
            vec!["text".to_string()]
        };
        let mut rec = ModelRec::new(
            model.get("id").and_then(Value::as_str).unwrap_or(""),
            model
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(model.get("id").and_then(Value::as_str).unwrap_or("")),
            "anthropic-messages",
            "vercel-ai-gateway",
            AI_GATEWAY_BASE_URL,
        );
        rec.reasoning = tags.iter().any(|tag| tag == "reasoning");
        rec.input = input;
        rec.cost = Cost {
            input: round_cost(to_number("/pricing/input") * 1_000_000.0),
            output: round_cost(to_number("/pricing/output") * 1_000_000.0),
            cache_read: round_cost(to_number("/pricing/input_cache_read") * 1_000_000.0),
            cache_write: round_cost(to_number("/pricing/input_cache_write") * 1_000_000.0),
            tiers: None,
        };
        rec.context_window = model
            .get("context_window")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        rec.max_tokens = model
            .get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        models.push(rec);
    }
    Ok(models)
}

// ---------------------------------------------------------------------------
// Post-processing + emit
// ---------------------------------------------------------------------------

fn static_openai_models() -> Vec<ModelRec> {
    // Upstream "Add missing gpt models".
    let mut models = Vec::new();
    let entries: [(&str, &str, Option<&[(&str, f64); 4]>, [(&str, f64); 4]); 4] = [
        (
            "gpt-5.6-sol",
            "GPT-5.6 Sol",
            Some(&[
                ("input", 5.0),
                ("output", 30.0),
                ("cacheRead", 0.5),
                ("cacheWrite", 6.25),
            ]),
            [("input", 0.0); 4],
        ),
        (
            "gpt-5.6-terra",
            "GPT-5.6 Terra",
            Some(&[
                ("input", 2.0),
                ("output", 12.0),
                ("cacheRead", 0.2),
                ("cacheWrite", 2.5),
            ]),
            [("input", 0.0); 4],
        ),
        (
            "gpt-5.6-luna",
            "GPT-5.6 Luna",
            Some(&[
                ("input", 0.2),
                ("output", 1.2),
                ("cacheRead", 0.02),
                ("cacheWrite", 0.25),
            ]),
            [("input", 0.0); 4],
        ),
        (
            "gpt-5-chat-latest",
            "GPT-5 Chat Latest",
            None,
            [
                ("input", 1.25),
                ("output", 10.0),
                ("cacheRead", 0.125),
                ("cacheWrite", 0.0),
            ],
        ),
    ];
    for (id, name, long_context_cost, standard_cost) in entries {
        let mut rec = ModelRec::new(
            id,
            name,
            "openai-responses",
            "openai",
            "https://api.openai.com/v1",
        );
        rec.reasoning = id != "gpt-5-chat-latest";
        rec.input = vec!["text".to_string(), "image".to_string()];
        rec.cost = match long_context_cost {
            Some(cost) => with_open_ai_long_context_pricing(cost_from_entries(*cost)),
            None => cost_from_entries(standard_cost),
        };
        rec.context_window = if id == "gpt-5-chat-latest" {
            128_000
        } else {
            OPENAI_LONG_CONTEXT_INPUT_THRESHOLD
        };
        rec.max_tokens = if id == "gpt-5-chat-latest" {
            16_384
        } else {
            128_000
        };
        models.push(rec);
    }
    models
}

fn static_deepseek_models() -> Vec<ModelRec> {
    let deepseek_compat = json!({
        "requiresReasoningContentOnAssistantMessages": true,
        "thinkingFormat": "deepseek",
    });
    let entries: [(&str, &str, bool, [(&str, f64); 4]); 3] = [
        (
            "deepseek-v4-flash",
            "DeepSeek V4 Flash",
            false,
            [
                ("input", 0.14),
                ("output", 0.28),
                ("cacheRead", 0.0028),
                ("cacheWrite", 0.0),
            ],
        ),
        (
            "deepseek-v4-flash-vision-exp",
            "DeepSeek V4 Flash Vision Exp",
            true,
            [
                ("input", 0.14),
                ("output", 0.28),
                ("cacheRead", 0.0028),
                ("cacheWrite", 0.0),
            ],
        ),
        (
            "deepseek-v4-pro",
            "DeepSeek V4 Pro",
            false,
            [
                ("input", 0.435),
                ("output", 0.87),
                ("cacheRead", 0.003625),
                ("cacheWrite", 0.0),
            ],
        ),
    ];
    entries
        .iter()
        .map(|(id, name, vision, cost)| {
            let mut rec = ModelRec::new(
                id,
                name,
                "openai-completions",
                "deepseek",
                "https://api.deepseek.com",
            );
            rec.reasoning = true;
            rec.input = if *vision {
                vec!["text".to_string(), "image".to_string()]
            } else {
                vec!["text".to_string()]
            };
            rec.cost = cost_from_entries(*cost);
            rec.context_window = 1_000_000;
            rec.max_tokens = 384_000;
            rec.compat = Some(deepseek_compat.clone());
            let _ = &model_id_contains(id, &[]);
            rec
        })
        .collect()
}

fn static_ant_ling_models() -> Vec<ModelRec> {
    let ant_ling_compat = json!({
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false,
        "maxTokensField": "max_tokens",
        "supportsLongCacheRetention": false,
    });
    let entries: [(&str, &str, bool, bool); 3] = [
        ("Ling-2.6-flash", "Ling 2.6 Flash", false, false),
        ("Ling-2.6-1T", "Ling 2.6 1T", false, false),
        ("Ring-2.6-1T", "Ring 2.6 1T", true, true),
    ];
    entries
        .iter()
        .map(|(id, name, reasoning, ring)| {
            let mut rec = ModelRec::new(
                id,
                name,
                "openai-completions",
                "ant-ling",
                "https://api.ant-ling.com/v1",
            );
            rec.reasoning = *reasoning;
            rec.cost = Cost {
                input: 0.06,
                output: 0.25,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: None,
            };
            if id.contains("flash") {
                rec.cost = Cost {
                    input: 0.01,
                    output: 0.02,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    tiers: None,
                };
            }
            rec.context_window = 262_144;
            rec.max_tokens = 65_536;
            let mut compat = ant_ling_compat.clone();
            if *ring {
                compat["thinkingFormat"] = json!("ant-ling");
            }
            rec.compat = Some(compat);
            rec
        })
        .collect()
}

fn static_codex_models() -> Vec<ModelRec> {
    const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
    const CODEX_CONTEXT: u64 = 272_000;
    const CODEX_SPARK_CONTEXT: u64 = 128_000;
    const CODEX_MAX_TOKENS: u64 = 128_000;
    let entries: [(&str, &str, bool, bool, f64, f64, f64, u64); 8] = [
        (
            "gpt-5.3-codex-spark",
            "GPT-5.3 Codex Spark",
            false,
            true,
            1.75,
            14.0,
            0.175,
            CODEX_SPARK_CONTEXT,
        ),
        (
            "gpt-5.4",
            "GPT-5.4",
            true,
            true,
            2.5,
            15.0,
            0.25,
            CODEX_CONTEXT,
        ),
        (
            "gpt-5.4-mini",
            "GPT-5.4 mini",
            false,
            true,
            0.75,
            4.5,
            0.075,
            CODEX_CONTEXT,
        ),
        (
            "gpt-5.5",
            "GPT-5.5",
            true,
            true,
            5.0,
            30.0,
            0.5,
            CODEX_CONTEXT,
        ),
        (
            "gpt-5.6-luna",
            "GPT-5.6 Luna",
            true,
            true,
            0.2,
            1.2,
            0.02,
            CODEX_CONTEXT,
        ),
        (
            "gpt-5.6-terra",
            "GPT-5.6 Terra",
            true,
            true,
            2.0,
            12.0,
            0.2,
            CODEX_CONTEXT,
        ),
        (
            "gpt-5.6-sol",
            "GPT-5.6 Sol",
            true,
            true,
            5.0,
            30.0,
            0.5,
            CODEX_CONTEXT,
        ),
        (
            "gpt-5.4-pro",
            "GPT-5.4 Pro",
            true,
            true,
            15.0,
            90.0,
            1.5,
            CODEX_CONTEXT,
        ),
    ];
    entries
        .iter()
        .map(
            |(id, name, long_context, image, input, output, cache_read, context_window)| {
                let mut rec = ModelRec::new(
                    id,
                    name,
                    "openai-codex-responses",
                    "openai-codex",
                    CODEX_BASE_URL,
                );
                rec.reasoning = true;
                rec.input = if *image {
                    vec!["text".to_string(), "image".to_string()]
                } else {
                    vec!["text".to_string()]
                };
                let base = Cost {
                    input: *input,
                    output: *output,
                    cache_read: *cache_read,
                    cache_write: 0.0,
                    tiers: None,
                };
                rec.cost = if *long_context {
                    with_open_ai_long_context_pricing(base)
                } else {
                    base
                };
                rec.context_window = *context_window;
                rec.max_tokens = CODEX_MAX_TOKENS;
                rec
            },
        )
        .collect()
}

fn apply_temporary_overrides(models: &mut [ModelRec]) {
    for model in models.iter_mut() {
        if model.provider == "github-copilot"
            && ["gpt-5.1-codex-max", "gpt-5.3-codex"]
                .iter()
                .any(|id| model.id.starts_with(id))
        {
            model.context_window = 1_000_000;
        }
        if (model.provider == "anthropic"
            || model.provider == "opencode"
            || model.provider == "opencode-go")
            && [
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "claude-opus-4.6",
                "claude-sonnet-4.6",
            ]
            .contains(&model.id.as_str())
        {
            model.context_window = 1_000_000;
        }
        if (model.provider == "opencode" || model.provider == "opencode-go")
            && (model.id == "claude-sonnet-4-5" || model.id == "claude-sonnet-4")
        {
            model.context_window = 200_000;
        }
        if (model.provider == "opencode" || model.provider == "opencode-go")
            && model.id == "gpt-5.4"
        {
            model.context_window = 272_000;
            model.max_tokens = 128_000;
        }
        if model.provider == "openai"
            && OPENAI_SHORT_CONTEXT_CAPPED_MODEL_IDS.contains(&model.id.as_str())
        {
            model.context_window = OPENAI_LONG_CONTEXT_INPUT_THRESHOLD;
            model.max_tokens = 128_000;
        }
        if model.provider == "openai"
            && OPENAI_LONG_CONTEXT_PRICING_MODEL_IDS.contains(&model.id.as_str())
        {
            let standard = OPENAI_GPT_56_STANDARD_COSTS
                .iter()
                .find(|(id, _)| *id == model.id)
                .map(|(_, cost)| cost_from_entries(*cost));
            let cost = standard.unwrap_or_else(|| model.cost.clone());
            model.cost = with_open_ai_long_context_pricing(cost);
        }
        if model.provider == "cloudflare-ai-gateway" {
            if let Some((_, cost)) = OPENAI_GPT_56_STANDARD_COSTS
                .iter()
                .find(|(id, _)| *id == model.id)
            {
                model.cost = with_open_ai_long_context_pricing(cost_from_entries(*cost));
            }
        }
        if model.provider == "openai" && model.id == "gpt-5-pro" {
            model.max_tokens = 128_000;
        }
        if (model.provider == "openrouter"
            && OPENROUTER_KIMI_K3_MODEL_IDS.contains(&model.id.as_str()))
            || (model.provider == "vercel-ai-gateway" && model.id == "moonshotai/kimi-k3")
        {
            model.max_tokens = KIMI_K3_MAX_TOKENS;
        }
        if model.provider == "openrouter" && model.id == "moonshotai/kimi-k2.5" {
            model.cost.input = 0.41;
            model.cost.output = 2.06;
            model.cost.cache_read = 0.07;
            model.max_tokens = 4096;
        }
        if model.provider == "openrouter" && model.id.starts_with("moonshotai/kimi-k2.6") {
            merge_compat(
                model,
                json!({
                    "supportsDeveloperRole": false,
                    "requiresReasoningContentOnAssistantMessages": true,
                }),
            );
        }
        if model.provider == "openrouter" && model.id == "z-ai/glm-5" {
            model.cost.input = 0.6;
            model.cost.output = 1.9;
            model.cost.cache_read = 0.119;
        }
    }
}

fn generate() -> Result<(BTreeMap<String, Vec<(String, String, Value)>>, String), String> {
    let mut all_models = load_models_dev_data()?;
    all_models.extend(fetch_open_router_models()?);
    all_models.extend(fetch_ai_gateway_models()?);
    all_models.retain(|model| {
        !(model.provider == "xai" && XAI_BUILTIN_EXCLUDED_MODEL_IDS.contains(&model.id.as_str()))
            && !((model.provider == "opencode" || model.provider == "opencode-go")
                && model.id == "gpt-5.3-codex-spark")
    });

    apply_temporary_overrides(&mut all_models);

    // Add missing gpt models.
    for model in static_openai_models() {
        if !all_models
            .iter()
            .any(|m| m.provider == model.provider && m.id == model.id)
        {
            all_models.push(model);
        }
    }

    // DeepSeek + ant-ling static catalogs.
    let deepseek = static_deepseek_models();
    let deepseek_compat = deepseek
        .first()
        .and_then(|model| model.compat.clone())
        .unwrap_or(Value::Null);
    all_models.extend(deepseek);
    all_models.extend(static_ant_ling_models());

    for candidate in all_models.iter_mut() {
        if candidate.api == "openai-completions"
            && candidate.id.contains("deepseek-v4")
            && !QWEN_TOKEN_PLAN_PROVIDER_IDS.contains(&candidate.provider.as_str())
        {
            let mut compat = candidate.compat.take().unwrap_or_else(|| json!({}));
            let preserves_native =
                candidate.provider == "openrouter" || candidate.provider == "opencode";
            if preserves_native {
                compat["requiresReasoningContentOnAssistantMessages"] =
                    json!(deepseek_compat.get("requiresReasoningContentOnAssistantMessages"));
            } else if let (Some(target), Some(source)) =
                (compat.as_object_mut(), deepseek_compat.as_object())
            {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            candidate.compat = Some(compat);
        }
    }

    // Keep only MiniMax models the direct APIs support.
    let minimax_direct = ["MiniMax-M2.7", "MiniMax-M2.7-highspeed", "MiniMax-M3"];
    all_models.retain(|model| {
        !(model.provider == "minimax" || model.provider == "minimax-cn")
            || minimax_direct.contains(&model.id.as_str())
    });

    // OpenAI Codex (ChatGPT OAuth) models.
    all_models.extend(static_codex_models());

    // Mistral Medium 3.5 until models.dev includes it.
    if !all_models
        .iter()
        .any(|m| m.provider == "mistral" && m.id == "mistral-medium-3.5")
    {
        let mut rec = ModelRec::new(
            "mistral-medium-3.5",
            "Mistral Medium 3.5",
            "mistral-conversations",
            "mistral",
            "https://api.mistral.ai",
        );
        rec.reasoning = true;
        rec.input = vec!["text".to_string(), "image".to_string()];
        rec.cost = Cost {
            input: 1.5,
            output: 7.5,
            cache_read: 0.0,
            cache_write: 0.0,
            tiers: None,
        };
        rec.context_window = 262_144;
        rec.max_tokens = 262_144;
        all_models.push(rec);
    }

    // openrouter/auto alias.
    if !all_models
        .iter()
        .any(|m| m.provider == "openrouter" && m.id == "auto")
    {
        let mut rec = ModelRec::new(
            "auto",
            "Auto",
            "openai-completions",
            "openrouter",
            "https://openrouter.ai/api/v1",
        );
        rec.reasoning = true;
        rec.input = vec!["text".to_string(), "image".to_string()];
        rec.context_window = 2_000_000;
        rec.max_tokens = 30_000;
        all_models.push(rec);
    }

    // openrouter/fusion alias.
    if !all_models
        .iter()
        .any(|m| m.provider == "openrouter" && m.id == "openrouter/fusion")
    {
        let mut rec = ModelRec::new(
            "openrouter/fusion",
            "OpenRouter: Fusion",
            "openai-completions",
            "openrouter",
            "https://openrouter.ai/api/v1",
        );
        rec.reasoning = true;
        rec.context_window = 1_000_000;
        rec.max_tokens = 30_000;
        all_models.push(rec);
    }

    // Azure clones of OpenAI responses models.
    let azure_overrides: [(&str, u64); 5] = [
        ("gpt-5.4", 1_050_000),
        ("gpt-5.5", 1_050_000),
        ("gpt-5.6-luna", 1_050_000),
        ("gpt-5.6-sol", 1_050_000),
        ("gpt-5.6-terra", 1_050_000),
    ];
    let azure_models: Vec<ModelRec> = all_models
        .iter()
        .filter(|model| model.provider == "openai" && model.api == "openai-responses")
        .map(|model| {
            let mut rec = model.clone();
            rec.api = "azure-openai-responses".to_string();
            rec.provider = "azure-openai-responses".to_string();
            rec.base_url = String::new();
            rec.context_window = azure_overrides
                .iter()
                .find(|(id, _)| *id == model.id)
                .map_or(model.context_window, |(_, size)| *size);
            rec
        })
        .collect();
    all_models.extend(azure_models);

    // Metadata passes.
    for model in all_models.iter_mut() {
        apply_openai_completions_compat_metadata(model);
        apply_anthropic_messages_compat_metadata(model);
        apply_thinking_level_metadata(model);
        apply_strict_tool_compat_metadata(model);
        apply_openai_grammar_tool_compat_metadata(model);
        apply_openai_tool_search_metadata(model);
        apply_openai_explicit_prompt_cache_metadata(model);
    }
    apply_anthropic_allowed_fallback_model_metadata(&mut all_models);

    // Group by provider and dedupe by model ID (models.dev priority).
    let mut providers: BTreeMap<String, BTreeMap<String, ModelRec>> = BTreeMap::new();
    for model in all_models {
        providers
            .entry(model.provider.clone())
            .or_default()
            .entry(model.id.clone())
            .or_insert(model);
    }

    // Serialize per-provider JSON grouped by API.
    let mut output = BTreeMap::new();
    for (provider_id, entries) in &providers {
        let mut by_api: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
        for (model_id, model) in entries {
            by_api
                .entry(model.api.clone())
                .or_default()
                .push((model_id.clone(), model.to_json()));
        }
        let mut provider_output: Vec<(String, String, Value)> = Vec::new();
        for (api, mut entries) in by_api {
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut group = serde_json::Map::new();
            for (model_id, value) in entries {
                group.insert(model_id, value);
            }
            provider_output.push((api, provider_id.clone(), Value::Object(group)));
        }
        output.insert(provider_id.clone(), provider_output);
    }

    // Build the generated Rust source.
    let mut source = String::from(
        "// This file is auto-generated by pillar-ai/src/bin/generate-models.rs\n\
         // Do not edit manually.\n\
         #![allow(clippy::all)]\n\n\
         use crate::models_catalog::{CatalogModel, ModelCatalog};\n\n",
    );
    writeln!(
        source,
        "/// Generated catalog: provider -> (model id, model JSON)."
    )
    .ok();
    writeln!(source, "pub static MODELS: &[ModelCatalog] = &[").ok();
    for (provider_id, groups) in &output {
        writeln!(source, "    ModelCatalog {{").ok();
        writeln!(source, "        provider: {provider_id:?},").ok();
        writeln!(source, "        models: &[").ok();
        for (api, _provider, group) in groups {
            let Some(group_object) = group.as_object() else {
                continue;
            };
            for (model_id, value) in group_object {
                let json_text = serde_json::to_string(value).expect("serialize model");
                writeln!(
                    source,
                    "            CatalogModel {{ api: {api:?}, id: {model_id:?}, data: {json_text:?} }},"
                )
                .ok();
            }
        }
        writeln!(source, "        ],").ok();
        writeln!(source, "    }},").ok();
    }
    writeln!(source, "];").ok();

    Ok((output, source))
}

fn main() -> ExitCode {
    match generate() {
        Ok((_data, source)) => {
            let manifest_dir = env!("CARGO_MANIFEST_DIR");
            let out_path = Path::new(manifest_dir).join("src/models_generated.rs");
            if let Err(error) = std::fs::write(&out_path, source) {
                eprintln!("failed to write {}: {error}", out_path.display());
                return ExitCode::FAILURE;
            }
            println!("Wrote {}", out_path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("generate-models failed: {error}");
            ExitCode::FAILURE
        }
    }
}
