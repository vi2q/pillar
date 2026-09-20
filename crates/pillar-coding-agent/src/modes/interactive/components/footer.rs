//! Port of components/footer.ts: the two-line status footer (pwd + git
//! branch + session name, token/cost/context stats with the model on the
//! right) plus the extension status line.
//!
//! divergence: upstream reads `HOME`/`USERPROFILE` from the environment at
//! render time; the port reads it once at construction (with
//! [`set_home_dir`] for tests/hosts). The keybindings-free extension
//! statuses come from the provider in a `BTreeMap` (already sorted —
//! upstream sorts with `localeCompare`).

use std::sync::Arc;

use pillar_tui::text_utils::{truncate_to_width, visible_width};
use pillar_tui::tui::{Component, RenderLines, render_lines};

use crate::core::agent_session_class::AgentSession;
use crate::core::footer_data_provider::FooterDataProvider;
use crate::core::resource_loader::GitPaths;
use crate::core::usage_totals::{UsageTotals, are_experimental_features_enabled};
use crate::modes::interactive::theme::theme;

/// Sanitize text for a single-line status (upstream `sanitizeStatusText`):
/// control characters become spaces, runs of spaces collapse, edges trim.
fn sanitize_status_text(text: &str) -> String {
    let spaced: String = text
        .chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut collapsed = String::new();
    let mut in_space = false;
    for c in spaced.chars() {
        if c == ' ' {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            in_space = false;
            collapsed.push(c);
        }
    }
    collapsed.trim().to_string()
}

/// Format token counts for compact display (upstream `formatTokens`).
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1000.0).round());
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round())
}

/// One path component of a normalised absolute path.
fn path_components(path: &std::path::Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            std::path::Component::RootDir => Some("/".to_string()),
            _ => None,
        })
        .collect()
}

/// Node's `path.relative(from, to)` for absolute POSIX-style paths.
fn relative_path(from: &std::path::Path, to: &std::path::Path) -> String {
    let from_parts = path_components(from);
    let to_components = path_components(to);
    let common = from_parts
        .iter()
        .zip(to_components.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..from_parts.len() {
        parts.push("..".to_string());
    }
    parts.extend(to_components[common..].iter().cloned());
    parts.join("/")
}

/// Format the cwd for the footer (upstream `formatCwdForFooter`): collapse
/// the home directory to `~/...` when the cwd lives inside it.
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return cwd.to_string();
    };

    let resolved_cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.into());
    let resolved_home = std::path::absolute(home).unwrap_or_else(|_| home.into());
    let relative_to_home = relative_path(&resolved_home, &resolved_cwd);
    // Upstream: inside home iff the relative path is empty, not exactly ".."
    // and not starting with `../` and not absolute.
    let is_inside_home = relative_to_home.is_empty()
        || (relative_to_home != ".."
            && !relative_to_home.starts_with("../")
            && !relative_to_home.starts_with('/'));

    if !is_inside_home {
        return cwd.to_string();
    }
    if relative_to_home.is_empty() {
        return "~".to_string();
    }
    format!("~/{relative_to_home}")
}

/// The footer (upstream `FooterComponent`).
pub struct FooterComponent {
    session: Arc<AgentSession>,
    footer_data: FooterDataProvider,
    git_paths: Option<GitPaths>,
    home_dir: Option<String>,
    auto_compact_enabled: bool,
}

impl FooterComponent {
    pub fn new(
        session: Arc<AgentSession>,
        footer_data: FooterDataProvider,
        git_paths: Option<GitPaths>,
    ) -> Self {
        let home_dir = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .filter(|home| !home.is_empty());
        Self {
            session,
            footer_data,
            git_paths,
            home_dir,
            auto_compact_enabled: true,
        }
    }

    pub fn set_session(&mut self, session: Arc<AgentSession>) {
        self.session = session;
    }

    pub fn set_home_dir(&mut self, home: Option<String>) {
        self.home_dir = home;
    }

    pub fn set_auto_compact_enabled(&mut self, enabled: bool) {
        self.auto_compact_enabled = enabled;
    }

    /// The provider (upstream `footerData`); hosts configure extension
    /// statuses and provider counts on it.
    pub fn footer_data(&mut self) -> &mut FooterDataProvider {
        &mut self.footer_data
    }

    /// No-op: git branch caching lives in the provider (upstream
    /// `invalidate`, kept for interactive-mode call sites).
    pub fn dispose(&self) {}
}

impl Component for FooterComponent {
    fn render(&mut self, width: usize) -> RenderLines {
        let session = self.session.as_ref();
        // Snapshot what render needs from the session manager and drop the
        // lock before `context_usage()` takes it again.
        let (entries, cwd, session_name) = {
            let session_manager = session.session_manager().lock().expect("session manager");
            (
                session_manager.get_entries_owned(),
                session_manager.cwd().to_string(),
                session_manager.session_name(),
            )
        };

        // Cumulative usage from ALL session entries (upstream the
        // getEntries loop; not just post-compaction messages).
        let mut usage_totals = UsageTotals::new();
        let mut latest_cache_hit_rate: Option<f64> = None;

        for entry in &entries {
            match entry {
                crate::core::session_entries::SessionEntry::Message(message_entry)
                    if matches!(
                        message_entry.message,
                        crate::core::messages::CodingAgentMessage::Base(
                            pillar_ai::types::Message::Assistant(_)
                        )
                    ) =>
                {
                    let assistant = match &message_entry.message {
                        crate::core::messages::CodingAgentMessage::Base(
                            pillar_ai::types::Message::Assistant(assistant),
                        ) => assistant,
                        _ => unreachable!(),
                    };
                    usage_totals.add(&assistant.usage);

                    let latest_prompt_tokens = assistant.usage.input
                        + assistant.usage.cache_read
                        + assistant.usage.cache_write;
                    latest_cache_hit_rate = if latest_prompt_tokens > 0 {
                        Some(
                            assistant.usage.cache_read as f64 / latest_prompt_tokens as f64 * 100.0,
                        )
                    } else {
                        None
                    };
                }
                crate::core::session_entries::SessionEntry::Message(message_entry)
                    if matches!(
                        message_entry.message,
                        crate::core::messages::CodingAgentMessage::Base(
                            pillar_ai::types::Message::ToolResult(_)
                        )
                    ) =>
                {
                    if let crate::core::messages::CodingAgentMessage::Base(
                        pillar_ai::types::Message::ToolResult(tool_result),
                    ) = &message_entry.message
                    {
                        if let Some(usage) = &tool_result.usage {
                            usage_totals.add(usage);
                        }
                    }
                }
                crate::core::session_entries::SessionEntry::BranchSummary(branch_summary)
                    if branch_summary.usage.is_some() =>
                {
                    usage_totals.add(branch_summary.usage.as_ref().expect("checked"));
                }
                crate::core::session_entries::SessionEntry::Compaction(compaction)
                    if compaction.usage.is_some() =>
                {
                    usage_totals.add(compaction.usage.as_ref().expect("checked"));
                }
                _ => {}
            }
        }

        // Context usage (handles compaction correctly; after a compaction,
        // tokens are unknown until the next LLM response).
        let context_usage = session.context_usage();
        let state = session.state();
        let model = session.model();
        let context_window = context_usage
            .as_ref()
            .map(|usage| usage.context_window)
            .unwrap_or_else(|| model.as_ref().map(|m| m.context_window).unwrap_or(0));
        let context_percent_value = context_usage
            .as_ref()
            .and_then(|usage| usage.percent)
            .unwrap_or(0.0);
        let context_percent = match context_usage.as_ref().and_then(|usage| usage.percent) {
            Some(percent) => format!("{percent:.1}"),
            None => "?".to_string(),
        };

        // Replace the home directory with ~.
        let mut pwd = format_cwd_for_footer(&cwd, self.home_dir.as_deref());

        // Git branch.
        if let Some(branch) = self.footer_data.git_branch(self.git_paths.as_ref()) {
            pwd = format!("{pwd} ({branch})");
        }

        // Session name.
        if let Some(session_name) = session_name {
            pwd = format!("{pwd} \u{2022} {session_name}");
        }

        // Stats line.
        let mut stats_parts: Vec<String> = Vec::new();
        if usage_totals.input > 0 {
            stats_parts.push(format!("\u{2191}{}", format_tokens(usage_totals.input)));
        }
        if usage_totals.output > 0 {
            stats_parts.push(format!("\u{2193}{}", format_tokens(usage_totals.output)));
        }
        if usage_totals.cache_read > 0 {
            stats_parts.push(format!("R{}", format_tokens(usage_totals.cache_read)));
        }
        if usage_totals.cache_write > 0 {
            stats_parts.push(format!("W{}", format_tokens(usage_totals.cache_write)));
        }
        if (usage_totals.cache_read > 0 || usage_totals.cache_write > 0)
            && latest_cache_hit_rate.is_some()
        {
            stats_parts.push(format!("CH{:.1}%", latest_cache_hit_rate.expect("checked")));
        }

        // Kimi Coding is subscription-backed despite using API-key
        // authentication.
        let using_subscription = model.as_ref().is_some_and(|model| {
            model.provider == "kimi-coding"
                || session
                    .model_runtime()
                    .is_using_subscription(&model.provider)
        });
        if usage_totals.cost != 0.0 || using_subscription {
            let cost_str = format!(
                "${:.3}{}",
                usage_totals.cost,
                if using_subscription { " (sub)" } else { "" }
            );
            stats_parts.push(cost_str);
        }

        // Context percentage, colourised by usage.
        let auto_indicator = if self.auto_compact_enabled {
            " (auto)"
        } else {
            ""
        };
        let context_percent_display = format!(
            "{context_percent}%/{}{auto_indicator}",
            format_tokens(context_window)
        );
        let theme_obj = theme();
        let context_percent_str = if context_percent_value > 90.0 {
            theme_obj.fg("error", &context_percent_display)
        } else if context_percent_value > 70.0 {
            theme_obj.fg("warning", &context_percent_display)
        } else {
            context_percent_display
        };
        stats_parts.push(context_percent_str);
        if are_experimental_features_enabled() {
            stats_parts.push(format!(
                "{} {}",
                theme_obj.fg("dim", "\u{2022}"),
                theme_obj.bold(&theme_obj.fg("warning", "xp"))
            ));
        }

        let stats_left = stats_parts.join(" ");
        let mut stats_left_width = visible_width(&stats_left);

        // Truncate the stats when they are too wide.
        let stats_left = if stats_left_width > width {
            let truncated = truncate_to_width(&stats_left, width, "...", false);
            stats_left_width = visible_width(&truncated);
            truncated
        } else {
            stats_left
        };

        // Minimum padding between the stats and the model.
        let min_padding = 2usize;

        // Thinking level indicator when the model supports reasoning.
        let model_name = model
            .as_ref()
            .map(|model| model.id.clone())
            .unwrap_or_else(|| "no-model".to_string());
        let mut right_side_without_provider = model_name.clone();
        if model.as_ref().is_some_and(|model| model.reasoning) {
            let thinking_level = state.thinking_level.as_str();
            right_side_without_provider = if thinking_level == "off" {
                format!("{model_name} \u{2022} thinking off")
            } else {
                format!("{model_name} \u{2022} {thinking_level}")
            };
        }

        // Provider in parentheses when there are multiple providers and
        // enough room.
        let mut right_side = right_side_without_provider.clone();
        if self.footer_data.available_provider_count() > 1 {
            if let Some(model) = &model {
                right_side = format!("({}) {right_side_without_provider}", model.provider);
                if stats_left_width + min_padding + visible_width(&right_side) > width {
                    // Too wide, fall back.
                    right_side = right_side_without_provider.clone();
                }
            }
        }

        let right_side_width = visible_width(&right_side);
        let total_needed = stats_left_width + min_padding + right_side_width;

        let stats_line = if total_needed <= width {
            // Both fit — pad to right-align the model.
            let padding = " ".repeat(width - stats_left_width - right_side_width);
            format!("{stats_left}{padding}{right_side}")
        } else {
            // Truncate the right side.
            let available_for_right = width.saturating_sub(stats_left_width + min_padding);
            if available_for_right > 0 {
                let truncated_right =
                    truncate_to_width(&right_side, available_for_right, "", false);
                let truncated_right_width = visible_width(&truncated_right);
                let padding =
                    " ".repeat(width.saturating_sub(stats_left_width + truncated_right_width));
                format!("{stats_left}{padding}{truncated_right}")
            } else {
                // Not enough space for the right side at all.
                stats_left.clone()
            }
        };

        // Dim each part separately: the context % colour codes end with a
        // reset, which would clear an outer dim wrapper.
        let dim_stats_left = theme_obj.fg("dim", &stats_left);
        let remainder = &stats_line[stats_left.len()..];
        let dim_remainder = theme_obj.fg("dim", remainder);

        let pwd_line = truncate_to_width(
            &theme_obj.fg("dim", &pwd),
            width,
            &theme_obj.fg("dim", "..."),
            false,
        );
        let mut lines = vec![pwd_line, format!("{dim_stats_left}{dim_remainder}")];

        // Extension statuses on one line, sorted by key (the provider's
        // BTreeMap is already sorted; upstream sorts with localeCompare).
        let statuses: Vec<String> = self
            .footer_data
            .extension_statuses()
            .values()
            .map(|text| sanitize_status_text(text))
            .collect();
        if !statuses.is_empty() {
            let status_line = statuses.join(" ");
            lines.push(truncate_to_width(
                &status_line,
                width,
                &theme_obj.fg("dim", "..."),
                false,
            ));
        }

        render_lines(lines)
    }
}
