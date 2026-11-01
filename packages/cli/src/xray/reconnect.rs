//! Explicit transport-only, gapful reattachment through the matching application.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    CliError, ConsumerFailure, ConsumerMessage, ConsumerRecording, XrayArgs,
    consume_segment_after_gap, enqueue, invalid, transport_error,
};
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
        let SegmentResult {
            intake,
            export_result,
            transport_failure,
            stopping,
        } = observe_segment(
            args,
            &mut controller,
            &mut stop,
            &identity,
            previous.as_deref(),
        )
        .await?;
        if let Some(error) = transport_failure {
            previous = Some(identity.observation_id);
            // Export/disk failures are not transport retry authorization.
            if let Err(local_error) = export_result {
                let retryable_export = matches!(&local_error,
                    ConsumerFailure::Payload(error) if error.permits_rebootstrap_retry());
                if !retryable_export {
                    controller.stop().await;
                    return Err(local_error.into_cli());
                }
            }
            if !stopping && retry_allowed(&error, retries, args.reconnect_retries) {
                retries += 1;
                tokio::select! {
                    result = &mut stop => { result?; controller.stop().await; return Err(transport_error(&error)); }
                    () = tokio::time::sleep(Duration::from_millis(args.reconnect_delay_ms)) => {}
                }
                continue;
            }
            controller.stop().await;
            return Err(transport_error(&error));
        }
        controller.stop().await;
        intake?;
        export_result.map_err(ConsumerFailure::into_cli)?;
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

struct SegmentResult {
    intake: Result<(), CliError>,
    export_result: Result<(), ConsumerFailure>,
    transport_failure: Option<XrayClientError>,
    stopping: bool,
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
    let observation_id = identity.observation_id.clone();
    let session_id = identity.session_id.clone();
    let predecessor = previous.map(str::to_owned);
    let runtime = tokio::runtime::Handle::current();
    let (sender, receiver) = tokio::sync::mpsc::channel(usize::from(args.consumer_queue_capacity));
    let (finished, done) = tokio::sync::oneshot::channel();
    let consumer = tokio::task::spawn_blocking(move || {
        let result = consume_segment_after_gap(
            receiver,
            &client,
            ConsumerRecording {
                destination: destination.as_deref(),
                observation_id: &observation_id,
                session_id: &session_id,
                previous: predecessor.as_deref(),
            },
            range_bytes,
            &runtime,
        );
        let _ = finished.send(());
        result
    });
    let mut delivery = SegmentDelivery::default();
    let intake = receive_segment(args, controller, stop, &sender, done, ready, &mut delivery).await;
    let SegmentDelivery {
        boundary,
        mut transport_failure,
        stopping,
    } = delivery;
    if intake.is_err() {
        // Local failure can leave healthy SSE alive. Establish engine cutoff
        // before dropping it, so the host can retain queued payloads for export.
        // A broken stream cannot establish cutoff and must remain interrupted.
        if transport_failure.is_none() && !stopping {
            let _ = controller.finalize().await;
        }
        // Retain old payload authorization until the FIFO consumer is joined.
        controller.interrupt();
    }
    if intake.is_ok()
        && let Some(boundary) = boundary
    {
        let _ = sender.send(ConsumerMessage::Finish(boundary)).await;
    } else if transport_failure.is_some() {
        let _ = sender.send(ConsumerMessage::Interrupt).await;
    }
    drop(sender);
    // Join payload export and flush before stop can release final retention.
    let mut export_result = consumer
        .await
        .map_err(|_| invalid("recording consumer panicked; segment is unfinished"))?;
    if transport_failure.is_none() && !stopping {
        export_result = match export_result {
            Err(ConsumerFailure::Payload(error)) if error.permits_rebootstrap_retry() => {
                transport_failure = Some(error);
                Ok(())
            }
            other => other,
        };
    }
    Ok(SegmentResult {
        intake,
        export_result,
        transport_failure,
        stopping,
    })
}

#[derive(Default)]
struct SegmentDelivery {
    boundary: Option<bcode_xray_models::XrayFinalization>,
    transport_failure: Option<XrayClientError>,
    stopping: bool,
}

async fn receive_segment(
    args: &XrayArgs,
    controller: &mut XrayObservationController,
    stop: &mut StopSignal<'_>,
    sender: &tokio::sync::mpsc::Sender<ConsumerMessage>,
    mut done: tokio::sync::oneshot::Receiver<()>,
    ready: bcode_xray_models::XrayEnvelope,
    delivery: &mut SegmentDelivery,
) -> Result<(), CliError> {
    enqueue(sender, ready)?;
    loop {
        tokio::select! {
            result = stop.as_mut() => {
                result?;
                delivery.stopping = true;
                controller.finalize().await.map_err(|error| transport_error(&error))?;
                let drain = async {
                    while let Some(envelope) = controller.recv().await.map_err(|error| transport_error(&error))? {
                        remember_boundary(&envelope, &mut delivery.boundary)?;
                        enqueue(sender, envelope)?;
                    }
                    Ok::<(), CliError>(())
                };
                tokio::time::timeout(Duration::from_secs(args.finalize_timeout_seconds), drain).await
                    .map_err(|_| invalid("finalization timed out; segment is unfinished"))??;
                return Ok(());
            }
            _ = &mut done => return Err(invalid("X-ray local consumer failed; segment is unfinished")),
            result = controller.recv() => match result {
                Ok(Some(envelope)) => {
                    remember_boundary(&envelope, &mut delivery.boundary)?;
                    enqueue(sender, envelope)?;
                }
                Ok(None) => return Ok(()),
                Err(error) => {
                    delivery.transport_failure = Some(error);
                    return Err(invalid("X-ray transport interrupted; capture gap is unrecoverable"));
                }
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

    use super::{XrayClientError, retry_allowed, segment_destination};

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
