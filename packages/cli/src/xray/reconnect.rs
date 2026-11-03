//! Explicit transport-only, gapful reattachment through the matching application.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    CliError, ConsumerFailure, ConsumerMessage, XrayArgs, enqueue, invalid, transport_error,
};
#[cfg(test)]
use super::{ConsumerRecording, consume_segment_after_gap};
use bcode_client::xray::{XrayClientError, XrayObservationController};

pub(super) async fn run(
    args: &XrayArgs,
    grant: Option<bcode_ipc::XrayOriginGrantRequest>,
) -> Result<(), CliError> {
    let application = crate::session_read_client(args.session_id).await?;
    let mut controller = XrayObservationController::new(&application, args.session_id, args.deep);
    controller.set_origin_grant(grant);
    let attempts = u64::from(args.reconnect_retries) + 1;
    controller
        .set_segment_limit(
            std::num::NonZeroU64::new(attempts).ok_or_else(|| invalid("invalid retry budget"))?,
        )
        .map_err(|error| transport_error(&error))?;
    // One stop future for the entire command, not a fresh duration per segment.
    let mut stop: StopSignal<'_> = Box::pin(async {
        if let Some(seconds) = args.duration_seconds {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(seconds)) => Ok(()),
                result = tokio::signal::ctrl_c() => result,
            }
        } else {
            tokio::signal::ctrl_c().await
        }
    });
    let mut retries = 0;
    let mut previous: Option<String> = None;
    loop {
        let identity = tokio::select! {
            result = &mut stop => { result?; controller.stop().await; return Ok(()); }
            result = controller.rebootstrap() => match result {
                Ok(identity) => identity,
                Err(error) => {
                    if !retry_allowed(&error, retries, args.reconnect_retries) {
                        return Err(transport_error(&error));
                    }
                    retries += 1;
                    tokio::select! {
                        result = &mut stop => { result?; controller.stop().await; return Ok(()); }
                        () = tokio::time::sleep(Duration::from_millis(args.reconnect_delay_ms)) => {}
                    }
                    continue;
                }
            }
        };
        let segment = observe_segment(
            args,
            &mut controller,
            &mut stop,
            &identity,
            previous.as_deref(),
        )
        .await?;
        if let Some(error) = segment.recovery_error() {
            previous = Some(identity.observation_id);
            if retry_allowed(error, retries, args.reconnect_retries) {
                retries += 1;
                tokio::select! {
                    result = &mut stop => { result?; controller.stop().await; return segment.finish(); }
                    () = tokio::time::sleep(Duration::from_millis(args.reconnect_delay_ms)) => {}
                }
                continue;
            }
        }
        controller.stop().await;
        segment.finish()?;
        let receipt = controller
            .last_result()
            .ok_or_else(|| invalid("missing segment closure receipt"))?;
        if !receipt.revocation_confirmed || receipt.retention_release_confirmed != Some(true) {
            return Err(invalid(
                "X-ray segment cleanup unconfirmed; session work is unaffected",
            ));
        }
        return Ok(());
    }
}

#[derive(Debug)]
enum IntakeFailure {
    Local(Box<CliError>),
    ConsumerStopped,
    Transport(XrayClientError),
}

impl From<CliError> for IntakeFailure {
    fn from(error: CliError) -> Self {
        Self::Local(Box::new(error))
    }
}

impl IntakeFailure {
    fn into_cli(self) -> CliError {
        match self {
            Self::Local(error) => *error,
            Self::ConsumerStopped => invalid("X-ray consumer stopped; segment is unfinished"),
            Self::Transport(error) => transport_error(&error),
        }
    }
}

struct SegmentResult {
    intake: Result<(), IntakeFailure>,
    export_result: Result<(), ConsumerFailure>,
    stopping: bool,
}

impl SegmentResult {
    // Local intake or export failure is never authorization for a fresh segment,
    // even when another operation concurrently reports a transport failure.
    const fn recovery_error(&self) -> Option<&XrayClientError> {
        if self.stopping
            || matches!(&self.intake, Err(IntakeFailure::Local(_)))
            || matches!(&self.export_result, Err(ConsumerFailure::Local(_)))
        {
            return None;
        }
        if let Err(ConsumerFailure::Payload(error)) = &self.export_result
            && !error.permits_rebootstrap_retry()
        {
            return None;
        }
        match (&self.intake, &self.export_result) {
            (Err(IntakeFailure::Transport(error)), _)
            | (
                Ok(()) | Err(IntakeFailure::ConsumerStopped),
                Err(ConsumerFailure::Payload(error)),
            ) => Some(error),
            _ => None,
        }
    }

    fn finish(self) -> Result<(), CliError> {
        super::finish_delivery(
            self.intake.map_err(IntakeFailure::into_cli),
            self.export_result.map_err(ConsumerFailure::into_cli),
        )
    }
}

type StopSignal<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>>;

async fn observe_segment(
    args: &XrayArgs,
    controller: &mut XrayObservationController,
    stop: &mut StopSignal<'_>,
    identity: &bcode_client::xray::XraySegmentIdentity,
    previous: Option<&str>,
) -> Result<SegmentResult, CliError> {
    let ready = controller
        .recv()
        .await
        .map_err(|error| transport_error(&error))?
        .ok_or_else(|| invalid("missing fresh segment readiness"))?;
    let capabilities: bcode_xray_models::XrayCapabilities =
        serde_json::from_value(ready.event.data.clone())?;
    let range_bytes = u32::try_from(capabilities.max_payload_range_bytes.min(64 * 1024))
        .map_err(|_| invalid("invalid payload range capability"))?;
    let client = controller
        .client()
        .ok_or_else(|| invalid("segment transport unavailable"))?
        .clone();
    let destination = args
        .record
        .as_deref()
        .map(|base| segment_destination(base, identity.ordinal));
    let init = super::export_process::Init::new(
        &client,
        destination,
        identity.observation_id.clone(),
        identity.session_id.clone(),
        previous.map(str::to_owned),
        range_bytes,
    )
    .map_err(ConsumerFailure::into_cli)?;
    let (sender, done, consumer) =
        super::export_process::spawn(init, usize::from(args.consumer_queue_capacity))
            .await
            .map_err(ConsumerFailure::into_cli)?;
    let mut delivery = SegmentDelivery::default();
    let intake = receive_segment(
        args,
        controller,
        stop,
        &sender,
        (done, ready),
        &mut delivery,
        &consumer,
    )
    .await;
    consumer.begin_shutdown();
    let SegmentDelivery { boundary, stopping } = delivery;
    if intake.is_err() {
        // Local failure can leave healthy SSE alive. Establish engine cutoff
        // before dropping it, so the host can retain queued payloads for export.
        // A broken stream cannot establish cutoff and must remain interrupted.
        if !matches!(&intake, Err(IntakeFailure::Transport(_))) && !stopping {
            let _ = controller.finalize().await;
        }
        // Retain old payload authorization until the FIFO consumer is joined.
        controller.interrupt();
    }
    if intake.is_ok()
        && let Some(boundary) = boundary
    {
        let _ = sender.send(ConsumerMessage::Finish(boundary)).await;
    } else if matches!(&intake, Err(IntakeFailure::Transport(_))) {
        let _ = sender.send(ConsumerMessage::Interrupt).await;
    }
    drop(sender);
    // Join payload export and flush before stop can release final retention.
    let export_result = consumer.join().await;
    Ok(SegmentResult {
        intake,
        export_result,
        stopping,
    })
}

#[derive(Default)]
struct SegmentDelivery {
    boundary: Option<bcode_xray_models::XrayFinalization>,
    stopping: bool,
}

async fn receive_segment(
    args: &XrayArgs,
    controller: &mut XrayObservationController,
    stop: &mut StopSignal<'_>,
    sender: &tokio::sync::mpsc::Sender<ConsumerMessage>,
    consumer_input: (
        tokio::sync::oneshot::Receiver<()>,
        bcode_xray_models::XrayEnvelope,
    ),
    delivery: &mut SegmentDelivery,
    consumer: &super::export_process::ExportProcess,
) -> Result<(), IntakeFailure> {
    let (mut done, ready) = consumer_input;
    enqueue(sender, ready)?;
    loop {
        tokio::select! {
            result = stop.as_mut() => {
                consumer.begin_shutdown();
                result.map_err(CliError::from)?;
                delivery.stopping = true;
                let drain = async {
                    controller.finalize().await.map_err(IntakeFailure::Transport)?;
                    while let Some(envelope) = controller.recv().await.map_err(IntakeFailure::Transport)? {
                        remember_boundary(&envelope, &mut delivery.boundary)?;
                        enqueue(sender, envelope)?;
                    }
                    Ok::<(), IntakeFailure>(())
                };
                tokio::time::timeout_at(
                    consumer.shutdown_deadline().min(tokio::time::Instant::now() + Duration::from_secs(args.finalize_timeout_seconds)), drain,
                ).await
                    .map_err(|_| invalid("finalization timed out; segment is unfinished"))??;
                return Ok(());
            }
            _ = &mut done => return Err(IntakeFailure::ConsumerStopped),
            result = controller.recv() => match result {
                Ok(Some(envelope)) => {
                    remember_boundary(&envelope, &mut delivery.boundary)?;
                    enqueue(sender, envelope)?;
                }
                Ok(None) => return Ok(()),
                Err(error) => return Err(IntakeFailure::Transport(error)),
            }
        }
    }
}

const fn retry_allowed(error: &XrayClientError, used: u32, limit: u32) -> bool {
    used < limit && error.permits_rebootstrap_retry()
}

fn segment_destination(base: &Path, ordinal: u64) -> PathBuf {
    if ordinal == 1 {
        return base.to_path_buf();
    }
    let mut name = base.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".segment-{ordinal}"));
    base.with_file_name(name)
}

fn remember_boundary(
    envelope: &bcode_xray_models::XrayEnvelope,
    boundary: &mut Option<bcode_xray_models::XrayFinalization>,
) -> Result<(), CliError> {
    if envelope.event.domain == "xray" && envelope.event.name == "finalized" {
        *boundary = Some(serde_json::from_value(envelope.event.data.clone())?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        ConsumerFailure, IntakeFailure, SegmentResult, XrayClientError, retry_allowed,
        segment_destination,
    };
    #[cfg(unix)]
    use super::{ConsumerRecording, consume_segment_after_gap, enqueue};

    #[cfg(unix)]
    fn close_payload_connection(listener: std::net::TcpListener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            use std::io::Read as _;
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            // Close without response headers: a real payload transport failure.
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overflow_with_payload_transport_failure_stays_terminal_and_interrupted() {
        use super::super::tests::pipeline_envelope;

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("combined-failure");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = close_payload_connection(listener);
        let client = bcode_client::xray::XrayClient::new(&endpoint, "private-test-token").unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        enqueue(
            &sender,
            pipeline_envelope(1, "ready", serde_json::json!({})),
        )
        .unwrap();
        let mut envelope = pipeline_envelope(
            2,
            "payload",
            serde_json::json!({
                "blob": {
                    "schema_version": 1, "payload_id": "blob", "session_id": "session",
                    "content_type": "text/plain", "byte_len": 3,
                    "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                }
            }),
        );
        envelope.event.domain = "plugin".into();
        envelope.event.capture = Some(
            serde_json::from_value(serde_json::json!({
                "schema_version": 1,
                "attachments": [{"kind": "payload_ref", "pointer": "/blob"}]
            }))
            .unwrap(),
        );
        enqueue(&sender, envelope).unwrap();
        // No consumer is running yet: overflow is deterministic, not a timing race.
        let primary = enqueue(
            &sender,
            pipeline_envelope(3, "event", serde_json::json!({})),
        )
        .unwrap_err();
        let primary_message = primary.to_string();
        let intake = Err(IntakeFailure::from(primary));
        client.begin_payload_drain();
        drop(sender);
        let destination = path.clone();
        let runtime = tokio::runtime::Handle::current();
        let consumer = tokio::task::spawn_blocking(move || {
            consume_segment_after_gap(
                receiver,
                &client,
                ConsumerRecording {
                    destination: Some(&destination),
                    observation_id: "observer",
                    session_id: "session",
                    previous: None,
                },
                16 * 1024,
                &runtime,
            )
        });
        let export_result = tokio::time::timeout(std::time::Duration::from_secs(5), consumer)
            .await
            .unwrap()
            .unwrap();
        server.join().unwrap();
        assert!(matches!(
            &export_result,
            Err(ConsumerFailure::Payload(XrayClientError::Transport))
        ));
        let segment = SegmentResult {
            intake,
            export_result,
            stopping: false,
        };
        assert!(matches!(&segment.intake, Err(IntakeFailure::Local(_))));
        assert!(segment.recovery_error().is_none());
        let diagnostic = segment.finish().unwrap_err().to_string();
        assert!(diagnostic.contains(&primary_message));
        assert!(diagnostic.contains("accepted-message drain also failed"));
        assert!(diagnostic.contains(&XrayClientError::Transport.to_string()));
        let manifest = bcode_xray::recording::inspect_bundle(
            &path,
            &bcode_xray::recording::RecordingOptions::default(),
        )
        .unwrap();
        assert_eq!(
            manifest.state,
            bcode_xray::recording::RecordingState::Interrupted
        );
        assert_eq!(manifest.events.envelope_count, 2);
        assert!(!manifest.evidence_complete);
        assert!(manifest.finalization.is_none());
    }

    #[test]
    fn recovery_requires_transport_without_local_failure_or_stop() {
        for intake in [Ok(()), Err(IntakeFailure::ConsumerStopped)] {
            let segment = SegmentResult {
                intake,
                export_result: Err(ConsumerFailure::Payload(XrayClientError::Transport)),
                stopping: false,
            };
            assert!(segment.recovery_error().is_some());
            assert!(segment.finish().is_err());
        }
        let local_export = SegmentResult {
            intake: Err(IntakeFailure::Transport(XrayClientError::Transport)),
            export_result: Err(super::invalid("disk failed").into()),
            stopping: false,
        };
        assert!(local_export.recovery_error().is_none());
        let diagnostic = local_export.finish().unwrap_err().to_string();
        assert!(diagnostic.contains("disk failed"));
        assert!(diagnostic.contains(&XrayClientError::Transport.to_string()));
        let stopped = SegmentResult {
            intake: Ok(()),
            export_result: Err(ConsumerFailure::Payload(XrayClientError::Transport)),
            stopping: true,
        };
        assert!(stopped.recovery_error().is_none());
        assert!(stopped.finish().is_err());
    }

    #[test]
    fn retries_are_transport_only_and_exhaustible() {
        assert!(retry_allowed(&XrayClientError::Transport, 0, 3));
        assert!(!retry_allowed(&XrayClientError::Transport, 3, 3));
        for error in [
            XrayClientError::AdmissionRejected,
            XrayClientError::Http(401),
            XrayClientError::Http(403),
            XrayClientError::Protocol,
            XrayClientError::InvalidCredential,
        ] {
            assert!(!retry_allowed(&error, 0, 3));
        }
    }

    #[test]
    fn segment_bundles_never_reuse_a_predecessor_path() {
        assert_eq!(
            segment_destination(Path::new("/private/capture"), 1),
            PathBuf::from("/private/capture")
        );
        assert_eq!(
            segment_destination(Path::new("/private/capture"), 2),
            PathBuf::from("/private/capture.segment-2")
        );
        let no_recording: Option<&Path> = None;
        assert_eq!(no_recording.map(|base| segment_destination(base, 2)), None);
    }
}
