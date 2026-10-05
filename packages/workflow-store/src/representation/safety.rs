//! Same-transaction resource reservations and nonterminal cancellation intent.
use super::{RUNS, RepresentationError as Error, column};
use crate::WorkflowExecutionAuthority;
use bcode_workflow::{ResourceAccess, ResourceClaim};
use std::collections::BTreeMap;
use switchy_database::{
    Database, DatabaseTransaction, DatabaseValue, TransactionMode,
    query::FilterableQuery,
    schema::{DataType, create_index, create_table},
};
const CANCEL: &str = "workflow_cancellation_v2";
const LEASES: &str = "workflow_resource_leases_v2";
const HANDOFFS: &str = "workflow_handoffs_v2";
const RECOVERY: &str = "workflow_recovery_barriers_v2";
// Query batch size only; not an admission limit on the number of claims.
const LEASE_PAGE_SIZE: usize = 128;

/// Exact authority transition to be verified by the trusted owner resolver.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuthorityHandoff {
    pub run_id: String,
    pub expected: WorkflowExecutionAuthority,
    pub replacement: WorkflowExecutionAuthority,
}
/// Resolver result. Unknown ownership never permits mutation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum HandoffEvidence {
    Unknown,
    /// Resolver verified cessation of the exact previous authority and replacement capability.
    Verified {
        transition: Box<AuthorityHandoff>,
        receipt_id: String,
        verified_at_ms: u64,
    },
}
/// Trusted application boundary, never implemented by model-provided declarations.
///
/// This is an in-process trusted adapter contract, not a cryptographic proof type.
/// Never expose resolver selection or `Verified` construction through tools, plugins,
/// or deserialized client/model input. The server owns evidence authentication.
/// Implementations must verify owner relinquishment/end and replacement compatibility.
pub trait AuthorityHandoffResolver: Send + Sync {
    /// Resolve exact authority facts; unavailable or ambiguous facts must return Unknown.
    fn verify<'a>(
        &'a self,
        transition: &'a AuthorityHandoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<HandoffEvidence, Error>> + Send + 'a>,
    >;
}

/// Transfer quiescent authority using trusted resolver evidence and an atomic compare-and-swap.
///
/// Does not probe process liveness or enable dispatch. No outstanding attempt may be transferred.
/// # Errors
/// Rejects unknown evidence, stale ownership, invalid generations, ambiguous work and storage failure.
pub async fn handoff_authority(
    db: &dyn Database,
    transition: &AuthorityHandoff,
    resolver: &dyn AuthorityHandoffResolver,
) -> Result<(), Error> {
    handoff_in_mode(db, transition, resolver, false).await
}

/// Transfer authority for reconciliation only, preserving unresolved receipts and effects.
/// # Errors
/// Rejects unknown/live ownership evidence, stale authority, invalid transition or storage failure.
pub async fn handoff_for_recovery(
    db: &dyn Database,
    transition: &AuthorityHandoff,
    resolver: &dyn AuthorityHandoffResolver,
) -> Result<(), Error> {
    handoff_in_mode(db, transition, resolver, true).await
}

async fn handoff_in_mode(
    db: &dyn Database,
    transition: &AuthorityHandoff,
    resolver: &dyn AuthorityHandoffResolver,
    recovery: bool,
) -> Result<(), Error> {
    validate_handoff(transition)?;
    let encoded =
        serde_json::to_string(&(transition, recovery)).map_err(|_| Error::InvalidAdmission)?;
    let (receipt_id, timestamp) = verify_handoff_evidence(transition, resolver).await?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let generation =
        i64::try_from(transition.replacement.generation).map_err(|_| Error::InvalidAdmission)?;
    let prior = tx
        .select(HANDOFFS)
        .where_eq("run_id", transition.run_id.as_str())
        .where_eq("generation", generation)
        .limit(1)
        .execute(&*tx)
        .await?;
    if let Some(prior) = prior.first() {
        if prior.get("transition_json") != Some(DatabaseValue::String(encoded))
            || prior.get("receipt_id") != Some(DatabaseValue::String(receipt_id.clone()))
        {
            return Err(Error::AuthorityConflict);
        }
        tx.commit().await?;
        return Ok(());
    }
    let expected =
        serde_json::to_string(&transition.expected).map_err(|_| Error::InvalidAdmission)?;
    let replacement =
        serde_json::to_string(&transition.replacement).map_err(|_| Error::InvalidAdmission)?;
    let run = tx
        .select(RUNS)
        .where_eq("run_id", transition.run_id.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    if run.first().and_then(|r| r.get("authority_json"))
        != Some(DatabaseValue::String(expected.clone()))
    {
        return Err(Error::AuthorityConflict);
    }
    ensure_nonterminal(run.first())?;
    if recovery {
        let barriers = tx
            .select(RECOVERY)
            .where_eq("run_id", transition.run_id.as_str())
            .limit(1)
            .execute(&*tx)
            .await?;
        if let Some(barrier) = barriers.first() {
            if barrier.get("authority_json") != Some(DatabaseValue::String(expected.clone())) {
                return Err(Error::AuthorityConflict);
            }
            tx.update(RECOVERY)
                .value("authority_json", replacement.clone())
                .where_eq("run_id", transition.run_id.as_str())
                .execute(&*tx)
                .await?;
        } else {
            tx.insert(RECOVERY)
                .value("run_id", transition.run_id.as_str())
                .value("authority_json", replacement.clone())
                .execute(&*tx)
                .await?;
        }
    } else {
        ensure_quiescent(&*tx, &transition.run_id).await?;
        if recovery_required(&*tx, &transition.run_id).await? {
            return Err(Error::AuthorityConflict);
        }
    }
    tx.update(RUNS)
        .value("authority_json", replacement)
        .where_eq("run_id", transition.run_id.as_str())
        .where_eq("authority_json", expected)
        .execute(&*tx)
        .await?;
    tx.insert(HANDOFFS)
        .value("run_id", transition.run_id.as_str())
        .value("generation", generation)
        .value("transition_json", encoded)
        .value("receipt_id", receipt_id.as_str())
        .value("verified_at_ms", timestamp)
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
async fn verify_handoff_evidence(
    transition: &AuthorityHandoff,
    resolver: &dyn AuthorityHandoffResolver,
) -> Result<(String, i64), Error> {
    let HandoffEvidence::Verified {
        transition: verified,
        receipt_id,
        verified_at_ms,
    } = resolver.verify(transition).await?
    else {
        return Err(Error::AuthorityConflict);
    };
    if *verified != *transition {
        return Err(Error::AuthorityConflict);
    }
    crate::validate_id("handoff receipt", &receipt_id).map_err(|_| Error::InvalidAdmission)?;
    Ok((
        receipt_id,
        i64::try_from(verified_at_ms).map_err(|_| Error::InvalidAdmission)?,
    ))
}

/// Read whether a durable recovery barrier prevents new effects. Grants no authority.
/// # Errors
/// Returns database failures; absence is only absence of this barrier, not dispatch permission.
pub async fn recovery_required(db: &dyn Database, run_id: &str) -> Result<bool, Error> {
    Ok(!db
        .select(RECOVERY)
        .columns(&["run_id"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(db)
        .await?
        .is_empty())
}

/// Clear reconciliation-only fencing after all exact canonical attempts have settled.
///
/// Does not settle any attempt or infer the outcome of receiptless ambiguous work.
/// # Errors
/// Rejects stale authority, mismatched barrier ownership, unresolved attempts/resources or damage.
pub async fn finish_recovery(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<(), Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let expected = serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?;
    let rows = tx
        .select(RUNS)
        .columns(&["authority_json"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    if rows.first().and_then(|r| r.get("authority_json"))
        != Some(DatabaseValue::String(expected.clone()))
    {
        return Err(Error::AuthorityConflict);
    }
    let barriers = tx
        .select(RECOVERY)
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    if let Some(barrier) = barriers.first() {
        if barrier.get("authority_json") != Some(DatabaseValue::String(expected)) {
            return Err(Error::AuthorityConflict);
        }
        ensure_quiescent(&*tx, run_id).await?;
        tx.delete(RECOVERY)
            .where_eq("run_id", run_id)
            .execute(&*tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn ensure_nonterminal(row: Option<&switchy_database::Row>) -> Result<(), Error> {
    match row.and_then(|row| row.get("status")) {
        Some(DatabaseValue::String(status)) if status == "paused" => Ok(()),
        Some(DatabaseValue::String(status))
            if matches!(status.as_str(), "completed" | "failed" | "cancelled") =>
        {
            Err(Error::AuthorityConflict)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}

fn validate_handoff(t: &AuthorityHandoff) -> Result<(), Error> {
    for id in [
        &t.run_id,
        &t.expected.target_artifact_id,
        &t.expected.daemon_instance_id,
        &t.expected.fencing_token,
        &t.replacement.target_artifact_id,
        &t.replacement.daemon_instance_id,
        &t.replacement.fencing_token,
    ] {
        crate::validate_id("handoff identity", id).map_err(|_| Error::InvalidAdmission)?;
    }
    if t.expected.generation == 0
        || t.expected.generation.checked_add(1) != Some(t.replacement.generation)
        || t.expected.daemon_instance_id == t.replacement.daemon_instance_id
        || t.expected.fencing_token == t.replacement.fencing_token
    {
        return Err(Error::AuthorityConflict);
    }
    Ok(())
}
async fn ensure_quiescent(tx: &dyn DatabaseTransaction, run_id: &str) -> Result<(), Error> {
    let run = tx
        .select(RUNS)
        .columns(&["active_attempts"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(tx)
        .await?;
    if run.first().and_then(|r| r.get("active_attempts")) != Some(DatabaseValue::Int64(0)) {
        return Err(Error::AuthorityConflict);
    }
    if !tx
        .select("workflow_attempts_v2")
        .columns(&["dispatch_identity"])
        .where_eq("run_id", run_id)
        .where_eq("outcome_json", DatabaseValue::Null)
        .limit(1)
        .execute(tx)
        .await?
        .is_empty()
        || !tx
            .select(LEASES)
            .columns(&["dispatch_identity"])
            .where_eq("run_id", run_id)
            .limit(1)
            .execute(tx)
            .await?
            .is_empty()
    {
        return Err(Error::AuthorityConflict);
    }
    Ok(())
}

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(RECOVERY)
        .column(column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .column(column("authority_json", DataType::Text, false))
        .execute(db)
        .await?;
    create_table(HANDOFFS)
        .column(column("run_id", DataType::Text, false))
        .column(column("generation", DataType::BigInt, false))
        .unique_columns(["run_id", "generation"])
        .column(column("transition_json", DataType::Text, false))
        .column(column("receipt_id", DataType::Text, false))
        .column(column("verified_at_ms", DataType::BigInt, false))
        .execute(db)
        .await?;
    create_table(CANCEL)
        .column(column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .column(column("requested_at_ms", DataType::BigInt, false))
        .execute(db)
        .await?;
    create_table(LEASES)
        .column(column("run_id", DataType::Text, false))
        .column(column("dispatch_identity", DataType::Text, false))
        .column(column("resource_key", DataType::Text, false))
        .column(column("access", DataType::Text, false))
        .unique_columns(["run_id", "dispatch_identity", "resource_key"])
        .execute(db)
        .await?;
    create_index("workflow_resource_leases_v2_conflicts")
        .table(LEASES)
        .columns(vec!["run_id", "resource_key", "access"])
        .execute(db)
        .await?;
    Ok(())
}

/// Persist an idempotent cancellation request under the current execution authority.
///
/// Does not terminalize work, release resources, or assert that effects were undone.
/// # Errors
/// Rejects invalid identities/timestamps, stale authority, unsupported state or persistence failure.
pub async fn request_cancellation(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
    now: u64,
) -> Result<(), Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    let now = i64::try_from(now).map_err(|_| Error::InvalidAdmission)?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let run = tx
        .select(RUNS)
        .columns(&["authority_json", "status"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let expected = serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?;
    if run.first().and_then(|row| row.get("authority_json"))
        != Some(DatabaseValue::String(expected))
    {
        return Err(Error::AuthorityConflict);
    }
    ensure_nonterminal(run.first())?;
    if tx
        .select(CANCEL)
        .columns(&["run_id"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?
        .is_empty()
    {
        tx.insert(CANCEL)
            .value("run_id", run_id)
            .value("requested_at_ms", now)
            .execute(&*tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Read the first durable cancellation request without granting execution authority.
/// # Errors
/// Rejects malformed timestamps, invalid identities or database failures.
pub async fn cancellation_requested_at(
    db: &dyn Database,
    run_id: &str,
) -> Result<Option<u64>, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    let rows = db
        .select(CANCEL)
        .columns(&["requested_at_ms"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(db)
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    match row.get("requested_at_ms") {
        Some(DatabaseValue::Int64(value)) if value >= 0 => Ok(Some(value.cast_unsigned())),
        _ => Err(Error::MaintenanceRequired),
    }
}

pub(super) async fn ensure_admission_allowed(
    tx: &dyn DatabaseTransaction,
    run_id: &str,
) -> Result<(), Error> {
    let blocks = admission_blocks(tx, run_id).await?;
    if blocks.user_paused || blocks.cancelled || blocks.control_failure || blocks.recovery {
        return Err(Error::CapacityUnavailable);
    }
    Ok(())
}
/// Canonical run-level admission barriers, not authorization or terminal outcomes.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct AdmissionBlocks {
    /// Explicit user pause.
    pub user_paused: bool,
    /// Cancellation intent is present.
    pub cancelled: bool,
    /// A control failure prevents admission.
    pub control_failure: bool,
    /// Recovery reconciliation is pending.
    pub recovery: bool,
}
/// Observe the same barriers enforced by attempt admission in the caller's snapshot.
/// # Errors
/// Rejects unsupported storage or database failures.
pub async fn admission_blocks(
    tx: &dyn DatabaseTransaction,
    run_id: &str,
) -> Result<AdmissionBlocks, Error> {
    let mut flags = [false; 3];
    for (flag, table) in flags
        .iter_mut()
        .zip([CANCEL, "workflow_control_failures_v2", RECOVERY])
    {
        *flag = !tx
            .select(table)
            .columns(&["run_id"])
            .where_eq("run_id", run_id)
            .limit(1)
            .execute(tx)
            .await?
            .is_empty();
    }
    Ok(AdmissionBlocks {
        user_paused: super::controls::user_paused(tx, run_id).await?,
        cancelled: flags[0],
        control_failure: flags[1],
        recovery: flags[2],
    })
}
fn normalized(claims: &[ResourceClaim]) -> Result<BTreeMap<String, ResourceAccess>, Error> {
    let mut result = BTreeMap::new();
    for claim in claims {
        crate::validate_id("resource", &claim.resource).map_err(|_| Error::InvalidAdmission)?;
        if claim.resource.trim() != claim.resource {
            return Err(Error::InvalidAdmission);
        }
        result
            .entry(claim.resource.clone())
            .and_modify(|access| {
                if claim.access == ResourceAccess::Write {
                    *access = ResourceAccess::Write;
                }
            })
            .or_insert(claim.access);
    }
    Ok(result)
}
const fn access_name(access: ResourceAccess) -> &'static str {
    match access {
        ResourceAccess::Read => "read",
        ResourceAccess::Write => "write",
    }
}

// Compare sorted pages to the normalized input without collecting persisted leases.
// All pages share the caller's transaction: a conflicting duplicate never fills gaps.
async fn matching_reservation(
    tx: &dyn DatabaseTransaction,
    run_id: &str,
    dispatch_identity: &str,
    claims: &BTreeMap<String, ResourceAccess>,
) -> Result<bool, Error> {
    let mut expected = claims.iter();
    let mut after: Option<String> = None;
    let mut found = false;
    loop {
        let mut query = tx
            .select(LEASES)
            .columns(&["resource_key", "access"])
            .where_eq("run_id", run_id)
            .where_eq("dispatch_identity", dispatch_identity)
            .sort("resource_key", switchy_database::query::SortDirection::Asc)
            .limit(LEASE_PAGE_SIZE);
        if let Some(after) = &after {
            query = query.where_gt("resource_key", after.as_str());
        }
        let page = query.execute(tx).await?;
        let exhausted = page.len() < LEASE_PAGE_SIZE;
        for row in page {
            found = true;
            let Some(DatabaseValue::String(key)) = row.get("resource_key") else {
                return Err(Error::MaintenanceRequired);
            };
            let Some((expected_key, access)) = expected.next() else {
                return Err(Error::InvalidAdmission);
            };
            if &key != expected_key
                || row.get("access") != Some(DatabaseValue::String(access_name(*access).into()))
            {
                return Err(Error::InvalidAdmission);
            }
            after = Some(key);
        }
        if exhausted {
            if found && expected.next().is_some() {
                return Err(Error::InvalidAdmission);
            }
            return Ok(found);
        }
    }
}

/// Caller holds an immediate, authority-verified admission transaction; errors must roll it back.
pub(super) async fn reserve(
    tx: &dyn DatabaseTransaction,
    run_id: &str,
    dispatch_identity: &str,
    claims: &[ResourceClaim],
) -> Result<(), Error> {
    let claims = normalized(claims)?;
    ensure_admission_allowed(tx, run_id).await?;
    if matching_reservation(tx, run_id, dispatch_identity, &claims).await? {
        return Ok(());
    }
    if conflicts_normalized(tx, run_id, &claims).await? {
        return Err(Error::CapacityUnavailable);
    }
    for (key, access) in claims {
        tx.insert(LEASES)
            .value("run_id", run_id)
            .value("dispatch_identity", dispatch_identity)
            .value("resource_key", key)
            .value("access", access_name(access))
            .execute(tx)
            .await?;
    }
    Ok(())
}

/// Report the normalized resource key when its canonical admission predicate conflicts.
/// # Errors
/// Rejects invalid claims or database failures.
pub async fn conflicting_resource(
    db: &dyn Database,
    run: &str,
    claim: &ResourceClaim,
) -> Result<Option<String>, Error> {
    Ok(resource_conflict(db, run, std::slice::from_ref(claim))
        .await?
        .then(|| claim.resource.clone()))
}

/// Read the exact reservation conflict predicate without reserving or granting authority.
/// Scope is the same local run used by admission. Caller supplies a bounded claim set.
/// # Errors
/// Returns malformed claim or database errors.
pub async fn resource_conflict(
    db: &dyn Database,
    run: &str,
    claims: &[ResourceClaim],
) -> Result<bool, Error> {
    conflicts_normalized(db, run, &normalized(claims)?).await
}
async fn conflicts_normalized(
    db: &dyn Database,
    run: &str,
    claims: &BTreeMap<String, ResourceAccess>,
) -> Result<bool, Error> {
    for (key, access) in claims {
        let mut query = db
            .select(LEASES)
            .columns(&["dispatch_identity"])
            .where_eq("run_id", run)
            .where_eq("resource_key", key.as_str())
            .limit(1);
        if *access == ResourceAccess::Read {
            query = query.where_not_eq("access", "read");
        }
        if !query.execute(db).await?.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Release only after authoritative terminal settlement in the same fenced transaction.
pub(super) async fn release(
    tx: &dyn DatabaseTransaction,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<(), Error> {
    tx.delete(LEASES)
        .where_eq("run_id", run_id)
        .where_eq("dispatch_identity", dispatch_identity)
        .execute(tx)
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "safety_tests.rs"]
mod tests;
