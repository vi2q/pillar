//! Port of packages/coding-agent/src/core/cache-stats.ts (pi v0.84.3).
//!
//! Prompt-cache waste accounting across a session: tokens that were in the
//! previous turn's prompt but were re-billed instead of read from cache.

use std::collections::BTreeMap;

/// Prompt-cache TTL: idle gaps longer than this are worth mentioning as the
/// likely cause of a miss. Anthropic's default cache TTL is 5 minutes.
pub const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Per-turn misses at or below this are cache breakpoint granularity noise.
const NOISE_FLOOR_TOKENS: u64 = 1024;

/// A counted cache miss on a single assistant message.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheMiss {
    /// Prompt tokens that were in the previous turn's prompt but not read
    /// from cache.
    pub missed_tokens: u64,
    /// Extra dollars paid vs. a full cache hit; 0 when pricing is unknown.
    pub missed_cost: f64,
    /// Milliseconds since the previous request (which last refreshed the cache).
    pub idle_ms: u64,
    /// True when the model changed relative to the previous request.
    pub model_changed: bool,
}

/// Cumulative cache waste totals.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: u64,
    pub missed_cost: f64,
    /// Number of counted misses (turns above the noise floor).
    pub miss_count: usize,
}

/// Minimal pricing lookup, satisfied by ModelRuntime. Cost is $/million tokens.
pub trait ModelPriceSource {
    /// Returns the per-million cache-read rate for the model, when known.
    fn cache_read_rate(&self, provider: &str, model_id: &str) -> f64;
}

/// No-pricing source (upstream callers without a ModelRuntime).
pub struct NoPricing;
impl ModelPriceSource for NoPricing {
    fn cache_read_rate(&self, _provider: &str, _model_id: &str) -> f64 {
        0.0
    }
}

/// A minimal assistant-message view for the scan (upstream consumes
/// `AssistantMessage` directly; the port narrows to the used fields so any
/// message representation can feed the scan).
#[derive(Debug, Clone)]
pub struct CacheStatsMessage {
    pub provider: String,
    pub model: String,
    pub timestamp: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_input: f64,
    pub cost_cache_read: f64,
    pub cost_cache_write: f64,
}

/// Entry kinds the scan distinguishes (upstream `SessionEntry.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatsEntryKind {
    Message,
    Compaction,
    BranchSummary,
    /// Other entry types pass through without affecting the scan state.
    Other,
}

/// One session entry view for the scan.
pub struct CacheStatsEntry<'a> {
    pub kind: CacheStatsEntryKind,
    /// Assistant message data when `kind == Message` with an assistant role.
    pub assistant: Option<&'a CacheStatsMessage>,
}

/// The last request seen by the scan; everything in its prompt should be cached.
struct PreviousRequest {
    prompt_tokens: u64,
    model_key: String,
    timestamp: u64,
    /// Sticky: some earlier request in this scan segment reported cache
    /// activity. Distinguishes a total miss on a cache-read-only provider
    /// (writes unreported) from a provider that never reports caching at all.
    reported_cache: bool,
}

/// Compute the cache miss for one assistant message relative to the previous
/// request. Returns None when nothing is counted: first turn, after a reset,
/// no cache activity ever reported (provider without cache support), or miss
/// below the noise floor.
fn detect_miss(
    prev: Option<&PreviousRequest>,
    message: &CacheStatsMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let prev = prev?;
    let prompt_tokens = message.input + message.cache_read + message.cache_write;
    // A zero-cache turn only counts when cache activity was reported before:
    // on cache-read-only providers that is a total miss, while on providers
    // that never report caching it means nothing.
    if prompt_tokens == 0 || (message.cache_read + message.cache_write == 0 && !prev.reported_cache)
    {
        return None;
    }

    let missed_tokens = prev
        .prompt_tokens
        .min(prompt_tokens)
        .saturating_sub(message.cache_read);
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    // Extra cost = missed tokens billed at the actual paid rate (input +
    // cacheWrite, incl. write premium) instead of the cache-read rate.
    let paid_tokens = message.input + message.cache_write;
    let paid_per_token = if paid_tokens > 0 {
        (message.cost_input + message.cost_cache_write) / paid_tokens as f64
    } else {
        0.0
    };
    let read_per_token = if message.cache_read > 0 {
        message.cost_cache_read / message.cache_read as f64
    } else {
        models.cache_read_rate(&message.provider, &message.model) / 1_000_000.0
    };

    Some(CacheMiss {
        missed_tokens,
        missed_cost: missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0),
        idle_ms: message.timestamp.saturating_sub(prev.timestamp),
        model_changed: format!("{}/{}", message.provider, message.model) != prev.model_key,
    })
}

fn as_previous_request(
    message: &CacheStatsMessage,
    reported_cache: bool,
) -> Option<PreviousRequest> {
    let prompt_tokens = message.input + message.cache_read + message.cache_write;
    if prompt_tokens == 0 {
        return None;
    }
    Some(PreviousRequest {
        prompt_tokens,
        model_key: format!("{}/{}", message.provider, message.model),
        timestamp: message.timestamp,
        reported_cache: reported_cache || message.cache_read + message.cache_write > 0,
    })
}

struct ScanResult {
    prev: Option<PreviousRequest>,
    totals: CacheWasteTotals,
    /// Keyed by assistant message timestamp (messages may repeat; the
    /// upstream map keys by reference, the port by timestamp+model).
    misses: BTreeMap<(u64, String), CacheMiss>,
}

fn scan(entries: &[CacheStatsEntry], models: &dyn ModelPriceSource) -> ScanResult {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();
    let mut misses: BTreeMap<(u64, String), CacheMiss> = BTreeMap::new();

    for entry in entries {
        if matches!(
            entry.kind,
            CacheStatsEntryKind::Compaction | CacheStatsEntryKind::BranchSummary
        ) {
            // The context legitimately changed; the next turn's prompt is new
            // content, not re-billed content. Model switches are NOT exempt:
            // they re-bill the full prompt and should be counted.
            prev = None;
            continue;
        }
        if entry.kind == CacheStatsEntryKind::Message {
            if let Some(message) = entry.assistant {
                if let Some(miss) = detect_miss(prev.as_ref(), message, models) {
                    totals.missed_tokens += miss.missed_tokens;
                    totals.missed_cost += miss.missed_cost;
                    totals.miss_count += 1;
                    misses.insert(
                        (
                            message.timestamp,
                            format!("{}/{}", message.provider, message.model),
                        ),
                        miss,
                    );
                }
                prev =
                    as_previous_request(message, prev.as_ref().is_some_and(|p| p.reported_cache))
                        .or(prev);
            }
        }
    }
    ScanResult {
        prev,
        totals,
        misses,
    }
}

/// Cumulative cache waste across a session: prompt tokens that should have
/// been cache reads (they were in the previous turn's prompt) but were
/// re-billed.
pub fn compute_cache_waste(
    entries: &[CacheStatsEntry],
    models: &dyn ModelPriceSource,
) -> CacheWasteTotals {
    scan(entries, models).totals
}

/// All counted cache misses across a session, keyed by (timestamp,
/// provider/model) of the assistant message that paid for them. Used to
/// re-derive transcript notices when rebuilding the chat from entries.
pub fn collect_cache_misses(
    entries: &[CacheStatsEntry],
    models: &dyn ModelPriceSource,
) -> BTreeMap<(u64, String), CacheMiss> {
    scan(entries, models).misses
}

/// Detect a cache miss on a just-completed assistant message. `entries` must
/// not yet contain `message` (message_end fires before persistence).
pub fn detect_cache_miss(
    entries: &[CacheStatsEntry],
    message: &CacheStatsMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let prev = scan(entries, models).prev;
    detect_miss(prev.as_ref(), message, models)
}
