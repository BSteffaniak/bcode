//! Durable triage requests: which items go to the intake agent and how far delivery got.
//!
//! A request is written in the same transaction as its item, so a recorded item never loses
//! the person's decision to triage it. Dispatchers (a submitting client, or the daemon's
//! background dispatcher after a restart) take a short lease on a queued request before
//! delivering it, so concurrent dispatchers deliver each generation at most once at a time;
//! the intake session deduplicates repeated deliveries of one generation by idempotency key.

use bcode_feedback_models::{
    FeedbackId, FeedbackItem, IntakeTurnEnding, LeaseDueTriageRequest, LeaseTriageRequest,
    MAX_TRIAGE_GENERATIONS, RecordForTriageRequest, StartedTriage, StartedTriageQuery,
    TriageDispatch, TriageDispatchState, TriageFailedRequest, TriageIntent, TriageLease,
    TriageStartedRequest, TriageState, TriageTurnEndedRequest, bounded_note, validate_batch,
};
use switchy_database::query::{FilterableQuery as _, SortDirection};
use switchy_database::{Database, DatabaseValue, Row, TransactionMode};

use crate::database::block_on;
use crate::{FeedbackStore, FeedbackStoreError, int, optional_text, text, unsigned};

/// Table of triage requests, one row per item that has one.
pub const TABLE: &str = "feedback_triage_requests";
/// First retry delay after a failed delivery.
const FIRST_RETRY_DELAY_MS: u64 = 5_000;
/// Longest retry delay.
const MAX_RETRY_DELAY_MS: u64 = 10 * 60 * 1000;
/// Queued rows a due-lease scan reads.
const DUE_SCAN: usize = 256;

/// Delay before the next attempt after `attempts` consecutive failures: 5 s doubling, at most
/// 10 minutes.
#[must_use]
pub fn retry_delay_ms(attempts: u32) -> u64 {
    let doublings = attempts.saturating_sub(1).min(16);
    FIRST_RETRY_DELAY_MS
        .saturating_mul(1 << doublings)
        .min(MAX_RETRY_DELAY_MS)
}

/// One stored request, with the columns models do not expose.
#[derive(Debug, Clone)]
struct Request {
    item_id: u64,
    dispatch: TriageDispatch,
    lease_holder: Option<String>,
    lease_until_ms: Option<u64>,
    instructions: Option<String>,
}

impl Request {
    fn leased_by_other(&self, holder: &str, now_ms: u64) -> bool {
        self.lease_holder
            .as_deref()
            .is_some_and(|current| current != holder)
            && self.lease_until_ms.is_some_and(|until| until > now_ms)
    }
}

impl FeedbackStore {
    /// Record a new item and its triage decision in one transaction.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`] for an out-of-bounds request, or a database error.
    pub fn record_for_triage(
        &self,
        request: &RecordForTriageRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        request.validate()?;
        let id = block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            let id = crate::insert_item(&*tx, &request.item, now_ms).await?;
            let (state, instructions) = match &request.triage {
                TriageIntent::Requested { instructions } => {
                    (TriageDispatchState::Queued, instructions.as_deref())
                }
                TriageIntent::Skipped => (TriageDispatchState::Skipped, None),
            };
            tx.insert(TABLE)
                .value("item_id", int(id.0)?)
                .value("state", state_name(state))
                .value("generation", 1)
                .value("attempts", 0)
                .value("requested_at_ms", int(now_ms)?)
                .value("updated_at_ms", int(now_ms)?)
                .value(
                    "next_attempt_at_ms",
                    if state == TriageDispatchState::Queued {
                        int(now_ms)?
                    } else {
                        DatabaseValue::Null
                    },
                )
                .value("instructions", optional(instructions))
                .execute(&*tx)
                .await?;
            tx.commit().await?;
            Ok::<_, FeedbackStoreError>(id)
        })?;
        self.get(id)
    }

    /// Queue an untriaged item for triage, again or for the first time.
    ///
    /// A request that was skipped, blocked or had a turn admitted is queued at once; one whose
    /// turn was admitted, or that was blocked, gets a new generation so a new turn is delivered.
    ///
    /// # Errors
    /// * [`FeedbackStoreError::NotFound`] for an unknown id.
    /// * [`FeedbackStoreError::Forbidden`] when the item is already triaged.
    /// * A database error.
    pub fn request_triage(
        &self,
        id: FeedbackId,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        let item = self.get(id)?;
        if item.triage_state == TriageState::Triaged {
            return Err(FeedbackStoreError::Forbidden(
                "this item is already triaged",
            ));
        }
        block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            match read(&*tx, id).await? {
                None => {
                    tx.insert(TABLE)
                        .value("item_id", int(id.0)?)
                        .value("state", state_name(TriageDispatchState::Queued))
                        .value("generation", 1)
                        .value("attempts", 0)
                        .value("requested_at_ms", int(now_ms)?)
                        .value("updated_at_ms", int(now_ms)?)
                        .value("next_attempt_at_ms", int(now_ms)?)
                        .execute(&*tx)
                        .await?;
                }
                Some(request) => {
                    let generation = match request.dispatch.state {
                        TriageDispatchState::Queued | TriageDispatchState::Skipped => {
                            request.dispatch.generation
                        }
                        TriageDispatchState::Started
                        | TriageDispatchState::Blocked
                        | TriageDispatchState::Completed => {
                            request.dispatch.generation.saturating_add(1)
                        }
                    };
                    tx.update(TABLE)
                        .where_eq("item_id", int(id.0)?)
                        .value("state", state_name(TriageDispatchState::Queued))
                        .value("generation", i64::from(generation))
                        .value("attempts", 0)
                        .value("updated_at_ms", int(now_ms)?)
                        .value("next_attempt_at_ms", int(now_ms)?)
                        .value("blocked_reason", DatabaseValue::Null)
                        .value("lease_holder", DatabaseValue::Null)
                        .value("lease_until_ms", DatabaseValue::Null)
                        .execute(&*tx)
                        .await?;
                }
            }
            tx.commit().await?;
            Ok::<(), FeedbackStoreError>(())
        })?;
        self.get(id)
    }

    /// Lease one queued item for delivery, whether or not its retry delay has passed.
    ///
    /// Returns `None` when the item is not queued, is triaged, or another holder's lease is
    /// still running.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`], [`FeedbackStoreError::NotFound`], or a database error.
    pub fn lease_triage(
        &self,
        request: &LeaseTriageRequest,
        now_ms: u64,
    ) -> Result<Option<TriageLease>, FeedbackStoreError> {
        request.validate()?;
        let item = self.get(request.id)?;
        if item.triage_state == TriageState::Triaged {
            return Ok(None);
        }
        let leased = block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            let leased = match read(&*tx, request.id).await? {
                Some(stored)
                    if stored.dispatch.state == TriageDispatchState::Queued
                        && !stored.leased_by_other(&request.holder, now_ms) =>
                {
                    take_lease(&*tx, &stored, &request.holder, request.lease_ms, now_ms).await?;
                    Some(stored)
                }
                _ => None,
            };
            tx.commit().await?;
            Ok::<_, FeedbackStoreError>(leased)
        })?;
        leased
            .map(|stored| self.lease_of(stored, &request.holder))
            .transpose()
    }

    /// Lease queued, untriaged items whose retry delay has passed, oldest first.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`] for a bad holder, lease or limit, or a database error.
    pub fn lease_due_triage(
        &self,
        request: &LeaseDueTriageRequest,
        now_ms: u64,
    ) -> Result<Vec<TriageLease>, FeedbackStoreError> {
        request.validate()?;
        let taken = block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            let rows = tx
                .select(TABLE)
                .where_eq("state", state_name(TriageDispatchState::Queued))
                .sort("item_id", SortDirection::Asc)
                .limit(DUE_SCAN)
                .execute(&*tx)
                .await?;
            let mut leased = Vec::new();
            for row in &rows {
                if leased.len() == request.limit {
                    break;
                }
                let stored = from_row(row)?;
                let due = request.ignore_retry_delay
                    || stored
                        .dispatch
                        .next_attempt_at_ms
                        .is_none_or(|next| next <= now_ms);
                if due && !stored.leased_by_other(&request.holder, now_ms) {
                    take_lease(&*tx, &stored, &request.holder, request.lease_ms, now_ms).await?;
                    leased.push(stored);
                }
            }
            tx.commit().await?;
            Ok::<_, FeedbackStoreError>(leased)
        })?;
        let mut leases = Vec::with_capacity(taken.len());
        for stored in taken {
            let lease = self.lease_of(stored, &request.holder)?;
            if lease.item.triage_state == TriageState::Pending {
                leases.push(lease);
            }
        }
        Ok(leases)
    }

    /// Record that a leased generation's intake turn was admitted.
    ///
    /// Repeating it for the same generation is a no-op, as is recording it for an item that
    /// was triaged meanwhile.
    ///
    /// # Errors
    /// * [`FeedbackStoreError::Conflict`] when the generation changed, a person blocked or
    ///   skipped delivery, or another holder's lease is running.
    /// * [`FeedbackStoreError::Invalid`], [`FeedbackStoreError::NotFound`], or a database error.
    pub fn triage_started(
        &self,
        request: &TriageStartedRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        request.validate()?;
        self.transition(
            request.id,
            |stored| {
                if stored.dispatch.generation != request.generation {
                    return Err(FeedbackStoreError::Conflict(request.id));
                }
                match stored.dispatch.state {
                    TriageDispatchState::Started | TriageDispatchState::Completed => Ok(None),
                    TriageDispatchState::Queued
                        if !stored.leased_by_other(&request.holder, now_ms) =>
                    {
                        Ok(Some(Change {
                            state: TriageDispatchState::Started,
                            generation: stored.dispatch.generation,
                            attempts: 0,
                            next_attempt_at_ms: None,
                            last_error: None,
                            blocked_reason: None,
                            session: Some((request.session_id.clone(), request.turn_id.clone())),
                        }))
                    }
                    _ => Err(FeedbackStoreError::Conflict(request.id)),
                }
            },
            now_ms,
        )
    }

    /// Record that a leased generation could not be delivered: retry it after a delay, or
    /// block delivery.
    ///
    /// # Errors
    /// * [`FeedbackStoreError::Conflict`] when the generation changed, the request is no longer
    ///   queued, or another holder's lease is running.
    /// * [`FeedbackStoreError::Invalid`], [`FeedbackStoreError::NotFound`], or a database error.
    pub fn triage_failed(
        &self,
        request: &TriageFailedRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        request.validate()?;
        self.transition(
            request.id,
            |stored| {
                if stored.dispatch.generation != request.generation
                    || stored.dispatch.state != TriageDispatchState::Queued
                    || stored.leased_by_other(&request.holder, now_ms)
                {
                    return Err(FeedbackStoreError::Conflict(request.id));
                }
                let attempts = stored.dispatch.attempts.saturating_add(1);
                Ok(Some(if request.retry {
                    Change {
                        state: TriageDispatchState::Queued,
                        generation: stored.dispatch.generation,
                        attempts,
                        next_attempt_at_ms: Some(now_ms.saturating_add(retry_delay_ms(attempts))),
                        last_error: Some(request.reason.clone()),
                        blocked_reason: None,
                        session: None,
                    }
                } else {
                    Change {
                        state: TriageDispatchState::Blocked,
                        generation: stored.dispatch.generation,
                        attempts,
                        next_attempt_at_ms: None,
                        last_error: Some(request.reason.clone()),
                        blocked_reason: Some(request.reason.clone()),
                        session: None,
                    }
                }))
            },
            now_ms,
        )
    }

    /// Untriaged items whose intake turn was admitted, oldest first.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`] for a bad limit, or a database error.
    pub fn started_triages(
        &self,
        query: StartedTriageQuery,
    ) -> Result<Vec<StartedTriage>, FeedbackStoreError> {
        validate_batch(query.limit)?;
        block_on(async {
            let rows = self
                .database
                .select(TABLE)
                .where_eq("state", state_name(TriageDispatchState::Started))
                .sort("item_id", SortDirection::Asc)
                .limit(query.limit)
                .execute(&*self.database)
                .await?;
            rows.iter()
                .map(|row| {
                    let stored = from_row(row)?;
                    match (stored.dispatch.session_id, stored.dispatch.turn_id) {
                        (Some(session_id), Some(turn_id)) => Ok(Some(StartedTriage {
                            id: FeedbackId(stored.item_id),
                            generation: stored.dispatch.generation,
                            session_id,
                            turn_id,
                        })),
                        _ => Ok(None),
                    }
                })
                .filter_map(Result::transpose)
                .collect()
        })
    }

    /// Record that an item's admitted intake turn ended without triaging it.
    ///
    /// A failed turn is delivered again as a new generation after a delay. A turn that
    /// finished without triaging is delivered again until [`MAX_TRIAGE_GENERATIONS`] turns
    /// did so, then delivery is blocked. A cancelled turn blocks delivery: a person stopped it.
    /// Nothing changes when the turn is no longer the item's current one or the item was
    /// triaged meanwhile.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`], [`FeedbackStoreError::NotFound`], or a database error.
    pub fn triage_turn_ended(
        &self,
        request: &TriageTurnEndedRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        request.validate()?;
        if self.get(request.id)?.triage_state == TriageState::Triaged {
            return self.get(request.id);
        }
        self.transition(
            request.id,
            |stored| {
                if stored.dispatch.state != TriageDispatchState::Started
                    || stored.dispatch.generation != request.generation
                    || stored.dispatch.turn_id.as_deref() != Some(request.turn_id.as_str())
                {
                    return Ok(None);
                }
                let attempts = stored.dispatch.attempts.saturating_add(1);
                let generation = stored.dispatch.generation;
                let retry = |reason: String| Change {
                    state: TriageDispatchState::Queued,
                    generation: generation.saturating_add(1),
                    attempts,
                    next_attempt_at_ms: Some(now_ms.saturating_add(retry_delay_ms(attempts))),
                    last_error: Some(reason),
                    blocked_reason: None,
                    session: None,
                };
                let block = |reason: String| Change {
                    state: TriageDispatchState::Blocked,
                    generation,
                    attempts,
                    next_attempt_at_ms: None,
                    last_error: Some(reason.clone()),
                    blocked_reason: Some(reason),
                    session: None,
                };
                Ok(Some(match request.ending {
                    IntakeTurnEnding::Failed => retry(bounded_note(&format!(
                        "intake turn failed: {}",
                        request.detail
                    ))),
                    IntakeTurnEnding::Untriaged if generation >= MAX_TRIAGE_GENERATIONS => {
                        block(bounded_note(&format!(
                            "{generation} intake turns finished without triaging it (last: {})",
                            request.detail
                        )))
                    }
                    IntakeTurnEnding::Untriaged => retry(bounded_note(&format!(
                        "intake turn finished without triaging it ({})",
                        request.detail
                    ))),
                    IntakeTurnEnding::Cancelled => {
                        block(bounded_note("the intake turn was cancelled"))
                    }
                }))
            },
            now_ms,
        )
    }

    /// Apply `decide`'s change to `id`'s request in one transaction, then read the item.
    fn transition(
        &self,
        id: FeedbackId,
        decide: impl FnOnce(&Request) -> Result<Option<Change>, FeedbackStoreError>,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            let Some(stored) = read(&*tx, id).await? else {
                tx.rollback().await?;
                return Err(FeedbackStoreError::Conflict(id));
            };
            let change = match decide(&stored) {
                Ok(change) => change,
                Err(error) => {
                    tx.rollback().await?;
                    return Err(error);
                }
            };
            if let Some(change) = change {
                let mut update = tx
                    .update(TABLE)
                    .where_eq("item_id", int(id.0)?)
                    .value("state", state_name(change.state))
                    .value("generation", i64::from(change.generation))
                    .value("attempts", i64::from(change.attempts))
                    .value("updated_at_ms", int(now_ms)?)
                    .value(
                        "next_attempt_at_ms",
                        optional_int(change.next_attempt_at_ms)?,
                    )
                    .value("last_error", optional(change.last_error.as_deref()))
                    .value("blocked_reason", optional(change.blocked_reason.as_deref()))
                    .value("lease_holder", DatabaseValue::Null)
                    .value("lease_until_ms", DatabaseValue::Null);
                if let Some((session_id, turn_id)) = &change.session {
                    update = update
                        .value("session_id", session_id.as_str())
                        .value("turn_id", turn_id.as_str());
                }
                update.execute(&*tx).await?;
            }
            tx.commit().await?;
            Ok::<(), FeedbackStoreError>(())
        })?;
        self.get(id)
    }

    fn lease_of(&self, stored: Request, holder: &str) -> Result<TriageLease, FeedbackStoreError> {
        Ok(TriageLease {
            item: self.get(FeedbackId(stored.item_id))?,
            generation: stored.dispatch.generation,
            instructions: stored.instructions,
            holder: holder.to_owned(),
        })
    }
}

/// A request's next delivery state.
struct Change {
    state: TriageDispatchState,
    generation: u32,
    attempts: u32,
    next_attempt_at_ms: Option<u64>,
    last_error: Option<String>,
    blocked_reason: Option<String>,
    /// The admitted turn, when one was.
    session: Option<(String, String)>,
}

/// Mark the triage request of a newly triaged item completed, inside the triage transaction.
pub(crate) async fn complete(
    database: &dyn Database,
    id: FeedbackId,
    now_ms: u64,
) -> Result<(), FeedbackStoreError> {
    database
        .update(TABLE)
        .where_eq("item_id", int(id.0)?)
        .value("state", state_name(TriageDispatchState::Completed))
        .value("updated_at_ms", int(now_ms)?)
        .value("next_attempt_at_ms", DatabaseValue::Null)
        .value("blocked_reason", DatabaseValue::Null)
        .value("lease_holder", DatabaseValue::Null)
        .value("lease_until_ms", DatabaseValue::Null)
        .execute(database)
        .await?;
    Ok(())
}

/// The dispatch state of each item in `rows` of this table.
pub(crate) fn dispatches(rows: &[Row]) -> Result<Vec<(u64, TriageDispatch)>, FeedbackStoreError> {
    rows.iter()
        .map(|row| from_row(row).map(|stored| (stored.item_id, stored.dispatch)))
        .collect()
}

async fn read(
    database: &dyn Database,
    id: FeedbackId,
) -> Result<Option<Request>, FeedbackStoreError> {
    database
        .select(TABLE)
        .where_eq("item_id", int(id.0)?)
        .execute_first(database)
        .await?
        .as_ref()
        .map(from_row)
        .transpose()
}

async fn take_lease(
    database: &dyn Database,
    stored: &Request,
    holder: &str,
    lease_ms: u64,
    now_ms: u64,
) -> Result<(), FeedbackStoreError> {
    database
        .update(TABLE)
        .where_eq("item_id", int(stored.item_id)?)
        .value("lease_holder", holder)
        .value("lease_until_ms", int(now_ms.saturating_add(lease_ms))?)
        .execute(database)
        .await?;
    Ok(())
}

fn from_row(row: &Row) -> Result<Request, FeedbackStoreError> {
    Ok(Request {
        item_id: unsigned(row, "item_id")?,
        dispatch: TriageDispatch {
            state: parse_state(&text(row, "state")?)?,
            generation: small(row, "generation")?,
            attempts: small(row, "attempts")?,
            requested_at_ms: unsigned(row, "requested_at_ms")?,
            updated_at_ms: unsigned(row, "updated_at_ms")?,
            next_attempt_at_ms: optional_unsigned(row, "next_attempt_at_ms")?,
            last_error: optional_text(row, "last_error")?,
            blocked_reason: optional_text(row, "blocked_reason")?,
            session_id: optional_text(row, "session_id")?,
            turn_id: optional_text(row, "turn_id")?,
        },
        lease_holder: optional_text(row, "lease_holder")?,
        lease_until_ms: optional_unsigned(row, "lease_until_ms")?,
        instructions: optional_text(row, "instructions")?,
    })
}

fn small(row: &Row, column: &str) -> Result<u32, FeedbackStoreError> {
    u32::try_from(unsigned(row, column)?)
        .map_err(|_| FeedbackStoreError::Corrupt(format!("column {column}")))
}

fn optional_unsigned(row: &Row, column: &str) -> Result<Option<u64>, FeedbackStoreError> {
    match row.get(column) {
        None | Some(DatabaseValue::Null | DatabaseValue::Int64Opt(None)) => Ok(None),
        Some(value) => value
            .as_i64()
            .and_then(|value| u64::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| FeedbackStoreError::Corrupt(format!("column {column}"))),
    }
}

fn optional(value: Option<&str>) -> DatabaseValue {
    value.map_or(DatabaseValue::Null, |value| {
        DatabaseValue::String(value.to_owned())
    })
}

fn optional_int(value: Option<u64>) -> Result<DatabaseValue, FeedbackStoreError> {
    value.map_or(Ok(DatabaseValue::Null), int)
}

const fn state_name(state: TriageDispatchState) -> &'static str {
    match state {
        TriageDispatchState::Queued => "queued",
        TriageDispatchState::Started => "started",
        TriageDispatchState::Blocked => "blocked",
        TriageDispatchState::Skipped => "skipped",
        TriageDispatchState::Completed => "completed",
    }
}

fn parse_state(value: &str) -> Result<TriageDispatchState, FeedbackStoreError> {
    Ok(match value {
        "queued" => TriageDispatchState::Queued,
        "started" => TriageDispatchState::Started,
        "blocked" => TriageDispatchState::Blocked,
        "skipped" => TriageDispatchState::Skipped,
        "completed" => TriageDispatchState::Completed,
        other => {
            return Err(FeedbackStoreError::Corrupt(format!(
                "triage dispatch state {other:?}"
            )));
        }
    })
}
