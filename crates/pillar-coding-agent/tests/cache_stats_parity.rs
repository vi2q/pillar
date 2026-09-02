//! Port of the upstream cache-stats behavior (pi v0.84.3, exercised via
//! agent-session-stats.test.ts): miss detection, noise floor, reset on
//! compaction/branch-summary, sticky reported-cache, and cost accounting.

use pillar_coding_agent::core::cache_stats::{
    CACHE_TTL_MS, CacheStatsEntry, CacheStatsEntryKind, CacheStatsMessage, ModelPriceSource,
    NoPricing, collect_cache_misses, compute_cache_waste, detect_cache_miss,
};
use std::sync::atomic::{AtomicU32, Ordering};

fn message(timestamp: u64, input: u64, cache_read: u64, cache_write: u64) -> CacheStatsMessage {
    CacheStatsMessage {
        provider: "test".to_string(),
        model: "m".to_string(),
        timestamp,
        input,
        cache_read,
        cache_write,
        cost_input: input as f64 / 1_000_000.0 * 3.0,
        cost_cache_read: cache_read as f64 / 1_000_000.0 * 0.3,
        cost_cache_write: cache_write as f64 / 1_000_000.0 * 3.75,
    }
}

fn assistant_entry(msg: &CacheStatsMessage) -> CacheStatsEntry<'_> {
    CacheStatsEntry {
        kind: CacheStatsEntryKind::Message,
        assistant: Some(msg),
    }
}

fn reset_entry() -> CacheStatsEntry<'static> {
    CacheStatsEntry {
        kind: CacheStatsEntryKind::Compaction,
        assistant: None,
    }
}

/// Pricing source backed by a table of (provider, model, rate).
struct TablePricing {
    rates: Vec<(&'static str, &'static str, f64)>,
    calls: AtomicU32,
}

impl TablePricing {
    fn with(rate: f64) -> Self {
        Self {
            rates: vec![("test", "m", rate)],
            calls: AtomicU32::new(0),
        }
    }
}

impl ModelPriceSource for TablePricing {
    fn cache_read_rate(&self, provider: &str, model_id: &str) -> f64 {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.rates
            .iter()
            .find(|(p, m, _)| *p == provider && *m == model_id)
            .map(|(_, _, rate)| *rate)
            .unwrap_or(0.0)
    }
}

#[test]
fn cache_ttl_constant_matches_anthropic_default() {
    assert_eq!(CACHE_TTL_MS, 5 * 60 * 1000);
}

#[test]
fn first_turn_produces_no_miss() {
    let msg = message(1000, 10_000, 0, 0);
    let entries = vec![];
    assert!(detect_cache_miss(&entries, &msg, &NoPricing).is_none());
}

#[test]
fn full_cache_hit_produces_no_miss() {
    let first = message(1000, 10_000, 0, 0);
    let second = message(2000, 500, 10_000, 0);
    let entries = vec![assistant_entry(&first)];
    assert!(detect_cache_miss(&entries, &second, &NoPricing).is_none());
}

#[test]
fn total_miss_on_reporting_provider_is_counted() {
    // Turn 1 reports cache writes; turn 2 reads nothing -> total miss.
    let first = message(1000, 10_000, 0, 10_000);
    let second = message(2000, 10_000, 0, 0);
    let entries = vec![assistant_entry(&first)];
    let miss = detect_cache_miss(&entries, &second, &NoPricing).expect("miss");
    // prev prompt = 10k input + 10k write = 20k; this turn's prompt = 10k.
    // missed = min(20k, 10k) - 0 = 10k (only what this turn billed).
    assert_eq!(miss.missed_tokens, 10_000);
    assert!(!miss.model_changed);
}

#[test]
fn zero_cache_on_never_reporting_provider_is_not_counted() {
    // No prior cache activity ever reported -> zero-cache turn means nothing.
    let first = message(1000, 10_000, 0, 0);
    let second = message(2000, 10_000, 0, 0);
    let entries = vec![assistant_entry(&first)];
    assert!(detect_cache_miss(&entries, &second, &NoPricing).is_none());
}

#[test]
fn zero_cache_after_prior_activity_is_a_total_miss() {
    let first = message(1000, 5_000, 0, 5_000);
    let second = message(2000, 3_000, 0, 3_000);
    let third = message(3000, 10_000, 0, 0);
    let entries = vec![assistant_entry(&first), assistant_entry(&second)];
    let miss = detect_cache_miss(&entries, &third, &NoPricing).expect("miss");
    assert!(miss.missed_tokens > 0);
}

#[test]
fn misses_below_the_noise_floor_are_ignored() {
    // prev prompt 10k (write); next prompt 10k with read 9.6k
    // -> missed 400 < 1024 noise floor.
    let first = message(1000, 0, 0, 10_000);
    let second = message(2000, 400, 9_600, 0);
    let entries = vec![assistant_entry(&first)];
    assert!(detect_cache_miss(&entries, &second, &NoPricing).is_none());
}

#[test]
fn partial_miss_counts_only_uncached_tokens() {
    let first = message(1000, 0, 0, 10_000);
    let second = message(2000, 2_000, 4_000, 0);
    let entries = vec![assistant_entry(&first)];
    let miss = detect_cache_miss(&entries, &second, &NoPricing).expect("miss");
    // prev prompt 10k; this prompt 6k; read 4k -> missed 2k (above floor).
    assert_eq!(miss.missed_tokens, 2_000);
}

#[test]
fn miss_cost_is_the_paid_rate_delta() {
    // prev prompt 10k written (write premium 3.75/M); turn 2 re-bills 6k at
    // input rate 3/M instead of read rate 0.3/M.
    let first = message(1000, 0, 0, 10_000);
    let second = message(2000, 6_000, 0, 0);
    let entries = vec![assistant_entry(&first)];
    let miss = detect_cache_miss(&entries, &second, &TablePricing::with(0.3)).expect("miss");
    assert_eq!(miss.missed_tokens, 6_000);
    // paid per token from this message = 3.0/1M; read per token falls back to
    // the pricing source = 0.3/1M; delta = 2.7/M over 6k tokens = 0.0162.
    assert!(
        (miss.missed_cost - 0.0162).abs() < 1e-9,
        "{}",
        miss.missed_cost
    );
}

#[test]
fn model_change_is_flagged() {
    let first = message(1000, 0, 0, 10_000);
    let second = CacheStatsMessage {
        model: "other".to_string(),
        ..message(2000, 6_000, 0, 0)
    };
    let entries = vec![assistant_entry(&first)];
    let miss = detect_cache_miss(&entries, &second, &TablePricing::with(0.3)).expect("miss");
    assert!(miss.model_changed);
}

#[test]
fn idle_time_is_measured_against_the_previous_request() {
    let first = message(0, 0, 0, 10_000);
    let second = message(400_000, 6_000, 0, 0);
    let entries = vec![assistant_entry(&first)];
    let miss = detect_cache_miss(&entries, &second, &NoPricing).expect("miss");
    assert_eq!(miss.idle_ms, 400_000);
    assert!(miss.idle_ms > CACHE_TTL_MS);
}

#[test]
fn compaction_resets_the_scan() {
    let first = message(1000, 0, 0, 10_000);
    let second = message(2000, 6_000, 0, 0);
    let compaction = reset_entry();
    let third = message(3000, 6_000, 0, 0);
    let entries = vec![
        assistant_entry(&first),
        assistant_entry(&second),
        compaction,
        assistant_entry(&third),
    ];
    let totals = compute_cache_waste(&entries, &NoPricing);
    // Only the second turn counts (missed 6k); after compaction the scan
    // restarts so the third turn is first-turn exempt.
    assert_eq!(totals.miss_count, 1);
    assert_eq!(totals.missed_tokens, 6_000);
}

#[test]
fn branch_summary_also_resets_the_scan() {
    let first = message(1000, 0, 0, 10_000);
    let second = message(2000, 6_000, 0, 0);
    let branch = CacheStatsEntry {
        kind: CacheStatsEntryKind::BranchSummary,
        assistant: None,
    };
    let third = message(3000, 6_000, 0, 0);
    let entries = vec![
        assistant_entry(&first),
        assistant_entry(&second),
        branch,
        assistant_entry(&third),
    ];
    let totals = compute_cache_waste(&entries, &NoPricing);
    assert_eq!(totals.miss_count, 1);
}

#[test]
fn non_assistant_messages_pass_through() {
    let first = message(1000, 0, 0, 10_000);
    let user_entry = CacheStatsEntry {
        kind: CacheStatsEntryKind::Message,
        assistant: None,
    };
    let second = message(2000, 6_000, 0, 0);
    let entries = vec![
        assistant_entry(&first),
        user_entry,
        assistant_entry(&second),
    ];
    let totals = compute_cache_waste(&entries, &NoPricing);
    assert_eq!(totals.miss_count, 1);
    assert_eq!(totals.missed_tokens, 6_000);
}

#[test]
fn compute_and_collect_agree() {
    let first = message(1000, 0, 0, 10_000);
    let second = message(2000, 6_000, 0, 0);
    let entries = vec![assistant_entry(&first), assistant_entry(&second)];
    let totals = compute_cache_waste(&entries, &NoPricing);
    let misses = collect_cache_misses(&entries, &NoPricing);
    assert_eq!(totals.miss_count, misses.len());
    let total_from_map: u64 = misses.values().map(|miss| miss.missed_tokens).sum();
    assert_eq!(total_from_map, totals.missed_tokens);
}

#[test]
fn pricing_source_fallback_is_used_when_cache_read_is_zero() {
    let pricing = TablePricing::with(0.3);
    let first = message(1000, 0, 0, 10_000);
    let second = message(2000, 6_000, 0, 0);
    let entries = vec![assistant_entry(&first)];
    let miss = detect_cache_miss(&entries, &second, &pricing).expect("miss");
    assert!(pricing.calls.load(Ordering::SeqCst) >= 1);
    let _ = miss;
}
