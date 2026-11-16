//! Caller-owned execution and activation authority for shared plugin adapters.
mod product_host_port;
mod retained_transport;

use crate::PluginLoadError;
use bcode_plugin_sdk::definition::host_session_models::{
    JobReservation, SessionBudget, SessionCode, SessionOutcome,
};
use std::{future::Future, pin::Pin, sync::Arc};

/// Drives lifecycle work using the selected host runtime/configuration.
///
/// Implementations must settle the supplied future before returning, including
/// on failure. They must not nest an executor on an active runtime thread or
/// discard pending author work when an observing caller is cancelled.
pub trait SharedLifecycleDriver: Send + Sync {
    /// Return the explicitly selected reactor; this driver must retain its owner
    /// until all accepted callback work has settled.
    fn runtime_handle(&self) -> tokio::runtime::Handle;

    /// Drive one lifecycle/cleanup operation to settlement.
    fn run(&self, work: Pin<Box<dyn Future<Output = ()> + Send + '_>>);
}

/// Activation-local publication authority.
///
/// The implementation closes admissions,
/// withdraws all partial registrations and drains producers/callbacks for this
/// exact activation. Failure retains its resources for an explicit retry.
pub trait SharedRegistrationOwner: Send + Sync {
    /// Withdraw and drain activation registrations/producers. Keep lifecycle host
    /// services usable until author deactivation settles; they remain retained in
    /// `SharedActivationContext` and are not replaced with an unavailable port.
    fn revoke_and_drain(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginLoadError>> + Send + '_>>;
}

/// Explicit host selection; no paths, runtime identity or capability defaults are
/// inferred by plugin loading. Clone retains the same authority, not a new host.
#[derive(Clone)]
pub struct SharedActivationContext {
    /// Runtime generation shared by all instances admitted by this host.
    pub runtime: bmux_plugin_runtime::PluginRuntimeScopeId,
    /// Exact compatible lifecycle metadata, selected paths and configuration.
    pub native: bmux_plugin_sdk::NativeLifecycleContext,
    /// Live activation host/registration bridge, retained through cleanup.
    pub services: bmux_plugin_sdk::definition::PluginHostServices,
    /// Invocation-bound host dispatcher used by the shared native runtime.
    pub dispatch: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
    /// Explicit selected concurrency policy.
    pub concurrency: bmux_plugin_runtime::PluginConcurrencyConfig,
    /// Product progress limits retained by each typed invocation.
    pub progress_limits: bcode_plugin_sdk::TransientProgressLimits,
    /// Driver bound to the real serving runtime.
    pub driver: Arc<dyn SharedLifecycleDriver>,
    /// Activation-specific resource cleanup authority.
    pub registrations: Arc<dyn SharedRegistrationOwner>,
}
impl std::fmt::Debug for SharedActivationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedActivationContext")
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}
impl SharedActivationContext {
    /// Retain explicitly selected product callbacks under one activation-owned session.
    ///
    /// `services` must be the actual product host, not an invocation callback proxy.
    /// `owner` retains its dependencies until explicit joined release. This is a
    /// Rust-local host API; neither owners nor runtime scopes cross the plugin ABI.
    /// The session consumes one configured activation invocation slot.
    ///
    /// # Errors
    /// Rejects a foreign runtime, closed activation, or exhausted runtime capacity.
    pub fn retain_product_host<T: Send + Sync + 'static>(
        &self,
        activation: &bmux_plugin_runtime::ActivationScope,
        services: bmux_plugin_sdk::definition::PluginHostServices,
        owner: Arc<T>,
    ) -> bmux_plugin_sdk::Result<ProductHostSession> {
        if activation.runtime_id() != self.runtime {
            return Err(host_session_error("product host runtime mismatch"));
        }
        // Retained work must run on the selected reactor, which keeps running
        // between lifecycle calls; never the caller's ambient runtime, which may
        // be a frontend thread that blocks while awaiting this same work.
        let _reactor = self.driver.runtime_handle().enter();
        ProductHostSession::new(activation, services, owner)
    }

    /// Create a retained session with explicit admission-time memory limits.
    ///
    /// # Errors
    /// Rejects invalid budgets, foreign runtime identity, or unavailable capacity.
    pub fn retain_product_host_with_limits<T: Send + Sync + 'static>(
        &self,
        activation: &bmux_plugin_runtime::ActivationScope,
        services: bmux_plugin_sdk::definition::PluginHostServices,
        owner: Arc<T>,
        limits: ProductHostLimits,
    ) -> bmux_plugin_sdk::Result<ProductHostSession> {
        if activation.runtime_id() != self.runtime {
            return Err(host_session_error("product host runtime mismatch"));
        }
        // See `retain_product_host`: retained work runs on the selected reactor.
        let _reactor = self.driver.runtime_handle().enter();
        ProductHostSession::new_with_limits(activation, services, owner, limits)
    }

    /// Open a session retained by the actual selected plugin's recovery ledger.
    /// Static and native owners use the same lifecycle API; native sessions pin
    /// their actual loaded execution/image independently of author observers.
    ///
    /// # Errors
    /// Rejects unsupported backends, inactive owners, or invalid/exhausted budgets.
    pub fn retain_selected_product_host(
        &self,
        plugin: &crate::LoadedPlugin,
        services: bmux_plugin_sdk::definition::PluginHostServices,
        limits: ProductHostLimits,
    ) -> bmux_plugin_sdk::Result<ProductHostSession> {
        limits.validate()?;
        if plugin
            .config
            .shared_activation
            .as_ref()
            .is_none_or(|context| context.runtime != self.runtime)
        {
            return Err(host_session_error("selected host runtime mismatch"));
        }
        match &plugin.backend {
            crate::LoadedPluginBackend::Shared(instance) => {
                instance.retain_host_session(services, limits)
            }
            crate::LoadedPluginBackend::SharedNative(instance) => {
                instance.retain_host_session(services, limits)
            }
            #[cfg(test)]
            crate::LoadedPluginBackend::Test(_) => Err(host_session_error(
                "selected retained host backend unavailable",
            )),
        }
    }

    /// Enumerate unresolved selected-owner sessions after author observer loss.
    /// Recovery remains available after activation closure. Limit is capped at 100.
    ///
    /// # Errors
    /// Reports unsupported backends or poisoned recovery state.
    pub fn product_host_recovery_page(
        &self,
        plugin: &crate::LoadedPlugin,
        after: Option<bmux_plugin_runtime::RuntimeInvocationId>,
        limit: usize,
    ) -> bmux_plugin_sdk::Result<Vec<ProductHostSession>> {
        match &plugin.backend {
            crate::LoadedPluginBackend::Shared(instance) => {
                instance.host_session_page(after, limit)
            }
            crate::LoadedPluginBackend::SharedNative(instance) => {
                instance.host_session_page(after, limit)
            }
            #[cfg(test)]
            crate::LoadedPluginBackend::Test(_) => Err(host_session_error(
                "selected retained host backend unavailable",
            )),
        }
    }

    /// Remove a successfully released session from its selected recovery ledger.
    /// Busy/unjoined/unacknowledged sessions cannot be forgotten.
    ///
    /// # Errors
    /// Rejects unknown/unreleased sessions and unavailable recovery ledgers.
    pub fn forget_released_product_host(
        &self,
        plugin: &crate::LoadedPlugin,
        id: bmux_plugin_runtime::RuntimeInvocationId,
    ) -> bmux_plugin_sdk::Result<()> {
        match &plugin.backend {
            crate::LoadedPluginBackend::Shared(instance) => {
                instance.forget_released_host_session(id)
            }
            crate::LoadedPluginBackend::SharedNative(instance) => {
                instance.forget_released_host_session(id)
            }
            #[cfg(test)]
            crate::LoadedPluginBackend::Test(_) => Err(host_session_error(
                "selected retained host backend unavailable",
            )),
        }
    }

    /// Run a static selected extension under a separately admitted retained job.
    /// The caller supplies the exact negotiated contract version. The descriptor
    /// is rechecked against the actual selected implementation before execution.
    /// Inspect `job.callback_outcomes()` and explicitly join/acknowledge afterward;
    /// dropping the observing caller never cancels or replays the operation.
    ///
    /// # Errors
    /// Rejects foreign owners, unsupported native transport, invalid request bounds,
    /// closed owners, or task admission failure. Effectful failures remain in the job.
    pub fn start_product_host_extension(
        &self,
        plugin: &crate::LoadedPlugin,
        job: &ProductHostJob,
        contract: String,
        version: u32,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::Result<()> {
        if self.runtime != job.invocation().runtime_id() {
            return Err(host_session_error("product host runtime mismatch"));
        }
        match &plugin.backend {
            crate::LoadedPluginBackend::Shared(instance) => self.drive(async {
                instance.start_product_host_extension(job, contract, version, payload)
            }),
            crate::LoadedPluginBackend::SharedNative(_) => Err(host_session_error(
                "retained extension transport unavailable; no fallback",
            )),
            #[cfg(test)]
            crate::LoadedPluginBackend::Test(_) => Err(host_session_error(
                "retained extension transport unavailable; no fallback",
            )),
        }
    }

    pub(crate) fn validate(&self, plugin_id: &str) -> Result<(), PluginLoadError> {
        let paths = &self.native.connection;
        if self.native.plugin_id != plugin_id
            || self.native.host.product_name != "bcode"
            || self.native.host.plugin_api_version != bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION
            || self.native.host.plugin_abi_version != bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION
            || [
                &paths.config_dir,
                &paths.runtime_dir,
                &paths.data_dir,
                &paths.state_dir,
            ]
            .iter()
            .any(|path| !std::path::Path::new(path).is_absolute())
        {
            return Err(super::shared_instance::error(
                plugin_id,
                "invalid_shared_activation_context",
            ));
        }
        Ok(())
    }
    pub(crate) fn drive<T: Send>(&self, work: impl Future<Output = T> + Send) -> T {
        let mut result = None;
        self.driver.run(Box::pin(async {
            result = Some(work.await);
        }));
        result.expect("lifecycle driver must settle work before returning")
    }
}

/// Join a retained session during owner teardown, bounded by the host's
/// teardown budget. Expiry leaves the session retained for explicit recovery.
pub async fn bounded_join(session: &ProductHostSession) -> bmux_plugin_sdk::Result<()> {
    tokio::time::timeout(bmux_plugin_runtime::teardown_timeout(), session.join())
        .await
        .unwrap_or_else(|_| {
            Err(host_session_error(
                "product host session join exceeded the teardown budget",
            ))
        })
}

fn host_session_error(details: &str) -> bmux_plugin_sdk::PluginError {
    bmux_plugin_sdk::PluginError::ServiceProtocol {
        details: details.to_owned(),
    }
}

/// Admission-time limits for a retained host session. Reservations include a
/// fixed-size failure/identity record as well as request and response payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductHostLimits {
    /// Maximum child identities retained over this session's lifetime.
    pub max_jobs: u32,
    /// Maximum operation identities, including acknowledged tombstones.
    pub max_operations: u32,
    /// Maximum request payload and routing identifier bytes per operation.
    pub max_request_bytes: u32,
    /// Maximum retained response payload bytes per operation.
    pub max_response_bytes: u32,
    /// Total reservation capacity for this session, in bytes.
    pub retained_bytes: u64,
}
impl ProductHostLimits {
    fn validate(self) -> bmux_plugin_sdk::Result<Self> {
        if self.max_jobs == 0
            || self.max_operations == 0
            || self.max_request_bytes == 0
            || self.max_response_bytes == 0
            || self.retained_bytes < self.operation_bytes()?
        {
            return Err(host_session_error(
                "invalid product host reservation limits",
            ));
        }
        Ok(self)
    }
    fn operation_bytes(self) -> bmux_plugin_sdk::Result<u64> {
        u64::from(self.max_request_bytes)
            .checked_add(u64::from(self.max_response_bytes))
            .and_then(|bytes| bytes.checked_add(512))
            .ok_or_else(|| host_session_error("product host reservation overflow"))
    }
}
impl Default for ProductHostLimits {
    fn default() -> Self {
        Self {
            max_jobs: 32,
            max_operations: 32,
            max_request_bytes: 1_048_576,
            max_response_bytes: 1_048_576,
            retained_bytes: 67_125_248,
        }
    }
}
impl From<ProductHostLimits> for SessionBudget {
    fn from(limits: ProductHostLimits) -> Self {
        Self {
            max_jobs: limits.max_jobs,
            max_operations: limits.max_operations,
            max_request_bytes: limits.max_request_bytes,
            max_response_bytes: limits.max_response_bytes,
            retained_bytes: limits.retained_bytes,
        }
    }
}
impl From<SessionBudget> for ProductHostLimits {
    fn from(budget: SessionBudget) -> Self {
        Self {
            max_jobs: budget.max_jobs,
            max_operations: budget.max_operations,
            max_request_bytes: budget.max_request_bytes,
            max_response_bytes: budget.max_response_bytes,
            retained_bytes: budget.retained_bytes,
        }
    }
}
struct ProductHostJobReservation {
    budget: JobReservation,
    operations: u32,
    request_bytes: u32,
}
struct ProductHostReservations {
    limits: ProductHostLimits,
    jobs: u32,
    operations: u32,
    bytes: u64,
}

impl ProductHostReservations {
    fn admit_job(
        &self,
        reservation: Option<JobReservation>,
    ) -> bmux_plugin_sdk::Result<(u32, u64)> {
        let reserved_operations = reservation.map_or(0, |budget| budget.operations);
        let reserved_bytes = if let Some(budget) = reservation {
            budget
                .validate_for(self.limits.into())
                .map_err(|_| host_session_error("invalid product host job reservation"))?;
            // Each outcome must hold the terminal header and bounded diagnostic.
            if u64::from(budget.response_bytes) < u64::from(budget.operations) * 128 {
                return Err(host_session_error(
                    "job response reservation cannot retain failure records",
                ));
            }
            u64::from(budget.request_bytes)
                + u64::from(budget.response_bytes)
                + u64::from(budget.operations) * 512
        } else {
            0
        };
        let bytes = self
            .bytes
            .checked_add(512)
            .and_then(|bytes| bytes.checked_add(reserved_bytes))
            .ok_or_else(|| host_session_error("product host job reservation overflow"))?;
        let operations = self
            .operations
            .checked_add(reserved_operations)
            .ok_or_else(|| host_session_error("product host operation reservation overflow"))?;
        if self.jobs >= self.limits.max_jobs
            || bytes > self.limits.retained_bytes
            || operations > self.limits.max_operations
        {
            return Err(host_session_error("product host job capacity exhausted"));
        }
        Ok((operations, bytes))
    }
}

/// Host-local owner of retained product callbacks and explicitly acknowledged jobs.
/// Dropping a clone neither cancels work nor releases its runtime-owned resources.
#[derive(Clone)]
pub struct ProductHostSession {
    inner: Arc<ProductHostSessionInner>,
}
struct ProductHostSessionInner {
    executor: Option<tokio::runtime::Handle>,
    transport: std::sync::Mutex<retained_transport::TransportLedger>,
    scope: bmux_plugin_runtime::InvocationScope,
    services: bmux_plugin_sdk::definition::PluginHostServices,
    owner: std::sync::Mutex<Option<Arc<dyn std::any::Any + Send + Sync>>>,
    resource: std::sync::Mutex<Option<bmux_plugin_runtime::PluginResourceId>>,
    jobs: std::sync::Mutex<
        std::collections::BTreeMap<
            bmux_plugin_runtime::RuntimeInvocationId,
            Arc<ProductHostJobInner>,
        >,
    >,
    job_ids: std::sync::Mutex<std::collections::BTreeMap<uuid::Uuid, [u8; 16]>>,
    requests: std::sync::Mutex<std::collections::BTreeMap<[u8; 16], ProductHostJob>>,
    admissions_closed: Arc<std::sync::atomic::AtomicBool>,
    released: std::sync::atomic::AtomicBool,
    reservations: Arc<std::sync::Mutex<ProductHostReservations>>,
    joined: std::sync::atomic::AtomicBool,
}

/// A separately admitted child capability. Its caller-owned outcome stays retained
/// until join succeeds and the host explicitly acknowledges it through the session.
#[derive(Clone)]
pub struct ProductHostJob {
    inner: Arc<ProductHostJobInner>,
}
/// Bounded operation recovery identity within a job capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductHostOperationIdentity {
    /// Original caller request key; never authority.
    pub request_id: [u8; 16],
    /// Host-local observation index.
    pub index: usize,
}
#[derive(Clone, Copy)]
struct ProductHostOperationKey {
    fingerprint: [u8; 32],
    index: usize,
    operation: uuid::Uuid,
    receipt: uuid::Uuid,
}
struct ProductHostJobInner {
    session_closed: Arc<std::sync::atomic::AtomicBool>,
    producers: Arc<std::sync::atomic::AtomicUsize>,
    admissions_closed: std::sync::atomic::AtomicBool,
    reservation: std::sync::Mutex<Option<ProductHostJobReservation>>,
    session: bmux_plugin_runtime::RuntimeInvocationId,
    scope: bmux_plugin_runtime::InvocationScope,
    services: bmux_plugin_sdk::definition::PluginHostServices,
    outcome_owner: std::sync::Mutex<Option<Arc<dyn std::any::Any + Send + Sync>>>,
    outcome_identity: std::sync::Weak<dyn std::any::Any + Send + Sync>,
    resource: std::sync::Mutex<Option<bmux_plugin_runtime::PluginResourceId>>,
    joined: std::sync::atomic::AtomicBool,
    retired: std::sync::atomic::AtomicBool,
    operation_ids: std::sync::Mutex<std::collections::BTreeMap<uuid::Uuid, [u8; 16]>>,
    operation_requests:
        std::sync::Mutex<std::collections::BTreeMap<[u8; 16], ProductHostOperationKey>>,
    callbacks: std::sync::Mutex<Vec<Arc<ProductHostCallbackOutcome>>>,
    callback_limit: std::num::NonZeroUsize,
    reservations: Arc<std::sync::Mutex<ProductHostReservations>>,
}
/// One retained callback result; `None` means the producer remains unresolved.
pub type ProductHostObservation = Option<Result<Vec<u8>, String>>;
/// Retained callback observations in admission order; `None` remains unresolved.
pub type ProductHostOutcomes = Vec<ProductHostObservation>;

struct ProductHostCallbackOutcome {
    value: std::sync::Mutex<Option<Result<Vec<u8>, String>>>,
    terminal_kind: std::sync::atomic::AtomicU32,
    observed_bytes: std::sync::atomic::AtomicU64,
    acknowledged: std::sync::atomic::AtomicBool,
    ready: tokio::sync::Notify,
    response_limit: usize,
}
struct ProductHostProducer(Arc<std::sync::atomic::AtomicUsize>);
impl ProductHostProducer {
    fn new(count: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self(count)
    }
}
impl Drop for ProductHostProducer {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

struct ProductHostOutcomeWriter(Arc<ProductHostCallbackOutcome>);
impl ProductHostOutcomeWriter {
    fn complete(&self, result: Result<Vec<u8>, String>) -> Result<(), String> {
        let kind = match &result {
            Ok(bytes) if bytes.len() > self.0.response_limit => {
                self.0.observed_bytes.store(
                    u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    std::sync::atomic::Ordering::Release,
                );
                3
            }
            Ok(_) => 0,
            Err(_) => 2,
        };
        self.0
            .terminal_kind
            .store(kind, std::sync::atomic::Ordering::Release);
        let result = match result {
            Ok(bytes) if bytes.len() > self.0.response_limit => Err(
                "product host response limit exceeded; effects unresolved; do not replay"
                    .to_owned(),
            ),
            result => result,
        };
        *self
            .0
            .value
            .lock()
            .map_err(|_| "product host outcome poisoned".to_owned())? = Some(result);
        self.0.ready.notify_one();
        Ok(())
    }
}
impl Drop for ProductHostOutcomeWriter {
    fn drop(&mut self) {
        if let Ok(mut value) = self.0.value.lock()
            && value.is_none()
        {
            self.0
                .terminal_kind
                .store(4, std::sync::atomic::Ordering::Release);
            *value = Some(Err(
                "product host producer ended without settlement; effects unresolved".to_owned(),
            ));
        }
        self.0.ready.notify_one();
    }
}

impl ProductHostSession {
    fn new<T: Send + Sync + 'static>(
        activation: &bmux_plugin_runtime::ActivationScope,
        services: bmux_plugin_sdk::definition::PluginHostServices,
        owner: Arc<T>,
    ) -> bmux_plugin_sdk::Result<Self> {
        Self::new_with_limits(activation, services, owner, ProductHostLimits::default())
    }

    fn new_with_limits<T: Send + Sync + 'static>(
        activation: &bmux_plugin_runtime::ActivationScope,
        services: bmux_plugin_sdk::definition::PluginHostServices,
        owner: Arc<T>,
        limits: ProductHostLimits,
    ) -> bmux_plugin_sdk::Result<Self> {
        let limits = limits.validate()?;
        let scope = activation
            .begin_invocation()
            .map_err(|_| host_session_error("product host session admission failed"))?;
        Self::from_scope(scope, services, owner, limits)
    }

    pub(super) fn from_scope<T: Send + Sync + 'static>(
        scope: bmux_plugin_runtime::InvocationScope,
        services: bmux_plugin_sdk::definition::PluginHostServices,
        owner: Arc<T>,
        limits: ProductHostLimits,
    ) -> bmux_plugin_sdk::Result<Self> {
        let limits = limits.validate()?;
        let inner = Arc::new(ProductHostSessionInner {
            executor: tokio::runtime::Handle::try_current().ok(),
            transport: std::sync::Mutex::new(retained_transport::TransportLedger::default()),
            scope,
            services,
            owner: std::sync::Mutex::new(Some(owner)),
            resource: std::sync::Mutex::new(None),
            jobs: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            admissions_closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            released: std::sync::atomic::AtomicBool::new(false),
            job_ids: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            requests: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            reservations: Arc::new(std::sync::Mutex::new(ProductHostReservations {
                limits,
                jobs: 0,
                operations: 0,
                bytes: 0,
            })),
            joined: std::sync::atomic::AtomicBool::new(false),
        });
        let Ok(resource) = inner.scope.register_resource(Arc::clone(&inner)) else {
            // No work or resources were admitted. Do not leave an empty
            // session occupying a configured invocation slot on this race.
            inner
                .scope
                .close()
                .map_err(|_| host_session_error("product host registration cleanup failed"))?;
            inner
                .scope
                .finish()
                .map_err(|_| host_session_error("product host registration retirement failed"))?;
            return Err(host_session_error(
                "product host session registration failed",
            ));
        };
        *inner
            .resource
            .lock()
            .map_err(|_| host_session_error("product host session poisoned"))? = Some(resource);
        Ok(Self { inner })
    }

    /// Return the session-local recovery key. This identity is not authority.
    #[must_use]
    pub fn id(&self) -> bmux_plugin_runtime::RuntimeInvocationId {
        self.inner.scope.id()
    }

    /// Whether explicit joined release completed successfully.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.inner
            .released
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Admit a distinct deferred job and retain its preallocated outcome owner.
    /// The host must reserve bounded, lossless outcome capacity before calling this
    /// method. This API never consumes, truncates, or implicitly acknowledges outcomes.
    ///
    /// # Errors
    /// Rejects closed sessions, exhausted capacity, or poisoned bookkeeping.
    pub fn begin_job<T: Send + Sync + 'static>(
        &self,
        outcome: Arc<T>,
        callback_limit: std::num::NonZeroUsize,
    ) -> bmux_plugin_sdk::Result<ProductHostJob> {
        self.begin_job_internal(outcome, callback_limit, None)
    }

    fn begin_job_internal<T: Send + Sync + 'static>(
        &self,
        outcome: Arc<T>,
        callback_limit: std::num::NonZeroUsize,
        job_reservation: Option<JobReservation>,
    ) -> bmux_plugin_sdk::Result<ProductHostJob> {
        let mut jobs = self
            .inner
            .jobs
            .lock()
            .map_err(|_| host_session_error("product host jobs poisoned"))?;
        if self
            .inner
            .admissions_closed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            drop(jobs);
            return Err(host_session_error("product host session admissions closed"));
        }
        let mut reservations = self
            .inner
            .reservations
            .lock()
            .map_err(|_| host_session_error("product host reservation poisoned"))?;
        let (operations, job_bytes) = reservations.admit_job(job_reservation)?;
        let scope = self
            .inner
            .scope
            .begin_child()
            .map_err(|_| host_session_error("product host job admission failed"))?;
        let Ok(resource) = scope.register_resource(Arc::clone(&outcome)) else {
            drop(reservations);
            drop(jobs);
            scope
                .close()
                .map_err(|_| host_session_error("product host job registration cleanup failed"))?;
            scope.finish().map_err(|_| {
                host_session_error("product host job registration retirement failed")
            })?;
            return Err(host_session_error(
                "product host outcome registration failed",
            ));
        };
        reservations.jobs += 1;
        reservations.operations = operations;
        reservations.bytes = job_bytes;
        drop(reservations);
        let outcome: Arc<dyn std::any::Any + Send + Sync> = outcome;
        let job = Arc::new(ProductHostJobInner {
            session_closed: Arc::clone(&self.inner.admissions_closed),
            producers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            admissions_closed: std::sync::atomic::AtomicBool::new(false),
            reservation: std::sync::Mutex::new(job_reservation.map(|budget| {
                ProductHostJobReservation {
                    budget,
                    operations: 0,
                    request_bytes: 0,
                }
            })),
            session: self.inner.scope.id(),
            scope,
            services: self.inner.services.clone(),
            outcome_identity: Arc::downgrade(&outcome),
            outcome_owner: std::sync::Mutex::new(Some(outcome)),
            resource: std::sync::Mutex::new(Some(resource)),
            joined: std::sync::atomic::AtomicBool::new(false),
            retired: std::sync::atomic::AtomicBool::new(false),
            operation_ids: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            operation_requests: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            callbacks: std::sync::Mutex::new(Vec::new()),
            callback_limit,
            reservations: Arc::clone(&self.inner.reservations),
        });
        // Publish only fully initialized ownership. Holding the ledger through
        // registration prevents join from observing a partially initialized child.
        jobs.insert(job.scope.id(), Arc::clone(&job));
        drop(jobs);
        Ok(ProductHostJob { inner: job })
    }

    /// Begin a recoverable job using a caller key within this session capability.
    /// Identical retries return the same child, including after closure. A key
    /// cannot be rebound to a different owner or callback limit. Keys are retained
    /// until session release and never confer authority outside this session.
    ///
    /// # Errors
    /// Rejects conflicting duplicates, poisoned bookkeeping, or admission limits.
    pub fn begin_job_once<T: Send + Sync + 'static>(
        &self,
        request_id: [u8; 16],
        outcome: Arc<T>,
        callback_limit: std::num::NonZeroUsize,
    ) -> bmux_plugin_sdk::Result<ProductHostJob> {
        let mut requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| host_session_error("product host request ledger poisoned"))?;
        if let Some(job) = requests.get(&request_id) {
            let owner: Arc<dyn std::any::Any + Send + Sync> = outcome;
            let same =
                std::sync::Weak::ptr_eq(&job.inner.outcome_identity, &Arc::downgrade(&owner))
                    && job.inner.callback_limit == callback_limit;
            let job = job.clone();
            drop(requests);
            return if same {
                Ok(job)
            } else {
                Err(host_session_error("conflicting product host job request"))
            };
        }
        let job = self.begin_job(outcome, callback_limit)?;
        self.inner
            .job_ids
            .lock()
            .map_err(|_| host_session_error("job identity index poisoned"))?
            .insert(job.inner.scope.id().as_uuid(), request_id);
        requests.insert(request_id, job.clone());
        drop(requests);
        Ok(job)
    }

    /// Begin/recover a job using B1's exact job-wide reservation semantics.
    /// Request capacity is cumulative across operations. Response capacity is
    /// divided into fixed shares before dispatch; each share reserves 128 bytes
    /// for header/failure metadata. Identity/tombstone metadata is charged separately.
    ///
    /// # Errors
    /// Rejects conflicting retries, insufficient failure capacity, or session limits.
    pub fn begin_reserved_job_once<T: Send + Sync + 'static>(
        &self,
        request_id: [u8; 16],
        outcome: Arc<T>,
        reservation: JobReservation,
    ) -> bmux_plugin_sdk::Result<ProductHostJob> {
        if request_id == [0; 16] {
            return Err(host_session_error(
                "invalid product host job request identity",
            ));
        }
        let mut requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| host_session_error("product host request ledger poisoned"))?;
        if let Some(job) = requests.get(&request_id) {
            let budget = job
                .inner
                .reservation
                .lock()
                .map_err(|_| host_session_error("product host job reservation poisoned"))?
                .as_ref()
                .map(|state| state.budget);
            let job = job.clone();
            drop(requests);
            return if budget == Some(reservation) {
                Ok(job)
            } else {
                Err(host_session_error(
                    "conflicting product host job reservation",
                ))
            };
        }
        let limit = usize::try_from(reservation.operations)
            .ok()
            .and_then(std::num::NonZeroUsize::new)
            .ok_or_else(|| host_session_error("invalid product host operation reservation"))?;
        let job = self.begin_job_internal(outcome, limit, Some(reservation))?;
        self.inner
            .job_ids
            .lock()
            .map_err(|_| host_session_error("job identity index poisoned"))?
            .insert(job.inner.scope.id().as_uuid(), request_id);
        requests.insert(request_id, job.clone());
        drop(requests);
        Ok(job)
    }

    /// Recover retained jobs after an interrupted observer or failed admission.
    /// No outcome is consumed by inspection.
    ///
    /// # Errors
    /// Reports poisoned lifecycle bookkeeping.
    pub fn jobs(&self) -> bmux_plugin_sdk::Result<Vec<ProductHostJob>> {
        let jobs = self
            .inner
            .jobs
            .lock()
            .map_err(|_| host_session_error("product host jobs poisoned"))?;
        let page = jobs
            .values()
            .take(100)
            .cloned()
            .map(|inner| ProductHostJob { inner })
            .collect();
        drop(jobs);
        Ok(page)
    }

    /// Read a bounded recovery page, including released keyed tombstones.
    /// Cursor IDs are observations within this session, never authority.
    ///
    /// # Errors
    /// Reports poisoned request bookkeeping.
    pub fn job_page(
        &self,
        after: Option<[u8; 16]>,
        limit: usize,
    ) -> bmux_plugin_sdk::Result<Vec<ProductHostJob>> {
        let requests = self
            .inner
            .requests
            .lock()
            .map_err(|_| host_session_error("product host request ledger poisoned"))?;
        let start = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let page = requests
            .range((start, std::ops::Bound::Unbounded))
            .take(limit.min(100))
            .map(|(_, job)| job.clone())
            .collect();
        drop(requests);
        Ok(page)
    }

    /// Fence new callbacks without requesting cancellation or claiming settlement.
    ///
    /// # Errors
    /// Reports poisoned admission bookkeeping.
    pub fn close(&self) -> bmux_plugin_sdk::Result<()> {
        let jobs = self
            .inner
            .jobs
            .lock()
            .map_err(|_| host_session_error("product host jobs poisoned"))?;
        self.inner
            .admissions_closed
            .store(true, std::sync::atomic::Ordering::Release);
        drop(jobs);
        Ok(())
    }

    /// Join every retained child and session admission without releasing outcomes.
    /// Dropping this observer retains all ownership for a later retry.
    ///
    /// # Errors
    /// Reports task failure or unavailable ownership; never treats cancellation as join.
    pub async fn join(&self) -> bmux_plugin_sdk::Result<()> {
        self.close()?;
        let jobs = self
            .inner
            .jobs
            .lock()
            .map_err(|_| host_session_error("product host jobs poisoned"))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for inner in jobs {
            ProductHostJob { inner }.join().await?;
        }
        self.inner
            .scope
            .drain()
            .await
            .map_err(|_| host_session_error("product host session not settled"))?;
        self.inner
            .joined
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Release a joined job only when every terminal operation was explicitly
    /// acknowledged. This is the strict lifecycle operation used by retained ABI ports.
    ///
    /// # Errors
    /// Rejects unacknowledged operations, foreign jobs, or incomplete settlement.
    pub fn release_job(&self, job: &ProductHostJob) -> bmux_plugin_sdk::Result<()> {
        let callbacks = job
            .inner
            .callbacks
            .lock()
            .map_err(|_| host_session_error("product host callbacks poisoned"))?;
        let acknowledged = callbacks.iter().all(|outcome| {
            outcome
                .acknowledged
                .load(std::sync::atomic::Ordering::Acquire)
        });
        drop(callbacks);
        if !acknowledged {
            return Err(host_session_error(
                "product host operations require acknowledgement",
            ));
        }
        self.retire_acknowledged_job(job)
    }

    /// Retire a joined job only after every operation's exact receipt is accepted.
    /// This compatibility name does not batch-acknowledge outcomes.
    ///
    /// # Errors
    /// Rejects foreign jobs, unacknowledged outcomes, or incomplete settlement.
    pub fn acknowledge_job(&self, job: &ProductHostJob) -> bmux_plugin_sdk::Result<()> {
        self.release_job(job)
    }

    fn retire_acknowledged_job(&self, job: &ProductHostJob) -> bmux_plugin_sdk::Result<()> {
        let jobs = self
            .inner
            .jobs
            .lock()
            .map_err(|_| host_session_error("product host jobs poisoned"))?;
        if job.inner.session != self.inner.scope.id() {
            drop(jobs);
            return Err(host_session_error("foreign product host session"));
        }
        if job.inner.retired.load(std::sync::atomic::Ordering::Acquire) {
            drop(jobs);
            return Ok(());
        }
        if !jobs
            .get(&job.inner.scope.id())
            .is_some_and(|inner| Arc::ptr_eq(inner, &job.inner))
            || !job.inner.joined.load(std::sync::atomic::Ordering::Acquire)
        {
            drop(jobs);
            return Err(host_session_error(
                "product host outcome is foreign or unsettled",
            ));
        }
        // No user destructors run under the session ledger lock.
        drop(jobs);
        release_host_resource(&job.inner.scope, &job.inner.resource)?;
        job.inner
            .scope
            .finish()
            .map_err(|_| host_session_error("product host job retirement failed"))?;
        job.inner
            .retired
            .store(true, std::sync::atomic::Ordering::Release);
        let outcome_owner = job
            .inner
            .outcome_owner
            .lock()
            .map_err(|_| host_session_error("product host outcome owner poisoned"))?
            .take();
        drop(outcome_owner);
        let removed = self
            .inner
            .jobs
            .lock()
            .map_err(|_| host_session_error("product host jobs poisoned"))?
            .remove(&job.inner.scope.id());
        drop(removed);
        Ok(())
    }

    /// Release the session only after explicit join and acknowledgement of all jobs.
    ///
    /// # Errors
    /// Rejects unjoined sessions, unacknowledged outcomes, or cleanup failures.
    pub fn release(&self) -> bmux_plugin_sdk::Result<()> {
        if self
            .inner
            .released
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let jobs = self
            .inner
            .jobs
            .lock()
            .map_err(|_| host_session_error("product host jobs poisoned"))?;
        if !jobs.is_empty() || !self.inner.joined.load(std::sync::atomic::Ordering::Acquire) {
            drop(jobs);
            return Err(host_session_error(
                "product host session has unacknowledged work",
            ));
        }
        // A joined session is closed, so no new jobs can be admitted after
        // releasing this guard. Resource destructors must run outside the ledger.
        drop(jobs);
        release_host_resource(&self.inner.scope, &self.inner.resource)?;
        self.inner
            .scope
            .finish()
            .map_err(|_| host_session_error("product host session retirement failed"))?;
        self.inner
            .released
            .store(true, std::sync::atomic::Ordering::Release);
        let owner = self
            .inner
            .owner
            .lock()
            .map_err(|_| host_session_error("product host owner poisoned"))?
            .take();
        drop(owner);
        let requests = std::mem::take(
            &mut *self
                .inner
                .requests
                .lock()
                .map_err(|_| host_session_error("product host request ledger poisoned"))?,
        );
        drop(requests);
        Ok(())
    }
}

fn release_host_resource(
    scope: &bmux_plugin_runtime::InvocationScope,
    resource: &std::sync::Mutex<Option<bmux_plugin_runtime::PluginResourceId>>,
) -> bmux_plugin_sdk::Result<()> {
    let mut resource = resource
        .lock()
        .map_err(|_| host_session_error("product host resource poisoned"))?;
    if let Some(id) = *resource {
        let owner = scope
            .release_resource(id)
            .map_err(|_| host_session_error("product host resource still admitted"))?;
        *resource = None;
        drop(resource);
        drop(owner);
    }
    Ok(())
}

impl ProductHostJobInner {
    fn reserve_response(&self, request_bytes: usize) -> bmux_plugin_sdk::Result<usize> {
        if self
            .admissions_closed
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .session_closed
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(host_session_error("product host admissions closed"));
        }
        let mut state = self
            .reservation
            .lock()
            .map_err(|_| host_session_error("product host job reservation poisoned"))?;
        if self
            .admissions_closed
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .session_closed
                .load(std::sync::atomic::Ordering::Acquire)
        {
            drop(state);
            return Err(host_session_error("product host admissions closed"));
        }
        if let Some(job) = state.as_mut() {
            let request_bytes = u32::try_from(request_bytes)
                .ok()
                .and_then(|bytes| job.request_bytes.checked_add(bytes))
                .ok_or_else(|| host_session_error("product host request reservation overflow"))?;
            if job.operations >= job.budget.operations || request_bytes > job.budget.request_bytes {
                drop(state);
                return Err(host_session_error("product host job reservation exhausted"));
            }
            job.operations += 1;
            job.request_bytes = request_bytes;
            let limit = job.budget.response_bytes / job.budget.operations - 128;
            drop(state);
            return Ok(limit as usize);
        }
        drop(state);
        let mut reservations = self
            .reservations
            .lock()
            .map_err(|_| host_session_error("product host reservation poisoned"))?;
        let limits = reservations.limits;
        let bytes = reservations
            .bytes
            .checked_add(limits.operation_bytes()?)
            .ok_or_else(|| host_session_error("product host reservation overflow"))?;
        if request_bytes > limits.max_request_bytes as usize
            || reservations.operations >= limits.max_operations
            || bytes > limits.retained_bytes
        {
            drop(reservations);
            return Err(host_session_error(
                "product host byte or operation capacity exhausted",
            ));
        }
        reservations.operations += 1;
        reservations.bytes = bytes;
        drop(reservations);
        Ok(limits.max_response_bytes as usize)
    }
}

impl ProductHostJob {
    pub(crate) fn invocation(&self) -> &bmux_plugin_runtime::InvocationScope {
        &self.inner.scope
    }

    /// Start one bounded asynchronous product callback exactly once per caller key.
    /// Duplicate keys compare a cryptographic digest of normalized operation facts;
    /// matching retries return the original observation index, never replay work.
    ///
    /// # Errors
    /// Rejects conflicting duplicates, closed children, and exhausted reservations.
    pub fn start_callback_once(
        &self,
        request_id: [u8; 16],
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::Result<usize> {
        use sha2::{Digest, Sha256};
        if request_id == [0; 16]
            || [&request.capability, &request.interface, &request.operation]
                .into_iter()
                .any(|value| {
                    value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
                })
        {
            return Err(host_session_error(
                "invalid product host operation identity",
            ));
        }
        let request_size = [
            request.capability.len(),
            request.interface.len(),
            request.operation.len(),
            request.payload.len(),
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .ok_or_else(|| host_session_error("product host request size overflow"))?;
        let max_request_bytes = self
            .inner
            .reservations
            .lock()
            .map_err(|_| host_session_error("product host reservation poisoned"))?
            .limits
            .max_request_bytes;
        if request_size > max_request_bytes as usize {
            return Err(host_session_error(
                "product host request exceeds admission bound",
            ));
        }
        let mut digest = Sha256::new();
        let kind = match request.kind {
            bmux_plugin_sdk::ServiceKind::Query => 0u8,
            bmux_plugin_sdk::ServiceKind::Command => 1,
            bmux_plugin_sdk::ServiceKind::Event => 2,
        };
        digest.update([kind]);
        let mut request_bytes = 0usize;
        for bytes in [
            request.capability.as_bytes(),
            request.interface.as_bytes(),
            request.operation.as_bytes(),
            request.payload.as_slice(),
        ] {
            request_bytes = request_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| host_session_error("product host request size overflow"))?;
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        let fingerprint: [u8; 32] = digest.finalize().into();
        let mut requests = self
            .inner
            .operation_requests
            .lock()
            .map_err(|_| host_session_error("product host operation ledger poisoned"))?;
        if let Some(ProductHostOperationKey {
            fingerprint: original,
            index,
            ..
        }) = requests.get(&request_id).copied()
        {
            drop(requests);
            return if original == fingerprint {
                Ok(index)
            } else {
                Err(host_session_error(
                    "conflicting product host operation request",
                ))
            };
        }
        let services = self.inner.services.clone();
        let index = self.start_operation(
            "retained-product-operation",
            request_bytes,
            move |_| async move { services.call_service_async(request).await },
        )?;
        let operation = uuid::Uuid::new_v4();
        self.inner
            .operation_ids
            .lock()
            .map_err(|_| host_session_error("operation identity index poisoned"))?
            .insert(operation, request_id);
        requests.insert(
            request_id,
            ProductHostOperationKey {
                fingerprint,
                index,
                operation,
                receipt: uuid::Uuid::new_v4(),
            },
        );
        drop(requests);
        Ok(index)
    }

    /// Read a bounded page of idempotent operation keys, including ack tombstones.
    /// The returned index/receipt remain meaningful only through this job capability.
    ///
    /// # Errors
    /// Reports poisoned operation bookkeeping.
    pub fn operation_page(
        &self,
        after: Option<[u8; 16]>,
        limit: usize,
    ) -> bmux_plugin_sdk::Result<Vec<ProductHostOperationIdentity>> {
        let requests = self
            .inner
            .operation_requests
            .lock()
            .map_err(|_| host_session_error("product host operation ledger poisoned"))?;
        let start = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let page = requests
            .range((start, std::ops::Bound::Unbounded))
            .take(limit.min(100))
            .map(|(request, operation)| ProductHostOperationIdentity {
                request_id: *request,
                index: operation.index,
            })
            .collect();
        drop(requests);
        Ok(page)
    }

    /// Observe B1's closed typed terminal model without interpreting error text.
    /// Response/failure records remain reserved until receipt acknowledgement.
    ///
    /// # Errors
    /// Rejects acknowledged/unknown operations or poisoned retained state.
    pub fn typed_callback_outcome(
        &self,
        index: usize,
    ) -> bmux_plugin_sdk::Result<Option<SessionOutcome>> {
        let callbacks = self
            .inner
            .callbacks
            .lock()
            .map_err(|_| host_session_error("product host callbacks poisoned"))?;
        let outcome = callbacks
            .get(index)
            .cloned()
            .ok_or_else(|| host_session_error("unknown product host operation"))?;
        drop(callbacks);
        if outcome
            .acknowledged
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(host_session_error(
                "product host outcome already acknowledged",
            ));
        }
        let value = outcome
            .value
            .lock()
            .map_err(|_| host_session_error("product host outcome poisoned"))?;
        let terminal = value.as_ref().map(|result| {
            result.as_ref().map_or_else(
                |_| match outcome
                    .terminal_kind
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    1 => SessionOutcome::RejectedBeforeEffects {
                        code: SessionCode::AdmissionsClosed,
                    },
                    3 => SessionOutcome::EffectsUnresolved {
                        code: SessionCode::ResponseLimitExceeded,
                        observed_bytes: Some(
                            outcome
                                .observed_bytes
                                .load(std::sync::atomic::Ordering::Acquire),
                        ),
                    },
                    _ => SessionOutcome::EffectsUnresolved {
                        code: SessionCode::ProducerFailed,
                        observed_bytes: None,
                    },
                },
                |payload| SessionOutcome::Completed {
                    payload: payload.clone(),
                },
            )
        });
        drop(value);
        Ok(terminal)
    }

    /// Encode the generic v1 terminal record (16-byte LE header plus success bytes).
    /// `None` is pending. Repeated reads preserve the exact terminal record.
    ///
    /// # Errors
    /// Rejects unknown operations or poisoned retained outcome state.
    pub fn terminal_record(&self, index: usize) -> bmux_plugin_sdk::Result<Option<Vec<u8>>> {
        let callbacks = self
            .inner
            .callbacks
            .lock()
            .map_err(|_| host_session_error("product host callbacks poisoned"))?;
        let outcome = callbacks
            .get(index)
            .cloned()
            .ok_or_else(|| host_session_error("unknown product host operation"))?;
        drop(callbacks);
        if outcome
            .acknowledged
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(host_session_error(
                "product host outcome already acknowledged",
            ));
        }
        let guard = outcome
            .value
            .lock()
            .map_err(|_| host_session_error("product host outcome poisoned"))?;
        let Some(value) = guard.as_ref() else {
            return Ok(None);
        };
        let kind = outcome
            .terminal_kind
            .load(std::sync::atomic::Ordering::Acquire);
        let observed = outcome
            .observed_bytes
            .load(std::sync::atomic::Ordering::Acquire);
        let payload = value.as_ref().ok().map_or(&[][..], Vec::as_slice);
        let mut record = Vec::with_capacity(16 + payload.len());
        record.extend_from_slice(&1u32.to_le_bytes());
        record.extend_from_slice(&kind.to_le_bytes());
        record.extend_from_slice(&observed.to_le_bytes());
        record.extend_from_slice(payload);
        drop(guard);
        Ok(Some(record))
    }

    /// Return the stable terminal receipt for one operation in this scoped job.
    /// Zero/pending has no receipt. Receipts validate acknowledgement, not authority.
    ///
    /// # Errors
    /// Rejects unknown operations, poisoned state, or exhausted receipt encoding.
    pub fn callback_receipt(&self, index: usize) -> bmux_plugin_sdk::Result<Option<u64>> {
        let callbacks = self
            .inner
            .callbacks
            .lock()
            .map_err(|_| host_session_error("product host callbacks poisoned"))?;
        let outcome = callbacks
            .get(index)
            .cloned()
            .ok_or_else(|| host_session_error("unknown product host operation"))?;
        drop(callbacks);
        let terminal = outcome
            .acknowledged
            .load(std::sync::atomic::Ordering::Acquire)
            || outcome
                .value
                .lock()
                .map_err(|_| host_session_error("product host outcome poisoned"))?
                .is_some();
        if !terminal {
            return Ok(None);
        }
        let receipt = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(|| host_session_error("product host receipt exhausted"))?;
        Ok(Some(receipt))
    }

    /// Acknowledge an exact terminal receipt without removing its retry tombstone.
    /// Release of the joined job remains a separate explicit lifecycle operation.
    ///
    /// # Errors
    /// Rejects pending, foreign, unknown, or mismatched receipts.
    pub fn acknowledge_callback(&self, index: usize, receipt: u64) -> bmux_plugin_sdk::Result<()> {
        if self.callback_receipt(index)? != Some(receipt) {
            return Err(host_session_error("product host terminal receipt mismatch"));
        }
        let callbacks = self
            .inner
            .callbacks
            .lock()
            .map_err(|_| host_session_error("product host callbacks poisoned"))?;
        let outcome = callbacks
            .get(index)
            .cloned()
            .ok_or_else(|| host_session_error("unknown product host operation"))?;
        drop(callbacks);
        let mut value = outcome
            .value
            .lock()
            .map_err(|_| host_session_error("product host outcome poisoned"))?;
        let removed = value.replace(Err("product host outcome acknowledged".to_owned()));
        drop(value);
        drop(removed);
        outcome
            .acknowledged
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Observe one retained outcome without consuming it, including after closure.
    ///
    /// # Errors
    /// Rejects unknown indices or poisoned retained state.
    pub fn observe_callback(
        &self,
        index: usize,
    ) -> bmux_plugin_sdk::Result<ProductHostObservation> {
        let callbacks = self
            .inner
            .callbacks
            .lock()
            .map_err(|_| host_session_error("product host callbacks poisoned"))?;
        let outcome = callbacks
            .get(index)
            .cloned()
            .ok_or_else(|| host_session_error("unknown product host operation"))?;
        drop(callbacks);
        let value = outcome
            .value
            .lock()
            .map_err(|_| host_session_error("product host outcome poisoned"))?
            .clone();
        Ok(value)
    }

    pub(crate) fn start_operation<F, Fut>(
        &self,
        name: &str,
        request_bytes: usize,
        work: F,
    ) -> bmux_plugin_sdk::Result<usize>
    where
        F: FnOnce(bmux_plugin_runtime::background::TaskCancellation) -> Fut + Send + 'static,
        Fut: Future<Output = bmux_plugin_sdk::Result<Vec<u8>>> + Send + 'static,
    {
        let admission = self
            .inner
            .scope
            .try_admit()
            .map_err(|_| host_session_error("product host job closed"))?;
        let producer = self.begin_producer()?;
        let response_limit = self.inner.reserve_response(request_bytes)?;
        let outcome = Arc::new(ProductHostCallbackOutcome {
            value: std::sync::Mutex::new(None),
            terminal_kind: std::sync::atomic::AtomicU32::new(0),
            observed_bytes: std::sync::atomic::AtomicU64::new(0),
            acknowledged: std::sync::atomic::AtomicBool::new(false),
            ready: tokio::sync::Notify::new(),
            response_limit,
        });
        let index = {
            let mut callbacks = self
                .inner
                .callbacks
                .lock()
                .map_err(|_| host_session_error("product host callbacks poisoned"))?;
            if callbacks.len() >= self.inner.callback_limit.get() {
                return Err(host_session_error(
                    "product host outcome capacity exhausted",
                ));
            }
            let index = callbacks.len();
            callbacks.push(Arc::clone(&outcome));
            drop(callbacks);
            index
        };
        let completed = ProductHostOutcomeWriter(Arc::clone(&outcome));
        if self
            .inner
            .scope
            .spawn(name, move |cancel| async move {
                let _producer = producer;
                let result = work(cancel)
                    .await
                    .map_err(|_| "product host operation failed; do not replay".to_owned());
                completed.complete(result)
            })
            .is_err()
        {
            outcome
                .terminal_kind
                .store(1, std::sync::atomic::Ordering::Release);
            *outcome
                .value
                .lock()
                .map_err(|_| host_session_error("product host outcome poisoned"))? =
                Some(Err("product host operation not admitted".to_owned()));
            // The identity and terminal rejection are retained. Return the
            // operation index so a lost begin reply can recover without replay.
            drop(admission);
            return Ok(index);
        }
        drop(admission);
        Ok(index)
    }

    /// Inspect all retained callback outcomes after settlement, in admission order.
    /// Observation never acknowledges or removes outcomes. `None` means unresolved.
    /// The callback count is admission-bounded; product responses must additionally
    /// obey the product port's negotiated byte bound before this adapter is used.
    ///
    /// # Errors
    /// Reports poisoned retained outcome state.
    pub fn callback_outcomes(&self) -> bmux_plugin_sdk::Result<ProductHostOutcomes> {
        let callbacks = self
            .inner
            .callbacks
            .lock()
            .map_err(|_| host_session_error("product host callbacks poisoned"))?;
        callbacks
            .iter()
            .map(|outcome| {
                outcome
                    .value
                    .lock()
                    .map(|value| value.clone())
                    .map_err(|_| host_session_error("product host outcome poisoned"))
            })
            .collect()
    }

    /// Return callbacks bound to this child, never the originating frontend invocation.
    /// These callbacks reject new admission after child/session/activation closure.
    #[must_use]
    pub fn services(&self) -> bmux_plugin_sdk::definition::PluginHostServices {
        let inner = Arc::clone(&self.inner);
        bmux_plugin_sdk::definition::PluginHostServices::new(|_| {
            Err(host_session_error(
                "retained product jobs require asynchronous callbacks",
            ))
        })
        .with_async(move |request| {
            let inner = Arc::clone(&inner);
            Box::pin(async move {
                let admission = inner
                    .scope
                    .try_admit()
                    .map_err(|_| host_session_error("product host job closed"))?;
                // Reserve retention before dispatch; cancelling the observer cannot
                // drop the producer or silently consume its effectful result.
                let request_bytes = [
                    request.payload.len(),
                    request.capability.len(),
                    request.interface.len(),
                    request.operation.len(),
                ]
                .into_iter()
                .try_fold(0usize, usize::checked_add)
                .ok_or_else(|| host_session_error("product host request size overflow"))?;
                let producer = Self {
                    inner: Arc::clone(&inner),
                }
                .begin_producer()?;
                let response_limit = inner.reserve_response(request_bytes)?;
                let outcome = Arc::new(ProductHostCallbackOutcome {
                    value: std::sync::Mutex::new(None),
                    terminal_kind: std::sync::atomic::AtomicU32::new(0),
                    observed_bytes: std::sync::atomic::AtomicU64::new(0),
                    acknowledged: std::sync::atomic::AtomicBool::new(false),
                    ready: tokio::sync::Notify::new(),
                    response_limit,
                });
                {
                    let mut callbacks = inner
                        .callbacks
                        .lock()
                        .map_err(|_| host_session_error("product host callbacks poisoned"))?;
                    if callbacks.len() >= inner.callback_limit.get() {
                        return Err(host_session_error(
                            "product host outcome capacity exhausted",
                        ));
                    }
                    callbacks.push(Arc::clone(&outcome));
                }
                let completed = ProductHostOutcomeWriter(Arc::clone(&outcome));
                let services = inner.services.clone();
                if inner
                    .scope
                    .spawn("retained-product-callback", move |_| async move {
                        let _producer = producer;
                        // Cancellation is observed by the actual product operation;
                        // never select/drop an admitted effectful callback here.
                        let result = services
                            .call_service_async(request)
                            .await
                            .map_err(|_| "product host callback failed; do not replay".to_owned());
                        completed.complete(result)
                    })
                    .is_err()
                {
                    outcome
                        .terminal_kind
                        .store(1, std::sync::atomic::Ordering::Release);
                    *outcome
                        .value
                        .lock()
                        .map_err(|_| host_session_error("product host outcome poisoned"))? =
                        Some(Err("product host callback not admitted".to_owned()));
                    return Err(host_session_error("product host callback not admitted"));
                }
                drop(admission);
                loop {
                    let ready = outcome.ready.notified();
                    if let Some(result) = outcome
                        .value
                        .lock()
                        .map_err(|_| host_session_error("product host outcome poisoned"))?
                        .as_ref()
                    {
                        return result
                            .clone()
                            .map_err(|details| host_session_error(&details));
                    }
                    ready.await;
                }
            })
        })
    }

    /// Start work under the child's retained task owner. The work must retain its
    /// effect outcome before returning, including cancellation paths.
    ///
    /// # Errors
    /// Rejects closed scopes or task capacity exhaustion before running work.
    pub fn spawn<F, Fut>(&self, name: &str, work: F) -> bmux_plugin_sdk::Result<()>
    where
        F: FnOnce(bmux_plugin_runtime::background::TaskCancellation) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let producer = self.begin_producer()?;
        self.inner
            .scope
            .spawn(name, move |cancel| async move {
                let _producer = producer;
                work(cancel).await
            })
            .map_err(|_| host_session_error("product host job spawn failed"))
    }

    /// Close job admission without requesting cancellation or claiming settlement.
    /// Observation and acknowledgement remain available afterward.
    ///
    /// # Errors
    /// Reports poisoned reservation bookkeeping.
    pub fn close(&self) -> bmux_plugin_sdk::Result<()> {
        let reservation = self
            .inner
            .reservation
            .lock()
            .map_err(|_| host_session_error("product host job reservation poisoned"))?;
        self.inner
            .admissions_closed
            .store(true, std::sync::atomic::Ordering::Release);
        drop(reservation);
        Ok(())
    }

    fn begin_producer(&self) -> bmux_plugin_sdk::Result<ProductHostProducer> {
        let reservation = self
            .inner
            .reservation
            .lock()
            .map_err(|_| host_session_error("product host reservation poisoned"))?;
        if self
            .inner
            .admissions_closed
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .inner
                .session_closed
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(host_session_error("product host admissions closed"));
        }
        let producer = ProductHostProducer::new(Arc::clone(&self.inner.producers));
        drop(reservation);
        Ok(producer)
    }

    /// Observe settlement without cancelling producers. Closing fences new work;
    /// runtime cleanup starts only after every admitted producer has returned.
    ///
    /// # Errors
    /// Reports poisoned bookkeeping or incomplete runtime cleanup.
    pub async fn observe_join(&self) -> bmux_plugin_sdk::Result<bool> {
        if self.inner.joined.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(true);
        }
        self.close()?;
        if self
            .inner
            .producers
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
        {
            return Ok(false);
        }
        self.join().await?;
        Ok(true)
    }

    /// Fence new callbacks, request cancellation, and explicitly join admitted work.
    /// Does not consume its outcome or release its resource.
    ///
    /// # Errors
    /// Reports task failure or incomplete runtime cleanup.
    pub async fn join(&self) -> bmux_plugin_sdk::Result<()> {
        if self.inner.joined.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        self.close()?;
        self.inner
            .scope
            .drain()
            .await
            .map_err(|_| host_session_error("product host job not settled"))?;
        self.inner
            .joined
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod product_host_tests {
    use super::*;
    use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(super) fn active() -> (InstanceScope, bmux_plugin_runtime::ActivationScope) {
        let instance = InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        (instance, activation)
    }
    fn request() -> bmux_plugin_sdk::HostServiceOwnedRequest {
        bmux_plugin_sdk::HostServiceOwnedRequest {
            capability: "test.host/v1".to_owned(),
            kind: bmux_plugin_sdk::ServiceKind::Command,
            interface: "test.host/v1".to_owned(),
            operation: "invoke".to_owned(),
            payload: vec![1, 2, 3],
        }
    }
    fn services(calls: Arc<AtomicUsize>) -> bmux_plugin_sdk::definition::PluginHostServices {
        bmux_plugin_sdk::definition::PluginHostServices::new(|_| panic!("no synchronous fallback"))
            .with_async(move |request| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(request.payload)
                })
            })
    }
    pub(super) async fn finish(
        instance: &InstanceScope,
        activation: &bmux_plugin_runtime::ActivationScope,
    ) {
        activation.close().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
        instance.close().unwrap();
        instance.drain().await.unwrap();
        instance.finish().unwrap();
    }

    #[tokio::test]
    async fn deferred_product_job_survives_origin_but_not_its_own_closure() {
        let (instance, activation) = active();
        let calls = Arc::new(AtomicUsize::new(0));
        let origin = activation.begin_invocation().unwrap();
        let session =
            ProductHostSession::new(&activation, services(calls.clone()), Arc::new(())).unwrap();
        let job = session
            .begin_job(Arc::new(()), std::num::NonZeroUsize::new(2).unwrap())
            .unwrap();
        let port = job.services();
        let lazy = port.call_service_async(request());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        origin.close().unwrap();
        origin.drain().await.unwrap();
        origin.finish().unwrap();
        assert_eq!(lazy.await.unwrap(), vec![1, 2, 3]);
        assert!(port.invoke(request().as_request()).is_err());
        job.join().await.unwrap();
        assert!(port.call_service_async(request()).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(session.acknowledge_job(&job).is_err());
        job.acknowledge_callback(0, job.callback_receipt(0).unwrap().unwrap())
            .unwrap();
        session.acknowledge_job(&job).unwrap();
        session.join().await.unwrap();
        session.release().unwrap();
        finish(&instance, &activation).await;
    }

    #[tokio::test]
    async fn dropped_observer_keeps_effect_outcome_until_explicit_join_and_acknowledgement() {
        let (instance, activation) = active();
        let entered = Arc::new(tokio::sync::Notify::new());
        let complete = Arc::new(tokio::sync::Notify::new());
        let port = bmux_plugin_sdk::definition::PluginHostServices::new(|_| panic!("sync"))
            .with_async({
                let entered = entered.clone();
                let complete = complete.clone();
                move |_| {
                    let entered = entered.clone();
                    let complete = complete.clone();
                    Box::pin(async move {
                        entered.notify_one();
                        complete.notified().await;
                        Ok(vec![9; 17])
                    })
                }
            });
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let session = ProductHostSession::new(&activation, port, Arc::new(())).unwrap();
        let job = session
            .begin_job(owner.clone(), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        drop(owner);
        let port = job.services();
        let observer = tokio::spawn(async move { port.call_service_async(request()).await });
        entered.notified().await;
        observer.abort();
        assert!(observer.await.unwrap_err().is_cancelled());
        session.close().unwrap();
        assert!(session.release().is_err());
        assert!(session.acknowledge_job(&job).is_err());
        let mut joining = Box::pin(session.join());
        let poll =
            std::future::poll_fn(|cx| std::task::Poll::Ready(joining.as_mut().poll(cx))).await;
        assert!(poll.is_pending());
        drop(joining);
        assert!(weak.upgrade().is_some());
        complete.notify_one();
        session.join().await.unwrap();
        assert_eq!(
            job.callback_outcomes().unwrap(),
            vec![Some(Ok(vec![9; 17]))]
        );
        assert!(session.release().is_err());
        job.acknowledge_callback(0, job.callback_receipt(0).unwrap().unwrap())
            .unwrap();
        session.acknowledge_job(&job).unwrap();
        drop(job);
        assert!(weak.upgrade().is_none());
        session.release().unwrap();
        finish(&instance, &activation).await;
    }

    #[tokio::test]
    async fn capacity_rejects_before_effects_and_sibling_cannot_acknowledge_outcome() {
        let (instance, activation) = active();
        let calls = Arc::new(AtomicUsize::new(0));
        let first =
            ProductHostSession::new(&activation, services(calls.clone()), Arc::new(())).unwrap();
        let second =
            ProductHostSession::new(&activation, services(calls.clone()), Arc::new(())).unwrap();
        let job = first
            .begin_job(Arc::new(()), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        let port = job.services();
        assert_eq!(
            port.call_service_async(request()).await.unwrap(),
            vec![1, 2, 3]
        );
        assert!(port.call_service_async(request()).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        job.join().await.unwrap();
        assert!(second.acknowledge_job(&job).is_err());
        assert_eq!(
            job.callback_outcomes().unwrap(),
            vec![Some(Ok(vec![1, 2, 3]))]
        );
        job.acknowledge_callback(0, job.callback_receipt(0).unwrap().unwrap())
            .unwrap();
        first.acknowledge_job(&job).unwrap();
        first.join().await.unwrap();
        first.release().unwrap();
        second.join().await.unwrap();
        second.release().unwrap();
        finish(&instance, &activation).await;
    }

    #[tokio::test]
    async fn reservation_limits_precede_dispatch_and_oversize_is_retained_uncertainty() {
        let (instance, activation) = active();
        let calls = Arc::new(AtomicUsize::new(0));
        let port = services(calls.clone());
        let limits = ProductHostLimits {
            max_jobs: 2,
            max_operations: 1,
            max_request_bytes: 64,
            max_response_bytes: 2,
            retained_bytes: 1090,
        };
        let session =
            ProductHostSession::new_with_limits(&activation, port, Arc::new(()), limits).unwrap();
        let owner = Arc::new(());
        let limit = std::num::NonZeroUsize::new(2).unwrap();
        let job = session
            .begin_job_once([1; 16], owner.clone(), limit)
            .unwrap();
        let same = session
            .begin_job_once([1; 16], owner.clone(), limit)
            .unwrap();
        assert!(Arc::ptr_eq(&job.inner, &same.inner));
        assert!(
            session
                .begin_job_once([1; 16], Arc::new(()), limit)
                .is_err()
        );
        let index = job.start_callback_once([2; 16], request()).unwrap();
        assert_eq!(job.start_callback_once([2; 16], request()).unwrap(), index);
        let mut conflicting = request();
        conflicting.payload = vec![7];
        assert!(job.start_callback_once([2; 16], conflicting).is_err());
        assert!(job.start_callback_once([3; 16], request()).is_err());
        job.join().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            matches!(job.observe_callback(index).unwrap(), Some(Err(message)) if message.contains("effects unresolved"))
        );
        assert_eq!(job.start_callback_once([2; 16], request()).unwrap(), index);
        assert!(session.release_job(&job).is_err());
        let record = job.terminal_record(index).unwrap().unwrap();
        assert_eq!(record.len(), 16);
        assert_eq!(&record[0..4], &1u32.to_le_bytes());
        assert_eq!(&record[4..8], &3u32.to_le_bytes());
        assert_eq!(&record[8..16], &3u64.to_le_bytes());
        assert!(job.acknowledge_callback(index, 99).is_err());
        let receipt = job.callback_receipt(index).unwrap().unwrap();
        job.acknowledge_callback(index, receipt).unwrap();
        job.acknowledge_callback(index, receipt).unwrap();
        session.release_job(&job).unwrap();
        session.acknowledge_job(&job).unwrap();
        assert_eq!(job.start_callback_once([2; 16], request()).unwrap(), index);
        session.join().await.unwrap();
        session.release().unwrap();
        session.release().unwrap();
        finish(&instance, &activation).await;
    }

    #[tokio::test]
    async fn oversized_request_rejects_before_callback_effects() {
        let (instance, activation) = active();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = ProductHostSession::new_with_limits(
            &activation,
            services(calls.clone()),
            Arc::new(()),
            ProductHostLimits {
                max_request_bytes: 1,
                ..ProductHostLimits::default()
            },
        )
        .unwrap();
        let job = session
            .begin_job(Arc::new(()), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert!(job.start_callback_once([1; 16], request()).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(job.callback_outcomes().unwrap(), ProductHostOutcomes::new());
        job.join().await.unwrap();
        session.acknowledge_job(&job).unwrap();
        session.join().await.unwrap();
        session.release().unwrap();
        finish(&instance, &activation).await;
    }

    #[tokio::test]
    async fn exact_job_reservation_is_charged_at_begin_and_receipts_gate_release() {
        let (instance, activation) = active();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = ProductHostSession::new_with_limits(
            &activation,
            services(calls.clone()),
            Arc::new(()),
            ProductHostLimits {
                max_jobs: 4,
                max_operations: 2,
                max_request_bytes: 64,
                max_response_bytes: 260,
                retained_bytes: 4096,
            },
        )
        .unwrap();
        let budget = JobReservation {
            operations: 2,
            request_bytes: 64,
            response_bytes: 260,
        };
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let job = session
            .begin_reserved_job_once([1; 16], owner.clone(), budget)
            .unwrap();
        drop(owner);
        let retry = session
            .begin_reserved_job_once([1; 16], Arc::new(()), budget)
            .unwrap();
        assert!(Arc::ptr_eq(&job.inner, &retry.inner));
        assert!(
            session
                .begin_reserved_job_once(
                    [1; 16],
                    Arc::new(()),
                    JobReservation {
                        operations: 1,
                        ..budget
                    }
                )
                .is_err()
        );
        // Job slots are available, but the reserved operation capacity is not.
        assert!(
            session
                .begin_reserved_job_once([2; 16], Arc::new(()), budget)
                .is_err()
        );
        let index = job.start_callback_once([3; 16], request()).unwrap();
        job.join().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            job.typed_callback_outcome(index).unwrap(),
            Some(SessionOutcome::EffectsUnresolved {
                code: SessionCode::ResponseLimitExceeded,
                observed_bytes: Some(3)
            })
        ));
        assert!(session.acknowledge_job(&job).is_err());
        let receipt = job.callback_receipt(index).unwrap().unwrap();
        assert!(job.acknowledge_callback(index, receipt + 1).is_err());
        job.acknowledge_callback(index, receipt).unwrap();
        job.acknowledge_callback(index, receipt).unwrap();
        assert!(job.typed_callback_outcome(index).is_err());
        session.release_job(&job).unwrap();
        assert!(
            weak.upgrade().is_none(),
            "request tombstone must not retain product outcome owner"
        );
        assert_eq!(job.start_callback_once([3; 16], request()).unwrap(), index);
        session.join().await.unwrap();
        session.release().unwrap();
        finish(&instance, &activation).await;
    }

    #[test]
    fn interrupted_producer_records_uncertainty_instead_of_fabricating_settlement() {
        let outcome = Arc::new(ProductHostCallbackOutcome {
            value: std::sync::Mutex::new(None),
            terminal_kind: std::sync::atomic::AtomicU32::new(0),
            observed_bytes: std::sync::atomic::AtomicU64::new(0),
            acknowledged: std::sync::atomic::AtomicBool::new(false),
            ready: tokio::sync::Notify::new(),
            response_limit: 32,
        });
        let writer = ProductHostOutcomeWriter(Arc::clone(&outcome));
        drop(writer);
        assert!(
            matches!(outcome.value.lock().unwrap().as_ref(), Some(Err(message)) if message.contains("effects unresolved"))
        );
    }

    #[tokio::test]
    async fn activation_closure_fences_lazy_jobs_without_implying_release() {
        let (instance, activation) = active();
        let calls = Arc::new(AtomicUsize::new(0));
        let session =
            ProductHostSession::new(&activation, services(calls.clone()), Arc::new(())).unwrap();
        let job = session
            .begin_job(Arc::new(()), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        let port = job.services();
        let lazy = port.call_service_async(request());
        activation.close().unwrap();
        assert!(lazy.await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(session.release().is_err());
        session.join().await.unwrap();
        session.acknowledge_job(&job).unwrap();
        session.release().unwrap();
        finish(&instance, &activation).await;
    }
}
