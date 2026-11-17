//! Shared native ABI loading. Retained read/ack outcomes never replay plugin work.
use super::shared_activation::bounded_join;
use crate::{PluginLoadError, PluginManifest};
use bmux_plugin_sdk::abi::{NativeOperation, NativeOutcome};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[path = "runtime_native_admission.rs"]
mod admission;
#[path = "runtime_native_blocking.rs"]
mod blocking;
#[path = "runtime_native_lifecycle.rs"]
mod lifecycle;
use lifecycle::{Ledger, Phase, StepKind};

/// Wake target for one polled native operation.
#[derive(Default)]
struct OperationWake(tokio::sync::Notify);
impl std::task::Wake for OperationWake {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.notify_one();
    }
}

struct LifecycleReservation<'a>(&'a AtomicBool);
impl Drop for LifecycleReservation<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

type Snapshot = bcode_plugin_models::activation::ActivationContributions;

type HostSessionRecovery = Arc<
    Mutex<
        std::collections::BTreeMap<
            bmux_plugin_runtime::RuntimeInvocationId,
            crate::ProductHostSession,
        >,
    >,
>;
struct NativeSelectedHost {
    activation: bmux_plugin_runtime::ActivationScope,
    context: crate::SharedActivationContext,
    execution: std::sync::Weak<bmux_plugin::LoadedExecution>,
    binding: Arc<Mutex<Option<crate::ProductHostSession>>>,
    recovery: HostSessionRecovery,
}
impl NativeSelectedHost {
    fn session(&self) -> bmux_plugin_sdk::Result<crate::ProductHostSession> {
        let mut binding = self.binding.lock().map_err(|_| native_host_error())?;
        if let Some(session) = binding.as_ref() {
            return Ok(session.clone());
        }
        let owner = self.execution.upgrade().ok_or_else(native_host_error)?;
        let session = self.context.retain_product_host(
            &self.activation,
            self.context.services.clone(),
            owner,
        )?;
        self.recovery
            .lock()
            .map_err(|_| native_host_error())?
            .insert(session.id(), session.clone());
        *binding = Some(session.clone());
        drop(binding);
        Ok(session)
    }
}
fn native_host_error() -> bmux_plugin_sdk::PluginError {
    bmux_plugin_sdk::PluginError::ServiceProtocol {
        details: "selected retained host unavailable".to_string(),
    }
}
impl bmux_plugin_sdk::abi::retained::RetainedHostDispatch for NativeSelectedHost {
    fn begin(
        &self,
        id: u64,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        reservation: bmux_plugin_sdk::abi::retained::NativeRetainedReservationV1,
    ) -> bmux_plugin_sdk::Result<u64> {
        bmux_plugin_sdk::abi::retained::RetainedHostDispatch::begin(
            &self.session()?,
            id,
            request,
            reservation,
        )
    }
    fn lookup(
        &self,
        id: u64,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        reservation: bmux_plugin_sdk::abi::retained::NativeRetainedReservationV1,
    ) -> bmux_plugin_sdk::Result<Option<u64>> {
        let session = self
            .binding
            .lock()
            .map_err(|_| native_host_error())?
            .clone();
        // Recovery observes the already-bound owner; it must never create a
        // session, admit producer work, or reopen a closed activation.
        session.as_ref().map_or(Ok(None), |session| {
            bmux_plugin_sdk::abi::retained::RetainedHostDispatch::lookup(
                session,
                id,
                request,
                reservation,
            )
        })
    }
    fn observe(
        &self,
        operation: u64,
        bytes: u64,
    ) -> bmux_plugin_sdk::Result<bmux_plugin_sdk::abi::retained::RetainedHostObservation> {
        bmux_plugin_sdk::abi::retained::RetainedHostDispatch::observe(
            &self.session()?,
            operation,
            bytes,
        )
    }
    fn control(&self, operation: u64, control: u32) -> bmux_plugin_sdk::Result<bool> {
        bmux_plugin_sdk::abi::retained::RetainedHostDispatch::control(
            &self.session()?,
            operation,
            control,
        )
    }
    fn acknowledge(&self, operation: u64, receipt: u64) -> bmux_plugin_sdk::Result<()> {
        bmux_plugin_sdk::abi::retained::RetainedHostDispatch::acknowledge(
            &self.session()?,
            operation,
            receipt,
        )
    }
    fn release(&self) -> bmux_plugin_sdk::Result<bool> {
        let binding = self.binding.lock().map_err(|_| native_host_error())?;
        binding.as_ref().map_or(
            Ok(true),
            bmux_plugin_sdk::abi::retained::RetainedHostDispatch::release,
        )
    }
}

pub struct SharedNative {
    execution: Arc<bmux_plugin::LoadedExecution>,
    host: crate::SharedActivationContext,
    snapshot: Mutex<Option<Snapshot>>,
    lifecycle: Mutex<Ledger>,
    lifecycle_busy: AtomicBool,
    workers: Arc<blocking::NativeWorkers>,
    control_workers: Arc<blocking::NativeWorkers>,
    selected_host: Arc<Mutex<Option<Arc<NativeSelectedHost>>>>,
    // The loader's actual activation, observed when it binds the retained host,
    // with the ledger of host resources charged to it.
    activation: Arc<Mutex<Option<bcode_plugin_sdk::definition::ActivationResources>>>,
    host_sessions: Arc<
        Mutex<
            std::collections::BTreeMap<
                bmux_plugin_runtime::RuntimeInvocationId,
                super::shared_activation::ProductHostSession,
            >,
        >,
    >,
    unused_invocations: Mutex<Vec<bmux_plugin_runtime::InvocationScope>>,
}
impl std::fmt::Debug for SharedNative {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedNative").finish_non_exhaustive()
    }
}
impl SharedNative {
    pub(super) fn retain_host_session(
        self: &Arc<Self>,
        services: bmux_plugin_sdk::definition::PluginHostServices,
        limits: super::shared_activation::ProductHostLimits,
    ) -> bmux_plugin_sdk::Result<super::shared_activation::ProductHostSession> {
        let reject = || bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: "selected native retained host admission unavailable".to_owned(),
        };
        let mut sessions = self.host_sessions.lock().map_err(|_| reject())?;
        let scope = self
            .admit_active(
                &self.host.native.plugin_id,
                "retained_host_admission",
                || self.execution.begin_invocation_scope(),
            )
            .map_err(|_| reject())?;
        // The registered session resource pins the actual native execution/image;
        // author table references are observers, never the final cleanup owner.
        // Retained work runs on the selected reactor, not the caller's runtime.
        let _reactor = self.host.driver.runtime_handle().enter();
        let session = super::shared_activation::ProductHostSession::from_scope(
            scope,
            services,
            Arc::clone(self),
            limits,
        )?;
        sessions.insert(session.id(), session.clone());
        drop(sessions);
        Ok(session)
    }

    pub(super) fn host_session_page(
        &self,
        after: Option<bmux_plugin_runtime::RuntimeInvocationId>,
        limit: usize,
    ) -> bmux_plugin_sdk::Result<Vec<super::shared_activation::ProductHostSession>> {
        let sessions = self.host_sessions.lock().map_err(|_| {
            bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "native host recovery ledger poisoned".to_owned(),
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
        id: bmux_plugin_runtime::RuntimeInvocationId,
    ) -> bmux_plugin_sdk::Result<()> {
        let mut sessions = self.host_sessions.lock().map_err(|_| {
            bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "native host recovery ledger poisoned".to_owned(),
            }
        })?;
        if !sessions
            .get(&id)
            .is_some_and(super::shared_activation::ProductHostSession::is_released)
        {
            drop(sessions);
            return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "native host session remains unresolved".to_owned(),
            });
        }
        let owner = sessions.remove(&id);
        drop(sessions);
        drop(owner);
        Ok(())
    }

    pub(super) fn load(
        path: &std::path::Path,
        manifest: &PluginManifest,
        host: crate::SharedActivationContext,
    ) -> Result<Self, PluginLoadError> {
        Self::load_component(
            path,
            manifest,
            host,
            bmux_plugin_sdk::definition::ComponentRole::Backend,
        )
    }

    pub(super) fn load_component(
        path: &std::path::Path,
        manifest: &PluginManifest,
        host: crate::SharedActivationContext,
        role: bmux_plugin_sdk::definition::ComponentRole,
    ) -> Result<Self, PluginLoadError> {
        host.validate(&manifest.id)?;
        // A dynamic artifact's plug-rt binds to the reactor current at load time.
        // Bind it to this activation's selected lifecycle reactor, never to
        // whichever runtime the caller happens to be polled on.
        let reactor = host.driver.runtime_handle();
        let entered = reactor.enter();
        // SAFETY: Product native loading is explicitly trusted, as with the prior loader.
        let artifact = unsafe { bmux_plugin::NativePluginLoader::load_definition_artifact(path) }
            .map_err(|_| error(&manifest.id, "shared_native_load"));
        drop(entered);
        let artifact = artifact?;
        if artifact.metadata().plugin_id != manifest.id {
            return Err(PluginLoadError::ManifestIdMismatch {
                file_id: manifest.id.clone(),
                library_id: artifact.metadata().plugin_id.clone(),
            });
        }
        // The embedded declaration, not merely the exported logical ID, must match.
        let embedded = artifact
            .metadata()
            .manifest_toml
            .as_ref()
            .ok_or_else(|| error(&manifest.id, "embedded_manifest"))?;
        let embedded: PluginManifest =
            toml::from_str(embedded).map_err(|_| error(&manifest.id, "embedded_manifest"))?;
        if !crate::embedded_manifest_matches(embedded, manifest) {
            return Err(error(&manifest.id, "embedded_manifest_mismatch"));
        }
        let execution = artifact
            .create(host.runtime, role, &host.native.available_capabilities)
            .map_err(|_| error(&manifest.id, "shared_native_create"))?;
        let product_host = Arc::new(Mutex::new(None));
        let host_sessions = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
        let binding = Arc::clone(&product_host);
        let recovery = Arc::clone(&host_sessions);
        let context = host.clone();
        let retained_execution = Arc::downgrade(&execution);
        let selected_host = Arc::new(Mutex::new(None));
        let selected_binding = Arc::clone(&selected_host);
        let selected_activation = Arc::new(Mutex::new(None));
        let activation_binding = Arc::clone(&selected_activation);
        execution
            .bind_retained_host_factory(Arc::new(move |activation| {
                *activation_binding.lock().map_err(|_| {
                    bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "native selected activation poisoned".to_string(),
                    }
                })? = Some(bcode_plugin_sdk::definition::ActivationResources::new(
                    activation.clone(),
                ));
                let dispatch = Arc::new(NativeSelectedHost {
                    activation: activation.clone(),
                    context: context.clone(),
                    execution: retained_execution.clone(),
                    binding: Arc::clone(&binding),
                    recovery: Arc::clone(&recovery),
                });
                *selected_binding.lock().map_err(|_| {
                    bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "native selected host poisoned".to_string(),
                    }
                })? = Some(Arc::clone(&dispatch));
                Ok(dispatch as Arc<dyn bmux_plugin_sdk::abi::retained::RetainedHostDispatch>)
            }))
            .map_err(|_| error(&manifest.id, "native_retained_factory_negotiation"))?;
        Ok(Self {
            selected_host,
            activation: selected_activation,
            host_sessions,
            execution,
            host,
            snapshot: Mutex::new(None),
            lifecycle: Mutex::new(Ledger::new()),
            lifecycle_busy: AtomicBool::new(false),
            workers: Arc::new(blocking::NativeWorkers::new(
                std::num::NonZeroUsize::new(bmux_plugin_sdk::abi::MAX_OPERATIONS)
                    .expect("native operation capacity is positive"),
            )),
            control_workers: Arc::new(blocking::NativeWorkers::new(
                std::num::NonZeroUsize::new(1).expect("control capacity is positive"),
            )),
            unused_invocations: Mutex::new(Vec::new()),
        })
    }

    pub(super) fn role(&self) -> bmux_plugin_sdk::definition::ComponentRole {
        self.execution.create_context().role
    }

    fn settle(&self, id: u64, plugin_id: &str) -> Result<NativeOutcome, PluginLoadError> {
        self.settle_cancelled(id, plugin_id, None)
    }
    fn settle_cancelled(
        &self,
        id: u64,
        plugin_id: &str,
        cancellation: Option<&bcode_plugin_sdk::ServiceCancellation>,
    ) -> Result<NativeOutcome, PluginLoadError> {
        let settled = self
            .host
            .drive(self.settlement(id, cancellation.cloned(), true));
        self.settled(plugin_id, settled)
    }

    /// Settle an operation from an asynchronous caller without blocking its
    /// thread. The settlement runs to acknowledgement on the selected reactor,
    /// so dropping this observer never strands an unacknowledged outcome.
    async fn settle_async(
        &self,
        id: u64,
        plugin_id: &str,
    ) -> Result<NativeOutcome, PluginLoadError> {
        let settled = self
            .host
            .driver
            .runtime_handle()
            .spawn(self.settlement(id, None, true))
            .await
            .unwrap_or_else(|_| {
                Err(SettleFailure::recorded(
                    "native_settlement_task",
                    "native settlement task did not complete".to_owned(),
                ))
            });
        self.settled(plugin_id, settled)
    }

    fn settled(
        &self,
        plugin_id: &str,
        settled: Result<NativeOutcome, SettleFailure>,
    ) -> Result<NativeOutcome, PluginLoadError> {
        let outcome = settled.map_err(|failure| self.settle_failure(plugin_id, &failure))?;
        if matches!(outcome, NativeOutcome::Error { .. }) {
            return Err(error(plugin_id, "shared_native_operation"));
        }
        Ok(outcome)
    }

    fn settle_failure(&self, plugin_id: &str, failure: &SettleFailure) -> PluginLoadError {
        if failure.record {
            self.record_failure(plugin_id, failure.stage, &failure.details)
        } else {
            error(plugin_id, failure.stage)
        }
    }

    fn poll_outcome(&self, id: u64, plugin_id: &str) -> Result<NativeOutcome, PluginLoadError> {
        let settled = self.host.drive(self.settlement(id, None, false));
        settled.map_err(|failure| self.settle_failure(plugin_id, &failure))
    }

    /// Observe one operation to its final outcome, then optionally acknowledge
    /// it. The future owns its execution and workers, so it may run on any
    /// reactor and outlive the caller that started it.
    fn settlement(
        &self,
        id: u64,
        cancellation: Option<bcode_plugin_sdk::ServiceCancellation>,
        acknowledge: bool,
    ) -> impl std::future::Future<Output = Result<NativeOutcome, SettleFailure>> + Send + 'static
    {
        let execution = Arc::clone(&self.execution);
        let workers = Arc::clone(&self.workers);
        let control = Arc::clone(&self.control_workers);
        async move {
            let outcome =
                observe_outcome(&execution, &workers, &control, id, cancellation.as_ref()).await?;
            if acknowledge {
                execution
                    .acknowledge(id)
                    .await
                    .map_err(|failure| SettleFailure {
                        stage: "shared_native_ack",
                        details: failure.to_string(),
                        record: false,
                    })?;
            }
            Ok(outcome)
        }
    }

    fn reserve_lifecycle(
        &self,
        plugin_id: &str,
    ) -> Result<LifecycleReservation<'_>, PluginLoadError> {
        self.lifecycle_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| error(plugin_id, "native_lifecycle_operation_in_progress"))?;
        Ok(LifecycleReservation(&self.lifecycle_busy))
    }

    fn record_failure(
        &self,
        plugin_id: &str,
        stage: &'static str,
        details: &str,
    ) -> PluginLoadError {
        if let Ok(mut ledger) = self.lifecycle.lock() {
            ledger.failed(stage, details);
        }
        error(plugin_id, stage)
    }

    pub fn failures(&self) -> Vec<String> {
        self.lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failures
            .clone()
    }

    fn start_step(&self, plugin_id: &str, id: u64, kind: StepKind) -> Result<(), PluginLoadError> {
        self.lifecycle
            .lock()
            .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?
            .started(id, kind)
            .map_err(|_| error(plugin_id, "native_lifecycle_already_pending"))
    }

    // Outcome observation and acknowledgment are separate durable-in-owner steps.
    // An ack failure must reuse this operation, never execute the hook again.
    fn finish_step(&self, plugin_id: &str) -> Result<NativeOutcome, PluginLoadError> {
        let (id, kind, cached) = {
            let ledger = self
                .lifecycle
                .lock()
                .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?;
            let step = ledger
                .pending
                .as_ref()
                .ok_or_else(|| error(plugin_id, "native_lifecycle_missing"))?;
            let retained = (step.id, step.kind, step.outcome.clone());
            drop(ledger);
            retained
        };
        let outcome = if let Some(outcome) = cached {
            outcome
        } else {
            let outcome = self.poll_outcome(id, plugin_id)?;
            self.lifecycle
                .lock()
                .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?
                .observe(outcome.clone())
                .map_err(|_| error(plugin_id, "native_lifecycle_observation_conflict"))?;
            outcome
        };
        if let NativeOutcome::Error { code, message } = &outcome {
            self.record_failure(
                plugin_id,
                "native_author_lifecycle_failed",
                &format!("code {code}: {message}"),
            );
            if kind != StepKind::Contributions {
                self.lifecycle
                    .lock()
                    .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?
                    .retain_failed_lifecycle()
                    .map_err(|_| error(plugin_id, "native_lifecycle_retention_failed"))?;
                return Err(error(plugin_id, "native_author_lifecycle_failed"));
            }
        }
        if kind == StepKind::Deactivate && matches!(outcome, NativeOutcome::Unit) {
            // Both acknowledgement paths finish the activation, which requires its
            // retained product host sessions (child invocations owning resources)
            // to be released first. The observed outcome stays cached in the
            // ledger, so a failed retirement retries here without replaying the hook.
            self.retire_host_sessions(plugin_id)?;
            self.retire_activation_resources(plugin_id)?;
        }
        let repaired = kind == StepKind::Deactivate
            && (!self
                .lifecycle
                .lock()
                .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?
                .failed_lifecycle
                .is_empty()
                || self
                    .execution
                    .scope()
                    .state()
                    .map_err(|_| error(plugin_id, "native_scope_state"))?
                    == bmux_plugin_runtime::LifecycleState::CleanupRequired);
        if repaired {
            self.host
                .drive(self.execution.acknowledge_repaired_cleanup(id))
                .map_err(|_| error(plugin_id, "native_repaired_cleanup_ack_pending"))?;
        } else {
            self.host
                .drive(self.execution.acknowledge(id))
                .map_err(|_| error(plugin_id, "native_lifecycle_ack_pending"))?;
        }
        self.lifecycle
            .lock()
            .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?
            .acknowledged()
            .map_err(|_| error(plugin_id, "native_lifecycle_ack_conflict"))?;
        if matches!(outcome, NativeOutcome::Error { .. }) {
            return Err(error(plugin_id, "native_contributions_failed"));
        }
        Ok(outcome)
    }

    /// Retire retained product host sessions once the author's deactivation hook
    /// has completed, so no author observer can admit further retained work.
    /// Release stays strict (explicit join, every job acknowledged); unresolved
    /// work fails closed and remains in the recovery ledger for an explicit retry.
    fn retire_host_sessions(&self, plugin_id: &str) -> Result<(), PluginLoadError> {
        let sessions = self
            .host_sessions
            .lock()
            .map_err(|_| error(plugin_id, "native_host_recovery_poisoned"))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            if session.is_released() {
                continue;
            }
            self.host.drive(bounded_join(&session)).map_err(|failure| {
                self.record_failure(plugin_id, "native_retained_host_join", &failure.to_string())
            })?;
            session.release().map_err(|failure| {
                self.record_failure(
                    plugin_id,
                    "native_retained_host_release",
                    &failure.to_string(),
                )
            })?;
        }
        Ok(())
    }

    /// Retire host resources still charged to the activation once the author's
    /// deactivation hook has settled, before acknowledgement finishes the
    /// activation. Unreleasable entries stay retained and fail closed.
    fn retire_activation_resources(&self, plugin_id: &str) -> Result<(), PluginLoadError> {
        let resources = self
            .activation
            .lock()
            .map_err(|_| error(plugin_id, "native_activation_poisoned"))?
            .clone();
        if let Some(resources) = resources {
            resources.retire_all().map_err(|failure| {
                self.record_failure(
                    plugin_id,
                    "native_activation_resource_release",
                    &failure.to_string(),
                )
            })?;
        }
        Ok(())
    }

    fn active_resources(
        &self,
    ) -> bmux_plugin_sdk::definition::PluginResult<bcode_plugin_sdk::definition::ActivationResources>
    {
        bcode_plugin_sdk::definition::ProductExtensionPort::ensure_active(self)?;
        self.activation
            .lock()
            .map_err(|_| {
                bmux_plugin_sdk::PluginCommandError::unavailable(
                    "selected native activation poisoned",
                )
            })?
            .clone()
            .ok_or_else(|| {
                bmux_plugin_sdk::PluginCommandError::unavailable(
                    "selected native activation unavailable",
                )
            })
    }

    pub(super) fn activate(&self, plugin_id: &str) -> Result<(), PluginLoadError> {
        let _reservation = self.reserve_lifecycle(plugin_id)?;
        {
            let mut ledger = self
                .lifecycle
                .lock()
                .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?;
            if ledger.phase != Phase::Created {
                return Err(error(plugin_id, "native_activation_not_created"));
            }
            ledger.phase = Phase::Activating;
        }
        match catch_unwind(AssertUnwindSafe(|| self.activate_inner(plugin_id))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(failure)) => {
                self.record_failure(plugin_id, "native_activation_failed", &failure.to_string());
                let _ = self.execution.cancel_background_tasks();
                Err(failure)
            }
            Err(_) => {
                let _ = self.execution.cancel_background_tasks();
                Err(self.record_failure(
                    plugin_id,
                    "native_activation_panicked",
                    "retained execution requires explicit cleanup",
                ))
            }
        }
    }
    fn activate_inner(&self, plugin_id: &str) -> Result<(), PluginLoadError> {
        let id = self
            .execution
            .begin_activation(self.host.native.clone(), self.host.concurrency)
            .map_err(|_| error(plugin_id, "activate"))?;
        self.start_step(plugin_id, id, StepKind::Activate)?;
        if !matches!(self.finish_step(plugin_id)?, NativeOutcome::Unit) {
            return Err(error(plugin_id, "activate_outcome"));
        }
        let id = self
            .execution
            .begin(NativeOperation::Extension {
                invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                contract: bcode_plugin_sdk::definition::ACTIVATION_CONTRIBUTIONS_EXTENSION
                    .to_string(),
                payload: Vec::new(),
            })
            .map_err(|_| error(plugin_id, "contributions"))?;
        self.start_step(plugin_id, id, StepKind::Contributions)?;
        let NativeOutcome::Bytes(bytes) = self.finish_step(plugin_id)? else {
            return Err(error(plugin_id, "contributions_outcome"));
        };
        let snapshot: Snapshot =
            serde_json::from_slice(&bytes).map_err(PluginLoadError::ServiceDecode)?;
        if snapshot.version != bcode_plugin_models::activation::ACTIVATION_CONTRIBUTIONS_VERSION {
            return Err(error(plugin_id, "contributions_version"));
        }
        let mut ledger = self
            .lifecycle
            .lock()
            .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?;
        *self
            .snapshot
            .lock()
            .map_err(|_| error(plugin_id, "contributions"))? = Some(snapshot);
        ledger.phase = Phase::Active;
        drop(ledger);
        Ok(())
    }

    fn admit_active<T>(
        &self,
        plugin_id: &str,
        stage: &'static str,
        begin: impl FnOnce() -> bmux_plugin_sdk::Result<T>,
    ) -> Result<T, PluginLoadError> {
        let ledger = self
            .lifecycle
            .lock()
            .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?;
        if ledger.phase != Phase::Active {
            return Err(error(plugin_id, "native_component_not_active"));
        }
        let result = begin().map_err(|_| error(plugin_id, stage));
        drop(ledger);
        result
    }

    pub(super) fn invoke(
        &self,
        context: &bcode_plugin_sdk::NativeServiceContext,
    ) -> Result<bcode_plugin_sdk::ServiceResponse, PluginLoadError> {
        if context.cancellation.is_cancelled() {
            return Err(error(&context.plugin_id, "cancelled_before_admission"));
        }
        let native = &self.host.native;
        let cancellation = bmux_plugin_sdk::ServiceCancellation::new();
        let service = bmux_plugin_sdk::NativeServiceContext {
            plugin_id: context.plugin_id.clone(),
            request: bmux_plugin_sdk::ServiceRequest {
                caller_plugin_id: "bcode".to_string(),
                service: bmux_plugin_sdk::RegisteredService {
                    capability: bmux_plugin_sdk::HostScope::new("bcode.product")
                        .expect("static capability"),
                    kind: bmux_plugin_sdk::ServiceKind::Command,
                    interface_id: context.request.interface_id.clone(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin(context.plugin_id.clone()),
                },
                operation: context.request.operation.clone(),
                payload: context.request.payload.clone(),
            },
            required_capabilities: native.required_capabilities.clone(),
            provided_capabilities: native.provided_capabilities.clone(),
            services: native.services.clone(),
            available_capabilities: native.available_capabilities.clone(),
            enabled_plugins: native.enabled_plugins.clone(),
            plugin_search_roots: native.plugin_search_roots.clone(),
            host: native.host.clone(),
            connection: native.connection.clone(),
            settings: native.settings.clone(),
            plugin_settings_map: native.plugin_settings_map.clone(),
            caller_client_id: None,
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: native.host_kernel_bridge.clone(),
            host_service_port: None,
        };
        let events =
            bmux_plugin_sdk::ServiceEventSinkHandle::new(ProductEvents(context.events.clone()));
        let invocation = self.admit_active(&context.plugin_id, "native_scope_admission", || {
            self.execution.begin_invocation_scope()
        })?;
        let Ok(activation) = invocation.activation_id() else {
            self.retire_unused_scope(&context.plugin_id, invocation)?;
            return Err(error(&context.plugin_id, "native_activation_identity"));
        };
        let binding = super::shared_invocation_host::Binding {
            runtime: invocation.runtime_id(),
            instance: invocation.instance_id(),
            activation,
            invocation: invocation.id(),
            context: context.clone(),
        };
        let dispatch = Arc::new(super::shared_invocation_host::InvocationDispatch::new(
            binding,
        ));
        let admitted = self.admit_active(&context.plugin_id, "native_stream_admission", || {
            self.execution.begin_streaming_with_dispatch(
                bmux_plugin_sdk::NativeStreamingServiceContext { service, events },
                cancellation,
                invocation.clone(),
                Some(dispatch),
                None,
            )
        });
        let id = match admitted {
            Ok(id) => id,
            Err(failure) => {
                self.retire_unused_scope(&context.plugin_id, invocation)?;
                return Err(failure);
            }
        };
        match self.settle_cancelled(id, &context.plugin_id, Some(&context.cancellation))? {
            NativeOutcome::Service(response) => Ok(bcode_plugin_sdk::ServiceResponse {
                payload: response.payload,
                error: response.error.map(|error| bcode_plugin_sdk::ServiceError {
                    code: error.code,
                    message: error.message,
                }),
            }),
            _ => Err(error(&context.plugin_id, "native_service_outcome")),
        }
    }

    fn retire_unused_scope(
        &self,
        plugin_id: &str,
        invocation: bmux_plugin_runtime::InvocationScope,
    ) -> Result<(), PluginLoadError> {
        let retired = invocation
            .close()
            .and_then(|()| {
                self.host
                    .drive(invocation.drain_until(bmux_plugin_runtime::teardown_deadline()))
            })
            .and_then(|()| invocation.finish());
        if let Err(failure) = retired {
            self.unused_invocations
                .lock()
                .map_err(|_| error(plugin_id, "unused_scope_registry_poisoned"))?
                .push(invocation);
            return Err(self.record_failure(
                plugin_id,
                "unused_scope_cleanup_required",
                &failure.to_string(),
            ));
        }
        Ok(())
    }

    pub(super) fn snapshot(&self, plugin_id: &str) -> Result<Snapshot, PluginLoadError> {
        let ledger = self
            .lifecycle
            .lock()
            .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?;
        if ledger.phase != Phase::Active {
            return Err(error(plugin_id, "native_component_not_active"));
        }
        let snapshot = self
            .snapshot
            .lock()
            .map_err(|_| error(plugin_id, "contributions"))?
            .clone()
            .ok_or_else(|| error(plugin_id, "contributions"));
        drop(ledger);
        snapshot
    }
    pub(super) fn event(
        &self,
        context: bcode_plugin_sdk::NativeEventContext,
    ) -> Result<(), PluginLoadError> {
        let id = context.plugin_id;
        let event = bmux_plugin_sdk::PluginEvent::typed(
            bmux_plugin_sdk::PluginEventKind::from_owned(
                bcode_plugin_sdk::definition::PRODUCT_EVENT_KIND.to_string(),
            ),
            &context.event,
        )
        .map_err(|_| error(&id, "event_encode"))?;
        let operation = self.admit_active(&id, "event_admission", || {
            self.execution.begin(NativeOperation::Event(event))
        })?;
        if !matches!(self.settle(operation, &id)?, NativeOutcome::Unit) {
            return Err(error(&id, "event_outcome"));
        }
        Ok(())
    }

    pub(super) fn deactivate(&self, plugin_id: &str) -> Result<(), PluginLoadError> {
        let _reservation = self.reserve_lifecycle(plugin_id)?;
        match catch_unwind(AssertUnwindSafe(|| self.deactivate_inner(plugin_id))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(failure)) => {
                self.record_failure(plugin_id, "native_cleanup_failed", &failure.to_string());
                Err(failure)
            }
            Err(_) => Err(self.record_failure(
                plugin_id,
                "native_cleanup_panicked",
                "execution and operation outcomes retained",
            )),
        }
    }
    fn deactivate_inner(&self, plugin_id: &str) -> Result<(), PluginLoadError> {
        let (phase, pending, cleanup_completed, activation_started) = {
            let mut ledger = self
                .lifecycle
                .lock()
                .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?;
            let prior = ledger.phase;
            if prior == Phase::Destroyed {
                return Ok(());
            }
            ledger.phase = Phase::Closing;
            (
                prior,
                ledger.pending.as_ref().map(|step| step.kind),
                ledger.cleanup_completed,
                ledger.activation_started,
            )
        };
        let unused = std::mem::take(
            &mut *self
                .unused_invocations
                .lock()
                .map_err(|_| error(plugin_id, "unused_scope_registry_poisoned"))?,
        );
        let mut failed = Vec::new();
        for invocation in unused {
            if invocation
                .close()
                .and_then(|()| {
                    self.host
                        .drive(invocation.drain_until(bmux_plugin_runtime::teardown_deadline()))
                })
                .and_then(|()| invocation.finish())
                .is_err()
            {
                failed.push(invocation);
            }
        }
        if !failed.is_empty() {
            self.unused_invocations
                .lock()
                .map_err(|_| error(plugin_id, "unused_scope_registry_poisoned"))?
                .extend(failed);
            return Err(error(plugin_id, "unused_scope_cleanup_required"));
        }
        if let Some(kind) = pending {
            self.finish_step(plugin_id)?;
            if kind == StepKind::Deactivate {
                return self.destroy_after_cleanup(plugin_id);
            }
        }
        if activation_started
            && !cleanup_completed
            && phase != Phase::Created
            && phase != Phase::Inactive
        {
            // No observer timeout authorizes dropping pending code. Shared closure
            // cancels admitted work; this owner retains hook IDs until terminal ack.
            self.host
                .drive(self.host.registrations.revoke_and_drain())?;
            let id = self
                .host
                .drive(self.execution.begin_deactivation(self.host.native.clone()))
                .map_err(|_| error(plugin_id, "native_begin_cleanup_failed"))?;
            self.start_step(plugin_id, id, StepKind::Deactivate)?;
            if !matches!(self.finish_step(plugin_id)?, NativeOutcome::Unit) {
                return Err(error(plugin_id, "deactivate_outcome"));
            }
        }
        self.destroy_after_cleanup(plugin_id)
    }
    fn destroy_after_cleanup(&self, plugin_id: &str) -> Result<(), PluginLoadError> {
        // All queued/executing author and control jobs settle before native
        // destruction. Cancelled shutdown observers retain their join state.
        self.host
            .drive(self.workers.shutdown())
            .map_err(|details| {
                self.record_failure(plugin_id, "native_worker_shutdown", &details)
            })?;
        self.host
            .drive(self.control_workers.shutdown())
            .map_err(|details| {
                self.record_failure(plugin_id, "native_control_shutdown", &details)
            })?;
        // An author observer that dropped the retained host while its session
        // was still unresolved left the final transport release pending. The
        // sessions are retired now, so retry that release (never producer
        // work); a still-pending release keeps the execution image pinned.
        let released = self
            .execution
            .retry_release_retained_host()
            .map_err(|failure| {
                self.record_failure(
                    plugin_id,
                    "native_retained_host_release",
                    &failure.to_string(),
                )
            })?;
        if !released {
            return Err(self.record_failure(
                plugin_id,
                "native_retained_host_release_pending",
                "retained host transport release still pending",
            ));
        }
        self.host
            .drive(self.execution.destroy())
            .map_err(|_| error(plugin_id, "destroy"))?;
        let snapshot = self
            .snapshot
            .lock()
            .map_err(|_| error(plugin_id, "deactivate"))?
            .take();
        drop(snapshot);
        let activation = self
            .activation
            .lock()
            .map_err(|_| error(plugin_id, "native_activation_poisoned"))?
            .take();
        drop(activation);
        self.lifecycle
            .lock()
            .map_err(|_| error(plugin_id, "native_lifecycle_poisoned"))?
            .phase = Phase::Destroyed;
        Ok(())
    }
}
struct ProductEvents(bcode_plugin_sdk::ServiceEventEmitter);
impl bmux_plugin_sdk::ServiceEventSink for ProductEvents {
    fn emit_event(&self, event: bmux_plugin_sdk::ServiceEvent) -> bmux_plugin_sdk::Result<()> {
        self.0.emit(&event.payload);
        Ok(())
    }
}

impl bcode_plugin_sdk::definition::ProductExtensionPort for SharedNative {
    fn product_host_session(
        &self,
    ) -> Result<
        bcode_plugin_sdk::definition::host_session::ProductHostSessionClient,
        bcode_plugin_sdk::definition::host_session::HostSessionClientError,
    > {
        use bcode_plugin_sdk::definition::host_session::HostSessionClientError;
        let selected = self
            .selected_host
            .lock()
            .map_err(|_| {
                HostSessionClientError::Uncertain(
                    bcode_plugin_sdk::definition::host_session_models::SessionCode::Unavailable,
                )
            })?
            .as_ref()
            .cloned()
            .ok_or(HostSessionClientError::Unsupported)?;
        selected
            .session()
            .map_err(|_| {
                HostSessionClientError::Uncertain(
                    bcode_plugin_sdk::definition::host_session_models::SessionCode::Unavailable,
                )
            })?
            .typed_client()
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
            let plugin_id = &self.host.native.plugin_id;
            let invocation = self
                .admit_active(plugin_id, "extension_scope_admission", || {
                    self.execution.begin_invocation_scope()
                })
                .map_err(|_| {
                    bmux_plugin_sdk::PluginCommandError::unavailable(
                        "selected extension scope unavailable",
                    )
                })?;
            let dispatch = Arc::new(super::shared_invocation_host::ExtensionDispatch {
                invocation: invocation.clone(),
                services,
            });
            let operation = self.admit_active(plugin_id, "extension_host_admission", || {
                self.execution.begin_with_dispatch(
                    NativeOperation::Extension {
                        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                        contract,
                        payload,
                    },
                    bmux_plugin_sdk::ServiceCancellation::new(),
                    invocation.clone(),
                    Some(dispatch),
                    None,
                )
            });
            let Ok(id) = operation else {
                self.retire_unused_scope(plugin_id, invocation)
                    .map_err(|_| {
                        bmux_plugin_sdk::PluginCommandError::unavailable(
                            "unused extension scope cleanup required",
                        )
                    })?;
                return Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                    "selected extension admission failed",
                ));
            };
            match self.settle_async(id, plugin_id).await.map_err(|_| {
                bmux_plugin_sdk::PluginCommandError::unavailable(
                    "selected extension settlement failed; do not replay",
                )
            })? {
                NativeOutcome::Bytes(bytes) => Ok(bytes),
                _ => Err(bmux_plugin_sdk::PluginCommandError::failed(
                    "unexpected selected extension outcome",
                )),
            }
        })
    }
    fn activation_scope(
        &self,
    ) -> bmux_plugin_sdk::definition::PluginResult<bmux_plugin_runtime::ActivationScope> {
        Ok(self.active_resources()?.scope().clone())
    }
    fn activation_resources(
        &self,
    ) -> bmux_plugin_sdk::definition::PluginResult<bcode_plugin_sdk::definition::ActivationResources>
    {
        self.active_resources()
    }
    fn ensure_active(&self) -> bmux_plugin_sdk::definition::PluginResult<()> {
        let ledger = self.lifecycle.lock().map_err(|_| {
            bmux_plugin_sdk::PluginCommandError::unavailable("selected native lifecycle poisoned")
        })?;
        if ledger.phase != Phase::Active {
            return Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                "selected native component is closing or inactive",
            ));
        }
        drop(ledger);
        if self
            .snapshot
            .lock()
            .map_err(|_| {
                bmux_plugin_sdk::PluginCommandError::unavailable("selected native state poisoned")
            })?
            .is_none()
        {
            return Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                "selected native activation is not active",
            ));
        }
        Ok(())
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
            self.ensure_active()?;
            let id = self
                .admit_active(
                    &self.host.native.plugin_id,
                    "native_extension_admission",
                    || {
                        self.execution.begin(NativeOperation::Extension {
                            invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                            contract,
                            payload,
                        })
                    },
                )
                .map_err(|_| {
                    bmux_plugin_sdk::PluginCommandError::unavailable(
                        "native extension admission failed",
                    )
                })?;
            // The retained execution owns library, admission and outcome until ack.
            match self
                .settle_async(id, &self.host.native.plugin_id)
                .await
                .map_err(|_| {
                    bmux_plugin_sdk::PluginCommandError::unavailable(
                        "native extension settlement failed; no replay",
                    )
                })? {
                NativeOutcome::Bytes(bytes) => Ok(bytes),
                _ => Err(bmux_plugin_sdk::PluginCommandError::failed(
                    "unexpected native extension outcome",
                )),
            }
        })
    }
}

/// A settlement failure, recorded in the lifecycle ledger by its owner.
struct SettleFailure {
    stage: &'static str,
    details: String,
    record: bool,
}
impl SettleFailure {
    const fn recorded(stage: &'static str, details: String) -> Self {
        Self {
            stage,
            details,
            record: true,
        }
    }
}

/// Poll an admitted operation to its final outcome.
///
/// The loader arms a wake of the polling waker for every `Pending` (the
/// operation itself, a host-wake guest, cancellation, or the ABI's legacy
/// repoll), so the host suspends on that wake, never a timer. The permit
/// survives a wake that lands before suspension.
async fn observe_outcome(
    execution: &Arc<bmux_plugin::LoadedExecution>,
    workers: &blocking::NativeWorkers,
    control: &blocking::NativeWorkers,
    id: u64,
    cancellation: Option<&bcode_plugin_sdk::ServiceCancellation>,
) -> Result<NativeOutcome, SettleFailure> {
    let wake = Arc::new(OperationWake::default());
    let waker = std::task::Waker::from(Arc::clone(&wake));
    let mut cancellation = cancellation;
    loop {
        let polled_execution = Arc::clone(execution);
        let job_waker = waker.clone();
        let job = workers.submit(move || {
            let mut context = std::task::Context::from_waker(&job_waker);
            polled_execution.poll(id, &mut context).map_err(Box::new)
        });
        tokio::pin!(job);
        let polled = loop {
            let Some(pending) = cancellation else {
                break job.as_mut().await;
            };
            tokio::select! {
                outcome = &mut job => break outcome,
                () = pending.cancelled() => {
                    forward_cancellation(execution, control, id).await?;
                    cancellation = None;
                }
            }
        }
        .map_err(|details| SettleFailure::recorded("native_poll_worker", details))?;
        match polled {
            Ok(std::task::Poll::Ready(outcome)) => return Ok(outcome),
            Ok(std::task::Poll::Pending) => loop {
                let Some(pending) = cancellation else {
                    wake.0.notified().await;
                    break;
                };
                tokio::select! {
                    () = wake.0.notified() => break,
                    () = pending.cancelled() => {
                        forward_cancellation(execution, control, id).await?;
                        cancellation = None;
                    }
                }
            },
            Err(failure) => {
                return Err(SettleFailure::recorded(
                    "shared_native_poll",
                    failure.to_string(),
                ));
            }
        }
    }
}

/// Forward caller cancellation once. Cancellation has independent bounded
/// control capacity, even when every author worker awaits a host reply.
async fn forward_cancellation(
    execution: &Arc<bmux_plugin::LoadedExecution>,
    control: &blocking::NativeWorkers,
    id: u64,
) -> Result<(), SettleFailure> {
    let execution = Arc::clone(execution);
    control
        .submit(move || execution.cancel(id).map_err(Box::new))
        .await
        .map_err(|details| SettleFailure::recorded("native_cancel_worker", details))?
        .map_err(|failure| SettleFailure::recorded("native_cancel_failed", failure.to_string()))
}

fn error(plugin_id: &str, hook: &'static str) -> PluginLoadError {
    PluginLoadError::LifecycleFailed {
        plugin_id: plugin_id.to_string(),
        hook,
        code: -1,
    }
}

#[cfg(test)]
mod retained_lookup_tests {
    use super::*;
    use bmux_plugin_sdk::abi::retained::{
        NativeRetainedReservationV1, RETAINED_HOST_VERSION, RetainedHostDispatch,
    };

    #[test]
    fn unbound_lookup_never_creates_a_product_session() {
        let context = crate::shared_instance_tests::fixture_config("example.hello")
            .shared_activation
            .expect("explicit fixture context");
        let instance = bmux_plugin_runtime::InstanceScope::new(context.runtime);
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let binding = Arc::new(Mutex::new(None));
        let recovery = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
        let host = NativeSelectedHost {
            activation,
            context,
            execution: std::sync::Weak::new(),
            binding: Arc::clone(&binding),
            recovery: Arc::clone(&recovery),
        };
        let request = bmux_plugin_sdk::HostServiceRequest {
            capability: "bcode.product-host-session/v1",
            kind: bmux_plugin_sdk::ServiceKind::Query,
            interface: "bcode.product-host-session/v1",
            operation: "negotiate",
            payload: &[],
        };
        let reservation = NativeRetainedReservationV1 {
            size: u32::try_from(std::mem::size_of::<NativeRetainedReservationV1>()).unwrap(),
            version: RETAINED_HOST_VERSION,
            max_outcome_bytes: 4096,
            operation_slots: 1,
            reserved: 0,
        };
        assert_eq!(host.lookup(1, &request, reservation).unwrap(), None);
        host.activation.close().unwrap();
        assert_eq!(host.lookup(1, &request, reservation).unwrap(), None);
        assert!(binding.lock().unwrap().is_none());
        assert!(recovery.lock().unwrap().is_empty());
        host.activation.finish().unwrap();
        instance.close().unwrap();
        instance.finish().unwrap();
    }
}
