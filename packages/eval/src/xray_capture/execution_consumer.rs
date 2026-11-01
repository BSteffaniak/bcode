//! Connection-principal-bound consumer of generic execution diagnostics.
//! The executor must open this against a prepared, undispatched run. Opening
//! after the current atomic goal admission cannot certify pre-dispatch coverage.
use super::budget::until;
use super::{
    CaptureLimits, Duration, EvalXrayAggregate, EvalXrayAggregation, EvalXrayEvidence,
    JsonlBundleRecorder, Path, PathBuf, RecordingOptions, RecordingSink, XRAY_SCHEMA_VERSION,
    XrayFinalization,
};
use bcode_xray_models::execution_watch::{
    EXECUTION_WATCH_SCHEMA_VERSION, ExecutionWatchEvent, ExecutionWatchReady,
    ExportExecutionWatchPayloadRequest, FinalizeExecutionWatchRequest, OpenExecutionWatchRequest,
    ReadExecutionWatchRequest, RevokeExecutionWatchRequest,
};
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

struct Recording {
    run: String,
    session: String,
    evidence: EvalXrayEvidence,
    sink: JsonlBundleRecorder,
    payloads_ok: bool,
    cutoff: Option<XrayFinalization>,
}

/// Bounded live consumer. The same authenticated connection owns every operation.
/// This is an eval recorder, not an execution scheduler or reattach controller.
pub struct ExecutionCapture {
    connection: bcode_client::ClientConnection,
    ready: ExecutionWatchReady,
    cursor: u64,
    directory: PathBuf,
    recordings: BTreeMap<String, Recording>,
    aggregate: EvalXrayAggregation,
    limits: CaptureLimits,
    final_inventory_complete: bool,
    revoked: bool,
}

impl ExecutionCapture {
    /// Open before activating a prepared run. The connection must target the exact
    /// subject artifact/state location. No default-daemon inference is performed.
    ///
    /// # Errors
    /// Returns a fixed secret-safe failure on unavailable authorization, unsupported
    /// readiness, absent private output directory or inconsistent requested scope.
    pub async fn open(
        mut connection: bcode_client::ClientConnection,
        request: OpenExecutionWatchRequest,
        directory: &Path,
        limits: CaptureLimits,
    ) -> Result<Self, &'static str> {
        let ready = until(
            tokio::time::Instant::now() + Duration::from_secs(15),
            connection.open_execution_watch(request.clone()),
        )
        .await
        .map_err(|_| "watch_admission_unresolved_no_retry")?
        .map_err(|_| "watch_admission_unavailable")?;
        if ready.scope != request.scope {
            let _ = until(
                tokio::time::Instant::now() + Duration::from_secs(5),
                connection.revoke_execution_watch(RevokeExecutionWatchRequest {
                    schema_version: EXECUTION_WATCH_SCHEMA_VERSION,
                    watch_id: ready.watch_id,
                }),
            )
            .await;
            return Err("watch_scope_mismatch");
        }
        Self::from_ready(connection, ready, directory, limits).await
    }

    /// Adopt an already authorized watch from atomic observed workflow admission.
    /// The connection must be the SAME owning principal returned by the start path;
    /// no reconnect, second watch admission, or inferred readiness occurs here.
    ///
    /// # Errors
    /// Rejects unsupported/inconsistent readiness or private recording creation
    /// failure, revoking the admitted watch without cancelling the started run.
    pub async fn from_ready(
        mut connection: bcode_client::ClientConnection,
        ready: ExecutionWatchReady,
        directory: &Path,
        limits: CaptureLimits,
    ) -> Result<Self, &'static str> {
        let mut aggregate = EvalXrayAggregation::new(limits);
        aggregate.execution_watch_ready(&ready);
        let directory = recording_directory(directory)?;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        if ready.schema_version != EXECUTION_WATCH_SCHEMA_VERSION
            || ready.scope.root_run_id.is_empty()
            || ready.watch_id.is_empty()
            || limits.sessions == 0
            || limits.segments == 0
            || ready.max_page_items == 0
            || ready.max_page_bytes == 0
            || ready.cursor != 0
            || builder.create(&directory).is_err()
        {
            let _ = until(
                tokio::time::Instant::now() + Duration::from_secs(5),
                connection.revoke_execution_watch(RevokeExecutionWatchRequest {
                    schema_version: EXECUTION_WATCH_SCHEMA_VERSION,
                    watch_id: ready.watch_id,
                }),
            )
            .await;
            return Err("watch_readiness_or_recording_unavailable");
        }
        Ok(Self {
            connection,
            cursor: ready.cursor,
            ready,
            directory,
            recordings: BTreeMap::new(),
            aggregate,
            limits,
            final_inventory_complete: false,
            revoked: false,
        })
    }

    /// Explicitly retain the current atomic admission API's pre-watch gap.
    pub fn mark_admission_gap(&mut self) {
        self.aggregate.gap();
    }

    /// Consume one bounded live page and verified typed attachments. An empty page
    /// is not a terminal barrier. Call during execution, never from producer hooks.
    ///
    /// # Errors
    /// Returns secret-safe failure; no destructive cursor replay or reconnect occurs.
    pub async fn poll(&mut self) -> Result<usize, &'static str> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        self.poll_until(deadline).await
    }

    async fn poll_until(&mut self, deadline: tokio::time::Instant) -> Result<usize, &'static str> {
        let result = until(
            deadline,
            self.connection
                .read_execution_watch(ReadExecutionWatchRequest {
                    schema_version: EXECUTION_WATCH_SCHEMA_VERSION,
                    watch_id: self.ready.watch_id.clone(),
                    cursor: self.cursor,
                    limit: self.ready.max_page_items.min(128),
                }),
        )
        .await;
        let Ok(Ok(page)) = result else {
            self.aggregate.gap();
            return Err("watch_page_unavailable");
        };
        if page.schema_version != EXECUTION_WATCH_SCHEMA_VERSION
            || page.watch_id != self.ready.watch_id
            || page.next_cursor <= self.cursor
            || page.events.len() > self.ready.max_page_items.min(128) as usize
        {
            self.aggregate.gap();
            return Err("watch_page_inconsistent");
        }
        if !envelopes_fit_page(&page.events, self.ready.max_page_bytes) {
            self.aggregate.gap();
            return Err("watch_envelope_bound_exceeded");
        }
        self.cursor = page.next_cursor;
        self.aggregate.execution_watch_page(&page);
        let count = page.events.len();
        for event in page.events {
            if until(deadline, self.record(event)).await.is_err() {
                // The destructive page was consumed, but some local evidence may
                // be absent or its payload export interrupted. Never replay it.
                self.aggregate.gap();
                return Err("watch_page_export_deadline");
            }
        }
        Ok(count)
    }

    async fn record(&mut self, event: ExecutionWatchEvent) {
        let id = event.envelope.observation_id.clone();
        if !self.recordings.contains_key(&id) {
            if event.envelope.sequence != 1
                || event.envelope.event.domain != "xray"
                || event.envelope.event.name != "ready"
            {
                self.aggregate.gap();
                return;
            }
            if self.recordings.len() >= self.limits.segments {
                self.aggregate.gap();
                return;
            }
            let path = self
                .directory
                .join(format!("observation-{:08}", self.recordings.len()));
            let sink = JsonlBundleRecorder::create(
                &path,
                &id,
                &event.session_id,
                RecordingOptions::default(),
            );
            let Ok(sink) = sink else {
                self.aggregate.gap();
                return;
            };
            let mut evidence = EvalXrayEvidence::new(event.session_id.clone());
            evidence.ready_before_submission = event.envelope.sequence == 1
                && event.envelope.event.domain == "xray"
                && event.envelope.event.name == "ready";
            self.recordings.insert(
                id.clone(),
                Recording {
                    run: event.run_id.clone(),
                    session: event.session_id.clone(),
                    evidence,
                    sink,
                    payloads_ok: true,
                    cutoff: None,
                },
            );
        }
        let recording = self.recordings.get_mut(&id).expect("inserted recording");
        if recording.run != event.run_id || recording.session != event.session_id {
            self.aggregate.gap();
            return;
        }
        if recording.sink.append(&event.envelope).is_err() {
            recording.payloads_ok = false;
        }
        recording.evidence.envelopes += 1;
        recording.payloads_ok &= !event.envelope.event.name.ends_with("_omitted");
        if event.envelope.event.domain != "xray" && event.envelope.event.capture.is_none() {
            recording.payloads_ok = false; // opaque domain data is not verified attachment coverage
        }
        let exported = tokio::time::timeout(
            Duration::from_secs(15),
            export_payloads(
                &mut self.connection,
                &self.ready.watch_id,
                &event.run_id,
                &event.envelope,
                &mut recording.sink,
            ),
        )
        .await;
        recording.payloads_ok &= matches!(exported, Ok(Ok(())));
        if event.envelope.event.domain == "xray" && event.envelope.event.name == "finalized" {
            recording.cutoff = serde_json::from_value(event.envelope.event.data).ok();
        }
    }

    /// Establish every engine cutoff, drain matching final envelopes, export their
    /// payloads, close all sinks, then revoke retention on the same principal.
    ///
    /// # Errors
    /// Returns a fixed failure for artifact publication. Transport/capture failures
    /// are retained in the aggregate and never change task score or cancel work.
    pub async fn finish(mut self) -> Result<EvalXrayAggregate, &'static str> {
        // One absolute allowance covers inventory AND export drain. A large
        // inventory must not multiply per-request timeouts during cancellation.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut offset = 0;
        let mut expected_cutoffs = BTreeMap::new();
        for _ in 0..self.limits.segments.max(1) {
            let result = until(
                deadline,
                self.connection
                    .finalize_execution_watch(FinalizeExecutionWatchRequest {
                        schema_version: EXECUTION_WATCH_SCHEMA_VERSION,
                        watch_id: self.ready.watch_id.clone(),
                        offset,
                        limit: self.ready.max_page_items.min(128),
                    }),
            )
            .await;
            let Ok(Ok(evidence)) = result else {
                self.aggregate.gap();
                break;
            };
            if evidence.watch_id != self.ready.watch_id {
                self.aggregate.gap();
                break;
            }
            self.aggregate.execution_watch_final_evidence(&evidence);
            for cutoff in &evidence.cutoffs {
                if let Some(boundary) = &cutoff.finalization {
                    expected_cutoffs.insert(
                        (cutoff.run_id.clone(), cutoff.session_id.clone()),
                        boundary.clone(),
                    );
                } else {
                    self.aggregate.gap();
                }
            }
            if let Some(next) = evidence.next_offset {
                if next <= offset {
                    self.aggregate.gap();
                    break;
                }
                offset = next;
            } else {
                self.final_inventory_complete = true;
                break;
            }
        }
        let mut drained = false;
        while tokio::time::Instant::now() < deadline {
            match self.poll_until(deadline).await {
                Ok(0) => {
                    drained = self.final_inventory_complete
                        && expected_cutoffs.iter().all(|((run, session), cutoff)| {
                            self.recordings
                                .get(&cutoff.observation_id)
                                .is_some_and(|recording| {
                                    &recording.run == run
                                        && &recording.session == session
                                        && recording
                                            .cutoff
                                            .as_ref()
                                            .is_some_and(|delivered| delivered == cutoff)
                                })
                        });
                    break;
                }
                Ok(_) => {}
                _ => {
                    self.aggregate.gap();
                    break;
                }
            }
        }
        for (observation_id, mut recording) in std::mem::take(&mut self.recordings) {
            let manifest = recording
                .cutoff
                .as_ref()
                .filter(|boundary| {
                    recording.payloads_ok
                        && expected_cutoffs.get(&(recording.run.clone(), recording.session.clone()))
                            == Some(*boundary)
                })
                .and_then(|cutoff| recording.sink.finalize(cutoff).ok());
            if manifest.is_none() {
                let _ = recording.sink.interrupt();
            }
            recording
                .evidence
                .settle(recording.cutoff, manifest.is_some());
            recording.evidence.payloads_complete = recording.payloads_ok
                && manifest.is_some_and(|manifest| manifest.evidence_complete);
            recording.evidence.continuity_complete = !self.ready.prior_execution_gap;
            recording.evidence.evidence_complete = recording.evidence.delivery_complete
                && recording.evidence.payloads_complete
                && recording.evidence.continuity_complete;
            self.aggregate.segment_finished(
                &recording.run,
                &recording.session,
                &self.ready.watch_id,
                &observation_id,
                Some(recording.evidence),
            );
        }
        if self.final_inventory_complete && drained {
            self.aggregate.watch_drained(&self.ready.watch_id);
        } else {
            self.aggregate.gap();
        }
        // Revocation gets a small, separate cleanup allowance even when export
        // used its budget. Timeout means unverified release, not success.
        let revoked = until(
            tokio::time::Instant::now() + Duration::from_secs(5),
            self.connection
                .revoke_execution_watch(RevokeExecutionWatchRequest {
                    schema_version: EXECUTION_WATCH_SCHEMA_VERSION,
                    watch_id: self.ready.watch_id.clone(),
                }),
        )
        .await;
        self.revoked = matches!(revoked, Ok(Ok(receipt)) if
            receipt.schema_version == EXECUTION_WATCH_SCHEMA_VERSION
                && receipt.watch_id == self.ready.watch_id
        );
        if !self.revoked {
            self.aggregate.gap();
        }
        let aggregate = self.aggregate.finish();
        let bytes =
            serde_json::to_vec_pretty(&aggregate).map_err(|_| "watch_receipt_encoding_failed")?;
        std::fs::write(self.directory.join("evidence.json"), bytes)
            .map_err(|_| "watch_receipt_write_failed")?;
        Ok(aggregate)
    }
}

fn recording_directory(parent: &Path) -> Result<PathBuf, &'static str> {
    parent
        .canonicalize()
        .map(|parent| parent.join("xray-execution"))
        .map_err(|_| "watch_recording_parent_unavailable")
}

fn envelopes_fit_page(events: &[ExecutionWatchEvent], max_bytes: u64) -> bool {
    events
        .iter()
        .try_fold(0_u64, |total, event| {
            let size = u64::try_from(serde_json::to_vec(&event.envelope).ok()?.len()).ok()?;
            total.checked_add(size).filter(|total| *total <= max_bytes)
        })
        .is_some()
}

async fn export_payloads(
    connection: &mut bcode_client::ClientConnection,
    watch: &str,
    run: &str,
    envelope: &bcode_xray_models::XrayEnvelope,
    sink: &mut JsonlBundleRecorder,
) -> Result<(), ()> {
    use sha2::{Digest as _, Sha256};
    for reference in envelope.payload_references().map_err(|_| ())? {
        if reference.byte_len > 1024 * 1024
            || reference.session_id != envelope.event.correlation.session_id
        {
            return Err(());
        }
        sink.expect_payload(&reference).map_err(|_| ())?;
        let mut bytes = Vec::new();
        loop {
            let offset = u64::try_from(bytes.len()).map_err(|_| ())?;
            let chunk = connection
                .export_execution_watch_payload(ExportExecutionWatchPayloadRequest {
                    schema_version: EXECUTION_WATCH_SCHEMA_VERSION,
                    watch_id: watch.into(),
                    run_id: run.into(),
                    payload: bcode_xray_models::XrayReadPayloadRequest {
                        schema_version: XRAY_SCHEMA_VERSION,
                        observation_id: envelope.observation_id.clone(),
                        session_id: reference.session_id.clone(),
                        payload_id: reference.payload_id.clone(),
                        offset,
                        length: 64 * 1024,
                    },
                })
                .await
                .map_err(|_| ())?;
            if chunk.schema_version != XRAY_SCHEMA_VERSION
                || chunk.observation_id != envelope.observation_id
                || chunk.payload != reference
                || chunk.offset != offset
                || chunk.bytes.len() > 64 * 1024
                || offset.saturating_add(chunk.bytes.len() as u64) > reference.byte_len
                || (chunk.bytes.is_empty() && !chunk.eof)
            {
                return Err(());
            }
            bytes.extend_from_slice(&chunk.bytes);
            if chunk.eof {
                break;
            }
        }
        if bytes.len() as u64 != reference.byte_len
            || hex::encode(Sha256::digest(&bytes)) != reference.sha256
        {
            return Err(());
        }
        sink.store_payload(&reference, &bytes).map_err(|_| ())?;
    }
    Ok(())
}
