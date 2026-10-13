//! Transport-independent bounded semantic session observation.
//!
//! Application owners construct observations after their normal access checks. Consumers receive
//! only this handle: no socket sink, server state, session manager, or raw event log is required.

use bcode_session::{SessionError, SessionEventSubscription, SessionManager};
use bcode_session_models::observation::{
    SESSION_OBSERVATION_SCHEMA_VERSION, SessionObservationEnd, SessionObservationEnvelope,
    SessionObservationError, SessionObservationEvent, SessionObservationRequest,
    SessionObservationSource, SessionObservationStream,
};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ProjectionWindowAnchor, SessionEvent, SessionId,
    SessionLiveEvent, SessionProjectionKind,
};
use bcode_session_view::SessionView;
use bcode_session_view_models::{SessionViewSnapshot, TextStreamViewStatus};
use tokio::sync::broadcast;

/// Public semantic delivery, using the existing renderer-neutral session projection.
pub type SessionObservationDelivery = SessionObservationEnvelope<SessionViewSnapshot>;

/// Pull-based bounded observation, independent of any transport or background forwarding task.
///
/// The first receive transfers the initial window. Durable source order is preserved; live and
/// durable sources have no common clock and are selected fairly. The canonical projector owns
/// stream integrity and absorbing terminal semantics. A turn terminal does not end session
/// observation: subsequent turns may still occur.
///
/// Source broadcast buffers have their existing bounded capacities. A slow consumer receives an
/// explicit terminal gap rather than silently losing updates. No additional unbounded delivery
/// queue exists. Resident projector bookkeeping is additionally bounded by cumulative examined
/// updates and per-source-input bytes; the encoded snapshot cap is not an exact heap-size cap.
/// `LimitReached` explicitly requires a fresh bounded observation to continue. Neither closing
/// nor dropping this handle cancels session execution. Drop releases
/// both source receivers immediately; no attached-client registration or task requires cleanup.
///
/// This is bounded state transfer, not durable resume. The initial window is not an atomic snapshot
/// of all live activity: live-only prefixes predating subscription may be absent. The projector
/// exposes incomplete/degraded stream state; degraded updates end observation with a live gap.
#[derive(Debug)]
pub struct SessionObservation {
    session_id: SessionId,
    view: Option<SessionView>,
    subscription: Option<SessionEventSubscription>,
    initial: Option<SessionViewSnapshot>,
    pending_end: Option<SessionObservationEnd>,
    delivery_sequence: u64,
    source_sequence: Option<u64>,
    initial_source_sequence: Option<u64>,
    updates: usize,
    max_updates: usize,
    max_snapshot_bytes: usize,
    durable_open: bool,
    live_open: bool,
}

/// Construct through the owning application after authorization, never through an adapter.
///
/// Subscriptions precede the bounded canonical read. Durable events already represented in that
/// read are discarded from the buffer; new events must continue its exact source sequence.
///
/// # Errors
/// Returns normalized errors for unsupported requests, missing sessions, unverifiable initial
/// windows, incompatible history, and an initial semantic snapshot exceeding its requested bound.
pub async fn observe_session(
    sessions: &SessionManager,
    request: SessionObservationRequest,
) -> Result<SessionObservation, SessionObservationError> {
    validate_request(&request)?;
    let subscription = sessions
        .subscribe_session_events(request.session_id)
        .await
        .map_err(|error| normalize_error(&error))?;
    if !encoded_fits(&subscription.session, request.max_snapshot_bytes) {
        return Err(SessionObservationError::Unavailable);
    }
    let page = sessions
        .session_projection_history_page(request.session_id, request.window)
        .await
        .map_err(|error| normalize_error(&error))?;
    if page.session_id != request.session_id
        || page.window.has_newer
        || page.events.len() > request.max_updates
    {
        return Err(SessionObservationError::Unavailable);
    }
    let mut previous = None;
    for event in &page.events {
        if !compatible(event, request.session_id)
            || previous.is_some_and(|sequence: u64| sequence.checked_add(1) != Some(event.sequence))
        {
            return Err(SessionObservationError::Unavailable);
        }
        previous = Some(event.sequence);
    }
    if !encoded_fits(&page.events, request.max_snapshot_bytes) {
        return Err(SessionObservationError::Unavailable);
    }
    let mut view = SessionView::new();
    view.set_session_summary(subscription.session.clone());
    view.apply_history(&page.events);
    view.set_history_window_metadata(
        page.window
            .source_range
            .as_ref()
            .map(|range| range.start_sequence),
        page.window
            .source_range
            .as_ref()
            .map(|range| range.end_sequence),
        page.window.has_older,
        page.window.has_newer,
    );
    if !snapshot_fits(view.snapshot(), request.max_snapshot_bytes) {
        return Err(SessionObservationError::Unavailable);
    }
    let initial = view.snapshot().clone();
    Ok(SessionObservation {
        session_id: request.session_id,
        view: Some(view),
        subscription: Some(subscription),
        initial: Some(initial),
        pending_end: None,
        delivery_sequence: 0,
        source_sequence: previous,
        initial_source_sequence: previous,
        updates: page.events.len(),
        max_updates: request.max_updates,
        max_snapshot_bytes: request.max_snapshot_bytes,
        durable_open: true,
        live_open: true,
    })
}

fn validate_request(request: &SessionObservationRequest) -> Result<(), SessionObservationError> {
    if request.schema_version != SESSION_OBSERVATION_SCHEMA_VERSION {
        return Err(SessionObservationError::UnsupportedSchema);
    }
    if request.max_updates == 0
        || request.max_snapshot_bytes == 0
        || u64::try_from(request.max_updates).map_or(true, |updates| updates > u64::MAX - 2)
        || request.window.limits.max_items == 0
        || request.window.limits.max_bytes == 0
        || request.window.limits.max_events_scanned == 0
        || request.window.limits.max_events_scanned > request.max_updates
        || request.window.anchor != ProjectionWindowAnchor::Latest
        || request.window.direction != bcode_session_models::ProjectionWindowDirection::Backward
        || request.window.projection != SessionProjectionKind::Transcript
    {
        return Err(SessionObservationError::InvalidRequest);
    }
    Ok(())
}

const fn normalize_error(error: &SessionError) -> SessionObservationError {
    match error {
        SessionError::NotFound(_) => SessionObservationError::NotFound,
        _ => SessionObservationError::Unavailable,
    }
}

fn compatible(event: &SessionEvent, session_id: SessionId) -> bool {
    event.session_id == session_id && event.schema_version == CURRENT_SESSION_EVENT_SCHEMA_VERSION
}

impl SessionObservation {
    /// Stop observing immediately, releasing subscriptions and resident projection state.
    /// One `Closed` delivery remains available; repeated closes cannot overwrite an earlier end.
    pub fn close(&mut self) {
        if self.subscription.is_some() {
            self.end(SessionObservationEnd::Closed);
        }
    }

    fn end(&mut self, reason: SessionObservationEnd) {
        self.subscription = None;
        self.view = None;
        self.initial = None;
        if self.pending_end.is_none() {
            self.pending_end = Some(reason);
        }
    }

    const fn delivery(
        &mut self,
        event: SessionObservationEvent<SessionViewSnapshot>,
    ) -> SessionObservationDelivery {
        let sequence = self.delivery_sequence;
        self.delivery_sequence += 1;
        SessionObservationEnvelope {
            schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
            session_id: self.session_id,
            sequence,
            source_sequence: self.source_sequence,
            event,
        }
    }

    /// Receive initial state, a subsequent semantic snapshot, or one final observation outcome.
    /// Returns None forever after that outcome. Cancelling the receive future leaves this handle
    /// usable; callers must close/drop it to release subscriptions. No execution is cancelled.
    pub async fn recv(&mut self) -> Option<SessionObservationDelivery> {
        if let Some(snapshot) = self.initial.take() {
            return Some(self.delivery(SessionObservationEvent::Snapshot {
                initial: true,
                snapshot,
            }));
        }
        loop {
            if let Some(reason) = self.pending_end.take() {
                return Some(self.delivery(SessionObservationEvent::Ended { reason }));
            }
            let subscription = self.subscription.as_mut()?;
            let update = tokio::select! {
                event = subscription.events.recv(), if self.durable_open => SourceUpdate::Durable(event.map(Box::new)),
                event = subscription.live_events.recv(), if self.live_open => SourceUpdate::Live(event.map(Box::new)),
                else => {
                    self.end(SessionObservationEnd::SourceClosed);
                    continue;
                }
            };
            if !self.apply(update) {
                continue;
            }
            let snapshot = self.view.as_ref()?.snapshot().clone();
            return Some(self.delivery(SessionObservationEvent::Snapshot {
                initial: false,
                snapshot,
            }));
        }
    }

    fn apply(&mut self, update: SourceUpdate) -> bool {
        let (source, failure) = match &update {
            SourceUpdate::Durable(result) => {
                (SessionObservationSource::Durable, result.as_ref().err())
            }
            SourceUpdate::Live(result) => (SessionObservationSource::Live, result.as_ref().err()),
        };
        if let Some(failure) = failure {
            match failure {
                broadcast::error::RecvError::Closed => match source {
                    SessionObservationSource::Durable => self.durable_open = false,
                    SessionObservationSource::Live => self.live_open = false,
                },
                broadcast::error::RecvError::Lagged(skipped) => {
                    self.end(SessionObservationEnd::Gap {
                        source,
                        skipped: Some(*skipped),
                    });
                }
            }
            return false;
        }
        let fits = match &update {
            SourceUpdate::Durable(Ok(event)) => encoded_fits(event, self.max_snapshot_bytes),
            SourceUpdate::Live(Ok(event)) => encoded_fits(event, self.max_snapshot_bytes),
            _ => true,
        };
        if !fits || self.updates >= self.max_updates {
            self.end(SessionObservationEnd::LimitReached);
            return false;
        }
        self.updates += 1;
        match update {
            SourceUpdate::Durable(Ok(event)) => {
                if !compatible(&event, self.session_id) {
                    self.end(SessionObservationEnd::IncompatibleSource);
                    return false;
                }
                if self
                    .initial_source_sequence
                    .is_some_and(|sequence| event.sequence <= sequence)
                {
                    return false; // Buffered history already represented by the initial read.
                }
                let expected = self
                    .source_sequence
                    .map_or(Some(0), |sequence| sequence.checked_add(1));
                if expected != Some(event.sequence) {
                    self.end(SessionObservationEnd::Gap {
                        source,
                        skipped: None,
                    });
                    return false;
                }
                self.source_sequence = Some(event.sequence);
                if let Some(view) = self.view.as_mut() {
                    view.apply_event(&event);
                }
            }
            SourceUpdate::Live(Ok(event)) => {
                if event.session_id != self.session_id {
                    self.end(SessionObservationEnd::IncompatibleSource);
                    return false;
                }
                if let Some(view) = self.view.as_mut() {
                    view.apply_live_event(&event);
                }
            }
            _ => return false,
        }
        let Some(view) = self.view.as_ref() else {
            return false;
        };
        if view
            .snapshot()
            .text_streams
            .values()
            .any(|stream| stream.status == TextStreamViewStatus::Degraded)
        {
            self.end(SessionObservationEnd::Gap {
                source: SessionObservationSource::Live,
                skipped: None,
            });
            return false;
        }
        if !snapshot_fits(view.snapshot(), self.max_snapshot_bytes) {
            self.end(SessionObservationEnd::LimitReached);
            return false;
        }
        true
    }
}

impl SessionObservationStream for SessionObservation {
    type Snapshot = SessionViewSnapshot;

    async fn recv(&mut self) -> Option<SessionObservationEnvelope<Self::Snapshot>> {
        Self::recv(self).await
    }

    fn close(&mut self) {
        Self::close(self);
    }
}

#[derive(Debug)]
enum SourceUpdate {
    Durable(Result<Box<SessionEvent>, broadcast::error::RecvError>),
    Live(Result<Box<SessionLiveEvent>, broadcast::error::RecvError>),
}

// Count encoded bytes without allocating an unbounded temporary serialization buffer.
fn snapshot_fits(snapshot: &SessionViewSnapshot, limit: usize) -> bool {
    encoded_fits(snapshot, limit)
}

fn encoded_fits(value: &impl serde::Serialize, limit: usize) -> bool {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("snapshot bound exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        ProjectionWindowDirection, ProjectionWindowLimits, ProjectionWindowRequest,
        ProjectionWindowTarget, SessionEventKind,
    };

    fn request(session_id: SessionId) -> SessionObservationRequest {
        SessionObservationRequest {
            schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
            session_id,
            window: ProjectionWindowRequest {
                projection: SessionProjectionKind::Transcript,
                anchor: ProjectionWindowAnchor::Latest,
                direction: ProjectionWindowDirection::Backward,
                target: ProjectionWindowTarget::default(),
                limits: ProjectionWindowLimits {
                    max_items: 32,
                    max_events_scanned: 64,
                    max_bytes: 32 * 1024,
                },
            },
            max_updates: 128,
            max_snapshot_bytes: 128 * 1024,
        }
    }

    fn event(session_id: SessionId, sequence: u64) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: 0,
            session_id,
            provenance: None,
            kind: SessionEventKind::AssistantMessage {
                text: "answer".into(),
            },
        }
    }

    async fn observation() -> (
        SessionObservation,
        broadcast::Sender<SessionEvent>,
        broadcast::Sender<SessionLiveEvent>,
    ) {
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let mut observation = observe_session(&manager, request(summary.id))
            .await
            .unwrap();
        let (durable, events) = broadcast::channel(2);
        let (live, live_events) = broadcast::channel(2);
        observation.subscription = Some(SessionEventSubscription {
            session: summary,
            events,
            live_events,
        });
        (observation, durable, live)
    }

    fn assert_end(delivery: SessionObservationDelivery, expected: SessionObservationEnd) {
        assert_eq!(
            delivery.event,
            SessionObservationEvent::Ended { reason: expected }
        );
        drop(delivery);
    }

    #[tokio::test]
    async fn initial_precedes_buffered_update_and_duplicate_initial_history_is_skipped() {
        let (mut observation, durable, _live) = observation().await;
        let baseline = observation.source_sequence.unwrap();
        durable
            .send(event(observation.session_id, baseline))
            .unwrap();
        durable
            .send(event(observation.session_id, baseline + 1))
            .unwrap();
        let initial = observation.recv().await.unwrap();
        let mut expected = observation.view.as_ref().unwrap().clone();
        expected.apply_event(&event(observation.session_id, baseline + 1));
        assert_eq!(initial.sequence, 0);
        assert!(matches!(
            initial.event,
            SessionObservationEvent::Snapshot { initial: true, .. }
        ));
        let update = observation.recv().await.unwrap();
        assert_eq!(update.sequence, 1);
        assert_eq!(update.source_sequence, Some(baseline + 1));
        let SessionObservationEvent::Snapshot { snapshot, initial } = update.event else {
            panic!("missing snapshot");
        };
        assert!(!initial);
        assert_eq!(snapshot, *expected.snapshot());
    }

    #[tokio::test]
    async fn broadcast_lag_ends_and_releases_both_receivers() {
        let (mut observation, durable, live) = observation().await;
        observation.recv().await.unwrap();
        let baseline = observation.source_sequence.unwrap();
        for offset in 1..=3 {
            durable
                .send(event(observation.session_id, baseline + offset))
                .unwrap();
        }
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::Gap {
                source: SessionObservationSource::Durable,
                skipped: Some(1),
            },
        );
        assert!(observation.recv().await.is_none());
        assert_eq!(durable.receiver_count(), 0);
        assert_eq!(live.receiver_count(), 0);
    }

    #[tokio::test]
    async fn source_sequence_gap_is_terminal_and_close_cannot_replace_it() {
        let (mut observation, durable, _live) = observation().await;
        observation.recv().await.unwrap();
        durable
            .send(event(
                observation.session_id,
                observation.source_sequence.unwrap() + 2,
            ))
            .unwrap();
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::Gap {
                source: SessionObservationSource::Durable,
                skipped: None,
            },
        );
        observation.close();
        assert!(observation.recv().await.is_none());
    }

    #[tokio::test]
    async fn repeated_new_source_sequence_is_not_silently_discarded() {
        let (mut observation, durable, _live) = observation().await;
        observation.recv().await.unwrap();
        let next = observation.source_sequence.unwrap() + 1;
        durable.send(event(observation.session_id, next)).unwrap();
        observation.recv().await.unwrap();
        durable.send(event(observation.session_id, next)).unwrap();
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::Gap {
                source: SessionObservationSource::Durable,
                skipped: None,
            },
        );
    }

    #[tokio::test]
    async fn close_before_initial_is_stable_and_drop_releases_sources() {
        let (mut observation, durable, live) = observation().await;
        observation.close();
        observation.close();
        assert_eq!(durable.receiver_count(), 0);
        assert_eq!(live.receiver_count(), 0);
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::Closed,
        );
        assert!(observation.recv().await.is_none());
        let (observation, durable, live) = self::observation().await;
        drop(observation);
        assert_eq!(durable.receiver_count(), 0);
        assert_eq!(live.receiver_count(), 0);
    }

    #[tokio::test]
    async fn both_sources_must_close_before_source_closed_delivery() {
        let (mut observation, durable, live) = observation().await;
        observation.recv().await.unwrap();
        drop(durable);
        drop(live);
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::SourceClosed,
        );
        assert!(observation.recv().await.is_none());
    }

    #[tokio::test]
    async fn bound_and_future_schema_fail_closed_without_delivering_bad_state() {
        let (mut observation, durable, _live) = observation().await;
        observation.recv().await.unwrap();
        observation.max_updates = observation.updates;
        durable
            .send(event(
                observation.session_id,
                observation.source_sequence.unwrap() + 1,
            ))
            .unwrap();
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::LimitReached,
        );

        let (mut observation, durable, _live) = self::observation().await;
        observation.recv().await.unwrap();
        let mut future = event(
            observation.session_id,
            observation.source_sequence.unwrap() + 1,
        );
        future.schema_version += 1;
        durable.send(future).unwrap();
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::IncompatibleSource,
        );
    }

    #[tokio::test]
    async fn stopping_real_observation_does_not_stop_canonical_mutation() {
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let mut observation = observe_session(&manager, request(summary.id))
            .await
            .unwrap();
        observation.close();
        let committed = manager
            .append_context_compacted(summary.id, "context".into(), 0)
            .await
            .unwrap();
        assert_eq!(committed.session_id, summary.id);
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::Closed,
        );
    }

    fn live_update(
        session_id: SessionId,
        revision: u64,
        operation: bcode_session_models::TextStreamOperation,
    ) -> SessionLiveEvent {
        SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: "turn".into(),
                segment_id: "segment".into(),
                segment_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation: 0,
                    first_revision: revision,
                    revision,
                    operation,
                },
            },
        }
    }

    #[tokio::test]
    async fn live_integrity_gap_ends_without_exposing_degraded_snapshot() {
        use bcode_session_models::TextStreamOperation;
        let (mut observation, _durable, live) = observation().await;
        observation.recv().await.unwrap();
        live.send(live_update(
            observation.session_id,
            1,
            TextStreamOperation::Append {
                expected_offset: 0,
                text: "a".into(),
            },
        ))
        .unwrap();
        observation.recv().await.unwrap();
        live.send(live_update(
            observation.session_id,
            3,
            TextStreamOperation::Append {
                expected_offset: 1,
                text: "gap".into(),
            },
        ))
        .unwrap();
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::Gap {
                source: SessionObservationSource::Live,
                skipped: None,
            },
        );
        assert!(observation.recv().await.is_none());
    }

    #[tokio::test]
    async fn projector_terminal_stream_cannot_reopen_and_does_not_end_observation() {
        use bcode_session_models::{TextStreamOperation, TextStreamTerminalStatus};
        let (mut observation, _durable, live) = observation().await;
        observation.recv().await.unwrap();
        live.send(live_update(
            observation.session_id,
            1,
            TextStreamOperation::Append {
                expected_offset: 0,
                text: "a".into(),
            },
        ))
        .unwrap();
        observation.recv().await.unwrap();
        live.send(live_update(
            observation.session_id,
            2,
            TextStreamOperation::Terminal {
                status: TextStreamTerminalStatus::Cancelled,
            },
        ))
        .unwrap();
        let terminal = observation.recv().await.unwrap();
        live.send(live_update(
            observation.session_id,
            3,
            TextStreamOperation::Append {
                expected_offset: 1,
                text: "late".into(),
            },
        ))
        .unwrap();
        let late = observation.recv().await.unwrap();
        let SessionObservationEvent::Snapshot {
            snapshot: terminal, ..
        } = terminal.event
        else {
            panic!("missing terminal snapshot");
        };
        let SessionObservationEvent::Snapshot { snapshot: late, .. } = late.event else {
            panic!("missing late snapshot");
        };
        assert_eq!(terminal.transcript.items, late.transcript.items);
        assert_eq!(terminal.text_streams, late.text_streams);
        assert!(late.text_streams.values().all(|stream| stream.status
            == TextStreamViewStatus::Terminal(TextStreamTerminalStatus::Cancelled)));
        assert!(observation.subscription.is_some());
    }

    #[tokio::test]
    async fn cancelled_receive_keeps_subscription_usable_and_live_lag_is_explicit() {
        let (mut observation, _durable, live) = observation().await;
        observation.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), observation.recv())
                .await
                .is_err()
        );
        assert_eq!(live.receiver_count(), 1);
        for revision in 1..=3 {
            live.send(live_update(
                observation.session_id,
                revision,
                bcode_session_models::TextStreamOperation::Append {
                    expected_offset: 0,
                    text: "a".into(),
                },
            ))
            .unwrap();
        }
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::Gap {
                source: SessionObservationSource::Live,
                skipped: Some(1),
            },
        );
        assert_eq!(live.receiver_count(), 0);
    }

    #[tokio::test]
    async fn oversized_update_is_not_delivered() {
        let (mut observation, durable, _live) = observation().await;
        observation.recv().await.unwrap();
        observation.max_snapshot_bytes = 1;
        durable
            .send(event(
                observation.session_id,
                observation.source_sequence.unwrap() + 1,
            ))
            .unwrap();
        assert_end(
            observation.recv().await.unwrap(),
            SessionObservationEnd::LimitReached,
        );
    }

    async fn receive_generic<S: SessionObservationStream>(
        stream: &mut S,
    ) -> Option<SessionObservationEnvelope<S::Snapshot>> {
        stream.recv().await
    }

    #[tokio::test]
    async fn domain_stream_contract_consumes_handle_without_private_inputs() {
        let (mut observation, durable, live) = observation().await;
        let delivery = receive_generic(&mut observation).await.unwrap();
        assert!(matches!(
            delivery.event,
            SessionObservationEvent::Snapshot { initial: true, .. }
        ));
        SessionObservationStream::close(&mut observation);
        assert_end(
            receive_generic(&mut observation).await.unwrap(),
            SessionObservationEnd::Closed,
        );
        assert!(receive_generic(&mut observation).await.is_none());
        assert_eq!(durable.receiver_count(), 0);
        assert_eq!(live.receiver_count(), 0);
    }

    #[test]
    fn byte_budget_stops_serialization_before_visiting_unbounded_input() {
        use serde::ser::SerializeSeq as _;
        struct ManyItems(std::cell::Cell<usize>);
        impl serde::Serialize for ManyItems {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut sequence = serializer.serialize_seq(Some(usize::MAX))?;
                for _ in 0..usize::MAX {
                    self.0.set(self.0.get() + 1);
                    sequence.serialize_element("01234567890123456789012345678901")?;
                }
                sequence.end()
            }
        }
        let input = ManyItems(std::cell::Cell::new(0));
        assert!(!encoded_fits(&input, 64));
        assert!(input.0.get() <= 2);
    }

    #[test]
    fn rejects_unknown_contract_and_historical_window_and_measures_snapshot_bytes() {
        let mut request = request(SessionId::new());
        request.schema_version += 1;
        assert_eq!(
            validate_request(&request),
            Err(SessionObservationError::UnsupportedSchema)
        );
        request.schema_version = SESSION_OBSERVATION_SCHEMA_VERSION;
        request.window.anchor = ProjectionWindowAnchor::BeforeSequence(5);
        assert_eq!(
            validate_request(&request),
            Err(SessionObservationError::InvalidRequest)
        );
        let view = SessionView::new();
        let size = serde_json::to_vec(view.snapshot()).unwrap().len();
        assert!(snapshot_fits(view.snapshot(), size));
        assert!(!snapshot_fits(view.snapshot(), size - 1));
    }
}
