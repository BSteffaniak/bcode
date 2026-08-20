//! Transport-neutral application operations for session lifecycle behavior.

use super::ServerState;
use bcode_session_models::SessionSummary;
use std::path::PathBuf;

/// Application-level failure while creating a session.
#[derive(Debug, thiserror::Error)]
pub enum CreateSessionError {
    /// Canonical session working directories must be absolute.
    #[error("session working directory must be absolute")]
    WorkingDirectoryMustBeAbsolute,
    /// Canonical session creation failed.
    #[error(transparent)]
    Session(#[from] bcode_session::SessionError),
}

/// Application-level failure while activating a session skill.
#[derive(Debug, thiserror::Error)]
pub enum ActivateSkillError {
    /// Skills are disabled for this session.
    #[error("skills are disabled")]
    Disabled,
    /// The requested skill is unavailable.
    #[error("unknown skill: {0}")]
    Unknown(bcode_skill_models::SkillId),
    /// Skill model policy prevented activation.
    #[error("{message}")]
    ModelPolicy {
        /// Stable public error code.
        code: String,
        /// Secret-safe public message.
        message: String,
    },
    /// Canonical session mutation failed.
    #[error(transparent)]
    Session(#[from] bcode_session::SessionError),
}

/// Activate one session skill through canonical policy and event paths.
pub async fn activate_skill(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    skill_id: bcode_skill_models::SkillId,
) -> Result<(), ActivateSkillError> {
    let Some(registry) = state.session_skills(session_id).await else {
        return Err(ActivateSkillError::Disabled);
    };
    let Some(summary) = registry.summary(&skill_id).cloned() else {
        return Err(ActivateSkillError::Unknown(skill_id));
    };
    super::apply_skill_model_policy(state, session_id, &skill_id)
        .await
        .map_err(|error| ActivateSkillError::ModelPolicy {
            code: error.code,
            message: error.message,
        })?;
    state
        .active_skills
        .lock()
        .await
        .entry(session_id)
        .or_default()
        .insert(skill_id.clone());
    let event = state
        .sessions
        .append_event(
            session_id,
            bcode_session_models::SessionEventKind::SkillActivated {
                skill_id,
                source: Some(summary.source),
                mode: bcode_skill_models::SkillActivationMode::Explicit,
                activated_at_ms: super::current_time_ms(),
            },
        )
        .await?;
    super::publish_session_event(state, &event).await;
    Ok(())
}

/// Return active skill contexts for one session without transport framing.
pub async fn active_skills(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
) -> Vec<bcode_skill_models::SkillContextResponse> {
    super::active_skill_contexts(state, session_id).await
}

/// Deactivate one session skill and restore any plugin-owned model override.
pub async fn deactivate_skill(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    skill_id: bcode_skill_models::SkillId,
) -> Result<(), super::ServerError> {
    if let Some(skills) = state.active_skills.lock().await.get_mut(&session_id) {
        skills.remove(&skill_id);
    }
    super::restore_skill_model_override(state, session_id, &skill_id).await?;
    let event = state
        .sessions
        .append_event(
            session_id,
            bcode_session_models::SessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms: super::current_time_ms(),
            },
        )
        .await?;
    super::publish_session_event(state, &event).await;
    Ok(())
}

/// Persist provider-neutral session reasoning selections when they change.
pub async fn set_reasoning(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    effort: Option<String>,
    summary: Option<String>,
) -> Result<bool, bcode_session::SessionError> {
    let event = super::set_session_reasoning_if_changed(state, session_id, effort, summary).await?;
    let changed = event.is_some();
    if let Some(event) = event {
        super::publish_session_event(state, &event).await;
    }
    Ok(changed)
}

/// Application-level failure while selecting a session model.
#[derive(Debug, thiserror::Error)]
pub enum SetModelError {
    /// An active skill requires retaining its model selection.
    #[error("skill {0} declares a required model; deactivate it before changing the session model")]
    SkillRequiredModelActive(String),
    /// Canonical session mutation failed.
    #[error(transparent)]
    Session(#[from] bcode_session::SessionError),
}

/// Persist one explicit session model selection.
pub async fn set_model(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    provider_plugin_id: Option<String>,
    model_id: String,
) -> Result<(), SetModelError> {
    if let Some(blocking_skill_id) = super::required_model_active_skill(state, session_id).await
        && super::required_model_override_behavior(
            &state.session_config(session_id).await.skills.model_policy,
            &blocking_skill_id,
        ) == bcode_config::SkillRequiredModelOverride::Deny
    {
        return Err(SetModelError::SkillRequiredModelActive(
            blocking_skill_id.to_string(),
        ));
    }
    let provider = provider_plugin_id.unwrap_or_else(|| "<auto>".to_owned());
    let event = state
        .sessions
        .append_model_changed(
            session_id,
            provider.clone(),
            model_id.clone(),
            bcode_session_models::ModelSelectionSource::UserExplicit,
        )
        .await?;
    let selection = super::SessionModelSelection {
        provider_plugin_id: super::provider_to_selection(&provider),
        requested_model_id: None,
        model_id: super::model_to_selection(&model_id),
        thinking_level: None,
        reasoning_effort: state.selected_reasoning.effort.clone(),
        reasoning_summary: state.selected_reasoning.summary.clone(),
        reasoning_capabilities: state.selected_reasoning_capabilities.clone(),
        provider_context: state.selected_provider_context.clone(),
    };
    state
        .session_model_selections
        .lock()
        .await
        .insert(session_id, selection);
    state
        .session_model_selection_origins
        .lock()
        .await
        .insert(session_id, super::SessionModelSelectionOrigin::User);
    super::publish_session_event(state, &event).await;
    Ok(())
}

/// Application-level failure while selecting a session agent.
#[derive(Debug, thiserror::Error)]
pub enum SetAgentError {
    /// The requested prompt profile is not registered.
    #[error("unknown prompt profile: {0}")]
    UnknownAgent(String),
    /// Canonical session mutation failed.
    #[error(transparent)]
    Session(#[from] bcode_session::SessionError),
}

/// Resolve and persist one session's active agent selection.
pub async fn set_agent(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    agent_id: String,
) -> Result<(), SetAgentError> {
    let Some(resolved_agent_id) = super::resolve_agent_id(state, &agent_id).await else {
        return Err(SetAgentError::UnknownAgent(agent_id));
    };
    state
        .sessions
        .set_current_agent(session_id, resolved_agent_id.clone())
        .await?;
    state
        .session_agent_selections
        .lock()
        .await
        .insert(session_id, resolved_agent_id);
    Ok(())
}

/// Application-level failure while changing a session working directory.
#[derive(Debug, thiserror::Error)]
pub enum ChangeWorkingDirectoryError {
    /// Canonical session working directories must be absolute.
    #[error("session working directory must be absolute")]
    MustBeAbsolute,
    /// The path cannot be inspected or canonicalized.
    #[error("{message}")]
    Unavailable {
        /// Stable public error code.
        code: &'static str,
        /// Secret-safe public message.
        message: String,
    },
    /// Active model work prevents changing session identity context.
    #[error("session has an active model turn: {0}")]
    Busy(bcode_session_models::SessionId),
    /// Canonical session mutation failed.
    #[error(transparent)]
    Session(#[from] bcode_session::SessionError),
}

impl ChangeWorkingDirectoryError {
    /// Return the stable public error code for this failure.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MustBeAbsolute => "session_working_directory_must_be_absolute",
            Self::Unavailable { code, .. } => code,
            Self::Busy(_) => "session_busy",
            Self::Session(_) => "session_cwd_change_failed",
        }
    }
}

/// Change one idle session's canonical working directory.
pub async fn change_working_directory(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    working_directory: PathBuf,
) -> Result<(SessionSummary, bool), ChangeWorkingDirectoryError> {
    if !working_directory.is_absolute() {
        return Err(ChangeWorkingDirectoryError::MustBeAbsolute);
    }
    let metadata = tokio::fs::metadata(&working_directory)
        .await
        .map_err(|error| ChangeWorkingDirectoryError::Unavailable {
            code: "session_working_directory_unavailable",
            message: format!("session working directory is not accessible: {error}"),
        })?;
    if !metadata.is_dir() {
        return Err(ChangeWorkingDirectoryError::Unavailable {
            code: "session_working_directory_not_directory",
            message: "session working directory path is not a directory".to_owned(),
        });
    }
    let working_directory = tokio::fs::canonicalize(working_directory)
        .await
        .map_err(|error| ChangeWorkingDirectoryError::Unavailable {
            code: "session_working_directory_unavailable",
            message: format!("session working directory cannot be resolved: {error}"),
        })?;
    if state.session_has_active_turn(session_id).await {
        return Err(ChangeWorkingDirectoryError::Busy(session_id));
    }
    let event = state
        .sessions
        .change_session_working_directory(session_id, working_directory)
        .await?;
    let changed = event.is_some();
    if let Some(event) = event {
        super::publish_session_event(state, &event).await;
    }
    let session = state.sessions.session_summary(session_id).await?;
    if changed {
        state
            .session_catalog
            .upsert_native_session(session.clone())
            .await;
    }
    Ok((session, changed))
}

/// Application-level failure while deleting a session.
#[derive(Debug, thiserror::Error)]
pub enum DeleteSessionError {
    /// Active model work prevents canonical deletion.
    #[error("session has an active model turn: {0}")]
    Busy(bcode_session_models::SessionId),
    /// Canonical session deletion failed.
    #[error(transparent)]
    Session(#[from] bcode_session::SessionError),
}

/// Delete one idle canonical session and dispose its derived state.
pub async fn delete(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
) -> Result<SessionSummary, DeleteSessionError> {
    if state.session_has_active_turn(session_id).await {
        return Err(DeleteSessionError::Busy(session_id));
    }
    let session = state.sessions.delete_session(session_id).await?;
    let generation = super::session_search::generation_fingerprint(&session);
    super::session_search::remove_session_from_providers(state, session_id, Some(generation)).await;
    state
        .session_model_selections
        .lock()
        .await
        .remove(&session_id);
    state
        .session_agent_selections
        .lock()
        .await
        .remove(&session_id);
    state
        .session_catalog
        .remove_native_session(session_id)
        .await;
    if let Err(error) =
        super::remove_session_artifact_dir(&super::default_session_artifact_dir(session_id))
    {
        tracing::warn!(
            session_id = %session_id,
            error = %error,
            "session deleted but its retained artifacts could not be removed"
        );
    }
    Ok(session)
}

/// Rename one canonical session and refresh its derived catalog entry.
pub async fn rename(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    name: Option<String>,
) -> Result<SessionSummary, bcode_session::SessionError> {
    let event = state.sessions.rename_session(session_id, name).await?;
    super::publish_session_event(state, &event).await;
    let session = state.sessions.session_summary(session_id).await?;
    state
        .session_catalog
        .upsert_native_session(session.clone())
        .await;
    Ok(session)
}

/// Create and publish one canonical session without transport framing.
pub async fn create(
    state: &ServerState,
    name: Option<String>,
    working_directory: PathBuf,
) -> Result<SessionSummary, CreateSessionError> {
    if !working_directory.is_absolute() {
        return Err(CreateSessionError::WorkingDirectoryMustBeAbsolute);
    }
    let session = state
        .sessions
        .create_session(name, working_directory)
        .await?;
    state
        .session_catalog
        .upsert_native_session(session.clone())
        .await;
    if let Ok(mut events) = state
        .sessions
        .session_events_range(session.id, 0, 0, 1)
        .await
        && let Some(event) = events.pop()
    {
        super::publish_session_event(state, &event).await;
    }
    Ok(session)
}
