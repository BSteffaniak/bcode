//! Bounded endpoint-local observation controls. No execution authority or replay retention.
use super::{
    Arc, FINALIZED_PAYLOAD_IDLE, HttpFailure, HttpState, Instant, Json, REQUEST_TIMEOUT, Request,
    Response, State, StatusCode, XrayAccessGrant, XrayObserveRequest, application_failure, failure,
    public_error, request_json, sleep_until, timeout, watch,
};
use axum::response::IntoResponse;
use bcode_xray_models::XrayFinalization;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub(super) type Cutoff = Option<Result<XrayFinalization, ()>>;

#[derive(Clone)]
pub(super) struct Control {
    pub session_id: String,
    pub finalize: watch::Sender<bool>,
    pub receipt: watch::Receiver<Cutoff>,
    pub release: watch::Sender<bool>,
    pub activity: watch::Sender<Instant>,
}

#[derive(Default)]
pub(super) struct Registry(Mutex<BTreeMap<String, Control>>);

pub(super) struct Registration {
    registry: Arc<Registry>,
    id: String,
    pub(super) release: watch::Receiver<bool>,
    activity: watch::Receiver<Instant>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.registry.0.lock() {
            entries.remove(&self.id);
        }
    }
}

impl Registration {
    /// Finalized retention only: no stream replay and no renewed capture. Each successful
    /// payload request renews inactivity; release, endpoint shutdown or expiry destroys it.
    pub async fn retain_payloads(&mut self, shutdown: &mut watch::Receiver<bool>) {
        let mut last_activity = Instant::now();
        loop {
            if *self.release.borrow() || *shutdown.borrow() {
                return;
            }
            tokio::select! {
                _ = self.release.changed() => return,
                _ = shutdown.changed() => return,
                result = self.activity.changed() => {
                    if result.is_err() { return; }
                    last_activity = last_activity.max(*self.activity.borrow_and_update());
                }
                () = sleep_until(last_activity + FINALIZED_PAYLOAD_IDLE) => return,
            }
        }
    }
}

impl Registry {
    pub fn active_count(&self) -> usize {
        // Poisoning must not incorrectly reclaim an endpoint as unused.
        self.0.lock().map_or(usize::MAX, |entries| entries.len())
    }
    pub fn register(
        self: &Arc<Self>,
        id: &str,
        session_id: String,
        limit: usize,
    ) -> Result<(Registration, watch::Receiver<bool>, watch::Sender<Cutoff>), HttpFailure> {
        let mut entries = self
            .0
            .lock()
            .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "xray_registry_unavailable"))?;
        if entries.len() >= limit || entries.contains_key(id) {
            return Err(failure(StatusCode::TOO_MANY_REQUESTS, "xray_http_capacity"));
        }
        let (finalize, requested) = watch::channel(false);
        let (receipt, result) = watch::channel(None);
        let (release, released) = watch::channel(false);
        let (activity, activity_rx) = watch::channel(Instant::now());
        entries.insert(
            id.to_owned(),
            Control {
                session_id,
                finalize,
                receipt: result,
                release,
                activity,
            },
        );
        drop(entries);
        Ok((
            Registration {
                registry: Arc::clone(self),
                id: id.to_owned(),
                release: released,
                activity: activity_rx,
            },
            requested,
            receipt,
        ))
    }

    pub fn authorized(&self, id: &str, grant: &XrayAccessGrant) -> Result<Control, HttpFailure> {
        let control = self
            .0
            .lock()
            .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "xray_registry_unavailable"))?
            .get(id)
            .cloned()
            .ok_or_else(|| failure(StatusCode::NOT_FOUND, "xray_observation_unavailable"))?;
        grant
            .authorize(&XrayObserveRequest {
                schema_version: 1,
                session_id: control.session_id.clone(),
                deep_capture: false,
            })
            .map_err(application_failure)?;
        Ok(control)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FinalizeRequest {
    pub schema_version: u16,
    pub observation_id: String,
}

pub(super) async fn finalize(State(state): State<Arc<HttpState>>, request: Request) -> Response {
    let Ok(_permit) = state.control_calls.try_acquire() else {
        return public_error(StatusCode::TOO_MANY_REQUESTS, "xray_http_capacity");
    };
    let input: FinalizeRequest = match request_json(request).await {
        Ok(input) => input,
        Err(response) => return response.into_response(),
    };
    if input.schema_version != 1
        || input.observation_id.is_empty()
        || input.observation_id.len() > 128
    {
        return public_error(StatusCode::BAD_REQUEST, "xray_invalid_request");
    }
    let control = match state
        .observations
        .authorized(&input.observation_id, &state.access.grant)
    {
        Ok(control) => control,
        Err(response) => return response.into_response(),
    };
    finalize_control(control).await
}

/// Release request uses exactly the finalize identity schema. It is accepted only after
/// cutoff, is idempotent while present, and returns 204 after engine ownership retires.
/// Unknown/expired/already retired identities return 404, never a successful replay claim.
pub(super) async fn release(State(state): State<Arc<HttpState>>, request: Request) -> Response {
    let Ok(_permit) = state.control_calls.try_acquire() else {
        return public_error(StatusCode::TOO_MANY_REQUESTS, "xray_http_capacity");
    };
    let input: FinalizeRequest = match request_json(request).await {
        Ok(input) => input,
        Err(response) => return response.into_response(),
    };
    if input.schema_version != 1
        || input.observation_id.is_empty()
        || input.observation_id.len() > 128
    {
        return public_error(StatusCode::BAD_REQUEST, "xray_invalid_request");
    }
    let control = match state
        .observations
        .authorized(&input.observation_id, &state.access.grant)
    {
        Ok(control) => control,
        Err(response) => return response.into_response(),
    };
    release_control(control).await
}

pub(super) async fn release_control(control: Control) -> Response {
    if !matches!(*control.receipt.borrow(), Some(Ok(_))) {
        return public_error(StatusCode::CONFLICT, "xray_finalize_required");
    }
    control.release.send_replace(true);
    match timeout(REQUEST_TIMEOUT, control.release.closed()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => public_error(StatusCode::SERVICE_UNAVAILABLE, "xray_release_pending"),
    }
}

pub(super) async fn finalize_control(mut control: Control) -> Response {
    // A watch flag coalesces concurrent/retried requests without an unbounded command queue.
    control.finalize.send_replace(true);
    let receipt = timeout(REQUEST_TIMEOUT, async {
        loop {
            let result = control.receipt.borrow().clone();
            if let Some(result) = result {
                return result;
            }
            if control.receipt.changed().await.is_err() {
                return Err(());
            }
        }
    })
    .await;
    match receipt {
        Ok(Ok(receipt)) => Json(receipt).into_response(),
        Ok(Err(())) => public_error(StatusCode::SERVICE_UNAVAILABLE, "xray_finalize_unavailable"),
        Err(_) => public_error(StatusCode::SERVICE_UNAVAILABLE, "xray_finalize_pending"),
    }
}
