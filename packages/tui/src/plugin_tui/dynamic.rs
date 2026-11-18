//! Thin selected-execution frontend proxies. All author work runs on a bounded worker;
//! the terminal frame loop only replays an M7 projection prepared by that worker.
use super::retained::RetainedFrontend;
use bcode_plugin_sdk::{
    definition::{PortableFrontendAdapter, ProductExtensionDescriptor, ProductExtensions},
    keymap,
    tui::{
        BoxedPluginTuiSurface, FocusedTextFieldEdit, FocusedTextFieldEditResult,
        FocusedTextFieldSnapshot, PluginFrontendCommand, PluginTuiAction, PluginTuiEventResult,
        PluginTuiFocusedTextFieldEditResult, PluginTuiHost, PluginTuiPresentationConfiguration,
        PluginTuiRegistry, PluginTuiSurface, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture,
        PluginTuiSurfaceOpenRequest, PluginTuiSurfaceUpdateReceiver, PluginTuiTheme,
        PluginTuiVisualAdapter, PluginTuiVisualRenderContext, portable as wire,
    },
};
use bmux_plugin::component_viewport::portable::{
    FrontendPresentationRequest, FrontendPresentationResponse, FrontendResourceId,
    PortablePresentationLimits,
};
use bmux_tui::{
    event::{Event, EventOutcome},
    geometry::Rect,
    paint::PaintCx,
};
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, atomic::Ordering},
};
use tokio::sync::{mpsc, oneshot};

type HostBinding = Arc<Mutex<Option<bmux_plugin_sdk::definition::PluginHostServices>>>;

/// One renderer-local factory adapter; its actual selected activation is carried by every proxy.
#[derive(Clone)]
pub struct DynamicFrontendAdapter {
    runtime: tokio::runtime::Handle,
    views: Arc<Mutex<Vec<Arc<RetainedFrontend>>>>,
    bindings: Arc<Mutex<std::collections::BTreeMap<String, HostBinding>>>,
    // One lazily allocated budget shared by every selected frontend in this adapter.
    // Legacy ordinary frontend-v2 paths never initialize it.
    completion_budget: Arc<std::sync::OnceLock<Arc<tokio::sync::Semaphore>>>,
    completion_records: Arc<std::sync::OnceLock<Arc<tokio::sync::Semaphore>>>,
}
impl DynamicFrontendAdapter {
    /// Construct on the frontend preparation runtime, never from daemon executable metadata.
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            runtime,
            views: Arc::new(Mutex::new(Vec::new())),
            bindings: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            completion_budget: Arc::new(std::sync::OnceLock::new()),
            completion_records: Arc::new(std::sync::OnceLock::new()),
        }
    }
    /// Allocate correlation bookkeeping against the adapter-wide budget only after the
    /// host lifetime owner has accepted responsibility. The caller must register the returned
    /// record with that owner before reserving tokens or dispatching author work.
    fn completion_record(
        &self,
        id: wire::settlement::ResponseId,
        owner: ProductExtensions,
    ) -> Result<Arc<CompletionEffectRecord>, &'static str> {
        let budget = self
            .completion_budget
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(32)));
        let capacity = Arc::clone(
            self.completion_records
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(32))),
        )
        .try_acquire_owned()
        .map_err(|_| "frontend response capacity exceeded")?;
        let record = CompletionEffectRecord::new(id, owner, Arc::clone(budget))?;
        record
            .permits
            .lock()
            .map_err(|_| "frontend capacity recovery required")?
            .push(capacity);
        Ok(record)
    }

    /// Install one explicit host routing slot before the selected activation is loaded.
    ///
    /// # Errors
    /// Rejects poisoned or duplicate selected bindings.
    pub(super) fn bind_host(&self, plugin: &str, binding: HostBinding) -> Result<(), String> {
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| "frontend bindings unavailable")?;
        if bindings.contains_key(plugin) {
            return Err("duplicate frontend host binding".to_owned());
        }
        bindings.insert(plugin.to_owned(), binding);
        drop(bindings);
        Ok(())
    }

    /// Transfer admitted rich views to the presentation's terminal output lifecycle.
    /// Take retained views constructed by this adapter.
    ///
    /// # Errors
    /// Returns an error if the retained-view lock is poisoned.
    pub fn take_views(&self) -> Result<Vec<Arc<RetainedFrontend>>, String> {
        self.views
            .lock()
            .map(|views| views.clone())
            .map_err(|_| "dynamic views unavailable".to_owned())
    }
}

/// Selected optional session. The sequence lock serializes admission, never author callbacks.
struct CorrelatedFrontend {
    adapter: DynamicFrontendAdapter,
    owner: ProductExtensions,
    session: uuid::Uuid,
    sequence: tokio::sync::Mutex<u64>,
    unresolved: AtomicBool,
}
impl CorrelatedFrontend {
    async fn execute(
        &self,
        request: wire::Request,
    ) -> (WorkerResult, Option<Arc<CompletionEffectRecord>>) {
        if self.unresolved.load(Ordering::Acquire) {
            return (
                Err("frontend original invocation unresolved; recovery required".to_owned()),
                None,
            );
        }
        let Ok(mut sequence) = self.sequence.try_lock() else {
            return (Err("frontend admission busy; no dispatch".to_owned()), None);
        };
        let Some(next) = sequence.checked_add(1) else {
            return (Err("frontend sequence exhausted".to_owned()), None);
        };
        let id = wire::settlement::ResponseId {
            session_id: self.session,
            sequence: next,
        };
        let record = match self.adapter.completion_record(id, self.owner.clone()) {
            Ok(record) => record,
            Err(error) => return (Err(error.to_owned()), None),
        };
        let token = match record.reserve(0) {
            Ok(token) => token,
            Err(error) => return (Err(error.to_owned()), Some(record)),
        };
        let installed = {
            let handoff = record.handoff.lock();
            handoff.is_ok_and(|mut handoff| {
                *handoff = Some(token);
                true
            })
        };
        if !installed {
            return (
                Err("frontend handoff requires recovery".to_owned()),
                Some(record),
            );
        }
        self.unresolved.store(true, Ordering::Release);
        *sequence = next;
        drop(sequence);
        // Keep the exact original record pinned through the invocation and response handoff.
        let retained_record = Arc::clone(&record);
        let reply = record
            .settlement_request(wire::settlement::Request::Execute {
                id,
                request: Box::new(request),
            })
            .await;
        drop(record);
        let result = match reply {
            Ok(wire::settlement::Response::Result {
                id: original,
                result: wire::settlement::FrontendResult::Completed(response),
                committed: false,
            }) if original == id => Ok(*response),
            _ => Err("frontend original response requires recovery; do not replay".to_owned()),
        };
        if result.is_ok() {
            self.unresolved.store(false, Ordering::Release);
        }
        (result, Some(retained_record))
    }
}

async fn invoke(
    owner: &ProductExtensions,
    request: wire::Request,
) -> Result<wire::Response, String> {
    let bytes =
        serde_json::to_vec(&request).map_err(|_| "frontend request encoding failed".to_owned())?;
    let response = owner
        .invoke(wire::TUI_FRONTEND_CONTRACT, bytes)
        .await
        .map_err(|_| "selected frontend operation failed; do not replay".to_owned())?;
    serde_json::from_slice(&response).map_err(|_| "invalid selected frontend response".to_owned())
}
fn blocking_invoke(
    runtime: &tokio::runtime::Handle,
    owner: &ProductExtensions,
    request: wire::Request,
) -> Result<wire::Response, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| runtime.block_on(invoke(owner, request)))
            .join()
            .map_err(|_| "frontend preparation worker failed".to_owned())?
    })
}

impl PortableFrontendAdapter for DynamicFrontendAdapter {
    fn registry(
        &self,
        owner: ProductExtensions,
        descriptor: &ProductExtensionDescriptor,
    ) -> bcode_plugin_sdk::definition::PluginResult<PluginTuiRegistry> {
        let build = || -> Result<PluginTuiRegistry, String> {
            if !descriptor.contracts.iter().any(|entry| {
                entry.interface == wire::TUI_FRONTEND_CONTRACT
                    && entry.version == wire::TUI_FRONTEND_VERSION
            }) {
                return Err("selected frontend lacks typed terminal operations".to_owned());
            }
            let view = RetainedFrontend::from_active_descriptor(
                owner.clone(),
                descriptor,
                PortablePresentationLimits::default(),
            )?;
            let wire::Response::Descriptor(declaration) =
                blocking_invoke(&self.runtime, &owner, wire::Request::Describe)?
            else {
                return Err("selected frontend did not declare a registry".to_owned());
            };
            if declaration.surfaces.len() > 128
                || declaration.commands.len() > 128
                || declaration.actions.len() > 1024
            {
                return Err("selected frontend registry exceeds budget".to_owned());
            }
            if descriptor.contracts.iter().any(|entry| {
                entry.interface == wire::settlement::CONTRACT
                    && entry.version != wire::settlement::VERSION
            }) {
                return Err(
                    "unsupported selected frontend settlement version; no fallback".to_owned(),
                );
            }
            let correlated = descriptor
                .contracts
                .iter()
                .any(|entry| {
                    entry.interface == wire::settlement::CONTRACT
                        && entry.version == wire::settlement::VERSION
                })
                .then(|| {
                    Arc::new(CorrelatedFrontend {
                        adapter: self.clone(),
                        owner: owner.clone(),
                        session: uuid::Uuid::new_v4(),
                        sequence: tokio::sync::Mutex::new(0),
                        unresolved: AtomicBool::new(false),
                    })
                });
            let mut registry = PluginTuiRegistry::default();
            registry.register_visual_adapter(
                ["bcode.frontend.configuration"],
                Box::new(Configuration {
                    owner: owner.clone(),
                    runtime: self.runtime.clone(),
                }),
            );
            let commands = Arc::new(Commands::new(
                owner.clone(),
                &self.runtime,
                correlated.clone(),
            ));
            for id in declaration.commands {
                registry.register_frontend_command(
                    id.clone(),
                    Arc::new(Command {
                        id,
                        commands: Arc::clone(&commands),
                    }),
                );
            }
            let mut seen = BTreeSet::new();
            let binding = self
                .bindings
                .lock()
                .map_err(|_| "frontend bindings unavailable")?
                .get(&descriptor.plugin_id)
                .cloned();
            for kind in declaration.surfaces {
                if kind.is_empty() || kind.len() > 256 || !seen.insert(kind.clone()) {
                    return Err("invalid or duplicate selected surface kind".to_owned());
                }
                registry.register_factory(Box::new(Factory {
                    kind,
                    owner: owner.clone(),
                    runtime: self.runtime.clone(),
                    view: Arc::clone(&view),
                    actions: declaration.actions.clone(),
                    binding: binding.clone(),
                    correlated: correlated.clone(),
                }));
            }
            for action in declaration.actions {
                registry.register_keymap_action(action);
            }
            self.views
                .lock()
                .map_err(|_| "dynamic views unavailable".to_owned())?
                .push(view);
            Ok(registry)
        };
        build().map_err(Into::into)
    }
}

struct Configuration {
    owner: ProductExtensions,
    runtime: tokio::runtime::Handle,
}
impl PluginTuiVisualAdapter for Configuration {
    fn supports(&self, _: &str) -> bool {
        false
    }
    fn rows(
        &self,
        _: &str,
        _: &serde_json::Value,
        _: &PluginTuiVisualRenderContext,
    ) -> Vec<bmux_tui::text::Line> {
        Vec::new()
    }
    fn configure(&self, configuration: &PluginTuiPresentationConfiguration) -> Result<(), String> {
        match blocking_invoke(
            &self.runtime,
            &self.owner,
            wire::Request::Configure {
                configuration: configuration.clone(),
            },
        )? {
            wire::Response::Applied => Ok(()),
            _ => Err("frontend configuration response mismatch".to_owned()),
        }
    }
}

struct Factory {
    correlated: Option<Arc<CorrelatedFrontend>>,
    binding: Option<HostBinding>,
    kind: String,
    owner: ProductExtensions,
    runtime: tokio::runtime::Handle,
    view: Arc<RetainedFrontend>,
    actions: Vec<keymap::KeymapAction>,
}
impl PluginTuiSurfaceFactory for Factory {
    fn surface_kind(&self) -> &str {
        &self.kind
    }
    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        let owner = self.owner.clone();
        let kind = self.kind.clone();
        let view = Arc::clone(&self.view);
        let runtime = self.runtime.clone();
        let actions = self.actions.clone();
        let binding = self.binding.clone();
        let correlated = self.correlated.clone();
        Box::pin(async move {
            let request = wire::Request::Open { kind, request };
            let (opened, opening) = if let Some(session) = &correlated {
                session.execute(request).await
            } else {
                (invoke(&owner, request).await, None)
            };
            let wire::Response::Opened {
                resource,
                id,
                title,
            } = opened.map_err(std::io::Error::other)?
            else {
                return Err(std::io::Error::other("frontend open response mismatch").into());
            };
            let created = view.apply(FrontendPresentationRequest::Create { generation: 1 });
            let local = match created {
                Ok(FrontendPresentationResponse::Created { resource }) => resource,
                result => {
                    // The remote open already succeeded. Explicitly release it once; never
                    // replay an open after a local admission/response failure.
                    let _ = invoke(&owner, wire::Request::Close { resource }).await;
                    return Err(std::io::Error::other(match result {
                        Err(error) => error,
                        Ok(_) => "frontend resource response mismatch".to_owned(),
                    })
                    .into());
                }
            };
            let (send, receive) = mpsc::channel(32);
            let (completed, completions) = mpsc::channel(8);
            let stopped = Arc::new(AtomicBool::new(false));
            if let Some(record) = opening {
                let mut token = record.take_handoff().map_err(std::io::Error::other)?;
                token
                    .retained()
                    .and_then(|()| token.dispatched())
                    .and_then(|()| token.terminal_applied())
                    .map_err(std::io::Error::other)?;
                monitor_completion(record);
            }
            let proxy_owner = owner.clone();
            runtime.spawn(worker(
                SurfaceExecution {
                    owner,
                    correlated,
                    remote: resource,
                    local,
                    view: Arc::clone(&view),
                    stopped: Arc::clone(&stopped),
                },
                receive,
                completed,
            ));
            Ok(Box::new(SurfaceProxy {
                owner: proxy_owner,
                id,
                title,
                remote: resource,
                local,
                view,
                send,
                completions,
                revision: None,
                projection: None,
                viewport: None,
                destination: Rect::new(0, 0, 0, 0),
                actions,
                bindings: wire::Bindings::default(),
                pending: VecDeque::new(),
                input_backlog: VecDeque::new(),
                completion: None,
                projection_completion: None,
                edit_completions: VecDeque::new(),
                edit_pending: false,
                edit_poll_pending: false,
                edit_remote_pending: false,
                edit_result: None,
                updates: None,
                stopped,
                binding,
            }) as BoxedPluginTuiSurface)
        })
    }
}

use std::collections::VecDeque;
struct Work {
    request: wire::Request,
    completion: Option<oneshot::Sender<Completion>>,
}
struct SurfaceProxy {
    owner: ProductExtensions,
    binding: Option<HostBinding>,
    id: String,
    title: String,
    remote: uuid::Uuid,
    local: FrontendResourceId,
    view: Arc<RetainedFrontend>,
    send: mpsc::Sender<Work>,
    completions: mpsc::Receiver<Completion>,
    revision: Option<u64>,
    projection: Option<wire::Projection>,
    viewport: Option<wire::Viewport>,
    destination: Rect,
    actions: Vec<keymap::KeymapAction>,
    bindings: wire::Bindings,
    pending: VecDeque<(PluginTuiAction, Option<FrontendEffectResponsibility>)>,
    input_backlog: VecDeque<wire::Request>,
    completion: Option<FrontendEffectResponsibility>,
    projection_completion: Option<(u64, PluginTuiAction, FrontendEffectResponsibility)>,
    edit_completions: VecDeque<FrontendEffectResponsibility>,
    edit_pending: bool,
    edit_poll_pending: bool,
    edit_remote_pending: bool,
    edit_result: Option<PluginTuiFocusedTextFieldEditResult>,
    updates: Option<PluginTuiSurfaceUpdateReceiver>,
    stopped: Arc<AtomicBool>,
}

type WorkerResult = Result<wire::Response, String>;

/// Correlation responsibility transferred with an actual product effect, never cloned or encoded.
/// The existing completion record owns its slot and selected-owner pin until verified retirement.
pub(crate) struct RecordEffectResponsibility {
    record: Arc<CompletionEffectRecord>,
    ordinal: usize,
}

/// Product queue adapter over the SDK-local move-only capability. No parallel ledger.
pub(crate) struct FrontendEffectResponsibility {
    inner: Box<dyn FrontendCompletionResponsibility>,
}
impl std::fmt::Debug for FrontendEffectResponsibility {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrontendEffectResponsibility")
            .field("response", &self.response_id())
            .field("ordinal", &self.ordinal())
            .field("disposition", &self.disposition())
            .finish_non_exhaustive()
    }
}
impl FrontendEffectResponsibility {
    #[must_use]
    pub(crate) fn from_opaque(inner: Box<dyn FrontendCompletionResponsibility>) -> Self {
        Self { inner }
    }
    #[must_use]
    pub(crate) fn response_id(&self) -> wire::settlement::ResponseId {
        self.inner.response_id()
    }
    #[must_use]
    pub(crate) fn ordinal(&self) -> usize {
        self.inner.ordinal()
    }
    #[must_use]
    pub(crate) fn disposition(&self) -> FrontendEffectDisposition {
        self.inner.disposition()
    }
    pub(crate) fn follow_up(&self) -> Result<Self, &'static str> {
        self.inner
            .follow_up()
            .map(Self::from_opaque)
            .map_err(|_| "frontend follow-up requires recovery")
    }
    pub(crate) fn retained(&mut self) -> Result<(), &'static str> {
        self.inner
            .retained()
            .map_err(|_| "frontend completion requires recovery")
    }
    pub(crate) fn dispatched(&mut self) -> Result<(), &'static str> {
        self.inner
            .dispatched()
            .map_err(|_| "frontend completion requires recovery")
    }
    pub(crate) fn displaced(&mut self) -> Result<(), &'static str> {
        self.inner
            .displaced()
            .map_err(|_| "frontend completion requires recovery")
    }
    pub(crate) fn not_dispatched(&mut self) -> Result<(), &'static str> {
        self.inner
            .not_dispatched()
            .map_err(|_| "frontend completion requires recovery")
    }
    pub(crate) fn terminal_applied(&mut self) -> Result<(), &'static str> {
        self.inner
            .terminal_applied()
            .map_err(|_| "frontend completion requires recovery")
    }
    #[cfg(test)]
    pub(crate) fn commit_transferred(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), CompletionResponsibilityError>> + Send + '_,
        >,
    > {
        self.inner.commit_transferred()
    }
    pub(crate) fn recovery_required(&mut self) {
        self.inner.recovery_required();
    }
}
impl FrontendCompletionResponsibility for RecordEffectResponsibility {
    fn response_id(&self) -> wire::settlement::ResponseId {
        Self::response_id(self)
    }
    fn ordinal(&self) -> usize {
        Self::ordinal(self)
    }
    fn disposition(&self) -> FrontendEffectDisposition {
        Self::disposition(self)
    }
    fn follow_up(
        &self,
    ) -> Result<Box<dyn FrontendCompletionResponsibility>, CompletionResponsibilityError> {
        Self::follow_up(self)
            .map(|token| token.inner)
            .map_err(|_| CompletionResponsibilityError::RecoveryRequired)
    }
    fn retained(&mut self) -> Result<(), CompletionResponsibilityError> {
        Self::retained(self).map_err(|_| CompletionResponsibilityError::RecoveryRequired)
    }
    fn dispatched(&mut self) -> Result<(), CompletionResponsibilityError> {
        Self::dispatched(self).map_err(|_| CompletionResponsibilityError::RecoveryRequired)
    }
    fn displaced(&mut self) -> Result<(), CompletionResponsibilityError> {
        Self::displaced(self).map_err(|_| CompletionResponsibilityError::RecoveryRequired)
    }
    fn not_dispatched(&mut self) -> Result<(), CompletionResponsibilityError> {
        Self::not_dispatched(self).map_err(|_| CompletionResponsibilityError::RecoveryRequired)
    }
    fn terminal_applied(&mut self) -> Result<(), CompletionResponsibilityError> {
        Self::terminal_applied(self).map_err(|_| CompletionResponsibilityError::RecoveryRequired)
    }
    fn commit_transferred(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), CompletionResponsibilityError>> + Send + '_,
        >,
    > {
        let record = Arc::clone(&self.record);
        Box::pin(async move {
            record
                .commit_transferred()
                .await
                .map_err(|_| CompletionResponsibilityError::RecoveryRequired)
        })
    }
    fn recovery_required(&mut self) {
        Self::recovery_required(self);
    }
}

/// Queue handoff. On reservation failure the entire value must be returned unchanged.
pub(crate) struct FrontendEffectTransfer {
    pub(crate) effect: crate::effects::TuiEffect,
    pub(crate) responsibility: FrontendEffectResponsibility,
}

impl FrontendEffectTransfer {
    /// Bind an already correlated token to its actual effect without allocating or minting authority.
    #[must_use]
    pub(crate) const fn new(
        effect: crate::effects::TuiEffect,
        responsibility: FrontendEffectResponsibility,
    ) -> Self {
        Self {
            effect,
            responsibility,
        }
    }

    /// Return both pieces unchanged, including when queue reservation fails.
    #[must_use]
    pub(crate) fn into_parts(self) -> (crate::effects::TuiEffect, FrontendEffectResponsibility) {
        (self.effect, self.responsibility)
    }
}

use wire::settlement::{
    CompletionDisposition as FrontendEffectDisposition, CompletionResponsibilityError,
    FrontendCompletionResponsibility,
};

/// Bounded slots embedded in a single existing completion, not another authority registry.
struct CompletionEffectRecord {
    id: wire::settlement::ResponseId,
    // The worker and completion both retain this record; token loss cannot release selected code.
    owner: ProductExtensions,
    slots: Mutex<[Option<FrontendEffectDisposition>; 32]>,
    retired: AtomicBool,
    changed: tokio::sync::Notify,
    budget: Arc<tokio::sync::Semaphore>,
    // Capacity remains charged to the existing record, including after token/queue drop.
    permits: Mutex<Vec<tokio::sync::OwnedSemaphorePermit>>,
    // Identity only: no stored ActivationScope back-edge. The actual activation tree owns us.
    registration: std::sync::OnceLock<bmux_plugin_runtime::PluginResourceId>,
    // Reserved before awaiting commit; a lost reply cannot reopen publication or replay.
    commit_started: AtomicBool,
    commit_confirmed: AtomicBool,
    handoff: Mutex<Option<FrontendEffectResponsibility>>,
}

impl CompletionEffectRecord {
    /// Construct only after negotiated correlation and byte/slot capacity were reserved.
    fn new(
        id: wire::settlement::ResponseId,
        owner: ProductExtensions,
        budget: Arc<tokio::sync::Semaphore>,
    ) -> Result<Arc<Self>, &'static str> {
        if id.session_id.is_nil() || id.sequence == 0 {
            return Err("invalid frontend completion identity");
        }
        let scope = owner
            .activation_scope()
            .map_err(|_| "actual frontend activation unavailable")?;
        let record = Arc::new(Self {
            id,
            owner,
            slots: Mutex::new([None; 32]),
            retired: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
            budget,
            permits: Mutex::new(Vec::new()),
            registration: std::sync::OnceLock::new(),
            commit_started: AtomicBool::new(false),
            commit_confirmed: AtomicBool::new(false),
            handoff: Mutex::new(None),
        });
        let resource = scope
            .register_resource(Arc::clone(&record))
            .map_err(|_| "frontend completion resource admission rejected")?;
        // This fresh cell is written once, before any token or dispatch can be published.
        if record.registration.set(resource).is_err() {
            return Err("frontend completion registration requires recovery");
        }
        Ok(record)
    }

    fn take_handoff(&self) -> Result<FrontendEffectResponsibility, &'static str> {
        self.handoff
            .lock()
            .map_err(|_| "frontend handoff requires recovery")?
            .take()
            .ok_or("frontend responsibility already transferred")
    }

    fn reserve(
        self: &Arc<Self>,
        ordinal: usize,
    ) -> Result<FrontendEffectResponsibility, &'static str> {
        if self.commit_started.load(Ordering::Acquire) {
            return Err("frontend response publication already committed");
        }
        self.reserve_follow_up(ordinal)
    }

    fn reserve_follow_up(
        self: &Arc<Self>,
        ordinal: usize,
    ) -> Result<FrontendEffectResponsibility, &'static str> {
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| "frontend completion recovery required")?;
        let occupied = slots
            .get(ordinal)
            .ok_or("frontend effect capacity exceeded")?
            .is_some();
        if self.retired.load(Ordering::Acquire) || occupied {
            return Err("frontend effect slot already reserved or retired");
        }
        self.permits
            .lock()
            .map_err(|_| "frontend capacity recovery required")?
            .push(
                Arc::clone(&self.budget)
                    .try_acquire_owned()
                    .map_err(|_| "frontend global completion capacity exceeded")?,
            );
        slots[ordinal] = Some(FrontendEffectDisposition::Reserved);
        drop(slots);
        Ok(FrontendEffectResponsibility::from_opaque(Box::new(
            RecordEffectResponsibility {
                record: Arc::clone(self),
                ordinal,
            },
        )))
    }

    async fn settlement_request(
        &self,
        request: wire::settlement::Request,
    ) -> Result<wire::settlement::Response, &'static str> {
        let payload = serde_json::to_vec(&wire::settlement::RequestEnvelope {
            schema: wire::settlement::VERSION,
            request,
        })
        .map_err(|_| "frontend settlement encoding failed")?;
        let bytes = self
            .owner
            .invoke(wire::settlement::CONTRACT, payload)
            .await
            .map_err(|_| "frontend settlement requires recovery; do not replay")?;
        if bytes.len() > bcode_plugin_sdk::SERVICE_BRIDGE_MAX_RESPONSE_BYTES {
            return Err("frontend settlement response exceeds budget");
        }
        let response: wire::settlement::ResponseEnvelope = serde_json::from_slice(&bytes)
            .map_err(|_| "frontend settlement response requires recovery")?;
        if response.schema != wire::settlement::VERSION {
            return Err("unsupported frontend settlement schema");
        }
        Ok(response.response)
    }

    /// Only the bound consumer may call after every effect has transferred to an existing
    /// completion owner. No callback receipt acknowledgment happens on delivery or decoding.
    async fn commit_transferred(&self) -> Result<(), &'static str> {
        {
            let slots = self
                .slots
                .lock()
                .map_err(|_| "frontend completion requires recovery")?;
            if slots.iter().flatten().any(|state| {
                !matches!(
                    state,
                    FrontendEffectDisposition::Retained
                        | FrontendEffectDisposition::Dispatched
                        | FrontendEffectDisposition::NotDispatched
                        | FrontendEffectDisposition::TerminalApplied
                )
            }) {
                return Err("frontend effect responsibility has not transferred");
            }
            drop(slots);
            if self.commit_started.swap(true, Ordering::AcqRel) {
                return if self.commit_confirmed.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err("frontend commitment requires original-response recovery")
                };
            }
        }
        match self
            .settlement_request(wire::settlement::Request::CommitResponse { id: self.id })
            .await?
        {
            wire::settlement::Response::Committed { id, .. } if id == self.id => {
                self.commit_confirmed.store(true, Ordering::Release);
                Ok(())
            }
            _ => Err("frontend commitment was not verified"),
        }
    }

    /// Observe one bounded settlement step; never release on Pending, malformed or uncertain reads.
    /// Session-settled is usable only after this exact response's commitment was confirmed.
    async fn settle_registered(&self) -> Result<bool, &'static str> {
        if !self.commit_confirmed.load(Ordering::Acquire) {
            return Err("frontend response commitment is unverified");
        }
        {
            let slots = self
                .slots
                .lock()
                .map_err(|_| "frontend completion requires recovery")?;
            if slots.iter().flatten().any(|state| {
                !matches!(
                    state,
                    FrontendEffectDisposition::NotDispatched
                        | FrontendEffectDisposition::TerminalApplied
                )
            }) {
                return Ok(false);
            }
        }
        let response = self
            .settlement_request(wire::settlement::Request::Settle {
                session_id: self.id.session_id,
                limit: 1,
            })
            .await?;
        match response {
            wire::settlement::Response::Settlement { session_id, state }
                if session_id == self.id.session_id =>
            {
                match state {
                    wire::settlement::SettlementState::Pending => Ok(false),
                    wire::settlement::SettlementState::RecoveryRequired => {
                        Err("frontend settlement requires recovery; resource retained")
                    }
                    wire::settlement::SettlementState::Settled => {
                        let resource = *self
                            .registration
                            .get()
                            .ok_or("frontend resource registration missing")?;
                        let scope = self
                            .owner
                            .activation_scope()
                            .map_err(|_| "frontend activation release authority unavailable")?;
                        // Runtime rejects release while child scopes/admitted operations remain.
                        let released = scope
                            .release_resource(resource)
                            .map_err(|_| "frontend resource still admitted; retain for recovery")?;
                        drop(released);
                        self.retired.store(true, Ordering::Release);
                        self.permits
                            .lock()
                            .map_err(|_| "frontend capacity requires recovery")?
                            .clear();
                        self.changed.notify_waiters();
                        Ok(true)
                    }
                }
            }
            _ => Err("frontend settlement response did not match the original session"),
        }
    }
}

impl std::fmt::Debug for RecordEffectResponsibility {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrontendEffectResponsibility")
            .field("response_id", &self.record.id)
            .field("ordinal", &self.ordinal)
            .field("disposition", &self.disposition())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for FrontendEffectTransfer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrontendEffectTransfer")
            .field("responsibility", &self.responsibility)
            .finish_non_exhaustive()
    }
}
impl RecordEffectResponsibility {
    /// Reserve a follow-up responsibility on the same original completion before dispatch.
    /// No new effect ledger or selected-owner authority is created.
    pub(crate) fn follow_up(&self) -> Result<FrontendEffectResponsibility, &'static str> {
        let ordinal = {
            let slots = self
                .record
                .slots
                .lock()
                .map_err(|_| "frontend completion requires recovery")?;
            slots
                .iter()
                .position(Option::is_none)
                .ok_or("frontend follow-up capacity exceeded")?
        };
        // Follow-ups are allowed after consumer commitment: they extend its existing
        // completion responsibility, not the already sealed author response publication.
        self.record.reserve_follow_up(ordinal)
    }

    /// Stable response correlation, not an authority credential.
    #[must_use]
    pub(crate) fn response_id(&self) -> wire::settlement::ResponseId {
        self.record.id
    }

    /// Stable effect slot within the original completion; never reused before retirement.
    #[must_use]
    pub(crate) const fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// Poisoned/missing state is visible uncertainty, never success.
    #[must_use]
    pub(crate) fn disposition(&self) -> FrontendEffectDisposition {
        self.record
            .slots
            .lock()
            .ok()
            .and_then(|slots| slots[self.ordinal])
            .unwrap_or(FrontendEffectDisposition::RecoveryRequired)
    }

    fn transition(
        &self,
        from: &[FrontendEffectDisposition],
        to: FrontendEffectDisposition,
    ) -> Result<(), &'static str> {
        let mut slots = self
            .record
            .slots
            .lock()
            .map_err(|_| "frontend completion recovery required")?;
        let current = slots[self.ordinal].ok_or("frontend effect slot unavailable")?;
        if current == to {
            return Ok(());
        }
        if !from.contains(&current) {
            return Err("frontend effect disposition requires recovery");
        }
        slots[self.ordinal] = Some(to);
        drop(slots);
        self.record.changed.notify_waiters();
        Ok(())
    }

    /// Call only after the existing queue/completion owner reserves and retains this token.
    /// Does not send `commit_response`; caller commits only after every effect transfers.
    pub(crate) fn retained(&self) -> Result<(), &'static str> {
        self.transition(
            &[FrontendEffectDisposition::Reserved],
            FrontendEffectDisposition::Retained,
        )
    }

    /// Record dispatch before starting an effect. Cancellation after this is uncertain.
    pub(crate) fn dispatched(&self) -> Result<(), &'static str> {
        self.transition(
            &[FrontendEffectDisposition::Retained],
            FrontendEffectDisposition::Dispatched,
        )
    }

    /// Preserve responsibility on keyed replacement; never attribute it to the replacement result.
    /// A dispatched entry cannot be declared not-dispatched by replacing its queue key.
    pub(crate) fn displaced(&self) -> Result<(), &'static str> {
        match self.disposition() {
            FrontendEffectDisposition::Reserved | FrontendEffectDisposition::Retained => self
                .transition(
                    &[
                        FrontendEffectDisposition::Reserved,
                        FrontendEffectDisposition::Retained,
                    ],
                    FrontendEffectDisposition::Displaced,
                ),
            FrontendEffectDisposition::Displaced
            | FrontendEffectDisposition::NotDispatched
            | FrontendEffectDisposition::TerminalApplied => Ok(()),
            _ => {
                self.recovery_required();
                Err("dispatched replacement requires recovery")
            }
        }
    }

    /// Queue owner verified removal before dispatch. Still not author/callback settlement.
    pub(crate) fn not_dispatched(&self) -> Result<(), &'static str> {
        self.transition(
            &[
                FrontendEffectDisposition::Reserved,
                FrontendEffectDisposition::Retained,
                FrontendEffectDisposition::Displaced,
            ],
            FrontendEffectDisposition::NotDispatched,
        )
    }

    /// Actual terminal result has been applied by the consumer, not merely delivered.
    /// Does not acknowledge receipts, infer task joining, or retire this slot.
    pub(crate) fn terminal_applied(&self) -> Result<(), &'static str> {
        self.transition(
            &[FrontendEffectDisposition::Dispatched],
            FrontendEffectDisposition::TerminalApplied,
        )
    }

    /// Retain the original completion/owner and expose unresolved responsibility.
    pub(crate) fn recovery_required(&self) {
        if let Ok(mut slots) = self.record.slots.lock() {
            slots[self.ordinal] = Some(FrontendEffectDisposition::RecoveryRequired);
        }
        self.record.changed.notify_waiters();
    }
}
impl Drop for RecordEffectResponsibility {
    fn drop(&mut self) {
        // The original worker still owns this record and waits for explicit retirement.
        // Successful local disposition is retained too; only verified receipt/job settlement
        // may release its code owner. All other token loss is explicit uncertainty.
        if !matches!(
            self.disposition(),
            FrontendEffectDisposition::NotDispatched | FrontendEffectDisposition::TerminalApplied
        ) {
            self.recovery_required();
        }
    }
}

struct Completion {
    result: WorkerResult,
    received: oneshot::Sender<()>,
    effects: Option<Arc<CompletionEffectRecord>>,
}

enum CompletionSlot {
    Stream(mpsc::OwnedPermit<Completion>),
    Direct(oneshot::Sender<Completion>),
}

impl CompletionSlot {
    async fn reserve(
        completed: &mpsc::Sender<Completion>,
        direct: Option<oneshot::Sender<Completion>>,
    ) -> Option<Self> {
        if let Some(direct) = direct {
            return (!direct.is_closed()).then_some(Self::Direct(direct));
        }
        completed
            .clone()
            .reserve_owned()
            .await
            .ok()
            .map(Self::Stream)
    }

    async fn deliver(self, result: WorkerResult) {
        self.deliver_correlated(result, None).await;
    }

    async fn deliver_correlated(
        self,
        result: WorkerResult,
        effects: Option<Arc<CompletionEffectRecord>>,
    ) {
        if let Some(record) = effects.as_ref() {
            monitor_completion(Arc::clone(record));
        }
        let (received, receipt) = oneshot::channel();
        let completion = Completion {
            result: result.clone(),
            received,
            effects,
        };
        match self {
            Self::Stream(permit) => {
                permit.send(completion);
            }
            Self::Direct(sender) => {
                let _ = sender.send(completion);
            }
        }
        drop(result);
        if receipt.await.is_err() {
            // Observation ended, not execution authority. Keep the result and worker's
            // owner resident until an explicit settlement path can reconcile them.
            std::future::pending::<()>().await;
        }
    }
}

fn monitor_completion(effects: Arc<CompletionEffectRecord>) {
    tokio::spawn(async move {
        loop {
            if !effects.commit_started.load(Ordering::Acquire) {
                let applied = effects.slots.lock().is_ok_and(|slots| {
                    matches!(slots[0], Some(FrontendEffectDisposition::TerminalApplied))
                        && slots.iter().flatten().all(|state| {
                            matches!(
                                state,
                                FrontendEffectDisposition::Retained
                                    | FrontendEffectDisposition::Dispatched
                                    | FrontendEffectDisposition::NotDispatched
                                    | FrontendEffectDisposition::TerminalApplied
                            )
                        })
                });
                if applied && effects.commit_transferred().await.is_err() {
                    break;
                }
            }
            if effects.commit_confirmed.load(Ordering::Acquire) {
                match effects.settle_registered().await {
                    Ok(false) => {}
                    Ok(true) | Err(_) => break, // registered original record remains inspectable
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
}

#[cfg(test)]
fn retain_action(
    pending: &mut VecDeque<PluginTuiAction>,
    action: PluginTuiAction,
) -> Result<(), PluginTuiAction> {
    if matches!(action, PluginTuiAction::None)
        || (matches!(action, PluginTuiAction::Redraw)
            && pending
                .iter()
                .any(|entry| matches!(entry, PluginTuiAction::Redraw)))
    {
        return Ok(());
    }
    if pending.len() >= 128 {
        return Err(action);
    }
    pending.push_back(action);
    Ok(())
}

struct SurfaceExecution {
    owner: ProductExtensions,
    correlated: Option<Arc<CorrelatedFrontend>>,
    remote: uuid::Uuid,
    local: FrontendResourceId,
    view: Arc<RetainedFrontend>,
    stopped: Arc<AtomicBool>,
}

async fn worker(
    execution: SurfaceExecution,
    mut receive: mpsc::Receiver<Work>,
    completed: mpsc::Sender<Completion>,
) {
    let SurfaceExecution {
        owner,
        correlated,
        remote,
        local,
        view,
        stopped,
    } = execution;
    let mut viewport = None;
    let mut bindings = wire::Bindings::default();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(80));
    loop {
        let Work {
            request,
            completion: direct,
        } = tokio::select! {
            work = receive.recv() => match work { Some(work) => work, None => break },
            _ = interval.tick(), if viewport.is_some() => Work {
                request: wire::Request::Prepare { resource: remote, viewport: viewport.clone().expect("guarded"), bindings: bindings.clone() }, completion: None,
            },
        };
        let Some(completion) = CompletionSlot::reserve(&completed, direct).await else {
            break;
        };
        if stopped.load(Ordering::Acquire) {
            break;
        }
        if let wire::Request::Prepare {
            viewport: next,
            bindings: next_bindings,
            ..
        } = &request
        {
            viewport = Some(next.clone());
            bindings = next_bindings.clone();
        }
        if matches!(request, wire::Request::Input { .. }) {
            let display = ViewDisplay { view: &view, local };
            let mut executor = WorkerExecutor {
                owner: &owner,
                correlated: correlated.as_deref(),
            };
            let (result, effects) = deliver_input(&display, request, &mut executor).await;
            let failed = result.is_err();
            completion.deliver_correlated(result, effects).await;
            if failed {
                break;
            }
            continue;
        }
        // Fenced operations reference a displayed snapshot; they reject, never replay.
        if let wire::Request::Edit {
            expected_revision, ..
        } = &request
            && view
                .validate_displayed(local, 1, *expected_revision)
                .is_err()
        {
            completion
                .deliver(Ok(wire::Response::Edit(
                    PluginTuiFocusedTextFieldEditResult::rejected(
                        bcode_plugin_sdk::composer::ComposerValidationError::Stale,
                    ),
                )))
                .await;
            continue;
        }
        let (mut result, effects) = execute(&owner, correlated.as_deref(), request).await;
        if let Ok(wire::Response::Projection(projection)) = &result {
            result = view
                .apply(FrontendPresentationRequest::Publish {
                    resource: local,
                    generation: 1,
                    revision: projection.revision,
                    presentation: Box::new(projection.scene.clone()),
                })
                .map(|_| wire::Response::Projection(projection.clone()));
        }
        let failed = result.is_err();
        completion.deliver_correlated(result, effects).await;
        if failed {
            break;
        } // ambiguous mutations are never silently replayed
    }
    stopped.store(true, Ordering::Release);
    let _ = view.apply(FrontendPresentationRequest::Destroy { resource: local });
    close_surface(&owner, correlated, remote).await;
}

type Execution = (WorkerResult, Option<Arc<CompletionEffectRecord>>);

async fn execute(
    owner: &ProductExtensions,
    correlated: Option<&CorrelatedFrontend>,
    request: wire::Request,
) -> Execution {
    if let Some(session) = correlated {
        session.execute(request).await
    } else {
        (invoke(owner, request).await, None)
    }
}

/// Re-deliveries of one stale plain input after its original delivery.
///
/// Only this surface's worker prepares new revisions, and it is busy re-delivering, so
/// one re-delivery normally applies the input. The bound stops a peer that keeps
/// reporting newer revisions from livelocking the worker.
const MAX_INPUT_REPLAYS: u32 = 3;

/// Bounded host-side input backlog used while the worker queue is full.
const INPUT_BACKLOG_CAPACITY: usize = 128;

/// The host's displayed lease, as observed for plain input admission.
trait InputDisplay: Sync {
    /// Whether `revision` is the displayed revision currently admitting input.
    fn admits(&self, revision: u64) -> bool;
    /// The displayed revision currently admitting input, if the surface is displayed.
    fn displayed(&self) -> Option<u64>;
}

/// One delivery of a request to the producer, exactly as the worker would dispatch it.
trait InputExecutor: Send {
    fn execute(&mut self, request: wire::Request) -> impl Future<Output = Execution> + Send;
}

struct WorkerExecutor<'a> {
    owner: &'a ProductExtensions,
    correlated: Option<&'a CorrelatedFrontend>,
}
impl InputExecutor for WorkerExecutor<'_> {
    fn execute(&mut self, request: wire::Request) -> impl Future<Output = Execution> + Send {
        execute(self.owner, self.correlated, request)
    }
}

struct ViewDisplay<'a> {
    view: &'a RetainedFrontend,
    local: FrontendResourceId,
}
impl InputDisplay for ViewDisplay<'_> {
    fn admits(&self, revision: u64) -> bool {
        self.view
            .validate_displayed(self.local, 1, revision)
            .is_ok()
    }
    fn displayed(&self) -> Option<u64> {
        self.view.displayed_revision(self.local, 1)
    }
}

/// Deliver one plain key/pointer input exactly once, in worker order.
///
/// The input is tagged with the revision the host displayed. When the host has since
/// displayed a newer revision, or the producer reports [`wire::Response::Stale`], the
/// same input is re-delivered against the current revision instead of being dropped.
/// A stale response is a definitive "not applied", so re-delivery cannot duplicate an
/// effect; ambiguous failures are still never replayed. Hidden or display-suspended
/// surfaces never receive input. When the bound is exhausted, the final `Stale`
/// response is returned so the proxy can surface a typed failure.
async fn deliver_input(
    display: &impl InputDisplay,
    request: wire::Request,
    executor: &mut impl InputExecutor,
) -> Execution {
    let wire::Request::Input {
        resource,
        mut expected_revision,
        event,
        bindings,
        focused,
    } = request
    else {
        return (
            Err("frontend input delivery requires input".to_owned()),
            None,
        );
    };
    if !display.admits(expected_revision) {
        match display.displayed() {
            // The host displayed a newer revision after tagging this input.
            Some(displayed) if displayed > expected_revision => expected_revision = displayed,
            _ => {
                return (
                    Ok(wire::Response::Event(PluginTuiEventResult::consumed())),
                    None,
                );
            }
        }
    }
    let mut replays = 0;
    loop {
        let (result, effects) = executor
            .execute(wire::Request::Input {
                resource,
                expected_revision,
                event: event.clone(),
                bindings: bindings.clone(),
                focused,
            })
            .await;
        let Ok(wire::Response::Stale { revision }) = result else {
            return (result, effects);
        };
        if revision <= expected_revision {
            return (
                Err("frontend stale input did not name a newer revision".to_owned()),
                effects,
            );
        }
        if let Err(error) = settle_unapplied(effects) {
            return (Err(error.to_owned()), None);
        }
        if replays == MAX_INPUT_REPLAYS {
            return (Ok(wire::Response::Stale { revision }), None);
        }
        if display.displayed().is_none() {
            // Hidden while re-delivering: hidden surfaces never receive input.
            return (
                Ok(wire::Response::Event(PluginTuiEventResult::consumed())),
                None,
            );
        }
        replays += 1;
        expected_revision = revision;
    }
}

/// Retire a correlated response whose input was definitively not applied.
fn settle_unapplied(effects: Option<Arc<CompletionEffectRecord>>) -> Result<(), &'static str> {
    let Some(record) = effects else {
        return Ok(());
    };
    let mut token = record.take_handoff()?;
    if let Err(error) = token
        .retained()
        .and_then(|()| token.dispatched())
        .and_then(|()| token.terminal_applied())
    {
        token.recovery_required();
        return Err(error);
    }
    monitor_completion(record);
    Ok(())
}

async fn close_surface(
    owner: &ProductExtensions,
    correlated: Option<Arc<CorrelatedFrontend>>,
    remote: uuid::Uuid,
) {
    if let Some(session) = correlated {
        let (result, record) = session
            .execute(wire::Request::Close { resource: remote })
            .await;
        if let Some(record) = record
            && matches!(result, Ok(wire::Response::Applied))
            && let Ok(mut token) = record.take_handoff()
        {
            if token
                .retained()
                .and_then(|()| token.dispatched())
                .and_then(|()| token.terminal_applied())
                .is_err()
            {
                token.recovery_required();
            }
            monitor_completion(record);
        }
    } else {
        let _ = invoke(owner, wire::Request::Close { resource: remote }).await;
    }
}

impl SurfaceProxy {
    fn bind_product_host(&self, host: &dyn PluginTuiHost) {
        let (Some(binding), Some(actual)) = (&self.binding, host.retained_product_host()) else {
            return;
        };
        let available = {
            let guard = binding.lock();
            let available = guard
                .as_ref()
                .map(|services| services.is_some())
                .map_err(|_| ());
            drop(guard);
            available
        };
        match available {
            Ok(true) => return,
            Err(()) => {
                self.stopped.store(true, Ordering::Release);
                return;
            }
            Ok(false) => {}
        }
        let services = self
            .owner
            .activation_scope()
            .map_err(|_| "frontend activation unavailable".to_owned())
            .and_then(|scope| super::host::services(actual, scope));
        let installed = services.and_then(|services| {
            let mut slot = binding
                .lock()
                .map_err(|_| "frontend binding unavailable".to_owned())?;
            if slot.is_none() {
                *slot = Some(services);
            }
            drop(slot);
            Ok(())
        });
        if installed.is_err() {
            self.stopped.store(true, Ordering::Release);
        }
    }

    fn collect(&mut self) {
        self.flush_input_backlog();
        for _ in 0..8 {
            if self.pending.len() >= 128
                || self.edit_result.is_some()
                || self.completion.is_some()
                || self.projection_completion.is_some()
            {
                break;
            }
            let Ok(Completion {
                result,
                received,
                effects,
            }) = self.completions.try_recv()
            else {
                break;
            };
            let Ok(token) = effects
                .as_ref()
                .map(|record| record.take_handoff())
                .transpose()
            else {
                self.view.display_failed();
                self.stopped.store(true, Ordering::Release);
                break;
            };
            match result {
                Ok(wire::Response::Projection(projection)) => {
                    self.revision = Some(projection.revision);
                    if let Some(token) = token {
                        self.projection_completion =
                            Some((projection.revision, projection.action.clone(), token));
                    } else {
                        self.queue_action(projection.action.clone());
                    }
                    self.projection = Some(*projection);
                }
                Ok(wire::Response::Event(result)) => self.queue_completion(result.action, token),
                Ok(wire::Response::Edit(result)) => {
                    if self.edit_pending {
                        self.edit_remote_pending =
                            result.edit_result == FocusedTextFieldEditResult::Pending;
                        self.edit_result = Some(result);
                        if let Some(token) = token {
                            self.edit_completions.push_back(token);
                        }
                    } else {
                        self.view.display_failed();
                        self.stopped.store(true, Ordering::Release);
                    }
                }
                Ok(wire::Response::EditStatus(result)) => {
                    self.edit_poll_pending = false;
                    if self.edit_pending {
                        self.edit_result = result;
                        if let Some(token) = token {
                            self.edit_completions.push_back(token);
                        }
                    }
                }
                Ok(wire::Response::Action(action)) => self.queue_completion(action, token),
                Ok(wire::Response::Stale { revision }) => self.fail_input(
                    wire::InputDeliveryFailure::FrontendInputReplayExhausted {
                        attempts: MAX_INPUT_REPLAYS,
                        revision,
                    },
                    token,
                ),
                Ok(_) => self.queue_completion(PluginTuiAction::None, token),
                Err(_) => {
                    self.view.display_failed();
                    self.stopped.store(true, Ordering::Release);
                }
            }
            let _ = received.send(());
        }
    }
    fn queue_action(&mut self, action: PluginTuiAction) {
        self.queue_completion(action, None);
    }
    /// Surface lost input as a typed closing outcome, after every earlier action.
    /// Later input would apply out of order, so the surface stops accepting input.
    fn fail_input(
        &mut self,
        failure: wire::InputDeliveryFailure,
        token: Option<FrontendEffectResponsibility>,
    ) {
        self.input_backlog.clear();
        self.queue_completion(
            PluginTuiAction::Close {
                outcome: serde_json::to_value(failure).ok(),
            },
            token,
        );
        self.stopped.store(true, Ordering::Release);
    }
    /// Queue plain input behind any backlog, so a full worker queue never drops or
    /// reorders it. Backlogged input becomes stale and is re-delivered by the worker.
    fn enqueue_input(&mut self, request: wire::Request) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        self.flush_input_backlog();
        let request = if self.input_backlog.is_empty() {
            let sent = self.send.try_send(Work {
                request,
                completion: None,
            });
            match sent {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(work)) => work.request,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.view.display_failed();
                    self.stopped.store(true, Ordering::Release);
                    return;
                }
            }
        } else {
            request
        };
        if self.input_backlog.len() >= INPUT_BACKLOG_CAPACITY {
            self.fail_input(
                wire::InputDeliveryFailure::FrontendInputBacklogExceeded {
                    capacity: INPUT_BACKLOG_CAPACITY,
                },
                None,
            );
            return;
        }
        self.input_backlog.push_back(request);
    }
    fn flush_input_backlog(&mut self) {
        while let Some(request) = self.input_backlog.pop_front() {
            if self.stopped.load(Ordering::Acquire) {
                self.input_backlog.clear();
                return;
            }
            let sent = self.send.try_send(Work {
                request,
                completion: None,
            });
            match sent {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(work)) => {
                    self.input_backlog.push_front(work.request);
                    return;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.input_backlog.clear();
                    self.view.display_failed();
                    self.stopped.store(true, Ordering::Release);
                    return;
                }
            }
        }
    }
    fn queue_completion(
        &mut self,
        action: PluginTuiAction,
        token: Option<FrontendEffectResponsibility>,
    ) {
        if token.is_none()
            && (matches!(action, PluginTuiAction::None)
                || (matches!(action, PluginTuiAction::Redraw)
                    && self.pending.iter().any(|(queued, token)| {
                        token.is_none() && matches!(queued, PluginTuiAction::Redraw)
                    })))
        {
            return;
        }
        self.pending.push_back((action, token));
    }
    fn next_action(&mut self) -> Option<PluginTuiAction> {
        if self
            .projection_completion
            .as_ref()
            .is_some_and(|(revision, _, _)| {
                self.view
                    .validate_displayed(self.local, 1, *revision)
                    .is_ok()
            })
            && let Some((_, action, token)) = self.projection_completion.take()
        {
            self.queue_completion(action, Some(token));
        }
        if self.completion.is_some() {
            return None;
        }
        let (action, token) = self.pending.pop_front()?;
        self.completion = token;
        Some(action)
    }
    fn enqueue(&self, request: wire::Request) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let sent = self.send.try_send(Work {
            request,
            completion: None,
        });
        match sent {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => false,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.view.display_failed();
                self.stopped.store(true, Ordering::Release);
                false
            }
        }
    }
    fn prepare(
        &mut self,
        destination: Rect,
        logical_row_offset: u16,
        theme: Option<&PluginTuiTheme>,
    ) {
        self.destination = destination;
        let viewport = wire::Viewport {
            width: destination.width,
            height: destination.height,
            logical_row_offset,
            theme: theme.copied(),
        };
        if self.viewport.as_ref() != Some(&viewport) && !destination.is_empty() {
            self.viewport = Some(viewport.clone());
            self.enqueue(wire::Request::Prepare {
                resource: self.remote,
                viewport: Box::new(viewport),
                bindings: self.bindings.clone(),
            });
        }
    }
    fn input(
        &mut self,
        event: &Event,
        host: &dyn PluginTuiHost,
        focused: bool,
    ) -> PluginTuiEventResult {
        self.collect();
        self.bind_product_host(host);
        // Tag input with the revision the user actually saw, not a newer collected
        // projection the frame loop has not displayed yet. Hidden surfaces get no input.
        let Some(revision) = self.view.displayed_revision(self.local, 1) else {
            return PluginTuiEventResult::consumed();
        };
        let bindings = capture_bindings(host, &self.actions, Some(event));
        let mut event = event.clone();
        if let Event::Mouse(mouse) = &mut event {
            if !self.destination.contains(mouse.position) {
                return PluginTuiEventResult::consumed();
            }
            mouse.position.x = mouse.position.x.saturating_sub(self.destination.x);
            mouse.position.y = mouse.position.y.saturating_sub(self.destination.y);
        }
        self.enqueue_input(wire::Request::Input {
            resource: self.remote,
            expected_revision: revision,
            event,
            bindings,
            focused,
        });
        PluginTuiEventResult {
            outcome: EventOutcome::Handled,
            action: PluginTuiAction::None,
        }
    }
}
impl PluginTuiSurface for SurfaceProxy {
    fn take_completion_responsibility(
        &mut self,
    ) -> Result<Option<Box<dyn FrontendCompletionResponsibility>>, CompletionResponsibilityError>
    {
        Ok(self.completion.take().map(|token| token.inner))
    }
    fn take_focused_edit_completion_responsibility(
        &mut self,
    ) -> Result<Option<Box<dyn FrontendCompletionResponsibility>>, CompletionResponsibilityError>
    {
        Ok(self.edit_completions.pop_front().map(|token| token.inner))
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn preferred_height(&mut self, _: u16) -> u16 {
        self.collect();
        self.projection
            .as_ref()
            .map_or(1, |projection| projection.logical_height)
    }
    fn focused_row_range(&mut self, _: u16) -> Option<std::ops::Range<u16>> {
        self.collect();
        self.projection
            .as_ref()
            .and_then(|projection| projection.focused_rows.clone())
    }
    fn prepare_keymap(&mut self, host: &dyn PluginTuiHost) {
        self.bind_product_host(host);
        self.bindings = capture_bindings(host, &self.actions, None);
    }
    fn render(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        self.render_slice(0, 0, area, cx);
    }
    fn render_slice(&mut self, _: u16, offset: u16, area: Rect, cx: &mut PaintCx<'_, '_>) {
        self.prepare(area, offset, None);
        if let Some(revision) = self.revision
            && self.projection.as_ref().is_some_and(|projection| {
                projection.scene.cells.area().width == area.width
                    && projection.scene.cells.area().height == area.height
            })
        {
            let _ = self.view.paint((self.local, 1, revision), area, 0, cx);
        }
    }
    fn render_with_theme(
        &mut self,
        area: Rect,
        cx: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        self.prepare(area, 0, theme);
        if let Some(revision) = self.revision {
            let _ = self.view.paint((self.local, 1, revision), area, 0, cx);
        }
    }
    fn render_slice_with_host(
        &mut self,
        logical_height: u16,
        logical_row_offset: u16,
        destination: Rect,
        frame: &mut PaintCx<'_, '_>,
        host: &dyn PluginTuiHost,
    ) {
        self.bindings = capture_bindings(host, &self.actions, None);
        self.render_slice(logical_height, logical_row_offset, destination, frame);
    }
    fn render_with_host(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
        host: &dyn PluginTuiHost,
    ) {
        self.bindings = capture_bindings(host, &self.actions, None);
        self.render_with_theme(area, frame, theme);
    }
    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        self.input(event, host, false).action
    }
    fn handle_event_result(
        &mut self,
        event: &Event,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiEventResult {
        self.input(event, host, false)
    }
    fn focused_text_field_event(
        &mut self,
        event: &Event,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiEventResult {
        self.input(event, host, true)
    }
    fn focused_text_field(&self) -> Option<FocusedTextFieldSnapshot> {
        self.view
            .validate_displayed(self.local, 1, self.revision?)
            .ok()?;
        let mut field = self.projection.as_ref()?.field.clone()?;
        field.content_rect.x = field.content_rect.x.saturating_add(self.destination.x);
        field.content_rect.y = field.content_rect.y.saturating_add(self.destination.y);
        field.anchor.x = field.anchor.x.saturating_add(self.destination.x);
        field.anchor.y = field.anchor.y.saturating_add(self.destination.y);
        Some(field)
    }
    fn apply_focused_text_field_edit_result(
        &mut self,
        edit: &FocusedTextFieldEdit,
        host: &dyn PluginTuiHost,
    ) -> PluginTuiFocusedTextFieldEditResult {
        self.collect();
        // Backlogged keys will change the field the edit's snapshot describes.
        if self.edit_pending || !self.input_backlog.is_empty() {
            return PluginTuiFocusedTextFieldEditResult::rejected(
                bcode_plugin_sdk::composer::ComposerValidationError::Stale,
            );
        }
        self.bind_product_host(host);
        let mut edit = edit.clone();
        edit.expected.content_rect.x = edit
            .expected
            .content_rect
            .x
            .saturating_sub(self.destination.x);
        edit.expected.content_rect.y = edit
            .expected
            .content_rect
            .y
            .saturating_sub(self.destination.y);
        edit.expected.anchor.x = edit.expected.anchor.x.saturating_sub(self.destination.x);
        edit.expected.anchor.y = edit.expected.anchor.y.saturating_sub(self.destination.y);
        if let Some(revision) = self.revision
            && self
                .view
                .validate_displayed(self.local, 1, revision)
                .is_ok()
            && self.enqueue(wire::Request::Edit {
                resource: self.remote,
                expected_revision: revision,
                edit: Box::new(edit),
            })
        {
            self.edit_pending = true;
            return PluginTuiFocusedTextFieldEditResult {
                event: PluginTuiEventResult::consumed(),
                edit_result: FocusedTextFieldEditResult::Pending,
            };
        }
        PluginTuiFocusedTextFieldEditResult::rejected(
            bcode_plugin_sdk::composer::ComposerValidationError::Stale,
        )
    }
    fn take_focused_text_field_edit_result(
        &mut self,
    ) -> Option<PluginTuiFocusedTextFieldEditResult> {
        self.collect();
        if self.pending.len() >= 128 {
            return None;
        }
        if self.edit_pending
            && self.edit_remote_pending
            && self.edit_result.is_none()
            && !self.edit_poll_pending
            && !self.stopped.load(Ordering::Acquire)
        {
            self.edit_poll_pending = self.enqueue(wire::Request::EditStatus {
                resource: self.remote,
            });
        }
        let result = self
            .edit_result
            .take()
            .map(|mut result| {
                if result.edit_result == FocusedTextFieldEditResult::Pending {
                    return result;
                }
                self.queue_action(result.event.action.clone());
                result.event.action = PluginTuiAction::None;
                result
            })
            .or_else(|| {
                (self.edit_pending && self.stopped.load(Ordering::Acquire)).then(|| {
                    PluginTuiFocusedTextFieldEditResult {
                        event: PluginTuiEventResult::consumed(),
                        edit_result: FocusedTextFieldEditResult::Unknown,
                    }
                })
            });
        if result.as_ref().is_some_and(|result| {
            result.edit_result != FocusedTextFieldEditResult::Pending
                && result.edit_result != FocusedTextFieldEditResult::Unknown
        }) {
            self.edit_pending = false;
            self.edit_remote_pending = false;
        }
        result
    }
    fn session_navigation_finished(
        &mut self,
        session_id: bcode_session_models::SessionId,
        result: Result<(), String>,
    ) {
        self.enqueue(wire::Request::NavigationFinished {
            resource: self.remote,
            session_id,
            result,
        });
    }
    fn attach_updates(&mut self, updates: PluginTuiSurfaceUpdateReceiver) {
        self.updates = Some(updates);
    }
    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        self.bind_product_host(host);
        for _ in 0..32 {
            let Some(update) = self
                .updates
                .as_mut()
                .and_then(|updates| updates.try_recv().ok())
            else {
                break;
            };
            if !self.enqueue(wire::Request::Update {
                resource: self.remote,
                update,
            }) {
                break;
            }
        }
        self.collect();
        if let Some(action) = self.next_action() {
            return action;
        }
        if self.stopped.load(Ordering::Acquire) {
            return PluginTuiAction::Close {
                outcome: Some(serde_json::json!({"status":"frontend_unavailable"})),
            };
        }
        if self.projection.is_some() {
            host.request_redraw();
        }
        self.next_action().unwrap_or(PluginTuiAction::None)
    }
    fn drain_effects<'a>(
        &'a mut self,
        host: &'a dyn PluginTuiHost,
    ) -> Pin<Box<dyn std::future::Future<Output = PluginTuiAction> + Send + 'a>> {
        self.bind_product_host(host);
        Box::pin(async move {
            self.collect();
            if let Some(action) = self.next_action() {
                return action;
            }
            if self.stopped.load(Ordering::Acquire) {
                return PluginTuiAction::Close {
                    outcome: Some(serde_json::json!({"status":"frontend_unavailable"})),
                };
            }
            let (send, receive) = oneshot::channel();
            let work = Work {
                request: wire::Request::Effects {
                    resource: self.remote,
                },
                completion: Some(send),
            };
            match self.send.try_send(work) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => return PluginTuiAction::None,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.view.display_failed();
                    self.stopped.store(true, Ordering::Release);
                    return PluginTuiAction::Close {
                        outcome: Some(serde_json::json!({"status":"frontend_unavailable"})),
                    };
                }
            }
            let mut receive = receive;
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(10));
            let completion = loop {
                tokio::select! {
                    completion = &mut receive => break completion,
                    _ = interval.tick() => self.collect(),
                }
            };
            if let Ok(Completion {
                result: Ok(wire::Response::Action(action)),
                received,
                effects,
            }) = completion
            {
                let Ok(token) = effects.map(|record| record.take_handoff()).transpose() else {
                    self.stopped.store(true, Ordering::Release);
                    return PluginTuiAction::None;
                };
                self.completion = token;
                let _ = received.send(());
                if matches!(action, PluginTuiAction::Close { .. }) {
                    // Closing re-renders the host, which satisfies any redraw an
                    // earlier input requested while this drain was in flight.
                    self.pending.retain(|(queued, token)| {
                        token.is_some() || !matches!(queued, PluginTuiAction::Redraw)
                    });
                }
                action
            } else {
                self.view.display_failed();
                self.stopped.store(true, Ordering::Release);
                PluginTuiAction::Close {
                    outcome: Some(serde_json::json!({"status":"frontend_unavailable"})),
                }
            }
        })
    }
}
impl Drop for SurfaceProxy {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn capture_bindings(
    host: &dyn PluginTuiHost,
    actions: &[keymap::KeymapAction],
    event: Option<&Event>,
) -> wire::Bindings {
    let mut bindings = wire::Bindings::default();
    for action in actions {
        bindings.labels.push((
            action.scope.clone(),
            action.id.clone(),
            host.keymap_keys(&action.scope, &action.id),
        ));
        if let Some(Event::Key(stroke)) = event {
            bindings.actions.push(wire::Binding {
                scope: action.scope.clone(),
                stroke: *stroke,
                action: host.keymap_action(&action.scope, *stroke),
                overridden: host.keymap_is_overridden(&action.scope, *stroke),
            });
        }
    }
    if let Some(Event::Key(stroke)) = event {
        bindings.edit = host.text_edit_command(*stroke);
        bindings.selection = host.text_selection_motion(*stroke);
        bindings.submits = host.text_submit(*stroke);
    }
    bindings
}

enum CommandWork {
    Invoke(String),
}
type CommandTextResult = Result<bcode_plugin_sdk::tui::PluginFrontendTextCompletion, String>;

fn command_text_completions(
    result: WorkerResult,
    record: Option<Arc<CompletionEffectRecord>>,
) -> Vec<CommandTextResult> {
    let mut values = match result {
        Ok(wire::Response::Text(text)) => text,
        Ok(wire::Response::Applied) => Vec::new(),
        Ok(_) => {
            return vec![Err(
                "frontend command response mismatch; original retained".to_owned()
            )];
        }
        Err(error) => return vec![Err(error)],
    };
    let Some(record) = record else {
        return values
            .into_iter()
            .map(|text| {
                Ok(bcode_plugin_sdk::tui::PluginFrontendTextCompletion {
                    text,
                    responsibility: None,
                })
            })
            .collect();
    };
    if values.len() > 32 {
        return vec![Err(
            "frontend text exceeds reserved slots; original retained".to_owned(),
        )];
    }
    if values.is_empty() {
        values.push(String::new());
    }
    let mut tokens = Vec::with_capacity(values.len());
    for ordinal in 0..values.len() {
        let token = if ordinal == 0 {
            record.take_handoff()
        } else {
            record.reserve(ordinal)
        };
        match token {
            Ok(token) => tokens.push(token),
            Err(error) => return vec![Err(error.to_owned())],
        }
    }
    monitor_completion(record);
    values
        .into_iter()
        .zip(tokens)
        .map(|(text, token)| {
            Ok(bcode_plugin_sdk::tui::PluginFrontendTextCompletion {
                text,
                responsibility: Some(token.inner),
            })
        })
        .collect()
}

struct Commands {
    pending: Arc<Mutex<VecDeque<CommandTextResult>>>,
    jobs: mpsc::Sender<CommandWork>,
    closed: Arc<AtomicBool>,
}
impl Commands {
    fn new(
        owner: ProductExtensions,
        runtime: &tokio::runtime::Handle,
        correlated: Option<Arc<CorrelatedFrontend>>,
    ) -> Self {
        let (jobs, mut receive) = mpsc::channel::<CommandWork>(32);
        let pending = Arc::new(Mutex::new(VecDeque::new()));
        let completed = Arc::clone(&pending);
        let closed = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&closed);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let available = completed.lock().is_ok_and(|pending| pending.is_empty());
                if !available {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    continue;
                }
                let request = tokio::select! {
                    work = receive.recv() => match work {
                        Some(CommandWork::Invoke(id)) => wire::Request::Command { id },
                        None => break,
                    },
                    _ = interval.tick() => wire::Request::DrainText,
                };
                let (result, record) = if let Some(session) = &correlated {
                    session.execute(request).await
                } else {
                    (invoke(&owner, request).await, None)
                };
                let output = command_text_completions(result, record);
                let failed = output.iter().any(Result::is_err);
                if let Ok(mut pending) = completed.lock() {
                    if pending.len().saturating_add(output.len()) > 128 {
                        pending.extend(output);
                        pending.push_back(Err(
                            "frontend text queue exceeded budget; original outcomes retained"
                                .to_owned(),
                        ));
                        stop.store(true, Ordering::Release);
                    } else {
                        pending.extend(output);
                    }
                } else {
                    stop.store(true, Ordering::Release);
                }
                if failed {
                    stop.store(true, Ordering::Release);
                }
            }
        });
        Self {
            pending,
            jobs,
            closed,
        }
    }
}
struct Command {
    id: String,
    commands: Arc<Commands>,
}
impl PluginFrontendCommand for Command {
    fn invoke(&self) -> Result<(), String> {
        if self.commands.closed.load(Ordering::Acquire) {
            return Err("frontend commands closed".to_owned());
        }
        self.commands
            .jobs
            .try_send(CommandWork::Invoke(self.id.clone()))
            .map_err(|_| "frontend command queue unavailable".to_owned())
    }
    fn drain_text(&self) -> Result<Option<String>, String> {
        self.commands
            .pending
            .lock()
            .map_err(|_| "frontend text unavailable".to_owned())?
            .pop_front()
            .transpose()
            .and_then(|completion| {
                completion
                    .map(|completion| {
                        if completion.responsibility.is_some() {
                            Err("correlated text requires actual completion consumer".to_owned())
                        } else {
                            Ok(completion.text)
                        }
                    })
                    .transpose()
            })
    }
    fn drain_text_completion(
        &self,
    ) -> Result<Option<bcode_plugin_sdk::tui::PluginFrontendTextCompletion>, String> {
        self.commands
            .pending
            .lock()
            .map_err(|_| "frontend text unavailable".to_owned())?
            .pop_front()
            .transpose()
    }
    fn shutdown(&self) {
        self.commands.closed.store(true, Ordering::Release);
    }
}

/// Real selected-owner fixture used by consumer ownership tests; never creates synthetic scope.
#[cfg(test)]
pub(crate) fn completion_test_responsibility() -> FrontendEffectResponsibility {
    CompletionEffectRecord::new(
        wire::settlement::ResponseId {
            session_id: uuid::Uuid::new_v4(),
            sequence: 1,
        },
        super::tests::completion_test_owner(),
        Arc::new(tokio::sync::Semaphore::new(32)),
    )
    .unwrap()
    .reserve(0)
    .unwrap()
}

#[cfg(test)]
mod retention_tests {
    use super::{CompletionEffectRecord, FrontendEffectDisposition as D, FrontendEffectTransfer};
    use crate::effects::{EffectSchedule, TuiEffect, TuiEffectQueue};

    fn effect(command: &str) -> TuiEffect {
        TuiEffect::InvokePluginCommand {
            plugin_id: "test.dynamic-visual".to_owned(),
            command_id: command.to_owned(),
            arguments: None,
            working_directory: std::env::temp_dir(),
            session_id: None,
        }
    }

    fn record(sequence: u64) -> Arc<CompletionEffectRecord> {
        CompletionEffectRecord::new(
            super::wire::settlement::ResponseId {
                session_id: uuid::Uuid::new_v4(),
                sequence,
            },
            super::super::tests::completion_test_owner(),
            Arc::new(tokio::sync::Semaphore::new(32)),
        )
        .unwrap()
    }

    fn disposition(record: &CompletionEffectRecord, ordinal: usize) -> D {
        record.slots.lock().unwrap()[ordinal].unwrap()
    }

    #[tokio::test]
    async fn selected_optional_admission_reserves_original_record_and_never_falls_back_after_rejection()
     {
        let session = super::CorrelatedFrontend {
            adapter: super::DynamicFrontendAdapter::new(tokio::runtime::Handle::current()),
            owner: super::super::tests::completion_test_owner(),
            session: uuid::Uuid::new_v4(),
            sequence: tokio::sync::Mutex::new(0),
            unresolved: AtomicBool::new(false),
        };
        let (result, original) = session.execute(wire::Request::Describe).await;
        // This actual selected fixture lacks the optional dispatcher. Its rejection must
        // retain the admitted record; ordinary frontend Describe is not a fallback.
        assert!(result.is_err());
        let original = original.unwrap();
        assert_eq!(original.id.session_id, session.session);
        assert_eq!(original.id.sequence, 1);
        assert_eq!(disposition(&original, 0), D::Reserved);
        assert!(original.registration.get().is_some());
        assert!(session.unresolved.load(Ordering::Acquire));
        let (retry, second) = session.execute(wire::Request::Describe).await;
        assert!(retry.is_err());
        assert!(second.is_none());
        assert_eq!(*session.sequence.lock().await, 1);
        assert!(!original.commit_started.load(Ordering::Acquire));
        drop(original);
    }

    #[tokio::test]
    async fn boxed_surface_moves_original_record_capability_without_commit_on_delivery() {
        use super::{FrontendCompletionResponsibility, FrontendEffectResponsibility};
        use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost, PluginTuiSurface};
        struct Forwarder(Option<Box<dyn FrontendCompletionResponsibility>>);
        impl PluginTuiSurface for Forwarder {
            fn id(&self) -> &'static str {
                "retained-completion"
            }
            fn title(&self) -> &'static str {
                "Retained completion"
            }
            fn render(
                &mut self,
                _: bmux_tui::geometry::Rect,
                _: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }
            fn handle_event(
                &mut self,
                _: &bmux_tui::event::Event,
                _: &dyn PluginTuiHost,
            ) -> PluginTuiAction {
                PluginTuiAction::None
            }
            fn take_completion_responsibility(
                &mut self,
            ) -> Result<
                Option<Box<dyn FrontendCompletionResponsibility>>,
                super::CompletionResponsibilityError,
            > {
                Ok(self.0.take())
            }
        }
        let record = record(1);
        let original = record.reserve(0).unwrap();
        let mut surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface =
            Box::new(Forwarder(Some(original.inner)));
        let opaque = surface.require_completion_responsibility().unwrap();
        assert_eq!(opaque.response_id(), record.id);
        assert_eq!(opaque.ordinal(), 0);
        assert_eq!(opaque.disposition(), D::Reserved);
        assert!(matches!(
            surface.require_completion_responsibility(),
            Err(super::CompletionResponsibilityError::Missing)
        ));
        assert!(!record.commit_started.load(Ordering::Acquire));
        drop(surface);
        let mut token = FrontendEffectResponsibility::from_opaque(opaque);
        assert!(token.commit_transferred().await.is_err());
        assert!(!record.commit_started.load(Ordering::Acquire));
        token.retained().unwrap();
        let follow_up = token.follow_up().unwrap();
        assert_eq!(follow_up.response_id(), record.id);
        assert_ne!(follow_up.ordinal(), token.ordinal());
        token.dispatched().unwrap();
        token.terminal_applied().unwrap();
        let follow_up_ordinal = follow_up.ordinal();
        drop(follow_up);
        assert_eq!(disposition(&record, follow_up_ordinal), D::RecoveryRequired);
        assert!(!record.commit_started.load(Ordering::Acquire));
        assert!(record.registration.get().is_some());
        assert!(!record.retired.load(Ordering::Acquire));
        drop(record);
    }

    #[tokio::test]
    async fn unverified_commit_reply_never_releases_registered_completion() {
        let record = record(1);
        let token = record.reserve(0).unwrap();
        assert!(
            record.commit_transferred().await.is_err(),
            "Reserved is not transferred"
        );
        assert!(!record.commit_started.load(Ordering::Acquire));
        let mut token = token;
        token.retained().unwrap();
        // Actual selected fixture has no optional settlement dispatcher. Its failure must
        // retain the activation resource, never invent a committed reply or fall back to v2.
        assert!(record.commit_transferred().await.is_err());
        assert!(record.commit_started.load(Ordering::Acquire));
        assert!(!record.commit_confirmed.load(Ordering::Acquire));
        assert!(record.settle_registered().await.is_err());
        assert!(!record.retired.load(Ordering::Acquire));
        assert!(record.registration.get().is_some());
        assert!(
            record.commit_transferred().await.is_err(),
            "uncertain commit cannot be replayed"
        );
        drop(token);
        assert_eq!(disposition(&record, 0), D::RecoveryRequired);
        drop(record);
    }

    #[test]
    fn activation_resource_retains_original_record_after_all_ui_references_drop() {
        let owner = super::super::tests::completion_test_owner();
        let scope = owner.activation_scope().unwrap();
        let completion = CompletionEffectRecord::new(
            wire::settlement::ResponseId {
                session_id: uuid::Uuid::new_v4(),
                sequence: 1,
            },
            owner,
            Arc::new(tokio::sync::Semaphore::new(32)),
        )
        .unwrap();
        let registration = *completion.registration.get().unwrap();
        let weak = Arc::downgrade(&completion);
        let token = completion.reserve(0).unwrap();
        drop(token);
        drop(completion);
        let retained = weak
            .upgrade()
            .expect("actual activation tree must retain the original record");
        assert_eq!(disposition(&retained, 0), D::RecoveryRequired);
        assert_eq!(retained.budget.available_permits(), 31);
        assert_eq!(retained.registration.get(), Some(&registration));
        // This test observes runtime resource ownership only, not verified product settlement.
        // Do not release an unresolved record merely to make fixture teardown look clean.
        drop(scope);
        drop(retained);
        assert!(weak.upgrade().is_some());
    }

    #[tokio::test]
    async fn response_record_admission_is_bounded_before_resource_publication() {
        let adapter = super::DynamicFrontendAdapter::new(tokio::runtime::Handle::current());
        let owner = super::super::tests::completion_test_owner();
        let session_id = uuid::Uuid::new_v4();
        let mut records = Vec::new();
        for sequence in 1..=32 {
            records.push(
                adapter
                    .completion_record(
                        wire::settlement::ResponseId {
                            session_id,
                            sequence,
                        },
                        owner.clone(),
                    )
                    .unwrap(),
            );
        }
        assert!(
            adapter
                .completion_record(
                    wire::settlement::ResponseId {
                        session_id,
                        sequence: 33,
                    },
                    owner.clone()
                )
                .is_err()
        );
        drop(records);
        assert!(
            adapter
                .completion_record(
                    wire::settlement::ResponseId {
                        session_id,
                        sequence: 33,
                    },
                    owner
                )
                .is_err(),
            "consumer removal cannot free registered unresolved records"
        );
    }

    #[tokio::test]
    async fn shared_budget_stays_charged_after_queue_and_token_removal() {
        let adapter = super::DynamicFrontendAdapter::new(tokio::runtime::Handle::current());
        let owner = super::super::tests::completion_test_owner();
        let first = adapter
            .completion_record(
                wire::settlement::ResponseId {
                    session_id: uuid::Uuid::new_v4(),
                    sequence: 1,
                },
                owner.clone(),
            )
            .unwrap();
        let second = adapter
            .completion_record(
                wire::settlement::ResponseId {
                    session_id: first.id.session_id,
                    sequence: 2,
                },
                owner,
            )
            .unwrap();
        let mut queue = TuiEffectQueue::default();
        for ordinal in 0..32 {
            queue
                .try_push_frontend(
                    EffectSchedule::Replace,
                    FrontendEffectTransfer::new(
                        effect(&ordinal.to_string()),
                        first.reserve(ordinal).unwrap(),
                    ),
                )
                .unwrap();
        }
        assert!(second.reserve(0).is_err());
        drop(queue);
        assert!(
            second.reserve(0).is_err(),
            "dropping consumers cannot reclaim unsettled capacity"
        );
        assert_eq!(first.budget.available_permits(), 0);
        assert_eq!(disposition(&first, 0), D::RecoveryRequired);
        assert!(!first.retired.load(Ordering::Acquire));
        drop(second);
        drop(first);
    }

    #[test]
    fn correlated_cancel_returns_whole_reserved_transfer_without_touching_target() {
        let record = record(1);
        let mut queue = TuiEffectQueue::default();
        queue.replace(effect("target"));
        let transfer = FrontendEffectTransfer::new(effect("target"), record.reserve(0).unwrap());
        let permits_before = record.budget.available_permits();
        let owners_before = Arc::strong_count(&record);
        let rejected = *queue
            .try_push_frontend(EffectSchedule::Cancel, transfer)
            .unwrap_err();
        assert_eq!(record.budget.available_permits(), permits_before);
        assert_eq!(Arc::strong_count(&record), owners_before);
        assert!(
            matches!(&rejected.effect, TuiEffect::InvokePluginCommand { command_id, .. }
            if command_id == "target")
        );
        assert_eq!(rejected.responsibility.response_id(), record.id);
        assert_eq!(rejected.responsibility.ordinal(), 0);
        assert_eq!(rejected.responsibility.disposition(), D::Reserved);
        assert_eq!(queue.queued_effect_count(), 1);
        let (mut entries, notes) = queue.drain_runtime();
        assert!(notes.is_empty());
        let target = entries.pop().unwrap();
        assert!(matches!(target.schedule, EffectSchedule::Replace));
        assert!(target.responsibility.is_none());
        assert!(
            matches!(&target.effect, TuiEffect::InvokePluginCommand { command_id, .. }
            if command_id == "target")
        );
        drop(rejected);
        assert_eq!(disposition(&record, 0), D::RecoveryRequired);
        assert!(!record.retired.load(Ordering::Acquire));
        drop(target);
        drop(queue);
        drop(record);
    }

    #[test]
    fn admitted_note_retains_exact_token_through_ordered_drain() {
        let record = record(1);
        let mut queue = TuiEffectQueue::default();
        let session_id = bcode_session_models::SessionId::new();
        let transfer = FrontendEffectTransfer::new(
            TuiEffect::AppendPresentationNote {
                session_id,
                source_id: "producer".to_owned(),
                note_id: "note".to_owned(),
                text: "exact text".to_owned(),
                format: bcode_command::CommandTextFormat::PlainText,
            },
            record.reserve(0).unwrap(),
        );
        queue
            .try_push_frontend(EffectSchedule::StartIfIdle, transfer)
            .unwrap();
        assert_eq!(disposition(&record, 0), D::Retained);
        assert_eq!(queue.queued_effect_count(), 1);
        let (entries, mut notes) = queue.drain_runtime();
        assert!(entries.is_empty());
        let mut ordered = notes.remove(&session_id).unwrap();
        assert!(notes.is_empty());
        assert_eq!(ordered.len(), 1);
        let entry = ordered.pop_front().unwrap();
        assert!(matches!(entry.schedule, EffectSchedule::StartIfIdle));
        assert!(matches!(&entry.effect,
            TuiEffect::AppendPresentationNote { session_id: actual, source_id, note_id, text, format }
            if *actual == session_id && source_id == "producer" && note_id == "note"
                && text == "exact text" && *format == bcode_command::CommandTextFormat::PlainText));
        let token = entry.responsibility.as_ref().unwrap();
        assert_eq!(token.response_id(), record.id);
        assert_eq!(token.ordinal(), 0);
        assert_eq!(token.disposition(), D::Retained);
        assert!(!record.commit_started.load(Ordering::Acquire));
        // Ordered drain is ownership transfer, never actual result application.
        drop(entry);
        assert_eq!(disposition(&record, 0), D::RecoveryRequired);
        assert!(!record.retired.load(Ordering::Acquire));
        drop(queue);
        drop(record);
    }

    #[test]
    fn queue_capacity_rejects_33rd_without_changing_its_token() {
        let original = record(1);
        let mut queue = TuiEffectQueue::default();
        for ordinal in 0..32 {
            queue
                .try_push_frontend(
                    EffectSchedule::Replace,
                    FrontendEffectTransfer::new(
                        effect(&ordinal.to_string()),
                        original.reserve(ordinal).unwrap(),
                    ),
                )
                .unwrap();
            assert_eq!(disposition(&original, ordinal), D::Retained);
        }
        let extra = record(2);
        let transfer = FrontendEffectTransfer::new(effect("extra"), extra.reserve(0).unwrap());
        let permits_before = extra.budget.available_permits();
        let owners_before = Arc::strong_count(&extra);
        let rejected = *queue
            .try_push_frontend(EffectSchedule::Replace, transfer)
            .unwrap_err();
        assert_eq!(extra.budget.available_permits(), permits_before);
        assert_eq!(Arc::strong_count(&extra), owners_before);
        assert_eq!(rejected.responsibility.response_id(), extra.id);
        assert_eq!(rejected.responsibility.ordinal(), 0);
        assert_eq!(rejected.responsibility.disposition(), D::Reserved);
        assert!(
            matches!(&rejected.effect, TuiEffect::InvokePluginCommand { command_id, .. } if command_id == "extra")
        );
        assert_eq!(queue.queued_effect_count(), 32);
        drop(queue);
        for ordinal in 0..32 {
            assert_eq!(disposition(&original, ordinal), D::RecoveryRequired);
        }
    }

    #[test]
    fn replacement_preserves_original_disposition_without_borrowing_new_terminal() {
        let record = record(1);
        let mut queue = TuiEffectQueue::default();
        queue
            .try_push_frontend(
                EffectSchedule::Replace,
                FrontendEffectTransfer::new(effect("same"), record.reserve(0).unwrap()),
            )
            .unwrap();
        queue
            .try_push_frontend(
                EffectSchedule::Replace,
                FrontendEffectTransfer::new(effect("same"), record.reserve(1).unwrap()),
            )
            .unwrap();
        assert_eq!(disposition(&record, 0), D::NotDispatched);
        assert_eq!(disposition(&record, 1), D::Retained);
        let (mut entries, notes) = queue.drain_runtime();
        assert!(notes.is_empty());
        let mut token = entries.pop().unwrap().responsibility.unwrap();
        assert!(token.terminal_applied().is_err());
        token.dispatched().unwrap();
        token.terminal_applied().unwrap();
        drop(token);
        assert_eq!(disposition(&record, 0), D::NotDispatched);
        assert_eq!(disposition(&record, 1), D::TerminalApplied);
        assert!(!record.retired.load(Ordering::Acquire));
        drop(queue);
        drop(record);
    }

    #[test]
    fn legacy_replacement_and_consumer_removal_do_not_fabricate_terminal() {
        let record = record(1);
        let mut queue = TuiEffectQueue::default();
        queue
            .try_push_frontend(
                EffectSchedule::Replace,
                FrontendEffectTransfer::new(effect("same"), record.reserve(0).unwrap()),
            )
            .unwrap();
        queue.replace(effect("same"));
        assert_eq!(disposition(&record, 0), D::NotDispatched);
        queue
            .try_push_frontend(
                EffectSchedule::Replace,
                FrontendEffectTransfer::new(effect("other"), record.reserve(1).unwrap()),
            )
            .unwrap();
        let (entries, _) = queue.drain_runtime();
        assert_eq!(disposition(&record, 1), D::Retained);
        drop(entries);
        assert_eq!(disposition(&record, 1), D::RecoveryRequired);
        assert!(!record.retired.load(Ordering::Acquire));
        drop(queue);
        drop(record);
    }

    use super::{Completion, CompletionSlot, PluginTuiAction, retain_action, wire};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::sync::{mpsc, oneshot};

    #[tokio::test]
    async fn completion_capacity_blocks_dispatch_until_observation() {
        let (send, mut receive) = mpsc::channel(1);
        let mut first = Some(CompletionSlot::reserve(&send, None).await.unwrap());
        let mut reserve = Box::pin(CompletionSlot::reserve(&send, None));
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(reserve.as_mut().poll(cx).is_pending())
            })
            .await
        );
        let delivery = tokio::spawn(first.take().unwrap().deliver(Ok(wire::Response::Action(
            PluginTuiAction::Close {
                outcome: Some(serde_json::json!("committed")),
            },
        ))));
        drop(first);
        let Completion {
            result,
            received,
            effects: _effects,
        } = receive.recv().await.unwrap();
        assert!(matches!(
            result,
            Ok(wire::Response::Action(PluginTuiAction::Close { .. }))
        ));
        assert!(!delivery.is_finished());
        let _ = received.send(());
        delivery.await.unwrap();
        assert!(reserve.await.is_some());
    }

    #[tokio::test]
    async fn dropped_observer_retains_dispatched_outcome_and_owner() {
        struct Owner(Arc<AtomicBool>);
        impl Drop for Owner {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let owner = Owner(Arc::clone(&released));
        let (send, receive) = mpsc::channel(1);
        let slot = CompletionSlot::reserve(&send, None).await.unwrap();
        let delivery = tokio::spawn(async move {
            slot.deliver(Ok(wire::Response::Action(PluginTuiAction::Close {
                outcome: Some(serde_json::json!("committed")),
            })))
            .await;
            drop(owner);
        });
        drop(receive);
        tokio::task::yield_now().await;
        assert!(!delivery.is_finished());
        assert!(!released.load(Ordering::Acquire));
        assert!(CompletionSlot::reserve(&send, None).await.is_none());
        // Explicit test teardown only; production does not abort retained delivery.
        delivery.abort();
        assert!(delivery.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn dropped_direct_observer_prevents_new_dispatch() {
        let (send, _receive) = mpsc::channel(1);
        let (direct, observer) = oneshot::channel();
        drop(observer);
        assert!(CompletionSlot::reserve(&send, Some(direct)).await.is_none());
    }

    #[test]
    fn action_saturation_preserves_order_and_returns_unadmitted_effect() {
        let mut pending = VecDeque::new();
        for index in 0..128 {
            retain_action(
                &mut pending,
                PluginTuiAction::Close {
                    outcome: Some(serde_json::json!(index)),
                },
            )
            .unwrap();
        }
        let overflow = retain_action(
            &mut pending,
            PluginTuiAction::Close {
                outcome: Some(serde_json::json!(128)),
            },
        )
        .unwrap_err();
        assert_eq!(pending.len(), 128);
        for index in 0..128 {
            let Some(PluginTuiAction::Close { outcome }) = pending.pop_front() else {
                panic!("retained terminal action");
            };
            assert_eq!(outcome, Some(serde_json::json!(index)));
        }
        retain_action(&mut pending, overflow).unwrap();
        retain_action(&mut pending, PluginTuiAction::Redraw).unwrap();
        retain_action(&mut pending, PluginTuiAction::Redraw).unwrap();
        assert_eq!(pending.len(), 2);
    }
}

#[cfg(test)]
mod input_replay_tests {
    use super::{
        Execution, Factory, InputDisplay, InputExecutor, MAX_INPUT_REPLAYS, RetainedFrontend,
        deliver_input, wire,
    };
    use bcode_plugin_sdk::definition::{
        ComponentRole, PluginCallContext, PluginCreateContext, PluginFuture, PluginResult,
        ProductComponentExtension, ProductExtensionDescriptor, ProductExtensionPort,
        ProductExtensions, extensions::PRODUCT_EXTENSIONS_CONTRACT,
    };
    use bcode_plugin_sdk::tui::{
        BoxedPluginTuiSurface, PluginTuiAction, PluginTuiEventResult, PluginTuiHost,
        PluginTuiRegistry, PluginTuiSurface, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture,
        PluginTuiSurfaceOpenRequest, TokioPluginTuiHost,
    };
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::hit::HitRegion;
    use bmux_tui::{
        buffer::Buffer, event::Event, frame::Frame, geometry::Rect, paint::PaintCx, style::Style,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct Display {
        displayed: Option<u64>,
    }
    impl InputDisplay for Display {
        fn admits(&self, revision: u64) -> bool {
            self.displayed == Some(revision)
        }
        fn displayed(&self) -> Option<u64> {
            self.displayed
        }
    }

    fn key(character: char) -> Event {
        Event::Key(KeyStroke::simple(KeyCode::Char(character)))
    }

    fn input(expected_revision: u64) -> wire::Request {
        wire::Request::Input {
            resource: uuid::Uuid::nil(),
            expected_revision,
            event: key('a'),
            bindings: wire::Bindings::default(),
            focused: false,
        }
    }

    fn named(request: &wire::Request) -> u64 {
        let wire::Request::Input {
            expected_revision, ..
        } = request
        else {
            panic!("only input is delivered");
        };
        *expected_revision
    }

    /// Scripted producer: reports `stale` revisions in order, then applies the input.
    struct Script {
        stale: std::collections::VecDeque<u64>,
        delivered: Vec<u64>,
    }
    impl InputExecutor for Script {
        fn execute(
            &mut self,
            request: wire::Request,
        ) -> impl std::future::Future<Output = Execution> + Send {
            self.delivered.push(named(&request));
            let response = self.stale.pop_front().map_or_else(
                || wire::Response::Event(PluginTuiEventResult::consumed()),
                |revision| wire::Response::Stale { revision },
            );
            std::future::ready((Ok(response), None))
        }
    }

    async fn deliver(displayed: Option<u64>, tagged: u64, stale: &[u64]) -> (Execution, Vec<u64>) {
        let mut script = Script {
            stale: stale.iter().copied().collect(),
            delivered: Vec::new(),
        };
        let result = deliver_input(&Display { displayed }, input(tagged), &mut script).await;
        (result, script.delivered)
    }

    #[tokio::test]
    async fn stale_input_is_redelivered_once_against_the_current_revision() {
        let ((result, _), delivered) = deliver(Some(4), 4, &[5]).await;
        assert!(matches!(result, Ok(wire::Response::Event(_))));
        assert_eq!(delivered, vec![4, 5]);
    }

    #[tokio::test]
    async fn input_tagged_before_a_newer_display_names_the_displayed_revision() {
        let ((result, _), delivered) = deliver(Some(6), 4, &[]).await;
        assert!(matches!(result, Ok(wire::Response::Event(_))));
        assert_eq!(delivered, vec![6]);
    }

    #[tokio::test]
    async fn hidden_surfaces_never_receive_input() {
        let ((result, _), delivered) = deliver(None, 4, &[]).await;
        assert!(matches!(
            result,
            Ok(wire::Response::Event(result)) if result == PluginTuiEventResult::consumed()
        ));
        assert_eq!(delivered, Vec::<u64>::new());
    }

    #[tokio::test]
    async fn replay_bound_returns_the_final_stale_outcome() {
        let stale = (5..).take(usize::try_from(MAX_INPUT_REPLAYS).unwrap() + 2);
        let stale = stale.collect::<Vec<_>>();
        let ((result, _), delivered) = deliver(Some(4), 4, &stale).await;
        let last = stale[usize::try_from(MAX_INPUT_REPLAYS).unwrap()];
        assert!(matches!(result, Ok(wire::Response::Stale { revision }) if revision == last));
        assert_eq!(
            delivered.len(),
            usize::try_from(MAX_INPUT_REPLAYS).unwrap() + 1
        );
    }

    #[tokio::test]
    async fn stale_outcome_that_does_not_advance_is_a_protocol_failure() {
        let ((result, _), delivered) = deliver(Some(4), 4, &[4]).await;
        assert!(result.is_err());
        assert_eq!(delivered, vec![4]);
    }

    /// Records every key it receives; painting a recognizable cell admits display.
    struct Recording(Arc<Mutex<Vec<char>>>);
    impl PluginTuiSurface for Recording {
        fn id(&self) -> &'static str {
            "recording"
        }
        fn title(&self) -> &'static str {
            "Recording"
        }
        fn preferred_height(&mut self, _: u16) -> u16 {
            1
        }
        fn render(&mut self, _: Rect, cx: &mut PaintCx<'_, '_>) {
            cx.set_cell(0, 0, "R", Style::new());
            cx.push_hit(HitRegion::new("recording", Rect::new(0, 0, 1, 1)));
        }
        fn handle_event(&mut self, event: &Event, _: &dyn PluginTuiHost) -> PluginTuiAction {
            if let Event::Key(KeyStroke {
                key: KeyCode::Char(character),
                ..
            }) = event
            {
                self.0.lock().unwrap().push(*character);
            }
            PluginTuiAction::Redraw
        }
    }

    /// Per-test sinks keyed by the open request's options; the factory is a plain fn.
    static RECORDED: Mutex<BTreeMap<String, Arc<Mutex<Vec<char>>>>> = Mutex::new(BTreeMap::new());

    struct RecordingFactory;
    impl PluginTuiSurfaceFactory for RecordingFactory {
        fn surface_kind(&self) -> &'static str {
            "recording"
        }
        fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
            let recorded = request
                .options
                .as_str()
                .and_then(|sink| RECORDED.lock().unwrap().get(sink).cloned())
                .expect("recording sink");
            Box::pin(async move { Ok(Box::new(Recording(recorded)) as BoxedPluginTuiSurface) })
        }
    }
    fn registry() -> PluginTuiRegistry {
        let mut registry = PluginTuiRegistry::default();
        registry.register_factory(Box::new(RecordingFactory));
        registry
    }

    /// The real author codec behind a port; `always_stale` makes it report every input
    /// stale against an ever-advancing revision, as a misbehaving producer would.
    struct Port {
        author: Arc<dyn ProductComponentExtension>,
        always_stale: AtomicBool,
    }
    impl ProductExtensionPort for Port {
        fn ensure_active(&self) -> PluginResult<()> {
            Ok(())
        }
        fn invoke(
            &self,
            contract: String,
            payload: Vec<u8>,
        ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
            Box::pin(async move {
                if contract == PRODUCT_EXTENSIONS_CONTRACT {
                    let contract =
                        |interface: &str, version| bmux_plugin_sdk::RegistrationContract {
                            kind: bmux_plugin_sdk::RegistrationKind::Extension,
                            interface: interface.to_owned(),
                            version,
                            name: interface.to_owned(),
                        };
                    return Ok(serde_json::to_vec(&ProductExtensionDescriptor {
                        plugin_id: "test.replay".to_owned(),
                        role: ComponentRole::Frontend,
                        contracts: vec![
                            contract(wire::TUI_FRONTEND_CONTRACT, wire::TUI_FRONTEND_VERSION),
                            contract(
                                bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID,
                                bmux_tui::presentation::portable::PORTABLE_PRESENTATION_VERSION,
                            ),
                        ],
                        native_frontend: false,
                    })
                    .unwrap());
                }
                assert_eq!(contract, wire::TUI_FRONTEND_CONTRACT);
                if self.always_stale.load(Ordering::Acquire)
                    && let wire::Request::Input {
                        expected_revision, ..
                    } = serde_json::from_slice(&payload).unwrap()
                {
                    return Ok(serde_json::to_vec(&wire::Response::Stale {
                        revision: expected_revision + 1,
                    })
                    .unwrap());
                }
                self.author
                    .invoke(
                        PluginCallContext {
                            callback_execution: None,
                            invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                            cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
                            host_services: None,
                        },
                        payload,
                    )
                    .await
            })
        }
    }

    struct Harness {
        surface: BoxedPluginTuiSurface,
        view: Arc<RetainedFrontend>,
        host: TokioPluginTuiHost,
        recorded: Arc<Mutex<Vec<char>>>,
        port: Arc<Port>,
        _redraw: tokio::sync::mpsc::Receiver<()>,
    }

    impl Harness {
        async fn open() -> Self {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let sink = uuid::Uuid::new_v4().to_string();
            RECORDED
                .lock()
                .unwrap()
                .insert(sink.clone(), Arc::clone(&recorded));
            let port = Arc::new(Port {
                author: bcode_plugin_sdk::tui::portable::extension(
                    registry,
                    PluginCreateContext {
                        cli_command: None,
                        runtime_scope: bmux_plugin_sdk::definition::RuntimeScopeId::from_uuid(
                            uuid::Uuid::new_v4(),
                        ),
                        instance: bmux_plugin_sdk::definition::LoadedInstanceId::from_uuid(
                            uuid::Uuid::new_v4(),
                        ),
                        role: ComponentRole::Frontend,
                    },
                ),
                always_stale: AtomicBool::new(false),
            });
            let owner =
                ProductExtensions::new(Arc::clone(&port) as Arc<dyn ProductExtensionPort>, None);
            let view = RetainedFrontend::for_component(
                owner.clone(),
                1,
                bmux_plugin::component_viewport::portable::PortablePresentationLimits::default(),
            )
            .await
            .unwrap();
            let surface = Factory {
                correlated: None,
                binding: None,
                kind: "recording".to_owned(),
                owner,
                runtime: tokio::runtime::Handle::current(),
                view: Arc::clone(&view),
                actions: Vec::new(),
            }
            .open(PluginTuiSurfaceOpenRequest {
                instance_id: "replay".to_owned(),
                repo_path: None,
                session_id: None,
                target: None,
                options: serde_json::Value::String(sink),
            })
            .await
            .unwrap();
            let (redraw, receiver) = tokio::sync::mpsc::channel(1);
            Self {
                surface,
                view,
                host: TokioPluginTuiHost::current(redraw),
                recorded,
                port,
                _redraw: receiver,
            }
        }

        /// Render until the producer's projection for `width` is painted, then commit it.
        async fn display(&mut self, width: u16) {
            let area = Rect::new(0, 0, width, 1);
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    self.view.begin_display();
                    let mut buffer = Buffer::empty(area);
                    let mut frame = Frame::new(&mut buffer);
                    self.surface.render_slice_with_host(
                        1,
                        0,
                        area,
                        &mut PaintCx::new(&mut frame),
                        &self.host,
                    );
                    if !frame.hits().regions().is_empty() {
                        self.view
                            .display_succeeded(frame.hits(), frame.selection())
                            .unwrap();
                        break;
                    }
                    drop(frame);
                    self.surface.poll(&self.host);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("producer projection must be displayed");
        }

        /// Make the producer prepare a newer revision that is collected but not displayed.
        fn supersede(&mut self, width: u16) {
            let area = Rect::new(0, 0, width, 1);
            let mut buffer = Buffer::empty(area);
            let mut frame = Frame::new(&mut buffer);
            self.surface.render_slice_with_host(
                1,
                0,
                area,
                &mut PaintCx::new(&mut frame),
                &self.host,
            );
        }

        /// Wait until every earlier worker request has completed and been collected.
        async fn settle(&mut self) {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    match self.surface.drain_effects(&self.host).await {
                        PluginTuiAction::None => break,
                        PluginTuiAction::Redraw => {}
                        action => panic!("unexpected action: {action:?}"),
                    }
                }
            })
            .await
            .expect("worker must settle");
        }

        fn press(&mut self, character: char) {
            self.surface
                .handle_event_result(&key(character), &self.host);
        }

        fn recorded(&self) -> Vec<char> {
            self.recorded.lock().unwrap().clone()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn key_against_superseded_revision_applies_exactly_once() {
        let mut harness = Harness::open().await;
        harness.display(4).await;
        harness.supersede(5);
        harness.settle().await;
        // The producer is now at a newer revision than the one displayed.
        harness.press('a');
        harness.settle().await;
        assert_eq!(harness.recorded(), vec!['a']);
        harness.display(5).await;
        harness.press('b');
        harness.settle().await;
        assert_eq!(harness.recorded(), vec!['a', 'b']);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn key_burst_across_revision_bump_preserves_order() {
        let mut harness = Harness::open().await;
        harness.display(4).await;
        // Undisplayed revisions stay within the retained host's pending-publication bound.
        harness.press('a');
        harness.supersede(5);
        harness.press('b');
        harness.settle().await;
        assert_eq!(harness.recorded(), vec!['a', 'b']);
        harness.display(5).await;
        harness.press('c');
        harness.supersede(6);
        harness.press('d');
        harness.settle().await;
        assert_eq!(harness.recorded(), vec!['a', 'b', 'c', 'd']);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_replay_surfaces_typed_failure() {
        let mut harness = Harness::open().await;
        harness.display(4).await;
        harness.settle().await;
        harness.port.always_stale.store(true, Ordering::Release);
        harness.press('a');
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                match harness.surface.poll(&harness.host) {
                    PluginTuiAction::Close { outcome } => break outcome,
                    PluginTuiAction::None | PluginTuiAction::Redraw => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    action => panic!("unexpected action: {action:?}"),
                }
            }
        })
        .await
        .expect("exhausted replay must be visible");
        let failure: wire::InputDeliveryFailure =
            serde_json::from_value(outcome.expect("typed outcome")).unwrap();
        assert!(matches!(
            failure,
            wire::InputDeliveryFailure::FrontendInputReplayExhausted { attempts, .. }
                if attempts == MAX_INPUT_REPLAYS
        ));
        assert_eq!(harness.recorded(), Vec::<char>::new());
    }
}
