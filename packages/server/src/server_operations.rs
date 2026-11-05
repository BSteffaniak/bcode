//! Transport-neutral application operations for daemon status and lifecycle.

pub mod shutdown_admission;

use super::ServerState;
use bcode_model_catalog_models::ModelCatalogDiagnostics;
use std::collections::BTreeSet;
use std::path::Path;

const MAX_CLIENT_INTERACTION_ADAPTERS: usize = 64;
pub const MAX_INTERACTION_ADAPTER_IDENTIFIER_BYTES: usize = 128;
pub const MAX_CLIENT_EFFECTIVE_CONFIG_BYTES: usize = 1024 * 1024;

// Provider-auth owns both execution and the portable error contract.
pub use bcode_provider_auth::{
    apply_auth_pool_preference as set_auth_pool_preference, list_auth_pools as auth_pools,
};

/// Return normalized daemon status without transport framing.
pub async fn status(
    state: &ServerState,
    working_directory: Option<&Path>,
) -> bcode_ipc::ServerStatus {
    state.status(working_directory).await
}

/// Return normalized model-catalog diagnostics without transport framing.
pub async fn model_catalog_diagnostics(state: &ServerState) -> ModelCatalogDiagnostics {
    state.model_catalog.diagnostics().await.into_public()
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
    state: &std::sync::Arc<ServerState>,
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
) -> Result<shutdown_admission::ShutdownReservation<'_>, StopBlocked> {
    // Reserve before inspecting asynchronous blockers, not after a stale status snapshot.
    // A rejected policy or failed acknowledgment drops the reservation and reopens admission.
    let reservation = state
        .shutdown_admission
        .prepare(mode == bcode_ipc::ServerStopMode::Force)
        .map_err(|blocker| {
            match blocker {
                shutdown_admission::AdmissionBlocked::Busy(snapshot) => {
                    tracing::debug!(?snapshot, "shutdown admission blocked");
                }
                shutdown_admission::AdmissionBlocked::Unavailable => {
                    tracing::debug!("shutdown admission unavailable");
                }
            }
            StopBlocked
        })?;
    tracing::debug!(admission = ?state.shutdown_admission.snapshot(), "shutdown admission reserved");
    if state.active_migration_shutdown_blocker().await.is_some() {
        return Err(StopBlocked);
    }
    if mode == bcode_ipc::ServerStopMode::IfIdle && state.idle_shutdown_blocker().await.is_some() {
        return Err(StopBlocked);
    }
    Ok(reservation)
}

/// Fail closed with a normalized lifecycle category.
fn lifecycle_failure(code: &str) -> bcode_daemon_lifecycle::application::RuntimeApplicationFailure {
    bcode_daemon_lifecycle::application::RuntimeApplicationFailure {
        code: code.into(),
        message: "runtime lifecycle observation or admission is unavailable".into(),
    }
}

pub fn runtime_identity(
    state: &ServerState,
) -> Result<
    bcode_ipc::RuntimeInstanceIdentity,
    bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
> {
    let daemon = &state.daemon_status;
    let artifact = daemon
        .artifact_id
        .as_ref()
        .ok_or_else(|| lifecycle_failure("runtime_identity_unavailable"))?;
    let location = daemon
        .state_location_id
        .clone()
        .ok_or_else(|| lifecycle_failure("runtime_identity_unavailable"))?;
    if daemon.instance_id.is_empty() || location.is_empty() {
        return Err(lifecycle_failure("runtime_identity_unavailable"));
    }
    Ok(bcode_ipc::RuntimeInstanceIdentity {
        instance_id: daemon.instance_id.clone(),
        artifact_id: artifact.to_string(),
        state_location_id: location,
    })
}

async fn quiescence(
    state: &ServerState,
) -> Result<
    bcode_ipc::RuntimeQuiescence,
    bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
> {
    use bcode_ipc::{RuntimeIdleBlocker, RuntimeIdleBlockerKind as Kind};
    let admission = state
        .shutdown_admission
        .snapshot()
        .map_err(|_| lifecycle_failure("runtime_admission_unavailable"))?;
    let mut blockers = Vec::new();
    let mut add = |kind, count| {
        if count > 0 {
            blockers.push(RuntimeIdleBlocker { kind, count });
        }
    };
    add(Kind::Admission, admission.active_operations);
    add(
        Kind::Stopping,
        usize::from(admission.phase != shutdown_admission::AdmissionPhase::Running),
    );
    add(
        Kind::Observation,
        usize::from(
            state.xray.observation_active().unwrap_or(true) || state.xray_endpoints.active().await,
        ),
    );
    add(
        Kind::Migration,
        state.session_migrations.active_count().await,
    );
    add(
        Kind::Clients,
        state.clients.lock().await.len().saturating_sub(1),
    );
    add(
        Kind::RecoveredInteraction,
        state.recovered_interaction_owners.lock().await.len(),
    );
    add(Kind::ModelTurn, state.active_runtime_turn_count().await);
    add(
        Kind::QueuedCommand,
        state
            .session_runtimes
            .lock()
            .await
            .values()
            .map(|handle| {
                handle
                    .queued_followups
                    .load(std::sync::atomic::Ordering::Acquire)
            })
            .sum(),
    );
    add(Kind::RuntimeWork, state.runtime_work.active_count().await);
    add(
        Kind::PluginTask,
        state
            .plugins
            .executor_statuses()
            .into_iter()
            .map(|status| status.running)
            .sum(),
    );
    Ok(bcode_ipc::RuntimeQuiescence {
        version: 1,
        instance: runtime_identity(state)?,
        admission_generation: admission.generation,
        blockers,
    })
}

/// Subscribe before sampling so mutation between sampling and waiting is not lost.
pub async fn wait_quiescence(
    state: &ServerState,
    expected: &bcode_ipc::RuntimeInstanceIdentity,
    after: Option<&bcode_ipc::RuntimeQuiescence>,
    wait_ms: u32,
) -> Result<
    bcode_ipc::RuntimeQuiescence,
    bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
> {
    if after.is_some_and(|snapshot| snapshot.version != 1 || snapshot.instance != *expected) {
        return Err(lifecycle_failure("unsupported_runtime_observation"));
    }
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(u64::from(wait_ms.min(30_000)));
    let mut changes = state.activity_changes.subscribe();
    let mut admission_changes = state.shutdown_admission.subscribe();
    loop {
        let snapshot = quiescence(state).await?;
        if snapshot.instance != *expected {
            return Err(lifecycle_failure("runtime_instance_changed"));
        }
        if after != Some(&snapshot) || tokio::time::Instant::now() >= deadline {
            return Ok(snapshot);
        }
        tokio::select! {
            _ = changes.recv() => {},
            _ = admission_changes.changed() => {},
            () = tokio::time::sleep_until(deadline) => return quiescence(state).await,
        }
    }
}

pub async fn prepare_quiescent_stop<'a>(
    state: &'a ServerState,
    expected: &bcode_ipc::RuntimeQuiescence,
) -> Result<
    shutdown_admission::ShutdownReservation<'a>,
    bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
> {
    if expected.version != 1 || !expected.blockers.is_empty() {
        return Err(lifecycle_failure("unsupported_runtime_observation"));
    }
    if runtime_identity(state)? != expected.instance {
        return Err(lifecycle_failure("runtime_instance_changed"));
    }
    let reservation = state
        .shutdown_admission
        .prepare_observed(expected.admission_generation)
        .map_err(|_| lifecycle_failure("runtime_observation_changed"))?;
    // Reserve before the authoritative blocker recheck. Snapshot idleness alone is not admission.
    if state.active_migration_shutdown_blocker().await.is_some()
        || state.idle_shutdown_blocker().await.is_some()
    {
        return Err(lifecycle_failure("daemon_busy"));
    }
    Ok(reservation)
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
        if identifiers.iter().any(|value| {
            value.trim().is_empty() || value.len() > MAX_INTERACTION_ADAPTER_IDENTIFIER_BYTES
        }) || adapter.tui_surface_kind.as_deref().is_some_and(|value| {
            value.trim().is_empty() || value.len() > MAX_INTERACTION_ADAPTER_IDENTIFIER_BYTES
        }) {
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
