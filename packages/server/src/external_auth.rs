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

pub async fn revoke(state: &ServerState, binding_id: String) -> Result<bool, ExternalAuthError> {
    // Listing and revoking stale grants must remain possible after a plugin is disabled.
    let store = ExternalAuthStore::new(&state.state_root);
    tokio::task::spawn_blocking(move || store.revoke(&binding_id))
        .await
        .map_err(|_| ExternalAuthError::StateUnavailable)?
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
