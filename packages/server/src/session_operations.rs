//! Transport-neutral application operations for session lifecycle behavior.

use super::{ServerState, session_catalog::SessionCatalogSnapshot};
use bcode_ipc::SessionCatalogStatus;
use bcode_session_models::SessionSummary;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Return the coherent bounded session catalog for one working directory.
///
/// This preserves the existing initial-load coordination while keeping response framing out of
/// the application operation. Catalog discovery remains best-effort, bounded, and non-mutating.
pub async fn list(
    state: &Arc<ServerState>,
    working_directory: &Path,
) -> Result<SessionCatalogSnapshot, bcode_session::SessionStoreError> {
    let snapshot = state
        .session_catalog
        .snapshot(state, working_directory)
        .await;
    if !matches!(snapshot.status, SessionCatalogStatus::Loading) {
        return Ok(snapshot);
    }

    state.sessions.wait_catalog_loaded().await?;
    state.session_catalog.refresh_native_now(state).await;
    Ok(state
        .session_catalog
        .snapshot(state, working_directory)
        .await)
}

/// Refresh selected session catalog sources without transport framing.
///
/// Refresh remains an explicit application operation; it does not run during ordinary bounded
/// catalog reads.
pub async fn refresh(
    state: &Arc<ServerState>,
    working_directory: &Path,
    sources: Option<&[String]>,
) -> SessionCatalogSnapshot {
    state
        .session_catalog
        .refresh(state, working_directory, sources)
        .await
}

/// Return the current bounded skill inventory without transport framing.
#[must_use]
pub fn list_skills(state: &ServerState) -> bcode_skill_models::SkillList {
    state.skills.as_ref().map_or_else(
        || bcode_skill_models::SkillList {
            skills: Vec::new(),
            diagnostics: Vec::new(),
        },
        bcode_skill::SkillRegistry::list,
    )
}

/// Application-level failure while describing a skill.
#[derive(Debug, thiserror::Error)]
pub enum DescribeSkillError {
    /// Skills are disabled for this server.
    #[error("skills are disabled")]
    Disabled,
    /// The skill registry could not describe the requested skill.
    #[error(transparent)]
    Registry(#[from] bcode_skill::SkillRegistryError),
}

impl DescribeSkillError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "skills_disabled",
            Self::Registry(_) => "skill_describe_failed",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::Disabled => "skills are disabled",
            Self::Registry(_) => "skill is unavailable",
        }
    }
}

/// Return one skill manifest without transport framing.
pub fn describe_skill(
    state: &ServerState,
    skill_id: &bcode_skill_models::SkillId,
) -> Result<bcode_skill_models::SkillManifest, DescribeSkillError> {
    let Some(registry) = &state.skills else {
        return Err(DescribeSkillError::Disabled);
    };
    Ok(registry.describe(skill_id)?)
}

/// Return the available agent profiles for one client's effective configuration.
pub async fn list_agents(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
) -> Vec<bcode_agent_profile::AgentInfo> {
    let config = state
        .client_runtime_context(client_id)
        .await
        .and_then(|context| context.effective_config_toml)
        .and_then(|contents| bcode_config::decode_effective_config(&contents).ok());
    super::list_profiles(state, config.as_ref()).await
}

/// Return effective agent policy status without transport framing.
pub async fn agent_policy_status(state: &ServerState) -> bcode_agent_profile::PolicyStatusResponse {
    super::load_agent_policy_status(state)
        .await
        .unwrap_or_else(|| bcode_agent_profile::PolicyStatusResponse {
            source: "prompt profile provider not loaded".to_string(),
            using_default: true,
            build_enabled_tools: Vec::new(),
            plan_enabled_tools: Vec::new(),
            diagnostics: vec!["prompt profile provider not loaded".to_string()],
        })
}

/// Return one session's normalized model status for a client's runtime context.
pub async fn model_status(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
    session_id: bcode_session_models::SessionId,
) -> bcode_ipc::SessionModelStatus {
    let selection = super::session_model_selection_with_runtime_context(
        state,
        session_id,
        state.client_runtime_context(client_id).await,
    )
    .await;
    let config = state.session_config(session_id).await;
    super::model_status_for_selection(state, selection, Some(session_id), &config).await
}

/// Return normalized default model status for a client's runtime context.
pub async fn default_model_status(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
) -> bcode_ipc::SessionModelStatus {
    let runtime_context = state.client_runtime_context(client_id).await;
    let config = runtime_context
        .as_ref()
        .and_then(|context| context.effective_config_toml.as_deref())
        .and_then(|contents| bcode_config::decode_effective_config(contents).ok())
        .unwrap_or_else(|| state.startup_config.clone());
    let selection = super::default_model_selection_with_runtime_context(state, runtime_context);
    super::model_status_for_selection(state, selection, None, &config).await
}

/// Public failure while listing models for a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListModelsError {
    /// The selected provider could not return a trustworthy model list.
    ProviderUnavailable,
}

impl ListModelsError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProviderUnavailable => "model_list_failed",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::ProviderUnavailable => "model list is unavailable",
        }
    }
}

/// Return the user-visible model list for a client's selected provider.
pub async fn list_models(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
    provider_plugin_id: Option<String>,
) -> Result<(Option<String>, bcode_model::ModelList), ListModelsError> {
    let runtime_context = state
        .client_runtime_contexts
        .try_lock()
        .ok()
        .and_then(|contexts| contexts.get(&client_id).cloned());
    let selected_provider_plugin_id = provider_plugin_id.or_else(|| {
        runtime_context
            .as_ref()
            .and_then(|context| context.selected_provider_plugin_id.clone())
    });
    let mut models = super::resolved_provider_models_view(
        state,
        selected_provider_plugin_id.clone(),
        bcode_model::ModelListRequest {
            provider_context: runtime_context
                .map_or_else(bcode_model::ProviderRequestContext::default, |context| {
                    context.provider_context
                }),
            selected_model_id: None,
        },
        bcode_model_catalog::ModelListView::UserVisible,
    )
    .await
    .map_err(|_| ListModelsError::ProviderUnavailable)?;
    let provider_for_ignores = selected_provider_plugin_id
        .as_deref()
        .unwrap_or("bcode.openai-compatible");
    if let Ok(rules) = bcode_config::effective_model_ignore_rules(provider_for_ignores) {
        super::model_ignores::apply_model_ignores(&mut models.models, &rules);
    }
    Ok((selected_provider_plugin_id, models))
}

/// Explicitly release all process ownership retained for one canonical session.
pub async fn release_ownership(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
) -> Result<bcode_ipc::SessionOwnershipReleaseOutcome, super::ServerError> {
    super::explicit_session_ownership_release_outcome(state, session_id).await
}

/// Application-level scope for persisted composer drafts.
pub enum ComposerDraftScope {
    /// Draft associated with one canonical session.
    Session(bcode_session_models::SessionId),
    /// Draft associated with a not-yet-created session in one launch directory.
    DraftSession(PathBuf),
}

/// Persist a composer draft without transport framing.
pub async fn set_composer_draft(
    state: &ServerState,
    scope: ComposerDraftScope,
    text: String,
) -> Result<(), bcode_session::SessionError> {
    match scope {
        ComposerDraftScope::Session(session_id) => {
            state
                .sessions
                .set_session_composer_draft(session_id, text)
                .await?;
        }
        ComposerDraftScope::DraftSession(launch_working_directory) => {
            state
                .sessions
                .set_draft_session_composer_draft(launch_working_directory, text)
                .await?;
        }
    }
    Ok(())
}

/// Return a persisted composer draft without transport framing.
pub async fn composer_draft(
    state: &ServerState,
    scope: ComposerDraftScope,
) -> Result<Option<String>, bcode_session::SessionError> {
    match scope {
        ComposerDraftScope::Session(session_id) => {
            state.sessions.session_composer_draft(session_id).await
        }
        ComposerDraftScope::DraftSession(launch_working_directory) => {
            state
                .sessions
                .draft_session_composer_draft(launch_working_directory)
                .await
        }
    }
}

/// Return one session's bounded derivation snapshot.
pub async fn derivation_snapshot(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
) -> Result<bcode_session_models::SessionDerivationSourceSnapshot, bcode_session::SessionError> {
    state.sessions.session_derivation_snapshot(session_id).await
}

/// Return bounded derivation prompt candidates for one session.
pub async fn derivation_prompts(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    query: bcode_session_models::SessionDerivationPromptQuery,
) -> Result<bcode_session_models::SessionDerivationPromptPage, bcode_session::SessionError> {
    state
        .sessions
        .session_derivation_prompt_candidates(session_id, query)
        .await
}

/// Execute one retry-safe canonical session derivation.
pub async fn derive(
    state: &ServerState,
    request: bcode_session_models::SessionDerivationRequest,
) -> Result<bcode_session_models::SessionDerivationTerminalOutcome, bcode_session::SessionError> {
    state.sessions.derive_session(request).await
}

/// Return status for one canonical derivation operation.
pub async fn derivation_status(
    state: &ServerState,
    operation_id: bcode_session_models::SessionDerivationOperationId,
) -> Result<bcode_session_models::SessionDerivationOperationSnapshot, bcode_session::SessionError> {
    state.sessions.session_derivation_status(operation_id).await
}

/// Request cancellation of one canonical session-derivation operation.
pub async fn cancel_derivation(
    state: &ServerState,
    operation_id: bcode_session_models::SessionDerivationOperationId,
) -> bool {
    state.sessions.cancel_session_derivation(operation_id).await
}

/// Application-level failure while reading session history for one client.
#[derive(Debug, thiserror::Error)]
pub enum ReadHistoryError {
    /// The active session belongs to another artifact namespace.
    #[error("active session uses incompatible artifact namespace {0}")]
    IncompatibleActiveNamespace(String),
    /// Canonical bounded history read failed.
    #[error(transparent)]
    Session(#[from] bcode_session::SessionError),
}

impl ReadHistoryError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::IncompatibleActiveNamespace(_) => "session_incompatible_active_client",
            Self::Session(_) => "session_unavailable",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::IncompatibleActiveNamespace(_) => {
                "session is active for an incompatible client; reconnect with a matching client or wait until the session is inactive"
            }
            Self::Session(_) => "session is unavailable",
        }
    }
}

/// Return complete canonical history for an explicit export/debug request.
///
/// This operation is intentionally separate from normal bounded history and inspection reads.
pub async fn complete_history(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
    session_id: bcode_session_models::SessionId,
) -> Result<Vec<bcode_session_models::SessionEvent>, ReadHistoryError> {
    if let Some(namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return Err(ReadHistoryError::IncompatibleActiveNamespace(namespace));
    }
    Ok(state.sessions.session_history(session_id).await?)
}

/// Return one bounded semantic session-inspection page without transport framing.
pub async fn inspect(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
    session_id: bcode_session_models::SessionId,
    query: bcode_session_models::SessionInspectionQuery,
) -> Result<bcode_session_models::SessionInspectionPage, ReadHistoryError> {
    if let Some(namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return Err(ReadHistoryError::IncompatibleActiveNamespace(namespace));
    }
    Ok(state
        .sessions
        .session_inspection_page(session_id, query)
        .await?)
}

/// Return one bounded session history page without transport framing.
pub async fn history_page(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
    session_id: bcode_session_models::SessionId,
    query: bcode_session_models::SessionHistoryQuery,
) -> Result<bcode_session_models::SessionHistoryPage, ReadHistoryError> {
    if let Some(namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return Err(ReadHistoryError::IncompatibleActiveNamespace(namespace));
    }
    Ok(state
        .sessions
        .session_history_page(session_id, query)
        .await?)
}

/// Return one bounded history window around a sequence without transport framing.
pub async fn history_around(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
    session_id: bcode_session_models::SessionId,
    query: bcode_session_models::SessionHistoryAroundQuery,
) -> Result<bcode_session_models::SessionHistoryWindow, ReadHistoryError> {
    if let Some(namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return Err(ReadHistoryError::IncompatibleActiveNamespace(namespace));
    }
    Ok(state
        .sessions
        .session_history_around(session_id, query)
        .await?)
}

/// Request canonical turn cancellation through the queued command path.
pub async fn cancel_turn(
    state: &Arc<ServerState>,
    session_id: bcode_session_models::SessionId,
    clear_queue: bool,
    client_id: bcode_session_models::ClientId,
) -> Result<bool, super::ServerError> {
    super::enqueue_cancel_turn_command(state, session_id, clear_queue, Some(client_id)).await
}

/// Application failure while appending a bounded presentation-only note.
#[derive(Debug, thiserror::Error)]
pub enum AppendPresentationNoteError {
    #[error("presentation note fields are empty or exceed their bounded limits")]
    Invalid,
    #[error(transparent)]
    Session(#[from] bcode_session::SessionError),
}

/// Append one bounded plugin-owned presentation note to canonical history.
pub async fn append_presentation_note(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    source_id: String,
    note_id: String,
    text: String,
    format: bcode_command::CommandTextFormat,
) -> Result<(), AppendPresentationNoteError> {
    let invalid = source_id.trim().is_empty()
        || source_id.len() > super::MAX_PRESENTATION_NOTE_SOURCE_ID_BYTES
        || note_id.trim().is_empty()
        || note_id.len() > super::MAX_PRESENTATION_NOTE_ID_BYTES
        || text.trim().is_empty()
        || text.len() > super::MAX_PRESENTATION_NOTE_TEXT_BYTES;
    if invalid {
        return Err(AppendPresentationNoteError::Invalid);
    }
    let metadata = std::collections::BTreeMap::from([
        (
            "format".to_owned(),
            serde_json::Value::String(
                match format {
                    bcode_command::CommandTextFormat::PlainText => "plain_text",
                    bcode_command::CommandTextFormat::Markdown => "markdown",
                    bcode_command::CommandTextFormat::Json => "json",
                }
                .to_owned(),
            ),
        ),
        (
            "presentation_only".to_owned(),
            serde_json::Value::Bool(true),
        ),
    ]);
    let event = state
        .sessions
        .append_event(
            session_id,
            bcode_session_models::SessionEventKind::PluginStatusNote {
                plugin_id: source_id,
                note_id,
                text,
                metadata,
            },
        )
        .await?;
    super::publish_session_event(state, &event).await;
    Ok(())
}

/// Application result of one explicit context-compaction request.
pub enum CompactResult {
    Compacted(String),
    Noop(String),
}

/// Application failure while explicitly compacting session context.
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("active session uses incompatible artifact namespace {0}")]
    IncompatibleActiveNamespace(String),
    #[error(transparent)]
    Compaction(#[from] super::CompactionError),
    #[error(transparent)]
    Server(#[from] super::ServerError),
}

/// Enqueue explicit compaction without waiting for provider execution.
pub async fn compact(
    state: &Arc<ServerState>,
    client_id: bcode_session_models::ClientId,
    session_id: bcode_session_models::SessionId,
) -> Result<tokio::sync::oneshot::Receiver<Result<String, super::CompactionError>>, CompactError> {
    if let Some(namespace) = state
        .active_session_namespace_mismatch(session_id, client_id)
        .await
    {
        return Err(CompactError::IncompatibleActiveNamespace(namespace));
    }
    let selection = super::session_model_selection_with_runtime_context(
        state,
        session_id,
        state.client_runtime_context(client_id).await,
    )
    .await;
    Ok(super::enqueue_compact_session_command(state, session_id, client_id, selection).await?)
}

/// Await the terminal result of an accepted compaction.
pub async fn complete_compaction(
    completion: tokio::sync::oneshot::Receiver<Result<String, super::CompactionError>>,
) -> Result<CompactResult, CompactError> {
    match completion.await.map_err(super::ServerError::from)? {
        Ok(message) => Ok(CompactResult::Compacted(message)),
        Err(super::CompactionError::PlanUnavailable(reason)) => {
            Ok(CompactResult::Noop(reason.to_string()))
        }
        Err(super::CompactionError::InsufficientProgress { message, .. }) => {
            Ok(CompactResult::Noop(message))
        }
        Err(error) => Err(CompactError::Compaction(error)),
    }
}

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

impl CreateSessionError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::WorkingDirectoryMustBeAbsolute => "session_working_directory_must_be_absolute",
            Self::Session(_) => "session_create_failed",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::WorkingDirectoryMustBeAbsolute => "session working directory must be absolute",
            Self::Session(_) => "session creation failed",
        }
    }
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

impl ActivateSkillError {
    /// Stable public operation error code.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::Disabled => "skills_disabled",
            Self::Unknown(_) => "unknown_skill",
            Self::ModelPolicy { code, .. } => code,
            Self::Session(_) => "skill_activation_failed",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Disabled => "skills are disabled",
            Self::Unknown(_) => "skill is unavailable",
            Self::ModelPolicy { message, .. } => message,
            Self::Session(_) => "skill activation failed",
        }
    }
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
    #[error("session working directory is unavailable")]
    Unavailable {
        /// Stable public error code.
        code: &'static str,
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
            Self::Unavailable { code } => code,
            Self::Busy(_) => "session_busy",
            Self::Session(_) => "session_cwd_change_failed",
        }
    }
    /// Return the secret-safe public message for this failure.
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            Self::MustBeAbsolute => "session working directory must be absolute",
            Self::Unavailable {
                code: "session_working_directory_not_directory",
            } => "session working directory path is not a directory",
            Self::Unavailable { .. } => "session working directory is unavailable",
            Self::Busy(_) => "session has active work and cannot change working directory",
            Self::Session(_) => "session working directory change failed",
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
    let metadata = tokio::fs::metadata(&working_directory).await.map_err(|_| {
        ChangeWorkingDirectoryError::Unavailable {
            code: "session_working_directory_unavailable",
        }
    })?;
    if !metadata.is_dir() {
        return Err(ChangeWorkingDirectoryError::Unavailable {
            code: "session_working_directory_not_directory",
        });
    }
    let working_directory = tokio::fs::canonicalize(working_directory)
        .await
        .map_err(|_| ChangeWorkingDirectoryError::Unavailable {
            code: "session_working_directory_unavailable",
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

impl DeleteSessionError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Busy(_) => "session_busy",
            Self::Session(_) => "session_delete_failed",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::Busy(_) => "session has active work and cannot be deleted",
            Self::Session(_) => "session deletion failed",
        }
    }
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
