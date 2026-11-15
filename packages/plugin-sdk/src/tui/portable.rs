//! Terminal-only author codec. Static and dynamic frontends use the same surface factories.
//! Scene data is exclusively M7's retained v2 envelope; product actions retain their owned sum.

use super::{
    BoxedPluginTuiSurface, FocusedTextFieldEdit, FocusedTextFieldSnapshot, PluginTuiAction,
    PluginTuiEventResult, PluginTuiFocusedTextFieldEditResult, PluginTuiPresentationConfiguration,
    PluginTuiRegistry, PluginTuiRegistryFactory, PluginTuiSurfaceOpenRequest,
    PluginTuiSurfaceUpdate, PluginTuiSurfaceUpdateSender, PluginTuiTextInputResolver,
    PluginTuiTheme, TokioPluginTuiHost,
};
use crate::definition::{
    PluginCallContext, PluginCreateContext, PluginFuture, PluginResult, ProductComponentExtension,
};
use crate::keymap;
use bcode_session_models::SessionId;
use bmux_keyboard::KeyStroke;
use bmux_plugin_sdk::PluginCommandError;
use bmux_text_edit::{TextEditCommand, TextMotion};
use bmux_tui::presentation::portable::{PortablePresentation, PortablePresentationLimits};
use bmux_tui::{damage::Damage, presentation::PresentationState};
use bmux_tui::{event::Event, geometry::Rect};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::mpsc;

pub(crate) mod wire;

/// Optional correlated response settlement; does not alter frontend-v2 encoding.
pub mod settlement;
mod settlement_dispatch;

#[cfg(test)]
mod tests;

/// Independently negotiated terminal surface operation contract.
///
/// Responses use an adjacent `response` discriminator and `payload` content.
/// Version 1's flattened `outcome` discriminator collided with event outcomes and
/// is not accepted. Producer and consumer share this codec.
/// Deferred edits use one in-flight edit per resource and `EditStatus` polling;
/// peers lacking that operation reject it rather than replaying an admitted edit.
/// Unknown edit outcomes remain explicitly unsettled and never authorize fallback.
///
/// Version 3 adds [`Response::Stale`]: input naming a superseded revision is
/// reported as not applied, distinctly from handled input, so the host can
/// re-deliver it against the current revision. Version 2 silently consumed such
/// input and is not accepted.
pub const TUI_FRONTEND_CONTRACT: &str = "bcode.tui.frontend/v3";
/// Negotiated schema version for terminal surface operations.
pub const TUI_FRONTEND_VERSION: u32 = 3;
const MAX_SURFACES: usize = 128;
const MAX_BYTES: usize = crate::SERVICE_BRIDGE_MAX_RESPONSE_BYTES;

/// One configured keymap lookup, including an explicit unbound override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub scope: String,
    pub stroke: KeyStroke,
    pub action: Option<String>,
    pub overridden: bool,
}

/// Renderer-local host intent resolved before delivery, never inferred by the producer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bindings {
    pub labels: Vec<(String, String, Vec<String>)>,
    pub actions: Vec<Binding>,
    #[serde(with = "wire::optional_command")]
    pub edit: Option<TextEditCommand>,
    #[serde(with = "wire::optional_motion")]
    pub selection: Option<TextMotion>,
    pub submits: bool,
}
impl PluginTuiTextInputResolver for Bindings {
    fn action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
        self.actions
            .iter()
            .find(|binding| binding.scope == scope && binding.stroke == stroke)
            .and_then(|binding| binding.action.clone())
    }
    fn is_overridden(&self, scope: &str, stroke: KeyStroke) -> bool {
        self.actions
            .iter()
            .any(|binding| binding.scope == scope && binding.stroke == stroke && binding.overridden)
    }
    fn keys(&self, scope: &str, action: &str) -> Vec<String> {
        self.labels
            .iter()
            .find(|(owner, id, _)| owner == scope && id == action)
            .map_or_else(Vec::new, |(_, _, keys)| keys.clone())
    }
    fn edit_command(&self, _: KeyStroke) -> Option<TextEditCommand> {
        self.edit.clone()
    }
    fn selection_motion(&self, _: KeyStroke) -> Option<TextMotion> {
        self.selection
    }
    fn submits(&self, _: KeyStroke) -> bool {
        self.submits
    }
}

/// Author registry declaration. No executable pointers or native layout cross the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub surfaces: Vec<String>,
    pub commands: Vec<String>,
    pub actions: Vec<keymap::KeymapAction>,
}

/// One renderer-owned viewport request. Source and destination geometry stay terminal-local.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u16,
    pub height: u16,
    pub logical_row_offset: u16,
    pub theme: Option<PluginTuiTheme>,
}

/// Exact surface operations. Resource handles are activation-local, never credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Describe,
    Open {
        kind: String,
        request: PluginTuiSurfaceOpenRequest,
    },
    Prepare {
        resource: uuid::Uuid,
        /// Boxed because the optional theme makes the viewport far larger than every
        /// other request; serde serializes `Box<T>` transparently, so the wire shape
        /// is unchanged.
        viewport: Box<Viewport>,
        bindings: Bindings,
    },
    Input {
        resource: uuid::Uuid,
        expected_revision: u64,
        #[serde(with = "wire::input")]
        event: Event,
        bindings: Bindings,
        focused: bool,
    },
    Update {
        resource: uuid::Uuid,
        update: PluginTuiSurfaceUpdate,
    },
    Edit {
        resource: uuid::Uuid,
        expected_revision: u64,
        /// Boxed for the same size reason as `Prepare::viewport`; wire shape unchanged.
        edit: Box<FocusedTextFieldEdit>,
    },
    /// Poll the single admitted edit for this resource without replaying it.
    EditStatus {
        resource: uuid::Uuid,
    },
    NavigationFinished {
        resource: uuid::Uuid,
        session_id: SessionId,
        result: Result<(), String>,
    },
    Effects {
        resource: uuid::Uuid,
    },
    Close {
        resource: uuid::Uuid,
    },
    Configure {
        configuration: PluginTuiPresentationConfiguration,
    },
    Command {
        id: String,
    },
    DrainText,
}

/// Full scene and controller observations produced outside the receiving frame loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Projection {
    pub action: PluginTuiAction,
    pub resource: uuid::Uuid,
    pub revision: u64,
    pub scene: PortablePresentation,
    pub logical_height: u16,
    pub focused_rows: Option<std::ops::Range<u16>>,
    pub field: Option<FocusedTextFieldSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "response", content = "payload", rename_all = "snake_case")]
pub enum Response {
    Descriptor(Descriptor),
    Opened {
        resource: uuid::Uuid,
        id: String,
        title: String,
    },
    Projection(Box<Projection>),
    Event(PluginTuiEventResult),
    Edit(PluginTuiFocusedTextFieldEditResult),
    Action(PluginTuiAction),
    Text(Vec<String>),
    EditStatus(Option<PluginTuiFocusedTextFieldEditResult>),
    Applied,
    /// Plain input named a revision the surface has already superseded. Nothing was
    /// applied; `revision` is the surface's current revision, which the host may name
    /// when re-delivering the same input. Fenced operations such as focused text field
    /// edits never use this outcome: they reject as stale instead.
    Stale {
        revision: u64,
    },
}

/// Typed host outcome when plain surface input could not be delivered.
///
/// Hosts surface it as the `outcome` of a closing [`PluginTuiAction::Close`]: once an
/// input is lost, later input would apply out of order, so the surface fails closed
/// visibly instead of silently dropping a keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputDeliveryFailure {
    /// Stale input was re-delivered `attempts` times while the surface kept advancing.
    FrontendInputReplayExhausted {
        /// Re-deliveries attempted after the original delivery.
        attempts: u32,
        /// The surface revision reported by the final stale response.
        revision: u64,
    },
    /// More input arrived than the host's bounded per-surface backlog admits.
    FrontendInputBacklogExceeded {
        /// The backlog bound that was exceeded.
        capacity: usize,
    },
}

struct Surface {
    implementation: BoxedPluginTuiSurface,
    host: TokioPluginTuiHost,
    redraw: mpsc::Receiver<()>,
    updates: PluginTuiSurfaceUpdateSender,
    scene: PresentationState,
    revision: u64,
    dirty: bool,
    viewport: Option<Viewport>,
    bindings: Option<Bindings>,
    edit_pending: bool,
}

impl Surface {
    fn deliver_update(&mut self, update: PluginTuiSurfaceUpdate) -> PluginResult<Response> {
        match self.updates.try_send(update) {
            // A surface that never attached its update stream (the trait
            // default drops it) declines updates; delivery is a no-op, exactly
            // as for the same surface linked statically.
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(failure("frontend update queue full; refresh required"));
            }
        }
        self.dirty = true;
        Ok(Response::Applied)
    }
}
struct State {
    registry: Option<Arc<PluginTuiRegistry>>,
    surfaces: BTreeMap<uuid::Uuid, Surface>,
}

pub(super) struct AuthorTasks {
    handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    failed: AtomicBool,
    closing: AtomicBool,
    context: Option<Arc<settlement::ResponseContext>>,
}
impl Default for AuthorTasks {
    fn default() -> Self {
        Self::new()
    }
}
impl AuthorTasks {
    const fn new() -> Self {
        Self {
            handles: std::sync::Mutex::new(Vec::new()),
            failed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            context: None,
        }
    }
    fn reap(&self) -> PluginResult<()> {
        let mut handles = self
            .handles
            .lock()
            .map_err(|_| failure("frontend tasks unavailable"))?;
        let mut outcomes = Vec::new();
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut index = 0;
        while index < handles.len() {
            if handles[index].is_finished() {
                match std::future::Future::poll(std::pin::Pin::new(&mut handles[index]), &mut cx) {
                    std::task::Poll::Ready(outcome) => {
                        drop(handles.swap_remove(index));
                        outcomes.push(outcome);
                    }
                    std::task::Poll::Pending => {
                        index += 1;
                    }
                }
            } else {
                index += 1;
            }
        }
        drop(handles);
        if outcomes.iter().any(Result::is_err) {
            self.failed.store(true, Ordering::Release);
            return Err(failure("frontend author task failed; cleanup required"));
        }
        Ok(())
    }
    pub(super) fn spawn(&self, handle: &tokio::runtime::Handle, task: super::PluginTask) {
        if self.reap().is_err() || self.closing.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut handles) = self.handles.lock() else {
            self.failed.store(true, Ordering::Release);
            return;
        };
        if self.closing.load(Ordering::Acquire) || handles.len() >= 32 {
            self.failed.store(true, Ordering::Release);
            return;
        }
        let Ok(lineage) = self
            .context
            .as_ref()
            .map(settlement::ResponseContext::reserve_task)
            .transpose()
        else {
            self.failed.store(true, Ordering::Release);
            return;
        };
        handles.push(handle.spawn(async move {
            task.await;
            if let Some(lineage) = lineage {
                lineage.finish();
            }
        }));
    }
    pub(super) fn spawn_blocking(
        &self,
        handle: &tokio::runtime::Handle,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) {
        if self.reap().is_err() || self.closing.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut handles) = self.handles.lock() else {
            self.failed.store(true, Ordering::Release);
            return;
        };
        if self.closing.load(Ordering::Acquire) || handles.len() >= 32 {
            self.failed.store(true, Ordering::Release);
            return;
        }
        // Track the blocking operation itself, not an abortable observer future.
        let Ok(lineage) = self
            .context
            .as_ref()
            .map(settlement::ResponseContext::reserve_task)
            .transpose()
        else {
            self.failed.store(true, Ordering::Release);
            return;
        };
        handles.push(handle.spawn_blocking(move || {
            task();
            if let Some(lineage) = lineage {
                lineage.finish();
            }
        }));
    }
    fn seal_joined_publication(&self) -> PluginResult<bool> {
        self.reap()?;
        let handles = self
            .handles
            .lock()
            .map_err(|_| failure("frontend tasks unavailable"))?;
        if !handles.is_empty() {
            return Ok(false);
        }
        // The same lock used for spawn admission fences the final empty observation.
        self.closing.store(true, Ordering::Release);
        if let Some(context) = &self.context {
            context.seal();
        }
        drop(handles);
        Ok(true)
    }

    pub(in crate::tui) fn settle(&self) -> PluginResult<()> {
        self.closing.store(true, Ordering::Release);
        self.reap()?;
        let handles = self
            .handles
            .lock()
            .map_err(|_| failure("frontend tasks unavailable"))?;
        let empty = handles.is_empty();
        drop(handles);
        if empty && !self.failed.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(failure(
                "frontend author work still settling; retry cleanup",
            ))
        }
    }
}

struct SettlementSession {
    session_id: uuid::Uuid,
    ledger: settlement::ResponseLedger,
    responses: BTreeMap<u64, SettlementWork>,
    callbacks: Arc<super::host::RetainedCallbacks>,
}
struct SettlementWork {
    context: Arc<settlement::ResponseContext>,
    tasks: Arc<AuthorTasks>,
    failed: bool,
    effects_required: bool,
    resource: Option<uuid::Uuid>,
    published_in: Option<settlement::ResponseId>,
}

/// An activation-owned adapter. Destruction waits for admitted work; no global registry state.
pub struct AuthorFrontend {
    context: PluginCreateContext,
    factory: PluginTuiRegistryFactory,
    state: std::sync::Mutex<Option<State>>,
    closed: AtomicBool,
    executor: std::sync::OnceLock<Result<AuthorExecutor, String>>,
    tasks: Arc<AuthorTasks>,
    retained_session:
        std::sync::Mutex<Option<Arc<crate::definition::host_session::RetainedProductHostSession>>>,
    callbacks: std::sync::Mutex<Option<Arc<super::host::RetainedCallbacks>>>,
    recovery_session:
        std::sync::Mutex<Option<Arc<crate::definition::host_session::RetainedProductHostSession>>>,
    settlement: std::sync::Mutex<Option<SettlementSession>>,
    settlement_busy: AtomicBool,
}

/// One author-local worker; its Tokio TLS belongs to this linked SDK image.
struct AuthorExecutor {
    handle: tokio::runtime::Handle,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl AuthorExecutor {
    fn new() -> Result<Self, String> {
        let (ready, receive) = std::sync::mpsc::sync_channel(1);
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("bcode-frontend-author".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .max_blocking_threads(1)
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        if ready.send(Ok(runtime.handle().clone())).is_ok() {
                            runtime.block_on(async {
                                let _ = stopped.await;
                            });
                        }
                    }
                    Err(_) => {
                        let _ = ready.send(Err("frontend author executor unavailable".to_owned()));
                    }
                }
            })
            .map_err(|_| "frontend author worker unavailable".to_owned())?;
        if let Ok(Ok(handle)) = receive.recv() {
            Ok(Self {
                handle,
                shutdown: Some(shutdown),
                worker: Some(worker),
            })
        } else {
            let _ = worker.join();
            Err("frontend author executor initialization failed".to_owned())
        }
    }
}
impl Drop for AuthorExecutor {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(worker) = self.worker.take() {
            // The worker owns runtime destruction, outside any caller runtime.
            // Joining keeps author code resident through executor shutdown.
            let _ = worker.join();
        }
    }
}

/// Attach this exact adapter to the frontend component's extension registrations.
/// Factory invocation is delayed until the admitted component runs in its host executor.
#[must_use]
pub fn extension(
    factory: PluginTuiRegistryFactory,
    context: PluginCreateContext,
) -> Arc<dyn ProductComponentExtension> {
    author(factory, context)
}

fn author(factory: PluginTuiRegistryFactory, context: PluginCreateContext) -> Arc<AuthorFrontend> {
    Arc::new(AuthorFrontend {
        factory,
        executor: std::sync::OnceLock::new(),
        tasks: Arc::new(AuthorTasks::new()),
        retained_session: std::sync::Mutex::new(None),
        callbacks: std::sync::Mutex::new(None),
        recovery_session: std::sync::Mutex::new(None),
        settlement: std::sync::Mutex::new(None),
        settlement_busy: AtomicBool::new(false),
        state: std::sync::Mutex::new(Some(State {
            registry: None,
            surfaces: BTreeMap::new(),
        })),
        closed: AtomicBool::new(context.role != crate::definition::ComponentRole::Frontend),
        context,
    })
}

/// Optional dispatcher handle sharing the exact ordinary frontend author instance.
/// It is not installed or advertised by ordinary `extension`.
pub struct SettlementExtension(Arc<AuthorFrontend>);
impl ProductComponentExtension for SettlementExtension {
    fn invoke(
        &self,
        call: PluginCallContext,
        payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        self.0.invoke_settlement(call, payload)
    }
    fn close(&self) -> PluginResult<()> {
        self.0.close()
    }
}

/// Build two dispatch handles over one actual author, without advertising either contract.
/// Installation must remain disabled until the product consumer settlement path is complete.
#[must_use]
pub fn extensions_with_settlement(
    factory: PluginTuiRegistryFactory,
    context: PluginCreateContext,
) -> (
    Arc<dyn ProductComponentExtension>,
    Arc<dyn ProductComponentExtension>,
) {
    let author = author(factory, context);
    (author.clone(), Arc::new(SettlementExtension(author)))
}

fn failure(message: &str) -> PluginCommandError {
    PluginCommandError::failed(message)
}

struct StateLease<'a> {
    owner: &'a AuthorFrontend,
    state: Option<State>,
}
impl Drop for StateLease<'_> {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.owner.state.lock() {
            *slot = self.state.take();
        }
    }
}

impl AuthorFrontend {
    /// Capture the actual independent recovery-V2 capability while this origin is live.
    /// This is separate from ordinary frontend-v2's retained-V1 session; neither substitutes
    /// for the other. Publish the recovery owner before any negotiation await.
    ///
    /// # Errors
    /// Missing/rejected V2 acquisition fails closed without V1 fallback. Poisoned state
    /// requires recovery and never replaces the original retained negotiation identity.
    pub fn capture_settlement_session(
        &self,
        call: &PluginCallContext,
    ) -> PluginResult<Arc<crate::definition::host_session::RetainedProductHostSession>> {
        let mut slot = self
            .recovery_session
            .lock()
            .map_err(|_| failure("frontend V2 recovery owner unavailable"))?;
        if let Some(session) = slot.as_ref() {
            return Ok(Arc::clone(session));
        }
        if self.closed.load(Ordering::Acquire) || call.cancellation.is_cancelled() {
            return Err(failure("frontend V2 capture admission closed"));
        }
        let session = crate::definition::host_session::open_retained_session_v2(
            self.context.clone(),
            call,
            Arc::new(crate::definition::host_session::retained::TokioRetainedSessionWait),
        )
        .map_err(|_| failure("frontend requires actual recovery-V2 capability; no V1 fallback"))?;
        *slot = Some(Arc::clone(&session));
        drop(slot);
        Ok(session)
    }

    /// Advance a correlated response's callback retirement after product consumer commitment.
    ///
    /// This author-side entry point is not advertised as a frontend extension. It requires
    /// the actual response-local task owner; ordinary frontend-v2 tasks cannot use it.
    /// Product dispatch must additionally negotiate generic lookup-V2 before admitting this mode.
    ///
    /// # Errors
    /// Rejects mismatched/unbound response lineage, unjoined tasks, missing consumer commitment,
    /// absent callback authority, uncertain replies and unresolved callback effects.
    pub async fn settle_response(
        &self,
        ledger: &settlement::ResponseLedger,
        context: &settlement::ResponseContext,
    ) -> PluginResult<bool> {
        if !self
            .tasks
            .context
            .as_ref()
            .is_some_and(|bound| std::ptr::eq(bound.as_ref(), context))
        {
            return Err(failure(
                "frontend response task ownership unavailable; recovery required",
            ));
        }
        let callbacks = self
            .callbacks
            .lock()
            .map_err(|_| failure("frontend retained callbacks unavailable"))?
            .clone()
            .ok_or_else(|| failure("frontend retained callback authority unavailable"))?;
        callbacks
            .settle_response(&self.tasks, ledger, context)
            .await
            .map_err(|_| failure("frontend callback settlement requires recovery"))
    }

    fn capture_retained(
        &self,
        call: &crate::definition::PluginCallContext,
    ) -> PluginResult<Option<Arc<crate::definition::host_session::RetainedProductHostSession>>>
    {
        // Capture only an independent opener while the origin invocation is valid.
        // Missing authority remains fail-closed; request-local services are never retained.
        {
            let mut retained = self
                .retained_session
                .lock()
                .map_err(|_| failure("frontend retained capability unavailable"))?;
            if retained.is_none() {
                *retained = match crate::definition::host_session::open_retained_session(
                    self.context.clone(),
                    call,
                    Arc::new(crate::definition::host_session::retained::TokioRetainedSessionWait),
                ) {
                    Ok(session) => Some(session),
                    Err(crate::definition::host_session::HostSessionClientError::Unsupported) => {
                        None
                    }
                    Err(_) => {
                        return Err(failure("frontend retained capability requires recovery"));
                    }
                };
            }
        }
        let retained = self
            .retained_session
            .lock()
            .map_err(|_| failure("frontend retained capability unavailable"))?
            .clone();
        Ok(retained)
    }
    async fn execute_retained(
        &self,
        request: Request,
        retained: Option<Arc<crate::definition::host_session::RetainedProductHostSession>>,
    ) -> PluginResult<Response> {
        let existing = self
            .callbacks
            .lock()
            .map_err(|_| failure("frontend retained callbacks unavailable"))?
            .clone();
        let callbacks = if let Some(callbacks) = existing {
            Some(callbacks)
        } else if let Some(retained) = retained {
            let client = retained
                .negotiate()
                .await
                .map_err(|_| failure("frontend retained negotiation requires recovery"))?;
            let callbacks = super::host::RetainedCallbacks::new(client);
            *self
                .callbacks
                .lock()
                .map_err(|_| failure("frontend retained callbacks unavailable"))? =
                Some(callbacks.clone());
            Some(callbacks)
        } else {
            None
        };
        self.execute_with_host(request, callbacks.map(|callbacks| callbacks.services()))
            .await
    }

    fn checkout(&self) -> PluginResult<StateLease<'_>> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("frontend state unavailable"))?
            .take()
            .ok_or_else(|| failure("frontend operation busy; no nested admission"))?;
        Ok(StateLease {
            owner: self,
            state: Some(state),
        })
    }

    #[cfg(test)]
    async fn execute(&self, request: Request) -> PluginResult<Response> {
        self.execute_with_host(request, None).await
    }

    fn bind_surfaces(
        state: &mut State,
        services: Option<&bmux_plugin_sdk::definition::PluginHostServices>,
    ) {
        for surface in state.surfaces.values_mut() {
            surface.host.set_product_host(services.cloned());
        }
    }

    async fn execute_with_host(
        &self,
        request: Request,
        services: Option<bmux_plugin_sdk::definition::PluginHostServices>,
    ) -> PluginResult<Response> {
        self.execute_with_tasks(request, services, Arc::clone(&self.tasks))
            .await
    }

    async fn execute_with_tasks(
        &self,
        request: Request,
        services: Option<bmux_plugin_sdk::definition::PluginHostServices>,
        tasks: Arc<AuthorTasks>,
    ) -> PluginResult<Response> {
        if self.context.role != crate::definition::ComponentRole::Frontend
            || self.closed.load(Ordering::Acquire)
        {
            return Err(failure("frontend activation closed"));
        }
        let mut lease = self.checkout()?;
        let state = lease
            .state
            .as_mut()
            .ok_or_else(|| failure("frontend state unavailable"))?;
        if self.closed.load(Ordering::Acquire) {
            return Err(failure("frontend activation closed"));
        }
        Self::bind_surfaces(state, services.as_ref());
        for surface in state.surfaces.values_mut() {
            surface.host = surface.host.clone().with_author_tasks(Arc::clone(&tasks));
        }
        let registry = state
            .registry
            .get_or_insert_with(|| Arc::new((self.factory)()))
            .clone();
        let response = match request {
            Request::Describe => Ok(Response::Descriptor(Descriptor {
                surfaces: registry.factories.keys().cloned().collect(),
                commands: registry.frontend_commands.keys().cloned().collect(),
                actions: registry.keymap_actions.clone(),
            })),
            Request::Open { kind, request } => {
                self.open_surface(state, &registry, &kind, request, services, tasks)
                    .await
            }
            Request::Configure { configuration } => {
                configuration
                    .validate()
                    .map_err(|_| failure("invalid frontend configuration"))?;
                registry
                    .configure(&configuration)
                    .map_err(|_| failure("frontend configuration rejected"))?;
                for surface in state.surfaces.values_mut() {
                    surface.dirty = true;
                }
                Ok(Response::Applied)
            }
            Request::Command { id } => {
                registry
                    .frontend_command(&id)
                    .ok_or_else(|| failure("frontend command unavailable"))?
                    .invoke()
                    .map_err(|_| failure("frontend command failed"))?;
                Ok(Response::Applied)
            }
            Request::DrainText => Ok(Response::Text(
                registry
                    .drain_frontend_text()
                    .map_err(|_| failure("frontend text unavailable"))?,
            )),
            Request::Close { resource } => {
                if let Some(callbacks) = self
                    .callbacks
                    .lock()
                    .map_err(|_| failure("frontend retained cleanup unavailable"))?
                    .as_ref()
                {
                    callbacks.ensure_settled().map_err(|_| {
                        failure("frontend retained outcomes require explicit settlement")
                    })?;
                }
                state
                    .surfaces
                    .remove(&resource)
                    .ok_or_else(|| failure("unknown frontend surface"))?;
                Ok(Response::Applied)
            }
            request => Self::execute_surface(state, request).await,
        };
        drop(lease);
        response
    }

    async fn open_surface(
        &self,
        state: &mut State,
        registry: &PluginTuiRegistry,
        kind: &str,
        request: PluginTuiSurfaceOpenRequest,
        services: Option<bmux_plugin_sdk::definition::PluginHostServices>,
        tasks: Arc<AuthorTasks>,
    ) -> PluginResult<Response> {
        if state.surfaces.len() >= MAX_SURFACES {
            return Err(failure("frontend resource budget exceeded"));
        }
        let mut surface = registry
            .open(kind, request)
            .await
            .map_err(|_| failure("frontend surface open failed"))?;
        let id = surface.id().to_owned();
        let title = surface.title().to_owned();
        let resource = uuid::Uuid::new_v4();
        let (redraw_sender, redraw) = mpsc::channel(1);
        let (updates, receiver) = mpsc::channel(32);
        surface.attach_updates(receiver);
        let mut host = TokioPluginTuiHost::current(redraw_sender).with_author_tasks(tasks);
        host.set_product_host(services);
        state.surfaces.insert(
            resource,
            Surface {
                implementation: surface,
                host,
                redraw,
                updates,
                scene: PresentationState::new(Rect::new(0, 0, 1, 1)),
                revision: 0,
                dirty: true,
                viewport: None,
                bindings: None,
                edit_pending: false,
            },
        );
        Ok(Response::Opened {
            resource,
            id,
            title,
        })
    }

    async fn execute_surface(state: &mut State, request: Request) -> PluginResult<Response> {
        let resource = match &request {
            Request::Prepare { resource, .. }
            | Request::Input { resource, .. }
            | Request::Update { resource, .. }
            | Request::Edit { resource, .. }
            | Request::EditStatus { resource }
            | Request::NavigationFinished { resource, .. }
            | Request::Effects { resource } => *resource,
            _ => return Err(failure("invalid surface operation")),
        };
        let surface = state
            .surfaces
            .get_mut(&resource)
            .ok_or_else(|| failure("unknown frontend surface"))?;
        match request {
            Request::Prepare {
                viewport, bindings, ..
            } => Self::prepare(surface, resource, *viewport, bindings),
            Request::Input {
                expected_revision,
                event,
                bindings,
                focused,
                ..
            } => {
                if expected_revision < surface.revision {
                    // Report rather than consume: the host re-delivers plain input
                    // against the current revision instead of losing it.
                    return Ok(Response::Stale {
                        revision: surface.revision,
                    });
                }
                if expected_revision != surface.revision {
                    return Err(failure("frontend input names an unprepared revision"));
                }
                surface.host.set_text_input_resolver(Arc::new(bindings));
                let result = if focused {
                    let editor = surface
                        .implementation
                        .focused_text_field_event(&event, &surface.host);
                    if editor.outcome == bmux_tui::event::EventOutcome::Ignored {
                        surface
                            .implementation
                            .handle_event_result(&event, &surface.host)
                    } else {
                        editor
                    }
                } else {
                    surface
                        .implementation
                        .handle_event_result(&event, &surface.host)
                };
                surface.dirty = true;
                Ok(Response::Event(result))
            }
            Request::Update { update, .. } => surface.deliver_update(update),
            Request::Edit {
                expected_revision,
                edit,
                ..
            } => {
                if surface.edit_pending || surface.revision != expected_revision {
                    return Ok(Response::Edit(
                        PluginTuiFocusedTextFieldEditResult::rejected(
                            bcode_plugin_models::composer::ComposerValidationError::Stale,
                        ),
                    ));
                }
                let result = surface
                    .implementation
                    .apply_focused_text_field_edit_result(&edit, &surface.host);
                surface.edit_pending = matches!(
                    result.edit_result,
                    super::FocusedTextFieldEditResult::Pending
                        | super::FocusedTextFieldEditResult::Unknown
                );
                surface.dirty = true;
                Ok(Response::Edit(result))
            }
            Request::EditStatus { .. } => Self::edit_status(surface),
            Request::NavigationFinished {
                session_id, result, ..
            } => {
                surface
                    .implementation
                    .session_navigation_finished(session_id, result);
                surface.dirty = true;
                Ok(Response::Applied)
            }
            Request::Effects { .. } => {
                let action = surface.implementation.drain_effects(&surface.host).await;
                if !matches!(action, PluginTuiAction::None) {
                    surface.dirty = true;
                }
                Ok(Response::Action(action))
            }
            _ => Err(failure("invalid surface operation")),
        }
    }

    /// Observe the sole admitted edit without repeating its mutation or effects.
    fn edit_status(surface: &mut Surface) -> PluginResult<Response> {
        if !surface.edit_pending {
            return Err(failure("no admitted frontend edit"));
        }
        let result = surface.implementation.take_focused_text_field_edit_result();
        if let Some(result) = &result {
            surface.edit_pending = matches!(
                result.edit_result,
                super::FocusedTextFieldEditResult::Pending
                    | super::FocusedTextFieldEditResult::Unknown
            );
            surface.dirty = true;
        }
        Ok(Response::EditStatus(result))
    }

    fn prepare(
        surface: &mut Surface,
        resource: uuid::Uuid,
        viewport: Viewport,
        bindings: Bindings,
    ) -> PluginResult<Response> {
        if viewport.width == 0
            || viewport.height == 0
            || viewport.height > 512
            || usize::from(viewport.width) * usize::from(viewport.height) > 131_072
        {
            return Err(failure("frontend viewport exceeds budget"));
        }
        if surface.bindings.as_ref() != Some(&bindings) {
            surface.dirty = true;
        }
        surface
            .host
            .set_text_input_resolver(Arc::new(bindings.clone()));
        let action = surface.implementation.poll(&surface.host);
        if !matches!(action, PluginTuiAction::None) {
            surface.dirty = true;
        }
        while surface.redraw.try_recv().is_ok() {
            surface.dirty = true;
        }
        if !surface.dirty && surface.viewport.as_ref() == Some(&viewport) {
            return Ok(Response::Applied);
        }
        surface.implementation.prepare_keymap(&surface.host);
        let logical_height = surface
            .implementation
            .preferred_height(viewport.width)
            .min(4096);
        let focused_rows = surface.implementation.focused_row_range(viewport.width);
        let area = Rect::new(0, 0, viewport.width, viewport.height);
        if surface.viewport.as_ref().is_none_or(|previous| {
            previous.width != viewport.width || previous.height != viewport.height
        }) {
            surface.scene.resize(area);
        }
        let mut prepared = surface
            .scene
            .prepare(Damage::Full, |cx| {
                if viewport.logical_row_offset == 0 && logical_height <= viewport.height {
                    surface.implementation.render_with_host(
                        area,
                        cx,
                        viewport.theme.as_ref(),
                        &surface.host,
                    );
                } else {
                    surface.implementation.render_slice_with_host(
                        logical_height,
                        viewport.logical_row_offset,
                        area,
                        cx,
                        &surface.host,
                    );
                }
            })
            .ok_or_else(|| failure("frontend did not prepare output"))?;
        prepared
            .set_selection_text(surface.implementation.retained_selection_text())
            .map_err(|_| failure("frontend selection source unavailable"))?;
        if !prepared.set_modal_scopes(&surface.implementation.retained_modal_scopes()) {
            return Err(failure("invalid frontend modal declarations"));
        }
        let scene = PortablePresentation::from_prepared(&prepared);
        scene
            .validate(PortablePresentationLimits::default())
            .map_err(|error| failure(&format!("invalid rich frontend projection: {error}")))?;
        // This commit is producer-local only. Receiving input is still fenced by the frontend's
        // successful displayed M7 lease, not this offscreen preparation.
        surface
            .scene
            .present_prepared(Some(prepared), |_, _| Ok::<(), ()>(()))
            .map_err(|()| failure("frontend preparation commit failed"))?;
        surface.revision = surface
            .revision
            .checked_add(1)
            .ok_or_else(|| failure("frontend revision exhausted"))?;
        surface.dirty = false;
        surface.viewport = Some(viewport);
        surface.bindings = Some(bindings);
        Ok(Response::Projection(Box::new(Projection {
            action,
            resource,
            revision: surface.revision,
            scene,
            logical_height,
            focused_rows,
            field: surface.implementation.focused_text_field(),
        })))
    }
}

impl ProductComponentExtension for AuthorFrontend {
    fn invoke(
        &self,
        call: PluginCallContext,
        payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            if call.cancellation.is_cancelled() {
                return Err(failure("frontend operation cancelled"));
            }
            if payload.len() > crate::SERVICE_BRIDGE_MAX_REQUEST_BYTES {
                return Err(failure("frontend request exceeds budget"));
            }
            if self.tasks.failed.load(Ordering::Acquire) {
                return Err(failure("frontend task admission failed; close activation"));
            }
            let request: Request = serde_json::from_slice(&payload)
                .map_err(|_| failure("invalid frontend operation"))?;
            let executor = self
                .executor
                .get_or_init(AuthorExecutor::new)
                .as_ref()
                .map_err(|message| failure(message))?;
            let retained = self.capture_retained(&call)?;
            let mut execution = std::pin::pin!(self.execute_retained(request, retained));
            // Enter on every poll: Tokio runtime TLS is image-local and a future
            // may resume on another host thread. Never block_on a caller thread.
            // The borrowed adapter/state lease stays inside this admitted future;
            // cancellation drops it rather than detaching effectful work.
            let response = std::future::poll_fn(|cx| {
                if call.cancellation.is_cancelled() {
                    return std::task::Poll::Ready(Err(failure("frontend operation cancelled")));
                }
                let _entered = executor.handle.enter();
                std::future::Future::poll(execution.as_mut(), cx)
            })
            .await?;
            let bytes = serde_json::to_vec(&response)
                .map_err(|_| failure("frontend response encoding failed"))?;
            if bytes.len() > MAX_BYTES {
                return Err(failure(
                    "frontend response exceeds budget; do not replay mutation",
                ));
            }
            Ok(bytes)
        })
    }
    fn close(&self) -> PluginResult<()> {
        if self.settlement_busy.load(Ordering::Acquire) {
            return Err(failure(
                "frontend settlement work admitted; cleanup requires recovery",
            ));
        }
        if let Some(session) = self
            .settlement
            .lock()
            .map_err(|_| failure("frontend settlement state unavailable"))?
            .as_ref()
            && !session.responses.is_empty()
        {
            return Err(failure("frontend correlated responses remain unsettled"));
        }
        self.closed.store(true, Ordering::Release);
        if let Some(callbacks) = self
            .callbacks
            .lock()
            .map_err(|_| failure("frontend retained cleanup unavailable"))?
            .as_ref()
        {
            callbacks
                .ensure_settled()
                .map_err(|_| failure("frontend retained outcomes require explicit settlement"))?;
        }
        let mut lease = self.checkout()?;
        let state = lease
            .state
            .as_mut()
            .ok_or_else(|| failure("frontend state unavailable"))?;
        self.tasks.settle()?;
        state.surfaces.clear();
        if let Some(registry) = state.registry.take() {
            registry.shutdown_frontend_commands();
        }
        drop(lease);
        self.tasks.settle()
    }
}
