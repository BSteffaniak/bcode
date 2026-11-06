//! Isolated synthetic observation source. No session client, provider, or canonical storage.
use bcode_client::xray::{XrayClient, XrayStream};
use bcode_xray_models::{
    XrayEnvelope, XrayFinalization, XrayObserveRequest, XrayPayloadRange, XrayReadPayloadRequest,
};
use std::sync::Arc;
use tokio::sync::{Mutex, watch};

pub(super) enum Backend {
    Live(XrayClient),
    Demo(Arc<Demo>),
}

pub(super) struct Demo {
    hub: bcode_xray::XrayHub,
    observation: Mutex<Option<bcode_xray::XrayObservation>>,
    cutoff: watch::Sender<bool>,
    released: watch::Sender<bool>,
}

pub(super) enum Stream {
    Live(Box<XrayStream>),
    Demo {
        backend: Arc<Demo>,
        cutoff: watch::Receiver<bool>,
        receipt: Option<XrayFinalization>,
        id: String,
        requested: bool,
    },
}

impl Backend {
    pub(super) fn demo() -> Self {
        let (cutoff, _) = watch::channel(false);
        let (released, _) = watch::channel(false);
        Self::Demo(Arc::new(Demo {
            hub: bcode_xray::XrayHub::new(bcode_xray::XrayConfig {
                queue_capacity: 4,
                ..Default::default()
            }),
            observation: Mutex::new(None),
            cutoff,
            released,
        }))
    }

    pub(super) async fn observe(&self, request: XrayObserveRequest) -> Result<Stream, ()> {
        match self {
            Self::Live(client) => client
                .observe(request)
                .await
                .map(|stream| Stream::Live(Box::new(stream)))
                .map_err(|_| ()),
            Self::Demo(backend) => {
                let mut slot = backend.observation.lock().await;
                if slot.is_some() {
                    return Err(());
                }
                let observation = backend.hub.observe(request.clone()).map_err(|_| ())?;
                let id = observation.id().to_owned();
                let correlation = bcode_xray_models::XrayCorrelation {
                    session_id: request.session_id,
                    ..Default::default()
                };
                let payload = backend
                    .hub
                    .capture_payload(
                        &correlation,
                        "text/plain",
                        b"SYNTHETIC X-ray demo payload; no provider request was made.\n",
                    )
                    .map_err(|_| ())?
                    .ok_or(())?;
                let mut event = bcode_xray_models::XrayEvent::new(
                    "demo",
                    "synthetic_payload",
                    correlation.clone(),
                    serde_json::json!({"synthetic":true,"payload":payload}),
                    true,
                );
                event.capture = Some(bcode_xray_models::XrayCaptureAttachments::new(vec![
                    bcode_xray_models::XrayCaptureAttachment::PayloadRef {
                        pointer: "/payload".into(),
                    },
                ]));
                backend.hub.emit(event);
                // Deliberately exceed a tiny isolated queue: gaps are engine-produced,
                // not fabricated continuity or sleep-dependent producer timing.
                for index in 0..12 {
                    backend.hub.emit(bcode_xray_models::XrayEvent::new(
                        "demo",
                        "synthetic_event",
                        correlation.clone(),
                        serde_json::json!({"synthetic":true,"index":index}),
                        false,
                    ));
                }
                *slot = Some(observation);
                drop(slot);
                backend.cutoff.send_replace(false);
                backend.released.send_replace(false);
                Ok(Stream::Demo {
                    backend: backend.clone(),
                    cutoff: backend.cutoff.subscribe(),
                    receipt: None,
                    id,
                    requested: false,
                })
            }
        }
    }

    pub(super) async fn finalize(&self, id: &str) -> Result<(), ()> {
        match self {
            Self::Live(client) => client.finalize(id).await.map_err(|_| ()),
            Self::Demo(backend) => {
                backend.cutoff.send_replace(true);
                Ok(())
            }
        }
    }

    pub(super) async fn release(&self, id: &str) -> Result<(), ()> {
        match self {
            Self::Live(client) => client.release(id).await.map_err(|_| ()),
            Self::Demo(backend) => {
                backend.released.send_replace(true);
                backend.observation.lock().await.take();
                Ok(())
            }
        }
    }

    pub(super) async fn read_payload(
        &self,
        request: &XrayReadPayloadRequest,
    ) -> Result<XrayPayloadRange, ()> {
        match self {
            Self::Live(client) => client.read_payload(request).await.map_err(|_| ()),
            Self::Demo(backend) => backend
                .hub
                .read_payload_complete(request)
                .await
                .map_err(|_| ()),
        }
    }
}

impl Stream {
    pub(super) fn observation_id(&self) -> &str {
        match self {
            Self::Live(stream) => stream.observation_id(),
            Self::Demo { id, .. } => id,
        }
    }
    pub(super) const fn finalization(&self) -> Option<&XrayFinalization> {
        match self {
            Self::Live(stream) => stream.finalization(),
            Self::Demo { receipt, .. } => receipt.as_ref(),
        }
    }
    pub(super) async fn recv(&mut self) -> Result<Option<XrayEnvelope>, ()> {
        match self {
            Self::Live(stream) => stream.recv().await.map_err(|_| ()),
            Self::Demo {
                backend,
                cutoff,
                receipt,
                requested,
                ..
            } => {
                let mut released = backend.released.subscribe();
                let mut slot = backend.observation.lock().await;
                let observation = slot.as_mut().ok_or(())?;
                if *cutoff.borrow() && !*requested {
                    observation.finalize_complete().await.map_err(|_| ())?;
                    *requested = true;
                }
                loop {
                    tokio::select! {
                        biased;
                        _ = released.changed() => return Err(()),
                        changed = cutoff.changed(), if !*requested => {
                            changed.map_err(|_| ())?;
                            if *cutoff.borrow() {
                                observation.finalize_complete().await.map_err(|_| ())?;
                                *requested = true;
                            }
                        }
                        envelope = observation.recv() => {
                            if let Some(value) = &envelope
                                && value.event.domain == "xray" && value.event.name == "finalized"
                            {
                                *receipt = Some(serde_json::from_value(value.event.data.clone()).map_err(|_| ())?);
                            }
                            drop(slot);
                            return Ok(envelope);
                        },
                    }
                }
            }
        }
    }
}
