//! Fresh, explicitly gapful observation segments through the matching application client.

use super::{HTTP_OPERATION_TIMEOUT, XrayClient, XrayClientError, XrayEndpointLease, XrayStream};
use bcode_session_models::SessionId;
use bcode_xray_models::{XRAY_SCHEMA_VERSION, XrayEnvelope, XrayFinalization, XrayObserveRequest};
use sha2::{Digest as _, Sha256};

/// Local segment identity. Sequence numbers are meaningful only inside its observation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct XraySegmentIdentity {
    /// Monotonic ordinal within this controller, not a durable cursor.
    pub ordinal: u64,
    /// Exact session admitted by the matching application.
    pub session_id: String,
    /// Fresh engine observation identity, never a previous observation's cursor.
    pub observation_id: String,
    /// Every segment after the first has an unknown-size capture gap before readiness.
    pub gap_before: bool,
}

/// Why this segment stopped; none of these outcomes describe execution success.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XraySegmentEnd {
    /// The engine cutoff was consumed.
    Finalized,
    /// Explicit credential renewal/rebootstrap; intervening diagnostics are unavailable.
    Renewed,
    /// Observer stopped without waiting for a cutoff.
    Stopped,
    /// Transport ended or was rejected; no replay is available.
    Interrupted,
}

/// Bounded receipt for one segment; contains neither credentials nor endpoint paths.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct XraySegmentResult {
    /// Receipt representation version; consumers must reject unsupported versions.
    pub schema_version: u16,
    /// Identity required to interpret observation-local sequence coordinates.
    pub identity: XraySegmentIdentity,
    /// Last envelope coordinate actually returned to the caller (including gap ranges).
    pub delivered_through: u64,
    /// Observation-only outcome, not task, sink or canonical-session completion.
    pub end: XraySegmentEnd,
    /// Cutoff only when its final envelope was delivered to the caller.
    pub finalization: Option<XrayFinalization>,
    /// Whether the owning daemon acknowledged endpoint/credential revocation.
    /// False is explicit ambiguity; expiry is not a confirmed revocation.
    pub revocation_confirmed: bool,
    /// Finalized retention-release acknowledgement. None means no finalized release
    /// was attempted, not that payload retention was successfully reclaimed.
    pub retention_release_confirmed: Option<bool>,
    /// Number of delivered domain events without typed capture metadata. Their JSON
    /// remains opaque; a nonzero count prohibits claiming complete attachment export.
    pub opaque_capture_events: u64,
}

struct ActiveSegment {
    identity: XraySegmentIdentity,
    stream: Option<XrayStream>,
    interruption: Option<XrayClientError>,
    drain_deadline: Option<tokio::time::Instant>,
    client: XrayClient,
    lease: XrayEndpointLease,
    delivered_through: u64,
    opaque_capture_events: u64,
}

impl ActiveSegment {
    fn interrupt(&mut self, error: XrayClientError) {
        if self.interruption.is_none() {
            self.interruption = Some(error);
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            self.drain_deadline = Some(deadline);
            self.client.close_capture_for_drain(deadline);
            // Closing SSE stops delivery/capture without revoking retained payload access.
            drop(self.stream.take());
        }
    }
}

struct InvalidateOnDrop(XrayClient);

impl Drop for InvalidateOnDrop {
    fn drop(&mut self) {
        self.0.invalidate();
    }
}

struct SegmentClock {
    attempts: u64,
    credential_fingerprints: std::collections::BTreeSet<[u8; 32]>,
    limit: u64,
}

impl Default for SegmentClock {
    fn default() -> Self {
        Self {
            attempts: 0,
            credential_fingerprints: std::collections::BTreeSet::new(),
            limit: 1024,
        }
    }
}

impl SegmentClock {
    fn begin(&mut self) -> Result<u64, XrayClientError> {
        if self.attempts >= self.limit {
            return Err(XrayClientError::Protocol);
        }
        self.attempts = self
            .attempts
            .checked_add(1)
            .ok_or(XrayClientError::Protocol)?;
        Ok(self.attempts)
    }

    fn credential(&mut self, client: &XrayClient) -> Result<(), XrayClientError> {
        let digest: [u8; 32] = Sha256::digest(client.authorization.as_bytes()).into();
        if !self.credential_fingerprints.insert(digest) {
            return Err(XrayClientError::InvalidCredential);
        }
        // Retain only bounded non-exported fingerprints, never stale usable bearers.
        Ok(())
    }
}

/// Reusable, read-only segmented observer for CLI and eval consumers.
///
/// The supplied client is cloned once and reused unchanged: artifact, state location,
/// configuration context and startup policy remain owned by its normal application
/// boundary. No private socket discovery, ambient-default client reconstruction, replay,
/// automatic retries, execution cancellation or execution submission occurs here.
///
/// Call `rebootstrap` explicitly to renew credentials or recover after transport loss.
/// Each successful admission has a fresh ready barrier and an explicit unknown-size gap.
/// Only the latest ended receipt is retained; consumers must copy receipts they need.
/// Cancellation of a control future can leave revocation unconfirmed. Dropping the
/// controller invalidates shared clients synchronously and schedules bounded best-effort
/// revocation when a Tokio runtime exists; server-owned expiry remains the backstop.
pub struct XrayObservationController {
    application: crate::BcodeClient,
    session_id: SessionId,
    deep_capture: bool,
    origin_grant: Option<bcode_ipc::XrayOriginGrantRequest>,
    active: Option<ActiveSegment>,
    clock: SegmentClock,
    last_result: Option<XraySegmentResult>,
    admission_revocation_confirmed: Option<bool>,
}

impl XrayObservationController {
    /// Construct an idle controller without enabling capture or reading credentials.
    #[must_use]
    pub fn new(
        application: &crate::BcodeClient,
        session_id: SessionId,
        deep_capture: bool,
    ) -> Self {
        Self {
            application: application.clone(),
            session_id,
            deep_capture,
            origin_grant: None,
            active: None,
            clock: SegmentClock::default(),
            last_result: None,
            admission_revocation_confirmed: None,
        }
    }

    /// Set an explicit browser Origin grant for subsequent endpoint admissions only.
    /// Defaults to None (no browser Origins). Authorization, exact Origin validation and
    /// bounded lifetime enforcement remain owned by application admission; this setter
    /// never grants access, starts capture or mutates an already-admitted endpoint.
    pub fn set_origin_grant(&mut self, grant: Option<bcode_ipc::XrayOriginGrantRequest>) {
        self.origin_grant = grant;
    }

    /// Set the explicit in-memory attempt/fingerprint retention allowance before admission.
    /// Default: 1024. Exhaustion fails closed; it never evicts fingerprints and risks stale
    /// reuse. This is an observer policy, not a workflow lifetime or recursion limit.
    ///
    /// # Errors
    /// Rejects changing policy once any admission attempt has begun.
    pub const fn set_segment_limit(
        &mut self,
        limit: std::num::NonZeroU64,
    ) -> Result<(), XrayClientError> {
        if self.clock.attempts != 0 {
            return Err(XrayClientError::Protocol);
        }
        self.clock.limit = limit.get();
        Ok(())
    }

    /// Current segment, including an interrupted segment retained solely for export.
    /// Available only after application authorization and HTTP readiness.
    #[must_use]
    pub fn segment(&self) -> Option<&XraySegmentIdentity> {
        self.active.as_ref().map(|active| &active.identity)
    }

    /// Latest closed segment. This does not imply any execution or recording succeeded.
    #[must_use]
    pub const fn last_result(&self) -> Option<&XraySegmentResult> {
        self.last_result.as_ref()
    }

    /// Cleanup acknowledgement for the latest failed admission after endpoint allocation.
    /// None means no such cleanup was observed, not a confirmed revocation.
    #[must_use]
    pub const fn admission_revocation_confirmed(&self) -> Option<bool> {
        self.admission_revocation_confirmed
    }

    /// Current authenticated client for bounded payload export while retention is live.
    /// After interruption, its shared fence rejects new observations but permits export
    /// for at most 30 seconds. Stop/drop/rebootstrap invalidate all retained clones.
    #[must_use]
    pub fn client(&self) -> Option<&XrayClient> {
        self.active.as_ref().map(|active| &active.client)
    }

    /// Revoke/drop the previous capability, authorize a new endpoint and await readiness.
    /// Failed attempts are never retried with cached credentials. Even failed admissions
    /// consume an ordinal so a later success cannot conceal an intervening capture gap.
    ///
    /// # Errors
    /// Reports bootstrap, credential, authorization or readiness failure. The controller
    /// remains disconnected on error; any admitted endpoint is explicitly revoked.
    /// Calling this method explicitly acknowledges that queued payload exports from the
    /// previous segment have drained or are being discarded. It closes/revokes that
    /// segment before new capture. To drain first, use `record_payloads`, then `stop`.
    pub async fn rebootstrap(&mut self) -> Result<XraySegmentIdentity, XrayClientError> {
        self.end(XraySegmentEnd::Renewed).await;
        self.admission_revocation_confirmed = None;
        let ordinal = self.clock.begin()?;
        let lease = tokio::time::timeout(HTTP_OPERATION_TIMEOUT, async {
            let mut connection = self.application.connect("bcode-xray").await?;
            let info = connection
                .open_xray_endpoint_with_origin(
                    self.session_id,
                    self.deep_capture,
                    self.origin_grant.clone(),
                )
                .await?;
            Ok::<_, crate::ClientError>(XrayEndpointLease { info, connection })
        })
        .await
        .map_err(|_| XrayClientError::Transport)?
        .map_err(|error| admission_error(&error))?;
        let admitted =
            async {
                let client = XrayClient::from_credential_file(
                    &lease.info.endpoint,
                    &lease.info.credential_file,
                )?;
                self.clock.credential(&client)?;
                let stream = client
                    .observe(XrayObserveRequest {
                        schema_version: XRAY_SCHEMA_VERSION,
                        session_id: self.session_id.to_string(),
                        deep_capture: self.deep_capture,
                    })
                    .await?;
                if self.last_result.as_ref().is_some_and(|previous| {
                    previous.identity.observation_id == stream.observation_id()
                }) {
                    return Err(XrayClientError::Protocol);
                }
                Ok::<_, XrayClientError>((client, stream))
            }
            .await;
        match admitted {
            Ok((client, stream)) => {
                let identity = XraySegmentIdentity {
                    ordinal,
                    session_id: self.session_id.to_string(),
                    observation_id: stream.observation_id().to_owned(),
                    gap_before: ordinal > 1,
                };
                self.active = Some(ActiveSegment {
                    identity: identity.clone(),
                    stream: Some(stream),
                    client,
                    lease,
                    delivered_through: 0,
                    interruption: None,
                    drain_deadline: None,
                    opaque_capture_events: 0,
                });
                Ok(identity)
            }
            Err(error) => {
                self.admission_revocation_confirmed = Some(false);
                self.admission_revocation_confirmed = Some(revoke(lease).await);
                Err(error)
            }
        }
    }

    /// Receive from this segment with no live SSE timeout or automatic reconnection.
    /// `None` means its cutoff was delivered. A failed stream is immediately dropped,
    /// but its client and lease remain for queued payload export until `stop` or explicit
    /// `rebootstrap`. Repeated calls return the same error without reading again.
    /// The post-failure export window is 30 seconds and never renewed by reads.
    ///
    /// # Errors
    /// Disconnected, rejected, unsupported or incomplete streams fail explicitly.
    pub async fn recv(&mut self) -> Result<Option<XrayEnvelope>, XrayClientError> {
        let active = self.active.as_mut().ok_or(XrayClientError::Incomplete)?;
        if let Some(error) = &active.interruption {
            return Err(error.clone());
        }
        let stream = active.stream.as_mut().ok_or(XrayClientError::Incomplete)?;
        match stream.recv().await {
            Ok(envelope) => {
                if let Some(envelope) = &envelope {
                    active.delivered_through =
                        super::validate_sequence(envelope, active.delivered_through)?;
                    if envelope.event.domain != "xray" && envelope.event.capture.is_none() {
                        active.opaque_capture_events =
                            active.opaque_capture_events.saturating_add(1);
                    }
                }
                Ok(envelope)
            }
            Err(error) => {
                active.interrupt(error.clone());
                Err(error)
            }
        }
    }

    /// Export declared attachments through the active segment's authenticated transport.
    /// Undeclared legacy data stays opaque; see the receipt's `opaque_capture_events`.
    /// A stream failure retains this transport for a non-renewable 30-second export
    /// window. Export/auth failures close it immediately; no stale credential retries.
    /// The caller still owns recording interruption semantics.
    ///
    /// # Errors
    /// Rejects disconnected controllers, foreign observations, malformed declarations,
    /// failed authorization, missing bytes, digest mismatch and sink failures.
    pub async fn record_payloads(
        &mut self,
        envelope: &XrayEnvelope,
        sink: &mut dyn bcode_xray::recording::RecordingSink,
        range_bytes: u32,
    ) -> Result<(), XrayClientError> {
        let active = self.active.as_ref().ok_or(XrayClientError::Incomplete)?;
        if active
            .drain_deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            let _ = sink.interrupt();
            self.end(XraySegmentEnd::Interrupted).await;
            return Err(XrayClientError::Incomplete);
        }
        if envelope.observation_id != active.identity.observation_id
            || envelope.event.correlation.session_id != active.identity.session_id
        {
            let _ = sink.interrupt();
            return Err(XrayClientError::Protocol);
        }
        let result = if let Some(deadline) = active.drain_deadline {
            tokio::time::timeout_at(
                deadline,
                active.client.record_payloads(envelope, sink, range_bytes),
            )
            .await
            .unwrap_or_else(|_| {
                let _ = sink.interrupt();
                Err(XrayClientError::Incomplete)
            })
        } else {
            active
                .client
                .record_payloads(envelope, sink, range_bytes)
                .await
        };
        if result.is_err() {
            self.end(XraySegmentEnd::Interrupted).await;
        }
        result
    }

    /// Request a cutoff with the control-operation deadline; never cancels session work.
    /// Drain `recv` and export referenced payloads before `stop`.
    ///
    /// # Errors
    /// Disconnected or unconfirmed cutoff requests fail without implying execution failure.
    pub async fn finalize(&mut self) -> Result<(), XrayClientError> {
        let active = self.active.as_ref().ok_or(XrayClientError::Incomplete)?;
        if let Some(error) = &active.interruption {
            return Err(error.clone());
        }
        let stream = active.stream.as_ref().ok_or(XrayClientError::Incomplete)?;
        let result = active.client.finalize(stream.observation_id()).await;
        if let Err(error) = &result
            && let Some(active) = &mut self.active
        {
            active.interrupt(error.clone());
        }
        result
    }

    /// Stop intake after a local queue/consumer failure while preserving the bounded
    /// payload drain window. Join queued exports before `stop` revokes the endpoint.
    /// This never retries capture or changes the observed execution.
    pub fn interrupt(&mut self) {
        if let Some(active) = &mut self.active {
            active.interrupt(XrayClientError::Incomplete);
        }
    }

    /// Drop this observer, release retained payloads if finalized, and revoke its endpoint.
    /// No session cancellation or steering is performed. Check `revocation_confirmed`.
    pub async fn stop(&mut self) {
        self.end(XraySegmentEnd::Stopped).await;
    }

    async fn end(&mut self, end: XraySegmentEnd) {
        let Some(active) = self.active.take() else {
            return;
        };
        let ActiveSegment {
            identity,
            stream,
            client,
            lease,
            delivered_through,
            opaque_capture_events,
            interruption,
            drain_deadline: _,
        } = active;
        let finalization = stream.as_ref().and_then(XrayStream::finalization).cloned();
        let invalidate_on_drop = InvalidateOnDrop(client.clone());
        let observation_id = identity.observation_id.clone();
        drop(stream);
        // Install the receipt before await: cancellation must not conceal interruption.
        self.last_result = Some(XraySegmentResult {
            schema_version: 1,
            identity,
            delivered_through,
            opaque_capture_events,
            end: if interruption.is_some() {
                XraySegmentEnd::Interrupted
            } else if finalization.is_some() {
                XraySegmentEnd::Finalized
            } else {
                end
            },
            finalization,
            revocation_confirmed: false,
            retention_release_confirmed: None,
        });
        if self
            .last_result
            .as_ref()
            .is_some_and(|result| result.finalization.is_some())
        {
            let confirmed = client.release(&observation_id).await.is_ok();
            if let Some(result) = &mut self.last_result {
                result.retention_release_confirmed = Some(confirmed);
            }
        }
        drop(invalidate_on_drop);
        drop(client);
        let confirmed = revoke(lease).await;
        if let Some(result) = &mut self.last_result {
            result.revocation_confirmed = confirmed;
        }
    }
}

impl Drop for XrayObservationController {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            active.client.invalidate();
            // Drop closes live SSE synchronously; revocation is bounded best effort only.
            drop(active.stream);
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let _cleanup = runtime.spawn(async move {
                    let _ = revoke(active.lease).await;
                });
            }
        }
    }
}

const fn admission_error(error: &crate::ClientError) -> XrayClientError {
    match error {
        crate::ClientError::Transport(_)
        | crate::ClientError::ConnectTimeout { .. }
        | crate::ClientError::DaemonStartupTimeout { .. }
        | crate::ClientError::RequestTimeout { .. } => XrayClientError::Transport,
        crate::ClientError::Server { .. } => XrayClientError::AdmissionRejected,
        _ => XrayClientError::Protocol,
    }
}

async fn revoke(lease: XrayEndpointLease) -> bool {
    tokio::time::timeout(HTTP_OPERATION_TIMEOUT, lease.close())
        .await
        .is_ok_and(|result| result.is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_attempts_preserve_gaps_and_reject_stale_credentials() {
        let mut clock = SegmentClock::default();
        let old = XrayClient::new("http://127.0.0.1:1234", "old-private-token").unwrap();
        assert_eq!(clock.begin().unwrap(), 1);
        clock.credential(&old).unwrap();
        assert_eq!(clock.begin().unwrap(), 2);
        assert!(matches!(
            clock.credential(&old),
            Err(XrayClientError::InvalidCredential)
        ));
        assert_eq!(clock.begin().unwrap(), 3);
        let fresh = XrayClient::new("http://127.0.0.1:1234", "renewed-private-token").unwrap();
        clock.credential(&fresh).unwrap();
        assert!(matches!(
            clock.credential(&old),
            Err(XrayClientError::InvalidCredential)
        ));
        assert!(clock.attempts > 1);
    }

    #[test]
    fn rejected_admission_is_not_reclassified_as_recoverable_transport() {
        let rejection = admission_error(&crate::ClientError::Server {
            code: "permission_denied".into(),
            message: "untrusted private server text".into(),
        });
        assert!(matches!(rejection, XrayClientError::AdmissionRejected));
        assert!(!rejection.permits_rebootstrap_retry());
        assert!(!rejection.to_string().contains("untrusted private"));
        for error in [
            XrayClientError::InvalidCredential,
            XrayClientError::Protocol,
            XrayClientError::Http(401),
            XrayClientError::Http(403),
        ] {
            assert!(!error.permits_rebootstrap_retry());
        }
        assert!(XrayClientError::Incomplete.permits_rebootstrap_retry());
    }

    #[tokio::test(start_paused = true)]
    async fn interrupted_capability_allows_only_bounded_export_not_new_capture() {
        let client = XrayClient::new("http://127.0.0.1:1234", "private-token").unwrap();
        let stale = client.clone();
        client.close_capture_for_drain(
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        assert!(stale.ensure_active().is_ok());
        assert!(matches!(
            stale
                .observe(XrayObserveRequest {
                    schema_version: 1,
                    session_id: "session".into(),
                    deep_capture: false,
                })
                .await,
            Err(XrayClientError::InvalidCredential)
        ));
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(matches!(
            stale.ensure_active(),
            Err(XrayClientError::InvalidCredential)
        ));
    }

    #[test]
    fn ordinal_exhaustion_fails_closed() {
        let mut clock = SegmentClock {
            attempts: u64::MAX,
            limit: u64::MAX,
            ..SegmentClock::default()
        };
        assert!(clock.begin().is_err());
        assert_eq!(clock.attempts, u64::MAX);
    }
}
