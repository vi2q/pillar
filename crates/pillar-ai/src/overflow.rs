//! Port of packages/ai/src/utils/overflow.ts (pi v0.84.3).
//!
//! Context overflow detection: error-message patterns per provider, silent
//! overflow (z.ai style usage over context), and length-stop overflow
//! (Xiaomi MiMo style filled context with zero output).

use std::sync::OnceLock;

use regex::Regex;

use crate::types::{AssistantMessage, StopReason};

fn overflow_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"prompt is too long",                    // Anthropic token overflow
            r"request_too_large",                     // Anthropic byte-size overflow (413)
            r"input is too long for requested model", // Amazon Bedrock
            r"exceeds the context window",            // OpenAI (Completions & Responses)
            r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))", // OpenAI-compatible proxies (LiteLLM)
            r"input token count.*exceeds the maximum", // Google (Gemini)
            r"maximum prompt length is \d+",           // xAI (Grok)
            r"reduce the length of the messages",      // Groq
            r"maximum context length is \d+ tokens",   // OpenRouter (most backends)
            r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?", // OpenRouter/Poolside
            r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)", // Together AI
            r"exceeds the limit of \d+",           // GitHub Copilot
            r"exceeds the available context size", // llama.cpp server
            r"greater than the context length",    // LM Studio
            r"context window exceeds limit",       // MiniMax
            r"exceeded model token limit",         // Kimi For Coding
            r"too large for model with \d+ maximum context length", // Mistral
            r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?", // DS4 server
            r"model_context_window_exceeded", // z.ai finish_reason surfaced as error text
            r"prompt too long; exceeded (?:max )?context length", // Ollama explicit overflow
            r"range of input length should be", // DashScope / Qwen Token Plan
            r"context[_ ]length[_ ]exceeded", // Generic fallback
            r"too many tokens",               // Generic fallback
            r"token limit exceeded",          // Generic fallback
            r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)", // Cerebras: 400/413 with no body
        ]
        .iter()
        .map(|pattern| Regex::new(&format!("(?i){pattern}")).expect("overflow pattern"))
        .collect()
    })
}

fn non_overflow_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"^(Throttling error|Service unavailable):", // AWS Bedrock human-readable prefixes
            r"rate limit",                               // Generic rate limiting
            r"too many requests",                        // Generic HTTP 429 style
        ]
        .iter()
        .map(|pattern| Regex::new(&format!("(?i){pattern}")).expect("non-overflow pattern"))
        .collect()
    })
}

/// Check if an assistant message represents a context overflow error.
///
/// Three cases:
/// 1. Error-based overflow: `stopReason "error"` matching a provider pattern.
/// 2. Silent overflow (z.ai): success with usage.input over the context window.
/// 3. Length-stop overflow (Xiaomi MiMo): "length" with zero output filling
///    the context window.
///
/// `context_window` enables cases 2 and 3.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    // Case 1: error message patterns.
    if message.stop_reason == StopReason::Error {
        if let Some(error_message) = &message.error_message {
            let is_non_overflow = non_overflow_patterns()
                .iter()
                .any(|p| p.is_match(error_message));
            if !is_non_overflow
                && overflow_patterns()
                    .iter()
                    .any(|p| p.is_match(error_message))
            {
                return true;
            }
        }
    }

    let Some(context_window) = context_window else {
        return false;
    };

    // Case 2: silent overflow — successful but usage exceeds context.
    if message.stop_reason == StopReason::Stop {
        let input_tokens = message.usage.input + message.usage.cache_read;
        if input_tokens > context_window {
            return true;
        }
    }

    // Case 3: length-stop overflow — server truncates oversized input,
    // leaving no room for output (0.99 fudge for tokenizer drift).
    if message.stop_reason == StopReason::Length && message.usage.output == 0 {
        let input_tokens = message.usage.input + message.usage.cache_read;
        if input_tokens as f64 >= context_window as f64 * 0.99 {
            return true;
        }
    }

    false
}

/// Check whether a length stop ended below the caller or model's intended
/// output limit; such responses may warrant one bounded compact-and-retry.
/// `desired_max_output` must be the original limit before clamping.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output
}

/// Get the overflow pattern source strings for testing purposes.
pub fn get_overflow_patterns() -> Vec<&'static str> {
    [
        "prompt is too long",
        "request_too_large",
        "input is too long for requested model",
        "exceeds the context window",
        "exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\\d,]+ tokens?|\\s*\\([\\d,]+\\))",
        "input token count.*exceeds the maximum",
        "maximum prompt length is \\d+",
        "reduce the length of the messages",
        "maximum context length is \\d+ tokens",
        "exceeds (?:the )?maximum allowed input length of [\\d,]+ tokens?",
        "input \\(\\d+ tokens\\) is longer than the model'?s context length \\(\\d+ tokens\\)",
        "exceeds the limit of \\d+",
        "exceeds the available context size",
        "greater than the context length",
        "context window exceeds limit",
        "exceeded model token limit",
        "too large for model with \\d+ maximum context length",
        "prompt has [\\d,]+ tokens?, but the configured context size is [\\d,]+ tokens?",
        "model_context_window_exceeded",
        "prompt too long; exceeded (?:max )?context length",
        "range of input length should be",
        "context[_ ]length[_ ]exceeded",
        "too many tokens",
        "token limit exceeded",
        "^4(?:00|13)\\s*(?:status code)?\\s*\\(no body\\)",
    ]
    .to_vec()
}
