//! Port of packages/coding-agent/src/core/agent-session.ts (pi
//! v0.84.3) — the model/thinking/queue mutation decision core:
//! setModel/cycleModel/setThinkingLevel/cycleThinkingLevel,
//! per-model thinking overrides, persisted-default propagation into
//! non-empty scopes, and queue modes.
//!
//! divergences: the extension runner's model_select /
//! thinking_level_select events surface as [`MutationEvent`] values
//! instead of async emits; auth checking and transcript appends are
//! caller callbacks (the runtime owns the session manager and model
//! runtime); settings persistence goes through
//! [`crate::core::settings_manager::SettingsManager`].

use pillar_ai::models::{clamp_thinking_level, get_supported_thinking_levels};
use pillar_ai::types::{Model, ModelThinkingLevel};

use crate::core::settings_manager::SettingsManager;

/// Direction for model cycling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleDirection {
    Forward,
    Backward,
}

/// A scoped model entry (upstream `--models` flag scope).
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedModel {
    pub model: Model,
    /// Explicit thinking level bound to this scoped entry.
    pub thinking_level: Option<String>,
}

impl ScopedModel {
    pub fn key(&self) -> String {
        model_key(&self.model)
    }
}

/// The stable identity key for a model (upstream
/// `provider\0id` for availability matching).
pub fn model_key(model: &Model) -> String {
    format!("{}\0{}", model.provider, model.id)
}

/// Whether two models are the same model (upstream `modelsAreEqual`).
pub fn models_are_equal(left: &Model, right: &Model) -> bool {
    left.provider == right.provider && left.id == right.id
}

/// Outcome of a successful model switch (upstream
/// `ModelCycleResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSwitchOutcome {
    pub model: Model,
    pub thinking_level: String,
    pub is_scoped: bool,
}

/// Emitted mutation events (upstream the extension runner's
/// model_select / thinking_level_select).
#[derive(Debug, Clone, PartialEq)]
pub enum MutationEvent {
    ModelSelect {
        provider: String,
        id: String,
        previous_provider: Option<String>,
        previous_id: Option<String>,
        source: &'static str,
    },
    ThinkingLevelSelect {
        level: String,
        previous_level: String,
    },
}

/// Errors from model mutations (upstream the thrown Errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationError {
    pub message: String,
}

fn mutation_error(message: impl Into<String>) -> MutationError {
    MutationError {
        message: message.into(),
    }
}

/// Auth-check callback (upstream `modelRuntime.checkAuth`).
pub type AuthCheck<'a> = &'a mut dyn FnMut(&str) -> bool;

/// Session transcript append hooks (upstream
/// `sessionManager.appendModelChange` / `appendThinkingLevelChange`).
#[derive(Default)]
pub struct TranscriptAppends {
    pub model_changes: Vec<(String, String)>,
    pub thinking_changes: Vec<String>,
}

/// The mutation decision core (upstream the Model Management and
/// Thinking Level Management sections of AgentSession).
pub struct ModelMutations<'a> {
    pub settings: &'a mut SettingsManager,
    /// Current model.
    pub model: Option<Model>,
    /// Current thinking level.
    pub thinking_level: String,
    /// Scoped models from the --models flag (empty = unrestricted).
    pub scoped_models: Vec<ScopedModel>,
    /// Models available from the runtime (upstream
    /// `modelRuntime.getAvailableSnapshot`).
    pub available_models: Vec<Model>,
    pub appends: TranscriptAppends,
    pub events: Vec<MutationEvent>,
}

impl<'a> ModelMutations<'a> {
    pub fn new(settings: &'a mut SettingsManager) -> Self {
        Self {
            settings,
            model: None,
            thinking_level: crate::core::session_support::DEFAULT_THINKING_LEVEL.to_string(),
            scoped_models: Vec::new(),
            available_models: Vec::new(),
            appends: TranscriptAppends::default(),
            events: Vec::new(),
        }
    }

    /// Upstream `_emitModelSelect`.
    fn emit_model_select(&mut self, next: &Model, previous: Option<&Model>, source: &'static str) {
        if previous.is_some_and(|previous| models_are_equal(previous, next)) {
            return;
        }
        self.events.push(MutationEvent::ModelSelect {
            provider: next.provider.clone(),
            id: next.id.clone(),
            previous_provider: previous.map(|model| model.provider.clone()),
            previous_id: previous.map(|model| model.id.clone()),
            source,
        });
    }

    /// Upstream `getAvailableThinkingLevels`.
    pub fn available_thinking_levels(&self) -> Vec<ModelThinkingLevel> {
        match &self.model {
            None => crate::core::session_support::THINKING_LEVEL_OPTIONS
                .iter()
                .filter_map(|level| parse_level(level))
                .collect(),
            Some(model) => get_supported_thinking_levels(model),
        }
    }

    /// Upstream `supportsThinking`.
    pub fn supports_thinking(&self) -> bool {
        self.model.as_ref().is_some_and(|model| model.reasoning)
    }

    /// Upstream `_getThinkingLevelForModelSwitch`: explicit scoped
    /// level, then per-model override, then the global default, then
    /// the current level.
    fn thinking_level_for_switch(
        &self,
        target: Option<&Model>,
        explicit_level: Option<String>,
    ) -> String {
        if let Some(level) = explicit_level {
            return level;
        }
        if let Some(target) = target {
            if let Some(per_model) = self
                .settings
                .model_thinking_level(&target.provider, &target.id)
            {
                return per_model;
            }
        }
        self.settings
            .default_thinking_level()
            .unwrap_or_else(|| self.thinking_level.clone())
    }

    /// Upstream `_clampThinkingLevel`.
    fn clamp_level(&self, level: &str) -> String {
        match &self.model {
            Some(model) => level_to_string(clamp_thinking_level(
                model,
                parse_level(level).unwrap_or(ModelThinkingLevel::Off),
            )),
            None => crate::core::session_support::DEFAULT_THINKING_LEVEL.to_string(),
        }
    }

    /// Upstream `setModel`: auth gate, model change append, persisted
    /// defaults, thinking-level switch, model_select event.
    pub fn set_model(
        &mut self,
        model: Model,
        persist: bool,
        check_auth: AuthCheck<'_>,
    ) -> Result<(), MutationError> {
        if !check_auth(&model.provider) {
            return Err(mutation_error(format!(
                "No API key for {}/{}",
                model.provider, model.id
            )));
        }
        let previous = self.model.clone();
        let thinking_level = self.thinking_level_for_switch(Some(&model), None);
        self.apply_model(model, persist)?;
        self.set_thinking_level(&thinking_level, false);
        self.emit_model_select(
            &self.model.clone().expect("model set"),
            previous.as_ref(),
            "set",
        );
        Ok(())
    }

    /// Upstream `cycleModel`: scoped scope when present, otherwise the
    /// available snapshot.
    pub fn cycle_model(
        &mut self,
        direction: CycleDirection,
        persist: bool,
        check_auth: AuthCheck<'_>,
    ) -> Result<Option<ModelSwitchOutcome>, MutationError> {
        if !self.scoped_models.is_empty() {
            return self.cycle_scoped_model(direction, persist, check_auth);
        }
        self.cycle_available_model(direction, persist, check_auth)
    }

    /// Upstream `_cycleScopedModel`.
    fn cycle_scoped_model(
        &mut self,
        direction: CycleDirection,
        persist: bool,
        _check_auth: AuthCheck<'_>,
    ) -> Result<Option<ModelSwitchOutcome>, MutationError> {
        let available: std::collections::HashSet<String> =
            self.available_models.iter().map(model_key).collect();
        let scoped: Vec<ScopedModel> = self
            .scoped_models
            .iter()
            .filter(|scoped| available.contains(&scoped.key()))
            .cloned()
            .collect();
        if scoped.len() <= 1 {
            return Ok(None);
        }
        let current = self.model.clone();
        let mut index = current
            .as_ref()
            .and_then(|current| {
                scoped
                    .iter()
                    .position(|scoped| models_are_equal(&scoped.model, current))
            })
            .unwrap_or(0);
        let len = scoped.len();
        index = match direction {
            CycleDirection::Forward => (index + 1) % len,
            CycleDirection::Backward => (index + len - 1) % len,
        };
        let next = &scoped[index];
        let thinking_level =
            self.thinking_level_for_switch(Some(&next.model), next.thinking_level.clone());
        let next_model = next.model.clone();
        let current_model = current.clone();
        self.apply_model(next_model.clone(), persist)?;
        self.set_thinking_level(&thinking_level, false);
        self.emit_model_select(
            &self.model.clone().expect("model set"),
            current_model.as_ref(),
            "cycle",
        );
        Ok(Some(ModelSwitchOutcome {
            model: next_model,
            thinking_level: self.thinking_level.clone(),
            is_scoped: true,
        }))
    }

    /// Upstream `_cycleAvailableModel`.
    fn cycle_available_model(
        &mut self,
        direction: CycleDirection,
        persist: bool,
        _check_auth: AuthCheck<'_>,
    ) -> Result<Option<ModelSwitchOutcome>, MutationError> {
        if self.available_models.len() <= 1 {
            return Ok(None);
        }
        let current = self.model.clone();
        let mut index = current
            .as_ref()
            .and_then(|current| {
                self.available_models
                    .iter()
                    .position(|model| models_are_equal(model, current))
            })
            .unwrap_or(0);
        let len = self.available_models.len();
        index = match direction {
            CycleDirection::Forward => (index + 1) % len,
            CycleDirection::Backward => (index + len - 1) % len,
        };
        let next_model = self.available_models[index].clone();
        let thinking_level = self.thinking_level_for_switch(Some(&next_model), None);
        let current_model = current.clone();
        self.apply_model(next_model.clone(), persist)?;
        self.set_thinking_level(&thinking_level, false);
        self.emit_model_select(
            &self.model.clone().expect("model set"),
            current_model.as_ref(),
            "cycle",
        );
        Ok(Some(ModelSwitchOutcome {
            model: next_model,
            thinking_level: self.thinking_level.clone(),
            is_scoped: false,
        }))
    }

    /// Shared model application (upstream the setModel body before
    /// thinking handling): state, transcript append, persisted
    /// defaults, non-empty scope propagation.
    fn apply_model(&mut self, model: Model, persist: bool) -> Result<(), MutationError> {
        if persist {
            self.settings
                .set_default_model_and_provider(&model.provider, &model.id);
        }
        self.appends
            .model_changes
            .push((model.provider.clone(), model.id.clone()));
        self.model = Some(model.clone());
        if persist {
            self.add_persisted_default_to_non_empty_scope(&model);
        }
        Ok(())
    }

    /// Upstream `_addPersistedDefaultToNonEmptyScope`.
    fn add_persisted_default_to_non_empty_scope(&mut self, model: &Model) {
        if self.scoped_models.is_empty() {
            return;
        }
        if self
            .scoped_models
            .iter()
            .any(|scoped| models_are_equal(&scoped.model, model))
        {
            return;
        }
        self.scoped_models.push(ScopedModel {
            model: model.clone(),
            thinking_level: None,
        });
        let Some(enabled) = self.settings.enabled_models() else {
            return;
        };
        if enabled.is_empty() {
            return;
        }
        let reference = format!("{}/{}", model.provider, model.id);
        if enabled
            .iter()
            .any(|pattern| pattern.to_lowercase() == reference.to_lowercase())
        {
            return;
        }
        let mut enabled = enabled;
        enabled.push(reference);
        self.settings.set_enabled_models(Some(enabled));
    }

    /// Upstream `setThinkingLevel`: clamp to model capabilities,
    /// transcript append only on change, persisted default stores the
    /// *requested* level.
    pub fn set_thinking_level(&mut self, level: &str, persist: bool) {
        let levels = self.available_thinking_levels();
        let parsed = parse_level(level);
        let effective = if parsed.is_some_and(|level| levels.contains(&level)) {
            level.to_string()
        } else {
            self.clamp_level(level)
        };
        let previous = self.thinking_level.clone();
        let is_changing = effective != previous;
        self.thinking_level = effective.clone();
        if persist {
            self.settings.set_default_thinking_level(level);
        }
        if is_changing {
            self.appends.thinking_changes.push(effective.clone());
            self.events.push(MutationEvent::ThinkingLevelSelect {
                level: effective,
                previous_level: previous,
            });
        }
    }

    /// Upstream `cycleThinkingLevel`.
    pub fn cycle_thinking_level(&mut self, persist: bool) -> Option<String> {
        if !self.supports_thinking() {
            return None;
        }
        let levels = self.available_thinking_levels();
        let current = parse_level(&self.thinking_level)?;
        let index = levels.iter().position(|level| *level == current)?;
        let next = levels[(index + 1) % levels.len()];
        let next = level_to_string(next);
        self.set_thinking_level(&next, persist);
        Some(next)
    }
}

fn parse_level(level: &str) -> Option<ModelThinkingLevel> {
    match level {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

fn level_to_string(level: ModelThinkingLevel) -> String {
    match level {
        ModelThinkingLevel::Off => "off".to_string(),
        ModelThinkingLevel::Minimal => "minimal".to_string(),
        ModelThinkingLevel::Low => "low".to_string(),
        ModelThinkingLevel::Medium => "medium".to_string(),
        ModelThinkingLevel::High => "high".to_string(),
        ModelThinkingLevel::Xhigh => "xhigh".to_string(),
        ModelThinkingLevel::Max => "max".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, provider: &str, reasoning: bool) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: "test-api".to_string(),
            provider: provider.to_string(),
            base_url: String::new(),
            reasoning,
            thinking_level_map: None,
            input: vec!["text".to_string()],
            cost: Default::default(),
            context_window: 100_000,
            max_tokens: 10_000,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn auth_ok(_: &str) -> bool {
        true
    }

    fn auth_missing(_: &str) -> bool {
        false
    }

    #[test]
    fn set_model_requires_auth() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        let error = mutations
            .set_model(model("m1", "p1", true), false, &mut auth_missing)
            .unwrap_err();
        assert_eq!(error.message, "No API key for p1/m1");
    }

    #[test]
    fn set_model_appends_and_emits() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        mutations
            .set_model(model("m1", "p1", true), false, &mut auth_ok)
            .unwrap();
        assert_eq!(
            mutations.appends.model_changes,
            vec![("p1".to_string(), "m1".to_string())]
        );
        assert!(
            mutations
                .events
                .iter()
                .any(|event| matches!(event, MutationEvent::ModelSelect { source: "set", .. }))
        );
        // Same model again: no duplicate event.
        mutations
            .set_model(model("m1", "p1", true), false, &mut auth_ok)
            .unwrap();
        assert_eq!(mutations.appends.model_changes.len(), 2);
        assert_eq!(
            mutations
                .events
                .iter()
                .filter(|event| matches!(event, MutationEvent::ModelSelect { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn thinking_level_clamps_to_model_capabilities() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        // Non-reasoning model supports only off; the switch level
        // (default "medium") clamps to off — a change from the
        // initial "medium" appends.
        mutations
            .set_model(model("m1", "p1", false), false, &mut auth_ok)
            .unwrap();
        assert_eq!(mutations.thinking_level, "off");
        mutations.set_thinking_level("high", false);
        assert_eq!(mutations.thinking_level, "off");
        // No extra append when clamped to the same level.
        assert!(
            mutations
                .appends
                .thinking_changes
                .iter()
                .all(|level| level == "off")
        );

        // Reasoning model accepts high.
        mutations
            .set_model(model("r1", "p1", true), false, &mut auth_ok)
            .unwrap();
        mutations.set_thinking_level("high", false);
        assert_eq!(mutations.thinking_level, "high");
        assert!(
            mutations
                .appends
                .thinking_changes
                .contains(&"high".to_string())
        );
        // Setting the same level again does not append.
        mutations.set_thinking_level("high", false);
        assert!(
            mutations
                .appends
                .thinking_changes
                .iter()
                .filter(|level| level.as_str() == "high")
                .count()
                == 1
        );
    }

    #[test]
    fn cycle_thinking_level_wraps() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        mutations
            .set_model(model("r1", "p1", true), false, &mut auth_ok)
            .unwrap();
        mutations.set_thinking_level("off", false);
        // Reasoning models support off → minimal → low → medium → high.
        assert_eq!(
            mutations.cycle_thinking_level(false).as_deref(),
            Some("minimal")
        );
        // Non-reasoning models have no thinking to cycle.
        mutations
            .set_model(model("m1", "p1", false), false, &mut auth_ok)
            .unwrap();
        assert_eq!(mutations.cycle_thinking_level(false), None);
    }

    #[test]
    fn cycle_available_model_wraps_and_persists() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        mutations.available_models = vec![
            model("a", "p", true),
            model("b", "p", true),
            model("c", "p", true),
        ];
        mutations
            .set_model(model("a", "p", true), false, &mut auth_ok)
            .unwrap();
        let outcome = mutations
            .cycle_model(CycleDirection::Forward, true, &mut auth_ok)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.model.id, "b");
        assert!(!outcome.is_scoped);
        // Persisted defaults recorded.
        assert_eq!(
            mutations.settings.default_model_and_provider(),
            Some(("p".to_string(), "b".to_string()))
        );
        // Backward from b returns to a.
        let outcome = mutations
            .cycle_model(CycleDirection::Backward, false, &mut auth_ok)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.model.id, "a");
    }

    #[test]
    fn cycle_available_model_needs_two_models() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        mutations.available_models = vec![model("a", "p", true)];
        assert_eq!(
            mutations
                .cycle_model(CycleDirection::Forward, false, &mut auth_ok)
                .unwrap(),
            None
        );
    }

    #[test]
    fn cycle_scoped_model_filters_to_available() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        mutations.available_models = vec![model("a", "p", true), model("b", "p", true)];
        mutations.scoped_models = vec![
            ScopedModel {
                model: model("a", "p", true),
                thinking_level: Some("high".to_string()),
            },
            ScopedModel {
                model: model("b", "p", true),
                thinking_level: None,
            },
            ScopedModel {
                model: model("gone", "p", true),
                thinking_level: None,
            },
        ];
        mutations
            .set_model(model("a", "p", true), false, &mut auth_ok)
            .unwrap();
        let outcome = mutations
            .cycle_model(CycleDirection::Forward, false, &mut auth_ok)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.model.id, "b");
        assert!(outcome.is_scoped);
    }

    #[test]
    fn per_model_thinking_level_takes_priority_on_switch() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        settings.set_model_thinking_level("p2", "x", "high");
        let mut mutations = ModelMutations::new(&mut settings);
        mutations
            .set_model(model("m", "p1", true), false, &mut auth_ok)
            .unwrap();
        mutations.set_thinking_level("low", false);
        // Switching to the model with a per-model default applies it.
        mutations
            .set_model(model("x", "p2", true), false, &mut auth_ok)
            .unwrap();
        assert_eq!(mutations.thinking_level, "high");
    }

    #[test]
    fn persisted_default_propagates_to_non_empty_scope() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        mutations.scoped_models = vec![ScopedModel {
            model: model("a", "p", true),
            thinking_level: None,
        }];
        mutations
            .set_model(model("b", "p", true), true, &mut auth_ok)
            .unwrap();
        // b was added to the scope; enabled models gained the reference.
        assert_eq!(mutations.scoped_models.len(), 2);
        // No enabled-models list configured: the reference is not
        // appended (upstream getEnabledModels() undefined -> return).
        assert_eq!(mutations.settings.enabled_models(), None);
        // Already-scoped models do not duplicate or extend.
        mutations
            .set_model(model("a", "p", true), true, &mut auth_ok)
            .unwrap();
        assert_eq!(mutations.scoped_models.len(), 2);
    }

    #[test]
    fn persisted_default_skipped_in_empty_scope() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        let mut mutations = ModelMutations::new(&mut settings);
        mutations
            .set_model(model("b", "p", true), true, &mut auth_ok)
            .unwrap();
        assert!(mutations.scoped_models.is_empty());
        assert!(mutations.settings.enabled_models().is_none());
        // Default model/provider still persisted.
        assert_eq!(
            mutations.settings.default_model_and_provider(),
            Some(("p".to_string(), "b".to_string()))
        );
    }

    #[test]
    fn enabled_model_reference_deduplication_is_case_insensitive() {
        let mut settings = SettingsManager::in_memory(
            serde_json::json!({}),
            crate::core::settings_manager::SettingsManagerCreateOptions::default(),
        );
        settings.set_enabled_models(Some(vec!["P/B".to_string()]));
        let mut mutations = ModelMutations::new(&mut settings);
        mutations.scoped_models = vec![ScopedModel {
            model: model("a", "p", true),
            thinking_level: None,
        }];
        mutations
            .set_model(model("b", "p", true), true, &mut auth_ok)
            .unwrap();
        assert_eq!(
            mutations.settings.enabled_models(),
            Some(vec!["P/B".to_string()])
        );
    }
}
