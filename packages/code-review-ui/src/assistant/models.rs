//! The model and reasoning effort of the assistant's conversations and the Ask AI cards.
//!
//! * **Per session.** A conversation's or card's choice is its Bcode session's own model and
//!   reasoning selection, so it survives restarts and shows in `bcode tui`; the app reads it back
//!   with the session's model status.
//! * **App default.** The latest choice made anywhere in the app is also saved as the app
//!   default (the review store's assistant settings record). New conversations and new card
//!   sessions get it right after their session is created and before their first message.
//!   Until one is saved, `model` and `effort` in the assistant's configuration seed it; with
//!   neither, nothing is set and Bcode's default applies.
//! * **Effort** cycles the effective model's values from the current one (else the model's
//!   default), wrapping; a value a newly chosen model does not offer is dropped.

use crate::host::{ModelCatalog, ModelOption, SessionModel};
use crate::services::ReviewServices;
use bcode_code_review_models::{
    GetReviewAssistantSettingsRequest, ReviewAssistantSettings, SaveReviewAssistantSettingsRequest,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Sessions whose model the app remembers at most.
const MAX_SESSION_MODELS: usize = 64;

/// What a model or effort choice applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The panel's active conversation (the app default until it has a session).
    Conversation,
    /// One session (an Ask AI card's).
    Session(String),
    /// Only the app default (a question not sent yet).
    AppDefault,
}

/// The models Bcode offers, the app default and the sessions' known models.
#[derive(Debug, Clone, Default)]
pub struct ModelState {
    /// The catalog once listed, or why listing failed.
    pub catalog: Option<Result<ModelCatalog, String>>,
    /// Whether a listing runs.
    pub listing: bool,
    /// The app default, once read.
    pub default: Option<ReviewAssistantSettings>,
    /// Whether the app default was asked for.
    pub default_asked: bool,
    /// Why reading the app default failed; it is read again when the picker opens.
    pub default_error: Option<String>,
    /// Whether a failed listing is tried again on the next poll (the panel was opened).
    pub relist: bool,
    /// Known session models, by session id.
    sessions: BTreeMap<String, SessionModel>,
    /// Order the sessions were learned in, oldest first (to bound the map).
    learned: Vec<String>,
}

impl ModelState {
    /// The known model of `session`.
    #[must_use]
    pub fn session(&self, session: &str) -> Option<&SessionModel> {
        self.sessions.get(session)
    }

    /// Remember `model` as the model of `session`.
    pub fn learn(&mut self, session: String, model: SessionModel) {
        self.learned.retain(|known| *known != session);
        self.learned.push(session.clone());
        self.sessions.insert(session, model);
        while self.learned.len() > MAX_SESSION_MODELS {
            let oldest = self.learned.remove(0);
            self.sessions.remove(&oldest);
        }
    }

    /// The listed catalog, when listing succeeded.
    #[must_use]
    pub fn listed(&self) -> Option<&ModelCatalog> {
        self.catalog
            .as_ref()
            .and_then(|catalog| catalog.as_ref().ok())
    }

    /// Bcode's default model, when known.
    #[must_use]
    pub fn bcode_default(&self) -> Option<&ModelOption> {
        self.listed()?.default.as_ref()?.as_ref().ok()
    }

    /// The model the app default names, as listed (or as recorded when not listed); `None`
    /// when it names none (Bcode's default applies).
    #[must_use]
    pub fn default_model(&self) -> Option<ModelOption> {
        let settings = self.default.as_ref()?;
        let model = settings.model_id.as_deref()?;
        Some(
            self.listed()
                .and_then(|catalog| catalog.find(settings.provider_plugin_id.as_deref(), model))
                .cloned()
                .unwrap_or_else(|| ModelOption {
                    provider: settings.provider_plugin_id.clone(),
                    model_id: model.to_owned(),
                    display_name: model.to_owned(),
                    ..ModelOption::default()
                }),
        )
    }

    /// The model and effort `target` runs with: the session's own when known, else the app
    /// default's (else Bcode's default model).
    #[must_use]
    pub fn effective(&self, session: Option<&str>) -> (Option<ModelOption>, Option<String>) {
        if let Some(model) = session.and_then(|session| self.session(session)) {
            return (Some(model.model.clone()), model.effort.clone());
        }
        let effort = self
            .default
            .as_ref()
            .and_then(|settings| settings.effort.clone());
        let model = self
            .default_model()
            .or_else(|| self.bcode_default().cloned());
        (model, effort)
    }

    /// The model chip's label for `session` (or the app default without one).
    #[must_use]
    pub fn model_label(&self, session: Option<&str>) -> String {
        if matches!(self.catalog, Some(Err(_))) {
            return "⚠ models unavailable".to_owned();
        }
        // A session on Bcode's default model, with no app default chosen, says so.
        let chosen = session
            .and_then(|session| self.session(session))
            .map(|model| model.model.clone())
            .filter(|model| {
                self.default_model().is_some()
                    || !self.bcode_default().is_some_and(|default| {
                        default.is(model.provider.as_deref(), &model.model_id)
                    })
            })
            .or_else(|| self.default_model());
        chosen.map_or_else(
            || {
                self.bcode_default().map_or_else(
                    || "Bcode's model".to_owned(),
                    |model| format!("Bcode's model ({})", label(model)),
                )
            },
            |model| label(&model).to_owned(),
        )
    }

    /// The effort chip's value for `session`, when its model has effort values.
    #[must_use]
    pub fn effort_label(&self, session: Option<&str>) -> Option<String> {
        let (model, effort) = self.effective(session);
        let model = model?;
        if model.effort_values.is_empty() {
            return None;
        }
        effort
            .or_else(|| model.default_effort.clone())
            .or_else(|| Some("default".to_owned()))
    }
}

/// A model's name for chips and rows.
#[must_use]
pub fn label(model: &ModelOption) -> &str {
    if model.display_name.trim().is_empty() {
        &model.model_id
    } else {
        &model.display_name
    }
}

/// The effort after `current` (else after the model's `default`) among `values`, wrapping;
/// the first value when neither is one of them. `None` when there are no values.
#[must_use]
pub fn effort_after(
    values: &[String],
    current: Option<&str>,
    default: Option<&str>,
) -> Option<String> {
    let from = current.or(default);
    let next = from
        .and_then(|from| values.iter().position(|value| value == from))
        .map_or(0, |index| (index + 1) % values.len().max(1));
    values.get(next).cloned()
}

/// The app default saved when `effort` is chosen for a target running `model`: the latest
/// choice names the model the effort was chosen for. The model stays unnamed when the target
/// runs Bcode's default (`bcode_default`) and `current` names no model either, so the default
/// keeps following Bcode's.
#[must_use]
pub fn default_with_effort(
    current: &ReviewAssistantSettings,
    model: &ModelOption,
    bcode_default: Option<&ModelOption>,
    effort: String,
) -> ReviewAssistantSettings {
    let follows_bcode = current.model_id.is_none()
        && bcode_default
            .is_some_and(|default| default.is(model.provider.as_deref(), &model.model_id));
    let (provider_plugin_id, model_id) = if follows_bcode {
        (None, None)
    } else {
        (model.provider.clone(), Some(model.model_id.clone()))
    };
    ReviewAssistantSettings {
        provider_plugin_id,
        model_id,
        effort: Some(effort),
        ..current.clone()
    }
}

/// The effort kept when switching to `model`: `effort` when the model offers it.
#[must_use]
pub fn kept_effort(model: &ModelOption, effort: Option<&str>) -> Option<String> {
    effort
        .filter(|effort| model.effort_values.iter().any(|value| value == effort))
        .map(str::to_owned)
}

/// The app default: the saved record, else the configured seed.
///
/// # Errors
///
/// * The store could not be read.
pub async fn load_default(
    services: &ReviewServices,
    repo: PathBuf,
) -> Result<ReviewAssistantSettings, String> {
    let saved = services
        .call(GetReviewAssistantSettingsRequest { repo_path: repo })
        .await
        .map_err(|error| error.message)?
        .settings;
    Ok(saved.unwrap_or_else(|| services.assistant_config().default_settings()))
}

/// Save `settings` as the app default.
///
/// # Errors
///
/// * The store could not be written.
pub async fn save_default(
    services: &ReviewServices,
    repo: &Path,
    settings: ReviewAssistantSettings,
) -> Result<ReviewAssistantSettings, String> {
    services
        .call(SaveReviewAssistantSettingsRequest {
            repo_path: repo.to_path_buf(),
            settings,
        })
        .await
        .map(|response| response.settings)
        .map_err(|error| error.message)
}

/// Give the new session `session` the app default (read now, so the latest choice applies):
/// its model, then its effort. Nothing is set when the default names neither.
///
/// # Errors
///
/// * Bcode refused the model or the effort (the session keeps Bcode's default).
pub async fn apply_default(
    services: &ReviewServices,
    repo: PathBuf,
    session: &str,
) -> Result<(), String> {
    let settings = load_default(services, repo).await.unwrap_or_default();
    let host = services.host();
    if let Some(model) = settings.model_id.clone() {
        host.set_session_model(
            session.to_owned(),
            settings.provider_plugin_id.clone(),
            model,
        )
        .await
        .map_err(|error| {
            format!(
                "could not use the model {}: {}",
                settings.model_id.unwrap_or_default(),
                error.message
            )
        })?;
    }
    if let Some(effort) = settings.effort.clone() {
        host.set_session_reasoning(session.to_owned(), Some(effort.clone()))
            .await
            .map_err(|error| format!("could not use effort {effort}: {}", error.message))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn effort_cycles_from_the_current_value_or_the_default_and_wraps() {
        let efforts = values(&["low", "medium", "high"]);
        assert_eq!(
            effort_after(&efforts, Some("low"), None).as_deref(),
            Some("medium")
        );
        assert_eq!(
            effort_after(&efforts, Some("high"), None).as_deref(),
            Some("low")
        );
        assert_eq!(
            effort_after(&efforts, None, Some("medium")).as_deref(),
            Some("high")
        );
        assert_eq!(effort_after(&efforts, None, None).as_deref(), Some("low"));
        assert_eq!(
            effort_after(&efforts, Some("max"), None).as_deref(),
            Some("low")
        );
        assert_eq!(effort_after(&[], Some("low"), None), None);
    }

    #[test]
    fn switching_models_keeps_only_an_effort_the_new_model_offers() {
        let model = ModelOption {
            effort_values: values(&["low", "high"]),
            ..ModelOption::default()
        };
        assert_eq!(kept_effort(&model, Some("high")).as_deref(), Some("high"));
        assert_eq!(kept_effort(&model, Some("medium")), None);
        assert_eq!(kept_effort(&ModelOption::default(), Some("high")), None);
    }

    #[test]
    fn an_effort_chosen_for_a_model_saves_that_model_as_the_default() {
        let a = ModelOption {
            model_id: "a".to_owned(),
            effort_values: values(&["low", "high"]),
            ..ModelOption::default()
        };
        let b = ModelOption {
            model_id: "b".to_owned(),
            effort_values: values(&["minimal", "max"]),
            ..ModelOption::default()
        };
        let default_a = ReviewAssistantSettings {
            model_id: Some("a".to_owned()),
            effort: Some("low".to_owned()),
            ..ReviewAssistantSettings::default()
        };
        let saved = default_with_effort(&default_a, &b, Some(&a), "max".to_owned());
        assert_eq!(saved.model_id.as_deref(), Some("b"));
        assert_eq!(saved.effort.as_deref(), Some("max"));
        let unnamed = ReviewAssistantSettings::default();
        let follows = default_with_effort(&unnamed, &a, Some(&a), "high".to_owned());
        assert_eq!(follows.model_id, None, "keeps following Bcode's default");
        assert_eq!(follows.effort.as_deref(), Some("high"));
        let named = default_with_effort(&unnamed, &b, Some(&a), "max".to_owned());
        assert_eq!(named.model_id.as_deref(), Some("b"));
    }

    #[test]
    fn labels_name_the_session_model_the_app_default_or_bcodes_default() {
        let mut state = ModelState::default();
        assert_eq!(state.model_label(None), "Bcode's model");
        state.catalog = Some(Ok(ModelCatalog {
            default: Some(Ok(ModelOption {
                model_id: "fake-echo".to_owned(),
                display_name: "Fake echo".to_owned(),
                effort_values: values(&["low", "high"]),
                default_effort: Some("low".to_owned()),
                ..ModelOption::default()
            })),
            providers: Vec::new(),
        }));
        assert_eq!(state.model_label(None), "Bcode's model (Fake echo)");
        assert_eq!(state.effort_label(None).as_deref(), Some("low"));
        state.default = Some(ReviewAssistantSettings {
            model_id: Some("other".to_owned()),
            ..ReviewAssistantSettings::default()
        });
        assert_eq!(state.model_label(None), "other");
        assert_eq!(
            state.effort_label(None),
            None,
            "an unlisted model offers no effort"
        );
        state.learn(
            "s1".to_owned(),
            SessionModel {
                model: ModelOption {
                    model_id: "mine".to_owned(),
                    display_name: "Mine 模型 🚀".to_owned(),
                    ..ModelOption::default()
                },
                effort: None,
                unavailable: None,
            },
        );
        assert_eq!(state.model_label(Some("s1")), "Mine 模型 🚀");
        state.catalog = Some(Err("daemon down".to_owned()));
        assert_eq!(state.model_label(Some("s1")), "⚠ models unavailable");
    }
}
