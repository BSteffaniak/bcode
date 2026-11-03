//! Workflow observed admission and recorder worker. One principal owns admission,
//! capture pages, payload exports, cutoff and revocation.
use super::budget::until;
use super::{
    Arc, AtomicBool, CONFIG, CaptureLimits, Duration, EvalXrayAggregate, ExecutionCapture,
    Ordering, Path,
};

pub struct WorkflowWatch {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Option<EvalXrayAggregate>>>,
}

impl WorkflowWatch {
    /// Whether this executor requested capture. Goal preparation is then required;
    /// never silently fall back to atomic unobserved admission followed by attach.
    pub(crate) fn requested() -> bool {
        CONFIG.with(|slot| slot.borrow().is_some())
    }

    /// Admit one plugin-prepared typed run through the ordinary authorized observed
    /// start boundary, then transfer the SAME connection/Ready to the recorder.
    /// This method never replays admission after a missing/ambiguous response.
    pub(crate) fn admit(
        start: bcode_workflow::WorkflowRunStartRequest,
        directory: &Path,
        binary: &Path,
        environment: impl Iterator<Item = (String, String)>,
    ) -> Result<(serde_json::Value, Self), &'static str> {
        let config = CONFIG
            .with(|slot| slot.borrow().clone())
            .ok_or("capture_not_requested")?;
        let environment_matches = environment.into_iter().all(|(key, value)| {
            std::env::var_os(key).is_some_and(|actual| actual == std::ffi::OsStr::new(&value))
        });
        let binary_matches = std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .zip(std::fs::canonicalize(binary).ok())
            .is_some_and(|(a, b)| a == b);
        // An HTTP session override is not an authenticated IPC workflow principal.
        if !environment_matches || !binary_matches || config.client.is_some() {
            return Err("exact_workflow_connection_unavailable");
        }
        let expected_run = start
            .run_id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or("prepared_run_missing_stable_identity")?;
        let expected_session = start.parent_session_id.to_string();
        let directory = directory.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (admission_tx, admission_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = admission_tx.send(Err("capture_runtime_unavailable"));
                return None;
            };
            let result = runtime.block_on(async {
                let application = bcode_client::BcodeClient::default_endpoint();
                let connection = until(
                    tokio::time::Instant::now() + Duration::from_secs(30),
                    application.connect("bcode-eval-xray-execution"),
                )
                .await;
                let Ok(Ok(mut connection)) = connection else {
                    let _ = admission_tx.send(Err("workflow_connection_unavailable"));
                    return None;
                };
                let started = until(
                    tokio::time::Instant::now() + Duration::from_secs(30),
                    connection.start_workflow_run_observed(
                        start,
                        bcode_xray_models::execution_watch::ExecutionWatchStartOptions {
                            schema_version: 1,
                            recursive: true,
                            deep_capture: config.deep_capture,
                        },
                    ),
                )
                .await;
                let Ok(Ok((started, ready))) = started else {
                    let _ = admission_tx.send(Err("observed_admission_unresolved_no_retry"));
                    return None;
                };
                // Preserve acknowledged admission even if recording setup later
                // fails; it is never permission to submit the run a second time.
                let admitted = serde_json::json!({
                    "run_id": started.run.run_id, "runtime_work_id": started.runtime_work_id,
                    "status": "admitted (not completed)",
                });
                if !observed_ready_matches(
                    &ready,
                    &expected_run,
                    &expected_session,
                    config.deep_capture,
                ) || admitted["run_id"].as_str() != Some(expected_run.as_str())
                {
                    let _ = until(
                        tokio::time::Instant::now() + Duration::from_secs(5),
                        connection.revoke_execution_watch(
                            bcode_xray_models::execution_watch::RevokeExecutionWatchRequest {
                                schema_version: 1,
                                watch_id: ready.watch_id,
                            },
                        ),
                    )
                    .await;
                    let _ =
                        admission_tx.send(Err("observed_admission_receipt_inconsistent_no_retry"));
                    return None;
                }
                // Admission is already an authoritative, independent fact. A
                // local recorder failure must not hide the run from evaluation
                // or prevent its ordinary timeout/cancellation reconciliation.
                let _ = admission_tx.send(Ok(admitted));
                let capture = Box::pin(until(
                    tokio::time::Instant::now() + Duration::from_secs(5),
                    Box::pin(ExecutionCapture::from_ready(
                        connection,
                        ready,
                        &directory,
                        CaptureLimits::default(),
                    )),
                ))
                .await;
                let Ok(Ok(mut capture)) = capture else {
                    return None;
                };
                while !worker_stop.load(Ordering::Acquire) {
                    if let Err(category) = capture.poll().await {
                        // Fixed consumer categories only, never raw IPC/provider errors.
                        let _ = std::fs::write(directory.join("xray-execution-poll-failure.json"),
                            serde_json::json!({"schema_version":1,"category":category,"evidence_complete":false}).to_string());
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                capture.finish().await.ok()
            });
            if result.is_none() {
                let _ = std::fs::write(directory.join("xray-execution-unavailable.json"),
                    b"{\"schema_version\":1,\"evidence_complete\":false,\"status\":\"watch_admission_or_consumer_failed\"}");
            }
            result
        });
        let watch = Self {
            stop,
            worker: Some(worker),
        };
        let admitted = admission_rx
            .recv()
            .map_err(|_| "observed_admission_unresolved_no_retry")??;
        Ok((admitted, watch))
    }

    pub(crate) fn finish(mut self) -> Option<EvalXrayAggregate> {
        self.stop.store(true, Ordering::Release);
        self.worker.take()?.join().ok()?
    }
}

fn observed_ready_matches(
    ready: &bcode_xray_models::execution_watch::ExecutionWatchReady,
    run: &str,
    session: &str,
    deep_capture: bool,
) -> bool {
    ready.schema_version == bcode_xray_models::execution_watch::EXECUTION_WATCH_SCHEMA_VERSION
        && ready.scope.root_run_id == run
        && ready.scope.recursive
        && ready.scope.deep_capture == deep_capture
        && ready.root_session_id.as_deref() == Some(session)
        && !ready.prior_execution_gap
        && ready.cursor == 0
        && !ready.watch_id.is_empty()
        && ready.max_page_items > 0
        && ready.max_page_bytes > 0
}

impl Drop for WorkflowWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_xray_models::execution_watch::{ExecutionWatchReady, ExecutionWatchScope};

    #[test]
    fn observed_start_requires_exact_pre_dispatch_authorized_scope() {
        let mut ready = ExecutionWatchReady {
            schema_version: 1,
            watch_id: "watch".into(),
            scope: ExecutionWatchScope {
                root_run_id: "run".into(),
                recursive: true,
                deep_capture: true,
            },
            root_session_id: Some("session".into()),
            cursor: 0,
            prior_execution_gap: false,
            max_page_items: 16,
            max_page_bytes: 65536,
        };
        assert!(observed_ready_matches(&ready, "run", "session", true));
        assert!(!observed_ready_matches(&ready, "foreign", "session", true));
        assert!(!observed_ready_matches(&ready, "run", "foreign", true));
        assert!(!observed_ready_matches(&ready, "run", "session", false));
        ready.prior_execution_gap = true;
        assert!(!observed_ready_matches(&ready, "run", "session", true));
        ready.prior_execution_gap = false;
        ready.scope.recursive = false;
        assert!(!observed_ready_matches(&ready, "run", "session", true));
    }
}
