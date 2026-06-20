//! Effective model ignore helpers.

use bcode_config::{EffectiveModelIgnoreRules, ModelIgnoreSource};
use bcode_model::{ModelInfo, ModelVisibility, ModelVisibilitySource};

/// Mark models ignored by the effective ignore rules for a provider.
pub fn apply_model_ignores(models: &mut [ModelInfo], rules: &EffectiveModelIgnoreRules) {
    for model in models {
        if let Some(matched) = rules.is_ignored(&model.model_id) {
            model.visibility = ModelVisibility::Ignored {
                source: match matched.source {
                    ModelIgnoreSource::Config => ModelVisibilitySource::Config,
                    ModelIgnoreSource::State => ModelVisibilitySource::State,
                    ModelIgnoreSource::Both => ModelVisibilitySource::Both,
                },
                rule: matched.rule,
            };
        }
    }
}

/// Return whether a model should be hidden from normal pickers.
#[must_use]
pub const fn is_ignored(model: &ModelInfo) -> bool {
    matches!(model.visibility, ModelVisibility::Ignored { .. })
}
