#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Session-scoped, opt-in, transient diagnostics.
//!
//! The caller authorizes attach and sensitive reads. This engine never opens session
//! storage, steers execution, or retains data after the last owning observation drops.
//! Producer operations never wait for queue capacity or acquire a blocking lock.

pub mod recording;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub use bcode_xray_models::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Notify;
use uuid::Uuid;

/// Hard per-observation retention bounds. Invalid configuration fails at attach.
#[derive(Clone, Debug)]
pub struct XrayConfig {
    /// Maximum simultaneous observations; capacity is released on drop.
    pub max_observations: usize,
    /// Buffered ordinary events per observation; ready/gap/final records cannot be crowded out.
    pub queue_capacity: usize,
    /// Maximum serialized producer event bytes; at least 1024.
    pub max_event_bytes: usize,
    /// Maximum bytes in one all-or-nothing payload capture.
    pub max_payload_bytes: usize,
    /// Maximum payload bytes owned by an observation; at least `max_payload_bytes`.
    pub max_retained_payload_bytes: usize,
    /// Maximum retained payload count, including zero-length payloads.
    pub max_retained_payloads: usize,
    /// Maximum bytes returned in one explicitly authorized range read.
    pub max_payload_range_bytes: usize,
}

impl Default for XrayConfig {
    fn default() -> Self {
        Self {
            max_observations: 32,
            queue_capacity: 256,
            max_event_bytes: 64 * 1024,
            max_payload_bytes: 1024 * 1024,
            max_retained_payload_bytes: 8 * 1024 * 1024,
            max_retained_payloads: 64,
            max_payload_range_bytes: 64 * 1024,
        }
    }
}

/// Secret-free, typed diagnostic operation failures; arbitrary payloads are never included.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum XrayError {
    /// Independent protocol version was not recognized.
    #[error("unsupported X-ray schema version")]
    UnsupportedVersion,
    /// Configured limits cannot support a bounded observation.
    #[error("invalid X-ray configuration")]
    InvalidConfiguration,
    /// An origin identity is empty, oversized or contains control characters.
    #[error("invalid session or correlation identity")]
    InvalidIdentity,
    /// The configured simultaneous observation limit was reached.
    #[error("X-ray observation capacity reached")]
    Capacity,
    /// Internal state is poisoned and cannot safely be used.
    #[error("X-ray engine unavailable")]
    Unavailable,
    /// A nonblocking registry operation encountered contention.
    #[error("X-ray producer busy; no capture retained")]
    Busy,
    /// The exact transient observation no longer exists.
    #[error("X-ray observation is no longer available")]
    ObservationUnavailable,
    /// The observation does not own the requested sensitive session scope.
    #[error("sensitive capture is not active for this scope")]
    CaptureInactive,
    /// Input bytes exceed the configured all-or-nothing payload bound.
    #[error("X-ray payload exceeds capture limit")]
    PayloadTooLarge,
    /// An explicitly negotiated event sink does not provide byte retention.
    #[error("X-ray external sink does not support payload capture")]
    PayloadCaptureUnsupported,
    /// The diagnostic media type is empty, oversized or contains control characters.
    #[error("invalid X-ray payload content type")]
    InvalidContentType,
    /// The exact payload was not captured or has been evicted.
    #[error("X-ray payload is unavailable or evicted")]
    PayloadUnavailable,
    /// The byte range is empty, oversized, overflowing or beyond retained EOF.
    #[error("invalid X-ray payload range")]
    InvalidRange,
    /// No terminal delivery coordinate remains in the observation sequence space.
    #[error("X-ray sequence exhausted")]
    SequenceExhausted,
}

#[derive(Clone)]
struct StoredPayload {
    reference: XrayPayloadRef,
    bytes: Arc<[u8]>,
}

struct Observer {
    session_id: String,
    run_id: Option<String>,
    deep_capture: bool,
    deadline: Option<Instant>,
    queue: VecDeque<XrayEnvelope>,
    next_sequence: u64,
    dropped: u64,
    contention_start: u64,
    finalization: Option<XrayFinalization>,
    notify: Arc<Notify>,
    payloads: BTreeMap<String, StoredPayload>,
    payload_order: VecDeque<String>,
    payload_bytes: usize,
    evicted_payloads: u64,
}

impl Observer {
    fn accepts_capture(&self) -> bool {
        self.finalization.is_none()
            && self
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline)
    }

    fn matches(&self, correlation: &XrayCorrelation) -> bool {
        self.session_id == correlation.session_id
            && self
                .run_id
                .as_ref()
                .is_none_or(|run| correlation.run_id.as_ref() == Some(run))
    }

    /// Evict oldest payloads before retaining one already validated bounded capture.
    fn retain_payload(&mut self, stored: &StoredPayload, config: &XrayConfig) {
        while self.payloads.len() >= config.max_retained_payloads
            || stored.bytes.len()
                > config
                    .max_retained_payload_bytes
                    .saturating_sub(self.payload_bytes)
        {
            let Some(oldest) = self.payload_order.pop_front() else {
                break;
            };
            if let Some(payload) = self.payloads.remove(&oldest) {
                self.payload_bytes -= payload.bytes.len();
                self.evicted_payloads = self.evicted_payloads.saturating_add(1);
            }
        }
        self.payload_bytes += stored.bytes.len();
        self.payload_order
            .push_back(stored.reference.payload_id.clone());
        self.payloads
            .insert(stored.reference.payload_id.clone(), stored.clone());
    }
}

#[derive(Default)]
struct State {
    observers: BTreeMap<String, Observer>,
}

#[derive(Clone)]
struct ActivityHint {
    correlation: XrayCorrelation,
    deep_capture: bool,
    deadline: Option<Instant>,
}

#[derive(Clone, Copy)]
enum LossCause {
    EmitState,
    PayloadPreflight,
    PayloadCommit,
    ActivityHint,
    External,
}

#[derive(Default)]
struct LossCounters {
    emit_state: AtomicU64,
    payload_preflight: AtomicU64,
    payload_commit: AtomicU64,
    activity_hint: AtomicU64,
    external: AtomicU64,
}

impl LossCounters {
    const fn counter(&self, cause: LossCause) -> &AtomicU64 {
        match cause {
            LossCause::EmitState => &self.emit_state,
            LossCause::PayloadPreflight => &self.payload_preflight,
            LossCause::PayloadCommit => &self.payload_commit,
            LossCause::ActivityHint => &self.activity_hint,
            LossCause::External => &self.external,
        }
    }
}

struct Inner {
    config: XrayConfig,
    state: Mutex<State>,
    // Lifecycle-only writes; delivery/retention never acquire this lock.
    activity: RwLock<BTreeMap<String, ActivityHint>>,
    contention: AtomicU64,
    loss_causes: LossCounters,
}

/// A cloneable application-owned hub; there is intentionally no process-global registry.
#[derive(Clone)]
pub struct XrayHub {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for XrayHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Numeric, hub-lifetime diagnostic snapshots only. Independent relaxed
        // loads are not a coherent cutoff receipt or a source of authorization.
        let causes = &self.inner.loss_causes;
        formatter
            .debug_struct("XrayHub")
            .field("loss_total", &self.inner.contention.load(Ordering::Relaxed))
            .field(
                "emit_state_busy",
                &causes.emit_state.load(Ordering::Relaxed),
            )
            .field(
                "payload_preflight_busy",
                &causes.payload_preflight.load(Ordering::Relaxed),
            )
            .field(
                "payload_commit_busy",
                &causes.payload_commit.load(Ordering::Relaxed),
            )
            .field(
                "activity_hint_unavailable",
                &causes.activity_hint.load(Ordering::Relaxed),
            )
            .field(
                "external_reported_loss",
                &causes.external.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl Default for XrayHub {
    fn default() -> Self {
        Self::new(XrayConfig::default())
    }
}

impl XrayHub {
    /// Construct an application-owned hub with explicit retention bounds.
    #[must_use]
    pub fn new(config: XrayConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                state: Mutex::new(State::default()),
                activity: RwLock::new(BTreeMap::new()),
                contention: AtomicU64::new(0),
                loss_causes: LossCounters::default(),
            }),
        }
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, XrayError> {
        self.inner.state.lock().map_err(|_| XrayError::Unavailable)
    }

    fn try_state(&self) -> Result<MutexGuard<'_, State>, XrayError> {
        self.inner.state.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => XrayError::Busy,
            std::sync::TryLockError::Poisoned(_) => XrayError::Unavailable,
        })
    }

    fn producer_state(&self, cause: LossCause) -> Result<MutexGuard<'_, State>, XrayError> {
        self.try_state().inspect_err(|error| {
            if matches!(error, XrayError::Busy) {
                self.record_loss(cause, 1);
            }
        })
    }

    /// Attach after application authorization, including explicit sensitive-scope approval.
    ///
    /// The first received envelope is `xray.ready`, including when events arrive immediately.
    ///
    /// # Errors
    /// Rejects unsupported versions, invalid identities/configuration, capacity and poisoned state.
    pub fn observe(&self, request: XrayObserveRequest) -> Result<XrayObservation, XrayError> {
        self.observe_with_deadline(request, None)
    }

    /// Attach with an optional hard capture cutoff, independent of scheduler wakeups.
    ///
    /// At or after the deadline no new events or payloads are retained. Existing
    /// evidence remains drainable until normal finalization/drop. None is unbounded.
    /// This does not schedule shutdown, cancel execution or grant authorization.
    ///
    /// # Errors
    /// Returns the same admission errors as [`Self::observe`].
    pub fn observe_with_deadline(
        &self,
        request: XrayObserveRequest,
        deadline: Option<Instant>,
    ) -> Result<XrayObservation, XrayError> {
        self.observe_filtered(request, None, deadline)
    }

    /// Observe only an exact run correlation within the authorized session.
    ///
    /// This filters instrumentation metadata; it does not discover descendants or
    /// grant workflow authority. Missing or different run metadata never matches.
    ///
    /// # Errors
    /// Returns attach errors or invalid identity for an empty/invalid run identity.
    pub fn observe_run(
        &self,
        request: XrayObserveRequest,
        run_id: String,
    ) -> Result<XrayObservation, XrayError> {
        self.observe_run_with_deadline(request, run_id, None)
    }

    /// Attach an exact-run observation with an optional hard capture deadline.
    ///
    /// Cutoff and drain semantics match [`Self::observe_with_deadline`].
    ///
    /// # Errors
    /// Returns the same admission errors as [`Self::observe_run`].
    pub fn observe_run_with_deadline(
        &self,
        request: XrayObserveRequest,
        run_id: String,
        deadline: Option<Instant>,
    ) -> Result<XrayObservation, XrayError> {
        if !valid_identity(&run_id) {
            return Err(XrayError::InvalidIdentity);
        }
        self.observe_filtered(request, Some(run_id), deadline)
    }

    fn observe_filtered(
        &self,
        request: XrayObserveRequest,
        run_id: Option<String>,
        deadline: Option<Instant>,
    ) -> Result<XrayObservation, XrayError> {
        check_version(request.schema_version)?;
        if !valid_identity(&request.session_id) {
            return Err(XrayError::InvalidIdentity);
        }
        let config = &self.inner.config;
        if config.max_observations == 0
            || config.queue_capacity == 0
            || config.max_event_bytes < 1024
            || config.max_payload_bytes == 0
            || config.max_retained_payload_bytes < config.max_payload_bytes
            || config.max_retained_payloads == 0
            || config.max_payload_range_bytes == 0
        {
            return Err(XrayError::InvalidConfiguration);
        }
        let mut state = self.state()?;
        if state.observers.len() >= config.max_observations {
            return Err(XrayError::Capacity);
        }
        let id = Uuid::new_v4().to_string();
        let notify = Arc::new(Notify::new());
        state.observers.insert(
            id.clone(),
            Observer {
                session_id: request.session_id.clone(),
                run_id: run_id.clone(),
                deep_capture: request.deep_capture,
                deadline,
                queue: VecDeque::new(),
                next_sequence: 2,
                dropped: 0,
                contention_start: self.inner.contention.load(Ordering::Relaxed),
                finalization: None,
                notify: Arc::clone(&notify),
                payloads: BTreeMap::new(),
                payload_order: VecDeque::new(),
                payload_bytes: 0,
                evicted_payloads: 0,
            },
        );
        self.refresh_activity(&state);
        drop(state);
        let capabilities = self.capabilities(request.deep_capture);
        Ok(XrayObservation {
            hub: self.clone(),
            id,
            session_id: request.session_id,
            run_id,
            notify,
            ready: Some(capabilities),
            expected_sequence: 2,
            terminal: false,
        })
    }

    fn capabilities(&self, deep_capture: bool) -> XrayCapabilities {
        let config = &self.inner.config;
        XrayCapabilities {
            schema_version: XRAY_SCHEMA_VERSION,
            deep_capture,
            queue_capacity: config.queue_capacity,
            max_event_bytes: config.max_event_bytes,
            max_payload_bytes: config.max_payload_bytes,
            max_retained_payload_bytes: config.max_retained_payload_bytes,
            max_retained_payloads: config.max_retained_payloads,
            max_payload_range_bytes: config.max_payload_range_bytes,
            durable_replay: false,
        }
    }

    /// Whether any observation is retained, including finalized streams still draining.
    ///
    /// Unlike producer hints this fails explicitly on contended state, so lifecycle
    /// coordination never mistakes an unavailable observation for an idle daemon.
    ///
    /// # Errors
    /// Returns busy or unavailable when the observation registry cannot be inspected.
    pub fn observation_active(&self) -> Result<bool, XrayError> {
        let state = self.try_state()?;
        Ok(!state.observers.is_empty())
    }

    /// Whether an exact session retains any observation or pending payload drain.
    ///
    /// # Errors
    /// Returns invalid identity, busy or unavailable registry state.
    pub fn session_observation_active(&self, session_id: &str) -> Result<bool, XrayError> {
        if !valid_identity(session_id) {
            return Err(XrayError::InvalidIdentity);
        }
        let state = self.try_state()?;
        Ok(state
            .observers
            .values()
            .any(|observer| observer.session_id == session_id))
    }

    /// A best-effort hint, not authorization; producers must still tolerate detach races.
    ///
    /// Returns false on lock contention and records conservative hub-wide uncertainty
    /// in finalization, since the caller may omit an event before reaching `emit`.
    #[must_use]
    pub fn capture_active(&self, session_id: &str) -> bool {
        self.active(session_id, false)
    }

    /// Gate expensive sensitive serialization; false on unavailable/contended hint state.
    ///
    /// Delivery and payload IO do not contend with these lifecycle-only hints.
    /// Lifecycle contention is counted as uncertainty even without a later `emit`.
    #[must_use]
    pub fn deep_capture_active(&self, session_id: &str) -> bool {
        self.active(session_id, true)
    }

    fn refresh_activity(&self, state: &State) {
        // A derived hint, rebuilt from authoritative state under the lifecycle lock.
        let mut activity = self
            .inner
            .activity
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        activity.clear();
        for (id, observer) in &state.observers {
            if observer.finalization.is_none() {
                activity.insert(
                    id.clone(),
                    ActivityHint {
                        correlation: XrayCorrelation {
                            session_id: observer.session_id.clone(),
                            run_id: observer.run_id.clone(),
                            ..XrayCorrelation::default()
                        },
                        deep_capture: observer.deep_capture,
                        deadline: observer.deadline,
                    },
                );
            }
        }
    }

    /// Report callbacks lost before engine sequence allocation.
    ///
    /// Counts are conservative hub-wide uncertainty in `hub_contention_drops`, not
    /// exact selected-session gaps. Report only newly lost callbacks, before cutoff;
    /// repeated reports add counts and finalized receipts are never rewritten.
    pub fn report_external_loss(&self, count: u64) {
        self.record_loss(LossCause::External, count);
    }

    fn record_loss(&self, cause: LossCause, count: u64) {
        // Preserve the authoritative aggregate update and cutoff rules. Cause
        // counters are independent attribution, not a replacement receipt.
        let _ = self
            .inner
            .contention
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(count))
            });
        let _ = self.inner.loss_causes.counter(cause).try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |value| Some(value.saturating_add(count)),
        );
    }

    fn active(&self, session_id: &str, deep: bool) -> bool {
        self.correlation_active(
            &XrayCorrelation {
                session_id: session_id.to_owned(),
                ..XrayCorrelation::default()
            },
            deep,
        )
    }

    /// Exact-correlated activity hint, including optional run filtering.
    /// Session-wide observers accept any run; scoped observers require the exact run.
    #[must_use]
    pub fn correlation_active(&self, correlation: &XrayCorrelation, deep: bool) -> bool {
        // Do not contend with delivery, payload reads, or queue draining just to
        // decide whether expensive diagnostic serialization is worth doing.
        self.inner.activity.try_read().map_or_else(
            |_| {
                self.record_loss(LossCause::ActivityHint, 1);
                false
            },
            |activity| {
                activity.values().any(|hint| {
                    hint.correlation.session_id == correlation.session_id
                        && hint
                            .correlation
                            .run_id
                            .as_ref()
                            .is_none_or(|run| correlation.run_id.as_ref() == Some(run))
                        && (!deep || hint.deep_capture)
                        && hint
                            .deadline
                            .is_none_or(|deadline| Instant::now() < deadline)
                })
            },
        )
    }

    /// Bind explicit origin metadata to this hub without enabling capture.
    #[must_use]
    pub fn context(&self, correlation: XrayCorrelation) -> CaptureContext {
        CaptureContext {
            target: CaptureTarget::Hub(self.clone()),
            correlation,
        }
    }

    /// Bounded, best-effort delivery. Never blocks on a slow observer or engine lock.
    ///
    /// Oversized/invalid event content becomes an explicit omission event. Sensitive
    /// events are delivered only to active deep observers, independently per observer.
    /// The `xray` domain is reserved for engine control records; producers cannot forge
    /// readiness, gaps or terminal delivery. This API is trusted instrumentation, not
    /// an authorization boundary for accepting arbitrary client-supplied events.
    pub fn emit(&self, mut event: XrayEvent) {
        // Validation is bounded but can serialize a full event. Keep it outside the
        // delivery lock so unrelated producers and receivers can make progress.
        if !self.correlation_active(&event.correlation, event.sensitive) {
            return;
        }
        let mut value_budget = self.inner.config.max_event_bytes;
        let reason = if !valid_event_identity(&event) {
            Some("invalid_event_identity")
        } else if !bounded_value(&event.data, 64, &mut value_budget)
            || serde_json::to_writer(LimitedWriter(self.inner.config.max_event_bytes), &event)
                .is_err()
        {
            Some("event_too_large")
        } else if event.payload_references().is_err() {
            Some("invalid_capture_metadata")
        } else {
            None
        };
        if let Some(reason) = reason {
            event = XrayEvent {
                domain: "xray".into(),
                name: "event_omitted".into(),
                correlation: XrayCorrelation {
                    session_id: event.correlation.session_id,
                    run_id: event.correlation.run_id.filter(|run| valid_identity(run)),
                    ..XrayCorrelation::default()
                },
                data: json!({"reason": reason}),
                sensitive: event.sensitive,
                capture: None,
            };
        }
        let Ok(mut state) = self.producer_state(LossCause::EmitState) else {
            return;
        };
        let mut notifications = Vec::with_capacity(state.observers.len());
        for (id, observer) in &mut state.observers {
            if !observer.matches(&event.correlation)
                || !observer.accepts_capture()
                || (event.sensitive && !observer.deep_capture)
            {
                continue;
            }
            let sequence = observer.next_sequence;
            let Some(next) = sequence.checked_add(1) else {
                continue;
            };
            observer.next_sequence = next;
            if observer.queue.len() >= self.inner.config.queue_capacity {
                observer.dropped = observer.dropped.saturating_add(1);
            } else {
                observer.queue.push_back(XrayEnvelope {
                    schema_version: XRAY_SCHEMA_VERSION,
                    observation_id: id.clone(),
                    sequence,
                    timestamp_unix_ms: timestamp(),
                    event: event.clone(),
                });
            }
            notifications.push(Arc::clone(&observer.notify));
        }
        drop(state);
        for notify in notifications {
            notify.notify_one();
        }
    }

    /// Retain sensitive bytes only in currently active deep observations for this session.
    ///
    /// Capture is all-or-nothing, never truncated. Producers must remove credentials before
    /// calling; this does not claim automatic redaction. Returns None when unobserved.
    ///
    /// # Errors
    /// Returns invalid identity/content type, oversized input, busy or unavailable state.
    pub fn capture_payload(
        &self,
        correlation: &XrayCorrelation,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<Option<XrayPayloadRef>, XrayError> {
        if !valid_correlation(correlation) {
            return Err(XrayError::InvalidIdentity);
        }
        if content_type.is_empty()
            || content_type.len() > 256
            || content_type.chars().any(char::is_control)
        {
            return Err(XrayError::InvalidContentType);
        }
        if bytes.len() > self.inner.config.max_payload_bytes {
            return Err(XrayError::PayloadTooLarge);
        }
        if !self.correlation_active(correlation, true) {
            return Ok(None);
        }
        // Verify authoritative scope/deadline before preparing sensitive retention,
        // not only after hashing. Hints alone never authorize capture.
        {
            let state = self.producer_state(LossCause::PayloadPreflight)?;
            if !state.observers.values().any(|observer| {
                observer.matches(correlation) && observer.deep_capture && observer.accepts_capture()
            }) {
                return Ok(None);
            }
        }
        // Hash and copy bounded bytes without holding the delivery/retention lock.
        // Authorization and deadline are rechecked under that lock before retention.
        let reference = XrayPayloadRef {
            schema_version: XRAY_SCHEMA_VERSION,
            payload_id: Uuid::new_v4().to_string(),
            session_id: correlation.session_id.clone(),
            content_type: content_type.to_owned(),
            byte_len: u64::try_from(bytes.len()).map_err(|_| XrayError::PayloadTooLarge)?,
            sha256: hex::encode(Sha256::digest(bytes)),
        };
        let stored = StoredPayload {
            reference: reference.clone(),
            bytes: Arc::from(bytes),
        };
        let mut state = self.producer_state(LossCause::PayloadCommit)?;
        let mut retained = false;
        for observer in state.observers.values_mut() {
            if !observer.matches(correlation)
                || !observer.deep_capture
                || !observer.accepts_capture()
            {
                continue;
            }
            observer.retain_payload(&stored, &self.inner.config);
            retained = true;
        }
        drop(state);
        Ok(retained.then_some(reference))
    }

    /// Read an authorized retained byte range without filesystem or canonical-history access.
    ///
    /// Finalized observations retain their payloads until the observation is dropped.
    ///
    /// # Errors
    /// Rejects unsupported versions, unavailable/foreign/shallow observations, missing or
    /// evicted payloads, zero/oversized lengths, overflow and offsets beyond EOF.
    pub fn read_payload(
        &self,
        request: &XrayReadPayloadRequest,
    ) -> Result<XrayPayloadRange, XrayError> {
        check_version(request.schema_version)?;
        let length = usize::try_from(request.length).map_err(|_| XrayError::InvalidRange)?;
        if length == 0
            || length > self.inner.config.max_payload_range_bytes
            || request
                .offset
                .checked_add(u64::from(request.length))
                .is_none()
        {
            return Err(XrayError::InvalidRange);
        }
        let state = self.state()?;
        let observer = state
            .observers
            .get(&request.observation_id)
            .ok_or(XrayError::ObservationUnavailable)?;
        if observer.session_id != request.session_id || !observer.deep_capture {
            return Err(XrayError::CaptureInactive);
        }
        let payload = observer
            .payloads
            .get(&request.payload_id)
            .ok_or(XrayError::PayloadUnavailable)?
            .clone();
        // Pin authorized bytes, then release retention state before copying a range.
        drop(state);
        let start = usize::try_from(request.offset).map_err(|_| XrayError::InvalidRange)?;
        if start > payload.bytes.len() {
            return Err(XrayError::InvalidRange);
        }
        let end = start.saturating_add(length).min(payload.bytes.len());
        let range = XrayPayloadRange {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: request.observation_id.clone(),
            payload: payload.reference.clone(),
            offset: request.offset,
            bytes: payload.bytes[start..end].to_vec(),
            eof: end == payload.bytes.len(),
        };
        Ok(range)
    }
}

/// A single live observation.
///
/// Dropping it only removes diagnostic retention, never execution.
/// The receiver is single-consumer and intentionally has no durable resume cursor.
pub struct XrayObservation {
    hub: XrayHub,
    id: String,
    session_id: String,
    run_id: Option<String>,
    notify: Arc<Notify>,
    ready: Option<XrayCapabilities>,
    expected_sequence: u64,
    terminal: bool,
}

impl std::fmt::Debug for XrayObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never include session/run identity, payloads, event contents or grants.
        formatter
            .debug_struct("XrayObservation")
            .field("hub_loss_attribution", &self.hub)
            .finish_non_exhaustive()
    }
}

impl XrayObservation {
    /// Stable identity of this transient observation.
    #[must_use]
    pub const fn id(&self) -> &str {
        self.id.as_str()
    }

    /// Establish an idempotent delivery cutoff and disable capture for this observer.
    ///
    /// Wake the receiver. Continue receiving through `xray.finalized` before closing a
    /// transport or finishing a recording. This never waits for execution or persistence.
    ///
    /// # Errors
    /// Returns unavailable state/observation or exhausted sequence space.
    pub fn finalize(&mut self) -> Result<XrayFinalization, XrayError> {
        let mut state = self.hub.state()?;
        let observer = state
            .observers
            .get_mut(&self.id)
            .ok_or(XrayError::ObservationUnavailable)?;
        if let Some(finalization) = &observer.finalization {
            return Ok(finalization.clone());
        }
        if observer.next_sequence == u64::MAX {
            return Err(XrayError::SequenceExhausted);
        }
        let finalization = XrayFinalization {
            observation_id: self.id.clone(),
            final_sequence: observer.next_sequence,
            dropped_events: observer.dropped,
            hub_contention_drops: self
                .hub
                .inner
                .contention
                .load(Ordering::Relaxed)
                .saturating_sub(observer.contention_start),
            evicted_payloads: observer.evicted_payloads,
        };
        observer.finalization = Some(finalization.clone());
        let notify = Arc::clone(&observer.notify);
        self.hub.refresh_activity(&state);
        drop(state);
        notify.notify_one();
        Ok(finalization)
    }

    /// Receive ready, ordered events/gaps, then finalization and EOF.
    ///
    /// Cancellation of this future does not pop a pending event, close the observation,
    /// or affect execution.
    pub async fn recv(&mut self) -> Option<XrayEnvelope> {
        if self.terminal {
            return None;
        }
        if let Some(capabilities) = self.ready.take() {
            return Some(self.control(1, "ready", json!(capabilities)));
        }
        loop {
            let notify = Arc::clone(&self.notify);
            let notified = notify.notified();
            {
                let Ok(mut state) = self.hub.state() else {
                    self.terminal = true;
                    return None;
                };
                let Some(observer) = state.observers.get_mut(&self.id) else {
                    self.terminal = true;
                    return None;
                };
                let next_available = observer
                    .queue
                    .front()
                    .map_or(observer.next_sequence, |event| event.sequence);
                if self.expected_sequence < next_available {
                    let first_sequence = self.expected_sequence;
                    self.expected_sequence = next_available;
                    drop(state);
                    let gap = XrayGap {
                        first_sequence,
                        last_sequence: next_available - 1,
                        reason: "observer_queue_full".into(),
                    };
                    return Some(self.control(first_sequence, "gap", json!(gap)));
                }
                if let Some(event) = observer.queue.pop_front() {
                    self.expected_sequence = event.sequence + 1;
                    drop(state);
                    return Some(event);
                }
                if let Some(finalization) = observer.finalization.clone() {
                    self.terminal = true;
                    drop(state);
                    return Some(self.control(
                        finalization.final_sequence,
                        "finalized",
                        json!(finalization),
                    ));
                }
            }
            notified.await;
        }
    }

    fn control(&self, sequence: u64, name: &str, data: serde_json::Value) -> XrayEnvelope {
        XrayEnvelope {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: self.id.clone(),
            sequence,
            timestamp_unix_ms: timestamp(),
            event: XrayEvent {
                domain: "xray".into(),
                name: name.to_owned(),
                data,
                sensitive: false,
                capture: None,
                correlation: XrayCorrelation {
                    session_id: self.session_id.clone(),
                    run_id: self.run_id.clone(),
                    ..XrayCorrelation::default()
                },
            },
        }
    }
}

impl Drop for XrayObservation {
    fn drop(&mut self) {
        if let Ok(mut state) = self.hub.state() {
            state.observers.remove(&self.id);
            self.hub.refresh_activity(&state);
        }
    }
}

tokio::task_local! {
    static CAPTURE_CONTEXT: CaptureContext;
}

/// Explicitly negotiated event callback for contexts hosted outside this engine instance.
///
/// This Rust trait is not a dynamic-library ABI. Plugin bridges must negotiate their
/// own typed compatibility boundary and adapt it locally. Callbacks must be bounded,
/// nonblocking, panic-free and secret-safe. The host must bind authorization to its
/// originating context and validate/replace incoming correlation; a plugin-supplied
/// session ID is never authority. Neither activity hint confers authorization.
pub trait XrayCaptureSink: Send + Sync {
    /// Whether the host-owned observation scope currently accepts scalar events.
    fn capture_active(&self) -> bool;
    /// Whether explicit sensitive capture is currently authorized and observed.
    fn deep_capture_active(&self) -> bool;
    /// Accept a bounded diagnostic event without waiting for delivery or persistence.
    ///
    /// The implementation rechecks activity, sensitive authorization and origin scope.
    fn emit(&self, event: XrayEvent);

    /// Retain sanitized sensitive bytes under the host-owned originating scope.
    ///
    /// Implementations must recheck authorization and activity and enforce bounded,
    /// all-or-nothing retention without blocking. Returned references must describe
    /// real host retention; incoming diagnostic metadata is never authority.
    ///
    /// # Errors
    /// Returns unsupported by default; adapters may return host capture errors.
    fn capture_payload(
        &self,
        _media_type: &str,
        _bytes: &[u8],
    ) -> Result<Option<XrayPayloadRef>, XrayError> {
        Err(XrayError::PayloadCaptureUnsupported)
    }

    /// Account for newly lost external callbacks before engine cutoff.
    ///
    /// Return true only when loss was retained by the host. Legacy event-only
    /// adapters return false; callers must not claim complete evidence in that case.
    fn report_external_loss(&self, _count: u64) -> bool {
        false
    }
}

type ExternalPayloadCapture = dyn Fn(&XrayCorrelation, &str, &[u8]) -> Result<Option<XrayPayloadRef>, XrayError>
    + Send
    + Sync;

#[derive(Clone)]
enum CaptureTarget {
    Hub(XrayHub),
    Sink(
        Arc<dyn XrayCaptureSink>,
        Option<Arc<ExternalPayloadCapture>>,
    ),
}

std::thread_local! {
    static SYNC_CAPTURE_CONTEXT: RefCell<Option<CaptureContext>> = const { RefCell::new(None) };
}

struct SyncCaptureRestore(Option<CaptureContext>);

impl Drop for SyncCaptureRestore {
    fn drop(&mut self) {
        SYNC_CAPTURE_CONTEXT.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

fn with_sync_capture<R>(capture: Option<CaptureContext>, operation: impl FnOnce() -> R) -> R {
    let previous = SYNC_CAPTURE_CONTEXT.with(|slot| slot.replace(capture));
    let _restore = SyncCaptureRestore(previous);
    operation()
}

/// Explicit origin context.
///
/// Pass clones into spawned tasks: task/thread locals are
/// not inherited. An external sink supplies plumbing, not session authority.
#[derive(Clone)]
pub struct CaptureContext {
    target: CaptureTarget,
    correlation: XrayCorrelation,
}

impl CaptureContext {
    /// Construct a local adapter for an explicitly negotiated external event sink.
    ///
    /// The sink must enforce its host-owned origin and sensitive authorization even
    /// after `with_correlation`; this constructor does not grant access to any session.
    #[must_use]
    pub const fn from_sink(correlation: XrayCorrelation, sink: Arc<dyn XrayCaptureSink>) -> Self {
        Self {
            target: CaptureTarget::Sink(sink, None),
            correlation,
        }
    }

    /// Construct a payload-capable external adapter without changing event-only sinks.
    ///
    /// The callback is host-owned and must recheck current deep authorization, bind
    /// origin correlation, enforce negotiated all-or-nothing byte bounds, and return
    /// only real retained references. It must be bounded, nonblocking, panic-free and
    /// secret-safe. The caller supplies sanitized bytes, never capture credentials.
    /// Neither this constructor nor activity hints grant capture authorization.
    #[must_use]
    pub fn with_external_sink(
        correlation: XrayCorrelation,
        sink: Arc<dyn XrayCaptureSink>,
        capture_payload: impl Fn(
            &XrayCorrelation,
            &str,
            &[u8],
        ) -> Result<Option<XrayPayloadRef>, XrayError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            target: CaptureTarget::Sink(sink, Some(Arc::new(capture_payload))),
            correlation,
        }
    }

    /// Report newly lost callbacks without emitting an event or allocating sequence.
    ///
    /// True means the host accepted accounting, not that the callbacks were captured.
    /// Report before finalization; late reports never rewrite a terminal receipt.
    #[must_use]
    pub fn report_external_loss(&self, count: u64) -> bool {
        match &self.target {
            CaptureTarget::Hub(hub) => {
                hub.report_external_loss(count);
                true
            }
            CaptureTarget::Sink(sink, _) => sink.report_external_loss(count),
        }
    }

    /// Read the innermost synchronous invocation scope, otherwise the async task's origin.
    ///
    /// Async `scope` masks parent thread-local state per poll, so
    /// unrelated executor-thread context cannot override that task's explicit origin.
    // Repository policy omits #[must_use] on Option/Result-returning APIs.
    #[allow(clippy::must_use_candidate)]
    pub fn current() -> Option<Self> {
        SYNC_CAPTURE_CONTEXT
            .with(|slot| slot.borrow().clone())
            .or_else(|| CAPTURE_CONTEXT.try_with(Clone::clone).ok())
    }

    /// Run synchronous service entry with panic-safe, nested thread-local restoration.
    ///
    /// This overrides any ambient async context, including with an inactive sink that
    /// explicitly masks capture. This does not scope future polling: capture the context
    /// in the closure and use `scope` around asynchronous work. No thread-local guard
    /// may be carried across an await point; `scope` installs its own per-poll mask.
    pub fn scope_sync<R>(&self, operation: impl FnOnce() -> R) -> R {
        with_sync_capture(Some(self.clone()), operation)
    }

    /// Origin metadata, not an authorization token.
    #[must_use]
    pub const fn correlation(&self) -> &XrayCorrelation {
        &self.correlation
    }

    /// Derive instrumentation metadata without changing the sink's host-owned authority.
    #[must_use]
    pub fn with_correlation(&self, correlation: XrayCorrelation) -> Self {
        Self {
            target: self.target.clone(),
            correlation,
        }
    }

    /// Best-effort hint for gating expensive sensitive serialization.
    #[must_use]
    pub fn deep_capture_active(&self) -> bool {
        match &self.target {
            CaptureTarget::Hub(hub) => hub.correlation_active(&self.correlation, true),
            CaptureTarget::Sink(sink, _) => sink.deep_capture_active(),
        }
    }

    /// Best-effort hint; each delivery still rechecks its host-owned scope.
    #[must_use]
    pub fn capture_active(&self) -> bool {
        match &self.target {
            CaptureTarget::Hub(hub) => hub.correlation_active(&self.correlation, false),
            CaptureTarget::Sink(sink, _) => sink.capture_active(),
        }
    }

    /// Emit a domain observation.
    ///
    /// External events are bounded to 64 KiB and 64 JSON
    /// levels before crossing the adapter; the host must impose its own negotiated
    /// bounds as well. Invalid/oversized data produces an explicit omission instead.
    pub fn emit(&self, domain: &str, name: &str, data: serde_json::Value, sensitive: bool) {
        self.emit_with_capture(domain, name, data, sensitive, None);
    }

    /// Emit domain data with explicit typed capture locations.
    ///
    /// Reference locations are relative to `data`; arbitrary JSON is never scanned.
    /// The same bounded delivery, sensitive gates and host-owned origin rules as
    /// [`Self::emit`] apply. External adapters must preserve the complete event.
    pub fn emit_with_capture(
        &self,
        domain: &str,
        name: &str,
        data: serde_json::Value,
        sensitive: bool,
        capture: Option<XrayCaptureAttachments>,
    ) {
        self.emit_event(XrayEvent {
            domain: domain.to_owned(),
            name: name.to_owned(),
            data,
            sensitive,
            correlation: self.correlation.clone(),
            capture,
        });
    }

    /// Emit a typed domain event, preserving explicit capture attachments.
    ///
    /// The context replaces caller-supplied correlation with its originating metadata.
    /// External adapters must still enforce host-owned authority and negotiated bounds.
    pub fn emit_event(&self, mut event: XrayEvent) {
        event.correlation = self.correlation.clone();
        match &self.target {
            CaptureTarget::Hub(hub) => hub.emit(event),
            CaptureTarget::Sink(sink, _) => {
                if !sink.capture_active() || (event.sensitive && !sink.deep_capture_active()) {
                    return;
                }
                let mut budget = 64 * 1024;
                if !valid_event_identity(&event)
                    || !bounded_value(&event.data, 64, &mut budget)
                    || serde_json::to_writer(LimitedWriter(64 * 1024), &event).is_err()
                {
                    // Never forward invalid origin metadata, including in omissions.
                    if !valid_correlation(&event.correlation) {
                        return;
                    }
                    event.domain = "capture".into();
                    event.name = "event_omitted".into();
                    event.data = json!({"reason": "external_sink_event_limit_or_identity"});
                    event.capture = None;
                }
                sink.emit(event);
            }
        }
    }

    /// Capture explicit sanitized diagnostic bytes under this originating session.
    ///
    /// External event-only sinks do not retain bytes: an active deep sink receives
    /// `capture.payload_omitted`, and the caller receives an explicit unsupported error.
    ///
    /// # Errors
    /// Hub errors are described in [`XrayHub::capture_payload`]. Payload-capable external
    /// sinks propagate host callback errors after validating identity and content type.
    /// Event-only sinks return [`XrayError::PayloadCaptureUnsupported`] when deep capture
    /// is active, otherwise None.
    pub fn capture_payload(
        &self,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<Option<XrayPayloadRef>, XrayError> {
        match &self.target {
            CaptureTarget::Hub(hub) => hub.capture_payload(&self.correlation, content_type, bytes),
            CaptureTarget::Sink(sink, capture_payload) => {
                if !self.capture_active() || !self.deep_capture_active() {
                    return Ok(None);
                }
                if !valid_correlation(&self.correlation) {
                    return Err(XrayError::InvalidIdentity);
                }
                if content_type.is_empty()
                    || content_type.len() > 256
                    || content_type.chars().any(char::is_control)
                {
                    return Err(XrayError::InvalidContentType);
                }
                let result = capture_payload.as_ref().map_or_else(
                    || sink.capture_payload(content_type, bytes),
                    |capture_payload| capture_payload(&self.correlation, content_type, bytes),
                );
                if result != Err(XrayError::PayloadCaptureUnsupported) {
                    return result;
                }
                self.emit(
                    "capture",
                    "payload_omitted",
                    json!({
                        "reason": "external_sink_payload_unsupported"
                    }),
                    true,
                );
                Err(XrayError::PayloadCaptureUnsupported)
            }
        }
    }

    /// Scope only this future; spawned tasks must receive and scope an explicit clone.
    ///
    /// Each poll masks parent synchronous state, then restores it before returning
    /// Ready or Pending (also on unwind). A synchronous scope entered inside that poll
    /// can override this task's context for its closure. No thread-local guard is
    /// retained in the future across yields, thread migration or cancellation.
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let polling =
            std::future::poll_fn(|cx| with_sync_capture(None, || future.as_mut().poll(cx)));
        CAPTURE_CONTEXT.scope(self.clone(), polling).await
    }
}

const fn check_version(version: u16) -> Result<(), XrayError> {
    if version == XRAY_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(XrayError::UnsupportedVersion)
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn valid_correlation(correlation: &XrayCorrelation) -> bool {
    valid_identity(&correlation.session_id)
        && [
            &correlation.turn_id,
            &correlation.request_id,
            &correlation.tool_call_id,
            &correlation.run_id,
            &correlation.activation_id,
            &correlation.attempt_id,
        ]
        .into_iter()
        .all(|identity| identity.as_deref().is_none_or(valid_identity))
}

fn valid_event_identity(event: &XrayEvent) -> bool {
    event.domain != "xray"
        && valid_identity(&event.domain)
        && valid_identity(&event.name)
        && valid_correlation(&event.correlation)
}

/// Bound recursion and node count before serde traverses untrusted diagnostic JSON.
fn bounded_value(value: &serde_json::Value, depth: usize, budget: &mut usize) -> bool {
    if depth == 0 || *budget == 0 {
        return false;
    }
    *budget -= 1;
    match value {
        serde_json::Value::Array(values) => {
            values.len() <= *budget
                && values
                    .iter()
                    .all(|value| bounded_value(value, depth - 1, budget))
        }
        serde_json::Value::Object(values) => {
            values.len() <= *budget
                && values
                    .values()
                    .all(|value| bounded_value(value, depth - 1, budget))
        }
        _ => true,
    }
}

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// Serialize at most the configured number of bytes, without allocating a second payload.
struct LimitedWriter(usize);

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(io::Error::other("event limit"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
