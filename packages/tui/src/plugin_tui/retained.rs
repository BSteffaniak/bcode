//! Renderer-local replay and successful-output admission for M7 retained resources.
//!
//! No scene format lives here. The selected owner is retained beside M7's resource store;
//! producer updates cannot acknowledge a display or choose their own owner namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_FRONTEND: AtomicU64 = AtomicU64::new(1);

use bcode_plugin_sdk::definition::{ProductExtensions, SelectedProductFrontend};
use bmux_plugin::component_viewport::portable::{
    FrontendPresentationRequest, FrontendPresentationResponse, FrontendResourceId,
    PortablePresentationError, PortablePresentationLimits, PreparedFrontendPresentation,
    RetainedFrontendHost,
};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitId, HitMap};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::selection::{SelectionContentId, SelectionScene};

impl Drop for RetainedFrontend {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests;

/// Producer-only selected frontend channel. It cannot acknowledge output or validate input.
#[derive(Clone)]
pub struct RetainedProducer {
    view: std::sync::Weak<RetainedFrontend>,
}

impl RetainedProducer {
    /// Publish through the exact selected frontend's bounded M7 resource contract.
    ///
    /// # Errors
    /// Rejects retired/closing owners, incompatible versions and invalid resource updates.
    pub fn apply(
        &self,
        request: FrontendPresentationRequest,
    ) -> Result<FrontendPresentationResponse, String> {
        self.view
            .upgrade()
            .ok_or_else(|| "frontend channel retired".to_owned())?
            .apply(request)
    }

    /// Dispatch the exact shared retained-scene extension without inventing a product codec.
    /// Execution owners must retain the returned result across ABI buffer growth; callers must
    /// never replay this mutation merely because encoding or response delivery failed.
    ///
    /// # Errors
    /// Rejects unknown contracts, oversized/malformed payloads and retired scopes.
    pub fn extension(
        &self,
        contract: &str,
        payload: &[u8],
    ) -> Result<FrontendPresentationResponse, String> {
        use bmux_plugin::component_viewport::portable::PORTABLE_PRESENTATION_CONTRACT_ID;
        if contract != PORTABLE_PRESENTATION_CONTRACT_ID {
            return Err("unsupported retained frontend contract".to_owned());
        }
        let view = self
            .view
            .upgrade()
            .ok_or_else(|| "frontend channel retired".to_owned())?;
        view.owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        if payload.len() > view.max_request_bytes {
            return Err("retained frontend request exceeds bound".to_owned());
        }
        let request = serde_json::from_slice::<FrontendPresentationRequest>(payload)
            .map_err(|_| "invalid retained frontend request".to_owned())?;
        view.apply(request)
    }
}

/// One selected activation's renderer-local resources. Construct off the frame loop.
pub struct RetainedFrontend {
    owner: ProductExtensions,
    max_request_bytes: usize,
    state: Arc<Mutex<State>>,
    registration: Mutex<Option<ActivationRegistration>>,
}

struct ActivationRegistration {
    resources: bcode_plugin_sdk::definition::ActivationResources,
    resource: bmux_plugin_runtime::PluginResourceId,
}

/// The activation-charged owner of a view's renderer resources. The activation
/// owner retires it at teardown when the view could not release it earlier.
struct RegisteredState(Arc<Mutex<State>>);

impl bcode_plugin_sdk::definition::ActivationResource for RegisteredState {
    fn retire(&self) {
        RetainedFrontend::retire_state(&self.0);
    }
}

struct State {
    resources: Option<RetainedFrontendHost>,
    prepared: BTreeMap<(FrontendResourceId, u64), PreparedFrontendPresentation>,
    generations: BTreeMap<FrontendResourceId, u64>,
    closed: bool,
    displayed_hits: BTreeSet<HitId>,
    displayed_content: BTreeSet<SelectionContentId>,
    // Only revisions actually replayed in the current output attempt may be acknowledged.
    painted: BTreeMap<FrontendResourceId, (u64, u64, Rect, u16)>,
    paint_order: Vec<FrontendResourceId>,
    displayed: BTreeMap<FrontendResourceId, (u64, u64, Rect, u16)>,
}

impl RetainedFrontend {
    /// Bind resources to host-observed owner/activation identities, never producer payload fields.
    ///
    /// # Errors
    /// Rejects unavailable owners and invalid resource-store configuration.
    pub fn new(
        selected: &SelectedProductFrontend,
        activation: u64,
        limits: PortablePresentationLimits,
    ) -> Result<Arc<Self>, String> {
        Self::from_owner(
            selected.owner().clone(),
            &selected.descriptor().plugin_id,
            activation,
            limits,
        )
    }

    /// Bind a portable registry factory to the descriptor of its actual selected component.
    /// This is usable while `resolve_frontend` is constructing the registry, before a
    /// `SelectedProductFrontend` exists; it never trusts producer-supplied owner IDs.
    ///
    /// # Errors
    /// Rejects inactive/backend components and missing or incompatible rich v2 contracts.
    pub async fn for_component(
        owner: ProductExtensions,
        activation: u64,
        limits: PortablePresentationLimits,
    ) -> Result<Arc<Self>, String> {
        let descriptor = owner
            .descriptor()
            .await
            .map_err(|_| "frontend descriptor unavailable".to_owned())?;
        if descriptor.role != bcode_plugin_sdk::definition::ComponentRole::Frontend
            || !descriptor.contracts.iter().any(|contract| {
                contract.interface
                    == bmux_plugin::component_viewport::portable::PORTABLE_PRESENTATION_CONTRACT_ID
                    && contract.version == 2
            })
        {
            return Err("selected frontend does not implement rich retained v2".to_owned());
        }
        Self::from_owner(owner, &descriptor.plugin_id, activation, limits)
    }

    /// Admit a descriptor already verified by `ProductExtensions::resolve_frontend`.
    ///
    /// # Errors
    /// Rejects backend roles, absent rich-v2 contracts and unavailable selected owners.
    pub fn from_descriptor(
        owner: ProductExtensions,
        descriptor: &bcode_plugin_sdk::definition::ProductExtensionDescriptor,
        activation: u64,
        limits: PortablePresentationLimits,
    ) -> Result<Arc<Self>, String> {
        if descriptor.role != bcode_plugin_sdk::definition::ComponentRole::Frontend
            || !descriptor.contracts.iter().any(|contract| {
                contract.interface
                    == bmux_plugin::component_viewport::portable::PORTABLE_PRESENTATION_CONTRACT_ID
                    && contract.version == 2
            })
        {
            return Err("selected frontend lacks rich-v2 contract".to_owned());
        }
        Self::from_owner(owner, &descriptor.plugin_id, activation, limits)
    }

    /// Bind retained resources to the same live activation that admits selected calls.
    ///
    /// # Errors
    /// Rejects missing activation authority, incompatible descriptors or failed registration.
    pub fn from_active_descriptor(
        owner: ProductExtensions,
        descriptor: &bcode_plugin_sdk::definition::ProductExtensionDescriptor,
        limits: PortablePresentationLimits,
    ) -> Result<Arc<Self>, String> {
        let resources = owner
            .activation_resources()
            .map_err(|_| "selected frontend activation unavailable".to_owned())?;
        let scope = resources.scope().clone();
        let admission = scope
            .try_admit()
            .map_err(|_| "selected frontend activation closed".to_owned())?;
        // The owner/activation namespace comes from the admitted host scope. The
        // numeric scene epoch is local to that unique activation, not scope authority.
        let namespace = format!("{}:{}", scope.instance_id(), scope.id());
        if descriptor.role != bcode_plugin_sdk::definition::ComponentRole::Frontend
            || !descriptor.contracts.iter().any(|contract| {
                contract.interface
                    == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID
                    && contract.version == 2
            })
        {
            return Err("selected frontend lacks rich-v2 contract".to_owned());
        }
        let mut view = Self::from_owner(owner, &namespace, 1, limits)?;
        let resource = resources
            .register(Arc::new(RegisteredState(Arc::clone(&view.state))))
            .map_err(|_| "frontend resource registration failed".to_owned())?;
        drop(admission);
        let unique = Arc::get_mut(&mut view)
            .ok_or_else(|| "frontend registration unexpectedly shared".to_owned())?;
        unique.registration = Mutex::new(Some(ActivationRegistration {
            resources,
            resource,
        }));
        Ok(view)
    }

    /// Validate immediate input against the currently displayed resource lease.
    ///
    /// # Errors
    /// Rejects hidden, retired, stale or display-suspended revisions.
    pub fn validate_displayed(
        &self,
        resource: FrontendResourceId,
        generation: u64,
        revision: u64,
    ) -> Result<(), String> {
        self.owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        let state = self
            .state
            .lock()
            .map_err(|_| "frontend resources unavailable".to_owned())?;
        if !state.displayed.contains_key(&resource) {
            return Err("frontend not displayed".to_owned());
        }
        let lease = state
            .resources
            .as_ref()
            .ok_or_else(|| "frontend retired".to_owned())?
            .committed_for_input(resource, generation, revision)
            .map_err(|error| error.to_string())?;
        drop(state);
        // A lease can be revoked after the lock is released; no queued input authority is issued.
        drop(lease);
        Ok(())
    }

    fn from_owner(
        owner: ProductExtensions,
        namespace: &str,
        activation: u64,
        limits: PortablePresentationLimits,
    ) -> Result<Arc<Self>, String> {
        owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        let instance = NEXT_FRONTEND
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "frontend identity exhausted".to_owned())?;
        let namespace = format!("bcode.frontend.{instance}:{namespace}");
        let resources = RetainedFrontendHost::new(namespace, activation, limits)
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(Self {
            owner,
            max_request_bytes: limits.max_bytes,
            state: Arc::new(Mutex::new(State {
                resources: Some(resources),
                prepared: BTreeMap::new(),
                generations: BTreeMap::new(),
                closed: false,
                displayed_hits: BTreeSet::new(),
                displayed_content: BTreeSet::new(),
                painted: BTreeMap::new(),
                paint_order: Vec::new(),
                displayed: BTreeMap::new(),
            })),
            registration: Mutex::new(None),
        }))
    }

    /// Retain a producer channel without granting display/input authority or extending host life.
    #[must_use]
    pub fn producer(self: &Arc<Self>) -> RetainedProducer {
        RetainedProducer {
            view: Arc::downgrade(self),
        }
    }

    /// Apply an already bounded, typed M7 producer update off the terminal frame loop.
    ///
    /// # Errors
    /// Rejects closed owners, invalid updates and poisoned local state. Never retries mutations.
    pub fn apply(
        &self,
        request: FrontendPresentationRequest,
    ) -> Result<FrontendPresentationResponse, String> {
        self.owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "frontend resources unavailable".to_owned())?;
        if state.closed {
            return Err("frontend resources closed".to_owned());
        }
        let published = match &request {
            FrontendPresentationRequest::Publish {
                resource, revision, ..
            } => Some((*resource, *revision)),
            _ => None,
        };
        let created_generation = match &request {
            FrontendPresentationRequest::Create { generation } => Some(*generation),
            _ => None,
        };
        let destroyed = match &request {
            FrontendPresentationRequest::Destroy { resource } => Some(*resource),
            _ => None,
        };
        let response = state
            .resources
            .as_ref()
            .ok_or_else(|| "frontend resources closed".to_owned())?
            .producer()
            .apply(request)
            .map_err(|error| error.to_string())?;
        if let (Some(generation), FrontendPresentationResponse::Created { resource }) =
            (created_generation, &response)
        {
            state.generations.insert(*resource, generation);
        }
        if let Some((resource, revision)) = published {
            let prepared = state
                .resources
                .as_ref()
                .ok_or_else(|| "frontend resources closed".to_owned())?
                .prepare(resource, revision)
                .map_err(|error| error.to_string())?;
            state.prepared.insert((resource, revision), prepared);
        }
        if let Some(resource) = destroyed {
            state.prepared.retain(|(id, _), _| *id != resource);
            state.generations.remove(&resource);
            state.painted.remove(&resource);
            state.paint_order.retain(|id| *id != resource);
            state.displayed.remove(&resource);
        }
        drop(state);
        Ok(response)
    }

    /// Replay full retained metadata through BMUX's one transform/clip boundary.
    /// No plugin calls or reconstruction occur here. Empty/offscreen placements do not admit input.
    ///
    /// # Errors
    /// Rejects unavailable owners/resources or unprepared revisions.
    pub fn paint(
        &self,
        observed: (FrontendResourceId, u64, u64),
        destination: Rect,
        logical_row_offset: u16,
        cx: &mut PaintCx<'_, '_>,
    ) -> Result<(), String> {
        let (resource, generation, revision) = observed;
        self.owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        if destination.is_empty() {
            return Ok(());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "frontend resources unavailable".to_owned())?;
        if state.closed || state.generations.get(&resource) != Some(&generation) {
            return Err("frontend resource closed or stale generation".to_owned());
        }
        let placement = (generation, revision, destination, logical_row_offset);
        if state
            .painted
            .get(&resource)
            .is_some_and(|old| *old != placement)
        {
            return Err("frontend resource painted at conflicting geometry or revision".to_owned());
        }
        let prepared = state
            .prepared
            .get(&(resource, revision))
            .ok_or_else(|| "frontend projection not prepared".to_owned())?;
        // Clip first in destination space, then translate the logical slice. BMUX applies this
        // exact transform to cells, wide glyphs, images, cursor, hits, semantics and selection.
        let mut visible = false;
        cx.with_child(
            i32::from(destination.x),
            i64::from(destination.y),
            LocalRect::new(0, 0, destination.width, destination.height),
            |cx| {
                visible = cx.area().width > 0 && cx.area().height > 0;
                cx.with_child(
                    0,
                    -i64::from(logical_row_offset),
                    LocalRect::new(
                        0,
                        i64::from(logical_row_offset),
                        destination.width,
                        destination.height,
                    ),
                    |cx| prepared.paint(cx),
                );
            },
        );
        if visible {
            if !state.painted.contains_key(&resource) {
                state.paint_order.push(resource);
            }
            state.painted.insert(
                resource,
                (generation, revision, destination, logical_row_offset),
            );
        }
        drop(state);
        Ok(())
    }

    /// Merge only replayed source text and modal declarations before the compositor writes output.
    /// This uses M7's complete prepared projection, never a reconstruction from cells.
    ///
    /// # Errors
    /// Rejects revoked owners, conflicting text identities and missing prepared resources.
    pub fn merge_prepared(
        &self,
        text: &mut BTreeMap<SelectionContentId, String>,
        modal_scopes: &mut Vec<bmux_tui::focus::FocusScopeId>,
    ) -> Result<(), String> {
        self.owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        let snapshots = {
            let state = self
                .state
                .lock()
                .map_err(|_| "frontend resources unavailable".to_owned())?;
            if state.closed {
                return Err("frontend resources closed".to_owned());
            }
            let mut snapshots = Vec::new();
            for id in &state.paint_order {
                let (_, revision, _, _) = state
                    .painted
                    .get(id)
                    .ok_or_else(|| "missing frontend paint receipt".to_owned())?;
                let prepared = state
                    .prepared
                    .get(&(*id, *revision))
                    .ok_or_else(|| "frontend projection not prepared".to_owned())?;
                snapshots.push((prepared.selection_text(), prepared.modal_scopes()));
            }
            drop(state);
            snapshots
        };
        for (sources, scopes) in snapshots {
            for (id, source) in sources {
                if text.get(&id).is_some_and(|existing| existing != &source) {
                    return Err("conflicting frontend source identity".to_owned());
                }
                text.insert(id, source);
            }
            for scope in scopes {
                if !modal_scopes.contains(&scope) {
                    modal_scopes.push(scope);
                }
            }
        }
        Ok(())
    }

    /// Start one terminal output attempt; stale paint receipts never cross frame boundaries.
    pub fn begin_display(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.painted.clear();
            state.paint_order.clear();
        }
    }

    /// Called only by the root presenter after its terminal flush succeeds.
    ///
    /// # Errors
    /// Rejects stale generations/revisions. Failure closes input rather than retaining stale hits.
    pub fn display_succeeded(
        &self,
        hits: &HitMap,
        selection: &SelectionScene,
    ) -> Result<(), String> {
        self.owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "frontend resources unavailable".to_owned())?;
        if state.closed {
            return Err("frontend resources closed".to_owned());
        }
        if let Some(resources) = &state.resources {
            let _ = resources.display_failed();
        }
        state.displayed.clear();
        state.displayed_content.clear();
        state.displayed_hits.clear();
        let painted = std::mem::take(&mut state.painted);
        state.paint_order.clear();
        for (id, (generation, revision, area, offset)) in painted {
            if let Err(error) = state
                .resources
                .as_ref()
                .ok_or_else(|| "frontend resources closed".to_owned())?
                .acknowledge(id, generation, revision)
            {
                if let Some(resources) = &state.resources {
                    let _ = resources.display_failed();
                }
                state.displayed.clear();
                return Err(error.to_string());
            }
            state.prepared.retain(|(resource, cached_revision), _| {
                *resource != id || *cached_revision >= revision
            });
            let prepared = state
                .prepared
                .get(&(id, revision))
                .ok_or_else(|| "frontend displayed projection unavailable".to_owned())?;
            let local_hits = hits
                .regions()
                .iter()
                .filter(|hit| prepared.local_hit(&hit.id).is_some())
                .map(|hit| hit.id.clone())
                .collect::<Vec<_>>();
            let content = selection
                .fragments()
                .iter()
                .filter(|fragment| prepared.text(&fragment.content_id).is_some())
                .map(|fragment| fragment.content_id.clone())
                .collect::<Vec<_>>();
            state.displayed_hits.extend(local_hits);
            state.displayed_content.extend(content);
            state
                .displayed
                .insert(id, (generation, revision, area, offset));
        }
        drop(state);
        Ok(())
    }

    /// Permanently revoke this view and destroy every M7 resource, invalidating prepared handles.
    pub fn close(&self) {
        Self::retire_state(&self.state);
        let registration = self
            .registration
            .lock()
            .ok()
            .and_then(|mut registration| registration.take());
        if let Some(ActivationRegistration {
            resources,
            resource,
        }) = registration
        {
            // Release now when the activation is idle. Otherwise the entry stays
            // in the activation's ledger and its owner releases it during
            // deactivation, after draining and before finishing the activation.
            // No failed release is treated as successful teardown.
            let _ = resources.release(resource);
        }
    }

    fn retire_state(state: &Mutex<State>) {
        if let Ok(mut state) = state.lock() {
            state.closed = true;
            if let Some(resources) = &state.resources {
                let _ = resources.display_failed();
            }
            let resources = std::mem::take(&mut state.generations);
            for id in resources.keys() {
                if let Some(resources) = &state.resources {
                    let _ = resources
                        .producer()
                        .apply(FrontendPresentationRequest::Destroy { resource: *id });
                }
            }
            state.resources.take();
            state.prepared.clear();
            state.painted.clear();
            state.paint_order.clear();
            state.displayed_hits.clear();
            state.displayed_content.clear();
            state.displayed.clear();
        }
    }

    /// Failed/reset terminal output suspends input, selection lookup and pending receipts.
    pub fn display_failed(&self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(resources) = &state.resources {
                let _ = resources.display_failed();
            }
            state.painted.clear();
            state.paint_order.clear();
            state.displayed_hits.clear();
            state.displayed_content.clear();
            state.displayed.clear();
        }
    }

    /// Translate an owner-scoped committed hit to producer-local identity and slice geometry.
    ///
    /// # Errors
    /// Rejects stale/hidden resources, unknown hits and closed selected owners before dispatch.
    pub fn local_hit(
        &self,
        resource: FrontendResourceId,
        generation: u64,
        revision: u64,
        hit: &HitId,
    ) -> Result<(String, Rect, u16), String> {
        self.owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        let state = self
            .state
            .lock()
            .map_err(|_| "frontend resources unavailable".to_owned())?;
        if !state.displayed_hits.contains(hit) {
            return Err("frontend hit not displayed".to_owned());
        }
        let (_, _, area, offset) = *state
            .displayed
            .get(&resource)
            .ok_or_else(|| PortablePresentationError::NotCommitted.to_string())?;
        let prepared = state
            .resources
            .as_ref()
            .ok_or_else(|| "frontend resources closed".to_owned())?
            .committed_for_input(resource, generation, revision)
            .map_err(|error| error.to_string())?;
        drop(state);
        let local = prepared
            .local_hit(hit)
            .ok_or_else(|| "unknown frontend hit".to_owned())?;
        Ok((local.id.as_str().to_owned(), area, offset))
    }

    /// Dispatch immediately under a revocable committed lease, using only authoritative hit IDs.
    /// The callback must not enqueue the lease or raw hit for later execution. Shared execution
    /// must independently admit its selected owner and must not replay effects on response growth.
    ///
    /// # Errors
    /// Rejects hidden, occluded, foreign, stale or retired input targets before callback admission.
    pub fn with_input<T>(
        &self,
        observed: (FrontendResourceId, u64, u64),
        hit: &HitId,
        dispatch: impl FnOnce(&str, Rect, u16) -> Result<T, String>,
    ) -> Result<T, String> {
        self.owner
            .ensure_active()
            .map_err(|_| "frontend owner unavailable".to_owned())?;
        let (resource, generation, revision) = observed;
        let (prepared, area, offset) = {
            let state = self
                .state
                .lock()
                .map_err(|_| "frontend resources unavailable".to_owned())?;
            if !state.displayed_hits.contains(hit) {
                return Err("frontend hit not displayed".to_owned());
            }
            let (_, _, area, offset) = *state
                .displayed
                .get(&resource)
                .ok_or_else(|| "frontend resource not displayed".to_owned())?;
            let prepared = state
                .resources
                .as_ref()
                .ok_or_else(|| "frontend resources closed".to_owned())?
                .committed_for_input(resource, generation, revision)
                .map_err(|error| error.to_string())?;
            drop(state);
            (prepared, area, offset)
        };
        let local = prepared
            .local_hit(hit)
            .ok_or_else(|| "frontend input lease revoked or foreign hit".to_owned())?;
        dispatch(local.id.as_str(), area, offset)
    }

    /// Read logical source text only from resources admitted by the displayed revision.
    #[must_use]
    pub fn selection_text(&self, content: &SelectionContentId) -> Option<String> {
        self.owner.ensure_active().ok()?;
        let displayed = {
            let state = self.state.lock().ok()?;
            if !state.displayed_content.contains(content) {
                return None;
            }
            let snapshots = state
                .displayed
                .iter()
                .map(|(id, (generation, revision, _, _))| {
                    state
                        .resources
                        .as_ref()?
                        .committed_for_input(*id, *generation, *revision)
                        .ok()
                })
                .collect::<Option<Vec<_>>>()?;
            drop(state);
            snapshots
        };
        for prepared in displayed {
            if let Some(text) = prepared.text(content) {
                return Some(text.to_owned());
            }
        }
        None
    }
}
