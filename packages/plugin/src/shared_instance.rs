//! Product adaptation of shared ownership. No author code runs under state locks.
use crate::{PluginLoadError, PluginManifest};
use bcode_plugin_sdk::definition::{BcodePluginService, PluginDefinition, PluginInstance};
use bmux_plugin_runtime::{ActivationScope, InstanceScope, InvocationScope, RuntimeInvocationId};
use bmux_plugin_sdk::definition::{
    ComponentRole, PluginCallContext, PluginCreateContext, PluginLifecycleContext,
};
use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
};

type Snapshot = bcode_plugin_models::activation::ActivationContributions;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Created,
    Activating,
    Active,
    Closing,
    CleanupRequired,
    Destroyed,
}
struct Publication {
    product: Arc<dyn BcodePluginService>,
    snapshot: Snapshot,
}
struct CallOwner {
    scope: InvocationScope,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
    settled: bool,
    retiring: bool,
}
struct State {
    phase: Phase,
    operation: bool,
    implementation: Option<Arc<dyn PluginInstance>>,
    activation: Option<ActivationScope>,
    // Host resources charged to `activation`; released in the teardown window.
    resources: Option<bcode_plugin_sdk::definition::ActivationResources>,
    publication: Option<Publication>,
    partial_services: Option<bmux_plugin_sdk::TypedServiceRegistry>,
    calls: BTreeMap<RuntimeInvocationId, CallOwner>,
    failures: Vec<String>,
    destruction_uncertain: bool,
    author_cleaned: bool,
}
pub struct SharedInstance {
    scope: InstanceScope,
    context: PluginCreateContext,
    callback_host: Mutex<Option<bmux_plugin::LoaderCallbackHost>>,
    host: crate::SharedActivationContext,
    state: Mutex<State>,
    product_host: Mutex<Option<crate::ProductHostSession>>,
    host_sessions:
        Mutex<BTreeMap<RuntimeInvocationId, super::shared_activation::ProductHostSession>>,
    retained: Mutex<Option<Arc<Self>>>,
    // Signalled whenever an invocation retirement ends, so cleanup can await a
    // concurrent retirement of the same call instead of failing on it.
    retirements: std::sync::Condvar,
}
impl std::fmt::Debug for SharedInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedInstance")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}
struct Retirement<'a> {
    owner: &'a SharedInstance,
    key: RuntimeInvocationId,
}
impl Drop for Retirement<'_> {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(call) = state.calls.get_mut(&self.key) {
            call.retiring = false;
        }
        drop(state);
        self.owner.retirements.notify_all();
    }
}
struct Reservation<'a>(&'a SharedInstance);
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .operation = false;
    }
}
impl SharedInstance {
    pub(super) fn create(
        definition: PluginDefinition,
        manifest: &PluginManifest,
        host: crate::SharedActivationContext,
        role: ComponentRole,
    ) -> Result<Arc<Self>, PluginLoadError> {
        host.validate(&manifest.id)?;
        if definition.metadata.plugin_id != manifest.id {
            return Err(PluginLoadError::ManifestIdMismatch {
                file_id: manifest.id.clone(),
                library_id: definition.metadata.plugin_id,
            });
        }
        if definition
            .metadata
            .required_capabilities
            .iter()
            .any(|capability| !host.native.available_capabilities.contains(capability))
        {
            return Err(error(&manifest.id, "required_capabilities_not_negotiated"));
        }
        let embedded = definition
            .metadata
            .manifest_toml
            .as_ref()
            .ok_or_else(|| error(&manifest.id, "embedded_manifest"))?;
        let embedded: PluginManifest =
            toml::from_str(embedded).map_err(|_| error(&manifest.id, "embedded_manifest"))?;
        if !crate::embedded_manifest_matches(embedded, manifest) {
            return Err(error(&manifest.id, "embedded_manifest_mismatch"));
        }
        let scope = InstanceScope::new(host.runtime);
        let context = PluginCreateContext {
            cli_command: None,
            runtime_scope: host.runtime,
            instance: scope.id(),
            role,
        };
        let Ok(Ok(implementation)) =
            catch_unwind(AssertUnwindSafe(|| definition.create(context.clone())))
        else {
            let _ = scope.close();
            let _ = host.drive(scope.drain());
            let _ = scope.finish();
            return Err(error(&manifest.id, "factory_failed_or_panicked"));
        };
        let callback_host = bmux_plugin::LoaderCallbackHost::new(
            scope.clone(),
            Arc::new((implementation.clone(), host.driver.clone())),
            host.driver.runtime_handle(),
        );
        let instance = Arc::new(Self {
            scope,
            context,
            callback_host: Mutex::new(Some(callback_host)),
            host,
            state: Mutex::new(State {
                phase: Phase::Created,
                operation: false,
                implementation: Some(implementation),
                activation: None,
                resources: None,
                publication: None,
                partial_services: None,
                calls: BTreeMap::new(),
                failures: Vec::new(),
                destruction_uncertain: false,
                author_cleaned: false,
            }),
            product_host: Mutex::new(None),
            host_sessions: Mutex::new(BTreeMap::new()),
            retained: Mutex::new(None),
            retirements: std::sync::Condvar::new(),
        });
        *instance.retained.lock().expect("fresh retention") = Some(instance.clone());
        Ok(instance)
    }
    fn reserve(&self, id: &str) -> Result<Reservation<'_>, PluginLoadError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error(id, "lifecycle_state_poisoned"))?;
        if state.operation {
            return Err(error(id, "lifecycle_operation_in_progress"));
        }
        state.operation = true;
        drop(state);
        Ok(Reservation(self))
    }
    fn implementation(&self, id: &str) -> Result<Arc<dyn PluginInstance>, PluginLoadError> {
        self.state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .implementation
            .clone()
            .ok_or_else(|| error(id, "implementation_retired"))
    }
    fn callback_context(
        &self,
        invocation: &InvocationScope,
        cancellation: bmux_plugin_sdk::ServiceCancellation,
    ) -> Result<bmux_plugin_sdk::PluginCallbackContext, PluginLoadError> {
        let activation = self
            .state
            .lock()
            .map_err(|_| error(&self.host.native.plugin_id, "state_poisoned"))?
            .activation
            .clone()
            .ok_or_else(|| error(&self.host.native.plugin_id, "callback_activation_missing"))?;
        let callback_host = self
            .callback_host
            .lock()
            .map_err(|_| error(&self.host.native.plugin_id, "callback_host_poisoned"))?
            .clone()
            .ok_or_else(|| error(&self.host.native.plugin_id, "callback_host_retired"))?;
        callback_host
            .bind_root(&activation, invocation.clone(), cancellation)
            .map(|binding| binding.context())
            .map_err(|_| {
                error(
                    &self.host.native.plugin_id,
                    "callback_authority_unavailable",
                )
            })
    }

    fn with_lifecycle<T>(
        &self,
        activation: &ActivationScope,
        kind: bmux_plugin_runtime::LifecycleInvocationKind,
        work: impl FnOnce(PluginLifecycleContext) -> Result<T, PluginLoadError>,
    ) -> Result<T, PluginLoadError> {
        let id = &self.host.native.plugin_id;
        let invocation = activation
            .begin_lifecycle_invocation(kind)
            .map_err(|_| error(id, "lifecycle_invocation_unavailable"))?;
        let cancellation = bmux_plugin_sdk::ServiceCancellation::new();
        self.state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .calls
            .insert(
                invocation.id(),
                CallOwner {
                    scope: invocation.clone(),
                    cancellation: bcode_plugin_sdk::ServiceCancellation::from_shared(
                        cancellation.clone(),
                    ),
                    settled: false,
                    retiring: false,
                },
            );
        let result = (|| {
            let callback_execution = self.callback_context(&invocation, cancellation)?;
            work(PluginLifecycleContext {
                callback_execution: Some(callback_execution),
                scope: self.context.clone(),
                activation: activation.id(),
                native: self.host.native.clone(),
                // Bcode registers no activation binders: product capabilities reach
                // implementations through scoped host services, never in-process values.
                capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
            })
        })();
        self.state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .calls
            .get_mut(&invocation.id())
            .ok_or_else(|| error(id, "lifecycle_owner_missing"))?
            .settled = true;
        self.retire_call(id, invocation.id())?;
        result
    }
    fn failure(
        &self,
        id: &str,
        stage: &'static str,
        details: impl Into<String>,
    ) -> PluginLoadError {
        let details = details.into().chars().take(4096).collect::<String>();
        if let Ok(mut state) = self.state.lock() {
            state.phase = Phase::CleanupRequired;
            if state.failures.len() == 128 {
                state.failures.remove(0);
            }
            state.failures.push(format!("{stage}: {details}"));
        }
        error(id, stage)
    }
    pub fn activation_scope(&self) -> bmux_plugin_sdk::definition::PluginResult<ActivationScope> {
        let state = self.state.lock().map_err(|_| {
            bmux_plugin_sdk::PluginCommandError::unavailable("selected activation state poisoned")
        })?;
        if state.phase != Phase::Active {
            return Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                "selected activation is not active",
            ));
        }
        let activation = state.activation.clone().ok_or_else(|| {
            bmux_plugin_sdk::PluginCommandError::unavailable("selected activation unavailable")
        })?;
        drop(state);
        Ok(activation)
    }

    fn ensure_product_host(&self) -> Result<crate::ProductHostSession, PluginLoadError> {
        let id = &self.host.native.plugin_id;
        let mut binding = self
            .product_host
            .lock()
            .map_err(|_| error(id, "retained_binding_poisoned"))?;
        if let Some(session) = binding.as_ref() {
            return Ok(session.clone());
        }
        let activation = self
            .state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .activation
            .clone()
            .ok_or_else(|| error(id, "retained_activation_unavailable"))?;
        let owner = self
            .retained
            .lock()
            .map_err(|_| error(id, "retained_owner_poisoned"))?
            .as_ref()
            .cloned()
            .ok_or_else(|| error(id, "retained_owner_unavailable"))?;
        let session = self
            .host
            .retain_product_host(&activation, self.host.services.clone(), owner)
            .map_err(|_| error(id, "retained_host_admission"))?;
        self.host_sessions
            .lock()
            .map_err(|_| error(id, "retained_recovery_poisoned"))?
            .insert(session.id(), session.clone());
        *binding = Some(session.clone());
        drop(binding);
        Ok(session)
    }
    fn bound_host_services(&self) -> bmux_plugin_sdk::definition::PluginHostServices {
        let owner = self
            .retained
            .lock()
            .ok()
            .and_then(|owner| owner.as_ref().map(Arc::downgrade));
        let legacy_owner = owner.clone();
        self.host
            .services
            .clone()
            .with_retained_host(move || {
                legacy_owner
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "selected retained owner unavailable".to_string(),
                    })?
                    .ensure_product_host()
                    .map(|session| session.retained_port())
                    .map_err(|_| bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "selected retained host admission failed".to_string(),
                    })
            })
            .with_retained_host_v2(move || {
                // This selected session implements exact non-mutating lookup over
                // the same retained ledger. V2 acquisition is explicit, not a retry
                // through the legacy opener after a selected V2 failure.
                owner
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "selected retained V2 owner unavailable".to_string(),
                    })?
                    .ensure_product_host()
                    .map(|session| session.retained_port())
                    .map_err(|_| bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "selected retained V2 host admission failed".to_string(),
                    })
            })
    }

    pub(super) fn retain_host_session(
        self: &Arc<Self>,
        services: bmux_plugin_sdk::definition::PluginHostServices,
        limits: super::shared_activation::ProductHostLimits,
    ) -> bmux_plugin_sdk::Result<super::shared_activation::ProductHostSession> {
        let mut sessions = self.host_sessions.lock().map_err(|_| {
            bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "selected host recovery ledger poisoned".to_owned(),
            }
        })?;
        let activation =
            self.activation_scope()
                .map_err(|_| bmux_plugin_sdk::PluginError::ServiceProtocol {
                    details: "selected activation unavailable".to_owned(),
                })?;
        let session = self.host.retain_product_host_with_limits(
            &activation,
            services,
            self.clone(),
            limits,
        )?;
        sessions.insert(session.id(), session.clone());
        drop(sessions);
        Ok(session)
    }

    pub(super) fn host_session_page(
        &self,
        after: Option<RuntimeInvocationId>,
        limit: usize,
    ) -> bmux_plugin_sdk::Result<Vec<super::shared_activation::ProductHostSession>> {
        let sessions = self.host_sessions.lock().map_err(|_| {
            bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "selected host recovery ledger poisoned".to_owned(),
            }
        })?;
        let start = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let page = sessions
            .range((start, std::ops::Bound::Unbounded))
            .take(limit.min(100))
            .map(|(_, session)| session.clone())
            .collect();
        drop(sessions);
        Ok(page)
    }

    pub(super) fn forget_released_host_session(
        &self,
        id: RuntimeInvocationId,
    ) -> bmux_plugin_sdk::Result<()> {
        let mut sessions = self.host_sessions.lock().map_err(|_| {
            bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "selected host recovery ledger poisoned".to_owned(),
            }
        })?;
        if !sessions
            .get(&id)
            .is_some_and(super::shared_activation::ProductHostSession::is_released)
        {
            drop(sessions);
            return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "host session remains unresolved".to_owned(),
            });
        }
        let owner = sessions.remove(&id);
        drop(sessions);
        drop(owner);
        Ok(())
    }

    pub(super) fn start_product_host_extension(
        &self,
        job: &super::shared_activation::ProductHostJob,
        contract: String,
        version: u32,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::Result<()> {
        let reject = || bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: "retained selected extension owner or contract unavailable".to_owned(),
        };
        if payload.len() > bcode_plugin_sdk::SERVICE_BRIDGE_MAX_REQUEST_BYTES {
            return Err(reject());
        }
        let state = self.state.lock().map_err(|_| reject())?;
        let activation = state.activation.as_ref().ok_or_else(reject)?;
        let invocation = job.invocation();
        if state.phase != Phase::Active
            || invocation.runtime_id() != activation.runtime_id()
            || invocation.instance_id() != activation.instance_id()
            || invocation.activation_id().map_err(|_| reject())? != activation.id()
        {
            return Err(reject());
        }
        let implementation = state.implementation.clone().ok_or_else(reject)?;
        let plugin_id = self.host.native.plugin_id.clone();
        let role = self.context.role;
        let services = job.services();
        let cancellation = bmux_plugin_sdk::ServiceCancellation::new();
        drop(state);
        let callback_execution = self
            .callback_context(job.invocation(), cancellation.clone())
            .map_err(|_| reject())?;
        job.start_operation(
            "retained-selected-extension",
            payload
                .len()
                .checked_add(contract.len())
                .ok_or_else(reject)?,
            move |mut cancelled| async move {
                let call = PluginCallContext {
                    callback_execution: Some(callback_execution),
                    invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                    cancellation: cancellation.clone(),
                    host_services: Some(services),
                };
                let mut execution = Box::pin(async move {
                    let descriptor = implementation
                        .extension(
                            call.clone(),
                            "bcode.component-extensions/v1".to_owned(),
                            Vec::new(),
                        )
                        .await
                        .map_err(|_| reject())?;
                    if descriptor.len() > bcode_plugin_sdk::SERVICE_BRIDGE_MAX_RESPONSE_BYTES {
                        return Err(reject());
                    }
                    let descriptor: bcode_plugin_sdk::definition::ProductExtensionDescriptor =
                        serde_json::from_slice(&descriptor).map_err(|_| reject())?;
                    if descriptor.plugin_id != plugin_id
                        || descriptor.role != role
                        || !descriptor
                            .contracts
                            .iter()
                            .any(|entry| entry.interface == contract && entry.version == version)
                    {
                        return Err(reject());
                    }
                    implementation
                        .extension(call, contract, payload)
                        .await
                        .map_err(|_| reject())
                });
                tokio::select! {
                    result = &mut execution => result,
                    () = cancelled.cancelled() => {
                        cancellation.cancel();
                        // Cancellation is not settlement; retain execution and outcome.
                        execution.await
                    }
                }
            },
        )
        .map(|_| ())
    }

    pub(super) const fn role(&self) -> ComponentRole {
        self.context.role
    }

    pub(super) fn extensions(
        self: &Arc<Self>,
    ) -> Result<bcode_plugin_sdk::definition::ProductExtensions, PluginLoadError> {
        let state = self
            .state
            .lock()
            .map_err(|_| error(&self.host.native.plugin_id, "state_poisoned"))?;
        if state.phase != Phase::Active {
            return Err(error(
                &self.host.native.plugin_id,
                "extension_owner_not_active",
            ));
        }
        let provider = state
            .publication
            .as_ref()
            .map(|publication| publication.product.clone());
        drop(state);
        Ok(bcode_plugin_sdk::definition::ProductExtensions::new(
            self.clone(),
            provider,
        ))
    }

    pub(super) fn failures(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failures
            .clone()
    }
    pub(super) fn validate_selection(
        &self,
        config: &crate::ResolvedPluginConfig,
        id: &str,
    ) -> Result<(), PluginLoadError> {
        let selected = config
            .shared_activation
            .as_ref()
            .ok_or_else(|| error(id, "shared_activation_context_required"))?;
        if selected.runtime != self.host.runtime
            || selected.native != self.host.native
            || !Arc::ptr_eq(&selected.driver, &self.host.driver)
            || !Arc::ptr_eq(&selected.registrations, &self.host.registrations)
            || !Arc::ptr_eq(&selected.dispatch, &self.host.dispatch)
        {
            return Err(error(id, "shared_activation_selection_changed"));
        }
        Ok(())
    }
    pub(super) fn activate(&self, id: &str) -> Result<(), PluginLoadError> {
        let _operation = self.reserve(id)?;
        if self
            .state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .phase
            != Phase::Created
        {
            return Err(error(id, "activation_not_created"));
        }
        let result = catch_unwind(AssertUnwindSafe(|| self.activate_inner(id)));
        match result {
            Ok(Ok(())) => Ok(()),
            failure => {
                let primary = match failure {
                    Ok(Err(error)) => error,
                    _ => self.failure(
                        id,
                        "activation_panicked",
                        "author or lifecycle driver panicked",
                    ),
                };
                self.failure(id, "activation_failed", primary.to_string());
                if catch_unwind(AssertUnwindSafe(|| self.cleanup(id))).is_err() {
                    self.failure(id, "rollback_panicked", "cleanup authority retained");
                }
                Err(primary)
            }
        }
    }
    fn begin_activation(&self, id: &str) -> Result<ActivationScope, PluginLoadError> {
        let mut state = self.state.lock().map_err(|_| error(id, "state_poisoned"))?;
        if state.phase != Phase::Created {
            return Err(error(id, "activation_not_created"));
        }
        let activation = self
            .scope
            .begin_activation(self.host.concurrency)
            .map_err(|_| error(id, "begin_activation"))?;
        state.activation = Some(activation.clone());
        state.resources = Some(bcode_plugin_sdk::definition::ActivationResources::new(
            activation.clone(),
        ));
        state.phase = Phase::Activating;
        drop(state);
        Ok(activation)
    }
    fn activate_inner(&self, id: &str) -> Result<(), PluginLoadError> {
        let activation = self.begin_activation(id)?;
        let implementation = self.implementation(id)?;
        let admission = activation
            .try_admit()
            .map_err(|_| error(id, "activation_admission"))?;
        // Store registry before any author registration can mutate it. The registry
        // itself stays outside locks during the hook, then is retained even on panic.
        self.with_lifecycle(
            &activation,
            bmux_plugin_runtime::LifecycleInvocationKind::Activation,
            |context| {
                self.host
                    .drive(
                        implementation.activate_scoped(context, Some(self.bound_host_services())),
                    )
                    .map_err(|error| self.failure(id, "author_activation", error.message))
            },
        )?;
        let mut services = bmux_plugin_sdk::TypedServiceRegistry::new();
        let native = &self.host.native;
        let host_service_port = self.bound_host_services();
        let registration = catch_unwind(AssertUnwindSafe(|| {
            implementation.register_typed_services_with_context(
                bmux_plugin_sdk::TypedServiceRegistrationContext {
                    host_service_port: Some(&host_service_port),
                    plugin_id: id,
                    host_kernel_bridge: native.host_kernel_bridge.as_ref(),
                    required_capabilities: &native.required_capabilities,
                    provided_capabilities: &native.provided_capabilities,
                    services: &native.services,
                    available_capabilities: &native.available_capabilities,
                    enabled_plugins: &native.enabled_plugins,
                    plugin_search_roots: &native.plugin_search_roots,
                    host: &native.host,
                    connection: &native.connection,
                    plugin_settings_map: &native.plugin_settings_map,
                },
                &mut services,
            )
        }));
        let product = services
            .get(
                &bmux_plugin_sdk::HostScope::new("bcode.product").expect("static scope"),
                bmux_plugin_sdk::ServiceKind::Command,
                bcode_plugin_sdk::definition::PRODUCT_SERVICE_INTERFACE,
            )
            .map(bmux_plugin_sdk::TypedServiceHandle::provider_as_trait::<dyn BcodePluginService>);
        self.state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .partial_services = Some(services);
        match registration {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(self.failure(id, "typed_registration_failed", error.message));
            }
            Err(_) => {
                return Err(self.failure(
                    id,
                    "typed_registration_panicked",
                    "partial provider registry retained",
                ));
            }
        }
        let product = product
            .ok_or_else(|| error(id, "product_provider_missing"))?
            .map_err(|error| self.failure(id, "product_provider_type", error.to_string()))?;
        let bytes = self.with_lifecycle(
            &activation,
            bmux_plugin_runtime::LifecycleInvocationKind::Activation,
            |context| {
                self.host
                    .drive(
                        implementation.extension(
                            PluginCallContext {
                                callback_execution: context.callback_execution,
                                invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                                cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
                                host_services: Some(self.bound_host_services()),
                            },
                            bcode_plugin_sdk::definition::ACTIVATION_CONTRIBUTIONS_EXTENSION
                                .to_string(),
                            Vec::new(),
                        ),
                    )
                    .map_err(|error| self.failure(id, "contribution_extension", error.message))
            },
        )?;
        let snapshot: Snapshot =
            serde_json::from_slice(&bytes).map_err(PluginLoadError::ServiceDecode)?;
        if snapshot.version != bcode_plugin_models::activation::ACTIVATION_CONTRIBUTIONS_VERSION {
            return Err(error(id, "activation_contributions_version"));
        }
        let mut state = self.state.lock().map_err(|_| error(id, "state_poisoned"))?;
        activation
            .mark_active()
            .map_err(|_| error(id, "activation_transition"))?;
        state.publication = Some(Publication { product, snapshot });
        state.phase = Phase::Active;
        drop(state);
        drop(admission);
        Ok(())
    }
    fn scope_error(
        &self,
        id: &str,
        stage: &'static str,
        error: &bmux_plugin_runtime::ScopeError,
    ) -> PluginLoadError {
        self.failure(id, stage, error.to_string())
    }
    pub(super) fn snapshot(&self, id: &str) -> Result<Snapshot, PluginLoadError> {
        let state = self.state.lock().map_err(|_| error(id, "state_poisoned"))?;
        if state.phase != Phase::Active {
            return Err(error(id, "publication_not_active"));
        }
        let snapshot = state
            .publication
            .as_ref()
            .ok_or_else(|| error(id, "publication_missing"))?
            .snapshot
            .clone();
        drop(state);
        Ok(snapshot)
    }
    pub(super) fn invoke<R>(
        &self,
        context: bcode_plugin_sdk::NativeServiceContext,
        work: impl FnOnce(
            Arc<dyn BcodePluginService>,
            bcode_plugin_sdk::NativeServiceContext,
            &InvocationScope,
        ) -> R,
    ) -> Result<R, PluginLoadError> {
        let id = context.plugin_id.clone();
        if context.cancellation.is_cancelled() {
            return Err(error(&id, "invocation_cancelled_before_admission"));
        }
        let (invocation, admission, product) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| error(&id, "state_poisoned"))?;
            if state.phase != Phase::Active {
                return Err(error(&id, "dispatch_not_active"));
            }
            let product = state
                .publication
                .as_ref()
                .ok_or_else(|| error(&id, "publication_missing"))?
                .product
                .clone();
            let activation = state
                .activation
                .as_ref()
                .ok_or_else(|| error(&id, "activation_missing"))?;
            let invocation = activation
                .begin_invocation()
                .map_err(|_| error(&id, "invocation_admission"))?;
            // Retain the child and cancellation before any subsequent fallible work.
            state.calls.insert(
                invocation.id(),
                CallOwner {
                    scope: invocation.clone(),
                    cancellation: context.cancellation.clone(),
                    settled: false,
                    retiring: false,
                },
            );
            let Ok(admission) = invocation.try_admit() else {
                if let Some(call) = state.calls.get_mut(&invocation.id()) {
                    call.settled = true;
                }
                return Err(error(&id, "invocation_admission"));
            };
            drop(state);
            (invocation, admission, product)
        };
        let mut context = context;
        context.transient_progress_limits = self.host.progress_limits;
        // The trusted host work closure owns scoped callbacks and must close/drain
        // them before returning (including unwinding). The admission outlives it.
        let result = catch_unwind(AssertUnwindSafe(|| {
            if context.cancellation.is_cancelled() {
                return Err(error(&id, "invocation_cancelled_before_effects"));
            }
            Ok(work(product, context, &invocation))
        }));
        self.state
            .lock()
            .map_err(|_| error(&id, "state_poisoned"))?
            .calls
            .get_mut(&invocation.id())
            .ok_or_else(|| error(&id, "invocation_owner_missing"))?
            .settled = true;
        drop(admission);
        let retirement = self.retire_call(&id, invocation.id());
        if result.is_err() {
            return Err(self.failure(
                &id,
                "invocation_panicked",
                "operation settled by unwind; callbacks drained before retirement",
            ));
        }
        retirement?;
        result.unwrap_or_else(|_| unreachable!())
    }
    /// Cleanup retires every settled call. A call whose own completion path is
    /// retiring it concurrently is awaited, bounded by the teardown budget, not
    /// reported as a cleanup failure.
    fn retire_call_for_cleanup(
        &self,
        id: &str,
        key: RuntimeInvocationId,
    ) -> Result<(), PluginLoadError> {
        let waited = self
            .retirements
            .wait_timeout_while(
                self.state.lock().map_err(|_| error(id, "state_poisoned"))?,
                bmux_plugin_runtime::teardown_timeout(),
                |state| state.calls.get(&key).is_some_and(|call| call.retiring),
            )
            .map_err(|_| error(id, "state_poisoned"))?
            .1;
        if waited.timed_out() {
            return Err(self.failure(
                id,
                "invocation_retirement_in_progress",
                "concurrent invocation retirement exceeded the teardown budget",
            ));
        }
        self.retire_call(id, key)
    }
    fn retire_call(&self, id: &str, key: RuntimeInvocationId) -> Result<(), PluginLoadError> {
        let scope = {
            let mut state = self.state.lock().map_err(|_| error(id, "state_poisoned"))?;
            let Some(call) = state.calls.get_mut(&key) else {
                return Ok(());
            };
            if !call.settled {
                return Err(error(id, "invocation_still_running"));
            }
            if call.retiring {
                return Err(error(id, "invocation_retirement_in_progress"));
            }
            call.retiring = true;
            let scope = call.scope.clone();
            drop(state);
            scope
        };
        let _retirement = Retirement { owner: self, key };
        scope
            .close()
            .map_err(|error| self.scope_error(id, "invocation_close", &error))?;
        self.host
            .drive(scope.drain())
            .map_err(|error| self.scope_error(id, "invocation_drain", &error))?;
        scope
            .finish()
            .map_err(|error| self.scope_error(id, "invocation_finish", &error))?;
        self.state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .calls
            .remove(&key);
        Ok(())
    }
    pub(super) fn event(
        &self,
        context: bcode_plugin_sdk::NativeEventContext,
    ) -> Result<(), PluginLoadError> {
        let id = context.plugin_id.clone();
        let (admission, product) = {
            let state = self
                .state
                .lock()
                .map_err(|_| error(&id, "state_poisoned"))?;
            if state.phase != Phase::Active {
                return Err(error(&id, "event_not_active"));
            }
            let product = state
                .publication
                .as_ref()
                .ok_or_else(|| error(&id, "publication_missing"))?
                .product
                .clone();
            let admission = state
                .activation
                .as_ref()
                .ok_or_else(|| error(&id, "activation_missing"))?
                .try_admit()
                .map_err(|_| error(&id, "event_admission"))?;
            drop(state);
            (admission, product)
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            let result = product.event(context);
            drop(product);
            result
        }));
        drop(admission);
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(self.failure(&id, "event_failed", error.message())),
            Err(_) => Err(self.failure(&id, "event_panicked", "author event panicked")),
        }
    }
    pub(super) fn deactivate(&self, id: &str) -> Result<(), PluginLoadError> {
        let _operation = self.reserve(id)?;
        catch_unwind(AssertUnwindSafe(|| self.cleanup(id))).unwrap_or_else(|_| {
            Err(self.failure(
                id,
                "cleanup_panicked",
                "cleanup owner and implementation retained",
            ))
        })
    }
    fn destroy_author(&self, id: &str) -> Result<(), PluginLoadError> {
        // Take resources under the lock; destructors run outside it, with panic
        // containment. A destruction panic is never silently retried as success.
        let resources = {
            let mut state = self.state.lock().map_err(|_| error(id, "state_poisoned"))?;
            state.destruction_uncertain = true;
            (
                state.publication.take(),
                state.partial_services.take(),
                state.implementation.take(),
            )
        };
        if catch_unwind(AssertUnwindSafe(|| drop(resources))).is_err() {
            return Err(self.failure(
                id,
                "author_destruction_panicked",
                "destruction outcome uncertain; explicit maintenance required",
            ));
        }
        self.state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .destruction_uncertain = false;
        Ok(())
    }

    fn cleanup(&self, id: &str) -> Result<(), PluginLoadError> {
        let (activation, cancellations) = {
            let mut state = self.state.lock().map_err(|_| error(id, "state_poisoned"))?;
            if state.phase == Phase::Destroyed {
                return Ok(());
            }
            if state.destruction_uncertain {
                return Err(error(id, "author_destruction_uncertain"));
            }
            state.phase = Phase::Closing;
            (
                state.activation.clone(),
                state
                    .calls
                    .values()
                    .map(|call| call.cancellation.clone())
                    .collect::<Vec<_>>(),
            )
        };
        for cancellation in cancellations {
            cancellation.cancel();
        }
        if let Some(activation) = &activation {
            activation
                .close()
                .map_err(|error| self.scope_error(id, "activation_close", &error))?;
        }
        self.host
            .drive(self.host.registrations.revoke_and_drain())?;
        if let Some(activation) = &activation {
            // Runtime drain may fail before proving admission/task settlement.
            // Cleanup errors require explicit checked recovery below, never an
            // assumption that an error means the subtree is already drained.
            match self
                .host
                .drive(activation.drain_until(bmux_plugin_runtime::teardown_deadline()))
            {
                Ok(()) => {}
                Err(bmux_plugin_runtime::ScopeError::Cleanup(details)) => {
                    self.failure(id, "retained_runtime_cleanup", details);
                    let calls = self
                        .state
                        .lock()
                        .map_err(|_| error(id, "state_poisoned"))?
                        .calls
                        .keys()
                        .copied()
                        .collect::<Vec<_>>();
                    for key in calls {
                        self.retire_call_for_cleanup(id, key)?;
                    }
                    // This typed acknowledgement checks zero admissions, settled
                    // tasks, no children/resources. Failure keeps cleanup closed;
                    // never treat a generic Cleanup error as proof of draining.
                    activation
                        .resolve_cleanup()
                        .map_err(|error| self.scope_error(id, "resolve_settled_cleanup", &error))?;
                }
                Err(error) => return Err(self.scope_error(id, "activation_drain", &error)),
            }
            let calls = self
                .state
                .lock()
                .map_err(|_| error(id, "state_poisoned"))?
                .calls
                .keys()
                .copied()
                .collect::<Vec<_>>();
            for key in calls {
                self.retire_call_for_cleanup(id, key)?;
            }
            let author_cleaned = self
                .state
                .lock()
                .map_err(|_| error(id, "state_poisoned"))?
                .author_cleaned;
            if !author_cleaned {
                let implementation = self.implementation(id)?;
                self.with_lifecycle(
                    activation,
                    bmux_plugin_runtime::LifecycleInvocationKind::Cleanup,
                    |context| {
                        self.host
                            .drive(
                                implementation
                                    .deactivate_scoped(context, Some(self.bound_host_services())),
                            )
                            .map_err(|error| self.failure(id, "author_deactivation", error.message))
                    },
                )?;
                drop(implementation);
                self.state
                    .lock()
                    .map_err(|_| error(id, "state_poisoned"))?
                    .author_cleaned = true;
            }
        }
        self.destroy_author(id)?;
        self.retire_host_sessions(id)?;
        self.retire_activation_resources(id)?;
        self.finish_scopes(id, activation)
    }

    /// Retire every retained product host session of this activation before the
    /// activation itself finishes. Each session is an activation child invocation
    /// that owns a registered resource, so `activation.finish()` cannot succeed
    /// while one is unreleased. This runs only after author deactivation and
    /// destruction, so no author observer can admit further retained work.
    ///
    /// Release keeps its strict contract: it succeeds only after an explicit
    /// join and only when every job was already acknowledged. Unresolved work is
    /// never discarded; it fails cleanup closed and remains reachable through the
    /// recovery ledger (`host_session_page`) for an explicit host retry.
    fn retire_host_sessions(&self, id: &str) -> Result<(), PluginLoadError> {
        let sessions = self
            .host_sessions
            .lock()
            .map_err(|_| error(id, "retained_recovery_poisoned"))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            if session.is_released() {
                continue;
            }
            self.host
                .drive(super::shared_activation::bounded_join(&session))
                .map_err(|failure| self.failure(id, "retained_host_join", failure.to_string()))?;
            session.release().map_err(|failure| {
                self.failure(id, "retained_host_release", failure.to_string())
            })?;
        }
        Ok(())
    }

    /// Retire host resources still charged to the activation. The activation has
    /// drained and the author cleanup hook settled, so release can only fail for
    /// genuinely retained work, which keeps cleanup closed for explicit recovery.
    fn retire_activation_resources(&self, id: &str) -> Result<(), PluginLoadError> {
        let resources = self
            .state
            .lock()
            .map_err(|_| error(id, "state_poisoned"))?
            .resources
            .clone();
        if let Some(resources) = resources {
            resources
                .retire_all()
                .map_err(|error| self.scope_error(id, "activation_resource_release", &error))?;
        }
        Ok(())
    }

    fn finish_scopes(
        &self,
        id: &str,
        activation: Option<ActivationScope>,
    ) -> Result<(), PluginLoadError> {
        if let Some(activation) = activation {
            if activation
                .state()
                .map_err(|error| self.scope_error(id, "activation_state", &error))?
                == bmux_plugin_runtime::LifecycleState::CleanupRequired
            {
                activation
                    .resolve_cleanup()
                    .map_err(|error| self.scope_error(id, "resolve_cleanup", &error))?;
            }
            activation
                .finish()
                .map_err(|error| self.scope_error(id, "activation_finish", &error))?;
        }
        self.scope
            .close()
            .map_err(|error| self.scope_error(id, "instance_close", &error))?;
        self.host
            .drive(
                self.scope
                    .drain_until(bmux_plugin_runtime::teardown_deadline()),
            )
            .map_err(|error| self.scope_error(id, "instance_drain", &error))?;
        self.scope
            .finish()
            .map_err(|error| self.scope_error(id, "instance_finish", &error))?;
        {
            let mut state = self.state.lock().map_err(|_| error(id, "state_poisoned"))?;
            state.activation = None;
            state.resources = None;
            state.phase = Phase::Destroyed;
        }
        // Only committed scope retirement permits releasing the instance's
        // executor/code owner. Outstanding bindings retain their own pins.
        let callback_host = self
            .callback_host
            .lock()
            .map_err(|_| error(id, "callback_host_poisoned"))?
            .take();
        drop(callback_host);
        let retained = self
            .retained
            .lock()
            .map_err(|_| error(id, "retention_poisoned"))?
            .take();
        drop(retained);
        Ok(())
    }
}
impl bcode_plugin_sdk::definition::ProductExtensionPort for SharedInstance {
    fn product_host_session(
        &self,
    ) -> Result<
        bcode_plugin_sdk::definition::host_session::ProductHostSessionClient,
        bcode_plugin_sdk::definition::host_session::HostSessionClientError,
    > {
        use bcode_plugin_sdk::definition::host_session::HostSessionClientError;
        self.ensure_product_host()
            .map_err(|_| {
                HostSessionClientError::Uncertain(
                    bcode_plugin_sdk::definition::host_session_models::SessionCode::Unavailable,
                )
            })?
            .typed_client()
    }
    fn activation_scope(&self) -> bmux_plugin_sdk::definition::PluginResult<ActivationScope> {
        Self::activation_scope(self)
    }
    fn activation_resources(
        &self,
    ) -> bmux_plugin_sdk::definition::PluginResult<bcode_plugin_sdk::definition::ActivationResources>
    {
        let state = self.state.lock().map_err(|_| {
            bmux_plugin_sdk::PluginCommandError::unavailable("selected activation state poisoned")
        })?;
        if state.phase != Phase::Active {
            return Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                "selected activation is not active",
            ));
        }
        let resources = state.resources.clone().ok_or_else(|| {
            bmux_plugin_sdk::PluginCommandError::unavailable("selected activation unavailable")
        })?;
        drop(state);
        Ok(resources)
    }
    fn ensure_active(&self) -> bmux_plugin_sdk::definition::PluginResult<()> {
        let state = self.state.lock().map_err(|_| {
            bmux_plugin_sdk::PluginCommandError::unavailable("selected owner state poisoned")
        })?;
        if state.phase != Phase::Active {
            return Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                "selected owner is not active",
            ));
        }
        drop(state);
        Ok(())
    }
    fn invoke_with_host(
        &self,
        contract: String,
        payload: Vec<u8>,
        services: bmux_plugin_sdk::definition::PluginHostServices,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        bmux_plugin_sdk::definition::PluginResult<Vec<u8>>,
    > {
        Box::pin(async move {
            let id = self.host.native.plugin_id.clone();
            let implementation = self.implementation(&id).map_err(|_| {
                bmux_plugin_sdk::PluginCommandError::unavailable("selected implementation retired")
            })?;
            let context = bcode_plugin_sdk::NativeServiceContext {
                plugin_id: id.clone(),
                request: bcode_plugin_sdk::ServiceRequest {
                    interface_id: contract.clone(),
                    operation: "extension".to_string(),
                    payload: Vec::new(),
                },
                config: bcode_plugin_sdk::PluginConfigContext {
                    config: serde_json::Value::Null,
                    redacted_config: serde_json::Value::Null,
                    secrets: BTreeMap::default(),
                    state_root: None,
                    durable_state_root: None,
                },
                events: bcode_plugin_sdk::ServiceEventEmitter::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                bridge: bcode_plugin_sdk::ServiceBridge::default(),
                transient_progress_limits: self.host.progress_limits,
            };

            Self::invoke(self, context, |_, _, invocation| {
                let dispatch = Arc::new(super::shared_invocation_host::ExtensionDispatch {
                    invocation: invocation.clone(),
                    services,
                });
                let asynchronous = dispatch.clone();
                let async_scope = invocation.clone();
                let bound = invocation.clone();
                let host = bmux_plugin_sdk::definition::PluginHostServices::new(move |request| {
                    bmux_plugin_sdk::HostServiceDispatch::invoke(dispatch.as_ref(), &bound, request)
                })
                .with_async(move |request| {
                    bmux_plugin_sdk::HostServiceDispatch::invoke_async(
                        asynchronous.as_ref(),
                        &async_scope,
                        request,
                    )
                });
                let cancellation = bmux_plugin_sdk::ServiceCancellation::new();
                let callback_execution = self
                    .callback_context(invocation, cancellation.clone())
                    .map_err(|_| {
                        bmux_plugin_sdk::PluginCommandError::unavailable(
                            "callback authority unavailable",
                        )
                    })?;
                let call = PluginCallContext {
                    callback_execution: Some(callback_execution),
                    invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                    cancellation,
                    host_services: Some(host),
                };
                self.host
                    .drive(implementation.extension(call, contract, payload))
            })
            .map_err(|_| {
                bmux_plugin_sdk::PluginCommandError::unavailable(
                    "selected extension execution failed",
                )
            })?
        })
    }
    fn invoke(
        &self,
        contract: String,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        bmux_plugin_sdk::definition::PluginResult<Vec<u8>>,
    > {
        Box::pin(async move {
            let id = self.host.native.plugin_id.clone();
            let implementation = self.implementation(&id).map_err(|_| {
                bmux_plugin_sdk::PluginCommandError::unavailable("selected implementation retired")
            })?;
            let context = bcode_plugin_sdk::NativeServiceContext {
                plugin_id: id.clone(),
                request: bcode_plugin_sdk::ServiceRequest {
                    interface_id: contract.clone(),
                    operation: "extension".to_string(),
                    payload: Vec::new(),
                },
                config: bcode_plugin_sdk::PluginConfigContext {
                    config: serde_json::Value::Null,
                    redacted_config: serde_json::Value::Null,
                    secrets: BTreeMap::default(),
                    state_root: None,
                    durable_state_root: None,
                },
                events: bcode_plugin_sdk::ServiceEventEmitter::default(),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                bridge: bcode_plugin_sdk::ServiceBridge::default(),
                transient_progress_limits: self.host.progress_limits,
            };

            Self::invoke(self, context, |_, _, invocation| {
                let cancellation = bmux_plugin_sdk::ServiceCancellation::new();
                let callback_execution = self
                    .callback_context(invocation, cancellation.clone())
                    .map_err(|_| {
                        bmux_plugin_sdk::PluginCommandError::unavailable(
                            "callback authority unavailable",
                        )
                    })?;
                let call = PluginCallContext {
                    callback_execution: Some(callback_execution),
                    invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                    cancellation,
                    host_services: Some(self.bound_host_services()),
                };
                self.host
                    .drive(implementation.extension(call, contract, payload))
            })
            .map_err(|_| {
                bmux_plugin_sdk::PluginCommandError::unavailable(
                    "selected extension execution failed",
                )
            })?
        })
    }
}

pub fn error(plugin_id: &str, hook: &'static str) -> PluginLoadError {
    PluginLoadError::LifecycleFailed {
        plugin_id: plugin_id.to_string(),
        hook,
        code: -1,
    }
}
