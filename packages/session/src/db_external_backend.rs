//! Indexed external evidence helpers owned by the canonical DB module.

use super::*;
use bcode_harness_models::HarnessDeliveryState as D;

use bcode_session_models::external_backend::{
    ExternalBackendEnvelope, ExternalBackendPage, ExternalBackendQuery, ExternalBackendRecord,
    MAX_EXTERNAL_RECORD_BYTES, external_backend_key,
};

const EXTERNAL_INDEX_KIND: &str = "external_v1";
const EXTERNAL_ID_KIND: &str = "external_id_v1";
const EXTERNAL_ALL_KEY: &str = "__bcode_external_v1:all";

pub(super) async fn external_index_ready(db: &dyn Database) -> SessionDbResult<()> {
    validate_storage_writer_contract(db).await?;
    if let Some(tail) = last_sequence(db).await? {
        let snapshot = projection_checkpoint_snapshot(db).await?;
        validate_projection_checkpoint_snapshot(&snapshot, tail)?;
    }
    let rows = db
        .select("sqlite_master")
        .columns(&["name"])
        .where_eq("type", "index")
        .where_eq("name", "idx_turn_evidence_sequence")
        .limit(1)
        .execute(db)
        .await?;
    if rows.is_empty() {
        return Err(SessionDbError::InvalidData(
            "external evidence index unavailable; explicit maintenance required".into(),
        ));
    }
    Ok(())
}

async fn external_event_at(
    db: &dyn Database,
    session_id: SessionId,
    sequence: u64,
) -> SessionDbResult<SessionEvent> {
    let lengths = db
        .select("events")
        .project_as(
            switchy::database::query::byte_length(switchy::database::query::identifier("payload")),
            "payload_bytes",
        )
        .where_eq("event_seq", seq_to_value(sequence))
        .limit(1)
        .execute(db)
        .await?;
    let row = lengths.first().ok_or_else(|| {
        SessionDbError::InvalidData("external evidence points to missing event".into())
    })?;
    // Bound the physical canonical row before loading it. Nested semantic strings may escape.
    if required_non_negative_u64(row, "payload_bytes")?
        > (MAX_EXTERNAL_RECORD_BYTES * 6 + 4096) as u64
    {
        return Err(SessionDbError::InvalidData(
            "external evidence exceeds physical byte bound".into(),
        ));
    }
    let rows = db
        .select("events")
        .columns(&["event_seq", "payload"])
        .where_eq("event_seq", seq_to_value(sequence))
        .limit(1)
        .execute(db)
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| SessionDbError::InvalidData("external canonical event missing".into()))?;
    let event = strict_event_from_row(row, session_id)?;
    ExternalBackendEnvelope::from_event(&event)
        .map_err(SessionDbError::InvalidData)?
        .ok_or_else(|| {
            SessionDbError::InvalidData("external locator targets unrelated event".into())
        })?;
    Ok(event)
}

async fn external_latest(
    db: &dyn Database,
    session_id: SessionId,
    key: &str,
    kind: &str,
) -> SessionDbResult<Option<SessionEvent>> {
    let rows = db
        .select("turn_evidence")
        .columns(&["event_seq"])
        .where_eq("turn_id", key.to_owned())
        .where_eq("evidence_kind", kind.to_owned())
        .sort("event_seq", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let event =
        external_event_at(db, session_id, required_non_negative_u64(row, "event_seq")?).await?;
    let envelope = external_envelope(&event)?;
    let valid = if kind == EXTERNAL_ID_KIND {
        envelope.record.identity_key() == key
    } else {
        key == EXTERNAL_ALL_KEY
            || envelope
                .record
                .correlation_keys()
                .iter()
                .any(|item| item == key)
    };
    if !valid {
        return Err(SessionDbError::InvalidData(
            "external locator correlation mismatch".into(),
        ));
    }
    Ok(Some(event))
}

fn external_envelope(event: &SessionEvent) -> SessionDbResult<ExternalBackendEnvelope> {
    ExternalBackendEnvelope::from_event(event)
        .map_err(SessionDbError::InvalidData)?
        .ok_or_else(|| SessionDbError::InvalidData("missing external envelope".into()))
}

pub(super) async fn project_external_backend(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<bool> {
    let Some(envelope) =
        ExternalBackendEnvelope::from_event(event).map_err(SessionDbError::InvalidData)?
    else {
        return Ok(false);
    };
    let identity = envelope.record.identity_key();
    db.insert("turn_evidence")
        .value("turn_id", identity)
        .value("evidence_kind", EXTERNAL_ID_KIND)
        .value("segment_order", seq_to_value(event.sequence))
        .value("event_seq", seq_to_value(event.sequence))
        .execute(db)
        .await?;
    let mut keys = envelope.record.correlation_keys();
    keys.push(EXTERNAL_ALL_KEY.into());
    for key in keys {
        db.insert("turn_evidence")
            .value("turn_id", key)
            .value("evidence_kind", EXTERNAL_INDEX_KIND)
            .value("segment_order", seq_to_value(event.sequence))
            .value("event_seq", seq_to_value(event.sequence))
            .execute(db)
            .await?;
    }
    Ok(true)
}

pub(super) async fn validate_external_backend_append(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    if matches!(
        event.kind,
        SessionEventKind::ExternalHarnessOperation { .. }
    ) {
        return Ok(());
    }
    let Some(envelope) =
        ExternalBackendEnvelope::from_event(event).map_err(SessionDbError::InvalidData)?
    else {
        return Ok(());
    };
    external_index_ready(db).await?;
    let record = &envelope.record;
    if external_latest(
        db,
        event.session_id,
        &record.identity_key(),
        EXTERNAL_ID_KIND,
    )
    .await?
    .is_some()
    {
        return Err(SessionDbError::InvalidData(
            "external identity already recorded; use retained receipt".into(),
        ));
    }
    let latest = external_latest(
        db,
        event.session_id,
        &external_backend_key("binding", "latest"),
        EXTERNAL_INDEX_KIND,
    )
    .await?;
    if let ExternalBackendRecord::Binding { instance, binding } = record {
        if let Some(latest) = latest {
            let ExternalBackendRecord::Binding {
                instance: previous_instance,
                binding: previous,
            } = external_envelope(&latest)?.record
            else {
                return Err(SessionDbError::InvalidData(
                    "external binding locator mismatch".into(),
                ));
            };
            if previous_instance != *instance
                || previous.instance_id != binding.instance_id
                || previous.native_session_id != binding.native_session_id
                || previous.generation.checked_add(1) != Some(binding.generation)
            {
                return Err(SessionDbError::InvalidData(
                    "external binding ownership fence mismatch".into(),
                ));
            }
        } else if binding.generation != 1 {
            return Err(SessionDbError::InvalidData(
                "initial external generation must be one".into(),
            ));
        }
        return Ok(());
    }
    let latest =
        latest.ok_or_else(|| SessionDbError::InvalidData("external binding required".into()))?;
    let binding = external_envelope(&latest)?;
    if record.generation() != binding.record.generation() {
        return Err(SessionDbError::InvalidData(
            "stale external generation".into(),
        ));
    }
    validate_external_lifecycle(db, event, record).await
}

async fn validate_external_lifecycle(
    db: &dyn Database,
    event: &SessionEvent,
    record: &ExternalBackendRecord,
) -> SessionDbResult<()> {
    match record {
        ExternalBackendRecord::Receipt { receipt } => {
            validate_external_receipt(db, event, receipt).await?;
        }
        ExternalBackendRecord::Response {
            interaction,
            response,
        } => {
            let prior = external_latest(
                db,
                event.session_id,
                &external_backend_key("interaction", &response.request_id),
                EXTERNAL_INDEX_KIND,
            )
            .await?
            .ok_or_else(|| {
                SessionDbError::InvalidData("external response lacks observed request".into())
            })?;
            let ExternalBackendRecord::Observation { observation } =
                external_envelope(&prior)?.record
            else {
                return Err(SessionDbError::InvalidData(
                    "external interaction already answered".into(),
                ));
            };
            let bcode_harness_models::HarnessObservationKind::Interaction {
                interaction: observed,
            } = observation.kind
            else {
                return Err(SessionDbError::InvalidData(
                    "external request locator mismatch".into(),
                ));
            };
            if observed != *interaction {
                return Err(SessionDbError::InvalidData(
                    "external response request mismatch".into(),
                ));
            }
        }
        ExternalBackendRecord::Observation { observation } => {
            if matches!(
                observation.kind,
                bcode_harness_models::HarnessObservationKind::Settlement { .. }
            ) && observation.completeness != bcode_harness_models::HarnessCompleteness::Complete
            {
                return Err(SessionDbError::InvalidData(
                    "incomplete external settlement is not authoritative".into(),
                ));
            }
            // A result/status/idle observation never creates this terminal fence. Only
            // explicit complete settlement does; later updates to that native stream fail closed.
            for key in record
                .correlation_keys()
                .iter()
                .filter(|key| key.starts_with("__bcode_external_v1:6:native:"))
            {
                if let Some(prior) =
                    external_latest(db, event.session_id, key, EXTERNAL_INDEX_KIND).await?
                    && matches!(external_envelope(&prior)?.record,
                        ExternalBackendRecord::Observation { observation } if matches!(observation.kind, bcode_harness_models::HarnessObservationKind::Settlement { .. }))
                {
                    return Err(SessionDbError::InvalidData(
                        "external stream already settled".into(),
                    ));
                }
            }
            if let bcode_harness_models::HarnessObservationKind::Interaction { interaction } =
                &observation.kind
                && external_latest(
                    db,
                    event.session_id,
                    &external_backend_key("interaction", &interaction.request_id),
                    EXTERNAL_INDEX_KIND,
                )
                .await?
                .is_some()
            {
                return Err(SessionDbError::InvalidData(
                    "external interaction identity already recorded".into(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

impl SessionDb {
    /// Exact retry lookup. Does not authorize resending a native mutation.
    /// # Errors
    /// Rejects stale indexes, conflicting retries and unavailable canonical storage.
    pub async fn external_backend_retry(
        &self,
        record: &ExternalBackendRecord,
    ) -> SessionDbResult<Option<SessionEvent>> {
        record.validate().map_err(SessionDbError::InvalidData)?;
        if record.session_id() != self.session_id.to_string() {
            return Err(SessionDbError::InvalidData(
                "external retry session mismatch".into(),
            ));
        }
        external_index_ready(&**self.db).await?;
        if let Some(binding) = external_latest(
            &**self.db,
            self.session_id,
            &external_backend_key("binding", "latest"),
            EXTERNAL_INDEX_KIND,
        )
        .await?
        {
            let current_generation = external_envelope(&binding)?.record.generation();
            if record.generation() < current_generation {
                return Err(SessionDbError::InvalidData(
                    "stale external retry generation".into(),
                ));
            }
        }
        let prior = external_latest(
            &**self.db,
            self.session_id,
            &record.identity_key(),
            EXTERNAL_ID_KIND,
        )
        .await?;
        if let Some(prior) = &prior
            && (external_envelope(prior)?.record != *record)
        {
            return Err(SessionDbError::InvalidData(
                "conflicting external retry".into(),
            ));
        }
        Ok(prior)
    }

    /// Read the canonical latest backend binding without native/plugin availability.
    /// # Errors
    /// Returns degraded-state errors rather than rebuilding an absent/stale index.
    pub async fn external_backend_binding(&self) -> SessionDbResult<Option<SessionEvent>> {
        external_index_ready(&**self.db).await?;
        if let Some(event) = self.harness_binding_event().await? {
            return Ok(Some(event));
        }
        external_latest(
            &**self.db,
            self.session_id,
            &external_backend_key("binding", "latest"),
            EXTERNAL_INDEX_KIND,
        )
        .await
    }

    /// Incremental indexed evidence read with event-count and encoded-byte budgets.
    /// # Errors
    /// Rejects invalid bounds, oversized first records, inconsistent locators and stale indexes.
    pub async fn external_backend_page(
        &self,
        query: &ExternalBackendQuery,
    ) -> SessionDbResult<ExternalBackendPage> {
        query.validate().map_err(SessionDbError::InvalidData)?;
        external_index_ready(&**self.db).await?;
        let key = query.correlation_key.as_deref().unwrap_or(EXTERNAL_ALL_KEY);
        let mut select = self
            .db
            .select("turn_evidence")
            .columns(&["event_seq"])
            .where_eq("turn_id", key.to_owned())
            .where_eq("evidence_kind", EXTERNAL_INDEX_KIND)
            .sort("event_seq", SortDirection::Asc)
            .limit(query.limit + 1);
        if let Some(after) = query.after_sequence {
            select = select.where_gt("event_seq", seq_to_value(after));
        }
        let rows = select.execute(&**self.db).await?;
        let mut events = Vec::new();
        let mut remaining = query.max_bytes;
        let mut has_more = rows.len() > query.limit;
        for row in rows.iter().take(query.limit) {
            let event = external_event_at(
                &**self.db,
                self.session_id,
                required_non_negative_u64(row, "event_seq")?,
            )
            .await?;
            let envelope = external_envelope(&event)?;
            if key != EXTERNAL_ALL_KEY
                && !envelope
                    .record
                    .correlation_keys()
                    .iter()
                    .any(|item| item == key)
            {
                return Err(SessionDbError::InvalidData(
                    "external page correlation mismatch".into(),
                ));
            }
            let bytes = crate::persisted::encode_session_event(&event)?.len();
            if bytes > remaining {
                if events.is_empty() {
                    return Err(SessionDbError::InvalidData(
                        "external record exceeds page budget".into(),
                    ));
                }
                has_more = true;
                break;
            }
            remaining -= bytes;
            events.push(event);
        }
        let next_after_sequence = events
            .last()
            .map(|event| event.sequence)
            .or(query.after_sequence);
        Ok(ExternalBackendPage {
            events,
            next_after_sequence,
            has_more,
        })
    }
}

async fn validate_external_receipt(
    db: &dyn Database,
    event: &SessionEvent,
    receipt: &bcode_harness_models::HarnessReceipt,
) -> SessionDbResult<()> {
    let prior = external_latest(
        db,
        event.session_id,
        &external_backend_key("submission_history", &receipt.submission_id),
        EXTERNAL_INDEX_KIND,
    )
    .await?
    .ok_or_else(|| {
        SessionDbError::InvalidData("external receipt lacks accepted submission".into())
    })?;
    let (attempt, generation, state) = match external_envelope(&prior)?.record {
        ExternalBackendRecord::Submission { submission } => {
            (submission.attempt_id, submission.generation, D::Accepted)
        }
        ExternalBackendRecord::Receipt { receipt } => {
            (receipt.attempt_id, receipt.generation, receipt.state)
        }
        _ => {
            return Err(SessionDbError::InvalidData(
                "invalid submission correlation".into(),
            ));
        }
    };
    let allowed = matches!(
        (state, receipt.state),
        (D::Accepted, D::Prepared | D::Rejected)
            | (D::Prepared, D::DeliveryAttempted | D::Rejected)
            | (
                D::DeliveryAttempted,
                D::NativeAccepted | D::Executing | D::Settled | D::Uncertain | D::Rejected
            )
            | (D::NativeAccepted, D::Executing | D::Settled | D::Uncertain)
            | (D::Executing, D::Settled | D::Uncertain)
            | (
                D::Uncertain,
                D::NativeAccepted | D::Executing | D::Settled | D::Rejected
            )
    );
    let reconciled_generation = generation == receipt.generation
        || (matches!(
            state,
            D::DeliveryAttempted | D::NativeAccepted | D::Executing | D::Uncertain
        ) && matches!(
            receipt.state,
            D::NativeAccepted | D::Executing | D::Settled | D::Rejected
        ));
    if attempt != receipt.attempt_id || !reconciled_generation || !allowed {
        return Err(SessionDbError::InvalidData(
            "external delivery transition rejected; reconciliation required".into(),
        ));
    }
    Ok(())
}
