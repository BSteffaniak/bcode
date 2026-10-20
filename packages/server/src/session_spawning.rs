//! Caller-bound composed canonical session creation and ordinary initial turn admission.

use std::sync::Arc;

use bcode_session_models::{
    ClientId, SessionApplicationFailure, SessionSpawnFailure, SessionSpawnRequest,
    SessionSpawnResult, SessionSpawnStage, TurnAdmission, TurnOrigin, TurnPriority,
};

use crate::{ServerState, session_operations};

fn failed(stage: SessionSpawnStage, code: &str, message: &str) -> SessionSpawnResult {
    SessionSpawnResult {
        session: None,
        admission: None,
        failure: Some(SessionSpawnFailure {
            stage,
            code: code.into(),
            message: message.into(),
        }),
    }
}

fn resolve_provider_context(
    state: &ServerState,
    config: &bcode_config::BcodeConfig,
    selection: &mut bcode_config::ResolvedModelSelection,
    runtime: &crate::ClientRuntimeContext,
    profile: Option<&str>,
) -> Result<bcode_model::ProviderRequestContext, &'static str> {
    let provider = selection
        .provider_plugin_id
        .as_deref()
        .ok_or("provider is unavailable")?;
    let Some(profile) = profile else {
        if runtime.selected_provider_plugin_id.as_deref() == Some(provider) {
            return Ok(runtime.provider_context.clone());
        }
        return bcode_provider_auth::try_resolve_provider_request_context(
            bcode_provider_auth::ProviderRequestContextResolution {
                config,
                selection: selection.clone(),
            },
        )
        .map_err(|_| "requested provider configuration cannot be resolved");
    };
    if profile.trim().is_empty() || profile.len() > 256 {
        return Err("invalid auth profile reference");
    }
    let matching: Vec<_> = state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .filter(|contribution| contribution.plugin_id == provider)
        .collect();
    if matching.len() != 1 {
        return Err("provider authentication ownership cannot be verified");
    }
    // Ownership is verified by the provider-auth domain before credential materialization.
    let owned = bcode_provider_auth::resolve_owned_profile_context(
        config,
        &matching[0].contribution.provider_id,
        provider,
        profile,
    )
    .map_err(|_| "requested auth profile ownership or credentials cannot be verified")?;
    selection.auth_profile = Some(profile.into());
    selection.auth_pool = None;
    let mut resolved = bcode_provider_auth::try_resolve_provider_request_context(
        bcode_provider_auth::ProviderRequestContextResolution {
            config,
            selection: selection.clone(),
        },
    )
    .map_err(|_| "requested auth profile configuration cannot be resolved")?;
    resolved.auth = owned.auth;
    resolved.auth_profile = owned.auth_profile;
    resolved.auth_pool = None;
    resolved.auth_candidates.clear();
    resolved.auth_pool_routing = bcode_model::ProviderAuthPoolRouting::default();
    resolved.auth_pool_selection_reason = None;
    Ok(resolved)
}

/// Prepare caller-bound spawning selection before creating a child or admitting work.
///
/// Malformed client configuration never falls back to daemon configuration. Clients without
/// runtime context use the same startup configuration policy as direct ordinary turn admission.
/// Model catalog negotiation remains owned by ordinary turn admission.
pub async fn session_spawn_runtime_context(
    state: &ServerState,
    client_id: ClientId,
    request: &SessionSpawnRequest,
) -> Result<Option<crate::ClientRuntimeContext>, SessionApplicationFailure> {
    prepare_runtime_context(state, client_id, request)
        .await
        .map_err(|message| SessionApplicationFailure {
            code: "session_spawn_selection_unavailable".into(),
            message: message.into(),
        })
}

async fn prepare_runtime_context(
    state: &ServerState,
    client_id: ClientId,
    request: &SessionSpawnRequest,
) -> Result<Option<crate::ClientRuntimeContext>, &'static str> {
    let runtime = state.client_runtime_context(client_id).await;
    let config = match runtime.as_ref() {
        Some(context) => {
            let contents = context
                .effective_config_toml
                .as_deref()
                .ok_or("caller effective configuration is unavailable")?;
            bcode_config::decode_effective_config(contents)
                .map_err(|_| "caller effective configuration is unsupported")?
        }
        None => state.startup_config.clone(),
    };
    if let Some(agent) = request.admission.execution.agent_profile.as_deref()
        && crate::resolve_agent_id(state, agent).await.is_none()
    {
        return Err("requested agent profile is unavailable");
    }
    if request.auth_profile.is_none()
        && request.admission.execution.provider_plugin_id.is_none()
        && request.admission.execution.model_id.is_none()
    {
        return Ok(runtime);
    }
    let mut selected_config = config.clone();
    let environment = bcode_config::ConfigEnvironmentSnapshot::new(
        std::collections::BTreeMap::new(),
        request.creation.working_directory.clone(),
    );
    let baseline = config.resolved_model_selection_with_environment(&environment);
    // Flatten the selected profile before explicit overrides: profile values must not override
    // a caller's requested model/provider when the canonical resolver applies profile precedence.
    selected_config.model.profile = None;
    selected_config.model.provider_plugin_id = runtime
        .as_ref()
        .and_then(|context| context.selected_provider_plugin_id.clone())
        .or(baseline.provider_plugin_id);
    selected_config.model.model_id = runtime
        .as_ref()
        .and_then(|context| context.selected_model_id.clone())
        .or(baseline.model_id);
    selected_config.model.auth_profile = baseline.auth_profile;
    selected_config.model.auth_pool = baseline.auth_pool;
    selected_config.model.settings = baseline.settings;
    selected_config.model.reasoning = baseline.reasoning;
    if let Some(provider) = &request.admission.execution.provider_plugin_id {
        if selected_config.model.provider_plugin_id.as_ref() != Some(provider) {
            selected_config.model.auth_profile = None;
            selected_config.model.auth_pool = None;
            selected_config.model.settings.clear();
        }
        selected_config.model.provider_plugin_id = Some(provider.clone());
    }
    if let Some(model) = &request.admission.execution.model_id {
        selected_config.model.model_id = Some(model.clone());
    }
    // Use the canonical configuration selection/alias path, without ambient daemon overrides.
    let mut selection = selected_config.resolved_model_selection_with_environment(
        &bcode_config::ConfigEnvironmentSnapshot::new(
            std::collections::BTreeMap::new(),
            request.creation.working_directory.clone(),
        ),
    );
    selection
        .validate_selection()
        .map_err(|_| "requested provider/model selection is incomplete")?;
    let mut context = runtime.clone().unwrap_or_default();
    let mut provider_context = resolve_provider_context(
        state,
        &config,
        &mut selection,
        &context,
        request.auth_profile.as_deref(),
    )?;
    crate::overlay_transient_process_env(&mut provider_context, context.process_env.clone());
    context.selected_provider_plugin_id = selection.provider_plugin_id;
    context.requested_model_id = request
        .admission
        .execution
        .model_id
        .clone()
        .or(selection.selected_model_id);
    context.selected_model_id = selection.model_id;
    context.provider_context = provider_context;
    Ok(Some(context))
}

/// Create an ordinary child and admit its initial input through the canonical scheduler.
///
/// The caller is verified by the application handle. All request preparation precedes effects.
/// Canonical creation retains the immutable composed request. Later failures retain known
/// receipts and never delete the child or its existing worktree.
pub async fn spawn(
    state: &Arc<ServerState>,
    client_id: ClientId,
    request: SessionSpawnRequest,
) -> SessionSpawnResult {
    if request.version != 1 {
        return failed(
            SessionSpawnStage::Preparation,
            "session_spawn_version_unsupported",
            "unsupported composed spawning request version",
        );
    }
    if request.creation.parent_id == request.creation.child_id {
        return failed(
            SessionSpawnStage::Preparation,
            "session_spawn_parent_invalid",
            "a child cannot be its own parent",
        );
    }
    if request
        .admission
        .execution
        .interaction_route
        .as_ref()
        .is_some_and(|route| route.destination_session_id != request.creation.child_id)
    {
        return failed(
            SessionSpawnStage::Preparation,
            "session_spawn_interaction_route_invalid",
            "ordinary child interactions must target the child session",
        );
    }
    if request.admission.execution.request_context_id.is_some()
        || !request.admission.execution.historical_context.is_empty()
    {
        return failed(
            SessionSpawnStage::Preparation,
            "session_spawn_context_invalid",
            "fresh ordinary children cannot inherit request-only or historical context capabilities",
        );
    }
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return failed(
            SessionSpawnStage::Preparation,
            "session_application_stopped",
            "session application is shutting down",
        );
    }
    if crate::ambiguous_session_location_response(state, request.creation.parent_id)
        .await
        .is_some()
        || crate::ambiguous_session_location_response(state, request.creation.child_id)
            .await
            .is_some()
    {
        return failed(
            SessionSpawnStage::Preparation,
            "session_location_ambiguous",
            "verify the canonical session state location before spawning",
        );
    }
    if state
        .active_session_namespace_mismatch(request.creation.child_id, client_id)
        .await
        .is_some()
    {
        return failed(
            SessionSpawnStage::Preparation,
            "session_incompatible_active_client",
            "child is active for an incompatible client",
        );
    }
    Box::pin(spawn_in_directory(state, client_id, request)).await
}

async fn spawn_in_directory(
    state: &Arc<ServerState>,
    client_id: ClientId,
    mut request: SessionSpawnRequest,
) -> SessionSpawnResult {
    if !request.creation.working_directory.is_absolute() {
        return failed(
            SessionSpawnStage::Preparation,
            "working_directory_must_be_absolute",
            "spawning requires an absolute working directory",
        );
    }
    let Ok(directory) = request.creation.working_directory.canonicalize() else {
        return failed(
            SessionSpawnStage::Preparation,
            "working_directory_unavailable",
            "working directory is unavailable",
        );
    };
    if !directory.is_dir() {
        return failed(
            SessionSpawnStage::Preparation,
            "working_directory_not_directory",
            "working directory is not a directory",
        );
    }
    request.creation.working_directory = directory.clone();
    if let Some(path) = request.worktree_directory.as_ref() {
        if !path.is_absolute() {
            return failed(
                SessionSpawnStage::Preparation,
                "worktree_invalid",
                "worktree directory must be absolute",
            );
        }
        let Ok(parent) = state
            .sessions
            .session_summary_non_owning(request.creation.parent_id)
            .await
        else {
            return failed(
                SessionSpawnStage::Preparation,
                "spawn_parent_unavailable",
                "parent canonical session cannot be verified",
            );
        };
        let cwd = parent.working_directory;
        let path = path.clone();
        let registered = tokio::task::spawn_blocking(move || {
            bcode_worktree::validate_registered_worktree(&cwd, &path)
        })
        .await;
        match registered {
            Ok(Ok(worktree)) if worktree.path() == directory => {
                request.worktree_directory = Some(directory);
            }
            _ => {
                return failed(
                    SessionSpawnStage::Preparation,
                    "worktree_invalid",
                    "working directory must match an existing registered worktree of the parent repository",
                );
            }
        }
    }
    Box::pin(spawn_with_admission(state, client_id, request)).await
}

async fn spawn_with_admission(
    state: &Arc<ServerState>,
    client_id: ClientId,
    mut request: SessionSpawnRequest,
) -> SessionSpawnResult {
    request.admission.activity = None;
    if request.admission.origin.is_none() {
        request.admission.origin = Some(TurnOrigin {
            producer: "bcode.session.spawn".into(),
            correlation_id: None,
            display_label: None,
        });
    }
    if request.admission.idempotency_key.is_none() {
        request.admission.idempotency_key = Some(request.creation.child_id.to_string());
    }
    if request.admission.validate().is_err() {
        return failed(
            SessionSpawnStage::Preparation,
            "session_spawn_options_invalid",
            "invalid composed spawning admission options",
        );
    }
    // Resolves configured auth references through the existing provider-owned context path.
    // Credentials are request-scoped and never enter the retained canonical envelope.
    let runtime_context = match session_spawn_runtime_context(state, client_id, &request).await {
        Ok(context) => context,
        Err(error) => return failed(SessionSpawnStage::Preparation, &error.code, &error.message),
    };
    let Ok(session) = state
        .sessions
        .create_spawning_session(request.clone())
        .await
    else {
        let mut result = failed(
            SessionSpawnStage::Creation,
            "session_spawn_creation_failed",
            "canonical child creation could not be verified; inspect the retained child identity before retrying the identical request",
        );
        result.session = state
            .sessions
            .session_summary_non_owning(request.creation.child_id)
            .await
            .ok();
        return result;
    };
    state
        .session_catalog
        .upsert_native_session(session.clone())
        .await;
    admit_spawned_session(state, client_id, request, session, runtime_context).await
}

async fn admit_spawned_session(
    state: &Arc<ServerState>,
    client_id: ClientId,
    request: SessionSpawnRequest,
    session: bcode_session_models::SessionSummary,
    runtime_context: Option<crate::ClientRuntimeContext>,
) -> SessionSpawnResult {
    let mut result = SessionSpawnResult {
        session: Some(session.clone()),
        admission: None,
        failure: None,
    };
    if request.admission.priority != TurnPriority::Interactive {
        result.admission = Some(TurnAdmission::Rejected(
            bcode_session_models::TurnRejectionReason::ExecutionPolicy,
        ));
        return result;
    }
    let mut execution_admission = request.admission.clone();
    if let Some(context) = &runtime_context {
        if execution_admission.execution.model_id.is_some() {
            execution_admission
                .execution
                .model_id
                .clone_from(&context.selected_model_id);
        }
        if execution_admission.execution.provider_plugin_id.is_some() {
            execution_admission
                .execution
                .provider_plugin_id
                .clone_from(&context.selected_provider_plugin_id);
        }
    }
    let mut ownership = None;
    let admission = Box::pin(session_operations::submit_turn_with_runtime_context(
        state,
        session.id,
        client_id,
        request.input,
        execution_admission,
        &mut ownership,
        runtime_context,
    ))
    .await;
    drop(ownership);
    match admission {
        Ok(admission) => result.admission = Some(admission),
        Err(error) => {
            if let Some((producer, key)) = request.admission.idempotency_identity() {
                result.admission = state
                    .sessions
                    .turn_receipt(session.id, producer, key)
                    .await
                    .ok()
                    .flatten()
                    .map(TurnAdmission::Existing);
            }
            let (code, message) = match error {
                session_operations::SubmitTurnError::Admission(_) => (
                    "session_spawn_admission_failed",
                    "initial turn admission failed; inspect the canonical child and admission identity",
                ),
                session_operations::SubmitTurnError::Dispatch(_) => (
                    "session_spawn_dispatch_failed",
                    "initial turn dispatch failed; durable admission may exist; inspect the exact receipt before retrying",
                ),
            };
            result.failure = Some(SessionSpawnFailure {
                stage: SessionSpawnStage::Admission,
                code: code.into(),
                message: message.into(),
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{SessionChildCreateRequest, SessionId, TurnAdmissionMetadata};

    async fn fixture() -> (tempfile::TempDir, Arc<ServerState>, SessionSpawnRequest) {
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::persistent(root.path()).unwrap();
        let parent = sessions
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
        let request = SessionSpawnRequest {
            version: 1,
            creation: SessionChildCreateRequest {
                child_id: SessionId::new(),
                parent_id: parent.id,
                name: Some("spawned".into()),
                working_directory: std::env::current_dir().unwrap(),
            },
            worktree_directory: None,
            input: "initial input".into(),
            admission: TurnAdmissionMetadata {
                priority: TurnPriority::Background,
                ..Default::default()
            },
            auth_profile: None,
        };
        (root, state, request)
    }

    #[tokio::test]
    async fn composed_rejection_preserves_child_and_conflicting_retry_does_not_submit() {
        let (_root, state, original) = fixture().await;
        let first = Box::pin(spawn(&state, ClientId::new(), original.clone())).await;
        first.validate().unwrap();
        assert_eq!(
            first.session.as_ref().unwrap().id,
            original.creation.child_id
        );
        assert_eq!(
            first.admission,
            Some(TurnAdmission::Rejected(
                bcode_session_models::TurnRejectionReason::ExecutionPolicy
            ))
        );
        let retry = Box::pin(spawn(&state, ClientId::new(), original.clone())).await;
        retry.validate().unwrap();
        assert_eq!(
            retry.session.as_ref().unwrap().id,
            first.session.as_ref().unwrap().id
        );
        assert_eq!(retry.admission, first.admission);
        let before = state
            .sessions
            .session_history(original.creation.child_id)
            .await
            .unwrap();
        let mut changed = original;
        changed.input = "different input".into();
        let conflict = Box::pin(spawn(&state, ClientId::new(), changed)).await;
        conflict.validate().unwrap();
        assert_eq!(conflict.failure.unwrap().stage, SessionSpawnStage::Creation);
        assert!(conflict.admission.is_none());
        assert_eq!(
            state
                .sessions
                .session_history(first.session.unwrap().id)
                .await
                .unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn unsupported_request_and_unregistered_worktree_have_no_creation_effect() {
        let (_root, state, mut request) = fixture().await;
        request.version = 999;
        let result = Box::pin(spawn(&state, ClientId::new(), request.clone())).await;
        result.validate().unwrap();
        assert_eq!(
            result.failure.unwrap().stage,
            SessionSpawnStage::Preparation
        );
        assert!(
            state
                .sessions
                .session_summary_non_owning(request.creation.child_id)
                .await
                .is_err()
        );
        request.version = 1;
        request.worktree_directory = Some(request.creation.working_directory.join("unregistered"));
        let result = Box::pin(spawn(&state, ClientId::new(), request.clone())).await;
        result.validate().unwrap();
        assert_eq!(
            result.failure.unwrap().stage,
            SessionSpawnStage::Preparation
        );
        assert!(
            state
                .sessions
                .session_summary_non_owning(request.creation.child_id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn initial_admission_is_canonical_and_identical_retry_preserves_receipt() {
        let (_root, state, mut request) = fixture().await;
        request.admission.priority = TurnPriority::Interactive;
        let first = Box::pin(spawn(&state, ClientId::new(), request.clone())).await;
        first.validate().unwrap();
        let receipt = match first.admission.as_ref().unwrap() {
            TurnAdmission::Accepted(receipt)
            | TurnAdmission::Existing(receipt)
            | TurnAdmission::Deferred(receipt) => receipt.clone(),
            admission => panic!("expected canonical admission receipt, got {admission:?}"),
        };
        let retry = Box::pin(spawn(&state, ClientId::new(), request.clone())).await;
        retry.validate().unwrap();
        assert!(
            matches!(retry.admission, Some(TurnAdmission::Existing(ref existing) | TurnAdmission::Deferred(ref existing)) if existing == &receipt)
        );
        let retained = state
            .sessions
            .turn_receipt(
                request.creation.child_id,
                "bcode.session.spawn",
                &request.creation.child_id.to_string(),
            )
            .await
            .unwrap();
        assert_eq!(retained, Some(receipt));
    }

    #[tokio::test]
    async fn malformed_client_config_fails_closed_before_creation() {
        let (_root, state, request) = fixture().await;
        let client_id = ClientId::new();
        state
            .set_client_runtime_context(
                client_id,
                Some(crate::ClientRuntimeContext {
                    effective_config_toml: Some(Box::new(String::from("[invalid"))),
                    ..Default::default()
                }),
            )
            .await;
        let failure = session_spawn_runtime_context(&state, client_id, &request)
            .await
            .unwrap_err();
        assert_eq!(failure.code, "session_spawn_selection_unavailable");
        let result = Box::pin(spawn(&state, client_id, request.clone())).await;
        assert_eq!(
            result.failure.unwrap().stage,
            SessionSpawnStage::Preparation
        );
        assert!(
            state
                .sessions
                .session_summary_non_owning(request.creation.child_id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unavailable_explicit_auth_fails_before_creation() {
        let (_root, state, mut request) = fixture().await;
        request.auth_profile = Some("missing-profile".into());
        let result = Box::pin(spawn(&state, ClientId::new(), request.clone())).await;
        result.validate().unwrap();
        assert_eq!(
            result.failure.unwrap().stage,
            SessionSpawnStage::Preparation
        );
        assert!(
            state
                .sessions
                .session_summary_non_owning(request.creation.child_id)
                .await
                .is_err()
        );
    }
}
