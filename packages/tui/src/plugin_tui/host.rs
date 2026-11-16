//! Selected frontend callback authority. Constructed only from the local product host.
use bcode_plugin_sdk::tui::{
    PluginSessionViewSubscription, PluginStructuredGenerationControl, PluginTuiHostError,
};
use bcode_plugin_sdk::tui::{PluginTuiHost, host};
use bmux_plugin_runtime::ActivationScope;
use bmux_plugin_sdk::definition::PluginHostServices;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Resources {
    subscriptions: BTreeMap<uuid::Uuid, Option<PluginSessionViewSubscription>>,
    generations: BTreeMap<uuid::Uuid, Option<Generation>>,
}
struct Generation {
    control: PluginStructuredGenerationControl,
    result: Arc<
        Mutex<
            Option<
                Result<bcode_plugin_sdk::tui::PluginStructuredGenerationResult, PluginTuiHostError>,
            >,
        >,
    >,
}
impl Drop for Resources {
    fn drop(&mut self) {
        for generation in self.generations.values().flatten() {
            generation.control.cancel();
        }
    }
}
fn invalid() -> PluginTuiHostError {
    PluginTuiHostError::InvalidRequest(
        "frontend callback resource unavailable or exhausted".to_owned(),
    )
}

async fn dispatch(
    host: &dyn PluginTuiHost,
    scope: &ActivationScope,
    resources: &Mutex<Resources>,
    request: host::Request,
) -> host::Response {
    match request {
        host::Request::SubscribeSession { request } => {
            let result = (|| {
                let id = uuid::Uuid::new_v4();
                {
                    let mut resources = resources.lock().map_err(|_| invalid())?;
                    if resources.subscriptions.len() >= 32 {
                        return Err(invalid());
                    }
                    resources.subscriptions.insert(id, None);
                }
                let subscription = host.subscribe_session_view(request);
                let mut resources = resources.lock().map_err(|_| invalid())?;
                match subscription {
                    Ok(subscription) => {
                        resources.subscriptions.insert(id, Some(subscription));
                    }
                    Err(error) => {
                        resources.subscriptions.remove(&id);
                        return Err(error);
                    }
                }
                drop(resources);
                Ok(id)
            })();
            host::Response::Subscribed(result)
        }
        host::Request::PollSession { subscription } => {
            let result = (|| {
                let mut resources = resources.lock().map_err(|_| invalid())?;
                let observer = resources
                    .subscriptions
                    .get_mut(&subscription)
                    .and_then(Option::as_mut)
                    .ok_or_else(invalid)?;
                let update = observer.receiver.try_recv();
                drop(resources);
                match update {
                    Ok(update) => Ok(Some(update)),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => Err(invalid()),
                }
            })();
            host::Response::SessionUpdate(result.map(Box::new))
        }
        host::Request::CloseSession { subscription } => {
            let result = resources
                .lock()
                .map_err(|_| invalid())
                .and_then(|mut resources| {
                    resources
                        .subscriptions
                        .remove(&subscription)
                        .ok_or_else(invalid)
                        .map(drop)
                });
            host::Response::Closed(result)
        }
        request @ (host::Request::StartGeneration { .. }
        | host::Request::PollGeneration { .. }
        | host::Request::CancelGeneration { .. }) => {
            dispatch_generation(host, scope, resources, request)
        }
        request => host::dispatch(host, request).await,
    }
}

fn dispatch_generation(
    host: &dyn PluginTuiHost,
    scope: &ActivationScope,
    resources: &Mutex<Resources>,
    request: host::Request,
) -> host::Response {
    match request {
        host::Request::StartGeneration { request } => {
            let result = (|| {
                let id = uuid::Uuid::new_v4();
                {
                    let mut resources = resources.lock().map_err(|_| invalid())?;
                    if resources.generations.len() >= 32 {
                        return Err(invalid());
                    }
                    resources.generations.insert(id, None);
                }
                let control = PluginStructuredGenerationControl::default();
                let future = host.generate_observable_structured_output(request, control.clone());
                let result = Arc::new(Mutex::new(None));
                let completed = Arc::clone(&result);
                let cancellation = control.clone();
                let admitted =
                    scope.spawn("frontend-host-generation", move |mut cancel| async move {
                        let mut future = future;
                        let outcome = tokio::select! {
                            outcome = &mut future => outcome,
                            () = cancel.cancelled() => {
                                cancellation.cancel();
                                future.await
                            }
                        };
                        if let Ok(mut completed) = completed.lock() {
                            *completed = Some(outcome);
                        }
                        Ok(())
                    });
                if admitted.is_err() {
                    control.cancel();
                    resources
                        .lock()
                        .map_err(|_| invalid())?
                        .generations
                        .remove(&id);
                    return Err(invalid());
                }
                resources
                    .lock()
                    .map_err(|_| invalid())?
                    .generations
                    .insert(id, Some(Generation { control, result }));
                Ok(id)
            })();
            host::Response::GenerationStarted(result)
        }
        host::Request::PollGeneration { generation } => {
            let result = (|| {
                let mut resources = resources.lock().map_err(|_| invalid())?;
                let operation = resources
                    .generations
                    .get(&generation)
                    .and_then(Option::as_ref)
                    .ok_or_else(invalid)?;
                let status = host::GenerationStatus {
                    session_id: operation.control.session_id(),
                    result: operation.result.lock().map_err(|_| invalid())?.clone(),
                };
                if status.result.is_some() {
                    resources.generations.remove(&generation);
                }
                drop(resources);
                Ok(status)
            })();
            host::Response::GenerationStatus(result.map(Box::new))
        }
        host::Request::CancelGeneration { generation } => {
            let result = resources
                .lock()
                .map_err(|_| invalid())
                .and_then(|resources| {
                    resources
                        .generations
                        .get(&generation)
                        .and_then(Option::as_ref)
                        .ok_or_else(invalid)
                        .map(|operation| operation.control.cancel())
                });
            host::Response::CancellationRequested(result)
        }
        _ => host::Response::Closed(Err(invalid())),
    }
}

/// Bind typed host callbacks to the exact selected live activation.
pub(super) fn services(
    host: Arc<dyn PluginTuiHost>,
    scope: ActivationScope,
) -> Result<PluginHostServices, String> {
    let resources = Arc::new(Mutex::new(Resources::default()));
    let cleanup = Arc::downgrade(&resources);
    scope
        .spawn("frontend-host-observers", move |mut cancelled| async move {
            cancelled.cancelled().await;
            if let Some(resources) = cleanup.upgrade() {
                let mut resources = resources
                    .lock()
                    .map_err(|_| "frontend observers unavailable".to_owned())?;
                resources.subscriptions.clear();
                for generation in resources.generations.values().flatten() {
                    generation.control.cancel();
                }
            }
            Ok(())
        })
        .map_err(|_| "frontend callback lifecycle unavailable".to_owned())?;
    let synchronous_resources = Arc::clone(&resources);
    let synchronous_host = Arc::clone(&host);
    let synchronous_scope = scope.clone();
    Ok(PluginHostServices::new(move |request| {
        let _admission = synchronous_scope
            .try_admit()
            .map_err(|_| protocol_error())?;
        if request.capability != host::CONTRACT
            || request.kind != bmux_plugin_sdk::ServiceKind::Command
            || request.interface != host::CONTRACT
            || request.operation != "invoke"
            || request.payload.len() > host::MAX_BYTES
        {
            return Err(protocol_error());
        }
        let response =
            match serde_json::from_slice(request.payload).map_err(|_| protocol_error())? {
                host::Request::CopyText { text } => {
                    host::Response::CopyText(synchronous_host.copy_text(text))
                }
                host::Request::Redraw {} => {
                    synchronous_host.request_redraw();
                    host::Response::Redrawn
                }
                host::Request::CloseSession { subscription } => {
                    let result = synchronous_resources
                        .lock()
                        .map_err(|_| invalid())
                        .and_then(|mut resources| {
                            resources
                                .subscriptions
                                .remove(&subscription)
                                .ok_or_else(invalid)
                                .map(drop)
                        });
                    host::Response::Closed(result)
                }
                host::Request::CancelGeneration { generation } => {
                    let result = synchronous_resources
                        .lock()
                        .map_err(|_| invalid())
                        .and_then(|resources| {
                            resources
                                .generations
                                .get(&generation)
                                .and_then(Option::as_ref)
                                .ok_or_else(invalid)
                                .map(|operation| operation.control.cancel())
                        });
                    host::Response::CancellationRequested(result)
                }
                _ => return Err(protocol_error()),
            };
        serde_json::to_vec(&response).map_err(|_| protocol_error())
    })
    .with_async(move |request| {
        let host = Arc::clone(&host);
        let scope = scope.clone();
        let resources = Arc::clone(&resources);
        Box::pin(async move {
            let _admission = scope.try_admit().map_err(|_| protocol_error())?;
            if request.capability != host::CONTRACT
                || request.kind != bmux_plugin_sdk::ServiceKind::Command
                || request.interface != host::CONTRACT
                || request.operation != "invoke"
                || request.payload.len() > host::MAX_BYTES
            {
                return Err(protocol_error());
            }
            let request: host::Request =
                serde_json::from_slice(&request.payload).map_err(|_| protocol_error())?;
            let response = dispatch(host.as_ref(), &scope, &resources, request).await;
            let bytes = serde_json::to_vec(&response).map_err(|_| protocol_error())?;
            if bytes.len() > host::MAX_BYTES {
                return Err(protocol_error());
            }
            Ok(bytes)
        })
    }))
}
fn protocol_error() -> bmux_plugin_sdk::PluginError {
    bmux_plugin_sdk::PluginError::ServiceProtocol {
        details: "invalid or unavailable selected frontend host callback".to_owned(),
    }
}
