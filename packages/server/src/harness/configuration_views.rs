//! Ephemeral host-admitted plugin configuration, never durable recovery authority.
use std::{collections::BTreeMap, sync::Arc};

use bcode_plugin::PluginServiceConfigView;
use bcode_session_models::{ClientId, SessionId};
use sha2::Digest as _;

use super::{HarnessOwner, ManagedHarnessError};
use crate::harness_dispatch::ManagedCaller;

const MAX_VIEWS: usize = 128;
const MAX_BINDINGS: usize = 1024;

/// Daemon-local immutable configuration bindings; never serialized as recovery authority.
#[derive(Default)]
pub struct ConfigurationViews {
    inner: tokio::sync::Mutex<Views>,
    baseline: std::sync::OnceLock<PluginServiceConfigView>,
    parents: std::sync::Mutex<BTreeMap<SessionId, ParentBinding>>,
    parent_capacity: std::sync::OnceLock<Arc<tokio::sync::Semaphore>>,
}

impl std::fmt::Debug for ConfigurationViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Retained views may contain non-public configuration. Do not traverse them.
        formatter
            .debug_struct("ConfigurationViews")
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Views {
    views: BTreeMap<String, PluginServiceConfigView>,
    bindings: BTreeMap<SessionId, Binding>,
}

struct ParentBinding {
    view: PluginServiceConfigView,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// Capacity owned before canonical parent creation and transferred after successful publication.
pub struct ParentReservation<'a> {
    registry: &'a ConfigurationViews,
    view: PluginServiceConfigView,
    permit: tokio::sync::OwnedSemaphorePermit,
}

impl ParentReservation<'_> {
    /// Transfer the reserved slot after creation of a fresh UUID, without an await/capacity race.
    pub(crate) fn commit(self, session: SessionId) {
        let mut parents = self
            .registry
            .parents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Existing bindings are never replaced, even if a caller repeats this handoff.
        parents.entry(session).or_insert(ParentBinding {
            view: self.view,
            _permit: self.permit,
        });
    }
}

struct Binding {
    owner: HarnessOwner,
    open_id: String,
    view: PluginServiceConfigView,
}

impl ConfigurationViews {
    async fn retain(
        &self,
        digest: String,
        view: PluginServiceConfigView,
    ) -> Result<PluginServiceConfigView, ManagedHarnessError> {
        let mut inner = self.inner.lock().await;
        if let Some(existing) = inner.views.get(&digest) {
            if existing.runtime_generation() != view.runtime_generation() {
                return Err(ManagedHarnessError::Conflict);
            }
            return Ok(existing.clone());
        }
        if inner.views.len() >= MAX_VIEWS {
            return Err(ManagedHarnessError::Unavailable);
        }
        inner.views.insert(digest, view.clone());
        Ok(view)
    }

    /// Reserve before inspection or canonical creation. No replacement or eviction is permitted.
    pub(super) async fn pin(
        &self,
        owner: &HarnessOwner,
        open_id: &str,
        view: PluginServiceConfigView,
    ) -> Result<(), ManagedHarnessError> {
        let mut inner = self.inner.lock().await;
        if let Some(binding) = inner.bindings.get(&owner.session_id) {
            return if binding.owner == *owner
                && binding.open_id == open_id
                && binding.view.same_config_view(&view)
            {
                Ok(())
            } else {
                Err(ManagedHarnessError::Conflict)
            };
        }
        if inner.bindings.len() >= MAX_BINDINGS {
            return Err(ManagedHarnessError::Unavailable);
        }
        inner.bindings.insert(
            owner.session_id,
            Binding {
                owner: owner.clone(),
                open_id: open_id.into(),
                view,
            },
        );
        Ok(())
    }

    pub(super) async fn existing_open(
        &self,
        owner: &HarnessOwner,
        open_id: &str,
    ) -> Result<Option<PluginServiceConfigView>, ManagedHarnessError> {
        let inner = self.inner.lock().await;
        let Some(binding) = inner.bindings.get(&owner.session_id) else {
            return Ok(None);
        };
        if binding.owner != *owner || binding.open_id != open_id {
            return Err(ManagedHarnessError::Conflict);
        }
        Ok(Some(binding.view.clone()))
    }

    /// Reserve capacity before publishing a new ordinary session. Dropping an unused
    /// reservation returns its permit without publishing or cancelling any session work.
    pub(crate) fn reserve_parent(
        &self,
        view: PluginServiceConfigView,
    ) -> Result<ParentReservation<'_>, ManagedHarnessError> {
        let capacity = self
            .parent_capacity
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_BINDINGS)));
        let permit = Arc::clone(capacity)
            .try_acquire_owned()
            .map_err(|_| ManagedHarnessError::Unavailable)?;
        Ok(ParentReservation {
            registry: self,
            view,
            permit,
        })
    }

    /// Caller must already have proved no remaining work can use this ordinary parent.
    /// Never call on detach, timeout, or a native Close acknowledgment.
    pub(crate) fn retire_parent(&self, session: SessionId) {
        self.parents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session);
    }

    pub(super) async fn get(
        &self,
        owner: &HarnessOwner,
    ) -> Result<PluginServiceConfigView, ManagedHarnessError> {
        let inner = self.inner.lock().await;
        let binding = inner
            .bindings
            .get(&owner.session_id)
            .ok_or(ManagedHarnessError::Unavailable)?;
        if binding.owner != *owner {
            tracing::debug!(target: "bcode_server::harness",
                generation = binding.owner.generation == owner.generation,
                artifact = binding.owner.artifact_id == owner.artifact_id,
                daemon = binding.owner.daemon_id == owner.daemon_id,
                "managed configuration binding belongs to another owner");
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(binding.view.clone())
    }
}

/// Exact already-loaded runtime configuration, including host overlays and lazy resolvers.
/// # Errors
/// Returns stale-owner failure if the retained runtime generation has changed.
pub fn baseline(
    state: &crate::ServerState,
) -> Result<PluginServiceConfigView, ManagedHarnessError> {
    let view = state.harness_configuration_views.baseline.get_or_init(|| {
        let mut configs = state.plugins.configs().clone();
        // PluginHost leaves LoadedPlugin's default configuration intact when an enabled
        // manifest has no explicit map entry. Materialize that exact startup baseline;
        // service views intentionally do not infer missing entries at invocation time.
        for id in state.plugins.registry().manifests().keys() {
            configs.entry(id.clone()).or_default();
        }
        for (id, config) in &mut configs {
            config.durable_state_root = Some(bcode_plugin_config::plugin_state_root(
                &state.state_root.join("plugins"),
                id,
            ));
        }
        state.plugins.service_config_view(configs)
    });
    if view.runtime_generation() != state.plugins.runtime_generation() {
        return Err(ManagedHarnessError::StaleOwner);
    }
    Ok(view.clone())
}

/// Build only from authenticated client data whose confined load-time sources still reproduce it.
/// Omitted legacy configuration means startup configuration; malformed/partial data never does.
/// # Errors
/// Rejects absent callers, invalid or unverified configuration, changed plugin selection,
/// exhausted capacity, and stale runtime identity.
pub async fn for_client(
    state: &Arc<crate::ServerState>,
    client_id: ClientId,
) -> Result<PluginServiceConfigView, ManagedHarnessError> {
    let view =
        super::registrations::effective(declarative_for_client(state, client_id).await?).await?;
    let encoded = serde_json::to_vec(&view.configuration_values())
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let digest = sha2::Sha256::digest(encoded)
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        });
    state.harness_configuration_views.retain(digest, view).await
}

fn verify_startup_sources(
    state: &crate::ServerState,
    context: &bcode_ipc::ClientRuntimeContext,
    supplied: &bcode_config::BcodeConfig,
) -> Result<(), ManagedHarnessError> {
    if let Some(authority) = &state.startup_config_authority {
        let sources = context
            .config_launch
            .as_ref()
            .and_then(|launch| launch.recovery_sources.as_ref())
            .ok_or(ManagedHarnessError::Invalid)?;
        authority
            .verify_workspace(supplied, sources)
            .map_err(|_| ManagedHarnessError::Conflict)?;
    }
    Ok(())
}

/// Verify declarative launch inputs without incorporating mutable registration state.
pub async fn declarative_for_client(
    state: &Arc<crate::ServerState>,
    client_id: ClientId,
) -> Result<PluginServiceConfigView, ManagedHarnessError> {
    let context = state
        .client_runtime_context(client_id)
        .await
        .ok_or(ManagedHarnessError::StaleOwner)?;
    let startup = state.startup_config.clone();
    let durable_root = state.state_root.join("plugins");
    tracing::trace!(target: "bcode_server::harness",
        supplied_config = context.effective_config_toml.is_some(),
        launch = context.config_launch.is_some(), "managed client configuration view");
    match context.effective_config_toml.as_deref() {
        None if context.config_launch.is_none() => return baseline(state),
        Some(encoded) => {
            let supplied = bcode_config::decode_effective_config(encoded)
                .map_err(|_| ManagedHarnessError::Invalid)?;
            if serde_json::to_value(&supplied).map_err(|_| ManagedHarnessError::Invalid)?
                == serde_json::to_value(&startup).map_err(|_| ManagedHarnessError::Invalid)?
            {
                verify_startup_sources(state, &context, &supplied)?;
                return baseline(state);
            }
        }
        None => return Err(ManagedHarnessError::Invalid),
    }
    let selection = state.startup_plugin_selection.clone();
    let defaults = state.default_plugin_ids.clone();
    let manifests = state.plugins.registry().manifests().clone();
    let authority = state.startup_config_authority.clone();
    let config = tokio::task::spawn_blocking(move || {
        let config = match context.effective_config_toml.as_deref() {
            None if context.config_launch.is_none() => startup,
            Some(encoded) => {
                let config = bcode_config::decode_effective_config(encoded)
                    .map_err(|_| ManagedHarnessError::Invalid)?;
                // Exact daemon-start configuration needs no client-directed file reads.
                // Compare the complete declarative value, not an execution-only digest.
                if serde_json::to_value(&config).map_err(|_| ManagedHarnessError::Invalid)?
                    != serde_json::to_value(&startup).map_err(|_| ManagedHarnessError::Invalid)?
                {
                    let launch = context
                        .config_launch
                        .as_deref()
                        .ok_or(ManagedHarnessError::Invalid)?;
                    if launch.inline_overrides {
                        return Err(ManagedHarnessError::Invalid);
                    }
                    let sources = launch
                        .recovery_sources
                        .as_ref()
                        .ok_or(ManagedHarnessError::Invalid)?;
                    let cwd = context
                        .working_directory
                        .as_deref()
                        .ok_or(ManagedHarnessError::Invalid)?;
                    if sources.cwd != cwd {
                        return Err(ManagedHarnessError::Invalid);
                    }
                    // Source authority was captured from the daemon's actual startup loader,
                    // before setup edits. Client cwd is correlation, not read authority.
                    authority
                        .as_ref()
                        .ok_or(ManagedHarnessError::Invalid)?
                        .verify_workspace(&config, sources)
                        .map_err(|_| ManagedHarnessError::Conflict)?;
                }
                config
            }
            None => return Err(ManagedHarnessError::Invalid),
        };
        let requested = bcode_config::plugin_selection_with_default_plugin_ids(&config, &defaults);
        if requested.mode != selection.mode
            || requested.enabled != selection.enabled
            || requested.disabled != selection.disabled
        {
            return Err(ManagedHarnessError::Conflict);
        }
        let digest = bcode_config::recovery::execution_fingerprint(&config)
            .map_err(|_| ManagedHarnessError::Invalid)?;
        let mut configs = bcode_plugin_config::resolve_plugin_configs_for_manifests(
            &config,
            manifests.values(),
            &crate::managed_plugin_state_base(),
            &BTreeMap::new(),
        );
        for (id, configuration) in &mut configs {
            configuration.durable_state_root =
                Some(bcode_plugin_config::plugin_state_root(&durable_root, id));
        }
        Ok((digest, configs))
    })
    .await
    .map_err(|_| ManagedHarnessError::Unavailable)??;
    state
        .harness_configuration_views
        .retain(config.0, state.plugins.service_config_view(config.1))
        .await
}

/// Reserve the exact owner/open view before creation inspection or canonical publication.
pub(super) async fn pin_creation(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    owner: &HarnessOwner,
    open_id: &str,
) -> Result<PluginServiceConfigView, ManagedHarnessError> {
    let existing = state
        .harness_configuration_views
        .existing_open(owner, open_id)
        .await?;
    if let Some(view) = existing {
        return Ok(view);
    }
    tracing::debug!(target: "bcode_server::harness", "managed configuration view pinned for creation");
    // A canonical session predating this reservation cannot be adopted by supplying new config.
    match state
        .sessions
        .session_external_harness_state(owner.session_id)
        .await
    {
        Ok(None) | Err(bcode_session::SessionError::NotFound(_)) => {}
        Ok(Some(_)) => return Err(ManagedHarnessError::Unavailable),
        Err(_) => return Err(ManagedHarnessError::Storage),
    }
    let view = for_caller(state, caller).await?;
    state
        .harness_configuration_views
        .pin(owner, open_id, view.clone())
        .await?;
    Ok(view)
}

pub(super) async fn for_owner(
    state: &Arc<crate::ServerState>,
    owner: &HarnessOwner,
) -> Result<PluginServiceConfigView, ManagedHarnessError> {
    let view = match state.harness_configuration_views.get(owner).await {
        Ok(view) => view,
        Err(ManagedHarnessError::Unavailable) => {
            tracing::debug!(target: "bcode_server::harness", "managed configuration binding absent; using daemon baseline");
            // Compatibility is only for an exact canonical owner in this daemon. This
            // does not adopt client configuration, change generation, or authorize resume.
            let store = super::CanonicalHarnessStore::new(state);
            // Canonical admission validates the durable build fingerprint and normalizes
            // the live artifact ID. They are distinct identities, not interchangeable strings.
            let current = store.current_owner(owner.session_id).await?;
            if current != *owner {
                return Err(ManagedHarnessError::StaleOwner);
            }
            drop(current);
            drop(store);
            baseline(state)?
        }
        Err(error) => return Err(error),
    };
    if view.runtime_generation() != state.plugins.runtime_generation() {
        return Err(ManagedHarnessError::StaleOwner);
    }
    Ok(view)
}

pub async fn for_session(
    state: &Arc<crate::ServerState>,
    session: SessionId,
) -> Result<PluginServiceConfigView, ManagedHarnessError> {
    let parent_view = state
        .harness_configuration_views
        .parents
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&session)
        .map(|binding| binding.view.clone());
    if let Some(view) = parent_view {
        if view.runtime_generation() != state.plugins.runtime_generation() {
            return Err(ManagedHarnessError::StaleOwner);
        }
        return Ok(view);
    }
    let Some(selected) = state
        .sessions
        .session_external_harness_state(session)
        .await
        .map_err(|_| ManagedHarnessError::Storage)?
    else {
        // An ordinary Bcode session (for example a workflow's parent) without a retained
        // parent view runs under the daemon's loaded configuration.
        return baseline(state);
    };
    let owner = super::CanonicalHarnessStore::live_owner_from_snapshot(state, session, &selected)?;
    // Session-scoped discovery/status is descriptive, not a live-owner transfer. A
    // retained binding still wins; historical snapshots without one use only the
    // actual daemon baseline. Live delivery independently uses for_owner.
    match state.harness_configuration_views.get(&owner).await {
        Ok(view) if view.runtime_generation() == state.plugins.runtime_generation() => Ok(view),
        Ok(_) => Err(ManagedHarnessError::StaleOwner),
        Err(ManagedHarnessError::Unavailable) => {
            super::registrations::effective(baseline(state)?).await
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn for_caller(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
) -> Result<PluginServiceConfigView, ManagedHarnessError> {
    match caller {
        ManagedCaller::Client(client) => for_client(state, *client).await,
        ManagedCaller::Invocation { session_id, .. }
        | ManagedCaller::Workflow { session_id, .. } => for_session(state, *session_id).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn legacy_and_exact_clients_use_loaded_baseline_without_reconstruction() {
        let state = Arc::new(crate::tests::test_server_state(
            crate::SessionManager::default(),
        ));
        let client = ClientId::new();
        state
            .client_runtime_contexts
            .lock()
            .await
            .insert(client, crate::ClientRuntimeContext::default());
        let first = for_client(&state, client).await.unwrap();
        assert!(first.same_config_view(&baseline(&state).unwrap()));
        let encoded = bcode_config::encode_effective_config(&state.startup_config).unwrap();
        state.client_runtime_contexts.lock().await.insert(
            client,
            crate::ClientRuntimeContext {
                effective_config_toml: Some(Box::new(encoded)),
                ..Default::default()
            },
        );
        let same = for_client(&state, client)
            .await
            .unwrap()
            .same_config_view(&first);
        drop(first);
        assert!(same);
        state.client_runtime_contexts.lock().await.insert(
            client,
            crate::ClientRuntimeContext {
                effective_config_toml: Some(Box::new("not valid TOML =".into())),
                ..Default::default()
            },
        );
        assert!(for_client(&state, client).await.is_err());
        drop(state);
    }

    #[tokio::test]
    async fn sparse_loaded_baseline_reaches_the_enabled_service() {
        let state =
            crate::tests::test_server_state_with_fake_provider(crate::SessionManager::default());
        let view = baseline(&state).unwrap();
        // Unsupported operation is answered by the plugin. A sparse startup map must
        // not fail before invocation as a missing service-view configuration.
        let response = view
            .invoke_service_with_bridge_scoped(
                "bcode.fake-provider",
                bcode_model::MODEL_PROVIDER_INTERFACE_ID,
                "unsupported-baseline-probe",
                Vec::new(),
                bcode_plugin::PluginInvocationScope::Global,
                None,
            )
            .await
            .unwrap();
        drop(view);
        assert!(response.error.is_some());
    }

    #[test]
    fn parent_capacity_is_reserved_reclaimed_and_never_replaces_a_binding() {
        let state = crate::tests::test_server_state(crate::SessionManager::default());
        let registry = ConfigurationViews::default();
        let first = baseline(&state).unwrap();
        let other = state
            .plugins
            .service_config_view(state.plugins.configs().clone());
        let reservations: Vec<_> = (0..MAX_BINDINGS)
            .map(|_| registry.reserve_parent(first.clone()).unwrap())
            .collect();
        assert!(registry.reserve_parent(first.clone()).is_err());
        drop(reservations);
        let session = SessionId::new();
        registry
            .reserve_parent(first.clone())
            .unwrap()
            .commit(session);
        registry
            .reserve_parent(other.clone())
            .unwrap()
            .commit(session);
        drop(other);
        assert!(
            registry.parents.lock().unwrap()[&session]
                .view
                .same_config_view(&first)
        );
        registry.retire_parent(session);
        assert!(registry.parents.lock().unwrap().is_empty());
        assert!(registry.reserve_parent(first.clone()).is_ok());
        drop(first);
        drop(registry);
        drop(state);
    }
}
