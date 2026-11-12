//! External connection consent plumbing. Credential acquisition remains provider-auth owned.

use bcode_provider_auth::external_auth::{self, ExternalAuthStore};
use bcode_provider_auth_models::external::{
    ExternalAuthCandidate, ExternalAuthError, ExternalAuthGrant, ExternalAuthScope,
};

use crate::ServerState;

/// Snapshot authenticated enabled declarations before entering blocking local storage work.
fn declarations(state: &ServerState) -> Vec<bcode_plugin::RegisteredAuthProvider> {
    state
        .plugins
        .auth_provider_registry()
        .providers()
        .into_iter()
        .cloned()
        .collect()
}

pub async fn discover(
    state: &ServerState,
) -> Result<Vec<ExternalAuthCandidate>, ExternalAuthError> {
    let registered = declarations(state);
    tokio::task::spawn_blocking(move || {
        let home = std::env::home_dir().ok_or(ExternalAuthError::SourceUnavailable)?;
        let enabled = registered
            .iter()
            .map(|entry| (entry.plugin_id.as_str(), &entry.contribution))
            .collect::<Vec<_>>();
        external_auth::discover(&home, &enabled)
    })
    .await
    .map_err(|_| ExternalAuthError::StateUnavailable)?
}

pub async fn approve(
    state: &ServerState,
    candidate: ExternalAuthCandidate,
    scope: Option<ExternalAuthScope>,
) -> Result<ExternalAuthGrant, ExternalAuthError> {
    // Never trust candidate.target.plugin_id as ownership: the domain revalidates the
    // exact binding against this server-owned enabled registry before granting consent.
    // Application-scoped approval requires a verified caller application identity.
    // The current local boundary has no such identity; never accept caller labels.
    if matches!(scope, Some(ExternalAuthScope::Application { .. })) {
        return Err(ExternalAuthError::ScopeDenied);
    }
    let registered = declarations(state);
    let store = ExternalAuthStore::new(&state.state_root);
    tokio::task::spawn_blocking(move || {
        let home = std::env::home_dir().ok_or(ExternalAuthError::SourceUnavailable)?;
        let enabled = registered
            .iter()
            .map(|entry| (entry.plugin_id.as_str(), &entry.contribution))
            .collect::<Vec<_>>();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ExternalAuthError::StateUnavailable)?
            .as_secs();
        store.approve(&home, &enabled, &candidate, scope, now)
    })
    .await
    .map_err(|_| ExternalAuthError::StateUnavailable)?
}

pub async fn list(state: &ServerState) -> Result<Vec<ExternalAuthGrant>, ExternalAuthError> {
    let store = ExternalAuthStore::new(&state.state_root);
    tokio::task::spawn_blocking(move || store.list())
        .await
        .map_err(|_| ExternalAuthError::StateUnavailable)?
}

pub async fn revoke(
    state: &ServerState,
    grant: ExternalAuthGrant,
) -> Result<bool, ExternalAuthError> {
    // Listing and revoking stale grants must remain possible after a plugin is disabled.
    let store = ExternalAuthStore::new(&state.state_root);
    tokio::task::spawn_blocking(move || store.revoke(&grant))
        .await
        .map_err(|_| ExternalAuthError::StateUnavailable)?
}
