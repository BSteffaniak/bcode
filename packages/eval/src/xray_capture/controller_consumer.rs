//! Ordinary application-client segment consumption. Lifecycle is owned by E's
//! controller; eval owns only sink writes and evidence outcomes.
use super::{
    AtomicBool, Duration, EvalXrayEvidence, JsonlBundleRecorder, Ordering, Path, RecordingOptions,
    RecordingSink,
};
use bcode_client::xray::XrayObservationController;

pub(super) async fn consume(
    application: &bcode_client::BcodeClient,
    session: &str,
    path: &Path,
    deep: bool,
    stop: &AtomicBool,
    ready_tx: &std::sync::mpsc::SyncSender<()>,
) -> EvalXrayEvidence {
    let mut evidence = EvalXrayEvidence::new(session.into());
    let Ok(session_id) = session.parse() else {
        let _ = ready_tx.send(());
        return evidence;
    };
    let mut controller = XrayObservationController::new(application, session_id, deep);
    let Ok(Ok(identity)) =
        tokio::time::timeout(Duration::from_secs(25), controller.rebootstrap()).await
    else {
        let _ = ready_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), controller.stop()).await;
        return evidence;
    };
    let Ok(mut sink) = JsonlBundleRecorder::create(
        path,
        &identity.observation_id,
        session,
        RecordingOptions::default(),
    ) else {
        evidence.status = "recording_unavailable";
        let _ = ready_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), controller.stop()).await;
        return evidence;
    };
    let mut payloads_ok = true;
    let mut omitted = false;
    let mut cutoff = None;
    let mut failed = false;
    // rebootstrap verifies readiness and retains the ready envelope for recv.
    match tokio::time::timeout(Duration::from_secs(5), controller.recv()).await {
        Ok(Ok(Some(envelope))) if sink.append(&envelope).is_ok() => {
            evidence.envelopes = 1;
            evidence.ready_before_submission = true;
        }
        _ => failed = true,
    }
    let _ = ready_tx.send(());
    while !failed && !stop.load(Ordering::Acquire) {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(25)) => {},
            result = controller.recv() => match result {
                Ok(Some(envelope)) => {
                    if sink.append(&envelope).is_err() { failed = true; continue; }
                    omitted |= envelope.event.name.ends_with("_omitted");
                    let exported = tokio::time::timeout(
                        Duration::from_secs(15),
                        controller.record_payloads(&envelope, &mut sink, 64 * 1024),
                    ).await;
                    payloads_ok &= matches!(exported, Ok(Ok(())));
                    evidence.envelopes += 1;
                    if envelope.event.domain == "xray" && envelope.event.name == "finalized" {
                        cutoff = serde_json::from_value(envelope.event.data).ok();
                    }
                }
                Ok(None) => break,
                Err(_) => failed = true,
            }
        }
    }
    if !failed && cutoff.is_none() {
        let drain = async {
            controller.finalize().await.ok()?;
            while let Some(envelope) = controller.recv().await.ok()? {
                sink.append(&envelope).ok()?;
                omitted |= envelope.event.name.ends_with("_omitted");
                payloads_ok &= controller
                    .record_payloads(&envelope, &mut sink, 64 * 1024)
                    .await
                    .is_ok();
                evidence.envelopes += 1;
                if envelope.event.domain == "xray" && envelope.event.name == "finalized" {
                    cutoff = serde_json::from_value(envelope.event.data).ok();
                }
            }
            Some(())
        };
        failed = !matches!(
            tokio::time::timeout(Duration::from_secs(20), drain).await,
            Ok(Some(()))
        );
    }
    let manifest = cutoff
        .as_ref()
        .filter(|_| !failed && payloads_ok && !omitted)
        .and_then(|cutoff| sink.finalize(cutoff).ok());
    if manifest.is_none() {
        let _ = sink.interrupt();
    }
    evidence.settle(cutoff, manifest.is_some());
    evidence.payloads_complete =
        payloads_ok && !omitted && manifest.is_some_and(|manifest| manifest.evidence_complete);
    let released = tokio::time::timeout(Duration::from_secs(5), controller.stop())
        .await
        .is_ok();
    if let Some(result) = controller.last_result() {
        evidence.observation_released =
            released && result.retention_release_confirmed == Some(true);
        evidence.payloads_complete &= result.opaque_capture_events == 0;
    }
    evidence.evidence_complete =
        evidence.delivery_complete && evidence.payloads_complete && evidence.continuity_complete;
    if !evidence.evidence_complete {
        evidence.status = "evidence_incomplete";
    }
    evidence
}
