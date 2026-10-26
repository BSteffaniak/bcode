//! Transport-independent bounded semantic session observation.
//!
//! Application owners construct observations after their normal access checks. Consumers receive
//! only this handle: no socket sink, server state, session manager, or raw event log is required.

#[cfg(test)]
use bcode_session::SessionEventSubscription;
use bcode_session::{SessionError, SessionManager};
use bcode_session_models::observation::{
    SESSION_OBSERVATION_SCHEMA_VERSION, SessionObservationEnd, SessionObservationEnvelope,
    SessionObservationError, SessionObservationEvent, SessionObservationRequest,
    SessionObservationSource, SessionObservationStream,
};
use bcode_session_models::retention::{
    ObservationRetentionExceeded, ObservationRetentionLimits, ObservationRetentionUsage,
};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ProjectionWindowAnchor, SessionEvent, SessionId,
    SessionLiveEvent, SessionProjectionKind,
};
use bcode_session_view::SessionView;
use bcode_session_view_models::{SessionViewSnapshot, TextStreamViewStatus};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use tokio::sync::broadcast;

// Source queues have a separate leased ledger. This account covers projector,
// initial snapshot, retry cache and every outstanding host delivery together.
#[derive(Debug)]
struct RetentionAccount {
    usage: Mutex<ObservationRetentionUsage>,
    limits: ObservationRetentionLimits,
}

impl RetentionAccount {
    #[must_use]
    fn new(limits: ObservationRetentionLimits) -> Arc<Self> {
        Arc::new(Self {
            usage: Mutex::new(ObservationRetentionUsage::default()),
            limits,
        })
    }

    fn reserve(
        self: &Arc<Self>,
        charge: ObservationRetentionUsage,
    ) -> Result<ObservationReplyReservation, ObservationRetentionExceeded> {
        self.usage
            .lock()
            .map_err(|_| ObservationRetentionExceeded)?
            .try_add(charge, self.limits)?;
        Ok(ObservationReplyReservation {
            account: Arc::clone(self),
            charge,
        })
    }
}

/// Nested host retention; keep alongside a response through final send/drop.
#[derive(Debug)]
#[must_use]
pub struct ObservationReplyReservation {
    account: Arc<RetentionAccount>,
    charge: ObservationRetentionUsage,
}

impl ObservationReplyReservation {
    fn join(mut self, mut other: Self) -> Result<Self, ObservationRetentionExceeded> {
        if !Arc::ptr_eq(&self.account, &other.account) {
            return Err(ObservationRetentionExceeded);
        }
        self.charge.try_add(other.charge, self.account.limits)?;
        other.charge = ObservationRetentionUsage::default();
        Ok(self)
    }
}

impl Drop for ObservationReplyReservation {
    fn drop(&mut self) {
        if let Ok(mut usage) = self.account.usage.lock() {
            // Accounting damage preserves the charge and fails closed.
            let _ = usage.try_release(self.charge);
        }
    }
}

#[derive(Debug)]
struct RetainedDelivery {
    // Payload must drop before its reservation.
    delivery: SessionObservationDelivery,
    reservation: ObservationReplyReservation,
}

/// Internal reply and nested reservation, independent of transport framing.
pub struct ChargedObservationReply {
    result: bcode_session_models::SessionObservationPullResult<SessionViewSnapshot>,
    reservation: ObservationReplyReservation,
}

impl ChargedObservationReply {
    /// Transfer both owners; the adapter retains the guard through send/drop.
    pub fn into_parts(
        self,
    ) -> (
        bcode_session_models::SessionObservationPullResult<SessionViewSnapshot>,
        ObservationReplyReservation,
    ) {
        (self.result, self.reservation)
    }
}

#[derive(Debug)]
enum ObservationSubscription {
    Bounded(bcode_session::BoundedSessionEventSubscription),
    #[cfg(test)]
    Broadcast(SessionEventSubscription),
}

impl ObservationSubscription {
    fn session(
        &self,
    ) -> Result<&bcode_session_models::SessionSummary, bcode_session::BoundedSessionEventError>
    {
        match self {
            Self::Bounded(subscription) => subscription.session(),
            #[cfg(test)]
            Self::Broadcast(subscription) => Ok(&subscription.session),
        }
    }
}

// Independent structural policy used for decode, projection and owned deliveries.
// It is not derived from serialized length or an allocator-byte estimate.
const INITIAL_MAX_DECODED_JSON_BYTES: usize = 8 * 1024 * 1024;
const INITIAL_RETENTION_LIMITS: bcode_session_models::retention::ObservationRetentionLimits =
    bcode_session_models::retention::ObservationRetentionLimits {
        scalar_capacity_bytes: 8 * 1024 * 1024,
        collection_entries: 65_536,
        value_nodes: 131_072,
        depth: 64,
        copies: 512,
    };

const TERMINAL_USAGE: ObservationRetentionUsage = ObservationRetentionUsage {
    scalar_capacity_bytes: 0,
    collection_entries: 0,
    value_nodes: 2,
    depth: 3,
    copies: 1,
};

#[must_use]
const fn source_limits() -> bcode_session_models::SessionObservationSourceLimits {
    bcode_session_models::SessionObservationSourceLimits {
        max_scalar_capacity_bytes: INITIAL_RETENTION_LIMITS.scalar_capacity_bytes,
        max_collection_entries: INITIAL_RETENTION_LIMITS.collection_entries,
        max_value_nodes: INITIAL_RETENTION_LIMITS.value_nodes,
        max_depth: INITIAL_RETENTION_LIMITS.depth,
        max_queued_events: INITIAL_RETENTION_LIMITS.copies - 1,
        max_in_flight_events: 1,
    }
}

fn clone_view_snapshot(
    view: &SessionView,
    account: &Arc<RetentionAccount>,
) -> Result<(SessionViewSnapshot, ObservationReplyReservation), ObservationRetentionExceeded> {
    let mut ledger = account
        .usage
        .lock()
        .map_err(|_| ObservationRetentionExceeded)?;
    let before = *ledger;
    let mut charge =
        bcode_session_view_models::retention::inspect_snapshot(view.snapshot(), account.limits)?;
    charge.depth = charge
        .depth
        .checked_add(3)
        .ok_or(ObservationRetentionExceeded)?;
    charge.try_add(
        ObservationRetentionUsage {
            value_nodes: 2,
            copies: 1,
            ..ObservationRetentionUsage::default()
        },
        account.limits,
    )?;
    // Admit the complete envelope peak before the owning API performs any clone.
    let mut peak = before;
    peak.try_add(charge, account.limits)?;
    let mut candidate = before;
    let snapshot = view.try_clone_snapshot_bounded(&mut candidate, account.limits)?;
    *ledger = peak;
    drop(ledger);
    Ok((
        snapshot,
        ObservationReplyReservation {
            account: Arc::clone(account),
            charge,
        },
    ))
}

fn delivery_usage(
    delivery: &SessionObservationDelivery,
    limits: ObservationRetentionLimits,
) -> Result<ObservationRetentionUsage, ObservationRetentionExceeded> {
    let mut usage = match &delivery.event {
        SessionObservationEvent::Snapshot { snapshot, .. } => {
            bcode_session_view_models::retention::inspect_snapshot(snapshot, limits)?
        }
        SessionObservationEvent::Ended { .. } => ObservationRetentionUsage::default(),
    };
    usage.depth = usage
        .depth
        .checked_add(3)
        .ok_or(ObservationRetentionExceeded)?;
    usage.try_add(
        ObservationRetentionUsage {
            value_nodes: 2,
            copies: 1,
            ..ObservationRetentionUsage::default()
        },
        limits,
    )?;
    Ok(usage)
}

fn clone_delivery(
    delivery: &SessionObservationDelivery,
    account: &Arc<RetentionAccount>,
) -> Result<RetainedDelivery, ObservationRetentionExceeded> {
    let mut ledger = account
        .usage
        .lock()
        .map_err(|_| ObservationRetentionExceeded)?;
    let before = *ledger;
    let reserved = delivery_usage(delivery, account.limits)?;
    let mut peak = before;
    peak.try_add(reserved, account.limits)?;
    let cloned = match &delivery.event {
        SessionObservationEvent::Snapshot { initial, snapshot } => {
            let mut candidate = before;
            let snapshot = bcode_session_view_models::retention::clone_snapshot_bounded(
                snapshot,
                &mut candidate,
                account.limits,
            )?;
            SessionObservationEnvelope {
                schema_version: delivery.schema_version,
                session_id: delivery.session_id,
                sequence: delivery.sequence,
                source_sequence: delivery.source_sequence,
                event: SessionObservationEvent::Snapshot {
                    initial: *initial,
                    snapshot,
                },
            }
        }
        SessionObservationEvent::Ended { .. } => delivery.clone(),
    };
    let actual = delivery_usage(&cloned, account.limits)?;
    // The owning clone API reconciles capacity; commit its complete envelope
    // reservation only after all rejection-atomic checks succeed.
    let mut next = before;
    next.try_add(actual, account.limits)?;
    *ledger = next;
    drop(ledger);
    let reservation = ObservationReplyReservation {
        account: Arc::clone(account),
        charge: actual,
    };
    Ok(RetainedDelivery {
        delivery: cloned,
        reservation,
    })
}

const fn initial_decode_budget() -> bcode_session::persisted::EventDecodeBudget {
    bcode_session::persisted::EventDecodeBudget::new(
        INITIAL_MAX_DECODED_JSON_BYTES,
        INITIAL_RETENTION_LIMITS,
    )
}

/// Public semantic delivery, using the existing renderer-neutral session projection.
pub type SessionObservationDelivery = SessionObservationEnvelope<SessionViewSnapshot>;

/// Pull-based bounded observation, independent of any transport or background forwarding task.
///
/// The first receive transfers the initial window. Durable source order is preserved; live and
/// durable sources have no common clock and are selected fairly. The canonical projector owns
/// stream integrity and absorbing terminal semantics. A turn terminal does not end session
/// observation: subsequent turns may still occur.
///
/// Source queues retain independent structural leases through projector admission.
/// A slow consumer receives an explicit terminal outcome rather than silently losing updates.
/// No additional unbounded delivery queue exists. Projector bookkeeping and host-owned snapshot,
/// retry and reply copies share structural accounting; serialized limits remain independent.
/// `LimitReached` explicitly requires a fresh bounded observation to continue. Neither closing
/// nor dropping this handle cancels session execution. Drop releases the source subscription
/// immediately; no attached-client registration or task requires cleanup.
///
/// This is bounded state transfer, not durable resume. The initial window is not an atomic snapshot
/// of all live activity: live-only prefixes predating subscription may be absent. The projector
/// exposes incomplete/degraded stream state; degraded updates end observation with a live gap.
#[derive(Debug)]
pub struct SessionObservation {
    session_id: SessionId,
    view: Option<SessionView>,
    subscription: Option<ObservationSubscription>,
    initial: Option<SessionViewSnapshot>,
    initial_charge: Option<ObservationReplyReservation>,
    view_charge: Option<ObservationReplyReservation>,
    terminal_charge: Option<ObservationReplyReservation>,
    retention: Arc<RetentionAccount>,
    pending_end: Option<SessionObservationEnd>,
    delivery_sequence: u64,
    source_sequence: Option<u64>,
    initial_source_sequence: Option<u64>,
    updates: usize,
    max_updates: usize,
    max_snapshot_bytes: usize,
    #[cfg(test)]
    durable_open: bool,
    #[cfg(test)]
    live_open: bool,
}

/// Construct through the owning application after authorization, never through an adapter.
///
/// Subscriptions precede the bounded canonical read. Durable events already represented in that
/// read are discarded from the buffer; new events must continue its exact source sequence.
///
/// # Errors
/// Returns normalized errors for unsupported requests, missing sessions, unverifiable initial
/// windows and incompatible history. Valid size bounds that cannot fit initial
/// content produce a terminal `LimitReached` observation without retaining it.
pub async fn observe_session(
    sessions: &SessionManager,
    request: SessionObservationRequest,
) -> Result<SessionObservation, SessionObservationError> {
    validate_request(&request)?;
    let subscription = sessions
        .subscribe_session_events_bounded(request.session_id, source_limits())
        .await
        .map(ObservationSubscription::Bounded)
        .map_err(|error| normalize_error(&error))?;
    if !initial_summary_fits(&subscription, request.max_snapshot_bytes)? {
        return Ok(initial_limit_observation(
            request.session_id,
            request.max_updates,
            request.max_snapshot_bytes,
        ));
    }
    let page = match sessions
        .session_projection_history_page_with_decode_budget(
            request.session_id,
            request.window,
            initial_decode_budget(),
        )
        .await
    {
        Ok(page) => page,
        Err(bcode_session::InitialObservationPageError::ResourceLimit) => {
            return Ok(initial_limit_observation(
                request.session_id,
                request.max_updates,
                request.max_snapshot_bytes,
            ));
        }
        Err(bcode_session::InitialObservationPageError::Canonical(error)) => {
            return Err(normalize_error(&error));
        }
    };
    initialize_observation(
        request.session_id,
        request.max_updates,
        request.max_snapshot_bytes,
        subscription,
        &page,
    )
}

fn initial_summary_fits(
    subscription: &ObservationSubscription,
    max_bytes: usize,
) -> Result<bool, SessionObservationError> {
    match subscription.session() {
        Ok(summary) => Ok(encoded_fits(summary, max_bytes)),
        Err(bcode_session::BoundedSessionEventError::LimitReached) => Ok(false),
        Err(_) => Err(SessionObservationError::Unavailable),
    }
}

fn admit_initial_projection(
    view: &mut SessionView,
    view_charge: &mut ObservationReplyReservation,
    retention: &Arc<RetentionAccount>,
    subscription: &ObservationSubscription,
    page: &bcode_session_models::ProjectionHistoryPage,
) -> Result<Result<(), SessionObservationEnd>, SessionObservationError> {
    // Initial summary/queued source ownership overlaps projector construction.
    let source_usage = match subscription {
        ObservationSubscription::Bounded(subscription) => subscription.retained_usage(),
        #[cfg(test)]
        ObservationSubscription::Broadcast(_) => ObservationRetentionUsage::default(),
    };
    let Ok(source_charge) = retention.reserve(source_usage) else {
        return Ok(Err(SessionObservationEnd::LimitReached));
    };
    let mut ledger = retention
        .usage
        .lock()
        .map_err(|_| SessionObservationError::Unavailable)?;
    let summary = subscription
        .session()
        .map_err(|_| SessionObservationError::Unavailable)?;
    let admitted = view
        .set_session_summary_bounded(summary, &mut ledger, INITIAL_RETENTION_LIMITS)
        .inspect_err(|_| {
            tracing::warn!("initial semantic observation summary admission rejected");
        })
        .map_err(|_| SessionObservationEnd::LimitReached)
        .and_then(|()| {
            for event in &page.events {
                // History source copies share the summary/queue/in-flight ledger.
                // Keep their lease until the projector has finished admitting the event.
                let source = match subscription {
                    ObservationSubscription::Bounded(subscription) => Some(
                        subscription
                            .admit_history_event(event)
                            .map_err(source_end)?,
                    ),
                    #[cfg(test)]
                    ObservationSubscription::Broadcast(_) => None,
                };
                let event = match source
                    .as_ref()
                    .map(bcode_session::BoundedSessionEventUpdate::event)
                {
                    Some(bcode_session::BoundedSessionEvent::Durable(event)) => event.as_ref(),
                    Some(bcode_session::BoundedSessionEvent::Live(_)) => {
                        return Err(SessionObservationEnd::IncompatibleSource);
                    }
                    None => event,
                };
                let source_charge = source
                    .as_ref()
                    .map_or_else(ObservationRetentionUsage::default, |source| {
                        source.retained_usage()
                    });
                ledger
                    .try_add(source_charge, INITIAL_RETENTION_LIMITS)
                    .map_err(|_| SessionObservationEnd::LimitReached)?;
                let presentation = super::compaction_privacy::presentation_event(event);
                let projected = view.apply_event_bounded(
                    presentation.as_ref(),
                    &mut ledger,
                    INITIAL_RETENTION_LIMITS,
                );
                if projected.is_err() {
                    tracing::warn!(
                        event_kind =
                            bcode_session_models::persisted_session_event_kind_name(&event.kind),
                        source_sequence = event.sequence,
                        "initial semantic observation projection admission rejected"
                    );
                }
                drop(source);
                ledger
                    .try_release(source_charge)
                    .map_err(|_| SessionObservationEnd::LimitReached)?;
                if projected.is_err() {
                    return Err(SessionObservationEnd::LimitReached);
                }
            }
            Ok(())
        });
    // Earlier successful events remain owned even if a later event is denied.
    view_charge.charge = view
        .observation_retention_usage(INITIAL_RETENTION_LIMITS)
        .map_err(|_| SessionObservationError::LimitReached)?;
    view_charge.charge.copies = 1;
    drop(ledger);
    drop(source_charge);
    Ok(admitted)
}

fn initial_source_sequence(
    session_id: SessionId,
    max_updates: usize,
    page: &bcode_session_models::ProjectionHistoryPage,
) -> Result<Option<u64>, SessionObservationError> {
    if page.session_id != session_id || page.window.has_newer || page.events.len() > max_updates {
        return Err(SessionObservationError::Unavailable);
    }
    let mut previous = None;
    for event in &page.events {
        if !compatible(event, session_id)
            || previous.is_some_and(|sequence: u64| sequence.checked_add(1) != Some(event.sequence))
        {
            return Err(SessionObservationError::Unavailable);
        }
        previous = Some(event.sequence);
    }
    Ok(previous)
}

fn initialize_observation(
    session_id: SessionId,
    max_updates: usize,
    max_snapshot_bytes: usize,
    subscription: ObservationSubscription,
    page: &bcode_session_models::ProjectionHistoryPage,
) -> Result<SessionObservation, SessionObservationError> {
    let previous = initial_source_sequence(session_id, max_updates, page)?;
    if !encoded_fits(&page.events, max_snapshot_bytes) {
        return Ok(initial_limit_observation(
            session_id,
            max_updates,
            max_snapshot_bytes,
        ));
    }
    let retention = RetentionAccount::new(INITIAL_RETENTION_LIMITS);
    let terminal_charge = retention
        .reserve(TERMINAL_USAGE)
        .map_err(|_| SessionObservationError::LimitReached)?;
    // Declare the guard before the view: unwinding must drop the nested owner
    // before releasing its reservation, just like the retained struct fields.
    let mut view_charge = retention
        .reserve(ObservationRetentionUsage::default())
        .map_err(|_| SessionObservationError::LimitReached)?;
    let mut view = SessionView::new();
    let mut retained = view
        .observation_retention_usage(INITIAL_RETENTION_LIMITS)
        .map_err(|_| SessionObservationError::LimitReached)?;
    retained.copies = 1;
    retention
        .usage
        .lock()
        .map_err(|_| SessionObservationError::Unavailable)?
        .try_add(retained, retention.limits)
        .map_err(|_| SessionObservationError::LimitReached)?;
    view_charge.charge = retained;
    let admitted =
        admit_initial_projection(&mut view, &mut view_charge, &retention, &subscription, page)?;
    if let Err(reason) = admitted {
        drop(view);
        drop(view_charge);
        let mut observation =
            initial_limit_observation(session_id, max_updates, max_snapshot_bytes);
        observation.pending_end = Some(reason);
        return Ok(observation);
    }
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
    if !snapshot_fits(view.snapshot(), max_snapshot_bytes) {
        drop(view);
        drop(view_charge);
        return Ok(initial_limit_observation(
            session_id,
            max_updates,
            max_snapshot_bytes,
        ));
    }
    let Ok((initial, initial_charge)) = clone_view_snapshot(&view, &retention) else {
        drop(view);
        drop(view_charge);
        return Ok(initial_limit_observation(
            session_id,
            max_updates,
            max_snapshot_bytes,
        ));
    };
    Ok(SessionObservation {
        session_id,
        view: Some(view),
        subscription: Some(subscription),
        initial: Some(initial),
        initial_charge: Some(initial_charge),
        view_charge: Some(view_charge),
        terminal_charge: Some(terminal_charge),
        retention,
        pending_end: None,
        delivery_sequence: 0,
        source_sequence: previous,
        initial_source_sequence: previous,
        updates: page.events.len(),
        max_updates,
        max_snapshot_bytes,
        #[cfg(test)]
        durable_open: true,
        #[cfg(test)]
        live_open: true,
    })
}

fn initial_limit_observation(
    session_id: SessionId,
    max_updates: usize,
    max_snapshot_bytes: usize,
) -> SessionObservation {
    let retention = RetentionAccount::new(INITIAL_RETENTION_LIMITS);
    let terminal_charge = retention.reserve(TERMINAL_USAGE).ok();
    SessionObservation {
        session_id,
        view: None,
        subscription: None,
        initial: None,
        initial_charge: None,
        view_charge: None,
        terminal_charge,
        retention,
        pending_end: Some(SessionObservationEnd::LimitReached),
        delivery_sequence: 0,
        source_sequence: None,
        initial_source_sequence: None,
        updates: 0,
        max_updates,
        max_snapshot_bytes,
        #[cfg(test)]
        durable_open: false,
        #[cfg(test)]
        live_open: false,
    }
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

const fn source_end(error: bcode_session::BoundedSessionEventError) -> SessionObservationEnd {
    match error {
        bcode_session::BoundedSessionEventError::Gap(loss) => source_gap(loss),
        bcode_session::BoundedSessionEventError::LimitReached => {
            SessionObservationEnd::LimitReached
        }
        bcode_session::BoundedSessionEventError::Closed => SessionObservationEnd::SourceClosed,
    }
}

const fn source_gap(loss: bcode_session::BoundedSessionEventLoss) -> SessionObservationEnd {
    let (source, skipped) = match (loss.durable_events, loss.live_events) {
        (Some(0), live) => (SessionObservationSource::Live, live),
        (durable, Some(0)) => (SessionObservationSource::Durable, durable),
        (durable, live) => (
            SessionObservationSource::Both,
            match (durable, live) {
                (Some(durable), Some(live)) => durable.checked_add(live),
                _ => None,
            },
        ),
    };
    SessionObservationEnd::Gap { source, skipped }
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
        // Nested owners drop before their reservations.
        self.view_charge = None;
        self.initial_charge = None;
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
        // Ordinary callers take ownership outside the host's retained ledger.
        self.recv_charged().await.map(|retained| retained.delivery)
    }

    async fn recv_charged(&mut self) -> Option<RetainedDelivery> {
        if let Some(snapshot) = self.initial.take() {
            let reservation = self.initial_charge.take()?;
            return Some(RetainedDelivery {
                delivery: self.delivery(SessionObservationEvent::Snapshot {
                    initial: true,
                    snapshot,
                }),
                reservation,
            });
        }
        loop {
            if let Some(reason) = self.pending_end.take() {
                let delivery = self.delivery(SessionObservationEvent::Ended { reason });
                // Terminal capacity was reserved when the observer was admitted;
                // exhaustion cannot erase the promised terminal transition.
                let reservation = self.terminal_charge.take()?;
                return Some(RetainedDelivery {
                    delivery,
                    reservation,
                });
            }
            let subscription = self.subscription.as_mut()?;
            let admitted = match subscription {
                ObservationSubscription::Bounded(subscription) => {
                    match subscription.recv().await {
                        Ok(update) => {
                            // Source and projected ownership overlap during admission.
                            let source_charge = self.retention.reserve(update.retained_usage());
                            let Ok(source_charge) = source_charge else {
                                drop(update);
                                self.end(SessionObservationEnd::LimitReached);
                                continue;
                            };
                            let admitted = match update.event() {
                                bcode_session::BoundedSessionEvent::Durable(event) => {
                                    self.apply_durable(event)
                                }
                                bcode_session::BoundedSessionEvent::Live(event) => {
                                    self.apply_live(event)
                                }
                            };
                            // The source lease survives until projector admission completes.
                            drop(update);
                            drop(source_charge);
                            admitted
                        }
                        Err(error) => {
                            self.end(source_end(error));
                            continue;
                        }
                    }
                }
                #[cfg(test)]
                ObservationSubscription::Broadcast(subscription) => {
                    let update = tokio::select! {
                        event = subscription.events.recv(), if self.durable_open => SourceUpdate::Durable(event.map(Box::new)),
                        event = subscription.live_events.recv(), if self.live_open => SourceUpdate::Live(event.map(Box::new)),
                        else => { self.end(SessionObservationEnd::SourceClosed); continue; }
                    };
                    self.apply(update)
                }
            };
            if !admitted {
                continue;
            }
            let Some(view) = self.view.as_ref() else {
                continue;
            };
            let cloned = clone_view_snapshot(view, &self.retention);
            let Ok((snapshot, reservation)) = cloned else {
                self.end(SessionObservationEnd::LimitReached);
                continue;
            };
            return Some(RetainedDelivery {
                delivery: self.delivery(SessionObservationEvent::Snapshot {
                    initial: false,
                    snapshot,
                }),
                reservation,
            });
        }
    }

    #[cfg(test)]
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
                    SessionObservationSource::Both => {
                        self.durable_open = false;
                        self.live_open = false;
                    }
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
        match update {
            SourceUpdate::Durable(Ok(event)) => self.apply_durable(&event),
            SourceUpdate::Live(Ok(event)) => self.apply_live(&event),
            _ => false,
        }
    }

    fn apply_durable(&mut self, event: &SessionEvent) -> bool {
        if !compatible(event, self.session_id) {
            self.end(SessionObservationEnd::IncompatibleSource);
            return false;
        }
        if !encoded_fits(event, self.max_snapshot_bytes) || self.updates >= self.max_updates {
            self.end(SessionObservationEnd::LimitReached);
            return false;
        }
        // Initial overlap is still a source update examined by this lifetime.
        self.updates += 1;
        if self
            .initial_source_sequence
            .is_some_and(|sequence| event.sequence <= sequence)
        {
            return false;
        }
        let expected = self
            .source_sequence
            .map_or(Some(0), |sequence| sequence.checked_add(1));
        if expected != Some(event.sequence) {
            self.end(SessionObservationEnd::Gap {
                source: SessionObservationSource::Durable,
                skipped: None,
            });
            return false;
        }
        let presentation = super::compaction_privacy::presentation_event(event);
        let admitted = self.project(|view, ledger, limits| {
            view.apply_event_bounded(presentation.as_ref(), ledger, limits)
        });
        if admitted {
            self.source_sequence = Some(event.sequence);
        } else if matches!(self.pending_end, Some(SessionObservationEnd::LimitReached)) {
            tracing::warn!(
                event_kind = bcode_session_models::persisted_session_event_kind_name(&event.kind),
                source_sequence = event.sequence,
                "live semantic observation durable projection admission rejected"
            );
        }
        admitted
    }

    fn apply_live(&mut self, event: &SessionLiveEvent) -> bool {
        if event.session_id != self.session_id {
            self.end(SessionObservationEnd::IncompatibleSource);
            return false;
        }
        if !encoded_fits(event, self.max_snapshot_bytes) || self.updates >= self.max_updates {
            self.end(SessionObservationEnd::LimitReached);
            return false;
        }
        self.updates += 1;
        self.project(|view, ledger, limits| view.apply_live_event_bounded(event, ledger, limits))
    }

    fn project(
        &mut self,
        apply: impl FnOnce(
            &mut SessionView,
            &mut ObservationRetentionUsage,
            ObservationRetentionLimits,
        ) -> Result<(), ObservationRetentionExceeded>,
    ) -> bool {
        let Some(view) = self.view.as_mut() else {
            return false;
        };
        let admitted = (|| {
            let mut ledger = self
                .retention
                .usage
                .lock()
                .map_err(|_| ObservationRetentionExceeded)?;
            apply(view, &mut ledger, self.retention.limits)?;
            let mut charge = view.observation_retention_usage(self.retention.limits)?;
            charge.copies = 1;
            if let Some(reservation) = self.view_charge.as_mut() {
                reservation.charge = charge;
            }
            Ok::<(), ObservationRetentionExceeded>(())
        })();
        if admitted.is_err() {
            self.end(SessionObservationEnd::LimitReached);
            return false;
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

#[cfg(test)]
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
    encoded_size(value, limit).is_some()
}

fn encoded_size(value: &impl serde::Serialize, limit: usize) -> Option<usize> {
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
    let mut budget = Budget(limit);
    serde_json::to_writer(&mut budget, value).ok()?;
    Some(limit - budget.0)
}

/// Host policy-bounded registry of authenticated connection-owned semantic observations.
/// Tokens, retained deliveries and projections are ephemeral and never durably resumed.
#[derive(Default)]
pub struct SessionObservationRegistry {
    entries:
        std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<HostedObservation>>>,
    reply_boxes: std::sync::Mutex<bcode_session_models::retention::ObservationRetentionUsage>,
}

// Outer reply allocations have an independent host-wide cardinality policy.
// Nested decoded values remain subject to their owning retention accounting.
const REPLY_BOX_LIMITS: bcode_session_models::retention::ObservationRetentionLimits =
    bcode_session_models::retention::ObservationRetentionLimits {
        scalar_capacity_bytes: 0,
        collection_entries: 64,
        value_nodes: 64,
        depth: 1,
        copies: 0,
    };
const REPLY_BOX_USAGE: bcode_session_models::retention::ObservationRetentionUsage =
    bcode_session_models::retention::ObservationRetentionUsage {
        scalar_capacity_bytes: 0,
        collection_entries: 1,
        value_nodes: 1,
        depth: 1,
        copies: 0,
    };

/// Reservation for one outer pull-response box, separate from nested payload retention.
///
/// Acquire before allocating the box. Keep this guard until the boxed response has
/// been sent or dropped, including during cancellation. Moving the response into a
/// queue requires moving this guard alongside it; cloning requires another reservation.
#[must_use]
pub struct SessionObservationReplyBoxReservation<'a> {
    usage: &'a std::sync::Mutex<bcode_session_models::retention::ObservationRetentionUsage>,
}

impl Drop for SessionObservationReplyBoxReservation<'_> {
    fn drop(&mut self) {
        if let Ok(mut usage) = self.usage.lock() {
            // A failed release preserves the charge and fails closed on later admission.
            let _ = usage.try_release(REPLY_BOX_USAGE);
        }
    }
}

struct HostedObservation {
    owner: bcode_session_models::ClientId,
    handle: bcode_session_models::observation::SessionObservationHandle,
    state: std::sync::Mutex<HostedObservationState>,
    closed: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

#[derive(Default)]
struct HostedObservationState {
    observation: Option<SessionObservation>,
    last: Option<RetainedDelivery>,
    pending: Option<RetainedDelivery>,
    retention: Option<Arc<RetentionAccount>>,
    next_sequence: u64,
    finished: bool,
}

// A cancelled pull restores the observation, not a silently consumed stream.
struct ObservationLease {
    entry: std::sync::Arc<HostedObservation>,
    observation: Option<SessionObservation>,
}
impl Drop for ObservationLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.entry.state.lock()
            && !self.entry.closed.load(std::sync::atomic::Ordering::SeqCst)
        {
            state.observation = self.observation.take();
        }
    }
}

impl std::fmt::Debug for SessionObservationRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionObservationRegistry")
            .field(
                "handles",
                &self
                    .entries
                    .lock()
                    .map(|entries| entries.len())
                    .unwrap_or_default(),
            )
            .finish_non_exhaustive()
    }
}

impl SessionObservationRegistry {
    #[cfg(test)]
    pub(super) fn retention_usage_probe(
        &self,
        handle_id: &str,
    ) -> impl Fn() -> ObservationRetentionUsage + use<> {
        let account = self
            .entries
            .lock()
            .unwrap()
            .get(handle_id)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .retention
            .clone()
            .unwrap();
        move || *account.usage.lock().unwrap()
    }

    /// Reserve one host-wide outer response allocation before `Box::new`.
    ///
    /// This reservation does not account for decoded response contents and does
    /// not make lost pull replies retryable. Hold the guard through transmission
    /// or response disposal, not merely through allocation.
    ///
    /// # Errors
    /// Returns `LimitReached` if all outer response slots are charged, or
    /// `Unavailable` if the ledger cannot be safely accessed.
    pub fn reserve_reply_box(
        &self,
    ) -> Result<SessionObservationReplyBoxReservation<'_>, SessionObservationError> {
        self.reply_boxes
            .lock()
            .map_err(|_| SessionObservationError::Unavailable)?
            .try_add(REPLY_BOX_USAGE, REPLY_BOX_LIMITS)
            .map_err(|_| SessionObservationError::LimitReached)?;
        Ok(SessionObservationReplyBoxReservation {
            usage: &self.reply_boxes,
        })
    }

    /// Reserve a bounded slot before creating a projection. The application must authorize
    /// the session and caller before invoking this operation.
    ///
    /// # Errors
    /// Rejects unknown/invalid contracts, policy limits and unavailable canonical sources.
    pub async fn open(
        &self,
        sessions: &SessionManager,
        owner: bcode_session_models::ClientId,
        request: SessionObservationRequest,
    ) -> Result<bcode_session_models::observation::SessionObservationHandle, SessionObservationError>
    {
        use bcode_session_models::observation::{
            MAX_CLIENT_SESSION_OBSERVATIONS, MAX_HOSTED_OBSERVATION_BYTES,
            MAX_HOSTED_OBSERVATION_UPDATES, MAX_HOSTED_SESSION_OBSERVATIONS,
            SessionObservationHandle,
        };
        validate_request(&request)?;
        if request.max_updates > MAX_HOSTED_OBSERVATION_UPDATES
            || request.max_snapshot_bytes > MAX_HOSTED_OBSERVATION_BYTES
            || request.window.limits.max_bytes > MAX_HOSTED_OBSERVATION_BYTES
            || request.window.limits.max_items > request.max_updates
            || request.window.limits.max_events_scanned > request.max_updates
        {
            return Err(SessionObservationError::InvalidRequest);
        }
        let handle = SessionObservationHandle {
            schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
            handle_id: uuid::Uuid::new_v4().to_string(),
            session_id: request.session_id,
        };
        let entry = std::sync::Arc::new(HostedObservation {
            owner,
            handle: handle.clone(),
            state: std::sync::Mutex::default(),
            closed: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        });
        {
            let mut entries = self
                .entries
                .lock()
                .map_err(|_| SessionObservationError::Unavailable)?;
            if entries.len() >= MAX_HOSTED_SESSION_OBSERVATIONS
                || entries
                    .values()
                    .filter(|entry| entry.owner == owner)
                    .count()
                    >= MAX_CLIENT_SESSION_OBSERVATIONS
            {
                return Err(SessionObservationError::LimitReached);
            }
            entries.insert(handle.handle_id.clone(), std::sync::Arc::clone(&entry));
        }
        let reservation = OpenReservation {
            registry: self,
            owner,
            handle: handle.clone(),
            committed: false,
        };
        let observation = observe_session(sessions, request).await?;
        {
            let mut state = entry
                .state
                .lock()
                .map_err(|_| SessionObservationError::Unavailable)?;
            if entry.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(SessionObservationError::NotFound);
            }
            state.retention = Some(Arc::clone(&observation.retention));
            state.observation = Some(observation);
        }
        let mut reservation = reservation;
        reservation.committed = true;
        Ok(handle)
    }

    /// Compatibility access for ordinary callers transferring ownership out of the host.
    /// Production adapters must use `pull_charged` and retain its reservation.
    ///
    /// # Errors
    /// Returns the same ownership, sequence and admission errors as `pull_charged`.
    #[cfg(test)]
    pub async fn pull(
        &self,
        owner: bcode_session_models::ClientId,
        request: bcode_session_models::observation::SessionObservationPullRequest,
    ) -> Result<
        bcode_session_models::SessionObservationPullResult<SessionViewSnapshot>,
        SessionObservationError,
    > {
        self.pull_charged(owner, request)
            .await
            .map(|reply| reply.result)
    }

    /// Pull one semantic delivery. No registry or per-handle mutex spans a receive await.
    /// At most the last delivery is retained for an exact sequence retry on this connection;
    /// disconnected callers must reopen, never replay from a token.
    ///
    /// # Errors
    /// Rejects future contracts, owner mismatch, expired tokens, concurrent pulls and stale sequences.
    pub async fn pull_charged(
        &self,
        owner: bcode_session_models::ClientId,
        request: bcode_session_models::observation::SessionObservationPullRequest,
    ) -> Result<ChargedObservationReply, SessionObservationError> {
        use bcode_session_models::observation::{
            MAX_OBSERVATION_PULL_WAIT_MS, SessionObservationPullResult,
        };
        if request.schema_version != SESSION_OBSERVATION_SCHEMA_VERSION {
            return Err(SessionObservationError::UnsupportedSchema);
        }
        if request.wait_ms > MAX_OBSERVATION_PULL_WAIT_MS || request.handle_id.len() > 64 {
            return Err(SessionObservationError::InvalidRequest);
        }
        let entry = self
            .entries
            .lock()
            .map_err(|_| SessionObservationError::Unavailable)?
            .get(&request.handle_id)
            .cloned()
            .ok_or(SessionObservationError::NotFound)?;
        if entry.owner != owner {
            return Err(SessionObservationError::Unauthorized);
        }
        let account = {
            let state = entry
                .state
                .lock()
                .map_err(|_| SessionObservationError::Unavailable)?;
            state
                .retention
                .clone()
                .or_else(|| {
                    state
                        .observation
                        .as_ref()
                        .map(|observation| Arc::clone(&observation.retention))
                })
                .ok_or(SessionObservationError::Conflict)?
        };
        // Reserve result wrapper and owned token BEFORE consuming a stream delivery.
        let header = account
            .reserve(ObservationRetentionUsage {
                scalar_capacity_bytes: request.handle_id.capacity(),
                value_nodes: 2,
                depth: 2,
                ..ObservationRetentionUsage::default()
            })
            .map_err(|_| SessionObservationError::LimitReached)?;
        let response = |delivery: Option<RetainedDelivery>, finished| {
            let (delivery, reservation) = match delivery {
                Some(delivery) => (
                    Some(delivery.delivery),
                    header
                        .join(delivery.reservation)
                        .map_err(|_| SessionObservationError::LimitReached)?,
                ),
                None => (None, header),
            };
            Ok(ChargedObservationReply {
                result: SessionObservationPullResult {
                    schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
                    handle_id: request.handle_id.clone(),
                    delivery,
                    finished,
                },
                reservation,
            })
        };
        Self::pull_delivery(&entry, &request, response).await
    }

    async fn pull_delivery(
        entry: &Arc<HostedObservation>,
        request: &bcode_session_models::observation::SessionObservationPullRequest,
        response: impl FnOnce(
            Option<RetainedDelivery>,
            bool,
        ) -> Result<ChargedObservationReply, SessionObservationError>,
    ) -> Result<ChargedObservationReply, SessionObservationError> {
        let (observation, pending) = {
            let mut state = entry
                .state
                .lock()
                .map_err(|_| SessionObservationError::Unavailable)?;
            if entry.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(SessionObservationError::NotFound);
            }
            if let Some(last) = &state.last
                && last.delivery.sequence == request.expected_sequence
            {
                let retained = clone_delivery(&last.delivery, &last.reservation.account)
                    .map_err(|_| SessionObservationError::LimitReached)?;
                return response(Some(retained), false);
            }
            if state.next_sequence != request.expected_sequence {
                return Err(SessionObservationError::Conflict);
            }
            if state.finished {
                return response(None, true);
            }
            let observation = state
                .observation
                .take()
                .ok_or(SessionObservationError::Conflict)?;
            let pending = state.pending.take();
            drop(state);
            (observation, pending)
        };
        let mut lease = ObservationLease {
            entry: std::sync::Arc::clone(entry),
            observation: Some(observation),
        };
        let notified = entry.notify.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        if entry.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SessionObservationError::NotFound);
        }
        let observation = lease
            .observation
            .as_mut()
            .ok_or(SessionObservationError::Unavailable)?;
        let received = if let Some(pending) = pending {
            Ok(Some(pending))
        } else {
            tokio::select! {
                () = &mut notified => return Err(SessionObservationError::NotFound),
                received = tokio::time::timeout(std::time::Duration::from_millis(u64::from(request.wait_ms)), observation.recv_charged()) => received,
            }
        };
        let Ok(delivery) = received else {
            return response(None, false);
        };
        let mut state = entry
            .state
            .lock()
            .map_err(|_| SessionObservationError::Unavailable)?;
        if entry.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SessionObservationError::NotFound);
        }
        let account = Arc::clone(
            &lease
                .observation
                .as_ref()
                .ok_or(SessionObservationError::Unavailable)?
                .retention,
        );
        if let Some(delivery) = delivery {
            if delivery.delivery.sequence != state.next_sequence {
                return Err(SessionObservationError::Conflict);
            }
            // Admit cache duplication while delivery, projector and prior cache
            // remain owned. Rejection does not allocate a retry copy.
            let Ok(cached) = clone_delivery(&delivery.delivery, &account) else {
                // Preserve the consumed delivery until outstanding replies release capacity.
                state.pending = Some(delivery);
                return Err(SessionObservationError::LimitReached);
            };
            state.next_sequence = state
                .next_sequence
                .checked_add(1)
                .ok_or(SessionObservationError::LimitReached)?;
            state.finished = matches!(
                delivery.delivery.event,
                SessionObservationEvent::Ended { .. }
            );
            state.last = Some(cached);
            drop(state);
            response(Some(delivery), false)
        } else {
            state.finished = true;
            drop(state);
            response(None, false)
        }
    }

    /// Release one authenticated owner's ephemeral handle; never cancels session execution.
    ///
    /// # Errors
    /// Rejects future versions, owner/session mismatch and poisoned state. Missing tokens return false.
    pub fn close(
        &self,
        owner: bcode_session_models::ClientId,
        handle: &bcode_session_models::observation::SessionObservationHandle,
    ) -> Result<bool, SessionObservationError> {
        if handle.schema_version != SESSION_OBSERVATION_SCHEMA_VERSION {
            return Err(SessionObservationError::UnsupportedSchema);
        }
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| SessionObservationError::Unavailable)?;
        let Some(entry) = entries.get(&handle.handle_id) else {
            return Ok(false);
        };
        if entry.owner != owner || entry.handle.session_id != handle.session_id {
            return Err(SessionObservationError::Unauthorized);
        }
        invalidate_observation(entry);
        let removed = entries.remove(&handle.handle_id);
        drop(entries);
        drop(removed);
        Ok(true)
    }

    /// Disconnect releases projections, retained replies and in-flight leases for the caller.
    pub fn remove_client(&self, owner: bcode_session_models::ClientId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|_, entry| {
                if entry.owner != owner {
                    return true;
                }
                invalidate_observation(entry);
                false
            });
        }
    }
}

fn invalidate_observation(entry: &HostedObservation) {
    entry
        .closed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    entry.notify.notify_waiters();
    if let Ok(mut state) = entry.state.lock() {
        *state = HostedObservationState::default();
    }
}
impl Drop for SessionObservationRegistry {
    fn drop(&mut self) {
        if let Ok(entries) = self.entries.get_mut() {
            for entry in entries.values() {
                invalidate_observation(entry);
            }
        }
    }
}
struct OpenReservation<'a> {
    registry: &'a SessionObservationRegistry,
    owner: bcode_session_models::ClientId,
    handle: bcode_session_models::observation::SessionObservationHandle,
    committed: bool,
}
impl Drop for OpenReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.registry.close(self.owner, &self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        ProjectionWindowDirection, ProjectionWindowLimits, ProjectionWindowRequest,
        ProjectionWindowTarget, SessionEventKind,
    };

    #[test]
    fn nested_clone_denial_preserves_ledger_and_snapshot() {
        let view = SessionView::new();
        let mut policy = INITIAL_RETENTION_LIMITS;
        policy.copies = 0;
        let account = RetentionAccount::new(policy);
        let owned = view.observation_retention_usage(policy).unwrap();
        let _view = account.reserve(owned).unwrap();
        let before = *account.usage.lock().unwrap();
        let snapshot = view.snapshot().clone();
        assert!(clone_view_snapshot(&view, &account).is_err());
        assert_eq!(*account.usage.lock().unwrap(), before);
        assert_eq!(view.snapshot(), &snapshot);
        let delivery = SessionObservationEnvelope {
            schema_version: 1,
            session_id: SessionId::new(),
            sequence: 0,
            source_sequence: None,
            event: SessionObservationEvent::Snapshot {
                initial: true,
                snapshot,
            },
        };
        assert!(clone_delivery(&delivery, &account).is_err());
        assert_eq!(*account.usage.lock().unwrap(), before);
    }

    #[tokio::test]
    async fn nested_reply_and_retry_cache_survive_close_until_final_drop() {
        use bcode_session_models::{ClientId, SessionObservationPullRequest};
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let registry = SessionObservationRegistry::default();
        let owner = ClientId::new();
        let handle = registry
            .open(&manager, owner, request(summary.id))
            .await
            .unwrap();
        let account = registry
            .entries
            .lock()
            .unwrap()
            .get(&handle.handle_id)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .retention
            .clone()
            .unwrap();
        let pull = || SessionObservationPullRequest {
            schema_version: 1,
            handle_id: handle.handle_id.clone(),
            expected_sequence: 0,
            wait_ms: 0,
        };
        let reply = registry.pull_charged(owner, pull()).await.unwrap();
        let first_usage = *account.usage.lock().unwrap();
        let duplicate = registry.pull_charged(owner, pull()).await.unwrap();
        assert_eq!(duplicate.result, reply.result);
        assert!(account.usage.lock().unwrap().value_nodes > first_usage.value_nodes);
        let cache = registry
            .entries
            .lock()
            .unwrap()
            .get(&handle.handle_id)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .last
            .as_ref()
            .unwrap()
            .reservation
            .charge;
        assert!(cache.value_nodes > 2);
        registry.close(owner, &handle).unwrap();
        let mut expected = reply.reservation.charge;
        expected
            .try_add(duplicate.reservation.charge, account.limits)
            .unwrap();
        let remaining = *account.usage.lock().unwrap();
        assert_eq!(
            remaining.scalar_capacity_bytes,
            expected.scalar_capacity_bytes
        );
        assert_eq!(remaining.collection_entries, expected.collection_entries);
        assert_eq!(remaining.value_nodes, expected.value_nodes);
        assert_eq!(remaining.copies, expected.copies);
        drop(duplicate);
        assert_eq!(
            account.usage.lock().unwrap().value_nodes,
            reply.reservation.charge.value_nodes
        );
        drop(reply);
        assert_eq!(
            *account.usage.lock().unwrap(),
            ObservationRetentionUsage::default()
        );
    }

    #[tokio::test]
    async fn cancelled_charged_pull_releases_header_but_keeps_cache() {
        use bcode_session_models::{ClientId, SessionObservationPullRequest};
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let registry = SessionObservationRegistry::default();
        let owner = ClientId::new();
        let handle = registry
            .open(&manager, owner, request(summary.id))
            .await
            .unwrap();
        let pull = |expected_sequence| SessionObservationPullRequest {
            schema_version: 1,
            handle_id: handle.handle_id.clone(),
            expected_sequence,
            wait_ms: 1000,
        };
        drop(registry.pull_charged(owner, pull(0)).await.unwrap());
        let account = registry
            .entries
            .lock()
            .unwrap()
            .get(&handle.handle_id)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .retention
            .clone()
            .unwrap();
        let before = *account.usage.lock().unwrap();
        {
            let waiting = registry.pull_charged(owner, pull(1));
            let mut waiting = std::pin::pin!(waiting);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(1), &mut waiting)
                    .await
                    .is_err()
            );
            assert!(account.usage.lock().unwrap().value_nodes > before.value_nodes);
        }
        assert_eq!(*account.usage.lock().unwrap(), before);
        let duplicate = registry.pull_charged(owner, pull(0)).await.unwrap();
        drop(duplicate);
        registry.close(owner, &handle).unwrap();
        assert_eq!(
            *account.usage.lock().unwrap(),
            ObservationRetentionUsage::default()
        );
    }

    #[tokio::test]
    async fn retry_clone_denial_is_atomic_and_original_cache_remains_readable() {
        use bcode_session_models::{ClientId, SessionObservationPullRequest};
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let registry = SessionObservationRegistry::default();
        let owner = ClientId::new();
        let handle = registry
            .open(&manager, owner, request(summary.id))
            .await
            .unwrap();
        let pull = || SessionObservationPullRequest {
            schema_version: 1,
            handle_id: handle.handle_id.clone(),
            expected_sequence: 0,
            wait_ms: 0,
        };
        let reply = registry.pull_charged(owner, pull()).await.unwrap();
        let account = Arc::clone(&reply.reservation.account);
        let usage = *account.usage.lock().unwrap();
        let occupied = account
            .reserve(ObservationRetentionUsage {
                scalar_capacity_bytes: account.limits.scalar_capacity_bytes
                    - usage.scalar_capacity_bytes,
                ..ObservationRetentionUsage::default()
            })
            .unwrap();
        let before = *account.usage.lock().unwrap();
        assert!(matches!(
            registry.pull_charged(owner, pull()).await,
            Err(SessionObservationError::LimitReached)
        ));
        assert_eq!(*account.usage.lock().unwrap(), before);
        drop(occupied);
        let retry = registry.pull_charged(owner, pull()).await.unwrap();
        assert_eq!(retry.result, reply.result);
        drop(retry);
        registry.close(owner, &handle).unwrap();
        drop(reply);
        assert_eq!(
            *account.usage.lock().unwrap(),
            ObservationRetentionUsage::default()
        );
    }

    #[tokio::test]
    async fn cache_copy_denial_retains_consumed_delivery_for_identical_retry() {
        use bcode_session_models::{ClientId, SessionObservationPullRequest};
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let registry = SessionObservationRegistry::default();
        let owner = ClientId::new();
        let handle = registry
            .open(&manager, owner, request(summary.id))
            .await
            .unwrap();
        let account = registry
            .entries
            .lock()
            .unwrap()
            .get(&handle.handle_id)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .retention
            .clone()
            .unwrap();
        let usage = *account.usage.lock().unwrap();
        let occupied = account
            .reserve(ObservationRetentionUsage {
                copies: account.limits.copies - usage.copies,
                ..ObservationRetentionUsage::default()
            })
            .unwrap();
        let pull = || SessionObservationPullRequest {
            schema_version: 1,
            handle_id: handle.handle_id.clone(),
            expected_sequence: 0,
            wait_ms: 0,
        };
        assert!(matches!(
            registry.pull_charged(owner, pull()).await,
            Err(SessionObservationError::LimitReached)
        ));
        {
            let entries = registry.entries.lock().unwrap();
            let state = entries
                .get(&handle.handle_id)
                .unwrap()
                .state
                .lock()
                .unwrap();
            assert_eq!(state.next_sequence, 0);
            assert!(state.last.is_none());
            assert_eq!(state.pending.as_ref().unwrap().delivery.sequence, 0);
            drop(state);
            drop(entries);
        }
        drop(occupied);
        let reply = registry.pull_charged(owner, pull()).await.unwrap();
        assert_eq!(reply.result.delivery.as_ref().unwrap().sequence, 0);
        registry.close(owner, &handle).unwrap();
        drop(reply);
        assert_eq!(
            *account.usage.lock().unwrap(),
            ObservationRetentionUsage::default()
        );
    }

    #[tokio::test]
    async fn projection_denial_releases_view_but_preserves_one_terminal_delivery() {
        let (mut observation, durable, _live) = observation().await;
        drop(observation.recv_charged().await.unwrap());
        let account = Arc::clone(&observation.retention);
        let usage = *account.usage.lock().unwrap();
        let occupied = account
            .reserve(ObservationRetentionUsage {
                scalar_capacity_bytes: account.limits.scalar_capacity_bytes
                    - usage.scalar_capacity_bytes,
                ..ObservationRetentionUsage::default()
            })
            .unwrap();
        let sequence = observation.source_sequence.unwrap() + 1;
        durable
            .send(event(observation.session_id, sequence))
            .unwrap();
        let delivery = observation.recv_charged().await.unwrap();
        assert!(matches!(
            delivery.delivery.event,
            SessionObservationEvent::Ended {
                reason: SessionObservationEnd::LimitReached
            }
        ));
        assert!(observation.view.is_none());
        assert!(observation.subscription.is_none());
        assert!(observation.recv_charged().await.is_none());
        drop(delivery);
        drop(occupied);
        assert_eq!(
            *account.usage.lock().unwrap(),
            ObservationRetentionUsage::default()
        );
    }

    #[tokio::test]
    async fn live_projection_denial_releases_all_owned_state_before_terminal() {
        let (mut observation, _durable, live) = observation().await;
        drop(observation.recv_charged().await.unwrap());
        let account = Arc::clone(&observation.retention);
        let usage = *account.usage.lock().unwrap();
        let occupied = account
            .reserve(ObservationRetentionUsage {
                scalar_capacity_bytes: account.limits.scalar_capacity_bytes
                    - usage.scalar_capacity_bytes,
                ..ObservationRetentionUsage::default()
            })
            .unwrap();
        live.send(live_update(
            observation.session_id,
            1,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 0,
                text: "live bounded admission".into(),
            },
        ))
        .unwrap();
        let delivery = observation.recv_charged().await.unwrap();
        assert!(matches!(
            delivery.delivery.event,
            SessionObservationEvent::Ended {
                reason: SessionObservationEnd::LimitReached
            }
        ));
        assert!(observation.view.is_none());
        assert!(observation.subscription.is_none());
        drop(delivery);
        drop(occupied);
        assert_eq!(
            *account.usage.lock().unwrap(),
            ObservationRetentionUsage::default()
        );
    }

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

    #[tokio::test]
    async fn bounded_source_queue_loss_reaches_observer_with_durable_provenance() {
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let mut observation = observe_session(&manager, request(summary.id))
            .await
            .unwrap();
        drop(observation.recv_charged().await.unwrap());
        let mut limits = source_limits();
        limits.max_queued_events = 1;
        let source = manager
            .subscribe_session_events_bounded(summary.id, limits)
            .await
            .unwrap();
        observation.subscription = Some(ObservationSubscription::Bounded(source));
        for _ in 0..2 {
            manager
                .append_context_compacted(summary.id, "context".into(), 0)
                .await
                .unwrap();
        }
        let terminal = observation.recv().await.unwrap();
        assert_eq!(terminal.sequence, 1);
        assert_end(
            terminal,
            SessionObservationEnd::Gap {
                source: SessionObservationSource::Durable,
                skipped: Some(2),
            },
        );
        assert!(observation.recv().await.is_none());
    }

    #[test]
    fn source_loss_preserves_live_durable_and_mixed_provenance() {
        for (durable_events, live_events, source, skipped) in [
            (Some(0), Some(3), SessionObservationSource::Live, Some(3)),
            (Some(2), Some(0), SessionObservationSource::Durable, Some(2)),
            (Some(2), Some(3), SessionObservationSource::Both, Some(5)),
            (None, Some(1), SessionObservationSource::Both, None),
            (
                Some(u64::MAX),
                Some(1),
                SessionObservationSource::Both,
                None,
            ),
        ] {
            assert_eq!(
                source_gap(bcode_session::BoundedSessionEventLoss {
                    durable_events,
                    live_events,
                    first_durable_sequence: None,
                    last_durable_sequence: None,
                }),
                SessionObservationEnd::Gap { source, skipped },
            );
        }
    }

    #[tokio::test]
    async fn initial_history_source_denial_emits_only_reserved_terminal() {
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let request = request(summary.id);
        let mut page = manager
            .session_projection_history_page(summary.id, request.window.clone())
            .await
            .unwrap();
        let mut oversized = event(summary.id, 0);
        oversized.kind = SessionEventKind::AssistantMessage {
            text: "a".repeat(4096),
        };
        page.events = vec![oversized];
        let mut limits = source_limits();
        limits.max_scalar_capacity_bytes = 2048;
        let source = manager
            .subscribe_session_events_bounded(summary.id, limits)
            .await
            .unwrap();
        assert!(
            source.session().is_ok(),
            "summary fits independently of history"
        );
        let mut observation = initialize_observation(
            summary.id,
            request.max_updates,
            request.max_snapshot_bytes,
            ObservationSubscription::Bounded(source),
            &page,
        )
        .unwrap();
        let usage = Arc::clone(&observation.retention);
        let terminal = observation.recv_charged().await.unwrap();
        assert_eq!(terminal.delivery.sequence, 0);
        assert_eq!(terminal.delivery.source_sequence, None);
        assert!(matches!(
            terminal.delivery.event,
            SessionObservationEvent::Ended {
                reason: SessionObservationEnd::LimitReached
            }
        ));
        assert!(observation.recv_charged().await.is_none());
        drop(terminal);
        assert_eq!(
            *usage.usage.lock().unwrap(),
            ObservationRetentionUsage::default()
        );
    }

    #[tokio::test]
    async fn denied_initial_summary_is_typed_and_never_exposed() {
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let mut limits = source_limits();
        limits.max_scalar_capacity_bytes = 0;
        let subscription = ObservationSubscription::Bounded(
            manager
                .subscribe_session_events_bounded(summary.id, limits)
                .await
                .unwrap(),
        );
        assert!(!initial_summary_fits(&subscription, 128 * 1024).unwrap());
        assert!(matches!(
            subscription.session(),
            Err(bcode_session::BoundedSessionEventError::LimitReached)
        ));
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
        observation.subscription = Some(ObservationSubscription::Broadcast(
            SessionEventSubscription {
                session: summary,
                events,
                live_events,
            },
        ));
        (observation, durable, live)
    }

    #[tokio::test]
    async fn initial_decode_budget_distinguishes_resource_canonical_and_valid_latest() {
        use bcode_session::{InitialObservationPageError, persisted::EventDecodeBudget};
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let request = request(summary.id);
        let limited = manager
            .session_projection_history_page_with_decode_budget(
                summary.id,
                request.window.clone(),
                EventDecodeBudget::new(
                    INITIAL_MAX_DECODED_JSON_BYTES,
                    bcode_session_models::retention::ObservationRetentionLimits {
                        value_nodes: 0,
                        ..INITIAL_RETENTION_LIMITS
                    },
                ),
            )
            .await;
        assert!(matches!(
            limited,
            Err(InitialObservationPageError::ResourceLimit)
        ));
        let mut observation = observe_session(&manager, request.clone()).await.unwrap();
        assert!(observation.initial.is_some());
        assert!(observation.subscription.is_some());
        observation.close();
        let missing = manager
            .session_projection_history_page_with_decode_budget(
                SessionId::new(),
                request.window,
                initial_decode_budget(),
            )
            .await;
        assert!(matches!(
            missing,
            Err(InitialObservationPageError::Canonical(
                SessionError::NotFound(_)
            ))
        ));
        let mut limited_observation = initial_limit_observation(summary.id, 128, 128 * 1024);
        assert!(limited_observation.subscription.is_none());
        assert!(limited_observation.view.is_none());
        assert_end(
            limited_observation.recv().await.unwrap(),
            SessionObservationEnd::LimitReached,
        );
        assert!(limited_observation.recv().await.is_none());
    }

    #[tokio::test]
    async fn tiny_initial_bound_delivers_limit_without_retaining_sources_or_projection() {
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let mut tiny = request(summary.id);
        tiny.max_snapshot_bytes = 1;
        let mut observation = observe_session(&manager, tiny.clone()).await.unwrap();
        assert!(observation.view.is_none());
        assert!(observation.subscription.is_none());
        assert!(observation.initial.is_none());
        let delivery = observation.recv().await.unwrap();
        assert_eq!(delivery.sequence, 0);
        assert_end(delivery, SessionObservationEnd::LimitReached);
        observation.close();
        assert!(observation.recv().await.is_none());
        tiny.max_snapshot_bytes = 0;
        assert!(matches!(
            observe_session(&manager, tiny).await,
            Err(SessionObservationError::InvalidRequest)
        ));
        let mut missing = request(SessionId::new());
        missing.max_snapshot_bytes = 1;
        assert!(matches!(
            observe_session(&manager, missing).await,
            Err(SessionObservationError::NotFound)
        ));
    }

    #[tokio::test]
    async fn hosted_request_caps_reject_before_reserving_resources() {
        use bcode_session_models::observation::{
            MAX_HOSTED_OBSERVATION_BYTES, MAX_HOSTED_OBSERVATION_UPDATES,
        };
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let owner = bcode_session_models::ClientId::new();
        let registry = SessionObservationRegistry::default();
        let base = request(summary.id);
        let mut oversized_snapshot = base.clone();
        oversized_snapshot.max_snapshot_bytes = MAX_HOSTED_OBSERVATION_BYTES + 1;
        let mut oversized_window = base.clone();
        oversized_window.window.limits.max_bytes = MAX_HOSTED_OBSERVATION_BYTES + 1;
        let mut oversized_updates = base.clone();
        oversized_updates.max_updates = MAX_HOSTED_OBSERVATION_UPDATES + 1;
        let mut oversized_scan = base;
        oversized_scan.window.limits.max_events_scanned = oversized_scan.max_updates + 1;
        for invalid in [
            oversized_snapshot,
            oversized_window,
            oversized_updates,
            oversized_scan,
        ] {
            assert_eq!(
                registry.open(&manager, owner, invalid).await,
                Err(SessionObservationError::InvalidRequest)
            );
            assert!(registry.entries.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn hosted_pull_preserves_projection_sequence_and_owner_cleanup() {
        use bcode_session_models::observation::SessionObservationPullRequest;
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let owner = bcode_session_models::ClientId::new();
        let registry = SessionObservationRegistry::default();
        let handle = registry
            .open(&manager, owner, request(summary.id))
            .await
            .unwrap();
        let pull = |sequence| SessionObservationPullRequest {
            schema_version: 1,
            handle_id: handle.handle_id.clone(),
            expected_sequence: sequence,
            wait_ms: 10,
        };
        let mut future_schema = pull(0);
        future_schema.schema_version += 1;
        assert_eq!(
            registry.pull(owner, future_schema).await,
            Err(SessionObservationError::UnsupportedSchema)
        );
        let mut missing_token = pull(0);
        missing_token.handle_id = "not-an-issued-token".into();
        assert_eq!(
            registry.pull(owner, missing_token).await,
            Err(SessionObservationError::NotFound)
        );
        assert_eq!(
            registry.close(bcode_session_models::ClientId::new(), &handle),
            Err(SessionObservationError::Unauthorized)
        );
        let mut mismatched_session = handle.clone();
        mismatched_session.session_id = SessionId::new();
        assert_eq!(
            registry.close(owner, &mismatched_session),
            Err(SessionObservationError::Unauthorized)
        );
        let first = registry.pull(owner, pull(0)).await.unwrap();
        assert_eq!(first.delivery.as_ref().unwrap().sequence, 0);
        assert!(matches!(
            first.delivery.as_ref().unwrap().event,
            SessionObservationEvent::Snapshot { initial: true, .. }
        ));
        assert_eq!(registry.pull(owner, pull(0)).await.unwrap(), first);
        assert_eq!(
            registry
                .pull(bcode_session_models::ClientId::new(), pull(1))
                .await,
            Err(SessionObservationError::Unauthorized)
        );
        assert_eq!(
            registry.pull(owner, pull(8)).await,
            Err(SessionObservationError::Conflict)
        );
        registry.remove_client(owner);
        assert_eq!(
            registry.pull(owner, pull(1)).await,
            Err(SessionObservationError::NotFound)
        );
        assert!(!registry.close(owner, &handle).unwrap());
    }

    #[tokio::test]
    async fn hosted_gap_is_terminal_and_fresh_handle_resyncs_without_replay() {
        use bcode_session_models::observation::{
            SessionObservationHandle, SessionObservationPullRequest,
        };
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
        observation.subscription = Some(ObservationSubscription::Broadcast(
            SessionEventSubscription {
                session: summary.clone(),
                events,
                live_events,
            },
        ));
        let owner = bcode_session_models::ClientId::new();
        let session_id = observation.session_id;
        let baseline = observation.source_sequence.unwrap();
        let handle = SessionObservationHandle {
            schema_version: 1,
            handle_id: "gap-fixture".into(),
            session_id,
        };
        let registry = SessionObservationRegistry::default();
        registry.entries.lock().unwrap().insert(
            handle.handle_id.clone(),
            std::sync::Arc::new(HostedObservation {
                owner,
                handle: handle.clone(),
                state: std::sync::Mutex::new(HostedObservationState {
                    observation: Some(observation),
                    ..HostedObservationState::default()
                }),
                closed: std::sync::atomic::AtomicBool::new(false),
                notify: tokio::sync::Notify::new(),
            }),
        );
        let pull = |sequence| SessionObservationPullRequest {
            schema_version: 1,
            handle_id: handle.handle_id.clone(),
            expected_sequence: sequence,
            wait_ms: 10,
        };
        registry.pull(owner, pull(0)).await.unwrap();
        durable.send(event(session_id, baseline + 2)).unwrap();
        let gap = registry.pull(owner, pull(1)).await.unwrap();
        assert!(matches!(
            gap.delivery.unwrap().event,
            SessionObservationEvent::Ended {
                reason: SessionObservationEnd::Gap {
                    source: SessionObservationSource::Durable,
                    skipped: None
                }
            }
        ));
        assert!(
            registry
                .pull(owner, pull(2))
                .await
                .unwrap()
                .delivery
                .is_none()
        );
        assert_eq!(durable.receiver_count(), 0);
        assert_eq!(live.receiver_count(), 0);
        assert!(registry.close(owner, &handle).unwrap());

        let fresh = registry
            .open(&manager, owner, request(session_id))
            .await
            .unwrap();
        assert_ne!(fresh.handle_id, handle.handle_id);
        let initial = registry
            .pull(
                owner,
                SessionObservationPullRequest {
                    schema_version: 1,
                    handle_id: fresh.handle_id.clone(),
                    expected_sequence: 0,
                    wait_ms: 10,
                },
            )
            .await
            .unwrap()
            .delivery
            .unwrap();
        assert_eq!(initial.sequence, 0);
        assert!(matches!(
            initial.event,
            SessionObservationEvent::Snapshot { initial: true, .. }
        ));
        registry.close(owner, &fresh).unwrap();
    }

    #[tokio::test]
    async fn hosted_cancelled_pull_restores_lease_and_disconnect_wakes_pending_receive() {
        use bcode_session_models::observation::SessionObservationPullRequest;
        let (observation, _durable, _live) = observation().await;
        let owner = bcode_session_models::ClientId::new();
        let handle = bcode_session_models::observation::SessionObservationHandle {
            schema_version: 1,
            handle_id: "lease-token".into(),
            session_id: observation.session_id,
        };
        let entry = std::sync::Arc::new(HostedObservation {
            owner,
            handle: handle.clone(),
            state: std::sync::Mutex::new(HostedObservationState {
                observation: Some(observation),
                ..HostedObservationState::default()
            }),
            closed: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        });
        let registry = SessionObservationRegistry::default();
        registry
            .entries
            .lock()
            .unwrap()
            .insert(handle.handle_id.clone(), std::sync::Arc::clone(&entry));
        let pull = |sequence| SessionObservationPullRequest {
            schema_version: 1,
            handle_id: handle.handle_id.clone(),
            expected_sequence: sequence,
            wait_ms: 1000,
        };
        registry.pull(owner, pull(0)).await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(1),
                registry.pull(owner, pull(1))
            )
            .await
            .is_err()
        );
        assert!(entry.state.lock().unwrap().observation.is_some());
        let waiting = registry.pull(owner, pull(1));
        tokio::pin!(waiting);
        tokio::select! { biased;
            result = &mut waiting => panic!("unexpected delivery: {result:?}"),
            () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
        }
        registry.remove_client(owner);
        assert_eq!(waiting.await, Err(SessionObservationError::NotFound));
        assert!(entry.state.lock().unwrap().observation.is_none());
    }

    #[tokio::test]
    async fn hosted_admission_caps_handle_count_and_releases_failed_reservations() {
        use bcode_session_models::observation::MAX_CLIENT_SESSION_OBSERVATIONS;
        let manager = SessionManager::default();
        let summary = manager
            .create_session(None, std::path::PathBuf::from("/"))
            .await
            .unwrap();
        let owner = bcode_session_models::ClientId::new();
        let registry = SessionObservationRegistry::default();
        for _ in 0..MAX_CLIENT_SESSION_OBSERVATIONS {
            registry
                .open(&manager, owner, request(summary.id))
                .await
                .unwrap();
        }
        assert_eq!(
            registry.open(&manager, owner, request(summary.id)).await,
            Err(SessionObservationError::LimitReached)
        );
        registry.remove_client(owner);
        assert!(registry.entries.lock().unwrap().is_empty());
        assert!(
            registry
                .open(&manager, owner, request(SessionId::new()))
                .await
                .is_err()
        );
        assert!(registry.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn reply_box_reservation_checks_before_allocation_and_releases_on_drop() {
        let registry = SessionObservationRegistry::default();
        let mut held = Vec::new();
        for _ in 0..REPLY_BOX_LIMITS.collection_entries {
            held.push(registry.reserve_reply_box().unwrap());
        }
        let before = *registry.reply_boxes.lock().unwrap();
        assert_eq!(
            before.collection_entries,
            REPLY_BOX_LIMITS.collection_entries
        );
        assert_eq!(before.value_nodes, REPLY_BOX_LIMITS.value_nodes);
        assert_eq!(before.scalar_capacity_bytes, 0);
        assert!(matches!(
            registry.reserve_reply_box(),
            Err(SessionObservationError::LimitReached)
        ));
        assert_eq!(*registry.reply_boxes.lock().unwrap(), before);
        drop(held.pop());
        let replacement = registry.reserve_reply_box().unwrap();
        assert_eq!(*registry.reply_boxes.lock().unwrap(), before);
        drop(replacement);
        drop(held);
        assert_eq!(registry.reply_boxes.lock().unwrap().collection_entries, 0);
        assert_eq!(registry.reply_boxes.lock().unwrap().value_nodes, 0);
    }

    #[tokio::test]
    async fn cancelled_response_send_releases_outer_box_reservation() {
        let registry = SessionObservationRegistry::default();
        let reservation = registry.reserve_reply_box().unwrap();
        let mut sending = Box::pin(async move {
            let _reservation = reservation;
            let _response = Box::new(
                bcode_session_models::observation::SessionObservationPullResult::<
                    SessionViewSnapshot,
                > {
                    schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
                    handle_id: String::new(),
                    delivery: None,
                    finished: false,
                },
            );
            std::future::pending::<()>().await;
        });
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(sending.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(registry.reply_boxes.lock().unwrap().collection_entries, 1);
        drop(sending);
        assert_eq!(registry.reply_boxes.lock().unwrap().collection_entries, 0);
        assert_eq!(registry.reply_boxes.lock().unwrap().value_nodes, 0);
    }

    #[tokio::test]
    async fn pending_open_drop_and_disconnect_release_admission_without_resurrection() {
        use bcode_session_models::observation::{
            MAX_CLIENT_SESSION_OBSERVATIONS, SessionObservationHandle,
        };
        let registry = SessionObservationRegistry::default();
        let owner = bcode_session_models::ClientId::new();
        let other = bcode_session_models::ClientId::new();
        let make_pending = |owner| {
            let handle = SessionObservationHandle {
                schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
                handle_id: uuid::Uuid::new_v4().to_string(),
                session_id: SessionId::new(),
            };
            let entry = std::sync::Arc::new(HostedObservation {
                owner,
                handle: handle.clone(),
                state: std::sync::Mutex::default(),
                closed: std::sync::atomic::AtomicBool::new(false),
                notify: tokio::sync::Notify::new(),
            });
            registry
                .entries
                .lock()
                .unwrap()
                .insert(handle.handle_id.clone(), std::sync::Arc::clone(&entry));
            (
                OpenReservation {
                    registry: &registry,
                    owner,
                    handle,
                    committed: false,
                },
                entry,
            )
        };
        let (cancelled, entry) = make_pending(owner);
        let mut opening = Box::pin(async move {
            let _reservation = cancelled;
            std::future::pending::<()>().await;
        });
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(opening.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(registry.entries.lock().unwrap().len(), 1);
        drop(opening);
        assert!(entry.closed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(registry.entries.lock().unwrap().is_empty());
        let mut pending = Vec::new();
        for _ in 0..MAX_CLIENT_SESSION_OBSERVATIONS {
            pending.push(make_pending(owner));
        }
        let (unrelated, unrelated_entry) = make_pending(other);
        // Pending reservations participate in normal admission even though
        // they have not subscribed or installed a projector yet.
        let manager = SessionManager::default();
        assert_eq!(
            registry
                .open(&manager, owner, request(SessionId::new()))
                .await,
            Err(SessionObservationError::LimitReached)
        );
        registry.remove_client(owner);
        assert_eq!(registry.entries.lock().unwrap().len(), 1);
        for (reservation, entry) in pending {
            assert!(entry.closed.load(std::sync::atomic::Ordering::SeqCst));
            assert!(entry.state.lock().unwrap().observation.is_none());
            drop(reservation);
        }
        assert!(
            !unrelated_entry
                .closed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        assert_eq!(registry.entries.lock().unwrap().len(), 1);
        drop(unrelated);
        assert!(registry.entries.lock().unwrap().is_empty());
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

    fn assert_live_snapshot(delivery: &SessionObservationDelivery, initial: bool) {
        match &delivery.event {
            SessionObservationEvent::Snapshot {
                initial: actual, ..
            } => {
                assert_eq!(*actual, initial);
            }
            SessionObservationEvent::Ended { reason } => panic!(
                "live stream ended before expected snapshot: reason={reason:?}, delivery_sequence={}, source_sequence={:?}",
                delivery.sequence, delivery.source_sequence,
            ),
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
        assert_live_snapshot(&observation.recv().await.unwrap(), false);
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
        assert_live_snapshot(&observation.recv().await.unwrap(), false);
        live.send(live_update(
            observation.session_id,
            2,
            TextStreamOperation::Terminal {
                status: TextStreamTerminalStatus::Cancelled,
            },
        ))
        .unwrap();
        let terminal = observation.recv().await.unwrap();
        assert_live_snapshot(&terminal, false);
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
