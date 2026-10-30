//! Eval is an ordinary live X-ray consumer, not an execution or usage authority.
//!
//! Configuration is request-only. No credential, endpoint or provider configuration is
//! serialized into eval artifacts. Capture failure never changes the task's exit code.
use bcode_xray::recording::{JsonlBundleRecorder, RecordingOptions, RecordingSink};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use bcode_client::xray::XrayClient;
use bcode_xray_models::{XRAY_SCHEMA_VERSION, XrayFinalization, XrayObserveRequest};
use serde::Serialize;

thread_local! {
    static CONFIG: RefCell<Option<EvalXrayCapture>> = const { RefCell::new(None) };
}

/// Explicit request-only diagnostic configuration. The client must target the exact
/// subject daemon's artifact and state location. No daemon discovery is inferred.
#[derive(Clone, Debug)]
pub struct EvalXrayCapture {
    /// Optional explicit public client override. `None` bootstraps through the
    /// actual subject application client after session creation.
    pub client: Option<XrayClient>,
    /// Explicit authorization to retain sensitive content during observation.
    pub deep_capture: bool,
}

/// Run synchronous eval work with explicit live capture configuration.
///
/// Nested scopes restore the prior configuration, including during unwinding.
/// Configuration is not inherited by unrelated threads or subsequent eval runs.
pub fn with_xray_capture<T>(capture: EvalXrayCapture, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<EvalXrayCapture>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONFIG.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(CONFIG.with(|slot| slot.replace(Some(capture))));
    run()
}

/// Evidence delivery receipt, intentionally independent of grading and canonical usage.
#[derive(Debug, Serialize)]
// These are independently reported evidence dimensions, not mutually exclusive
// behavior switches. Keep the version-1 receipt's flat boolean fields stable so
// consumers can distinguish readiness, delivery, payload, continuity and child
// coverage failures even when the aggregate evidence result is incomplete.
#[allow(clippy::struct_excessive_bools)]
pub struct EvalXrayEvidence {
    /// Eval-derived contract, not a session storage schema.
    pub schema_version: u16,
    /// Exact observed session; workflow children require independent observations.
    pub session_id: String,
    /// Whether attach returned its readiness barrier before execution submission.
    pub ready_before_submission: bool,
    /// Whether the engine cutoff arrived and local JSONL was flushed successfully.
    pub delivery_complete: bool,
    /// Sensitive payload bundles are not reconstructed from canonical transcripts.
    pub payloads_complete: bool,
    /// True only when delivery, payload export and continuity all succeeded.
    pub evidence_complete: bool,
    /// False after a deliberate daemon restart; no durable replay is implied.
    pub continuity_complete: bool,
    /// Session-local receipt, never a certificate of workflow subtree coverage.
    pub workflow_children_complete: bool,
    /// Fixed secret-safe category, never arbitrary transport or payload text.
    pub status: &'static str,
    /// Number of retained envelopes (including control events).
    pub envelopes: u64,
    /// Engine cutoff is evidence only, not a task-completion receipt.
    pub finalization: Option<XrayFinalization>,
}

impl EvalXrayEvidence {
    const fn new(session_id: String) -> Self {
        Self {
            schema_version: 1,
            session_id,
            ready_before_submission: false,
            delivery_complete: false,
            payloads_complete: false,
            evidence_complete: false,
            continuity_complete: true,
            workflow_children_complete: false,
            status: "attach_unavailable",
            envelopes: 0,
            finalization: None,
        }
    }

    fn settle(&mut self, finalization: Option<XrayFinalization>, flushed: bool) {
        self.delivery_complete = flushed
            && self.ready_before_submission
            && finalization.as_ref().is_some_and(|receipt| {
                receipt.dropped_events == 0
                    && receipt.hub_contention_drops == 0
                    && receipt.evicted_payloads == 0
            });
        self.status = if self.delivery_complete {
            "delivery_complete"
        } else {
            "evidence_incomplete"
        };
        self.finalization = finalization;
    }
}

/// A session-local capture. Drop stops observation only, never execution.
pub struct Capture {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<EvalXrayEvidence>>,
    receipt: PathBuf,
}

impl Capture {
    /// Wait for ready before returning to prompt submission. Attachment failure is
    /// retained as evidence failure rather than being mistaken for task failure.
    pub(crate) fn start(
        session: &str,
        directory: &Path,
        application: &bcode_client::BcodeClient,
    ) -> Option<Self> {
        let application = application.clone();
        let config = CONFIG.with(|slot| slot.borrow().clone())?;
        let session = session.to_owned();
        let receipt = directory.join("xray-evidence.json");
        let path = directory.join("xray");
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir().ok()?.join(path)
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut evidence = EvalXrayEvidence::new(session.clone());
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = ready_tx.send(());
                return evidence;
            };
            runtime.block_on(async {
                let mut endpoint_lease = None;
                let client = if let Some(client) = config.client {
                    client
                } else {
                    let Ok(session_id) = evidence.session_id.parse() else {
                        let _ = ready_tx.send(());
                        return evidence;
                    };
                    let opened = tokio::time::timeout(Duration::from_secs(20),
                        application.open_xray_endpoint(session_id, config.deep_capture)).await;
                    let Ok(Ok(endpoint)) = opened else {
                        let _ = ready_tx.send(());
                        return evidence;
                    };
                    let client = read_bootstrap_client(endpoint.info());
                    let Ok(client) = client else {
                        let _ = endpoint.close().await;
                        let _ = ready_tx.send(());
                        return evidence;
                    };
                    endpoint_lease = Some(endpoint);
                    client
                };
                let outcome = async {
                let attached = tokio::time::timeout(Duration::from_secs(20), client.observe(XrayObserveRequest {
                    schema_version: XRAY_SCHEMA_VERSION, session_id: session,
                    deep_capture: config.deep_capture,
                })).await;
                let Ok(Ok(mut stream)) = attached else {
                    let _ = ready_tx.send(());
                    return evidence;
                };
                let Ok(mut sink) = JsonlBundleRecorder::create(&path, stream.observation_id(), &evidence.session_id, RecordingOptions::default()) else {
                    evidence.status = "recording_unavailable";
                    let _ = ready_tx.send(());
                    return evidence;
                };
                let first = stream.recv().await;
                if !matches!(first, Ok(Some(ref envelope)) if sink.append(envelope).is_ok()) {
                    evidence.status = "recording_unavailable";
                    let _ = ready_tx.send(());
                    return evidence;
                }
                evidence.envelopes = 1;
                evidence.ready_before_submission = true;
                let _ = ready_tx.send(());
                let mut payloads_ok = true;
                let mut semantic_omission = false;
                loop {
                    // Transport and heartbeats NEVER enter the eval progress clock.
                    if worker_stop.load(Ordering::Acquire) { break; }
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_millis(25)) => {},
                        result = stream.recv() => match result {
                            Ok(Some(envelope)) => {
                                if sink.append(&envelope).is_err() {
                                    evidence.status = "recording_incomplete";
                                    return evidence;
                                }
                                semantic_omission |= envelope.event.name.ends_with("_omitted");
                                payloads_ok &= matches!(tokio::time::timeout(Duration::from_secs(15),
                                    client.record_payloads(&envelope, &mut sink, 64 * 1024)).await, Ok(Ok(())));
                                evidence.envelopes += 1;
                            }
                            Ok(None) => break,
                            Err(_) => { evidence.status = "transport_incomplete"; return evidence; }
                        }
                    }
                }
                let drain = async {
                    client.finalize(stream.observation_id()).await.ok()?;
                    while let Some(envelope) = stream.recv().await.ok()? {
                        sink.append(&envelope).ok()?;
                        semantic_omission |= envelope.event.name.ends_with("_omitted");
                        payloads_ok &= matches!(tokio::time::timeout(Duration::from_secs(15),
                            client.record_payloads(&envelope, &mut sink, 64 * 1024)).await, Ok(Ok(())));
                        evidence.envelopes += 1;
                    }
                    Some(())
                };
                let drained = matches!(tokio::time::timeout(Duration::from_secs(15), drain).await, Ok(Some(())));
                let finalization = stream.finalization().cloned();
                let manifest = finalization.as_ref().filter(|_| drained && payloads_ok && !semantic_omission)
                    .and_then(|boundary| sink.finalize(boundary).ok());
                if manifest.is_none() { let _ = sink.interrupt(); }
                if stream.finalization().is_some() {
                    let _ = client.release(stream.observation_id()).await;
                }
                evidence.settle(finalization, manifest.is_some());
                evidence.payloads_complete = payloads_ok && !semantic_omission
                    && manifest.is_some_and(|manifest| manifest.evidence_complete);
                evidence.evidence_complete = evidence.delivery_complete
                    && evidence.payloads_complete && evidence.continuity_complete;
                if !evidence.evidence_complete { evidence.status = "evidence_incomplete"; }
                evidence
                }.await;
                if let Some(endpoint) = endpoint_lease {
                    let _ = tokio::time::timeout(Duration::from_secs(5), endpoint.close()).await;
                }
                outcome
            })
        });
        // The worker has its own bounded attach deadline. Disconnection is also a
        // settled attach failure, not permission to claim readiness.
        let _ = ready_rx.recv();
        Some(Self {
            stop,
            worker: Some(worker),
            receipt,
        })
    }

    /// Workflow executors can deliberately target another executable and environment.
    /// Never bootstrap against this consumer's default daemon in that case.
    pub(crate) fn start_workflow(
        session: &str,
        directory: &Path,
        binary: &Path,
        environment: impl Iterator<Item = (String, String)>,
    ) -> Option<Self> {
        let config = CONFIG.with(|slot| slot.borrow().clone())?;
        let matching_environment = environment.into_iter().all(|(key, value)| {
            std::env::var_os(key).is_some_and(|actual| actual == std::ffi::OsStr::new(&value))
        });
        let matching_binary = std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .zip(std::fs::canonicalize(binary).ok())
            .is_some_and(|(consumer, subject)| consumer == subject);
        if config.client.is_none() && !(matching_binary && matching_environment) {
            let mut evidence = EvalXrayEvidence::new(session.into());
            evidence.status = "workflow_exact_daemon_bootstrap_unavailable";
            if let Ok(bytes) = serde_json::to_vec_pretty(&evidence) {
                let _ = std::fs::write(directory.join("xray-evidence.json"), bytes);
            }
            return None;
        }
        Self::start(
            session,
            directory,
            &bcode_client::BcodeClient::default_endpoint(),
        )
    }

    pub(crate) fn finish_with_restart_gap(mut self) -> Option<EvalXrayEvidence> {
        let mut evidence = self.finish_inner()?;
        evidence.continuity_complete = false;
        evidence.evidence_complete = false;
        evidence.status = "daemon_restart_gap";
        std::fs::write(&self.receipt, serde_json::to_vec_pretty(&evidence).ok()?).ok()?;
        Some(evidence)
    }

    pub(crate) fn finish(mut self) -> Option<EvalXrayEvidence> {
        self.finish_inner()
    }

    fn finish_inner(&mut self) -> Option<EvalXrayEvidence> {
        self.stop.store(true, Ordering::Release);
        let evidence = self.worker.take()?.join().ok()?;
        let bytes = serde_json::to_vec_pretty(&evidence).ok()?;
        std::fs::write(&self.receipt, bytes).ok()?;
        Some(evidence)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.finish_inner();
    }
}

fn read_bootstrap_client(endpoint: &bcode_ipc::XrayEndpointInfo) -> Result<XrayClient, ()> {
    use std::io::Read as _;
    let path = &endpoint.credential_file;
    let before = std::fs::symlink_metadata(path).map_err(|_| ())?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(());
    }
    let file = std::fs::File::open(path).map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.mode() & 0o077 != 0
            || before.dev() != metadata.dev()
            || before.ino() != metadata.ino()
        {
            return Err(());
        }
    }
    #[cfg(not(unix))]
    {
        return Err(());
    }
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() > 4096 {
        return Err(());
    }
    let token = String::from_utf8(bytes).map_err(|_| ())?;
    XrayClient::new(&endpoint.endpoint, token.trim()).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_configuration_is_restored_after_nested_scope() {
        let client = XrayClient::new("http://127.0.0.1:12345/", "test-token").unwrap();
        assert!(CONFIG.with(|slot| slot.borrow().is_none()));
        with_xray_capture(
            EvalXrayCapture {
                client: Some(client.clone()),
                deep_capture: false,
            },
            || {
                assert!(!CONFIG.with(|slot| slot.borrow().as_ref().unwrap().deep_capture));
                with_xray_capture(
                    EvalXrayCapture {
                        client: Some(client),
                        deep_capture: true,
                    },
                    || {
                        assert!(CONFIG.with(|slot| slot.borrow().as_ref().unwrap().deep_capture));
                    },
                );
                assert!(!CONFIG.with(|slot| slot.borrow().as_ref().unwrap().deep_capture));
            },
        );
        assert!(CONFIG.with(|slot| slot.borrow().is_none()));
    }

    #[test]
    fn delivery_and_payload_completeness_are_not_grading() {
        let mut evidence = EvalXrayEvidence::new("session".into());
        evidence.ready_before_submission = true;
        evidence.settle(
            Some(XrayFinalization {
                observation_id: "observation".into(),
                final_sequence: 2,
                dropped_events: 0,
                hub_contention_drops: 0,
                evicted_payloads: 0,
            }),
            true,
        );
        assert!(evidence.delivery_complete);
        assert!(!evidence.payloads_complete);
        evidence.settle(None, true);
        assert!(!evidence.delivery_complete);
    }

    #[test]
    fn cutoff_with_losses_is_incomplete() {
        let mut evidence = EvalXrayEvidence::new("session".into());
        evidence.ready_before_submission = true;
        evidence.settle(
            Some(XrayFinalization {
                observation_id: "observation".into(),
                final_sequence: 4,
                dropped_events: 1,
                hub_contention_drops: 0,
                evicted_payloads: 0,
            }),
            true,
        );
        assert!(!evidence.delivery_complete);
    }

    #[test]
    fn recording_preserves_origin_correlation_and_ready_order() {
        use bcode_xray_models::{XrayCorrelation, XrayEnvelope, XrayEvent};
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().canonicalize().unwrap().join("bundle");
        let mut sink: Box<dyn RecordingSink> = Box::new(
            JsonlBundleRecorder::create(
                &bundle,
                "observation",
                "session",
                RecordingOptions::default(),
            )
            .unwrap(),
        );
        let correlation = XrayCorrelation {
            session_id: "session".into(),
            run_id: Some("run".into()),
            activation_id: Some("activation".into()),
            attempt_id: Some("attempt".into()),
            ..XrayCorrelation::default()
        };
        let ready = XrayEnvelope {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: "observation".into(),
            sequence: 1,
            timestamp_unix_ms: 0,
            event: XrayEvent {
                domain: "xray".into(),
                name: "ready".into(),
                correlation: correlation.clone(),
                data: serde_json::json!({}),
                sensitive: false,
            },
        };
        sink.append(&ready).unwrap();
        let interrupted = sink.interrupt().unwrap();
        assert!(!interrupted.evidence_complete);
        let retained = std::fs::read_to_string(bundle.join("events.jsonl")).unwrap();
        let envelope: XrayEnvelope = serde_json::from_str(retained.trim()).unwrap();
        assert_eq!(envelope.sequence, 1);
        assert_eq!(envelope.event.correlation, correlation);
    }

    fn capture_fixture(workflow: bool, restart_gap: bool) -> EvalXrayEvidence {
        use bcode_xray_models::{XrayCapabilities, XrayCorrelation, XrayEnvelope, XrayEvent};
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 8192];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            let envelope = |sequence, name: &str, data| XrayEnvelope {
                schema_version: XRAY_SCHEMA_VERSION,
                observation_id: "observation".into(),
                sequence,
                timestamp_unix_ms: 0,
                event: XrayEvent {
                    domain: "xray".into(),
                    name: name.into(),
                    data,
                    sensitive: false,
                    correlation: XrayCorrelation {
                        session_id: "session".into(),
                        ..Default::default()
                    },
                },
            };
            let ready = envelope(
                1,
                "ready",
                serde_json::to_value(XrayCapabilities {
                    schema_version: XRAY_SCHEMA_VERSION,
                    deep_capture: false,
                    queue_capacity: 8,
                    max_event_bytes: 65536,
                    max_payload_bytes: 65536,
                    max_retained_payload_bytes: 65536,
                    max_retained_payloads: 8,
                    max_payload_range_bytes: 65536,
                    durable_replay: false,
                })
                .unwrap(),
            );
            write!(
                stream,
                "data: {}\n\n",
                serde_json::to_string(&ready).unwrap()
            )
            .unwrap();
            stream.flush().unwrap();
            let (mut finalize, _) = listener.accept().unwrap();
            let _ = finalize.read(&mut request).unwrap();
            finalize
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            let finished = envelope(
                2,
                "finalized",
                serde_json::to_value(XrayFinalization {
                    observation_id: "observation".into(),
                    final_sequence: 2,
                    dropped_events: 0,
                    hub_contention_drops: 0,
                    evicted_payloads: 0,
                })
                .unwrap(),
            );
            write!(
                stream,
                "data: {}\n\n",
                serde_json::to_string(&finished).unwrap()
            )
            .unwrap();
            stream.flush().unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().canonicalize().unwrap();
        let client = XrayClient::new(&endpoint, "test-token").unwrap();
        let evidence = with_xray_capture(
            EvalXrayCapture {
                client: Some(client),
                deep_capture: false,
            },
            || {
                let capture = if workflow {
                    Capture::start_workflow(
                        "session",
                        &destination,
                        Path::new("/different-subject"),
                        std::iter::empty(),
                    )
                } else {
                    Capture::start(
                        "session",
                        &destination,
                        &bcode_client::BcodeClient::default_endpoint(),
                    )
                }
                .unwrap();
                if restart_gap {
                    capture.finish_with_restart_gap().unwrap()
                } else {
                    capture.finish().unwrap()
                }
            },
        );
        server.join().unwrap();
        assert!(directory.path().join("xray/manifest.json").is_file());
        evidence
    }

    #[test]
    fn direct_submission_path_waits_for_ready_and_records_confirmed_cutoff() {
        let evidence = capture_fixture(false, false);
        assert!(evidence.ready_before_submission);
        assert!(evidence.delivery_complete);
        assert!(evidence.payloads_complete);
        assert!(evidence.evidence_complete);
        assert!(!evidence.workflow_children_complete);
    }

    #[test]
    fn workflow_admission_path_uses_explicit_override_without_guessing_subject_daemon() {
        let evidence = capture_fixture(true, false);
        assert!(evidence.ready_before_submission);
        assert!(evidence.evidence_complete);
        // Root delivery must never promote unsupported child coverage.
        assert!(!evidence.workflow_children_complete);
    }

    #[test]
    fn restart_boundary_cannot_claim_continuity_or_complete_evidence() {
        let evidence = capture_fixture(false, true);
        assert!(evidence.delivery_complete);
        assert!(!evidence.continuity_complete);
        assert!(!evidence.evidence_complete);
        assert_eq!(evidence.status, "daemon_restart_gap");
    }

    #[test]
    fn disabled_capture_has_no_worker_or_artifact() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            Capture::start(
                "session",
                directory.path(),
                &bcode_client::BcodeClient::default_endpoint()
            )
            .is_none()
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
