//! Native plugin TUI presentation and surface helpers.

use bcode_plugin::{PluginHost, PluginLoadError, PluginRuntimeHost, StaticBundledPlugin};
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiArtifactChunk, PluginTuiRegistry, PluginTuiSurfaceOpenRequest,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

const MAX_DYNAMIC_VISUAL_CACHE_ENTRIES: usize = 512;
const DYNAMIC_VISUAL_QUEUE_CAPACITY: usize = 64;
const DYNAMIC_VISUAL_COMPLETION_CAPACITY: usize =
    MAX_DYNAMIC_VISUAL_CACHE_ENTRIES + DYNAMIC_VISUAL_QUEUE_CAPACITY;
const DYNAMIC_VISUAL_SERVICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const MAX_DYNAMIC_VISUAL_COMPLETIONS_PER_POLL: usize = 16;

#[derive(Debug, Clone)]
enum PresentationPluginBackend {
    Synchronous(Arc<PluginHost>),
    Runtime(PluginRuntimeHost),
}

impl PresentationPluginBackend {
    fn visual_adapters(
        &self,
        schema: &str,
        schema_version: u32,
        surface: &str,
        producer_plugin_id: Option<&str>,
    ) -> Vec<bcode_plugin::PluginVisualAdapterRoute> {
        match self {
            Self::Synchronous(host) => {
                host.visual_adapters(schema, schema_version, surface, producer_plugin_id)
            }
            Self::Runtime(runtime) => runtime.registry().visual_adapters(
                schema,
                schema_version,
                surface,
                producer_plugin_id,
            ),
        }
    }

    fn tool_presentation(
        &self,
        tool_name: &str,
    ) -> Option<(&str, &bcode_plugin::PluginToolPresentationDeclaration)> {
        match self {
            Self::Synchronous(host) => host.tool_presentation(tool_name),
            Self::Runtime(runtime) => runtime.tool_presentation(tool_name),
        }
    }

    fn has_service(&self, plugin_id: &str, interface_id: &str) -> bool {
        match self {
            Self::Synchronous(host) => host.has_service(plugin_id, interface_id),
            Self::Runtime(runtime) => {
                runtime
                    .registry()
                    .manifests()
                    .get(plugin_id)
                    .is_some_and(|manifest| {
                        manifest
                            .services
                            .iter()
                            .any(|service| service.interface_id == interface_id)
                    })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DynamicVisualKey {
    plugin_id: String,
    adapter_id: String,
    invocation_id: String,
    schema: String,
    schema_version: u32,
    payload_revision: u64,
    width: u16,
    theme_fingerprint: u64,
}

#[derive(Debug)]
struct DynamicVisualRequest {
    key: DynamicVisualKey,
    request: bcode_plugin_sdk::tui_visual::RenderTuiVisualRequest,
}

#[derive(Debug)]
enum DynamicVisualJob {
    Render(DynamicVisualRequest),
    Artifact {
        plugin_id: String,
        invocation_id: String,
        request: bcode_plugin_sdk::tui_visual::SerializedTuiArtifactChunkRequest,
    },
}

#[derive(Debug)]
struct DynamicVisualCompletion {
    key: Option<DynamicVisualKey>,
    invocation_id: String,
    invalidate_invocation_cache: bool,
    result: Result<bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse, String>,
}

#[derive(Debug)]
struct DynamicVisualCoordinator {
    request_sender: Option<SyncSender<DynamicVisualJob>>,
    completion_receiver: Receiver<DynamicVisualCompletion>,
    worker: Option<JoinHandle<()>>,
    requested: BTreeSet<DynamicVisualKey>,
    cache: BTreeMap<
        DynamicVisualKey,
        Result<bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse, String>,
    >,
}

impl DynamicVisualCoordinator {
    fn new(backend: PresentationPluginBackend) -> Self {
        let (request_sender, request_receiver) =
            std::sync::mpsc::sync_channel::<DynamicVisualJob>(DYNAMIC_VISUAL_QUEUE_CAPACITY);
        let (completion_sender, completion_receiver) = std::sync::mpsc::sync_channel::<
            DynamicVisualCompletion,
        >(DYNAMIC_VISUAL_COMPLETION_CAPACITY);
        let worker = std::thread::Builder::new()
            .name("bcode-tui-visual-adapter".to_owned())
            .spawn(move || {
                while let Ok(job) = request_receiver.recv() {
                    let completion = match job {
                        DynamicVisualJob::Render(job) => {
                            let result = invoke_dynamic_visual_service::<
                                _,
                                bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse,
                            >(
                                &backend,
                                &job.key.plugin_id,
                                bcode_plugin_sdk::tui_visual::OP_RENDER_TUI_VISUAL,
                                &job.request,
                            )
                            .and_then(|response| {
                                response.validate()?;
                                Ok(response)
                            });
                            DynamicVisualCompletion {
                                invocation_id: job.key.invocation_id.clone(),
                                key: Some(job.key),
                                invalidate_invocation_cache: false,
                                result,
                            }
                        }
                        DynamicVisualJob::Artifact {
                            plugin_id,
                            invocation_id,
                            request,
                        } => {
                            let result = invoke_dynamic_visual_service::<_, serde_json::Value>(
                                &backend,
                                &plugin_id,
                                bcode_plugin_sdk::tui_visual::OP_DELIVER_TUI_VISUAL_ARTIFACT,
                                &request,
                            )
                            .map(|_| {
                                bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse {
                                    version:
                                        bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_CONTRACT_VERSION,
                                    render_mode: String::new(),
                                    title: None,
                                    timeout_ms: None,
                                    rows: Vec::new(),
                                }
                            });
                            DynamicVisualCompletion {
                                key: None,
                                invocation_id,
                                invalidate_invocation_cache: result.is_ok(),
                                result,
                            }
                        }
                    };
                    if completion_sender.send(completion).is_err() {
                        break;
                    }
                }
            })
            .ok();
        Self {
            request_sender: worker.as_ref().map(|_| request_sender),
            completion_receiver,
            worker,
            requested: BTreeSet::new(),
            cache: BTreeMap::new(),
        }
    }

    fn response(
        &self,
        key: &DynamicVisualKey,
    ) -> Option<&bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse> {
        self.cache.get(key)?.as_ref().ok()
    }

    fn request(&mut self, request: DynamicVisualRequest) {
        if self.cache.len().saturating_add(self.requested.len()) >= MAX_DYNAMIC_VISUAL_CACHE_ENTRIES
            || self.cache.contains_key(&request.key)
            || !self.requested.insert(request.key.clone())
        {
            return;
        }
        let Some(sender) = self.request_sender.as_ref() else {
            return;
        };
        match sender.try_send(DynamicVisualJob::Render(request)) {
            Ok(()) => {}
            Err(
                TrySendError::Full(DynamicVisualJob::Render(request))
                | TrySendError::Disconnected(DynamicVisualJob::Render(request)),
            ) => {
                self.requested.remove(&request.key);
            }
            Err(error) => {
                debug_assert!(matches!(
                    error,
                    TrySendError::Full(DynamicVisualJob::Artifact { .. })
                        | TrySendError::Disconnected(DynamicVisualJob::Artifact { .. })
                ));
            }
        }
    }

    fn artifact(
        &self,
        plugin_id: String,
        invocation_id: String,
        request: bcode_plugin_sdk::tui_visual::SerializedTuiArtifactChunkRequest,
    ) -> bool {
        self.request_sender.as_ref().is_some_and(|sender| {
            sender
                .try_send(DynamicVisualJob::Artifact {
                    plugin_id,
                    invocation_id,
                    request,
                })
                .is_ok()
        })
    }

    fn poll(&mut self) -> BTreeSet<String> {
        let mut dirty = BTreeSet::new();
        for _ in 0..MAX_DYNAMIC_VISUAL_COMPLETIONS_PER_POLL {
            match self.completion_receiver.try_recv() {
                Ok(completion) => {
                    if completion.invalidate_invocation_cache {
                        self.cache
                            .retain(|key, _| key.invocation_id != completion.invocation_id);
                    }
                    dirty.insert(completion.invocation_id);
                    if let Some(key) = completion.key {
                        self.requested.remove(&key);
                        if self.cache.len() >= MAX_DYNAMIC_VISUAL_CACHE_ENTRIES
                            && let Some(oldest) = self.cache.keys().next().cloned()
                        {
                            self.cache.remove(&oldest);
                        }
                        self.cache.insert(key, completion.result);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        dirty
    }
}

fn invoke_dynamic_visual_service<Q, R>(
    backend: &PresentationPluginBackend,
    plugin_id: &str,
    operation: &str,
    request: &Q,
) -> Result<R, String>
where
    Q: serde::Serialize + Sync,
    R: serde::de::DeserializeOwned,
{
    match backend {
        PresentationPluginBackend::Synchronous(host) => host
            .invoke_service_json(
                plugin_id,
                bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_INTERFACE_ID,
                operation,
                request,
            )
            .map_err(|error| error.to_string()),
        PresentationPluginBackend::Runtime(runtime) => {
            let runtime_handle = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|error| error.to_string())?;
            runtime_handle
                .block_on(runtime.invoke_service_json_scoped_with_timeout(
                    plugin_id,
                    bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_INTERFACE_ID,
                    operation,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                    DYNAMIC_VISUAL_SERVICE_TIMEOUT,
                ))
                .map_err(|error| error.to_string())
        }
    }
}

impl Drop for DynamicVisualCoordinator {
    fn drop(&mut self) {
        self.request_sender = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Bounded route metadata and duration for one generic plugin visual operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginVisualTiming {
    /// Stable operation label.
    pub operation: &'static str,
    /// Routed plugin identifier.
    pub plugin_id: String,
    /// Routed schema identifier.
    pub schema: String,
    /// Duration in microseconds.
    pub duration_micros: u64,
}

/// One bounded adapter diagnostic after generic host routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginVisualDiagnostic {
    /// Routed plugin identifier.
    pub plugin_id: String,
    /// Adapter-owned bounded diagnostic name.
    pub name: String,
    /// Non-negative observation value.
    pub value: u64,
}

/// Process-local presentation state for one TUI instance.
///
/// Registries are retained because visual adapters may accumulate incremental artifact state that
/// later render passes consume.
#[derive(Debug)]
pub struct PluginTuiPresentation {
    backend: PresentationPluginBackend,
    visual_adapter_config: bcode_config::TuiVisualAdapterConfig,
    registry_factories: BTreeMap<String, bcode_plugin_sdk::tui::PluginTuiRegistryFactory>,
    registries: Mutex<BTreeMap<String, Arc<PluginTuiRegistry>>>,
    visual_revisions: Mutex<BTreeMap<String, u64>>,
    dirty_visuals: Mutex<BTreeSet<String>>,
    visual_generation: AtomicU64,
    full_generation: AtomicU64,
    timings: Mutex<Vec<PluginVisualTiming>>,
    dynamic_visuals: Mutex<DynamicVisualCoordinator>,
}

/// One renderer-local visual route ready for native row conversion.
#[derive(Debug, Clone)]
pub struct RoutedTuiVisual {
    pub route: bcode_plugin::PluginVisualAdapterRoute,
    pub render_mode: bcode_plugin_sdk::tui::PluginTuiVisualRenderMode,
    pub rows: Vec<bmux_tui::prelude::Line>,
    pub header: bcode_plugin_sdk::tui::PluginTuiTranscriptHeader,
}

impl PluginTuiPresentation {
    /// Create presentation state around a loaded plugin host.
    #[must_use]
    pub fn new(host: PluginHost) -> Self {
        Self::with_config(host, bcode_config::TuiVisualAdapterConfig::default())
    }

    /// Create presentation state with explicit adapter preferences.
    #[must_use]
    pub fn with_config(
        host: PluginHost,
        visual_adapter_config: bcode_config::TuiVisualAdapterConfig,
    ) -> Self {
        Self::from_backend(
            PresentationPluginBackend::Synchronous(Arc::new(host)),
            visual_adapter_config,
            test_tui_extensions(),
        )
    }

    /// Create production presentation state around an isolated plugin runtime.
    #[must_use]
    pub fn with_runtime_config_and_extensions(
        runtime: PluginRuntimeHost,
        visual_adapter_config: bcode_config::TuiVisualAdapterConfig,
        extensions: &[bcode_plugin_sdk::tui::StaticPluginTuiExtension],
    ) -> Self {
        Self::from_backend(
            PresentationPluginBackend::Runtime(runtime),
            visual_adapter_config,
            extensions,
        )
    }

    /// Create presentation state around a shared loaded plugin host.
    #[must_use]
    pub fn from_shared(host: Arc<PluginHost>) -> Self {
        Self::from_backend(
            PresentationPluginBackend::Synchronous(host),
            bcode_config::TuiVisualAdapterConfig::default(),
            test_tui_extensions(),
        )
    }

    fn from_backend(
        backend: PresentationPluginBackend,
        visual_adapter_config: bcode_config::TuiVisualAdapterConfig,
        extensions: &[bcode_plugin_sdk::tui::StaticPluginTuiExtension],
    ) -> Self {
        let registry_factories = extensions
            .iter()
            .map(|extension| {
                (
                    extension.plugin_id().to_owned(),
                    extension.registry_factory(),
                )
            })
            .collect();
        let dynamic_visuals = Mutex::new(DynamicVisualCoordinator::new(backend.clone()));
        Self {
            backend,
            visual_adapter_config,
            registry_factories,
            registries: Mutex::new(BTreeMap::new()),
            visual_revisions: Mutex::new(BTreeMap::new()),
            dirty_visuals: Mutex::new(BTreeSet::new()),
            visual_generation: AtomicU64::new(0),
            full_generation: AtomicU64::new(0),
            timings: Mutex::new(Vec::new()),
            dynamic_visuals,
        }
    }

    /// Return presentation metadata for an exact model-callable tool.
    #[must_use]
    pub fn tool_presentation(
        &self,
        tool_name: &str,
    ) -> Option<(&str, &bcode_plugin::PluginToolPresentationDeclaration)> {
        self.backend.tool_presentation(tool_name)
    }

    /// Return the full presentation generation for registry/adapter replacement.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.full_generation.load(Ordering::Relaxed)
    }

    /// Return the aggregate generation for isolated visual-state updates.
    #[must_use]
    pub fn visual_generation(&self) -> u64 {
        self.visual_generation.load(Ordering::Relaxed)
    }

    /// Return the generic adapter-state revision for one invocation.
    #[must_use]
    pub fn visual_revision(&self, invocation_id: &str) -> u64 {
        self.visual_revisions
            .lock()
            .ok()
            .and_then(|revisions| revisions.get(invocation_id).copied())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn bump_visual_revision_for_test(&self, invocation_id: &str) {
        if let Ok(mut revisions) = self.visual_revisions.lock() {
            let revision = revisions.entry(invocation_id.to_owned()).or_default();
            *revision = revision.wrapping_add(1);
        }
        self.mark_visual_dirty(invocation_id);
    }

    fn mark_visual_dirty(&self, invocation_id: &str) {
        self.visual_generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut dirty) = self.dirty_visuals.lock() {
            dirty.insert(invocation_id.to_owned());
        }
    }

    /// Drain at most `limit` invocation identifiers whose retained adapter state changed.
    ///
    /// Identifiers beyond the limit remain dirty for a later render tick.
    pub fn drain_dirty_visuals_bounded(&self, limit: usize) -> BTreeSet<String> {
        self.dirty_visuals.lock().map_or_else(
            |_| BTreeSet::new(),
            |mut dirty| {
                let mut drained = BTreeSet::new();
                for _ in 0..limit {
                    let Some(invocation_id) = dirty.pop_first() else {
                        break;
                    };
                    drained.insert(invocation_id);
                }
                drained
            },
        )
    }

    /// Drain all invocation identifiers whose retained adapter state changed.
    pub fn drain_dirty_visuals(&self) -> BTreeSet<String> {
        self.dirty_visuals
            .lock()
            .map_or_else(|_| BTreeSet::new(), |mut dirty| std::mem::take(&mut *dirty))
    }

    #[cfg(test)]
    pub fn install_registry_for_test(&self, plugin_id: &str, registry: PluginTuiRegistry) {
        if let Ok(mut registries) = self.registries.lock() {
            registries.insert(plugin_id.to_owned(), Arc::new(registry));
            self.full_generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Return one retained native TUI registry.
    #[must_use]
    pub fn registry(&self, plugin_id: &str) -> Option<Arc<PluginTuiRegistry>> {
        let mut registries = self.registries.lock().ok()?;
        if let Some(registry) = registries.get(plugin_id).cloned() {
            return Some(registry);
        }
        let registry = Arc::new((self.registry_factories.get(plugin_id)?)());
        registries.insert(plugin_id.to_owned(), Arc::clone(&registry));
        drop(registries);
        Some(registry)
    }

    /// Return compatible routes in configured order whose native implementations are available.
    #[must_use]
    pub fn visual_routes(
        &self,
        schema: &str,
        schema_version: u32,
        producer_plugin_id: Option<&str>,
    ) -> Vec<bcode_plugin::PluginVisualAdapterRoute> {
        self.visual_adapter_config
            .order_routes(self.backend.visual_adapters(
                schema,
                schema_version,
                "tui",
                producer_plugin_id,
            ))
            .into_iter()
            .filter(|route| {
                self.registry(&route.plugin_id).is_some_and(|registry| {
                    registry.supports_visual_adapter(&route.adapter_id, &route.schema)
                }) || self.backend.has_service(
                    &route.plugin_id,
                    bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_INTERFACE_ID,
                )
            })
            .collect()
    }

    /// Return the first configured route with an available native implementation.
    #[must_use]
    pub fn visual_route(
        &self,
        schema: &str,
        schema_version: u32,
        producer_plugin_id: Option<&str>,
    ) -> Option<bcode_plugin::PluginVisualAdapterRoute> {
        self.visual_routes(schema, schema_version, producer_plugin_id)
            .into_iter()
            .next()
    }

    /// Poll bounded dynamic-adapter completions and mark affected invocations dirty.
    pub fn poll_dynamic_visuals(&self) -> bool {
        let dirty = self
            .dynamic_visuals
            .lock()
            .map_or_else(|_| BTreeSet::new(), |mut coordinator| coordinator.poll());
        let changed = !dirty.is_empty();
        for invocation_id in dirty {
            if let Ok(mut revisions) = self.visual_revisions.lock() {
                let revision = revisions.entry(invocation_id.clone()).or_default();
                *revision = revision.wrapping_add(1);
            }
            self.mark_visual_dirty(&invocation_id);
        }
        changed
    }

    /// Resolve native rows from the first ready candidate, enqueueing dynamic candidates off-frame.
    #[must_use]
    #[allow(clippy::too_many_arguments)] // Exact cache identity and renderer context stay explicit.
    pub fn routed_visual(
        &self,
        invocation_id: &str,
        payload_revision: u64,
        schema: &str,
        schema_version: u32,
        producer_plugin_id: Option<&str>,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Option<RoutedTuiVisual> {
        for route in self.visual_routes(schema, schema_version, producer_plugin_id) {
            if let Some(registry) = self.registry(&route.plugin_id)
                && let Some(rows) =
                    registry.visual_rows(&route.adapter_id, &route.schema, payload, context)
            {
                let render_mode = registry
                    .visual_render_mode(&route.adapter_id, &route.schema, payload)
                    .unwrap_or_else(|| manifest_render_mode(route.render_mode));
                let header = registry
                    .visual_transcript_header(&route.adapter_id, &route.schema, payload)
                    .unwrap_or_default();
                return Some(RoutedTuiVisual {
                    route,
                    render_mode,
                    rows,
                    header,
                });
            }
            if !self.backend.has_service(
                &route.plugin_id,
                bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_INTERFACE_ID,
            ) {
                continue;
            }
            let key = DynamicVisualKey {
                plugin_id: route.plugin_id.clone(),
                adapter_id: route.adapter_id.clone(),
                invocation_id: invocation_id.to_owned(),
                schema: schema.to_owned(),
                schema_version,
                payload_revision,
                width: context.width(),
                theme_fingerprint: context.theme().map_or(0, theme_fingerprint),
            };
            let response = {
                let dynamic = self.dynamic_visuals.lock().ok()?;
                dynamic.response(&key).cloned()
            };
            if let Some(response) = response {
                return Some(RoutedTuiVisual {
                    render_mode: serialized_render_mode(&response, route.render_mode),
                    route,
                    rows: serialized_visual_rows(&response, context.theme()),
                    header: bcode_plugin_sdk::tui::PluginTuiTranscriptHeader {
                        title: response.title,
                        timeout_ms: response.timeout_ms,
                    },
                });
            }
            let Some(mut dynamic) = self.dynamic_visuals.lock().ok() else {
                continue;
            };
            dynamic.request(DynamicVisualRequest {
                key,
                request: bcode_plugin_sdk::tui_visual::RenderTuiVisualRequest {
                    version: bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_CONTRACT_VERSION,
                    adapter_id: route.adapter_id.clone(),
                    invocation_id: invocation_id.to_owned(),
                    schema: schema.to_owned(),
                    schema_version,
                    payload: payload.clone(),
                    context: bcode_plugin_sdk::tui_visual::SerializedTuiVisualContext {
                        width: context.width(),
                        diff_layout: format!("{:?}", context.diff_layout()),
                        working_directory: context.working_directory().map(ToOwned::to_owned),
                        theme_fingerprint: context.theme().map_or(0, theme_fingerprint),
                    },
                },
            });
        }
        None
    }

    /// Return whether the host can route one visual to a native TUI adapter.
    #[must_use]
    pub fn accepts_visual(
        &self,
        producer_plugin_id: &str,
        schema: &str,
        schema_version: u32,
    ) -> bool {
        let producer = Some(producer_plugin_id);
        self.visual_route(schema, schema_version, producer)
            .is_some()
    }

    /// Return whether the routed adapter consumes bytes from one artifact reference.
    #[must_use]
    pub fn accepts_artifact_reference(
        &self,
        producer_plugin_id: &str,
        schema: &str,
        schema_version: u32,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> bool {
        let producer = Some(producer_plugin_id);
        let Some(route) = self.visual_route(schema, schema_version, producer) else {
            return false;
        };
        self.registry(&route.plugin_id).is_some_and(|registry| {
            registry.visual_accepts_artifact_reference(
                &route.adapter_id,
                &route.schema,
                reference_key,
                content_type,
            )
        }) || self.backend.has_service(
            &route.plugin_id,
            bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_INTERFACE_ID,
        )
    }

    /// Drain bounded diagnostics from retained plugin visual registries.
    pub fn drain_diagnostics(&self) -> Vec<PluginVisualDiagnostic> {
        const MAX_DIAGNOSTICS: usize = 64;
        let Ok(registries) = self.registries.lock() else {
            return Vec::new();
        };
        registries
            .iter()
            .flat_map(|(plugin_id, registry)| {
                registry
                    .drain_visual_diagnostics()
                    .into_iter()
                    .filter(|diagnostic| valid_diagnostic_name(&diagnostic.name))
                    .map(|diagnostic| PluginVisualDiagnostic {
                        plugin_id: plugin_id.clone(),
                        name: diagnostic.name,
                        value: diagnostic.value,
                    })
            })
            .take(MAX_DIAGNOSTICS)
            .collect()
    }

    /// Drain bounded generic visual-operation timings.
    pub fn drain_timings(&self) -> Vec<PluginVisualTiming> {
        self.timings
            .lock()
            .map_or_else(|_| Vec::new(), |mut timings| std::mem::take(&mut *timings))
    }

    /// Record one bounded routed visual-operation timing.
    pub fn record_visual_timing(
        &self,
        operation: &'static str,
        plugin_id: &str,
        schema: &str,
        started: Instant,
    ) {
        self.record_timing(PluginVisualTiming {
            operation,
            plugin_id: plugin_id.to_owned(),
            schema: schema.to_owned(),
            duration_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        });
    }

    fn record_timing(&self, timing: PluginVisualTiming) {
        if let Ok(mut timings) = self.timings.lock() {
            const MAX_PENDING_TIMINGS: usize = 256;
            if timings.len() < MAX_PENDING_TIMINGS {
                timings.push(timing);
            }
        }
    }

    /// Deliver opaque artifact bytes to the retained adapter selected by generic routing metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the owning adapter rejects the chunk.
    pub fn deliver_artifact_chunk(&self, chunk: &PluginTuiArtifactChunk) -> Result<bool, String> {
        let producer = Some(chunk.producer_plugin_id.as_str());
        let Some(route) = self.visual_route(&chunk.schema, chunk.schema_version, producer) else {
            return Ok(false);
        };
        if let Some(registry) = self.registry(&route.plugin_id)
            && registry.supports_visual_adapter(&route.adapter_id, &route.schema)
        {
            let started = Instant::now();
            let delivered = registry.visual_artifact_chunk(&route.adapter_id, chunk)?;
            self.record_timing(PluginVisualTiming {
                operation: "artifact_delivery",
                plugin_id: route.plugin_id.clone(),
                schema: route.schema,
                duration_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            });
            if delivered && let Ok(mut revisions) = self.visual_revisions.lock() {
                let revision = revisions.entry(chunk.tool_call_id.clone()).or_default();
                *revision = revision.wrapping_add(1);
            }
            if delivered {
                self.mark_visual_dirty(&chunk.tool_call_id);
            }
            return Ok(delivered);
        }
        let queued = self.dynamic_visuals.lock().is_ok_and(|coordinator| {
            coordinator.artifact(
                route.plugin_id,
                chunk.tool_call_id.clone(),
                bcode_plugin_sdk::tui_visual::SerializedTuiArtifactChunkRequest {
                    version: bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_CONTRACT_VERSION,
                    adapter_id: route.adapter_id,
                    chunk: serialized_artifact_chunk(chunk),
                },
            )
        });
        Ok(queued)
    }
}

#[cfg(test)]
fn test_tui_extensions() -> &'static [bcode_plugin_sdk::tui::StaticPluginTuiExtension] {
    static EXTENSIONS: std::sync::OnceLock<Vec<bcode_plugin_sdk::tui::StaticPluginTuiExtension>> =
        std::sync::OnceLock::new();
    EXTENSIONS.get_or_init(|| {
        vec![
            bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
                "bcode.filesystem",
                bcode_filesystem_plugin::filesystem_tui_registry,
            ),
            bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
                "bcode.git",
                bcode_git_plugin::git_tui_registry,
            ),
            bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
                "bcode.question",
                bcode_question_plugin::question_tui_registry,
            ),
            bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
                "bcode.shell",
                bcode_shell_plugin::shell_tui_registry,
            ),
            bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
                "bcode.vim-edit",
                bcode_vim_edit_plugin::vim_edit_tui_registry,
            ),
        ]
    })
}

#[cfg(not(test))]
const fn test_tui_extensions() -> &'static [bcode_plugin_sdk::tui::StaticPluginTuiExtension] {
    &[]
}

fn serialized_artifact_chunk(
    chunk: &PluginTuiArtifactChunk,
) -> bcode_plugin_sdk::tui_visual::SerializedTuiArtifactChunk {
    bcode_plugin_sdk::tui_visual::SerializedTuiArtifactChunk {
        tool_call_id: chunk.tool_call_id.clone(),
        artifact_id: chunk.artifact_id.clone(),
        reference_key: chunk.reference_key.clone(),
        producer_plugin_id: chunk.producer_plugin_id.clone(),
        schema: chunk.schema.clone(),
        schema_version: chunk.schema_version,
        content_type: chunk.content_type.clone(),
        offset: chunk.offset,
        total_bytes: chunk.total_bytes,
        revision: chunk.revision,
        finalized: chunk.finalized,
        bytes: chunk.bytes.clone(),
    }
}

const fn manifest_render_mode(
    mode: bcode_plugin::PluginVisualAdapterRenderMode,
) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
    match mode {
        bcode_plugin::PluginVisualAdapterRenderMode::Inline => {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::Inline
        }
        bcode_plugin::PluginVisualAdapterRenderMode::TranscriptBlock => {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
        }
        bcode_plugin::PluginVisualAdapterRenderMode::FullBlock => {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::FullBlock
        }
    }
}

fn serialized_render_mode(
    response: &bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse,
    default: bcode_plugin::PluginVisualAdapterRenderMode,
) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
    match response.render_mode.as_str() {
        "inline" => bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::Inline,
        "transcript_block" => bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock,
        "full_block" => bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::FullBlock,
        _ => manifest_render_mode(default),
    }
}

fn theme_fingerprint(theme: bcode_plugin_sdk::tui::PluginTuiTheme) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("{theme:?}").hash(&mut hasher);
    hasher.finish()
}

fn serialized_visual_rows(
    response: &bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse,
    theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>,
) -> Vec<bmux_tui::prelude::Line> {
    use bcode_plugin_sdk::tui_visual::{
        SerializedTuiColor, SerializedTuiModifier, SerializedTuiStyleRole,
    };
    use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
    response
        .rows
        .iter()
        .map(|row| {
            Line::from_spans(
                row.spans
                    .iter()
                    .map(|span| {
                        let color = match span.foreground {
                            SerializedTuiColor::Reset => Color::Default,
                            SerializedTuiColor::Black => Color::Black,
                            SerializedTuiColor::Red => Color::Red,
                            SerializedTuiColor::Green => Color::Green,
                            SerializedTuiColor::Yellow => Color::Yellow,
                            SerializedTuiColor::Blue => Color::Blue,
                            SerializedTuiColor::Magenta => Color::Magenta,
                            SerializedTuiColor::Cyan => Color::Cyan,
                            SerializedTuiColor::Gray | SerializedTuiColor::White => Color::White,
                            SerializedTuiColor::DarkGray => Color::BrightBlack,
                            SerializedTuiColor::LightRed => Color::BrightRed,
                            SerializedTuiColor::LightGreen => Color::BrightGreen,
                            SerializedTuiColor::LightYellow => Color::BrightYellow,
                            SerializedTuiColor::LightBlue => Color::BrightBlue,
                            SerializedTuiColor::LightMagenta => Color::BrightMagenta,
                            SerializedTuiColor::LightCyan => Color::BrightCyan,
                        };
                        let compatibility_style = Style::new().fg(color);
                        let role_style = span.role.and_then(|role| {
                            theme.map(|theme| match role {
                                SerializedTuiStyleRole::Text => theme.text,
                                SerializedTuiStyleRole::Muted => theme.muted,
                                SerializedTuiStyleRole::Accent | SerializedTuiStyleRole::Info => {
                                    theme.focused
                                }
                                SerializedTuiStyleRole::Success
                                | SerializedTuiStyleRole::DiffAdded => theme.diff.added,
                                SerializedTuiStyleRole::Warning
                                | SerializedTuiStyleRole::DiffHunk => theme.diff.hunk,
                                SerializedTuiStyleRole::Error
                                | SerializedTuiStyleRole::DiffRemoved => theme.diff.removed,
                            })
                        });
                        let mut style = role_style.unwrap_or(compatibility_style);
                        for modifier in &span.modifiers {
                            style = style.add_modifier(match modifier {
                                SerializedTuiModifier::Bold => Modifier::BOLD,
                                SerializedTuiModifier::Dim => Modifier::DIM,
                                SerializedTuiModifier::Italic => Modifier::ITALIC,
                                SerializedTuiModifier::Underlined => Modifier::UNDERLINE,
                            });
                        }
                        Span::styled(span.text.clone(), style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn valid_diagnostic_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')
        })
}

/// Return a newly constructed platform-owned TUI registry for an enabled bundled plugin.
///
/// Long-lived visual rendering must acquire registries through [`PluginTuiPresentation`]. Fresh
/// registries remain appropriate for opening independent interactive surface instances.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn tui_registry(plugin_id: &str) -> Option<PluginTuiRegistry> {
    let registry = super::bundled_tui_extensions()
        .into_iter()
        .find(|extension| extension.plugin_id() == plugin_id)
        .map(bcode_plugin_sdk::tui::StaticPluginTuiExtension::registry);
    #[cfg(test)]
    let registry = registry.or_else(|| match plugin_id {
        "bcode.filesystem" => Some(bcode_filesystem_plugin::filesystem_tui_registry()),
        "bcode.git" => Some(bcode_git_plugin::git_tui_registry()),
        "bcode.question" => Some(bcode_question_plugin::question_tui_registry()),
        "bcode.shell" => Some(bcode_shell_plugin::shell_tui_registry()),
        "bcode.vim-edit" => Some(bcode_vim_edit_plugin::vim_edit_tui_registry()),
        _ => None,
    });
    registry
}

/// Load the default local plugin host for TUI client-side services.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_host_with_static_bundled(
    selection: &bcode_plugin::PluginSelection,
    static_plugins: &[StaticBundledPlugin],
    extensions: &[bcode_plugin_sdk::tui::StaticPluginTuiExtension],
) -> Result<PluginHost, PluginLoadError> {
    if let Ok(host) = PluginHost::load_defaults_with_static_bundled(selection, static_plugins) {
        Ok(host)
    } else {
        let selected = bcode_plugin::filter_selected_static_plugins(static_plugins, selection)?;
        let visual_plugins = selected
            .into_iter()
            .filter(|(manifest, _)| {
                extensions
                    .iter()
                    .find(|extension| extension.plugin_id() == manifest.id)
                    .is_some_and(|extension| {
                        let registry = extension.registry();
                        manifest.visual_adapters.iter().any(|adapter| {
                            (adapter.surfaces.is_empty()
                                || adapter.surfaces.iter().any(|surface| surface == "tui"))
                                && registry.supports_visual_adapter(&adapter.id, &adapter.schema)
                        })
                    })
            })
            .collect::<Vec<_>>();
        Ok(PluginHost::load_static_plugins_best_effort(&visual_plugins))
    }
}

/// Load persistent presentation state for TUI client-side visual adapters.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_presentation_with_static_bundled(
    selection: &bcode_plugin::PluginSelection,
    visual_adapter_config: bcode_config::TuiVisualAdapterConfig,
    static_plugins: &[StaticBundledPlugin],
    extensions: &[bcode_plugin_sdk::tui::StaticPluginTuiExtension],
) -> Result<PluginTuiPresentation, PluginLoadError> {
    load_default_host_with_static_bundled(selection, static_plugins, extensions).map(|host| {
        PluginTuiPresentation::with_runtime_config_and_extensions(
            host.into(),
            visual_adapter_config,
            extensions,
        )
    })
}

/// Load the default local plugin runtime for TUI client-side services.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_runtime_with_static_bundled(
    static_plugins: &[StaticBundledPlugin],
) -> Result<PluginRuntimeHost, PluginLoadError> {
    PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        static_plugins,
    )
}

/// Open a native TUI surface from a platform-owned registry.
///
/// # Errors
///
/// Returns an error when the plugin is not loaded, has no native TUI registry, or the surface
/// factory fails to open the surface.
pub async fn open_plugin_tui_surface(
    runtime: &PluginRuntimeHost,
    plugin_id: &str,
    surface_kind: &str,
    request: PluginTuiSurfaceOpenRequest,
) -> Result<BoxedPluginTuiSurface, PluginLoadError> {
    if !runtime
        .plugin_ids()
        .iter()
        .any(|loaded| loaded == plugin_id)
    {
        return Err(PluginLoadError::PluginNotLoaded(plugin_id.to_string()));
    }
    let registry = tui_registry(plugin_id)
        .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.to_string()))?;
    if runtime
        .registry()
        .tui_surface(plugin_id, surface_kind)
        .is_none()
    {
        return Err(PluginLoadError::TuiSurfaceOpen {
            plugin_id: plugin_id.to_string(),
            message: format!("plugin does not declare TUI surface kind '{surface_kind}'"),
        });
    }
    registry
        .open(surface_kind, request)
        .await
        .map_err(|error| PluginLoadError::TuiSurfaceOpen {
            plugin_id: plugin_id.to_string(),
            message: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::prelude::Line;

    const DYNAMIC_TEST_MANIFEST: &str = r#"
id = "test.dynamic-visual"
name = "Dynamic Visual Test Plugin"
version = "0.0.1"

[[services]]
interface_id = "bcode.tui-visual-adapter/v1"
name = "Dynamic TUI Visual Adapter"
class = "service"

[[visual_adapters]]
id = "success"
schema = "bcode.shell.run"
min_schema_version = 1
max_schema_version = 1
service_interface_id = "bcode.tui-visual-adapter/v1"
surfaces = ["tui"]
priority = 100
producer_default = false
render_mode = "transcript_block"

[[visual_adapters]]
id = "malformed"
schema = "bcode.shell.run"
min_schema_version = 1
max_schema_version = 1
service_interface_id = "bcode.tui-visual-adapter/v1"
surfaces = ["tui"]
priority = 99
producer_default = false
render_mode = "transcript_block"

[[visual_adapters]]
id = "failure"
schema = "bcode.shell.run"
min_schema_version = 1
max_schema_version = 1
service_interface_id = "bcode.tui-visual-adapter/v1"
surfaces = ["tui"]
priority = 98
producer_default = false
render_mode = "transcript_block"

[[visual_adapters]]
id = "timeout"
schema = "bcode.shell.run"
min_schema_version = 1
max_schema_version = 1
service_interface_id = "bcode.tui-visual-adapter/v1"
surfaces = ["tui"]
priority = 97
producer_default = false
render_mode = "transcript_block"

[concurrency]
type = "concurrent"

[runtime]
type = "native"
abi_version = 3
library = "libdynamic_visual_test.dylib"
"#;

    #[derive(Default)]
    struct DynamicVisualTestPlugin {
        artifact_revisions: BTreeMap<String, u64>,
    }

    impl bcode_plugin_sdk::RustPlugin for DynamicVisualTestPlugin {
        fn invoke_service(
            &mut self,
            context: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            match context.request.operation.as_str() {
                bcode_plugin_sdk::tui_visual::OP_RENDER_TUI_VISUAL => {
                    let Ok(request) = context
                        .request
                        .payload_json::<bcode_plugin_sdk::tui_visual::RenderTuiVisualRequest>(
                    ) else {
                        return bcode_plugin_sdk::ServiceResponse::error(
                            "invalid_request",
                            "render request did not decode",
                        );
                    };
                    match request.adapter_id.as_str() {
                        "failure" => bcode_plugin_sdk::ServiceResponse::error(
                            "test_failure",
                            "dynamic adapter failed",
                        ),
                        "malformed" => bcode_plugin_sdk::ServiceResponse::json(
                            &bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse {
                                version: 999,
                                render_mode: "transcript_block".to_owned(),
                                title: None,
                                timeout_ms: None,
                                rows: Vec::new(),
                            },
                        )
                        .expect("encode malformed response"),
                        "timeout" => {
                            while !context.cancellation.is_cancelled() {
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            bcode_plugin_sdk::ServiceResponse::error(
                                "cancelled",
                                "dynamic adapter was cancelled",
                            )
                        }
                        "success" => {
                            let artifact_revision = self
                                .artifact_revisions
                                .get(&request.invocation_id)
                                .copied()
                                .unwrap_or_default();
                            bcode_plugin_sdk::ServiceResponse::json(
                                &bcode_plugin_sdk::tui_visual::RenderTuiVisualResponse {
                                    version:
                                        bcode_plugin_sdk::tui_visual::TUI_VISUAL_ADAPTER_CONTRACT_VERSION,
                                    render_mode: "transcript_block".to_owned(),
                                    title: Some("Dynamic shell".to_owned()),
                                    timeout_ms: Some(321),
                                    rows: vec![bcode_plugin_sdk::tui_visual::SerializedTuiRow {
                                        spans: vec![bcode_plugin_sdk::tui_visual::SerializedTuiSpan {
                                            text: format!(
                                                "dynamic:{}:artifact-{artifact_revision}",
                                                request.adapter_id
                                            ),
                                            role: Some(
                                                bcode_plugin_sdk::tui_visual::SerializedTuiStyleRole::Success,
                                            ),
                                            foreground:
                                                bcode_plugin_sdk::tui_visual::SerializedTuiColor::Green,
                                            modifiers: vec![
                                                bcode_plugin_sdk::tui_visual::SerializedTuiModifier::Bold,
                                            ],
                                        }],
                                    }],
                                },
                            )
                            .expect("encode dynamic response")
                        }
                        _ => bcode_plugin_sdk::ServiceResponse::error(
                            "unknown_adapter",
                            "adapter id was not exact",
                        ),
                    }
                }
                bcode_plugin_sdk::tui_visual::OP_DELIVER_TUI_VISUAL_ARTIFACT => {
                    let Ok(request) = context
                        .request
                        .payload_json::<bcode_plugin_sdk::tui_visual::SerializedTuiArtifactChunkRequest>(
                    ) else {
                        return bcode_plugin_sdk::ServiceResponse::error(
                            "invalid_request",
                            "artifact request did not decode",
                        );
                    };
                    let revision = self
                        .artifact_revisions
                        .entry(request.chunk.tool_call_id)
                        .or_default();
                    *revision = revision.wrapping_add(1);
                    bcode_plugin_sdk::ServiceResponse::json(&serde_json::json!({"accepted": true}))
                        .expect("encode artifact response")
                }
                _ => bcode_plugin_sdk::ServiceResponse::error(
                    "unsupported_operation",
                    "unsupported dynamic visual operation",
                ),
            }
        }
    }

    fn dynamic_visual_test_plugin() -> StaticBundledPlugin {
        StaticBundledPlugin::new(
            DYNAMIC_TEST_MANIFEST,
            bcode_plugin_sdk::static_plugin_vtable!(DynamicVisualTestPlugin, DYNAMIC_TEST_MANIFEST),
        )
    }

    fn dynamic_test_presentation(
        preferred_adapter: &str,
        disabled_adapters: &[&str],
    ) -> PluginTuiPresentation {
        let bundled = [
            dynamic_visual_test_plugin(),
            StaticBundledPlugin::new(
                include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
                bcode_shell_plugin::static_plugin(),
            ),
        ];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("select dynamic visual test plugins");
        let runtime: PluginRuntimeHost = PluginHost::load_static_plugins(&selected)
            .expect("load dynamic visual test plugins")
            .into();
        PluginTuiPresentation::with_runtime_config_and_extensions(
            runtime,
            bcode_config::TuiVisualAdapterConfig {
                preferred: vec![preferred_adapter.to_owned()],
                disabled: disabled_adapters
                    .iter()
                    .map(|adapter| (*adapter).to_owned())
                    .collect(),
            },
            test_tui_extensions(),
        )
    }

    fn test_plugin_theme(fingerprint: u64) -> bcode_plugin_sdk::tui::PluginTuiTheme {
        use bmux_tui::prelude::{Color, Modifier, Style};

        let style = Style::new();
        let added = Style::new().fg(Color::Green);
        let removed = Style::new().fg(Color::Red);
        let hunk = Style::new().fg(Color::Yellow);
        let syntax = bcode_plugin_sdk::tui::PluginTuiSyntaxColor::rgb(
            u8::try_from(fingerprint).unwrap_or(u8::MAX),
            2,
            3,
        );
        bcode_plugin_sdk::tui::PluginTuiTheme {
            canvas: style,
            text: style.fg(Color::White),
            muted: style.fg(Color::BrightBlack),
            border: style,
            focused: style.fg(Color::Cyan),
            selection: style.add_modifier(Modifier::REVERSED),
            source: bcode_plugin_sdk::tui::PluginTuiSourceTheme {
                source: style,
                border: style,
                gutter: style,
                truncated: style,
            },
            diff: bcode_plugin_sdk::tui::PluginTuiDiffTheme {
                text: style,
                muted: style,
                title: style,
                label: style,
                added,
                removed,
                hunk,
                added_row: style,
                removed_row: style,
                added_emphasis: style,
                removed_emphasis: style,
            },
            syntax: bcode_plugin_sdk::tui::PluginTuiSyntaxTheme {
                text: syntax,
                comment: syntax,
                keyword: syntax,
                function: syntax,
                variable: syntax,
                string: syntax,
                number: syntax,
                type_name: syntax,
                operator: syntax,
                punctuation: syntax,
            },
        }
    }

    #[test]
    fn serialized_semantic_role_precedes_legacy_color_with_readable_fallback() {
        use bcode_plugin_sdk::tui_visual::{
            RenderTuiVisualResponse, SerializedTuiColor, SerializedTuiRow, SerializedTuiSpan,
            SerializedTuiStyleRole,
        };
        use bmux_tui::prelude::{Color, Style};

        let response = RenderTuiVisualResponse {
            version: 2,
            render_mode: "inline".to_owned(),
            title: None,
            timeout_ms: None,
            rows: vec![SerializedTuiRow {
                spans: vec![SerializedTuiSpan {
                    text: "status".to_owned(),
                    role: Some(SerializedTuiStyleRole::Success),
                    foreground: SerializedTuiColor::Red,
                    modifiers: Vec::new(),
                }],
            }],
        };
        let themed = serialized_visual_rows(&response, Some(test_plugin_theme(7)));
        assert_eq!(themed[0].spans[0].style.fg, Some(Color::Green));

        let fallback = serialized_visual_rows(&response, None);
        assert_eq!(fallback[0].spans[0].style, Style::new().fg(Color::Red));
    }

    #[test]
    fn serialized_visual_cache_key_includes_theme_identity() {
        let presentation = dynamic_test_presentation("success", &[]);
        let key = |theme_fingerprint| DynamicVisualKey {
            plugin_id: "test.dynamic-visual".to_owned(),
            adapter_id: "success".to_owned(),
            invocation_id: "call-theme".to_owned(),
            schema: "bcode.shell.run".to_owned(),
            schema_version: 1,
            payload_revision: 0,
            width: 80,
            theme_fingerprint,
        };
        assert_ne!(key(1), key(2));
        let context = |fingerprint| {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                None,
            )
            .with_theme(test_plugin_theme(fingerprint))
        };

        let first = dynamic_test_visual_with_context(&presentation, "call-theme", &context(1));
        if first.is_none() {
            wait_for_dynamic_completion(&presentation);
        }
        assert!(
            dynamic_test_visual_with_context(&presentation, "call-theme", &context(1)).is_some()
        );
        let changed = dynamic_test_visual_with_context(&presentation, "call-theme", &context(2));
        if let Some(visual) = changed {
            assert_eq!(visual.route.plugin_id, "bcode.shell");
        }
    }

    fn dynamic_test_visual_with_context(
        presentation: &PluginTuiPresentation,
        invocation_id: &str,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Option<RoutedTuiVisual> {
        presentation.routed_visual(
            invocation_id,
            0,
            "bcode.shell.run",
            1,
            None,
            &serde_json::json!({
                "arguments": {"command": "printf hello", "cwd": "/tmp/project"}
            }),
            context,
        )
    }

    fn dynamic_test_visual(
        presentation: &PluginTuiPresentation,
        invocation_id: &str,
    ) -> Option<RoutedTuiVisual> {
        presentation.routed_visual(
            invocation_id,
            0,
            "bcode.shell.run",
            1,
            Some("bcode.shell"),
            &serde_json::json!({
                "arguments": {
                    "command": "printf hello",
                    "cwd": "/tmp/project"
                }
            }),
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                Some(std::path::PathBuf::from("/tmp/project")),
            ),
        )
    }

    fn routed_text(visual: &RoutedTuiVisual) -> String {
        visual
            .rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect()
    }

    fn wait_for_dynamic_completion(presentation: &PluginTuiPresentation) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if presentation.poll_dynamic_visuals() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("dynamic visual completion timed out");
    }

    fn wait_for_dynamic_text(
        presentation: &PluginTuiPresentation,
        invocation_id: &str,
        expected: &str,
    ) -> RoutedTuiVisual {
        // Aggregate workspace tests can heavily contend the single adapter worker. Keep this
        // bounded while allowing an artifact completion followed by its replacement render.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            let _ = presentation.poll_dynamic_visuals();
            if let Some(visual) = dynamic_test_visual(presentation, invocation_id)
                && routed_text(&visual) == expected
            {
                return visual;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("dynamic visual text did not converge to {expected}");
    }

    fn hello_dynamic_library_path() -> std::path::PathBuf {
        let executable = std::env::current_exe().expect("current test executable path");
        let directory = executable.parent().expect("test executable parent");
        let prefix = format!("{}bcode_hello_plugin", std::env::consts::DLL_PREFIX);
        std::fs::read_dir(directory)
            .expect("test dependency directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(&prefix) && name.ends_with(std::env::consts::DLL_SUFFIX)
                    })
            })
            .expect("hello plugin dynamic library")
    }

    fn discovered_hello_presentation() -> PluginTuiPresentation {
        let root = tempfile::tempdir().expect("plugin discovery root");
        let plugin_dir = root.path().join("hello");
        std::fs::create_dir_all(&plugin_dir).expect("plugin directory");
        let manifest_path = plugin_dir.join("bcode-plugin.toml");
        std::fs::write(
            &manifest_path,
            include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
        )
        .expect("plugin manifest");
        let library = hello_dynamic_library_path();
        std::fs::copy(
            &library,
            plugin_dir.join(
                library
                    .file_name()
                    .expect("hello plugin dynamic library name"),
            ),
        )
        .expect("plugin library");
        let registered = bcode_plugin::discover_plugins_in_roots(&[root.path().to_path_buf()])
            .expect("discover user plugin");
        let runtime: PluginRuntimeHost = PluginHost::load_registered_plugins(&registered)
            .expect("load discovered user plugin")
            .into();
        PluginTuiPresentation::with_runtime_config_and_extensions(
            runtime,
            bcode_config::TuiVisualAdapterConfig {
                preferred: vec!["example.hello/hello-shell-request-card".to_owned()],
                disabled: BTreeSet::new(),
            },
            &[],
        )
    }

    #[tokio::test]
    async fn discovered_dynamic_user_adapter_renders_shell_request_and_result() {
        let presentation = discovered_hello_presentation();
        for (schema, adapter_id) in [
            ("bcode.tool.request.shell.run", "hello-shell-request-card"),
            ("bcode.shell.run", "hello-shell-card"),
        ] {
            let invocation_id = format!("call-{adapter_id}");
            let initial = presentation.routed_visual(
                &invocation_id,
                1,
                schema,
                1,
                Some("bcode.shell"),
                &serde_json::json!({"command": "printf hello"}),
                &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                    80,
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                    None,
                ),
            );
            assert!(initial.is_none(), "dynamic work must be off-frame");
            wait_for_dynamic_completion(&presentation);
            let routed = presentation
                .routed_visual(
                    &invocation_id,
                    1,
                    schema,
                    1,
                    Some("bcode.shell"),
                    &serde_json::json!({"command": "printf hello"}),
                    &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                        80,
                        bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                        None,
                    ),
                )
                .expect("discovered dynamic visual");
            assert_eq!(routed.route.plugin_id, "example.hello");
            assert_eq!(routed.route.adapter_id, adapter_id);
            assert_eq!(routed.header.title.as_deref(), Some("Hello user shell"));
            assert_eq!(
                routed_text(&routed),
                format!("hello dynamic {invocation_id}")
            );
        }
    }

    #[test]
    fn preferred_dynamic_adapter_overrides_producer_native_adapter_after_completion() {
        let presentation = dynamic_test_presentation(
            "test.dynamic-visual/success",
            &[
                "test.dynamic-visual/malformed",
                "test.dynamic-visual/failure",
                "test.dynamic-visual/timeout",
            ],
        );

        let initial = dynamic_test_visual(&presentation, "call-success")
            .expect("producer-native fallback while dynamic visual runs");
        assert_eq!(initial.route.plugin_id, "bcode.shell");
        wait_for_dynamic_completion(&presentation);

        let routed =
            dynamic_test_visual(&presentation, "call-success").expect("completed dynamic visual");
        let dynamic_cache = presentation
            .dynamic_visuals
            .lock()
            .expect("dynamic visual cache");
        assert_eq!(
            routed.route.adapter_reference(),
            "test.dynamic-visual/success",
            "cached dynamic results: {:?}",
            dynamic_cache.cache
        );
        drop(dynamic_cache);
        assert_eq!(routed.header.title.as_deref(), Some("Dynamic shell"));
        assert_eq!(routed.header.timeout_ms, Some(321));
        assert_eq!(routed_text(&routed), "dynamic:success:artifact-0");
    }

    #[test]
    fn malformed_failed_and_timed_out_dynamic_adapters_fall_through() {
        for adapter_id in ["malformed", "failure", "timeout"] {
            let disabled = ["success", "malformed", "failure", "timeout"]
                .into_iter()
                .filter(|candidate| *candidate != adapter_id)
                .map(|candidate| format!("test.dynamic-visual/{candidate}"))
                .collect::<Vec<_>>();
            let disabled = disabled.iter().map(String::as_str).collect::<Vec<_>>();
            let presentation =
                dynamic_test_presentation(&format!("test.dynamic-visual/{adapter_id}"), &disabled);

            let initial = dynamic_test_visual(&presentation, &format!("call-{adapter_id}"))
                .expect("native fallback while dynamic visual runs");
            assert_eq!(initial.route.plugin_id, "bcode.shell");
            wait_for_dynamic_completion(&presentation);

            let fallback = dynamic_test_visual(&presentation, &format!("call-{adapter_id}"))
                .expect("native fallback after dynamic failure");
            assert_eq!(fallback.route.plugin_id, "bcode.shell", "{adapter_id}");
        }
    }

    #[test]
    fn dynamic_visual_cache_and_pending_work_remain_bounded() {
        let presentation = dynamic_test_presentation(
            "test.dynamic-visual/success",
            &[
                "test.dynamic-visual/malformed",
                "test.dynamic-visual/failure",
                "test.dynamic-visual/timeout",
            ],
        );
        for index in 0..(MAX_DYNAMIC_VISUAL_CACHE_ENTRIES + 64) {
            let _ = dynamic_test_visual(&presentation, &format!("call-bound-{index}"));
        }
        let coordinator = presentation
            .dynamic_visuals
            .lock()
            .expect("dynamic coordinator");
        assert!(
            coordinator
                .cache
                .len()
                .saturating_add(coordinator.requested.len())
                <= MAX_DYNAMIC_VISUAL_CACHE_ENTRIES
        );
        assert!(coordinator.requested.len() <= MAX_DYNAMIC_VISUAL_CACHE_ENTRIES);
        drop(coordinator);
    }

    #[test]
    fn dynamic_artifact_update_invalidates_only_affected_invocation() {
        let presentation = dynamic_test_presentation(
            "test.dynamic-visual/success",
            &[
                "test.dynamic-visual/malformed",
                "test.dynamic-visual/failure",
                "test.dynamic-visual/timeout",
            ],
        );
        for invocation_id in ["call-owner", "call-other"] {
            let _ = dynamic_test_visual(&presentation, invocation_id);
            wait_for_dynamic_completion(&presentation);
            let visual =
                dynamic_test_visual(&presentation, invocation_id).expect("cached dynamic visual");
            assert_eq!(routed_text(&visual), "dynamic:success:artifact-0");
            let _ = presentation.drain_dirty_visuals();
        }

        assert!(
            presentation
                .deliver_artifact_chunk(&PluginTuiArtifactChunk {
                    tool_call_id: "call-owner".to_owned(),
                    artifact_id: "artifact-owner".to_owned(),
                    reference_key: "shell_recording".to_owned(),
                    producer_plugin_id: "bcode.shell".to_owned(),
                    schema: "bcode.shell.run".to_owned(),
                    schema_version: 1,
                    content_type: Some(
                        "application/x-bcode-shell-recording; version=3".to_owned(),
                    ),
                    offset: 0,
                    total_bytes: 3,
                    revision: 1,
                    finalized: false,
                    bytes: b"abc".to_vec(),
                })
                .expect("queue dynamic artifact")
        );
        wait_for_dynamic_completion(&presentation);
        assert_eq!(presentation.visual_revision("call-owner"), 2);
        assert_eq!(presentation.visual_revision("call-other"), 1);
        assert_eq!(
            presentation.drain_dirty_visuals(),
            BTreeSet::from(["call-owner".to_owned()])
        );

        let during_refresh = dynamic_test_visual(&presentation, "call-owner")
            .expect("native fallback during dynamic refresh");
        assert_eq!(during_refresh.route.plugin_id, "bcode.shell");
        let refreshed =
            wait_for_dynamic_text(&presentation, "call-owner", "dynamic:success:artifact-1");
        assert_eq!(routed_text(&refreshed), "dynamic:success:artifact-1");
        let unchanged =
            dynamic_test_visual(&presentation, "call-other").expect("unaffected cached visual");
        assert_eq!(routed_text(&unchanged), "dynamic:success:artifact-0");
    }

    #[derive(Default)]
    struct StatefulTestAdapter {
        bytes: Mutex<Vec<u8>>,
        chunks: Mutex<Vec<(String, String, u64, u64)>>,
    }

    impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for StatefulTestAdapter {
        fn supports(&self, kind: &str) -> bool {
            kind == "bcode.shell.run"
        }

        fn accepts_artifact_reference(
            &self,
            kind: &str,
            reference_key: &str,
            content_type: Option<&str>,
        ) -> bool {
            kind == "bcode.shell.run"
                && reference_key == "shell_recording"
                && content_type == Some("application/x-bcode-shell-recording; version=3")
        }

        fn artifact_chunk(&self, chunk: &PluginTuiArtifactChunk) -> Result<(), String> {
            self.bytes
                .lock()
                .map_err(|_| "test adapter state poisoned".to_owned())?
                .extend_from_slice(&chunk.bytes);
            self.chunks
                .lock()
                .map_err(|_| "test adapter chunk state poisoned".to_owned())?
                .push((
                    chunk.artifact_id.clone(),
                    chunk.reference_key.clone(),
                    chunk.offset,
                    chunk.revision,
                ));
            Ok(())
        }

        fn rows(
            &self,
            _kind: &str,
            _payload: &serde_json::Value,
            _context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
        ) -> Vec<Line> {
            let text = self
                .bytes
                .lock()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            vec![Line::from(text)]
        }
    }

    fn test_presentation() -> PluginTuiPresentation {
        let bundled = [StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("select test plugin");
        let host = PluginHost::load_static_plugins(&selected).expect("load test plugin");
        PluginTuiPresentation::new(host)
    }

    fn deliver_shell_recording_commit(
        presentation: &PluginTuiPresentation,
        commit: &bcode_shell_plugin::recording::ShellRecordingCommit,
        previous_bytes: u64,
        revision: u64,
    ) -> u64 {
        let mut bytes = std::fs::read(&commit.path).expect("recording prefix");
        bytes.truncate(usize::try_from(commit.committed_bytes).expect("committed length"));
        bytes.drain(..usize::try_from(previous_bytes).expect("offset"));
        assert!(
            presentation
                .deliver_artifact_chunk(&PluginTuiArtifactChunk {
                    tool_call_id: "shell-call".to_owned(),
                    artifact_id: "shell-artifact".to_owned(),
                    reference_key: "shell_recording".to_owned(),
                    producer_plugin_id: "bcode.shell".to_owned(),
                    schema: "bcode.shell.run".to_owned(),
                    schema_version: 1,
                    content_type: Some(
                        "application/x-bcode-shell-recording; version=3".to_owned(),
                    ),
                    offset: previous_bytes,
                    total_bytes: commit.committed_bytes,
                    revision,
                    finalized: commit.finalized,
                    bytes,
                })
                .expect("deliver growing artifact chunk")
        );
        assert_eq!(presentation.visual_revision("shell-call"), revision);
        assert_eq!(
            presentation.drain_dirty_visuals(),
            BTreeSet::from(["shell-call".to_owned()])
        );
        commit.committed_bytes
    }

    #[test]
    fn dirty_visual_drain_is_bounded_and_retains_overflow_for_next_tick() {
        let presentation = test_presentation();
        for index in 0..100 {
            presentation.bump_visual_revision_for_test(&format!("call-{index:03}"));
        }
        let first = presentation.drain_dirty_visuals_bounded(64);
        assert_eq!(first.len(), 64);
        assert_eq!(first.first().map(String::as_str), Some("call-000"));
        assert_eq!(first.last().map(String::as_str), Some("call-063"));
        let second = presentation.drain_dirty_visuals_bounded(64);
        assert_eq!(second.len(), 36);
        assert_eq!(second.first().map(String::as_str), Some("call-064"));
        assert_eq!(second.last().map(String::as_str), Some("call-099"));
        assert!(presentation.drain_dirty_visuals_bounded(64).is_empty());
    }

    #[test]
    fn one_visual_revision_changes_only_its_transcript_signature() {
        let presentation = test_presentation();
        let first = crate::transcript::tool_request_item(
            "call-one",
            Some("bcode.shell"),
            "shell",
            "{}",
            None,
        );
        let second = crate::transcript::tool_request_item(
            "call-two",
            Some("bcode.shell"),
            "shell",
            "{}",
            None,
        );
        let first_before =
            crate::transcript_projection::test_layout_signature(&first, 80, Some(&presentation));
        let second_before =
            crate::transcript_projection::test_layout_signature(&second, 80, Some(&presentation));

        presentation.bump_visual_revision_for_test("call-one");

        let first_after =
            crate::transcript_projection::test_layout_signature(&first, 80, Some(&presentation));
        let second_after =
            crate::transcript_projection::test_layout_signature(&second, 80, Some(&presentation));
        assert_ne!(first_before, first_after);
        assert_eq!(second_before, second_after);
    }

    #[test]
    #[ignore = "manual deterministic performance baseline"]
    fn targeted_visual_update_transcript_baseline_report() {
        for transcript_len in [10_usize, 500, 2_000] {
            let presentation = test_presentation();
            let items = (0..transcript_len)
                .map(|index| {
                    crate::transcript::tool_request_item(
                        &format!("call-{index}"),
                        Some("bcode.shell"),
                        "shell",
                        "{}",
                        None,
                    )
                })
                .collect::<Vec<_>>();
            let mut cache = crate::transcript_layout::TranscriptLayoutCache::default();
            cache.sync(crate::transcript_layout::TranscriptLayoutSpec {
                width: 80,
                fingerprint: crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "baseline-initial".to_owned(),
                ),
                structural_fingerprint: crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "baseline-structure".to_owned(),
                ),
                transcript_len,
                pending_len: 0,
                transcript_signature: |index| {
                    crate::transcript_projection::test_layout_signature(
                        &items[index],
                        80,
                        Some(&presentation),
                    )
                },
                transcript_rows: |index| vec![Line::from(format!("row-{index}"))],
                transcript_invocation_id: |index| Some(format!("call-{index}")),
                pending_signature: |index| {
                    crate::transcript_layout::TranscriptLayoutSignature::new(format!(
                        "pending-{index}"
                    ))
                },
                pending_rows: |_| Vec::new(),
                history_banner_signature: || None,
                history_banner_rows: Vec::new,
                reset: || false,
            });
            presentation.bump_visual_revision_for_test("call-0");
            let started = Instant::now();
            let stats = cache.sync_visuals(
                crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "baseline-updated".to_owned(),
                ),
                &std::collections::BTreeSet::from(["call-0".to_owned()]),
                |index| {
                    crate::transcript_projection::test_layout_signature(
                        &items[index],
                        80,
                        Some(&presentation),
                    )
                },
                |index| vec![Line::from(format!("row-{index}"))],
            );
            println!(
                "BCODE_PERF_CASE {}",
                serde_json::json!({
                    "domain": "transcript_visual_update",
                    "transcript_entries": transcript_len,
                    "entries_scanned": stats.entries_scanned,
                    "signatures_changed": stats.signatures_changed,
                    "entries_rebuilt": stats.entries_rebuilt,
                    "rows_regenerated": stats.rows_regenerated,
                    "sync_us": stats.duration_micros,
                    "wall_us": u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                })
            );
        }
    }

    #[test]
    fn one_visual_update_rebuilds_only_its_entry_across_transcript_sizes() {
        for transcript_len in [10_usize, 500, 2_000] {
            let presentation = test_presentation();
            let items = (0..transcript_len)
                .map(|index| {
                    crate::transcript::tool_request_item(
                        &format!("call-{index}"),
                        Some("bcode.shell"),
                        "shell",
                        "{}",
                        None,
                    )
                })
                .collect::<Vec<_>>();
            let mut cache = crate::transcript_layout::TranscriptLayoutCache::default();
            let initial = cache.sync(crate::transcript_layout::TranscriptLayoutSpec {
                width: 80,
                fingerprint: crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "initial".to_owned(),
                ),
                structural_fingerprint: crate::transcript_layout::TranscriptLayoutFingerprint::new(
                    "structure".to_owned(),
                ),
                transcript_len,
                pending_len: 0,
                transcript_signature: |index| {
                    crate::transcript_projection::test_layout_signature(
                        &items[index],
                        80,
                        Some(&presentation),
                    )
                },
                transcript_rows: |index| vec![Line::from(format!("row-{index}"))],
                transcript_invocation_id: |index| Some(format!("call-{index}")),
                pending_signature: |index| {
                    crate::transcript_layout::TranscriptLayoutSignature::new(format!(
                        "pending-{index}"
                    ))
                },
                pending_rows: |_| Vec::new(),
                history_banner_signature: || None,
                history_banner_rows: Vec::new,
                reset: || false,
            });
            assert_eq!(initial.entries_rebuilt, transcript_len);

            presentation.bump_visual_revision_for_test("call-0");
            let updated = cache.sync_visuals(
                crate::transcript_layout::TranscriptLayoutFingerprint::new("updated".to_owned()),
                &std::collections::BTreeSet::from(["call-0".to_owned()]),
                |index| {
                    crate::transcript_projection::test_layout_signature(
                        &items[index],
                        80,
                        Some(&presentation),
                    )
                },
                |index| vec![Line::from(format!("row-{index}"))],
            );
            assert_eq!(updated.entries_scanned, 1);
            assert_eq!(updated.signatures_changed, 1);
            assert_eq!(updated.entries_rebuilt, 1);
            assert_eq!(updated.rows_regenerated, 1);
        }
    }

    #[test]
    fn git_contribution_schema_routes_through_platform_registry() {
        let bundled = [StaticBundledPlugin::new(
            include_str!("../../../plugins/git-plugin/bcode-plugin.toml"),
            bcode_git_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("select Git plugin");
        let presentation = PluginTuiPresentation::new(
            PluginHost::load_static_plugins(&selected).expect("load Git plugin"),
        );
        let route = presentation
            .visual_route("bcode.git.clone_request", 1, Some("bcode.git"))
            .expect("Git contribution route");
        assert_eq!(route.plugin_id, "bcode.git");
        let registry = presentation.registry("bcode.git").expect("Git registry");
        let rows = registry
            .visual_rows(
                &route.adapter_id,
                "bcode.git.clone_request",
                &serde_json::json!({
                    "url": "https://github.com/bmorphism/bcode",
                    "ref": "main"
                }),
                &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                    80,
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                    None,
                ),
            )
            .expect("Git contribution rows");
        let rendered = rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(rendered.contains("github.com/bmorphism/bcode"));
        assert!(rendered.contains("main"));
    }

    #[test]
    fn growing_shell_artifact_advances_only_owner_revision() {
        let presentation = test_presentation();
        let producer = "bcode.shell";
        let schema = "bcode.shell.run";
        let reference_key = "shell_recording";
        let content_type = "application/x-bcode-shell-recording; version=3";
        assert!(presentation.accepts_artifact_reference(
            producer,
            schema,
            1,
            reference_key,
            Some(content_type),
        ));
        let directory = tempfile::tempdir().expect("recording temp dir");
        let path = directory.path().join("growing.bcsr");
        let (commit_sender, commit_receiver) = std::sync::mpsc::channel();
        let observer: bcode_shell_plugin::recording::ShellRecordingCommitObserver =
            Arc::new(move |commit| {
                let _ = commit_sender.send(commit);
            });
        let mut writer =
            bcode_shell_plugin::recording::AsyncShellRecordingWriter::create_with_observer(
                &path,
                80,
                24,
                Some(observer),
            )
            .expect("recording writer");
        let mut previous_bytes = deliver_shell_recording_commit(
            &presentation,
            &commit_receiver
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("initial recording commit"),
            0,
            1,
        );
        let mut delivered_revisions = 1_u64;
        for revision in 1..=32 {
            writer
                .write_output_with(
                    revision,
                    format!("raw-{revision}\n").as_bytes(),
                    Some(format!("rendered-{revision}\r\n").as_bytes()),
                    || {},
                )
                .expect("recording output");
            delivered_revisions = delivered_revisions.saturating_add(1);
            previous_bytes = deliver_shell_recording_commit(
                &presentation,
                &commit_receiver
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .expect("growing recording commit"),
                previous_bytes,
                delivered_revisions,
            );
        }
        writer
            .finish(33, Some(0), None, false, false)
            .expect("finish recording");
        delivered_revisions = delivered_revisions.saturating_add(1);
        let final_commit = commit_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("final recording commit");
        assert!(final_commit.finalized);
        let _final_bytes = deliver_shell_recording_commit(
            &presentation,
            &final_commit,
            previous_bytes,
            delivered_revisions,
        );
        assert_eq!(presentation.visual_revision("other-call"), 0);
        assert_eq!(
            presentation.drain_timings().len(),
            usize::try_from(delivered_revisions).expect("revisions")
        );
    }

    #[test]
    fn artifact_delivery_routes_only_supported_references_and_invalidates_the_owner() {
        let presentation = test_presentation();
        let mut registry = PluginTuiRegistry::default();
        registry.register_visual_adapter(
            ["shell-run-terminal-card"],
            Box::new(StatefulTestAdapter::default()),
        );
        presentation
            .registries
            .lock()
            .expect("presentation registries")
            .insert("bcode.shell".to_owned(), Arc::new(registry));

        assert!(presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "shell_recording",
            Some("application/x-bcode-shell-recording; version=3"),
        ));
        assert!(!presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "clean_output",
            Some("text/plain"),
        ));
        assert!(!presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "shell_recording",
            Some("text/plain"),
        ));

        assert!(
            presentation
                .deliver_artifact_chunk(&PluginTuiArtifactChunk {
                    tool_call_id: "call-owner".to_owned(),
                    artifact_id: "artifact-owner".to_owned(),
                    reference_key: "shell_recording".to_owned(),
                    producer_plugin_id: "bcode.shell".to_owned(),
                    schema: "bcode.shell.run".to_owned(),
                    schema_version: 1,
                    content_type: Some(
                        "application/x-bcode-shell-recording; version=3".to_owned(),
                    ),
                    offset: 0,
                    total_bytes: 3,
                    revision: 7,
                    finalized: false,
                    bytes: b"abc".to_vec(),
                })
                .expect("deliver owned artifact chunk")
        );
        assert_eq!(presentation.visual_revision("call-owner"), 1);
        assert_eq!(presentation.visual_revision("call-other"), 0);
        assert_eq!(
            presentation.drain_dirty_visuals(),
            BTreeSet::from(["call-owner".to_owned()])
        );
    }

    #[test]
    fn presentation_retains_one_registry_for_delivery_and_rendering() {
        let presentation = test_presentation();
        assert!(presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "shell_recording",
            Some("application/x-bcode-shell-recording; version=3"),
        ));
        assert!(!presentation.accepts_artifact_reference(
            "bcode.shell",
            "bcode.shell.run",
            1,
            "clean_output",
            Some("text/plain; charset=utf-8"),
        ));
        let mut registry = PluginTuiRegistry::default();
        registry.register_visual_adapter(
            ["shell-run-terminal-card"],
            Box::new(StatefulTestAdapter::default()),
        );
        presentation
            .registries
            .lock()
            .expect("presentation registries")
            .insert("bcode.shell".to_owned(), Arc::new(registry));

        let first = presentation.registry("bcode.shell").expect("registry");
        assert_eq!(presentation.revision(), 0);
        assert!(
            presentation
                .deliver_artifact_chunk(&PluginTuiArtifactChunk {
                    tool_call_id: "call".to_owned(),
                    artifact_id: "artifact".to_owned(),
                    reference_key: "reference".to_owned(),
                    producer_plugin_id: "bcode.shell".to_owned(),
                    schema: "bcode.shell.run".to_owned(),
                    schema_version: 1,
                    content_type: None,
                    offset: 0,
                    total_bytes: 3,
                    revision: 1,
                    finalized: false,
                    bytes: b"abc".to_vec(),
                })
                .expect("deliver artifact chunk")
        );
        assert_eq!(presentation.revision(), 0);
        assert_eq!(presentation.visual_revision("call"), 1);
        assert_eq!(presentation.visual_revision("other-call"), 0);
        assert_eq!(
            presentation.drain_dirty_visuals(),
            BTreeSet::from(["call".to_owned()])
        );

        let second = presentation.registry("bcode.shell").expect("registry");
        assert!(Arc::ptr_eq(&first, &second));
        let rows = second
            .visual_rows(
                "shell-run-terminal-card",
                "bcode.shell.run",
                &serde_json::Value::Null,
                &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                    80,
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                    None,
                ),
            )
            .expect("stateful adapter rows");
        assert_eq!(rows[0].spans[0].content, "abc");
    }
}
