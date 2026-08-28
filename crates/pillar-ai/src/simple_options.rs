//! Port of packages/ai/src/api/simple-options.ts (pi v0.84.3).
//!
//! Shared option assembly used by simple stream/complete calls: context
//! clamping and thinking-budget math.

use crate::estimate::estimate_context_tokens;
use crate::types::{Context, ThinkingBudgets, ThinkingLevel};

const CONTEXT_SAFETY_TOKENS: u64 = 4096;
const MIN_MAX_TOKENS: u64 = 1;

pub fn clamp_max_tokens_to_context(context_window: u64, context: &Context, max_tokens: u64) -> u64 {
    if context_window == 0 {
        return max_tokens.max(MIN_MAX_TOKENS);
    }
    let available = context_window
        .saturating_sub(estimate_context_tokens(context).tokens)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    max_tokens.min(available.max(MIN_MAX_TOKENS))
}

/// Tokens always left for the answer when a thinking budget shares the
/// response ceiling.
pub const MIN_ANSWER_TOKENS: u64 = 1024;

pub const fn default_thinking_budgets() -> ThinkingBudgets {
    ThinkingBudgets {
        minimal: Some(1024),
        low: Some(2048),
        medium: Some(8192),
        high: Some(16384),
    }
}

pub fn clamp_reasoning(effort: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
    match effort {
        Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => Some(ThinkingLevel::High),
        other => other,
    }
}

pub fn thinking_budget_for_level(
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<ThinkingBudgets>,
) -> u64 {
    let defaults = default_thinking_budgets();
    let budgets = ThinkingBudgets {
        minimal: custom_budgets.and_then(|b| b.minimal).or(defaults.minimal),
        low: custom_budgets.and_then(|b| b.low).or(defaults.low),
        medium: custom_budgets.and_then(|b| b.medium).or(defaults.medium),
        high: custom_budgets.and_then(|b| b.high).or(defaults.high),
    };
    let level = clamp_reasoning(Some(reasoning_level)).expect("clamped level");
    match level {
        ThinkingLevel::Minimal => budgets.minimal.unwrap_or(1024),
        ThinkingLevel::Low => budgets.low.unwrap_or(2048),
        ThinkingLevel::Medium => budgets.medium.unwrap_or(8192),
        _ => budgets.high.unwrap_or(16384),
    }
}

/// Cap a thinking budget so at least [`MIN_ANSWER_TOKENS`] remain under a
/// shared response ceiling.
pub fn clamp_thinking_budget_to_answer_room(thinking_budget: u64, ceiling: u64) -> u64 {
    thinking_budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}

/// Fit a thinking budget inside the response ceiling. An undefined base cap
/// uses the model cap; thinking fits inside it.
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<ThinkingBudgets>,
) -> (u64, u64) {
    let mut thinking_budget = thinking_budget_for_level(reasoning_level, custom_budgets);
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => (base + thinking_budget).min(model_max_tokens),
    };

    if max_tokens <= thinking_budget {
        thinking_budget = clamp_thinking_budget_to_answer_room(thinking_budget, max_tokens);
    }

    (max_tokens, thinking_budget)
}
