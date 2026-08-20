//! Transport-neutral application operations for daemon status and lifecycle.

use super::ServerState;
use std::path::Path;

/// Return normalized daemon status without transport framing.
pub async fn status(
    state: &ServerState,
    working_directory: Option<&Path>,
) -> bcode_ipc::ServerStatus {
    state.status(working_directory).await
}

/// Transport-neutral model-catalog diagnostics.
pub struct ModelCatalogDiagnostics {
    pub embedded_revision: String,
    pub remote_revision: Option<String>,
    pub remote_enabled: bool,
    pub cache_state: String,
    pub cache_age_seconds: Option<u64>,
    pub refresh_in_progress: bool,
    pub last_refresh_attempt_ms: Option<u64>,
    pub last_refresh_success_ms: Option<u64>,
    pub last_refresh_error: Option<String>,
}

/// Return normalized model-catalog diagnostics without transport framing.
pub async fn model_catalog_diagnostics(state: &ServerState) -> ModelCatalogDiagnostics {
    let diagnostics = state.model_catalog.diagnostics().await;
    let epoch_ms = |time: Option<std::time::SystemTime>| {
        time.and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|value| u64::try_from(value.as_millis()).ok())
    };
    ModelCatalogDiagnostics {
        embedded_revision: diagnostics.embedded_revision,
        remote_revision: diagnostics.remote_revision,
        remote_enabled: diagnostics.remote_enabled,
        cache_state: format!("{:?}", diagnostics.cache_state).to_lowercase(),
        cache_age_seconds: diagnostics.cache_age.map(|age| age.as_secs()),
        refresh_in_progress: diagnostics.refresh_in_progress,
        last_refresh_attempt_ms: epoch_ms(diagnostics.last_refresh_attempt),
        last_refresh_success_ms: epoch_ms(diagnostics.last_refresh_success),
        last_refresh_error: diagnostics.last_refresh_error,
    }
}

/// Application failure while requesting daemon shutdown.
#[derive(Debug, thiserror::Error)]
#[error("daemon is busy: {0}")]
pub struct StopBlocked(pub String);

/// Validate a shutdown request before the transport publishes acknowledgment.
pub async fn prepare_stop(
    state: &ServerState,
    mode: bcode_ipc::ServerStopMode,
) -> Result<(), StopBlocked> {
    if let Some(message) = state.active_migration_shutdown_blocker().await {
        return Err(StopBlocked(message));
    }
    if mode == bcode_ipc::ServerStopMode::IfIdle
        && let Some(message) = state.idle_shutdown_blocker().await
    {
        return Err(StopBlocked(message));
    }
    Ok(())
}

/// Trigger daemon shutdown after transport acknowledgment.
pub fn request_shutdown(state: &ServerState) {
    state.request_shutdown();
}
