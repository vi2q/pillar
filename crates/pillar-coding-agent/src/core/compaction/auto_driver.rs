//! Port of the auto-compaction decision core of
//! packages/coding-agent/src/core/agent-session.ts (pi v0.84.3):
//! `_checkCompaction` and `_runAutoCompaction` — the three automatic
//! compaction cases (context overflow, recoverable length stop,
//! threshold) with their guards: disabled settings, aborted
//! messages, cross-model overflow errors, stale pre-compaction
//! usage, error/zero-usage estimation from the last valid response,
//! and the one-attempt overflow recovery budget.
//!
//! divergences: the summary generation call and the extension
//! `session_before_compact` hook surface as enum outputs (the caller
//! runs [`crate::core::compaction::driver::prepare_compaction`] and
//! the summarizer); agent-state mutation (dropping the trailing
//! assistant message before an overflow retry) is returned as an
//! instruction instead of applied in place.

use pillar_ai::types::{AssistantMessage, StopReason, Usage};

use crate::core::compaction::driver::CompactionSettings;
use crate::core::compaction::driver::{
    calculate_context_tokens, estimate_context_tokens, should_compact,
};
use crate::core::messages::CodingAgentMessage;
use crate::core::session_entries::SessionEntry;

/// Automatic compaction trigger (upstream the `reason` union).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoReason {
    Threshold,
    Overflow,
}

impl AutoReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// What the caller should do after [`check_compaction`].
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionDecision {
    /// No compaction.
    None,
    /// Run the auto-compaction flow for the given reason; `will_retry`
    /// continues the interrupted turn after an overflow compaction.
    Run {
        reason: AutoReason,
        will_retry: bool,
        /// Drop the trailing assistant message from agent state before
        /// compacting (upstream the overflow Case 1 state edit).
        drop_trailing_assistant: bool,
    },
    /// Overflow recovery already attempted once; the caller should
    /// surface the given error message (upstream the compaction_end
    /// errorMessage).
    RecoveryFailed { error_message: String },
}

/// Inputs for [`check_compaction`] (upstream the fields the method
/// reads off AgentSession).
pub struct CheckInput<'a> {
    pub assistant: &'a AssistantMessage,
    pub settings: CompactionSettings,
    /// Current model (provider, id, context window, max tokens).
    pub current_model: Option<(&'a str, &'a str, u64, u64)>,
    /// The session branch (for the latest compaction boundary guard).
    pub branch: &'a [SessionEntry],
    /// Agent-state messages (for error/zero-usage estimation).
    pub messages: &'a [CodingAgentMessage],
    /// Whether the one overflow-recovery attempt was already used.
    pub overflow_recovery_attempted: bool,
}

fn entry_timestamp(entry: &SessionEntry) -> u64 {
    match entry {
        SessionEntry::Message(e) => e.base.timestamp,
        SessionEntry::ThinkingLevelChange(e) => e.base.timestamp,
        SessionEntry::ModelChange(e) => e.base.timestamp,
        SessionEntry::Compaction(e) => e.base.timestamp,
        SessionEntry::BranchSummary(e) => e.base.timestamp,
        SessionEntry::Custom(e) => e.base.timestamp,
        SessionEntry::Label(e) => e.base.timestamp,
        SessionEntry::SessionInfo(e) => e.base.timestamp,
        SessionEntry::CustomMessage(e) => e.base.timestamp,
    }
}

fn latest_compaction_timestamp(branch: &[SessionEntry]) -> Option<u64> {
    branch.iter().rev().find_map(|entry| match entry {
        SessionEntry::Compaction(_) => Some(entry_timestamp(entry)),
        _ => None,
    })
}

/// Upstream `_checkCompaction`: decide whether/how to auto-compact
/// after an assistant message.
pub fn check_compaction(input: &CheckInput<'_>) -> CompactionDecision {
    if !input.settings.enabled {
        return CompactionDecision::None;
    }
    // Upstream skipAbortedCheck default: user-cancelled messages never
    // compact.
    if input.assistant.stop_reason == StopReason::Aborted {
        return CompactionDecision::None;
    }
    let Some((provider, id, context_window, max_tokens)) = input.current_model else {
        return CompactionDecision::None;
    };
    // Cross-model overflow errors must not compact the new model's
    // context.
    let same_model = input.assistant.provider == provider && input.assistant.model == id;
    // Stale pre-compaction messages never retrigger.
    let compaction_at = latest_compaction_timestamp(input.branch);
    if compaction_at.is_some_and(|at| input.assistant.timestamp <= at) {
        return CompactionDecision::None;
    }

    let context_overflow =
        same_model && pillar_ai::is_context_overflow(input.assistant, Some(context_window));
    let recoverable_length =
        same_model && pillar_ai::is_recoverable_length(input.assistant, max_tokens);
    if context_overflow || recoverable_length {
        let will_retry = input.assistant.stop_reason != StopReason::Stop;
        if !will_retry {
            // Case 2: completed response — compact without retrying.
            return CompactionDecision::Run {
                reason: AutoReason::Overflow,
                will_retry: false,
                drop_trailing_assistant: false,
            };
        }
        if input.overflow_recovery_attempted {
            // One compact-and-retry attempt only.
            let error_message = if context_overflow {
                "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.".to_string()
            } else {
                "Truncated response recovery failed after one compact-and-retry attempt."
                    .to_string()
            };
            return CompactionDecision::RecoveryFailed { error_message };
        }
        // Case 1: drop the failed/trailing assistant message, compact,
        // retry once.
        return CompactionDecision::Run {
            reason: AutoReason::Overflow,
            will_retry: true,
            drop_trailing_assistant: true,
        };
    }

    // Case 3: threshold compaction without retry. Error or zero-usage
    // responses estimate from the last valid usage-bearing message.
    let direct = if input.assistant.stop_reason == StopReason::Error {
        0
    } else {
        calculate_context_tokens(&input.assistant.usage)
    };
    let context_tokens = if input.assistant.stop_reason == StopReason::Error || direct == 0 {
        let estimate = estimate_context_tokens(input.messages);
        match estimate.last_usage_index {
            None => estimate.tokens,
            Some(last_usage_index) => {
                // Usage-backed estimates must come from post-compaction
                // messages; pre-compaction usage is stale (larger context).
                if let Some(at) = compaction_at {
                    if let Some(CodingAgentMessage::Base(pillar_ai::types::Message::Assistant(
                        assistant,
                    ))) = input.messages.get(last_usage_index)
                    {
                        if assistant.timestamp <= at {
                            return CompactionDecision::None;
                        }
                    }
                }
                estimate.tokens
            }
        }
    } else {
        direct
    };
    if should_compact(context_tokens, context_window, &input.settings) {
        return CompactionDecision::Run {
            reason: AutoReason::Threshold,
            will_retry: false,
            drop_trailing_assistant: false,
        };
    }
    CompactionDecision::None
}

/// Upstream the `estimatedTokensAfter` computation and the
/// `tokensBefore` guard shapes shared by manual/auto compaction
/// results.
pub fn tokens_before_from_usage(usage: Option<&Usage>) -> u64 {
    usage.map(calculate_context_tokens).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session_entries::{CompactionEntry, SessionEntryBase};
    use pillar_ai::types::UsageCost;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + output,
            cost: UsageCost::default(),
        }
    }

    fn assistant(
        provider: &str,
        model: &str,
        stop: StopReason,
        usage: Usage,
        timestamp: u64,
    ) -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            api: "test-api".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            response_model: None,
            response_id: None,
            diagnostics: vec![],
            usage,
            stop_reason: stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp,
        }
    }

    fn settings() -> CompactionSettings {
        CompactionSettings {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }

    fn compaction_entry(timestamp: u64) -> SessionEntry {
        SessionEntry::Compaction(CompactionEntry {
            base: SessionEntryBase {
                id: "c1".to_string(),
                parent_id: None,
                timestamp,
            },
            summary: "summary".to_string(),
            first_kept_entry_id: "e1".to_string(),
            tokens_before: 100,
            details: None,
            usage: None,
            from_hook: false,
        })
    }

    #[test]
    fn disabled_settings_never_compact() {
        let assistant = assistant("p", "m", StopReason::Stop, usage(90_000, 100), 10);
        let input = CheckInput {
            assistant: &assistant,
            settings: CompactionSettings {
                enabled: false,
                ..settings()
            },
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(check_compaction(&input), CompactionDecision::None);
    }

    #[test]
    fn aborted_messages_never_compact() {
        let assistant = assistant("p", "m", StopReason::Aborted, usage(99_000, 100), 10);
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(check_compaction(&input), CompactionDecision::None);
    }

    #[test]
    fn cross_model_overflow_is_ignored() {
        // The overflow error came from the previous (smaller) model.
        let mut assistant = assistant("p", "old", StopReason::Error, usage(0, 0), 10);
        assistant.error_message =
            Some("prompt is too long: 200000 tokens > 100000 maximum".to_string());
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "new", 1_000_000, 10_000)),
            branch: &[],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(check_compaction(&input), CompactionDecision::None);
    }

    #[test]
    fn stale_pre_compaction_messages_are_ignored() {
        let assistant = assistant("p", "m", StopReason::Error, usage(0, 0), 50);
        let compaction = compaction_entry(100);
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[compaction],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(check_compaction(&input), CompactionDecision::None);
    }

    #[test]
    fn overflow_error_compacts_and_retries_once() {
        let mut assistant = assistant("p", "m", StopReason::Error, usage(0, 0), 200);
        assistant.error_message =
            Some("prompt is too long: 200000 tokens > 100000 maximum".to_string());
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(
            check_compaction(&input),
            CompactionDecision::Run {
                reason: AutoReason::Overflow,
                will_retry: true,
                drop_trailing_assistant: true,
            }
        );
        // The second attempt fails recovery.
        let input = CheckInput {
            overflow_recovery_attempted: true,
            ..input
        };
        assert_eq!(
            check_compaction(&input),
            CompactionDecision::RecoveryFailed {
                error_message: "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model.".to_string(),
            }
        );
    }

    #[test]
    fn completed_overflow_compacts_without_retry() {
        // z.ai-style silent overflow: successful stop with input above
        // the window.
        let assistant = assistant("p", "m", StopReason::Stop, usage(100_001, 100), 200);
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(
            check_compaction(&input),
            CompactionDecision::Run {
                reason: AutoReason::Overflow,
                will_retry: false,
                drop_trailing_assistant: false,
            }
        );
    }

    #[test]
    fn recoverable_length_compacts_and_retries() {
        // length stop below the desired output limit.
        let assistant = assistant("p", "m", StopReason::Length, usage(50_000, 5_000), 200);
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(
            check_compaction(&input),
            CompactionDecision::Run {
                reason: AutoReason::Overflow,
                will_retry: true,
                drop_trailing_assistant: true,
            }
        );
    }

    #[test]
    fn threshold_compaction_from_usage() {
        // usage total above (contextWindow - reserve).
        let assistant = assistant("p", "m", StopReason::Stop, usage(90_000, 100), 200);
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(
            check_compaction(&input),
            CompactionDecision::Run {
                reason: AutoReason::Threshold,
                will_retry: false,
                drop_trailing_assistant: false,
            }
        );
    }

    #[test]
    fn below_threshold_does_not_compact() {
        let assistant = assistant("p", "m", StopReason::Stop, usage(50_000, 100), 200);
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[],
            messages: &[],
            overflow_recovery_attempted: false,
        };
        assert_eq!(check_compaction(&input), CompactionDecision::None);
    }

    #[test]
    fn error_response_estimates_from_last_usage() {
        // An error response has no usable usage; the estimate comes
        // from the message list (a single large user message).
        let mut assistant = assistant("p", "m", StopReason::Error, usage(0, 0), 200);
        assistant.error_message = Some("upstream 529".to_string());
        let large_user = CodingAgentMessage::Base(pillar_ai::types::Message::User {
            content: pillar_ai::types::UserContent::Text("x".repeat(400_000)),
            timestamp: 1,
        });
        let input = CheckInput {
            assistant: &assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[],
            messages: &[large_user],
            overflow_recovery_attempted: false,
        };
        assert_eq!(
            check_compaction(&input),
            CompactionDecision::Run {
                reason: AutoReason::Threshold,
                will_retry: false,
                drop_trailing_assistant: false,
            }
        );
    }

    #[test]
    fn stale_usage_backed_estimate_is_ignored() {
        // The last usage-bearing message predates the compaction
        // boundary: no compaction.
        let mut error_assistant = assistant("p", "m", StopReason::Error, usage(0, 0), 200);
        error_assistant.error_message = Some("upstream 529".to_string());
        let stale_assistant =
            CodingAgentMessage::Base(pillar_ai::types::Message::Assistant(Box::new(assistant(
                "p",
                "m",
                StopReason::Stop,
                usage(90_000, 100),
                50,
            ))));
        let compaction = compaction_entry(100);
        let input = CheckInput {
            assistant: &error_assistant,
            settings: settings(),
            current_model: Some(("p", "m", 100_000, 10_000)),
            branch: &[compaction],
            messages: &[stale_assistant],
            overflow_recovery_attempted: false,
        };
        assert_eq!(check_compaction(&input), CompactionDecision::None);
    }

    #[test]
    fn tokens_before_helper() {
        assert_eq!(tokens_before_from_usage(Some(&usage(10, 5))), 15);
        assert_eq!(tokens_before_from_usage(None), 0);
    }
}
