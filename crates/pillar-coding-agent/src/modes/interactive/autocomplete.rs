//! Host-side autocomplete wiring.
//!
//! Upstream `interactive-mode.ts` builds the provider (`createBaseAutocompleteProvider`
//! / `setupAutocompleteProvider`) and upstream's `Editor` owns the request/apply
//! state machine; the port's `pillar-tui::Editor` only renders the dropdown
//! (documented divergence: "the autocomplete provider is host-side"), so this
//! module is the host half:
//!
//! - [`InteractiveAutocomplete::commands`] assembles the command list like
//!   upstream: `BUILTIN_SLASH_COMMANDS`, then prompt templates, extension
//!   commands and (when `enableSkillCommands` is on) `skill:<name>` commands,
//!   each description carrying a `[<source tag>]` prefix.
//! - [`InteractiveAutocomplete::suggestions`] delegates to the ported
//!   `CombinedAutocompleteProvider` and resolves `/command <args>` itself:
//!   upstream hangs `getArgumentCompletions` off the command, and the ported
//!   provider returns `None` there by design.
//! - [`InteractiveAutocomplete::should_request_on_change`] mirrors the trigger
//!   rules the upstream editor applies after an insertion or deletion.
//!
//! divergence: the ported provider is synchronous and file completion uses the
//! plain directory listing (`@`-completion needs the `fd` walk, which is not
//! wired yet — upstream also produces nothing without `fd`), so upstream's 20ms
//! attachment debounce is unnecessary and the host requests immediately.

use std::path::Path;
use std::sync::Arc;

use pillar_ai::types::Model;
use pillar_tui::autocomplete::{
    AutocompleteItem, AutocompleteSuggestions, CombinedAutocompleteProvider, FileEntry,
    SlashCommand, slash_command_argument_prefix,
};
use pillar_tui::editor_autocomplete::{
    DebouncePattern, SlashMenuContext, TriggerPattern, get_best_autocomplete_match_index,
};
use pillar_tui::fuzzy::fuzzy_filter_by;

use crate::core::agent_session_class::AgentSession;
use crate::core::source_info::{SourceInfo, SourceScope};
use crate::modes::interactive::model_search::{ModelSearchItem, model_search_text};
use crate::utils::git::parse_git_url;

/// The commands a slash menu can offer (upstream the provider's command table).
pub fn commands(session: &AgentSession, enable_skill_commands: bool) -> Vec<SlashCommand> {
    let mut commands: Vec<SlashCommand> = crate::core::slash_commands::BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|command| SlashCommand {
            name: command.name.clone(),
            description: Some(command.description.clone()),
            argument_hint: command.argument_hint.clone(),
        })
        .collect();
    let builtin_names: std::collections::BTreeSet<String> = commands
        .iter()
        .map(|command| command.name.clone())
        .collect();

    // Prompt templates (upstream `session.promptTemplates`).
    for template in session.prompt_templates() {
        commands.push(SlashCommand {
            name: template.name.clone(),
            description: prefix_source_tag(
                Some(&template.description),
                Some(&template.source_info),
            ),
            argument_hint: template.argument_hint.clone(),
        });
    }

    // Extension commands (upstream `extensionRunner.getRegisteredCommands()`),
    // skipping names the built-ins already own.
    let runner_arc = session.extension_runner_arc();
    let mut runner = runner_arc.lock().expect("extension runner lock");
    for command in runner.registered_commands() {
        if builtin_names.contains(&command.invocation_name) {
            continue;
        }
        commands.push(SlashCommand {
            name: command.invocation_name.clone(),
            description: prefix_source_tag(Some(&command.description), None),
            argument_hint: None,
        });
    }
    drop(runner);

    // Skill commands (upstream `if (settings.getEnableSkillCommands())`).
    if enable_skill_commands {
        for skill in session.skills() {
            commands.push(SlashCommand {
                name: format!("skill:{}", skill.name),
                description: prefix_source_tag(Some(&skill.description), Some(&skill.source_info)),
                argument_hint: None,
            });
        }
    }
    commands
}

/// Upstream `getAutocompleteSourceTag`: `u` / `p` / `t` for the scope, plus the
/// package origin for `npm:` / git sources.
fn source_tag(source_info: &SourceInfo) -> Option<String> {
    let scope_prefix = match source_info.scope {
        SourceScope::User => "u",
        SourceScope::Project => "p",
        SourceScope::Temporary => "t",
    };
    let source = source_info.source.trim();
    if source.is_empty() {
        return None;
    }
    if source == "auto" || source == "local" || source == "cli" {
        return Some(scope_prefix.to_string());
    }
    if let Some(rest) = source.strip_prefix("npm:") {
        return Some(format!("{scope_prefix}:npm:{rest}"));
    }
    if let Some(git) = parse_git_url(source) {
        let reference = git
            .ref_
            .as_deref()
            .map(|reference| format!("@{reference}"))
            .unwrap_or_default();
        return Some(format!(
            "{scope_prefix}:git:{}/{}{reference}",
            git.host, git.path
        ));
    }
    Some(scope_prefix.to_string())
}

/// Upstream `prefixAutocompleteDescription`.
fn prefix_source_tag(
    description: Option<&str>,
    source_info: Option<&SourceInfo>,
) -> Option<String> {
    let tag = source_info.and_then(source_tag);
    match (tag, description) {
        (None, description) => description.map(str::to_string),
        (Some(tag), None) => Some(format!("[{tag}]")),
        (Some(tag), Some(description)) => Some(format!("[{tag}] {description}")),
    }
}

/// A host-owned argument completer (upstream `SlashCommand.getArgumentCompletions`).
pub type ArgumentCompleter = Arc<dyn Fn(&str) -> Vec<AutocompleteItem> + Send + Sync>;

/// Fuzzy items over `candidates`, mirroring upstream `createFuzzyAutocompleteItems`:
/// the search text leads with what the user is most likely typing.
pub fn fuzzy_items<T, F, G>(
    candidates: &[T],
    prefix: &str,
    text: F,
    item: G,
) -> Vec<AutocompleteItem>
where
    F: Fn(&T) -> String,
    G: Fn(&T) -> AutocompleteItem,
{
    fuzzy_filter_by(candidates, prefix, text)
        .into_iter()
        .map(item)
        .collect()
}

/// The argument completers the port wires (upstream: `model`, `thinking`,
/// `login` — login is not ported, and `/m` is the port's own picker).
pub fn argument_completers(session: &Arc<AgentSession>) -> Vec<(String, ArgumentCompleter)> {
    let mut completers: Vec<(String, ArgumentCompleter)> = Vec::new();

    // `/model <provider>/<id>`, scoped models first (upstream the `model`
    // command's completer).
    let model_session = Arc::clone(session);
    completers.push((
        "model".to_string(),
        Arc::new(move |prefix: &str| {
            let scoped = model_session.scoped_models();
            let models: Vec<Model> = if scoped.is_empty() {
                model_session.model_runtime().get_available_snapshot()
            } else {
                scoped.into_iter().map(|scoped| scoped.model).collect()
            };
            fuzzy_items(
                &models,
                prefix,
                |model| {
                    model_search_text(&ModelSearchItem {
                        id: &model.id,
                        provider: &model.provider,
                        name: Some(&model.name),
                    })
                },
                |model| AutocompleteItem {
                    value: format!("{}/{}", model.provider, model.id),
                    label: model.id.clone(),
                    description: Some(model.provider.clone()),
                },
            )
        }),
    ));

    // `/thinking <level>`.
    let thinking_session = Arc::clone(session);
    completers.push((
        "thinking".to_string(),
        Arc::new(move |prefix: &str| {
            let levels = thinking_session.available_thinking_levels();
            fuzzy_items(
                &levels,
                prefix,
                |level| level.clone(),
                |level| AutocompleteItem {
                    value: level.clone(),
                    label: level.clone(),
                    description: None,
                },
            )
        }),
    ));

    // `/m <provider>` (the 2-column picker's filter; the user's extension
    // completes provider names the same way).
    let picker_session = Arc::clone(session);
    completers.push((
        "m".to_string(),
        Arc::new(move |prefix: &str| {
            let providers = provider_names(&picker_session);
            fuzzy_items(
                &providers,
                prefix,
                |(id, display_name)| format!("{id} {display_name}"),
                |(id, display_name)| AutocompleteItem {
                    value: id.clone(),
                    label: display_name.clone(),
                    description: Some(id.clone()),
                },
            )
        }),
    ));

    completers
}

/// `(provider id, display name)` pairs in scope, sorted by display name
/// (upstream `getProviderDisplayName`, the same grouping the 2-column picker
/// uses).
fn provider_names(session: &AgentSession) -> Vec<(String, String)> {
    let scoped = session.scoped_models();
    let models: Vec<Model> = if scoped.is_empty() {
        session.model_runtime().get_available_snapshot()
    } else {
        scoped.into_iter().map(|scoped| scoped.model).collect()
    };
    let runtime = session.model_runtime();
    let mut names: Vec<(String, String)> = Vec::new();
    for model in models {
        if names.iter().any(|(id, _)| id == &model.provider) {
            continue;
        }
        let display_name = runtime
            .get_provider(&model.provider)
            .map(|provider| provider.name.clone())
            .unwrap_or_else(|| model.provider.clone());
        names.push((model.provider.clone(), display_name));
    }
    names.sort_by_key(|a| a.1.to_lowercase());
    names
}

/// The host autocomplete state (upstream the editor's provider plus its
/// `autocompletePrefix` / selection).
pub struct InteractiveAutocomplete {
    provider: CombinedAutocompleteProvider,
    completers: Vec<(String, ArgumentCompleter)>,
    trigger_pattern: TriggerPattern,
    debounce_pattern: DebouncePattern,
    max_visible: usize,
    /// The prefix of the open menu (upstream `autocompletePrefix`).
    open_prefix: Option<String>,
    /// Whether the open menu came from a forced (Tab / path) request.
    forced: bool,
}

impl InteractiveAutocomplete {
    pub fn new(
        commands: Vec<SlashCommand>,
        completers: Vec<(String, ArgumentCompleter)>,
        cwd: &str,
        max_visible: usize,
    ) -> Self {
        let provider = CombinedAutocompleteProvider::new(commands, Path::new(cwd), None);
        let trigger_characters =
            pillar_tui::editor_autocomplete::register_trigger_characters(&[], &[]);
        Self {
            provider,
            completers,
            trigger_pattern: TriggerPattern::new(&trigger_characters),
            debounce_pattern: DebouncePattern::new(&trigger_characters),
            max_visible: max_visible.clamp(3, 20),
            open_prefix: None,
            forced: false,
        }
    }

    pub fn max_visible(&self) -> usize {
        self.max_visible
    }

    pub fn set_max_visible(&mut self, max_visible: usize) {
        self.max_visible = max_visible.clamp(3, 20);
    }

    pub fn is_open(&self) -> bool {
        self.open_prefix.is_some()
    }

    pub fn open_prefix(&self) -> Option<&str> {
        self.open_prefix.as_deref()
    }

    /// Upstream `applyAutocompleteSuggestions` / `cancelAutocomplete`: remember
    /// (or clear) the prefix of the menu the host is showing.
    pub fn set_open(&mut self, prefix: Option<String>, forced: bool) {
        self.open_prefix = prefix;
        self.forced = forced;
    }

    pub fn is_forced(&self) -> bool {
        self.forced
    }

    /// Whether a text change in this edit context should request suggestions
    /// (upstream `editor.ts`'s insert/delete trigger rules).
    pub fn should_request_on_change(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        if self.is_open() {
            return true;
        }
        let text_before = text_before_cursor(lines, cursor_line, cursor_col);
        // Typing inside a slash command (including its arguments) keeps the
        // menu alive.
        if SlashMenuContext::is_in_slash_command_context(cursor_line, text_before) {
            return true;
        }
        // Symbol triggers (@, #, …) at a token boundary.
        self.trigger_pattern.is_match(text_before)
    }

    /// Upstream the editor's `getSuggestions` call, plus the host's argument
    /// completions.
    pub fn suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        let text_before = text_before_cursor(lines, cursor_line, cursor_col);
        if !force && let Some((name, arguments)) = slash_command_argument_prefix(text_before) {
            // The provider answers `None` for arguments; the host owns them.
            let completer = self
                .completers
                .iter()
                .find(|(command, _)| command == name)?;
            let items = (completer.1)(arguments);
            if items.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions {
                items,
                prefix: arguments.to_string(),
            });
        }
        let mut fd = |_base: &str, _query: &str, _depth: usize| Vec::<FileEntry>::new();
        let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        self.provider
            .get_suggestions(&line_refs, cursor_line, cursor_col, force, &mut fd)
    }

    /// Upstream `applyCompletion` plus the selection of the best match
    /// (`getBestAutocompleteMatchIndex`).
    pub fn apply(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> (Vec<String>, usize, usize) {
        pillar_tui::autocomplete::apply_completion(lines, cursor_line, cursor_col, item, prefix)
    }

    /// The index to select in a fresh menu (upstream `applyAutocompleteSuggestions`).
    pub fn best_match_index(&self, items: &[AutocompleteItem], prefix: &str) -> isize {
        let values: Vec<&str> = items.iter().map(|item| item.value.as_str()).collect();
        get_best_autocomplete_match_index(&values, prefix)
    }

    /// Upstream `getAutocompleteDebounceMs` for the host's request policy.
    /// The ported provider is synchronous, so only the `@`-attachment path
    /// would ever debounce — and that path needs the (unwired) `fd` walk.
    pub fn debounce_ms(
        &self,
        force: bool,
        explicit_tab: bool,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> u64 {
        let text_before = text_before_cursor(lines, cursor_line, cursor_col);
        pillar_tui::editor_autocomplete::get_autocomplete_debounce_ms(
            force,
            explicit_tab,
            &self.debounce_pattern,
            text_before,
        )
    }
}

/// The text of `lines[cursor_line]` up to `cursor_col`.
fn text_before_cursor(lines: &[String], cursor_line: usize, cursor_col: usize) -> &str {
    let line = lines.get(cursor_line).map(String::as_str).unwrap_or("");
    let end = cursor_col.min(line.len());
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_tags_follow_the_scope_and_origin() {
        let info = |source: &str, scope: SourceScope| SourceInfo {
            path: "/tmp/x".to_string(),
            source: source.to_string(),
            scope,
            origin: Default::default(),
            base_dir: None,
        };
        assert_eq!(
            source_tag(&info("auto", SourceScope::User)).as_deref(),
            Some("u")
        );
        assert_eq!(
            source_tag(&info("local", SourceScope::Project)).as_deref(),
            Some("p")
        );
        assert_eq!(
            source_tag(&info("cli", SourceScope::Temporary)).as_deref(),
            Some("t")
        );
        assert_eq!(
            source_tag(&info("npm:pi-lens", SourceScope::User)).as_deref(),
            Some("u:npm:pi-lens")
        );
        assert_eq!(
            source_tag(&info(
                "git:github.com/vi2q/pi-model-picker",
                SourceScope::User
            ))
            .as_deref(),
            Some("u:git:github.com/vi2q/pi-model-picker")
        );
        assert_eq!(source_tag(&info("  ", SourceScope::User)), None);
    }

    #[test]
    fn descriptions_get_the_source_tag_prefix() {
        let info = SourceInfo {
            path: "/tmp/x".to_string(),
            source: "npm:pi-lens".to_string(),
            scope: SourceScope::User,
            origin: Default::default(),
            base_dir: None,
        };
        assert_eq!(
            prefix_source_tag(Some("a skill"), Some(&info)).as_deref(),
            Some("[u:npm:pi-lens] a skill")
        );
        assert_eq!(
            prefix_source_tag(None, Some(&info)).as_deref(),
            Some("[u:npm:pi-lens]")
        );
        assert_eq!(
            prefix_source_tag(Some("plain"), None).as_deref(),
            Some("plain")
        );
        assert_eq!(prefix_source_tag(None, None), None);
    }

    #[test]
    fn suggestions_resolve_host_arguments_and_delegate_commands() {
        let commands = vec![SlashCommand {
            name: "model".to_string(),
            description: Some("switch model".to_string()),
            argument_hint: Some("<model>".to_string()),
        }];
        let completer: ArgumentCompleter = Arc::new(|prefix: &str| {
            ["anthropic/claude-opus-5", "opencode-go/omen-alpha"]
                .iter()
                .filter(|value| value.starts_with(prefix))
                .map(|value| AutocompleteItem {
                    value: value.to_string(),
                    label: value.to_string(),
                    description: None,
                })
                .collect()
        });
        let autocomplete = InteractiveAutocomplete::new(
            commands,
            vec![("model".to_string(), completer)],
            "/tmp",
            5,
        );

        // Command names come from the provider.
        let lines = vec!["/mo".to_string()];
        let suggestions = autocomplete
            .suggestions(&lines, 0, 3, false)
            .expect("command suggestions");
        assert_eq!(suggestions.prefix, "/mo");
        assert_eq!(
            suggestions
                .items
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>(),
            vec!["model"]
        );

        // Arguments come from the host completer; the prefix is the argument.
        let lines = vec!["/model op".to_string()];
        let suggestions = autocomplete
            .suggestions(&lines, 0, 9, false)
            .expect("argument suggestions");
        assert_eq!(suggestions.prefix, "op");
        assert_eq!(
            suggestions
                .items
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>(),
            vec!["opencode-go/omen-alpha"]
        );

        // A command without a completer has no argument suggestions.
        let lines = vec!["/thinking h".to_string()];
        assert!(autocomplete.suggestions(&lines, 0, 11, false).is_none());

        // Applying the argument completion replaces the argument text.
        let argument_lines = vec!["/model op".to_string()];
        let item = suggestions.items[0].clone();
        let (applied, line, col) =
            autocomplete.apply(&argument_lines, 0, "/model op".len(), &item, "op");
        assert_eq!(applied, vec!["/model opencode-go/omen-alpha".to_string()]);
        assert_eq!((line, col), (0, "/model opencode-go/omen-alpha".len()));
    }

    #[test]
    fn requests_follow_the_slash_and_trigger_contexts() {
        let autocomplete = InteractiveAutocomplete::new(Vec::new(), Vec::new(), "/tmp", 5);
        assert!(
            autocomplete.should_request_on_change(&["/thi".to_string()], 0, 4),
            "typing inside a slash command"
        );
        assert!(
            autocomplete.should_request_on_change(&["see @src".to_string()], 0, 8),
            "symbol trigger at a token boundary"
        );
        assert!(
            !autocomplete.should_request_on_change(&["plain word".to_string()], 0, 10),
            "ordinary text"
        );
        assert_eq!(
            autocomplete.best_match_index(
                &[AutocompleteItem {
                    value: "thinking".to_string(),
                    label: "thinking".to_string(),
                    description: None
                }],
                "/th"
            ),
            -1,
            "the slash itself is not part of the value"
        );
    }
}
