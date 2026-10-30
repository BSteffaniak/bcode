//! Canonical opt-in producer checkpoint storage. No attachment or renderer state is retained.

use crate::{
    SessionError, SessionManager,
    db::{SessionDbError, SessionDbResult},
    db_row::required_string,
};
use bcode_session_models::{
    DURABLE_INTERACTION_VERSION, DurableInteractionKey, DurableInteractionLocator,
    DurableInteractionOperation, DurableInteractionPage, DurableInteractionQuery,
    DurableInteractionReceipt, DurableInteractionRecord, DurableInteractionTransition,
    MAX_DURABLE_INTERACTION_OPERATIONS, MAX_DURABLE_INTERACTION_PAGE,
    MAX_DURABLE_INTERACTION_RETAINED_BYTES, SessionEvent, SessionEventKind, SessionId,
};
use switchy::database::{
    Database,
    query::{FilterableQuery, SortDirection},
};

/// Owned execution authority held across canonical interaction commit and acknowledgement.
///
/// Implementations must prevent authority transfer while alive, not merely cache a check.
/// Storage leases and attachment identities do not establish execution authority.
pub trait DurableInteractionCommitGuard: Send + Sync + std::fmt::Debug {
    /// Revalidate the held execution authority without acquiring session locks.
    /// # Errors
    /// Rejects revoked, stale, cancelled or otherwise unprovable execution ownership.
    fn validate(&self) -> Result<(), SessionError>;
}

/// Exact canonical source facts from one current-compatible database read snapshot.
/// This evidence is not execution authority. Debug deliberately omits source payloads.
#[derive(Clone, PartialEq, Eq)]
pub struct DurableInteractionFacts {
    /// Canonical tail observed in the same snapshot as both records.
    pub generation: u64,
    /// Exact positioned invocation request event.
    pub invocation: SessionEvent,
    /// Exact original exchange request event.
    pub exchange: SessionEvent,
}

impl std::fmt::Debug for DurableInteractionFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableInteractionFacts")
            .field("generation", &self.generation)
            .field("invocation_sequence", &self.invocation.sequence)
            .field("exchange_sequence", &self.exchange.sequence)
            .finish_non_exhaustive()
    }
}

impl DurableInteractionFacts {
    /// SHA-256 source identity, distinct from the caller's per-operation input digest.
    ///
    /// Convention v1: SHA256(domain bytes followed by UTF-8 JSON tuple of the exact
    /// positioned request event and original exchange event, in that order). The read
    /// generation is intentionally excluded. No text normalization or checkpoint hashing.
    /// The v1 convention uses the current serde event-envelope representation; future
    /// incompatible representations require a new digest domain rather than reinterpretation.
    /// # Errors
    /// Rejects malformed correspondence, oversized sources or encoding failures.
    pub fn source_digest(&self) -> Result<String, SessionError> {
        use sha2::{Digest as _, Sha256};
        validate_facts(self)?;
        let mut writer = SourceDigestWriter {
            digest: Sha256::new(),
            remaining: 2 * MAX_SOURCE_BYTES,
        };
        writer
            .digest
            .update(b"bcode.durable-interaction.source.v1\0");
        serde_json::to_writer(&mut writer, &(&self.invocation, &self.exchange)).map_err(|_| {
            SessionError::EventSerialization(
                "durable source encoding exceeded its bound or failed".into(),
            )
        })?;
        Ok(hex::encode(writer.digest.finalize()))
    }
}

struct SourceDigestWriter {
    digest: sha2::Sha256,
    remaining: usize,
}

impl std::io::Write for SourceDigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("durable source digest bound exceeded"))?;
        self.digest.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Exact original retry envelope and acknowledgement, retained after terminalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableInteractionOperationRecord {
    /// Original operation, including its caller-supplied request digest.
    pub operation: DurableInteractionOperation,
    /// Original committed receipt, not a newly generated acknowledgement.
    pub receipt: DurableInteractionReceipt,
}

const MAX_SOURCE_BYTES: usize = 1024 * 1024;

fn validate_facts(facts: &DurableInteractionFacts) -> SessionDbResult<()> {
    let SessionEventKind::PositionedToolCallRequested {
        turn_id,
        tool_call_id,
        producer_plugin_id: Some(producer),
        arguments_json,
        ..
    } = &facts.invocation.kind
    else {
        return Err(invalid());
    };
    let SessionEventKind::ToolExchangeRequested { request } = &facts.exchange.kind else {
        return Err(invalid());
    };
    let key = DurableInteractionKey {
        turn_id: turn_id.clone(),
        invocation_id: tool_call_id.clone(),
        interaction_id: request.exchange_id.clone(),
    };
    key.validate().map_err(|_| invalid())?;
    if producer.trim().is_empty()
        || producer.len() > bcode_session_models::MAX_DURABLE_INTERACTION_ID_BYTES
        || arguments_json.len() > MAX_SOURCE_BYTES
        || request.producer_id != *producer
        || request.invocation_id != *tool_call_id
        || request.schema.trim().is_empty()
        || request.schema.len() > bcode_session_models::MAX_DURABLE_INTERACTION_ID_BYTES
        || request.schema_version == 0
        || facts.invocation.session_id != facts.exchange.session_id
        || facts.invocation.sequence >= facts.exchange.sequence
        || facts.exchange.sequence > facts.generation
        || facts.invocation.schema_version
            != bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
        || facts.exchange.schema_version
            != bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
    {
        return Err(invalid());
    }
    Ok(())
}

pub async fn exact_source(
    db: &dyn Database,
    session_id: SessionId,
    sequence: u64,
) -> SessionDbResult<SessionEvent> {
    if sequence > i64::MAX as u64 {
        return Err(invalid());
    }
    let row = db
        .select("events")
        .columns(&["event_seq", "event_type", "schema_version", "payload"])
        .where_eq("event_seq", crate::db_event_store::seq_to_value(sequence))
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    let value = row.get("payload").ok_or_else(invalid)?;
    let payload = value.as_str().ok_or_else(invalid)?;
    let mut budget = crate::persisted::EventDecodeBudget::new(
        MAX_SOURCE_BYTES,
        bcode_session_models::ObservationRetentionLimits {
            scalar_capacity_bytes: MAX_SOURCE_BYTES * 4,
            collection_entries: 65536,
            value_nodes: 65536,
            depth: 64,
            copies: 1,
        },
    );
    let event = crate::persisted::decode_session_event_with_budget(payload, &mut budget)
        .map_err(|_| invalid())?;
    if event.sequence != sequence
        || event.session_id != session_id
        || crate::db_row::required_i64(&row, "schema_version")? != i64::from(event.schema_version)
    {
        return Err(invalid());
    }
    if required_string(&row, "event_type")?
        != bcode_session_models::persisted_session_event_kind_name(&event.kind)
    {
        return Err(invalid());
    }
    Ok(event)
}

pub async fn facts(
    db: &dyn Database,
    session_id: SessionId,
    invocation_sequence: u64,
    exchange_sequence: u64,
    generation: u64,
) -> SessionDbResult<DurableInteractionFacts> {
    let facts = DurableInteractionFacts {
        generation,
        invocation: exact_source(db, session_id, invocation_sequence).await?,
        exchange: exact_source(db, session_id, exchange_sequence).await?,
    };
    validate_facts(&facts)?;
    Ok(facts)
}

pub async fn retained_operation(
    db: &dyn Database,
    session_id: SessionId,
    key: &DurableInteractionKey,
    operation_id: &str,
) -> SessionDbResult<Option<DurableInteractionOperationRecord>> {
    key.validate().map_err(|_| invalid())?;
    if operation_id.trim().is_empty()
        || operation_id.len() > bcode_session_models::MAX_DURABLE_INTERACTION_ID_BYTES
    {
        return Err(invalid());
    }
    let operation_key = serde_json::to_string(&(key, operation_id))?;
    let row = db
        .select("durable_interaction_operations")
        .columns(&["operation", "receipt"])
        .where_eq("operation_key", operation_key)
        .execute_first(db)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let operation: DurableInteractionOperation = bounded_json(
        &row,
        "operation",
        bcode_session_models::MAX_DURABLE_INTERACTION_OPERATION_BYTES,
    )?;
    operation.validate().map_err(|_| invalid())?;
    let receipt: DurableInteractionReceipt = bounded_json(&row, "receipt", 4096)?;
    if operation.key != *key
        || operation.operation_id != operation_id
        || receipt.key != *key
        || receipt.operation_id != operation_id
        || receipt.version != DURABLE_INTERACTION_VERSION
        || receipt.revision == 0
    {
        return Err(invalid());
    }
    let event = exact_source(db, session_id, receipt.event_sequence).await?;
    if !matches!(event.kind, SessionEventKind::DurableInteractionCommitted { operation: original } if original == operation)
    {
        return Err(invalid());
    }
    if operation
        .expected_revision
        .map_or(Some(1), |revision| revision.checked_add(1))
        != Some(receipt.revision)
        || transition_receipt(&operation, receipt.revision, receipt.event_sequence)? != receipt
    {
        return Err(invalid());
    }
    if let DurableInteractionTransition::Complete {
        result,
        exchange_resolution,
        ..
    } = &operation.transition
    {
        let resolution = exact_source(
            db,
            session_id,
            receipt.event_sequence.checked_add(1).ok_or_else(invalid)?,
        )
        .await?;
        let terminal = exact_source(
            db,
            session_id,
            receipt.event_sequence.checked_add(2).ok_or_else(invalid)?,
        )
        .await?;
        if !matches!(resolution.kind, SessionEventKind::ToolExchangeResolved { event } if event == *exchange_resolution)
            || !matches!(terminal.kind, SessionEventKind::ToolInvocationResultRecorded { record } if record == **result)
        {
            return Err(invalid());
        }
    }
    Ok(Some(DurableInteractionOperationRecord {
        operation,
        receipt,
    }))
}

/// Integration must enable this only with the event-schema/writer-epoch upgrade,
/// registered schema migrations, projection maintenance and compatibility coverage.
pub const PUBLICATION_ENABLED: bool = false;

pub fn ensure_publication_enabled() -> SessionDbResult<()> {
    if !PUBLICATION_ENABLED {
        return Err(SessionDbError::InvalidData(
            "durable interaction publication requires the coordinated writer-contract upgrade"
                .into(),
        ));
    }
    Ok(())
}

fn invalid() -> SessionDbError {
    SessionDbError::InvalidData(
        "invalid, stale, conflicting or exhausted durable interaction".into(),
    )
}

fn bounded_json<T: serde::de::DeserializeOwned>(
    row: &switchy::database::Row,
    column: &str,
    limit: usize,
) -> SessionDbResult<T> {
    let value = row.get(column).ok_or_else(invalid)?;
    let text = value.as_str().ok_or_else(invalid)?;
    if text.len() > limit {
        return Err(invalid());
    }
    Ok(serde_json::from_str(text)?)
}

fn key_string(key: &DurableInteractionKey) -> SessionDbResult<String> {
    key.validate().map_err(|_| invalid())?;
    Ok(serde_json::to_string(key)?)
}

fn operation_key(operation: &DurableInteractionOperation) -> SessionDbResult<String> {
    Ok(serde_json::to_string(&(
        &operation.key,
        &operation.operation_id,
    ))?)
}

/// Exact indexed lookup, requiring the caller to validate the projection checkpoint first.
pub async fn lookup(
    db: &dyn Database,
    key: &DurableInteractionKey,
) -> SessionDbResult<Option<DurableInteractionRecord>> {
    let row = db
        .select("durable_interactions")
        .columns(&["record"])
        .where_eq("interaction_key", key_string(key)?)
        .execute_first(db)
        .await?;
    row.map(|row| {
        let record: DurableInteractionRecord = bounded_json(&row, "record", 512 * 1024)?;
        if record.version != DURABLE_INTERACTION_VERSION || &record.key != key {
            return Err(invalid());
        }
        Ok(record)
    })
    .transpose()
}

/// Exact retry check precedes revision validation, so lost acknowledgements remain recoverable.
pub async fn retry(
    db: &dyn Database,
    operation: &DurableInteractionOperation,
) -> SessionDbResult<Option<DurableInteractionReceipt>> {
    operation.validate().map_err(|_| invalid())?;
    let row = db
        .select("durable_interaction_operations")
        .columns(&["operation", "receipt"])
        .where_eq("operation_key", operation_key(operation)?)
        .execute_first(db)
        .await?;
    row.map(|row| {
        let original: DurableInteractionOperation = bounded_json(
            &row,
            "operation",
            bcode_session_models::MAX_DURABLE_INTERACTION_OPERATION_BYTES,
        )?;
        if original != *operation {
            return Err(invalid());
        }
        let receipt: DurableInteractionReceipt = bounded_json(&row, "receipt", 4 * 1024)?;
        if receipt.version != DURABLE_INTERACTION_VERSION
            || receipt.key != operation.key
            || receipt.operation_id != operation.operation_id
        {
            return Err(invalid());
        }
        Ok(receipt)
    })
    .transpose()
}

/// Pure CAS state transition, shared by canonical append validation and projection.
pub fn advance(
    previous: Option<DurableInteractionRecord>,
    operation: &DurableInteractionOperation,
    event_sequence: u64,
) -> SessionDbResult<DurableInteractionRecord> {
    operation.validate().map_err(|_| invalid())?;
    if operation.request_event_sequence >= event_sequence
        || operation.expected_revision == Some(0)
        || operation.expected_revision == Some(u64::MAX)
    {
        return Err(invalid());
    }
    let bytes = u64::try_from(serde_json::to_vec(operation)?.len()).map_err(|_| invalid())?;
    let revision = previous.as_ref().map_or(Ok(1), |previous| {
        if previous.version != DURABLE_INTERACTION_VERSION
            || previous.key != operation.key
            || previous.producer_plugin_id != operation.producer_plugin_id
            || previous.request_event_sequence != operation.request_event_sequence
            || previous.exchange_request_event_sequence != operation.exchange_request_event_sequence
            || previous.receipt.event_sequence >= event_sequence
            || previous.receipt.version != DURABLE_INTERACTION_VERSION
            || previous.receipt.key != operation.key
            || operation.expected_revision != Some(previous.receipt.revision)
        {
            return Err(invalid());
        }
        previous.receipt.revision.checked_add(1).ok_or_else(invalid)
    })?;
    if revision > MAX_DURABLE_INTERACTION_OPERATIONS {
        return Err(invalid());
    }
    let retained_operation_bytes = previous
        .as_ref()
        .map_or(0, |previous| previous.retained_operation_bytes)
        .checked_add(bytes)
        .filter(|bytes| *bytes <= MAX_DURABLE_INTERACTION_RETAINED_BYTES)
        .ok_or_else(invalid)?;
    let receipt = transition_receipt(operation, revision, event_sequence)?;
    let tool_result_event_sequence = receipt.tool_result_event_sequence;
    let mut record = if let Some(previous) = previous {
        previous
    } else {
        let DurableInteractionTransition::Checkpoint { payload } = &operation.transition else {
            return Err(invalid());
        };
        if operation.expected_revision.is_some() {
            return Err(invalid());
        }
        DurableInteractionRecord {
            version: DURABLE_INTERACTION_VERSION,
            key: operation.key.clone(),
            producer_plugin_id: operation.producer_plugin_id.clone(),
            request_event_sequence: operation.request_event_sequence,
            exchange_request_event_sequence: operation.exchange_request_event_sequence,
            receipt: receipt.clone(),
            checkpoint: payload.clone(),
            outcome: None,
            tool_result_event_sequence: None,
            continuation_publication_id: None,
            retained_operation_bytes: 0,
        }
    };
    match &operation.transition {
        DurableInteractionTransition::Checkpoint { payload } => {
            if record.outcome.is_some()
                || payload.contract_id != record.checkpoint.contract_id
                || payload.contract_version != record.checkpoint.contract_version
            {
                return Err(invalid());
            }
            record.checkpoint = payload.clone();
        }
        DurableInteractionTransition::Complete { outcome, .. } => {
            if record.outcome.is_some() {
                return Err(invalid());
            }
            record.outcome = Some(outcome.clone());
            record.tool_result_event_sequence = tool_result_event_sequence;
        }
        DurableInteractionTransition::PublishContinuation { publication_id } => {
            if record.outcome.is_none()
                || record.continuation_publication_id.is_some()
                || record
                    .tool_result_event_sequence
                    .is_none_or(|sequence| sequence >= event_sequence)
            {
                return Err(invalid());
            }
            record.continuation_publication_id = Some(publication_id.clone());
        }
    }
    record.receipt = receipt;
    record.retained_operation_bytes = retained_operation_bytes;
    Ok(record)
}

fn transition_receipt(
    operation: &DurableInteractionOperation,
    revision: u64,
    event_sequence: u64,
) -> SessionDbResult<DurableInteractionReceipt> {
    let tool_result_event_sequence = if matches!(
        operation.transition,
        DurableInteractionTransition::Complete { .. }
    ) {
        Some(event_sequence.checked_add(2).ok_or_else(invalid)?)
    } else {
        None
    };
    Ok(DurableInteractionReceipt {
        version: DURABLE_INTERACTION_VERSION,
        key: operation.key.clone(),
        operation_id: operation.operation_id.clone(),
        revision,
        event_sequence,
        tool_result_event_sequence,
    })
}

/// Validate canonical identity and exclude generic/single-event completion publication.
pub async fn validate_append(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    if PUBLICATION_ENABLED
        && matches!(&event.kind, SessionEventKind::ToolInvocationLifecycle { event: lifecycle } if lifecycle.stage == bcode_session_models::ToolInvocationLifecycleStage::Started)
    {
        let SessionEventKind::ToolInvocationLifecycle { event: lifecycle } = &event.kind else {
            unreachable!()
        };
        if db
            .select("durable_interactions")
            .columns(&["interaction_key"])
            .where_eq("invocation_id", lifecycle.invocation_id.clone())
            .limit(1)
            .execute_first(db)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
    }
    validate_resolution_append(db, event).await?;
    if let SessionEventKind::ToolInvocationResultRecorded { record } = &event.kind {
        if PUBLICATION_ENABLED {
            let rows = db
                .select("durable_interactions")
                .columns(&["record"])
                .where_eq("invocation_id", record.invocation_id.clone())
                .limit(2)
                .execute(db)
                .await?;
            if rows.len() > 1 {
                return Err(invalid());
            }
            if let Some(row) = rows.first() {
                let interaction: DurableInteractionRecord =
                    bounded_json(row, "record", 512 * 1024)?;
                if interaction.tool_result_event_sequence != Some(event.sequence) {
                    return Err(invalid());
                }
                let operation_row = db
                    .select("durable_interaction_operations")
                    .columns(&["operation"])
                    .where_eq(
                        "operation_key",
                        serde_json::to_string(&(
                            &interaction.key,
                            &interaction.receipt.operation_id,
                        ))?,
                    )
                    .execute_first(db)
                    .await?
                    .ok_or_else(invalid)?;
                let operation: DurableInteractionOperation = bounded_json(
                    &operation_row,
                    "operation",
                    bcode_session_models::MAX_DURABLE_INTERACTION_OPERATION_BYTES,
                )?;
                if !matches!(operation.transition, DurableInteractionTransition::Complete { result, .. } if *result == *record)
                {
                    return Err(invalid());
                }
            }
        }
        return Ok(());
    }
    let SessionEventKind::DurableInteractionCommitted { operation } = &event.kind else {
        return Ok(());
    };
    ensure_publication_enabled()?;
    validate_transition(db, event.session_id, operation, event.sequence).await
}

async fn validate_resolution_append(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    if let SessionEventKind::ToolExchangeResolved { event: resolution } = &event.kind
        && PUBLICATION_ENABLED
    {
        let rows = db
            .select("durable_interactions")
            .columns(&["record"])
            .where_eq("invocation_id", resolution.invocation_id.clone())
            .limit(2)
            .execute(db)
            .await?;
        if rows.len() > 1 {
            return Err(invalid());
        }
        if let Some(row) = rows.first() {
            let interaction: DurableInteractionRecord = bounded_json(row, "record", 512 * 1024)?;
            if interaction.receipt.event_sequence.checked_add(1) != Some(event.sequence) {
                return Err(invalid());
            }
            let operation_row = db
                .select("durable_interaction_operations")
                .columns(&["operation"])
                .where_eq(
                    "operation_key",
                    serde_json::to_string(&(&interaction.key, &interaction.receipt.operation_id))?,
                )
                .execute_first(db)
                .await?
                .ok_or_else(invalid)?;
            let operation: DurableInteractionOperation = bounded_json(
                &operation_row,
                "operation",
                bcode_session_models::MAX_DURABLE_INTERACTION_OPERATION_BYTES,
            )?;
            if !matches!(operation.transition, DurableInteractionTransition::Complete { exchange_resolution, .. } if exchange_resolution == *resolution)
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

/// Exact checkpointed exchange evidence; stale or inconsistent locators fail closed.
async fn validate_exchange_binding(
    db: &dyn Database,
    session_id: SessionId,
    operation: &DurableInteractionOperation,
    sequence: u64,
) -> SessionDbResult<()> {
    if operation.exchange_request_event_sequence <= operation.request_event_sequence
        || operation.exchange_request_event_sequence >= sequence
    {
        return Err(invalid());
    }
    let row = db
        .select("events")
        .columns(&["event_seq", "payload"])
        .where_eq(
            "event_seq",
            crate::db_event_store::seq_to_value(operation.exchange_request_event_sequence),
        )
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    let original = crate::db_event_store::strict_event_from_row(&row, session_id)?;
    let SessionEventKind::ToolExchangeRequested { request } = original.kind else {
        return Err(invalid());
    };
    if request.invocation_id != operation.key.invocation_id
        || request.producer_id != operation.producer_plugin_id
    {
        return Err(invalid());
    }
    if let DurableInteractionTransition::Complete {
        exchange_resolution,
        ..
    } = &operation.transition
        && (exchange_resolution.invocation_id != request.invocation_id
            || exchange_resolution.exchange_id != request.exchange_id)
    {
        return Err(invalid());
    }
    validate_exchange_checkpoint(db, sequence.checked_sub(1).ok_or_else(invalid)?).await?;
    let row = db
        .select("interaction_exchange_locators")
        .columns(&["request_seq", "resolution_seq"])
        .where_eq(
            "exchange_key",
            exchange_key(&request.invocation_id, &request.exchange_id)?,
        )
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    if crate::db_row::required_non_negative_u64(&row, "request_seq")?
        != operation.exchange_request_event_sequence
    {
        return Err(invalid());
    }
    if !matches!(
        operation.transition,
        DurableInteractionTransition::PublishContinuation { .. }
    ) && crate::db_row::optional_i64(&row, "resolution_seq").is_some()
    {
        return Err(invalid());
    }

    Ok(())
}

async fn validate_current_lifecycle(
    db: &dyn Database,
    operation: &DurableInteractionOperation,
) -> SessionDbResult<()> {
    // Exact indexed terminal evidence is negative proof only, never dispatch authority.
    if db
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
    let row = db
        .select("tool_runs")
        .columns(&["status", "event_seq_start", "event_seq_end"])
        .where_eq("tool_call_id", operation.key.invocation_id.clone())
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    if crate::db_row::required_non_negative_u64(&row, "event_seq_start")?
        != operation.request_event_sequence
    {
        return Err(invalid());
    }
    if !matches!(
        operation.transition,
        DurableInteractionTransition::PublishContinuation { .. }
    ) && (required_string(&row, "status")? != "running"
        || crate::db_row::optional_i64(&row, "event_seq_end").is_some())
    {
        return Err(invalid());
    }
    Ok(())
}

pub async fn validate_transition(
    db: &dyn Database,
    session_id: SessionId,
    operation: &DurableInteractionOperation,
    sequence: u64,
) -> SessionDbResult<()> {
    operation.validate().map_err(|_| invalid())?;
    validate_checkpoint(db, sequence.checked_sub(1).ok_or_else(invalid)?).await?;
    let row = db
        .select("events")
        .columns(&["event_seq", "event_type", "schema_version", "payload"])
        .where_eq(
            "event_seq",
            crate::db_event_store::seq_to_value(operation.request_event_sequence),
        )
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    let request = crate::db_event_store::strict_event_from_row(&row, session_id)?;
    if !matches!(&request.kind, SessionEventKind::PositionedToolCallRequested { turn_id, tool_call_id, producer_plugin_id: Some(producer), .. }
        if turn_id == &operation.key.turn_id && tool_call_id == &operation.key.invocation_id && producer == &operation.producer_plugin_id)
        || operation.request_event_sequence >= sequence
    {
        return Err(invalid());
    }
    validate_current_lifecycle(db, operation).await?;
    validate_exchange_binding(db, session_id, operation, sequence).await?;
    let previous = lookup(db, &operation.key).await?;
    if previous.is_none()
        && db
            .select("durable_interactions")
            .columns(&["interaction_key"])
            .where_eq("invocation_id", operation.key.invocation_id.clone())
            .limit(1)
            .execute_first(db)
            .await?
            .is_some()
    {
        return Err(invalid());
    }
    // A terminal invocation cannot acquire a new wait or a second outcome.
    if !matches!(
        operation.transition,
        DurableInteractionTransition::PublishContinuation { .. }
    ) {
        let terminal = db
            .select("tool_runs")
            .columns(&["status"])
            .where_eq("tool_call_id", operation.key.invocation_id.clone())
            .execute_first(db)
            .await?;
        if let Some(row) = terminal {
            let status = required_string(&row, "status")?;
            if status != "running" {
                return Err(invalid());
            }
        }
    }
    advance(previous, operation, sequence)?;
    Ok(())
}

/// Payload-free canonical exchange locators are retained after resolution. No TTL/eviction.
async fn exchange_checkpoint(db: &dyn Database) -> SessionDbResult<Option<i64>> {
    let row = db
        .select("projection_checkpoints")
        .columns(&["last_event_seq", "projection_version"])
        .where_eq("projection_name", "interaction_exchange_locators")
        .execute_first(db)
        .await?;
    row.map(|row| {
        if crate::db_row::required_i64(&row, "projection_version")? != 1 {
            return Err(invalid());
        }
        crate::db_row::required_i64(&row, "last_event_seq")
    })
    .transpose()
}

pub async fn validate_exchange_checkpoint(
    db: &dyn Database,
    generation: u64,
) -> SessionDbResult<()> {
    let checkpoint = exchange_checkpoint(db).await?;
    if checkpoint != i64::try_from(generation).ok() {
        return Err(SessionDbError::ProjectionStale {
            projection: "interaction_exchange_locators",
            checkpoint: checkpoint.and_then(|value| u64::try_from(value).ok()),
            expected: generation,
        });
    }
    Ok(())
}

fn exchange_key(invocation: &str, exchange: &str) -> SessionDbResult<String> {
    for identity in [invocation, exchange] {
        if identity.trim().is_empty()
            || identity.len() > bcode_session_models::MAX_DURABLE_INTERACTION_ID_BYTES
        {
            return Err(invalid());
        }
    }
    Ok(serde_json::to_string(&(invocation, exchange))?)
}

pub async fn project_exchange_locator(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let Some(checkpoint) = exchange_checkpoint(db).await? else {
        if PUBLICATION_ENABLED || db.table_exists("interaction_exchange_locators").await? {
            return Err(invalid());
        }
        return Ok(());
    };
    let sequence = i64::try_from(event.sequence).map_err(|_| invalid())?;
    if sequence.checked_sub(1) != Some(checkpoint) {
        return Err(invalid());
    }
    match &event.kind {
        SessionEventKind::ToolExchangeRequested { request } => {
            let key = exchange_key(&request.invocation_id, &request.exchange_id)?;
            if db
                .select("interaction_exchange_locators")
                .columns(&["request_seq"])
                .where_eq("exchange_key", key.clone())
                .execute_first(db)
                .await?
                .is_some()
            {
                return Err(invalid());
            }
            db.insert("interaction_exchange_locators")
                .value("exchange_key", key)
                .value("invocation_id", request.invocation_id.clone())
                .value("exchange_id", request.exchange_id.clone())
                .value("request_seq", sequence)
                .execute(db)
                .await?;
        }
        SessionEventKind::ToolExchangeResolved { event: resolution } => {
            let key = exchange_key(&resolution.invocation_id, &resolution.exchange_id)?;
            if let Some(row) = db
                .select("interaction_exchange_locators")
                .columns(&["resolution_seq"])
                .where_eq("exchange_key", key.clone())
                .execute_first(db)
                .await?
            {
                if crate::db_row::optional_i64(&row, "resolution_seq").is_some() {
                    return Err(invalid());
                }
                db.update("interaction_exchange_locators")
                    .value("resolution_seq", sequence)
                    .where_eq("exchange_key", key)
                    .execute(db)
                    .await?;
            }
        }
        _ => {}
    }
    db.update("projection_checkpoints")
        .value("last_event_seq", sequence)
        .value(
            "updated_at_ms",
            crate::db_event_store::seq_to_value(event.timestamp_ms),
        )
        .where_eq("projection_name", "interaction_exchange_locators")
        .execute(db)
        .await?;
    Ok(())
}

pub fn validate_source_ids(invocation_id: &str, exchange_id: &str) -> SessionDbResult<()> {
    exchange_key(invocation_id, exchange_id)?;
    Ok(())
}

pub async fn source(
    db: &dyn Database,
    session_id: SessionId,
    invocation_id: &str,
    exchange_id: &str,
    generation: u64,
) -> SessionDbResult<DurableInteractionFacts> {
    validate_source_ids(invocation_id, exchange_id)?;
    validate_exchange_checkpoint(db, generation).await?;
    let row = db
        .select("tool_runs")
        .columns(&["event_seq_start"])
        .where_eq("tool_call_id", invocation_id.to_owned())
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    let sequence = crate::db_row::required_non_negative_u64(&row, "event_seq_start")?;
    let facts = exchange_facts(db, session_id, sequence, exchange_id, generation).await?;
    if !matches!(&facts.invocation.kind, SessionEventKind::PositionedToolCallRequested { tool_call_id, .. } if tool_call_id == invocation_id)
    {
        return Err(invalid());
    }
    Ok(facts)
}

pub async fn exchange_facts(
    db: &dyn Database,
    session_id: SessionId,
    invocation_sequence: u64,
    exchange_id: &str,
    generation: u64,
) -> SessionDbResult<DurableInteractionFacts> {
    validate_exchange_checkpoint(db, generation).await?;
    if db
        .select("sqlite_schema")
        .columns(&["name"])
        .where_eq("type", "index")
        .where_eq("name", "idx_interaction_exchange_identity")
        .where_eq("tbl_name", "interaction_exchange_locators")
        .execute_first(db)
        .await?
        .is_none()
    {
        return Err(invalid());
    }
    let invocation = exact_source(db, session_id, invocation_sequence).await?;
    let SessionEventKind::PositionedToolCallRequested { tool_call_id, .. } = &invocation.kind
    else {
        return Err(invalid());
    };
    exchange_key(tool_call_id, exchange_id)?;
    let rows = db
        .select("interaction_exchange_locators")
        .columns(&["exchange_key", "request_seq"])
        .where_eq("invocation_id", tool_call_id.clone())
        .where_eq("exchange_id", exchange_id.to_owned())
        .limit(2)
        .execute(db)
        .await?;
    if rows.len() != 1 {
        return Err(invalid());
    }
    let row = &rows[0];
    if required_string(row, "exchange_key")? != exchange_key(tool_call_id, exchange_id)? {
        return Err(invalid());
    }
    let sequence = crate::db_row::required_non_negative_u64(row, "request_seq")?;
    let result = facts(db, session_id, invocation_sequence, sequence, generation).await?;
    if !matches!(&result.exchange.kind, SessionEventKind::ToolExchangeRequested { request } if request.exchange_id == exchange_id)
    {
        return Err(invalid());
    }
    Ok(result)
}

/// Validate and update derived rows inside the caller's canonical append transaction.
pub async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    if let SessionEventKind::DurableInteractionCommitted { operation } = &event.kind {
        if retry(db, operation).await?.is_some() {
            return Err(invalid());
        }
        let record = advance(lookup(db, &operation.key).await?, operation, event.sequence)?;
        db.upsert("durable_interactions")
            .unique(&["interaction_key"])
            .value("interaction_key", key_string(&operation.key)?)
            .value("invocation_id", operation.key.invocation_id.clone())
            .value(
                "event_seq",
                crate::db_event_store::seq_to_value(event.sequence),
            )
            .value(
                "locator",
                serde_json::to_string(&DurableInteractionLocator {
                    key: operation.key.clone(),
                    event_sequence: event.sequence,
                    terminal: record.outcome.is_some(),
                    continuation_published: record.continuation_publication_id.is_some(),
                })?,
            )
            .value("record", serde_json::to_string(&record)?)
            .execute(db)
            .await?;
        db.insert("durable_interaction_operations")
            .value("operation_key", operation_key(operation)?)
            .value("operation", serde_json::to_string(operation)?)
            .value("receipt", serde_json::to_string(&record.receipt)?)
            .execute(db)
            .await?;
    }
    if PUBLICATION_ENABLED
        || matches!(
            event.kind,
            SessionEventKind::DurableInteractionCommitted { .. }
        )
    {
        db.upsert("projection_checkpoints")
            .unique(&["projection_name"])
            .value("projection_name", "durable_interactions")
            .value(
                "last_event_seq",
                crate::db_event_store::seq_to_value(event.sequence),
            )
            .value("projection_version", 1_i64)
            .value(
                "updated_at_ms",
                crate::db_event_store::seq_to_value(event.timestamp_ms),
            )
            .execute(db)
            .await?;
    }
    Ok(())
}

pub async fn validate_checkpoint(db: &dyn Database, generation: u64) -> SessionDbResult<()> {
    let row = db
        .select("projection_checkpoints")
        .columns(&["last_event_seq", "projection_version"])
        .where_eq("projection_name", "durable_interactions")
        .execute_first(db)
        .await?;
    let checkpoint = row
        .as_ref()
        .map(|row| crate::db_row::required_i64(row, "last_event_seq"))
        .transpose()?;
    let version = row
        .as_ref()
        .map(|row| crate::db_row::required_i64(row, "projection_version"))
        .transpose()?;
    if checkpoint.and_then(|value| u64::try_from(value).ok()) != Some(generation)
        || version != Some(1)
    {
        return Err(SessionDbError::ProjectionStale {
            projection: "durable_interactions",
            checkpoint: checkpoint.and_then(|value| u64::try_from(value).ok()),
            expected: generation,
        });
    }
    Ok(())
}

pub async fn page(
    db: &dyn Database,
    query: &DurableInteractionQuery,
    generation: u64,
) -> SessionDbResult<DurableInteractionPage> {
    if query.limit == 0
        || query.limit > MAX_DURABLE_INTERACTION_PAGE
        || query
            .expected_generation
            .is_some_and(|expected| expected != generation)
    {
        return Err(invalid());
    }
    validate_checkpoint(db, generation).await?;
    let mut select = db.select("durable_interactions").columns(&["locator"]);
    if let Some(after) = query.after_event_sequence {
        select = select.where_gt("event_seq", crate::db_event_store::seq_to_value(after));
    }
    let rows = select
        .sort("event_seq", SortDirection::Asc)
        .limit(query.limit + 1)
        .execute(db)
        .await?;
    let more = rows.len() > query.limit;
    let entries = rows
        .into_iter()
        .take(query.limit)
        .map(|row| {
            let entry: DurableInteractionLocator = bounded_json(&row, "locator", 4 * 1024)?;
            entry.key.validate().map_err(|_| invalid())?;
            Ok(entry)
        })
        .collect::<SessionDbResult<Vec<_>>>()?;
    let next_after_event_sequence = if more {
        entries.last().map(|entry| entry.event_sequence)
    } else {
        None
    };
    Ok(DurableInteractionPage {
        generation,
        entries,
        next_after_event_sequence,
    })
}

impl SessionManager {
    /// Resolve canonical invocation/exchange source by bounded IDs without enumerating active work.
    /// # Errors
    /// Rejects missing, stale, corrupt or mismatched indexed/canonical source evidence.
    pub async fn durable_interaction_source(
        &self,
        session_id: SessionId,
        invocation_id: String,
        exchange_id: String,
    ) -> Result<DurableInteractionFacts, SessionError> {
        validate_source_ids(&invocation_id, &exchange_id)?;
        self.session_handle(session_id)
            .await?
            .durable_interaction_source(invocation_id, exchange_id)
            .await
    }

    /// Resolve an exact canonical exchange identity through its current checkpointed locator.
    /// # Errors
    /// Rejects absent/stale/corrupt locators, duplicate identities or mismatched canonical sources.
    pub async fn durable_interaction_exchange_facts(
        &self,
        session_id: SessionId,
        invocation_sequence: u64,
        exchange_id: String,
    ) -> Result<DurableInteractionFacts, SessionError> {
        self.session_handle(session_id)
            .await?
            .durable_interaction_exchange_facts(invocation_sequence, exchange_id)
            .await
    }

    /// Read exact positioned invocation and exchange events from one database snapshot.
    /// No execution authority is granted and publication need not be enabled.
    /// # Errors
    /// Rejects missing, oversized, mismatched, incompatible or stale canonical source state.
    pub async fn durable_interaction_facts(
        &self,
        session_id: SessionId,
        invocation_sequence: u64,
        exchange_sequence: u64,
    ) -> Result<DurableInteractionFacts, SessionError> {
        self.session_handle(session_id)
            .await?
            .durable_interaction_facts(invocation_sequence, exchange_sequence)
            .await
    }

    /// Read the original retained operation and receipt after detach or terminalization.
    /// # Errors
    /// Rejects malformed identities, stale projections or conflicting canonical receipt evidence.
    pub async fn durable_interaction_operation(
        &self,
        session_id: SessionId,
        key: DurableInteractionKey,
        operation_id: String,
    ) -> Result<Option<DurableInteractionOperationRecord>, SessionError> {
        self.session_handle(session_id)
            .await?
            .durable_interaction_operation(key, operation_id)
            .await
    }

    /// Commit an opt-in checkpoint/transition under existing session execution ownership.
    /// Exact retries return the original receipt; publication is disabled until coordinated upgrade.
    ///
    /// # Errors
    /// Rejects disabled compatibility, missing durable storage/ownership, stale CAS, conflicting retries,
    /// terminal edits, exhausted retention, invalid producer identity or storage failures.
    pub async fn commit_durable_interaction(
        &self,
        session_id: SessionId,
        operation: DurableInteractionOperation,
        guard: std::sync::Arc<dyn DurableInteractionCommitGuard>,
    ) -> Result<DurableInteractionReceipt, SessionError> {
        guard.validate()?;
        ensure_publication_enabled()?;
        operation
            .validate()
            .map_err(|field| SessionError::EventSerialization(field.into()))?;
        let handle = self.session_handle(session_id).await?;
        handle
            .commit_durable_interaction(
                operation,
                self.next_activity_timestamp_ms(),
                self.mutation_tx.clone(),
                guard,
            )
            .await
    }

    /// Read one bounded durable checkpoint, without replay, migration or attachment authority.
    ///
    /// # Errors
    /// Returns compatibility, stale projection, malformed record or storage errors.
    pub async fn durable_interaction(
        &self,
        session_id: SessionId,
        key: DurableInteractionKey,
    ) -> Result<Option<DurableInteractionRecord>, SessionError> {
        ensure_publication_enabled()?;
        self.session_handle(session_id)
            .await?
            .durable_interaction(key)
            .await
    }

    /// Observe the opt-in durable producer state for an invocation; never a tool retry permit.
    /// # Errors
    /// Rejects incompatible storage, stale projections or malformed invocation bindings.
    pub async fn durable_interaction_for_invocation(
        &self,
        session_id: SessionId,
        invocation_id: String,
    ) -> Result<Option<DurableInteractionRecord>, SessionError> {
        ensure_publication_enabled()?;
        self.session_handle(session_id)
            .await?
            .durable_interaction_for_invocation(invocation_id)
            .await
    }

    /// Read metadata-only recovery locators pinned to a canonical generation.
    ///
    /// # Errors
    /// Rejects invalid bounds, changed generations, stale projections or incompatible storage.
    pub async fn durable_interaction_page(
        &self,
        session_id: SessionId,
        query: DurableInteractionQuery,
    ) -> Result<DurableInteractionPage, SessionError> {
        ensure_publication_enabled()?;
        self.session_handle(session_id)
            .await?
            .durable_interaction_page(query)
            .await
    }
}

#[cfg(test)]
#[path = "durable_interaction_tests.rs"]
mod tests;
