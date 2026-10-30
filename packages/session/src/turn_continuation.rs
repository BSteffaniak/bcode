//! Guarded original-turn continuation persistence. Deliberate semantics only; no dispatcher.
use crate::{
    SessionError, SessionManager,
    db::{SessionDbError, SessionDbResult},
    db_row::{required_non_negative_u64, required_string},
};
use bcode_session_models::{
    DurableInteractionOperation, DurableInteractionTransition, DurableTurnContinuationV1,
    InteractionContinuationReceipt, MAX_TURN_CONTINUATION_BYTES, MAX_TURN_CONTINUATION_OPERATIONS,
    MAX_TURN_CONTINUATION_RETAINED_BYTES, SessionEvent, SessionEventKind, SessionId,
    SubmissionExecutionDescriptor, SubmissionId, SubmissionStatus, TURN_CONTINUATION_VERSION,
    ToolPresentationRetention, TurnContinuationKey, TurnContinuationMemberState,
    TurnContinuationOperation, TurnContinuationOperationRecord, TurnContinuationPhase,
    TurnContinuationReceipt, TurnContinuationRecord, TurnContinuationSettlement,
    TurnContinuationSourceRef, TurnContinuationTransition, persisted_session_event_kind_name,
    validate_turn_continuation_id,
};
use switchy::database::{Database, query::FilterableQuery};

#[cfg(test)]
#[path = "turn_continuation_tests.rs"]
mod tests;

fn invalid() -> SessionDbError {
    SessionDbError::InvalidData(
        "invalid, stale, unavailable or exhausted original-turn continuation".into(),
    )
}
fn key(key: &TurnContinuationKey) -> SessionDbResult<String> {
    Ok(serde_json::to_string(key)?)
}
fn op_key(key: &TurnContinuationKey, id: &str) -> SessionDbResult<String> {
    Ok(serde_json::to_string(&(key, id))?)
}
fn decode<T: serde::de::DeserializeOwned>(
    row: &switchy::database::Row,
    column: &str,
) -> SessionDbResult<T> {
    let value = row.get(column).ok_or_else(invalid)?;
    let text = value.as_str().ok_or_else(invalid)?;
    if text.len() > MAX_TURN_CONTINUATION_BYTES * 2 {
        return Err(invalid());
    }
    Ok(serde_json::from_str(text)?)
}

/// Domain-separated digest of an exact current canonical event, not caller JSON.
/// # Errors
/// Rejects oversized or unencodable sources.
pub fn turn_continuation_source_ref(
    event: &SessionEvent,
) -> SessionDbResult<TurnContinuationSourceRef> {
    use sha2::{Digest as _, Sha256};
    let bytes = crate::persisted::encode_session_event(event)?;
    if bytes.len() > MAX_TURN_CONTINUATION_BYTES {
        return Err(invalid());
    }
    let mut hash = Sha256::new();
    hash.update(b"bcode.turn-continuation.source.v1\0");
    hash.update(bytes);
    Ok(TurnContinuationSourceRef {
        sequence: event.sequence,
        kind: persisted_session_event_kind_name(&event.kind).into(),
        digest: hex::encode(hash.finalize()),
    })
}
/// Digest the complete effective descriptor, including the durable recovery recipe.
/// # Errors
/// Rejects missing/request-only recovery recipes and invalid or oversized descriptors.
pub fn turn_continuation_descriptor_digest(
    descriptor: &SubmissionExecutionDescriptor,
) -> SessionDbResult<String> {
    use sha2::{Digest as _, Sha256};
    crate::submission::validate_descriptor(Some(descriptor))?;
    if descriptor.recovery_recipe.is_none() {
        return Err(invalid());
    }
    let bytes = serde_json::to_vec(descriptor)?;
    if bytes.len() > MAX_TURN_CONTINUATION_BYTES {
        return Err(invalid());
    }
    let mut hash = Sha256::new();
    hash.update(b"bcode.turn-continuation.descriptor.v1\0");
    hash.update(bytes);
    Ok(hex::encode(hash.finalize()))
}

pub async fn exact_event(
    db: &dyn Database,
    session: SessionId,
    source: &TurnContinuationSourceRef,
) -> SessionDbResult<SessionEvent> {
    let event = crate::durable_interaction::exact_source(db, session, source.sequence).await?;
    if turn_continuation_source_ref(&event)? != *source {
        return Err(invalid());
    }
    Ok(event)
}
pub async fn checkpoint(db: &dyn Database, generation: u64) -> SessionDbResult<()> {
    let row = db
        .select("projection_checkpoints")
        .columns(&["projection_version", "last_event_seq"])
        .where_eq("projection_name", "turn_continuations")
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    if required_non_negative_u64(&row, "last_event_seq")? != generation
        || required_non_negative_u64(&row, "projection_version")? != 1
    {
        return Err(invalid());
    }
    Ok(())
}
pub async fn lookup(
    db: &dyn Database,
    id: &TurnContinuationKey,
) -> SessionDbResult<Option<TurnContinuationRecord>> {
    let row = db
        .select("turn_continuations")
        .columns(&["record"])
        .where_eq("continuation_key", key(id)?)
        .execute_first(db)
        .await?;
    row.map(|row| {
        let record: TurnContinuationRecord = decode(&row, "record")?;
        if record.capture.key != *id || record.capture.version != TURN_CONTINUATION_VERSION {
            return Err(invalid());
        }
        Ok(record)
    })
    .transpose()
}
pub async fn operation(
    db: &dyn Database,
    id: &TurnContinuationKey,
    operation_id: &str,
) -> SessionDbResult<Option<TurnContinuationOperationRecord>> {
    validate_turn_continuation_id(operation_id).map_err(|_| invalid())?;
    let row = db
        .select("turn_continuation_operations")
        .columns(&["operation", "receipt"])
        .where_eq("operation_key", op_key(id, operation_id)?)
        .execute_first(db)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let operation: TurnContinuationOperation = decode(&row, "operation")?;
    let receipt: TurnContinuationReceipt = decode(&row, "receipt")?;
    operation.validate().map_err(|_| invalid())?;
    if operation.key != *id
        || operation.operation_id != operation_id
        || receipt.key != *id
        || receipt.operation_id != operation_id
        || receipt.version != TURN_CONTINUATION_VERSION
        || receipt.revision == 0
        || receipt.revision > MAX_TURN_CONTINUATION_OPERATIONS
        || operation.expected_revision.unwrap_or(0).checked_add(1) != Some(receipt.revision)
    {
        return Err(invalid());
    }
    let event =
        crate::durable_interaction::exact_source(db, id.session_id, receipt.event_sequence).await?;
    if !matches!(&event.kind, SessionEventKind::TurnContinuationCommitted { operation: canonical } if canonical.as_ref() == &operation)
        || operation_result_ref(&operation, &event)? != receipt.result_ref
    {
        return Err(invalid());
    }
    if let Some(source) = &receipt.result_ref {
        let result = exact_event(db, id.session_id, source).await?;
        if let Some(expected) = result_event(&operation, &event)? {
            if result != expected {
                return Err(invalid());
            }
        } else {
            validate_settled_result(&operation, &result)?;
        }
    }
    Ok(Some(TurnContinuationOperationRecord { operation, receipt }))
}

/// Idempotency precedes revision/phase admission, including after retirement.
pub async fn retry(
    db: &dyn Database,
    requested: &TurnContinuationOperation,
) -> SessionDbResult<Option<TurnContinuationReceipt>> {
    requested.validate().map_err(|_| invalid())?;
    match operation(db, &requested.key, &requested.operation_id).await? {
        Some(original) if original.operation == *requested => Ok(Some(original.receipt)),
        Some(_) => Err(invalid()),
        None => Ok(None),
    }
}

fn validate_settled_result(
    operation: &TurnContinuationOperation,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let TurnContinuationTransition::AttachSelectedResult { invocation_id, .. } =
        &operation.transition
    else {
        return Err(invalid());
    };
    if !matches!(&event.kind, SessionEventKind::ToolInvocationResultRecorded { record } if record.invocation_id == *invocation_id)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Verify exact canonical prepare/latest receipts and every retained result without replay.
pub async fn verified_lookup(
    db: &dyn Database,
    id: &TurnContinuationKey,
) -> SessionDbResult<Option<TurnContinuationRecord>> {
    let Some(record) = lookup(db, id).await? else {
        return Ok(None);
    };
    if record.members.len() != record.capture.members.len()
        || record.member_operation_ids.len() != record.members.len()
        || record.retained_operation_bytes > MAX_TURN_CONTINUATION_RETAINED_BYTES
    {
        return Err(invalid());
    }
    let prepare = operation(db, id, &record.prepare_operation_id)
        .await?
        .ok_or_else(invalid)?;
    if prepare.receipt.revision != 1
        || !matches!(&prepare.operation.transition, TurnContinuationTransition::Prepare { capture } if capture.as_ref() == &record.capture)
    {
        return Err(invalid());
    }
    let latest = operation(db, id, &record.receipt.operation_id)
        .await?
        .ok_or_else(invalid)?;
    if latest.receipt != record.receipt {
        return Err(invalid());
    }
    let expected_phase = match &latest.operation.transition {
        TurnContinuationTransition::ReadyForModel => TurnContinuationPhase::ReadyForModel,
        TurnContinuationTransition::StartModel { dispatch_id } => {
            TurnContinuationPhase::ModelDispatchStarted {
                dispatch_id: dispatch_id.clone(),
            }
        }
        TurnContinuationTransition::Retire { successor_ref } => {
            let successor = exact_event(db, id.session_id, successor_ref).await?;
            if !matches!(&successor.kind, SessionEventKind::ModelTurnFinished { turn_id, .. } if turn_id == &id.turn_id)
                || successor_ref.sequence >= latest.receipt.event_sequence
            {
                return Err(invalid());
            }
            TurnContinuationPhase::Retired {
                successor_ref: successor_ref.clone(),
            }
        }
        _ => TurnContinuationPhase::AwaitingBatchSettlement,
    };
    if record.phase != expected_phase {
        return Err(invalid());
    }
    validate_execution_sources(db, &record.capture).await?;
    verify_members(db, id, &record).await?;
    Ok(Some(record))
}

async fn verify_members(
    db: &dyn Database,
    id: &TurnContinuationKey,
    record: &TurnContinuationRecord,
) -> SessionDbResult<()> {
    for (index, (member, state)) in record
        .capture
        .members
        .iter()
        .zip(&record.members)
        .enumerate()
    {
        let member_op = operation(db, id, &record.member_operation_ids[index])
            .await?
            .ok_or_else(invalid)?;
        let expected_state = match &member_op.operation.transition {
            TurnContinuationTransition::Prepare { .. } if member_op.receipt.revision == 1 => {
                TurnContinuationMemberState::Pending
            }
            TurnContinuationTransition::StartMember {
                invocation_id,
                dispatch_id,
            } if invocation_id == &member.invocation_id => TurnContinuationMemberState::Started {
                dispatch_id: dispatch_id.clone(),
            },
            TurnContinuationTransition::SettleMember { invocation_id, .. }
            | TurnContinuationTransition::SettleWithoutDispatch { invocation_id, .. }
            | TurnContinuationTransition::AttachSelectedResult { invocation_id, .. }
                if invocation_id == &member.invocation_id =>
            {
                TurnContinuationMemberState::Settled {
                    result_ref: member_op.receipt.result_ref.clone().ok_or_else(invalid)?,
                }
            }
            _ => return Err(invalid()),
        };
        if expected_state != *state || member_op.receipt.revision > record.receipt.revision {
            return Err(invalid());
        }
        if matches!(
            member_op.operation.transition,
            TurnContinuationTransition::AttachSelectedResult { .. }
        ) {
            validate_settlement(
                db,
                id.session_id,
                &member_op.operation,
                &record.capture,
                member_op.receipt.event_sequence,
            )
            .await?;
        }
        let request = exact_event(db, id.session_id, &member.request).await?;
        if !matches!(&request.kind, SessionEventKind::PositionedToolCallRequested { turn_id, tool_call_id, .. } if turn_id == &id.turn_id && tool_call_id == &member.invocation_id)
        {
            return Err(invalid());
        }
        let row = db
            .select("turn_continuation_members")
            .columns(&["continuation_key"])
            .where_eq("invocation_id", member.invocation_id.clone())
            .execute_first(db)
            .await?
            .ok_or_else(invalid)?;
        if decode::<TurnContinuationKey>(&row, "continuation_key")? != *id {
            return Err(invalid());
        }
        if let TurnContinuationMemberState::Settled { result_ref } = state {
            let event = exact_event(db, id.session_id, result_ref).await?;
            if !matches!(&event.kind, SessionEventKind::ToolInvocationResultRecorded { record } if record.invocation_id == member.invocation_id)
                || result_ref.sequence < member.request.sequence
            {
                return Err(invalid());
            }
        } else if record.phase != TurnContinuationPhase::AwaitingBatchSettlement {
            return Err(invalid());
        }
    }
    Ok(())
}

fn initial_record(
    previous: Option<TurnContinuationRecord>,
    operation: &TurnContinuationOperation,
    sequence: u64,
) -> SessionDbResult<TurnContinuationRecord> {
    Ok(if let Some(record) = previous {
        if record.capture.key != operation.key
            || operation.expected_revision != Some(record.receipt.revision)
            || record.receipt.event_sequence >= sequence
        {
            return Err(invalid());
        }
        record
    } else {
        let TurnContinuationTransition::Prepare { capture } = &operation.transition else {
            return Err(invalid());
        };
        if operation.expected_revision.is_some() {
            return Err(invalid());
        }
        TurnContinuationRecord {
            capture: *capture.clone(),
            members: vec![TurnContinuationMemberState::Pending; capture.members.len()],
            member_operation_ids: vec![operation.operation_id.clone(); capture.members.len()],
            phase: TurnContinuationPhase::AwaitingBatchSettlement,
            receipt: TurnContinuationReceipt {
                version: TURN_CONTINUATION_VERSION,
                key: operation.key.clone(),
                operation_id: String::new(),
                revision: 0,
                event_sequence: 0,
                result_ref: None,
            },
            prepare_operation_id: operation.operation_id.clone(),
            retained_operation_bytes: 0,
        }
    })
}

pub fn advance(
    previous: Option<TurnContinuationRecord>,
    operation: &TurnContinuationOperation,
    sequence: u64,
    result_ref: Option<TurnContinuationSourceRef>,
) -> SessionDbResult<TurnContinuationRecord> {
    operation.validate().map_err(|_| invalid())?;
    match &operation.transition {
        TurnContinuationTransition::SettleMember { .. }
        | TurnContinuationTransition::SettleWithoutDispatch { .. } => {
            let source = result_ref.as_ref().ok_or_else(invalid)?;
            if source.kind != "tool_invocation_result_recorded"
                || sequence.checked_add(1) != Some(source.sequence)
            {
                return Err(invalid());
            }
        }
        TurnContinuationTransition::AttachSelectedResult {
            result_ref: expected,
            ..
        } => {
            if result_ref.as_ref() != Some(expected)
                || expected.kind != "tool_invocation_result_recorded"
                || expected.sequence.checked_add(1) != Some(sequence)
            {
                return Err(invalid());
            }
        }
        _ if result_ref.is_some() => return Err(invalid()),
        _ => {}
    }
    let bytes = u64::try_from(serde_json::to_vec(operation)?.len()).map_err(|_| invalid())?;
    let mut record = initial_record(previous, operation, sequence)?;
    if record.members.len() != record.capture.members.len()
        || record.member_operation_ids.len() != record.members.len()
        || matches!(record.phase, TurnContinuationPhase::Retired { .. })
    {
        return Err(invalid());
    }
    let revision = record
        .receipt
        .revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_TURN_CONTINUATION_OPERATIONS)
        .ok_or_else(invalid)?;
    let receipt_bytes = u64::try_from(
        serde_json::to_vec(&TurnContinuationReceipt {
            version: TURN_CONTINUATION_VERSION,
            key: operation.key.clone(),
            operation_id: operation.operation_id.clone(),
            revision,
            event_sequence: sequence,
            result_ref: result_ref.clone(),
        })?
        .len(),
    )
    .map_err(|_| invalid())?;
    let retained = record
        .retained_operation_bytes
        .checked_add(bytes)
        .and_then(|bytes| bytes.checked_add(receipt_bytes))
        .filter(|bytes| *bytes <= MAX_TURN_CONTINUATION_RETAINED_BYTES)
        .ok_or_else(invalid)?;
    apply_transition(&mut record, &operation.transition, result_ref.clone())?;
    let invocation = match &operation.transition {
        TurnContinuationTransition::StartMember { invocation_id, .. }
        | TurnContinuationTransition::SettleMember { invocation_id, .. }
        | TurnContinuationTransition::SettleWithoutDispatch { invocation_id, .. }
        | TurnContinuationTransition::AttachSelectedResult { invocation_id, .. } => {
            Some(invocation_id)
        }
        _ => None,
    };
    if let Some(invocation) = invocation {
        let index = record
            .capture
            .members
            .iter()
            .position(|member| &member.invocation_id == invocation)
            .ok_or_else(invalid)?;
        record
            .member_operation_ids
            .get_mut(index)
            .ok_or_else(invalid)?
            .clone_from(&operation.operation_id);
    }
    record.receipt = TurnContinuationReceipt {
        version: TURN_CONTINUATION_VERSION,
        key: operation.key.clone(),
        operation_id: operation.operation_id.clone(),
        revision,
        event_sequence: sequence,
        result_ref,
    };
    record.retained_operation_bytes = retained;
    Ok(record)
}
fn member_mut<'a>(
    record: &'a mut TurnContinuationRecord,
    invocation: &str,
) -> SessionDbResult<&'a mut TurnContinuationMemberState> {
    if record.phase != TurnContinuationPhase::AwaitingBatchSettlement {
        return Err(invalid());
    }
    let index = record
        .capture
        .members
        .iter()
        .position(|member| member.invocation_id == invocation)
        .ok_or_else(invalid)?;
    record.members.get_mut(index).ok_or_else(invalid)
}
fn apply_transition(
    record: &mut TurnContinuationRecord,
    transition: &TurnContinuationTransition,
    result_ref: Option<TurnContinuationSourceRef>,
) -> SessionDbResult<()> {
    match transition {
        TurnContinuationTransition::Prepare { .. } if record.receipt.revision != 0 => {
            return Err(invalid());
        }
        TurnContinuationTransition::Prepare { .. } => {}
        TurnContinuationTransition::StartMember {
            invocation_id,
            dispatch_id,
        } => {
            validate_turn_continuation_id(dispatch_id).map_err(|_| invalid())?;
            let member = member_mut(record, invocation_id)?;
            if *member != TurnContinuationMemberState::Pending {
                return Err(invalid());
            }
            *member = TurnContinuationMemberState::Started {
                dispatch_id: dispatch_id.clone(),
            };
        }
        TurnContinuationTransition::SettleMember {
            invocation_id,
            dispatch_id,
            ..
        } => {
            if invocation_id == &record.capture.selected_invocation_id {
                return Err(invalid());
            }
            let member = member_mut(record, invocation_id)?;
            if !matches!(member, TurnContinuationMemberState::Started { dispatch_id: started } if started == dispatch_id)
            {
                return Err(invalid());
            }
            *member = TurnContinuationMemberState::Settled {
                result_ref: result_ref.ok_or_else(invalid)?,
            };
        }
        TurnContinuationTransition::SettleWithoutDispatch {
            invocation_id,
            result,
        } => {
            if !result.is_error || invocation_id == &record.capture.selected_invocation_id {
                return Err(invalid());
            }
            let member = member_mut(record, invocation_id)?;
            if *member != TurnContinuationMemberState::Pending {
                return Err(invalid());
            }
            *member = TurnContinuationMemberState::Settled {
                result_ref: result_ref.ok_or_else(invalid)?,
            };
        }
        TurnContinuationTransition::AttachSelectedResult {
            invocation_id,
            result_ref,
        } => {
            if invocation_id != &record.capture.selected_invocation_id {
                return Err(invalid());
            }
            let member = member_mut(record, invocation_id)?;
            if !matches!(member, TurnContinuationMemberState::Started { .. }) {
                return Err(invalid());
            }
            *member = TurnContinuationMemberState::Settled {
                result_ref: result_ref.clone(),
            };
        }
        TurnContinuationTransition::ReadyForModel => {
            if record.phase != TurnContinuationPhase::AwaitingBatchSettlement
                || !record
                    .members
                    .iter()
                    .all(|member| matches!(member, TurnContinuationMemberState::Settled { .. }))
            {
                return Err(invalid());
            }
            record.phase = TurnContinuationPhase::ReadyForModel;
        }
        TurnContinuationTransition::StartModel { dispatch_id } => {
            validate_turn_continuation_id(dispatch_id).map_err(|_| invalid())?;
            if record.phase != TurnContinuationPhase::ReadyForModel {
                return Err(invalid());
            }
            record.phase = TurnContinuationPhase::ModelDispatchStarted {
                dispatch_id: dispatch_id.clone(),
            };
        }
        TurnContinuationTransition::Retire { successor_ref } => {
            if !matches!(
                record.phase,
                TurnContinuationPhase::ModelDispatchStarted { .. }
            ) {
                return Err(invalid());
            }
            record.phase = TurnContinuationPhase::Retired {
                successor_ref: successor_ref.clone(),
            };
        }
    }
    Ok(())
}

async fn validate_capture(
    db: &dyn Database,
    capture: &DurableTurnContinuationV1,
) -> SessionDbResult<()> {
    let session = capture.key.session_id;
    let accepted = exact_event(db, session, &capture.accepted).await?;
    let activated = exact_event(db, session, &capture.activated).await?;
    let SessionEventKind::SubmissionAccepted { submission } = &accepted.kind else {
        return Err(invalid());
    };
    let SessionEventKind::SubmissionActivated {
        submission_id,
        turn_id,
        ..
    } = &activated.kind
    else {
        return Err(invalid());
    };
    if submission.id.0 != capture.submission_id
        || *submission_id != submission.id
        || turn_id.to_string() != capture.key.turn_id
        || capture.accepted.sequence >= capture.activated.sequence
    {
        return Err(invalid());
    }
    let current = crate::submission::lookup(db, submission_id)
        .await?
        .ok_or_else(invalid)?;
    let descriptor = current
        .effective_execution_descriptor()
        .ok_or_else(invalid)?;
    if turn_continuation_descriptor_digest(descriptor)? != capture.descriptor_digest {
        return Err(invalid());
    }
    let source = exact_event(db, session, &capture.descriptor_source).await?;
    let source_descriptor = match &source.kind {
        SessionEventKind::SubmissionAccepted {
            submission: original,
        } if original.id == submission.id => original.execution_descriptor.as_ref(),
        SessionEventKind::SubmissionRecoveryAuthorized {
            submission_id: original,
            descriptor,
            ..
        } if original == submission_id => Some(descriptor),
        _ => None,
    }
    .ok_or_else(invalid)?;
    if source_descriptor != descriptor {
        return Err(invalid());
    }
    if !matches!(&current.status, SubmissionStatus::Activated { turn_id: active, conversation_sequence } if active.to_string() == capture.key.turn_id && *conversation_sequence == capture.activated.sequence)
        || capture.model_work_id != format!("model_{}", capture.key.turn_id)
    {
        return Err(invalid());
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut previous_position = None;
    for member in &capture.members {
        if !ids.insert(&member.invocation_id) {
            return Err(invalid());
        }
        let event = exact_event(db, session, &member.request).await?;
        let SessionEventKind::PositionedToolCallRequested {
            turn_id,
            tool_call_id,
            output_position,
            producer_plugin_id,
            ..
        } = &event.kind
        else {
            return Err(invalid());
        };
        if turn_id != &capture.key.turn_id
            || tool_call_id != &member.invocation_id
            || (tool_call_id == &capture.selected_invocation_id && producer_plugin_id.is_none())
            || event.sequence <= capture.activated.sequence
            || previous_position.is_some_and(|position| position >= output_position.get())
        {
            return Err(invalid());
        }
        previous_position = Some(output_position.get());
        let run = db
            .select("tool_runs")
            .columns(&["event_seq_start", "event_seq_end", "status"])
            .where_eq("tool_call_id", member.invocation_id.clone())
            .execute_first(db)
            .await?
            .ok_or_else(invalid)?;
        if required_non_negative_u64(&run, "event_seq_start")? != event.sequence
            || crate::db_row::optional_i64(&run, "event_seq_end").is_some()
            || required_string(&run, "status")? != "running"
        {
            return Err(invalid());
        }
    }
    if !ids.contains(&capture.selected_invocation_id) {
        return Err(invalid());
    }
    // The selected exchange identity is reserved in this immutable capture before the
    // plugin is dispatched. Its request does not exist yet; Complete later proves the
    // exact canonical exchange and producer binding in the same transaction.
    validate_turn_continuation_id(&capture.selected_exchange_id).map_err(|_| invalid())?;
    Ok(())
}

/// Whole-transaction proof for bound selected results. Called inside the storage write
/// transaction, before any projection or canonical insertion.
pub async fn validate_atomic_attachments(
    db: &dyn Database,
    events: &[(SessionEvent, Option<u64>)],
) -> SessionDbResult<()> {
    if !db.table_exists("turn_continuation_members").await? {
        return Ok(());
    }
    for (index, (event, _)) in events.iter().enumerate() {
        match &event.kind {
            SessionEventKind::DurableInteractionCommitted {
                operation: interaction,
            } if matches!(
                interaction.transition,
                DurableInteractionTransition::Complete { .. }
            ) =>
            {
                let row = db
                    .select("turn_continuation_members")
                    .columns(&["continuation_key"])
                    .where_eq("invocation_id", interaction.key.invocation_id.clone())
                    .execute_first(db)
                    .await?;
                let Some(row) = row else { continue };
                let id: TurnContinuationKey = decode(&row, "continuation_key")?;
                let record = lookup(db, &id).await?.ok_or_else(invalid)?;
                if record.capture.selected_invocation_id != interaction.key.invocation_id {
                    return Err(invalid());
                }
                let batch = events
                    .get(index..index.checked_add(4).ok_or_else(invalid)?)
                    .ok_or_else(invalid)?;
                crate::db::validate_durable_interaction_batch(&batch[..3])?;
                let SessionEventKind::TurnContinuationCommitted { operation } = &batch[3].0.kind
                else {
                    return Err(invalid());
                };
                let TurnContinuationTransition::AttachSelectedResult {
                    invocation_id,
                    result_ref,
                } = &operation.transition
                else {
                    return Err(invalid());
                };
                if operation.key != id
                    || invocation_id != &interaction.key.invocation_id
                    || batch[3].0.session_id != event.session_id
                    || batch[3].0.timestamp_ms != event.timestamp_ms
                    || batch[3].0.provenance != event.provenance
                    || batch[2].0.sequence.checked_add(1) != Some(batch[3].0.sequence)
                    || *result_ref != turn_continuation_source_ref(&batch[2].0)?
                {
                    return Err(invalid());
                }
            }
            SessionEventKind::TurnContinuationCommitted { operation }
                if matches!(
                    operation.transition,
                    TurnContinuationTransition::AttachSelectedResult { .. }
                ) =>
            {
                let preceding = index.checked_sub(3).ok_or_else(invalid)?;
                if !matches!(&events[preceding].0.kind, SessionEventKind::DurableInteractionCommitted { operation: interaction } if matches!(interaction.transition, DurableInteractionTransition::Complete { .. }))
                {
                    return Err(invalid());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub async fn validate_member_result(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let SessionEventKind::ToolInvocationResultRecorded { record } = &event.kind else {
        return Ok(());
    };
    if !db.table_exists("turn_continuation_members").await? {
        return Ok(());
    }
    let row = db
        .select("turn_continuation_members")
        .columns(&["continuation_key"])
        .where_eq("invocation_id", record.invocation_id.clone())
        .execute_first(db)
        .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let key: TurnContinuationKey = decode(&row, "continuation_key")?;
    let current = lookup(db, &key).await?.ok_or_else(invalid)?;
    if record.invocation_id == current.capture.selected_invocation_id {
        let index = current
            .capture
            .members
            .iter()
            .position(|member| member.invocation_id == record.invocation_id)
            .ok_or_else(invalid)?;
        if current.phase != TurnContinuationPhase::AwaitingBatchSettlement
            || !matches!(
                current.members.get(index),
                Some(TurnContinuationMemberState::Started { .. })
            )
        {
            return Err(invalid());
        }
        let original = crate::durable_interaction::exact_source(
            db,
            event.session_id,
            event.sequence.checked_sub(2).ok_or_else(invalid)?,
        )
        .await?;
        if !matches!(&original.kind, SessionEventKind::DurableInteractionCommitted { operation } if operation.key.invocation_id == record.invocation_id && matches!(&operation.transition, DurableInteractionTransition::Complete { result, .. } if result.as_ref() == record))
        {
            return Err(invalid());
        }
    } else {
        let previous = crate::durable_interaction::exact_source(
            db,
            event.session_id,
            event.sequence.checked_sub(1).ok_or_else(invalid)?,
        )
        .await?;
        if !matches!(&previous.kind, SessionEventKind::TurnContinuationCommitted { operation } if operation.key == key && result_event(operation, &previous)?.as_ref() == Some(event))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub async fn validate_append(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    let SessionEventKind::TurnContinuationCommitted { operation } = &event.kind else {
        return Ok(());
    };
    crate::durable_interaction::ensure_publication_enabled()?;
    validate_operation(db, event.session_id, operation, event.sequence).await
}
pub async fn validate_operation(
    db: &dyn Database,
    session: SessionId,
    operation: &TurnContinuationOperation,
    sequence: u64,
) -> SessionDbResult<()> {
    operation.validate().map_err(|_| invalid())?;
    if operation.key.session_id != session {
        return Err(invalid());
    }
    checkpoint(db, sequence.checked_sub(1).ok_or_else(invalid)?).await?;
    let previous = verified_lookup(db, &operation.key).await?;
    let capture = match (&operation.transition, &previous) {
        (TurnContinuationTransition::Prepare { capture }, None) => capture.as_ref(),
        (_, Some(record)) => &record.capture,
        _ => return Err(invalid()),
    };
    // Positive ownership, cancellation and workflow fencing remain mandatory in the held host guard.
    if !matches!(
        operation.transition,
        TurnContinuationTransition::Retire { .. }
    ) && db
        .select("turn_evidence")
        .columns(&["event_seq"])
        .where_eq("turn_id", operation.key.turn_id.clone())
        .where_eq("evidence_kind", "terminal")
        .limit(1)
        .execute_first(db)
        .await?
        .is_some()
    {
        return Err(invalid());
    }
    if matches!(
        operation.transition,
        TurnContinuationTransition::Prepare { .. }
    ) {
        validate_capture(db, capture).await?;
    }
    validate_execution_sources(db, capture).await?;
    // Dispatch markers, like settlements, require an exact still-running invocation.
    if let TurnContinuationTransition::StartMember { invocation_id, .. } = &operation.transition {
        let member = capture
            .members
            .iter()
            .find(|member| member.invocation_id == *invocation_id)
            .ok_or_else(invalid)?;
        let request = exact_event(db, session, &member.request).await?;
        if request.sequence >= sequence {
            return Err(invalid());
        }
        let run = db
            .select("tool_runs")
            .columns(&["status", "event_seq_start", "event_seq_end"])
            .where_eq("tool_call_id", invocation_id.clone())
            .execute_first(db)
            .await?
            .ok_or_else(invalid)?;
        if required_string(&run, "status")? != "running"
            || required_non_negative_u64(&run, "event_seq_start")? != request.sequence
            || crate::db_row::optional_i64(&run, "event_seq_end").is_some()
        {
            return Err(invalid());
        }
    }
    validate_settlement(db, session, operation, capture, sequence).await?;
    if let TurnContinuationTransition::Retire { successor_ref } = &operation.transition {
        let successor = exact_event(db, session, successor_ref).await?;
        if successor_ref.sequence >= sequence
            || !matches!(&successor.kind, SessionEventKind::ModelTurnFinished { turn_id, .. } if turn_id == &operation.key.turn_id)
        {
            return Err(invalid());
        }
    }
    if let Some(record) = &previous
        && record.members.len() != record.capture.members.len()
    {
        return Err(invalid());
    }
    for member in &capture.members {
        if let Some(record) = &previous {
            let index = capture
                .members
                .iter()
                .position(|entry| entry.invocation_id == member.invocation_id)
                .ok_or_else(invalid)?;
            if let TurnContinuationMemberState::Settled { result_ref } = &record.members[index] {
                let result = exact_event(db, session, result_ref).await?;
                if !matches!(result.kind, SessionEventKind::ToolInvocationResultRecorded { record } if record.invocation_id == member.invocation_id)
                {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}

pub fn operation_result_ref(
    operation: &TurnContinuationOperation,
    event: &SessionEvent,
) -> SessionDbResult<Option<TurnContinuationSourceRef>> {
    if let TurnContinuationTransition::AttachSelectedResult { result_ref, .. } =
        &operation.transition
    {
        return Ok(Some(result_ref.clone()));
    }
    result_event(operation, event)?
        .as_ref()
        .map(turn_continuation_source_ref)
        .transpose()
}

async fn validate_execution_sources(
    db: &dyn Database,
    capture: &DurableTurnContinuationV1,
) -> SessionDbResult<()> {
    let session = capture.key.session_id;
    let accepted = exact_event(db, session, &capture.accepted).await?;
    let activated = exact_event(db, session, &capture.activated).await?;
    let original = exact_event(db, session, &capture.descriptor_source).await?;
    let id = SubmissionId(capture.submission_id.clone());
    let current = crate::submission::lookup(db, &id)
        .await?
        .ok_or_else(invalid)?;
    let descriptor = current
        .effective_execution_descriptor()
        .ok_or_else(invalid)?;
    let source_descriptor = match &original.kind {
        SessionEventKind::SubmissionAccepted { submission } if submission.id == id => {
            submission.execution_descriptor.as_ref()
        }
        SessionEventKind::SubmissionRecoveryAuthorized {
            submission_id,
            descriptor,
            ..
        } if submission_id == &id => Some(descriptor),
        _ => None,
    }
    .ok_or_else(invalid)?;
    if !matches!(&accepted.kind, SessionEventKind::SubmissionAccepted { submission } if submission.id == id && submission == &current.submission)
        || !matches!(&activated.kind, SessionEventKind::SubmissionActivated { submission_id, turn_id, .. } if submission_id == &id && turn_id.to_string() == capture.key.turn_id)
        || !matches!(&current.status, SubmissionStatus::Activated { turn_id, conversation_sequence } if turn_id.to_string() == capture.key.turn_id && *conversation_sequence == capture.activated.sequence)
        || capture.accepted.sequence >= capture.activated.sequence
        || capture.descriptor_source.sequence >= capture.activated.sequence
        || current.receipt.accepted_event_sequence != capture.accepted.sequence
        || capture.model_work_id != format!("model_{}", capture.key.turn_id)
        || source_descriptor != descriptor
        || turn_continuation_descriptor_digest(descriptor)? != capture.descriptor_digest
    {
        return Err(invalid());
    }
    Ok(())
}

async fn validate_settlement(
    db: &dyn Database,
    session: SessionId,
    operation: &TurnContinuationOperation,
    capture: &DurableTurnContinuationV1,
    sequence: u64,
) -> SessionDbResult<()> {
    match &operation.transition {
        TurnContinuationTransition::SettleMember { invocation_id, .. }
        | TurnContinuationTransition::SettleWithoutDispatch { invocation_id, .. } => {
            if invocation_id == &capture.selected_invocation_id {
                return Err(invalid());
            }
            let run = db
                .select("tool_runs")
                .columns(&["status", "event_seq_end"])
                .where_eq("tool_call_id", invocation_id.clone())
                .execute_first(db)
                .await?
                .ok_or_else(invalid)?;
            if required_string(&run, "status")? != "running"
                || crate::db_row::optional_i64(&run, "event_seq_end").is_some()
            {
                return Err(invalid());
            }
        }
        TurnContinuationTransition::AttachSelectedResult {
            invocation_id,
            result_ref,
        } => {
            if invocation_id != &capture.selected_invocation_id
                || result_ref.sequence.checked_add(1) != Some(sequence)
            {
                return Err(invalid());
            }
            let result = exact_event(db, session, result_ref).await?;
            validate_settled_result(operation, &result)?;
            let interaction_event = crate::durable_interaction::exact_source(
                db,
                session,
                sequence.checked_sub(3).ok_or_else(invalid)?,
            )
            .await?;
            let resolution = crate::durable_interaction::exact_source(
                db,
                session,
                sequence.checked_sub(2).ok_or_else(invalid)?,
            )
            .await?;
            let SessionEventKind::DurableInteractionCommitted {
                operation: interaction,
            } = &interaction_event.kind
            else {
                return Err(invalid());
            };
            let DurableInteractionTransition::Complete {
                result: expected,
                exchange_resolution,
                ..
            } = &interaction.transition
            else {
                return Err(invalid());
            };
            let member = capture
                .members
                .iter()
                .find(|member| member.invocation_id == *invocation_id)
                .ok_or_else(invalid)?;
            let request = exact_event(db, session, &member.request).await?;
            let exchange = crate::durable_interaction::exact_source(
                db,
                session,
                interaction.exchange_request_event_sequence,
            )
            .await?;
            if interaction.key.invocation_id != *invocation_id
                || interaction.key.turn_id != capture.key.turn_id
                || interaction.request_event_sequence != member.request.sequence
                || interaction.exchange_request_event_sequence >= interaction_event.sequence
                || exchange_resolution.exchange_id != capture.selected_exchange_id
                || !matches!(&request.kind, SessionEventKind::PositionedToolCallRequested { producer_plugin_id: Some(producer), .. } if producer == &interaction.producer_plugin_id)
                || !matches!(&exchange.kind, SessionEventKind::ToolExchangeRequested { request } if request.invocation_id == *invocation_id && request.exchange_id == capture.selected_exchange_id && request.producer_id == interaction.producer_plugin_id)
                || !matches!(&resolution.kind, SessionEventKind::ToolExchangeResolved { event } if event == exchange_resolution)
                || !matches!(&result.kind, SessionEventKind::ToolInvocationResultRecorded { record } if expected.as_ref() == record)
                || result.timestamp_ms != interaction_event.timestamp_ms
                || resolution.timestamp_ms != interaction_event.timestamp_ms
                || result.provenance != interaction_event.provenance
                || resolution.provenance != interaction_event.provenance
            {
                return Err(invalid());
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn result_event(
    operation: &TurnContinuationOperation,
    event: &SessionEvent,
) -> SessionDbResult<Option<SessionEvent>> {
    let result = match &operation.transition {
        TurnContinuationTransition::SettleMember {
            invocation_id,
            result,
            ..
        }
        | TurnContinuationTransition::SettleWithoutDispatch {
            invocation_id,
            result,
        } => {
            if result.invocation_id != *invocation_id
                || result.presentation.as_ref().is_some_and(|presentation| {
                    presentation.retention == ToolPresentationRetention::ActiveOnly
                })
            {
                return Err(invalid());
            }
            result.as_ref().clone()
        }
        _ => return Ok(None),
    };
    Ok(Some(SessionEvent {
        sequence: event.sequence.checked_add(1).ok_or_else(invalid)?,
        kind: SessionEventKind::ToolInvocationResultRecorded { record: result },
        ..event.clone()
    }))
}

pub async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    if let SessionEventKind::TurnContinuationCommitted { operation } = &event.kind {
        if self::operation(db, &operation.key, &operation.operation_id)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
        let result_ref = operation_result_ref(operation, event)?;
        let record = advance(
            lookup(db, &operation.key).await?,
            operation,
            event.sequence,
            result_ref,
        )?;
        if matches!(
            operation.transition,
            TurnContinuationTransition::Prepare { .. }
        ) {
            for member in &record.capture.members {
                if db
                    .select("turn_continuation_members")
                    .columns(&["continuation_key"])
                    .where_eq("invocation_id", member.invocation_id.clone())
                    .execute_first(db)
                    .await?
                    .is_some()
                {
                    return Err(invalid());
                }
                db.insert("turn_continuation_members")
                    .value("invocation_id", member.invocation_id.clone())
                    .value("continuation_key", key(&operation.key)?)
                    .execute(db)
                    .await?;
            }
        }
        db.upsert("turn_continuations")
            .unique(&["continuation_key"])
            .value("continuation_key", key(&operation.key)?)
            .value("record", serde_json::to_string(&record)?)
            .execute(db)
            .await?;
        db.insert("turn_continuation_operations")
            .value(
                "operation_key",
                op_key(&operation.key, &operation.operation_id)?,
            )
            .value("operation", serde_json::to_string(operation)?)
            .value("receipt", serde_json::to_string(&record.receipt)?)
            .execute(db)
            .await?;
    }
    if crate::durable_interaction::PUBLICATION_ENABLED
        || matches!(
            event.kind,
            SessionEventKind::TurnContinuationCommitted { .. }
        )
    {
        db.upsert("projection_checkpoints")
            .unique(&["projection_name"])
            .value("projection_name", "turn_continuations")
            .value("projection_version", 1_i64)
            .value(
                "last_event_seq",
                crate::db_event_store::seq_to_value(event.sequence),
            )
            .value(
                "updated_at_ms",
                crate::db_event_store::seq_to_value(event.timestamp_ms),
            )
            .execute(db)
            .await?;
    }
    Ok(())
}

impl SessionManager {
    /// Atomically complete the selected pure interaction and attach its exact canonical result.
    /// # Errors
    /// Rejects partial/conflicting retries, stale revisions, source mismatches and lost authority.
    pub async fn complete_interaction_with_continuation(
        &self,
        interaction_operation: DurableInteractionOperation,
        continuation_settlement: TurnContinuationSettlement,
        guard: std::sync::Arc<dyn crate::DurableInteractionCommitGuard>,
    ) -> Result<InteractionContinuationReceipt, SessionError> {
        guard.validate()?;
        crate::durable_interaction::ensure_publication_enabled()?;
        self.session_handle(continuation_settlement.key.session_id)
            .await?
            .complete_interaction_with_continuation(
                interaction_operation,
                continuation_settlement,
                guard,
                self.next_activity_timestamp_ms(),
                self.mutation_tx.clone(),
            )
            .await
    }

    /// Capture complete original-turn batch semantics before dispatch or recoverable readiness.
    /// # Errors
    /// Rejects missing canonical activation/recipe, lost authority, invalid sources or admission bounds.
    pub async fn prepare_turn_continuation(
        &self,
        capture: DurableTurnContinuationV1,
        operation_id: String,
        request_digest: String,
        guard: std::sync::Arc<dyn crate::DurableInteractionCommitGuard>,
    ) -> Result<TurnContinuationReceipt, SessionError> {
        let operation = TurnContinuationOperation {
            version: TURN_CONTINUATION_VERSION,
            key: capture.key.clone(),
            operation_id,
            request_digest,
            expected_revision: None,
            transition: TurnContinuationTransition::Prepare {
                capture: Box::new(capture),
            },
        };
        self.commit_turn_continuation(operation, guard).await
    }
    /// CAS an original-turn member or model continuation boundary; never dispatches work.
    /// # Errors
    /// Rejects stale revisions, conflicting retry identities, lost authority and ambiguous/invalid transitions.
    pub async fn transition_turn_continuation(
        &self,
        key: TurnContinuationKey,
        expected_revision: u64,
        operation_id: String,
        request_digest: String,
        transition: TurnContinuationTransition,
        guard: std::sync::Arc<dyn crate::DurableInteractionCommitGuard>,
    ) -> Result<TurnContinuationReceipt, SessionError> {
        if matches!(transition, TurnContinuationTransition::Prepare { .. }) {
            return Err(invalid().into());
        }
        self.commit_turn_continuation(
            TurnContinuationOperation {
                version: TURN_CONTINUATION_VERSION,
                key,
                operation_id,
                request_digest,
                expected_revision: Some(expected_revision),
                transition,
            },
            guard,
        )
        .await
    }
    async fn commit_turn_continuation(
        &self,
        operation: TurnContinuationOperation,
        guard: std::sync::Arc<dyn crate::DurableInteractionCommitGuard>,
    ) -> Result<TurnContinuationReceipt, SessionError> {
        guard.validate()?;
        crate::durable_interaction::ensure_publication_enabled()?;
        operation.validate().map_err(|_| invalid())?;
        self.session_handle(operation.key.session_id)
            .await?
            .commit_turn_continuation(
                operation,
                guard,
                self.next_activity_timestamp_ms(),
                self.mutation_tx.clone(),
            )
            .await
    }
    /// Exact bounded continuation read. Pending frontend memory is not required.
    /// # Errors
    /// Rejects stale projections, incompatible storage or corrupt canonical envelopes.
    pub async fn read_turn_continuation(
        &self,
        key: TurnContinuationKey,
        expected_revision: u64,
    ) -> Result<TurnContinuationRecord, SessionError> {
        self.session_handle(key.session_id)
            .await?
            .read_turn_continuation(key, expected_revision)
            .await
    }
    /// Exact retained operation and acknowledgement, without replay or invented digests.
    /// # Errors
    /// Rejects stale projections, malformed operation identity or damaged receipt evidence.
    pub async fn turn_continuation_operation(
        &self,
        key: TurnContinuationKey,
        operation_id: String,
    ) -> Result<Option<TurnContinuationOperationRecord>, SessionError> {
        self.session_handle(key.session_id)
            .await?
            .turn_continuation_operation(key, operation_id)
            .await
    }
}
