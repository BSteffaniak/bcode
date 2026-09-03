//! Transport-neutral application operations for daemon status and lifecycle.

use super::ServerState;
use std::collections::BTreeSet;
use std::path::Path;

const MAX_CLIENT_INTERACTION_ADAPTERS: usize = 64;
pub const MAX_INTERACTION_ADAPTER_IDENTIFIER_BYTES: usize = 128;
pub const MAX_CLIENT_EFFECTIVE_CONFIG_BYTES: usize = 1024 * 1024;

/// Return normalized daemon status without transport framing.
pub async fn status(
    state: &ServerState,
    working_directory: Option<&Path>,
) -> bcode_ipc::ServerStatus {
    state.status(working_directory).await
}

/// Return normalized model-catalog diagnostics without transport framing.
pub async fn model_catalog_diagnostics(state: &ServerState) -> bcode_ipc::ModelCatalogDiagnostics {
    let diagnostics = state.model_catalog.diagnostics().await;
    let epoch_ms = |time: Option<std::time::SystemTime>| {
        time.and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|value| u64::try_from(value.as_millis()).ok())
    };
    bcode_ipc::ModelCatalogDiagnostics {
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

#[derive(Debug, thiserror::Error)]
pub enum UpdateClientContextError {
    #[error("invalid client configuration")]
    InvalidConfig,
    #[error("incompatible client configuration")]
    IncompatibleConfig,
    #[error("invalid interaction adapters")]
    InvalidInteractionAdapters,
}

impl UpdateClientContextError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid_client_config",
            Self::IncompatibleConfig => "incompatible_config",
            Self::InvalidInteractionAdapters => "invalid_interaction_adapters",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::InvalidConfig => "client configuration is invalid",
            Self::IncompatibleConfig => "client configuration is incompatible with this daemon",
            Self::InvalidInteractionAdapters => "client interaction adapters are invalid",
        }
    }
}

/// Validate and retain one client's runtime context without transport framing.
pub async fn update_client_runtime_context(
    state: &ServerState,
    client_id: bcode_session_models::ClientId,
    runtime_context: Option<bcode_ipc::ClientRuntimeContext>,
) -> Result<(), UpdateClientContextError> {
    validate_client_effective_config(runtime_context.as_ref())?;
    validate_client_plugin_selection(state, runtime_context.as_ref())?;
    validate_client_interaction_adapters(runtime_context.as_ref())?;
    state
        .set_client_runtime_context(client_id, runtime_context)
        .await;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum IngestClientMetricsError {
    #[error("invalid client metrics")]
    InvalidBatch,
}

impl IngestClientMetricsError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidBatch => "invalid_client_metrics",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::InvalidBatch => "client metrics batch is invalid",
        }
    }
}

/// Validate and ingest one bounded client metric batch.
pub fn ingest_client_metrics(
    state: &ServerState,
    batch: bcode_metrics::ClientMetricBatch,
) -> Result<usize, IngestClientMetricsError> {
    batch
        .validate_for_namespace("tui.")
        .map_err(|_| IngestClientMetricsError::InvalidBatch)?;
    let accepted = batch.observations.len();
    state.metrics.record_client_batch(batch);
    Ok(accepted)
}
#[derive(Debug, thiserror::Error)]
#[error("daemon is busy")]
pub struct StopBlocked;

impl StopBlocked {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code() -> &'static str {
        "daemon_busy"
    }

    /// Secret-safe public operation error message.
    ///
    /// The blocker category is deliberately not exposed here: it is reported through
    /// `ServerStatus::idle_shutdown_blocker` and the daemon log, where the caller already holds
    /// daemon-level visibility, rather than in the error a bare stop request receives.
    #[must_use]
    pub const fn message() -> &'static str {
        "daemon has active work and cannot stop in the requested mode"
    }
}

/// Validate a shutdown request before the transport publishes acknowledgment.
pub async fn prepare_stop(
    state: &ServerState,
    mode: bcode_ipc::ServerStopMode,
) -> Result<(), StopBlocked> {
    if state.active_migration_shutdown_blocker().await.is_some() {
        return Err(StopBlocked);
    }
    if mode == bcode_ipc::ServerStopMode::IfIdle && state.idle_shutdown_blocker().await.is_some() {
        return Err(StopBlocked);
    }
    Ok(())
}

/// Trigger daemon shutdown after transport acknowledgment.
pub fn request_shutdown(state: &ServerState) {
    state.request_shutdown();
}

pub fn validate_client_effective_config(
    context: Option<&bcode_ipc::ClientRuntimeContext>,
) -> Result<(), UpdateClientContextError> {
    let Some(contents) = context.and_then(|context| context.effective_config_toml.as_deref())
    else {
        return Ok(());
    };
    if contents.len() > MAX_CLIENT_EFFECTIVE_CONFIG_BYTES {
        return Err(UpdateClientContextError::InvalidConfig);
    }
    bcode_config::decode_effective_config(contents)
        .map(|_| ())
        .map_err(|_| UpdateClientContextError::InvalidConfig)
}

pub fn validate_client_plugin_selection(
    state: &ServerState,
    context: Option<&bcode_ipc::ClientRuntimeContext>,
) -> Result<(), UpdateClientContextError> {
    let Some(contents) = context.and_then(|context| context.effective_config_toml.as_deref())
    else {
        return Ok(());
    };
    let config = bcode_config::decode_effective_config(contents)
        .map_err(|_| UpdateClientContextError::InvalidConfig)?;
    let selection =
        bcode_config::plugin_selection_with_default_plugin_ids(&config, &state.default_plugin_ids);
    if selection == state.startup_plugin_selection {
        return Ok(());
    }
    Err(UpdateClientContextError::IncompatibleConfig)
}

pub fn validate_client_interaction_adapters(
    context: Option<&bcode_ipc::ClientRuntimeContext>,
) -> Result<(), UpdateClientContextError> {
    let Some(context) = context else {
        return Ok(());
    };
    if context.interaction_adapters.len() > MAX_CLIENT_INTERACTION_ADAPTERS {
        return Err(UpdateClientContextError::InvalidInteractionAdapters);
    }
    let mut routes = BTreeSet::new();
    for adapter in &context.interaction_adapters {
        let identifiers = [
            adapter.producer_id.as_str(),
            adapter.exchange_schema.as_str(),
            adapter.platform_id.as_str(),
            adapter.interaction_kind.as_str(),
        ];
        if identifiers
            .iter()
            .any(|value| value.is_empty() || value.len() > MAX_INTERACTION_ADAPTER_IDENTIFIER_BYTES)
            || adapter.tui_surface_kind.as_deref().is_some_and(|value| {
                value.is_empty() || value.len() > MAX_INTERACTION_ADAPTER_IDENTIFIER_BYTES
            })
        {
            return Err(UpdateClientContextError::InvalidInteractionAdapters);
        }
        if adapter.min_schema_version == 0
            || adapter.max_schema_version < adapter.min_schema_version
        {
            return Err(UpdateClientContextError::InvalidInteractionAdapters);
        }
        if !routes.insert((
            adapter.producer_id.as_str(),
            adapter.exchange_schema.as_str(),
            adapter.min_schema_version,
            adapter.max_schema_version,
            adapter.platform_id.as_str(),
            adapter.priority,
        )) {
            return Err(UpdateClientContextError::InvalidInteractionAdapters);
        }
    }
    Ok(())
}
