//! External connection consent plumbing. Credential acquisition remains provider-auth owned.

use bcode_provider_auth::external_auth::{self, ExternalAuthStore, SourceRoots};
use bcode_provider_auth_models::external::{
    ExternalAuthCandidate, ExternalAuthError, ExternalAuthGrant,
};

use crate::ServerState;

/// Snapshot authenticated enabled declarations before entering blocking local storage work.
pub fn declarations(state: &ServerState) -> Vec<bcode_plugin::RegisteredAuthProvider> {
    state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .cloned()
        .collect()
}

/// Run `operation` on a blocking thread with the serving process's document roots.
async fn with_roots<T: Send + 'static>(
    registered: Vec<bcode_plugin::RegisteredAuthProvider>,
    home: Option<std::path::PathBuf>,
    operation: impl FnOnce(
        &SourceRoots,
        external_auth::EnabledDeclarations<'_>,
    ) -> Result<T, ExternalAuthError>
    + Send
    + 'static,
) -> Result<T, ExternalAuthError> {
    tokio::task::spawn_blocking(move || {
        let enabled = registered
            .iter()
            .map(|entry| (entry.plugin_id.as_str(), &entry.contribution))
            .collect::<Vec<_>>();
        let home = home
            .or_else(std::env::home_dir)
            .ok_or(ExternalAuthError::SourceUnavailable)?;
        operation(&SourceRoots::from_process(home, &enabled), &enabled)
    })
    .await
    .map_err(|_| ExternalAuthError::StateUnavailable)?
}

pub async fn discover(
    state: &ServerState,
) -> Result<Vec<ExternalAuthCandidate>, ExternalAuthError> {
    with_roots(declarations(state), None, |roots, enabled| {
        external_auth::discover(roots, enabled)
    })
    .await
}

pub async fn approve(
    state: &ServerState,
    candidate: ExternalAuthCandidate,
) -> Result<ExternalAuthGrant, ExternalAuthError> {
    // The domain revalidates the candidate against this server-owned enabled registry, so a
    // caller-supplied plugin ID never establishes ownership.
    let store = ExternalAuthStore::new(&state.state_root);
    with_roots(declarations(state), None, move |roots, enabled| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ExternalAuthError::StateUnavailable)?
            .as_secs();
        store.approve(roots, enabled, &candidate, now)
    })
    .await
}

pub async fn list(state: &ServerState) -> Result<Vec<ExternalAuthGrant>, ExternalAuthError> {
    let store = ExternalAuthStore::new(&state.state_root);
    tokio::task::spawn_blocking(move || store.list())
        .await
        .map_err(|_| ExternalAuthError::StateUnavailable)?
}

pub async fn status(
    state: &ServerState,
) -> Result<Vec<bcode_provider_auth_models::external::ExternalAuthConnection>, ExternalAuthError> {
    let store = ExternalAuthStore::new(&state.state_root);
    let mut connections = with_roots(declarations(state), None, move |roots, enabled| {
        store.status(roots, enabled)
    })
    .await?;
    for connection in &mut connections {
        connection.recommended_models = recommended_models(state, &connection.grant.target).await;
    }
    Ok(connections)
}

/// Models the provider recommends for an approved method; empty when the provider is
/// unavailable or offers none.
async fn recommended_models(
    state: &ServerState,
    target: &bcode_provider_auth_models::external::ExternalAuthTarget,
) -> Vec<String> {
    state
        .plugins
        .invoke_service_json::<_, bcode_model::StartupModelChoicesResponse>(
            &target.plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_STARTUP_MODEL_CHOICES,
            &bcode_model::StartupRecommendationRequest {
                auth_provider_id: target.provider_id.clone(),
                method_id: target.method_id.clone(),
            },
        )
        .await
        .map(|response| {
            response
                .choices
                .into_iter()
                .filter(|choice| choice.provider_plugin_id == target.plugin_id)
                .map(|choice| choice.model_id)
                .collect()
        })
        .unwrap_or_default()
}

pub async fn revoke(state: &ServerState, binding_id: String) -> Result<bool, ExternalAuthError> {
    // Listing and revoking stale grants must remain possible after a plugin is disabled.
    let store = ExternalAuthStore::new(&state.state_root);
    tokio::task::spawn_blocking(move || store.revoke(&binding_id))
        .await
        .map_err(|_| ExternalAuthError::StateUnavailable)?
}

/// Provider routing for a session that selects an approved connection with `model_id`.
///
/// The settings come from the provider's recommendation for the approved method, so the
/// connection is used only at a destination the provider vouches for. Credentials are not read;
/// dispatch revalidates approval, account, and expiry.
///
/// # Errors
/// Rejects unapproved connections, another provider's connection, and models the provider does
/// not recommend for the approved method.
pub async fn session_context(
    state: &ServerState,
    provider_plugin_id: &str,
    model_id: &str,
    binding_id: &str,
) -> Result<bcode_model::ProviderRequestContext, ExternalAuthError> {
    let store = ExternalAuthStore::new(&state.state_root);
    let binding = binding_id.to_owned();
    let target = tokio::task::spawn_blocking(move || store.approved_target(&binding))
        .await
        .map_err(|_| ExternalAuthError::StateUnavailable)??;
    if target.plugin_id != provider_plugin_id {
        return Err(ExternalAuthError::DeclarationChanged);
    }
    let choices: bcode_model::StartupModelChoicesResponse = state
        .plugins
        .invoke_service_json(
            provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_STARTUP_MODEL_CHOICES,
            &bcode_model::StartupRecommendationRequest {
                auth_provider_id: target.provider_id,
                method_id: target.method_id,
            },
        )
        .await
        .map_err(|_| ExternalAuthError::DestinationRejected)?;
    let choice = choices
        .choices
        .into_iter()
        .find(|choice| {
            choice.provider_plugin_id == provider_plugin_id && choice.model_id == model_id
        })
        .ok_or(ExternalAuthError::DestinationRejected)?;
    Ok(bcode_model::ProviderRequestContext {
        settings: choice.settings,
        external_auth_binding: Some(binding_id.to_owned()),
        ..Default::default()
    })
}

/// Acquire an approved connection for one model turn at the authoritative dispatch boundary.
///
/// Returns `None` when the request does not select an external connection. `home` overrides
/// the serving process's home directory.
pub async fn context_for_dispatch(
    state: &ServerState,
    provider_plugin_id: &str,
    request: &bcode_model::ModelTurnRequest,
    home: Option<&std::path::Path>,
) -> Result<Option<bcode_model::ProviderRequestContext>, ExternalAuthError> {
    let Some(binding_id) = request.provider_context.external_auth_binding.clone() else {
        return Ok(None);
    };
    let store = ExternalAuthStore::new(&state.state_root);
    let lookup = store.clone();
    let target = tokio::task::spawn_blocking(move || lookup.approved_target(&binding_id))
        .await
        .map_err(|_| ExternalAuthError::StateUnavailable)??;
    let choices: bcode_model::StartupModelChoicesResponse = state
        .plugins
        .invoke_service_json(
            provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_STARTUP_MODEL_CHOICES,
            &bcode_model::StartupRecommendationRequest {
                auth_provider_id: target.provider_id,
                method_id: target.method_id,
            },
        )
        .await
        .map_err(|_| ExternalAuthError::DestinationRejected)?;
    let recipe = request.provider_context.clone();
    let provider = provider_plugin_id.to_owned();
    let model_id = request.model_id.clone();
    with_roots(
        declarations(state),
        home.map(Into::into),
        move |roots, enabled| {
            store.resolve_dispatch(external_auth::DispatchResolution {
                recipe: &recipe,
                provider_plugin_id: &provider,
                model_id: &model_id,
                roots,
                enabled,
                provider_choices: &choices.choices,
            })
        },
    )
    .await
    .map(Some)
}

#[cfg(test)]
mod tests;
