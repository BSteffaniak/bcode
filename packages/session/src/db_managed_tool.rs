//! Bounded canonical managed-tool claim projection. Rows are disposable indexes into events.
use crate::db::{SessionDb, SessionDbError, SessionDbResult};
use crate::db_event_store::strict_event_from_row;
use crate::db_row::{optional_non_negative_u64, required_non_negative_u64, required_string};
use bcode_session_models::{
    ManagedToolClaimEvent, ManagedToolClaimRecord, SessionEvent, SessionEventKind, SessionId,
};
use switchy::database::query::FilterableQuery as _;
use switchy::database::{Database, DatabaseValue};

fn invalid() -> SessionDbError {
    SessionDbError::InvalidRow {
        column: "managed_tool_claims canonical evidence".into(),
    }
}
fn sequence(value: u64) -> SessionDbResult<DatabaseValue> {
    i64::try_from(value).map(Into::into).map_err(|_| invalid())
}
async fn event_at(
    db: &dyn Database,
    session: SessionId,
    seq: u64,
) -> SessionDbResult<SessionEvent> {
    let length = db
        .select("events")
        .project_as(
            switchy::database::query::byte_length(switchy::database::query::identifier("payload")),
            "bytes",
        )
        .where_eq("event_seq", sequence(seq)?)
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    if required_non_negative_u64(&length, "bytes")? > 4 * 1024 * 1024 {
        return Err(invalid());
    }
    let row = db
        .select("events")
        .columns(&["event_seq", "payload"])
        .where_eq("event_seq", sequence(seq)?)
        .execute_first(db)
        .await?
        .ok_or_else(invalid)?;
    strict_event_from_row(&row, session)
}

impl SessionDb {
    /// Read and verify one claim against its canonical claim/start/result events.
    pub(crate) async fn managed_tool_claim(
        &self,
        invocation_id: &str,
    ) -> SessionDbResult<Option<ManagedToolClaimRecord>> {
        self.validate_write_readiness().await?;
        read(self.database(), self.session_id(), invocation_id, None).await
    }
    /// Read exact canonical result evidence selected by a verified claim.
    pub(crate) async fn managed_tool_result(
        &self,
        claim: &ManagedToolClaimRecord,
    ) -> SessionDbResult<bcode_session_models::ToolInvocationResultRecord> {
        let event = event_at(
            self.database(),
            self.session_id(),
            claim.result_sequence.ok_or_else(invalid)?,
        )
        .await?;
        if let SessionEventKind::ToolInvocationResultRecorded { record } = event.kind
            && record.invocation_id == claim.identity.invocation_id
        {
            return Ok(record);
        }
        Err(invalid())
    }
}

/// Canonical indexed identity, stored atomically on the event row, not a second ledger.
pub fn canonical_key(event: &SessionEvent) -> SessionDbResult<Option<String>> {
    if let Some(claim) = ManagedToolClaimEvent::from_event(event).map_err(|_| invalid())? {
        let identity = match claim {
            ManagedToolClaimEvent::Claimed { identity }
            | ManagedToolClaimEvent::Started { identity, .. } => identity,
        };
        return Ok(Some(format!("claim:{}", identity.invocation_id)));
    }
    Ok(match &event.kind {
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            Some(format!("result:{}", record.invocation_id))
        }
        _ => None,
    })
}

async fn canonical_latest(
    db: &dyn Database,
    session: SessionId,
    id: &str,
    before: Option<u64>,
) -> SessionDbResult<Option<SessionEvent>> {
    let indexes = db
        .select("sqlite_master")
        .columns(&["name"])
        .where_eq("type", "index")
        .where_eq("name", "idx_events_managed_tool_key")
        .limit(1)
        .execute(db)
        .await?;
    if indexes.is_empty() {
        return Err(invalid());
    }
    let mut query = db
        .select("events")
        .columns(&["event_seq"])
        .where_eq("managed_tool_key", id.to_owned());
    if let Some(before) = before {
        query = query.where_lt("event_seq", sequence(before)?);
    }
    let row = query
        .sort("event_seq", switchy::database::query::SortDirection::Desc)
        .limit(1)
        .execute_first(db)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let event = event_at(db, session, required_non_negative_u64(&row, "event_seq")?).await?;
    if canonical_key(&event)?.as_deref() != Some(id) {
        return Err(invalid());
    }
    Ok(Some(event))
}

async fn read(
    db: &dyn Database,
    session: SessionId,
    id: &str,
    before: Option<u64>,
) -> SessionDbResult<Option<ManagedToolClaimRecord>> {
    if before.is_none() && (id.trim().is_empty() || id.len() > 256) {
        return Err(invalid());
    }
    let latest = canonical_latest(db, session, &format!("claim:{id}"), before).await?;
    let Some(row) = db
        .select("managed_tool_claims")
        .columns(&[
            "invocation_id",
            "claim_sequence",
            "started_sequence",
            "result_sequence",
            "record",
        ])
        .where_eq("invocation_id", id.to_owned())
        .execute_first(db)
        .await?
    else {
        if latest.is_some()
            || (before.is_none()
                && canonical_latest(db, session, &format!("result:{id}"), before)
                    .await?
                    .is_some())
        {
            return Err(invalid());
        }
        return Ok(None);
    };
    let json = required_string(&row, "record")?;
    if json.len() > 16_384 {
        return Err(invalid());
    }
    let claim: ManagedToolClaimRecord = serde_json::from_str(&json)?;
    claim.identity.validate().map_err(|_| invalid())?;
    if claim.identity.invocation_id != id
        || claim.identity.owner.session_id != session
        || required_string(&row, "invocation_id")? != id
        || required_non_negative_u64(&row, "claim_sequence")? != claim.claim_sequence
        || optional_non_negative_u64(&row, "started_sequence")? != claim.started_sequence
        || optional_non_negative_u64(&row, "result_sequence")? != claim.result_sequence
    {
        return Err(invalid());
    }
    let result = canonical_latest(db, session, &format!("result:{id}"), before).await?;
    let latest_sequence = latest
        .as_ref()
        .map(|event| event.sequence)
        .into_iter()
        .chain(result.as_ref().map(|event| event.sequence))
        .max();
    let expected_latest = claim
        .result_sequence
        .or(claim.started_sequence)
        .unwrap_or(claim.claim_sequence);
    if latest_sequence != Some(expected_latest) {
        return Err(invalid());
    }
    verify_claim_events(db, session, id, &claim).await?;
    Ok(Some(claim))
}

async fn verify_claim_events(
    db: &dyn Database,
    session: SessionId,
    id: &str,
    claim: &ManagedToolClaimRecord,
) -> SessionDbResult<()> {
    let original = event_at(db, session, claim.claim_sequence).await?;
    if !matches!(ManagedToolClaimEvent::from_event(&original).map_err(|_| invalid())?,
        Some(ManagedToolClaimEvent::Claimed { identity }) if identity == claim.identity)
    {
        return Err(invalid());
    }
    if let Some(seq) = claim.started_sequence {
        let started = event_at(db, session, seq).await?;
        if seq <= claim.claim_sequence
            || !matches!(ManagedToolClaimEvent::from_event(&started).map_err(|_| invalid())?,
            Some(ManagedToolClaimEvent::Started { identity, claim_sequence }) if identity == claim.identity && claim_sequence == claim.claim_sequence)
        {
            return Err(invalid());
        }
    }
    if let Some(seq) = claim.result_sequence {
        if claim.started_sequence.is_none_or(|start| seq <= start) {
            return Err(invalid());
        }
        let event = event_at(db, session, seq).await?;
        if !matches!(event.kind, SessionEventKind::ToolInvocationResultRecorded { record } if record.invocation_id == id)
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Explicit maintenance only: validate canonical identity columns and projected claim evidence
/// against every canonical event in bounded pages before publishing the writer contract.
pub async fn validate_migration_keys(db: &dyn Database) -> SessionDbResult<()> {
    let mut after = None;
    loop {
        let mut query = db
            .select("events")
            .columns(&["event_seq", "payload", "managed_tool_key"]);
        if let Some(after) = after {
            query = query.where_gt("event_seq", sequence(after)?);
        }
        let rows = query
            .sort("event_seq", switchy::database::query::SortDirection::Asc)
            .limit(128)
            .execute(db)
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        for row in rows {
            let event = crate::persisted::decode_session_event(&required_string(&row, "payload")?)?;
            let seq = required_non_negative_u64(&row, "event_seq")?;
            if event.sequence != seq {
                return Err(invalid());
            }
            let key = match row.get("managed_tool_key") {
                Some(DatabaseValue::Null) => None,
                Some(_) => Some(required_string(&row, "managed_tool_key")?),
                None => return Err(invalid()),
            };
            if canonical_key(&event)? != key {
                return Err(invalid());
            }
            if let Some(transition) =
                ManagedToolClaimEvent::from_event(&event).map_err(|_| invalid())?
            {
                let identity = match transition {
                    ManagedToolClaimEvent::Claimed { identity }
                    | ManagedToolClaimEvent::Started { identity, .. } => identity,
                };
                read(db, event.session_id, &identity.invocation_id, None)
                    .await?
                    .ok_or_else(invalid)?;
            }
            after = Some(seq);
        }
    }
}

async fn new_claim(
    db: &dyn Database,
    event: &SessionEvent,
    identity: bcode_session_models::ManagedToolIdentity,
) -> SessionDbResult<ManagedToolClaimRecord> {
    crate::db::harness_operation::verify_managed_tool_owner(db, &identity.owner).await?;
    if canonical_latest(
        db,
        event.session_id,
        &format!("result:{}", identity.invocation_id),
        Some(event.sequence),
    )
    .await?
    .is_some()
        || read(
            db,
            event.session_id,
            &identity.invocation_id,
            Some(event.sequence),
        )
        .await?
        .is_some()
        || db
            .select("tool_runs")
            .columns(&["tool_call_id"])
            .where_eq("tool_call_id", identity.invocation_id.clone())
            .execute_first(db)
            .await?
            .is_some()
    {
        return Err(invalid());
    }
    Ok(ManagedToolClaimRecord {
        identity,
        claim_sequence: event.sequence,
        started_sequence: None,
        result_sequence: None,
    })
}

/// Called exactly once by the canonical projector inside its event transaction.
pub async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    let transition = ManagedToolClaimEvent::from_event(event).map_err(|_| invalid())?;
    let claim = match transition {
        Some(ManagedToolClaimEvent::Claimed { identity }) => new_claim(db, event, identity).await?,
        Some(ManagedToolClaimEvent::Started {
            identity,
            claim_sequence,
        }) => {
            crate::db::harness_operation::verify_managed_tool_owner(db, &identity.owner).await?;
            let mut claim = read(
                db,
                event.session_id,
                &identity.invocation_id,
                Some(event.sequence),
            )
            .await?
            .ok_or_else(invalid)?;
            if claim.identity != identity
                || claim.claim_sequence != claim_sequence
                || claim.started_sequence.is_some()
                || claim.result_sequence.is_some()
            {
                return Err(invalid());
            }
            claim.started_sequence = Some(event.sequence);
            claim
        }
        None => {
            let id = match &event.kind {
                SessionEventKind::ToolInvocationResultRecorded { record } => &record.invocation_id,
                _ => return Ok(()),
            };
            let Some(mut claim) = read(db, event.session_id, id, Some(event.sequence)).await?
            else {
                return Ok(());
            };
            crate::db::harness_operation::verify_managed_tool_owner(db, &claim.identity.owner)
                .await?;
            if claim.started_sequence.is_none() || claim.result_sequence.is_some() {
                return Err(invalid());
            }
            claim.result_sequence = Some(event.sequence);
            claim
        }
    };
    db.upsert("managed_tool_claims")
        .value("invocation_id", claim.identity.invocation_id.clone())
        .value("claim_sequence", sequence(claim.claim_sequence)?)
        .value(
            "started_sequence",
            claim
                .started_sequence
                .map(i64::try_from)
                .transpose()
                .map_err(|_| invalid())?,
        )
        .value(
            "result_sequence",
            claim
                .result_sequence
                .map(i64::try_from)
                .transpose()
                .map_err(|_| invalid())?,
        )
        .value("record", serde_json::to_string(&claim)?)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        ExternalHarnessOwner, ManagedToolIdentity, ToolInvocationLifecycleEvent,
        ToolInvocationLifecycleStage,
    };

    async fn verifier_fixture() -> (tempfile::TempDir, SessionDb, ManagedToolClaimRecord) {
        let root = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, root.path())
            .await
            .unwrap();
        let identity = ManagedToolIdentity {
            owner: ExternalHarnessOwner {
                session_id,
                generation: 1,
                artifact_id: "artifact".into(),
                daemon_id: "daemon".into(),
            },
            grant_id: "grant".into(),
            invocation_id: "invocation".into(),
            producer_plugin_id: "producer".into(),
            tool_name: "tool".into(),
            arguments_sha256: "a".repeat(64),
            preparation_sha256: "b".repeat(64),
            working_directory: root.path().to_str().unwrap().into(),
        };
        // Maintenance-verifier input: insert canonical evidence independently of its projection.
        let event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 0,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolInvocationLifecycle {
                event: ToolInvocationLifecycleEvent {
                    invocation_id: identity.invocation_id.clone(),
                    sequence: 0,
                    stage: ToolInvocationLifecycleStage::Waiting,
                    message: None,
                    metadata: serde_json::json!({
                        (bcode_session_models::MANAGED_TOOL_CLAIM_KEY):
                            ManagedToolClaimEvent::Claimed { identity: identity.clone() }
                    }),
                },
            },
        };
        crate::db_event_store::insert_event(db.database(), &event, None)
            .await
            .unwrap();
        let claim = ManagedToolClaimRecord {
            identity,
            claim_sequence: 0,
            started_sequence: None,
            result_sequence: None,
        };
        db.database()
            .insert("managed_tool_claims")
            .value("invocation_id", claim.identity.invocation_id.clone())
            .value("claim_sequence", 0_i64)
            .value("record", serde_json::to_string(&claim).unwrap())
            .execute(db.database())
            .await
            .unwrap();
        (root, db, claim)
    }

    #[tokio::test]
    async fn migration_verifier_rejects_deleted_claim_projection() {
        let (_root, db, claim) = verifier_fixture().await;
        validate_migration_keys(db.database()).await.unwrap();
        db.database()
            .delete("managed_tool_claims")
            .where_eq("invocation_id", claim.identity.invocation_id)
            .execute(db.database())
            .await
            .unwrap();
        assert!(validate_migration_keys(db.database()).await.is_err());
        db.database().close().await.unwrap();
    }

    #[tokio::test]
    async fn migration_verifier_rejects_omitted_or_wrong_canonical_keys() {
        let (_root, db, _claim) = verifier_fixture().await;
        validate_migration_keys(db.database()).await.unwrap();
        for key in [
            DatabaseValue::Null,
            DatabaseValue::String("claim:unrelated".into()),
        ] {
            db.database()
                .update("events")
                .value("managed_tool_key", key)
                .where_eq("event_seq", 0_i64)
                .execute(db.database())
                .await
                .unwrap();
            assert!(validate_migration_keys(db.database()).await.is_err());
        }
        db.database().close().await.unwrap();
    }
}
