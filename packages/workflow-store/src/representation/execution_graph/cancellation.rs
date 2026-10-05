//! Durable acceptance of exact receipt-backed graph cancellation.
use super::{
    DataType, Database, DatabaseValue, Error, FilterableQuery, TABLE, TransactionMode,
    WorkflowExecutionAuthority, WorkflowExecutionSessionLink, WorkflowRunGraphEditBatch,
    WorkflowRunGraphReconciliation, create_table, encode, validate, verify_execution,
};
use bcode_workflow::WorkflowRunGraphPublicationStatus as Status;
const JOURNAL: &str = "workflow_graph_cancel_publications_v2";

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(JOURNAL)
        .column(super::super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::super::column("targets_json", DataType::Text, false))
        .column(super::super::column("status_json", DataType::Text, false))
        .execute(db)
        .await?;
    Ok(())
}

/// Accept exact staged cancellation, or reconcile the same accepted candidate.
/// Acceptance persists intents, not topology. Only exact cancelled receipts permit publication.
/// # Errors
/// Rejects stale authority/caller, changed candidates, ambiguous preparations and invalid topology.
pub async fn accept_publication_from_execution(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
) -> Result<Status, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    validate(batch, link)?;
    verify_execution(&*tx, owner, link).await?;
    let identity = encode(&(&batch.run_id, &batch.mutation_id))?;
    let staged = tx
        .select(TABLE)
        .where_eq("identity", identity.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    let row = staged.first().ok_or(Error::InvalidAdmission)?;
    if row.get("request_json") != Some(DatabaseValue::String(encode(batch)?))
        || row.get("link_json") != Some(DatabaseValue::String(encode(link)?))
    {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(JOURNAL)
        .where_eq("identity", identity.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    let revision = super::super::graph_pages::revision(&*tx, &batch.run_id).await?;
    let (targets, existing): (Vec<crate::DispatchReceipt>, Option<Status>) =
        if let Some(row) = rows.first() {
            (
                decode(row, "targets_json")?,
                Some(decode(row, "status_json")?),
            )
        } else {
            if revision != batch.expected_revision {
                return Err(Error::InvalidAdmission);
            }
            let targets = targets(&*tx, batch, link).await?;
            super::super::graph_revision::validate_cancellation_candidate(&*tx, batch).await?;
            super::super::safety::ensure_admission_allowed(&*tx, &batch.run_id).await?;
            let pending = Status::Pending {
                expected_revision: batch.expected_revision,
            };
            tx.insert(JOURNAL)
                .value("identity", identity.as_str())
                .value("targets_json", encode(&targets)?)
                .value("status_json", encode(&pending)?)
                .execute(&*tx)
                .await?;
            for receipt in &targets {
                if tx
                    .select("workflow_parallel_cancellations_v2")
                    .where_eq("activation_id", receipt.activation_id.as_str())
                    .limit(1)
                    .execute(&*tx)
                    .await?
                    .is_empty()
                {
                    tx.insert("workflow_parallel_cancellations_v2")
                        .value("activation_id", receipt.activation_id.as_str())
                        .value("run_id", batch.run_id.as_str())
                        .execute(&*tx)
                        .await?;
                }
            }
            tx.commit().await?;
            return Ok(pending);
        };
    if let Some(status @ (Status::Committed { .. } | Status::Conflicted { .. })) = existing {
        return Ok(status);
    }
    let status = reconcile(&*tx, batch, owner, link, revision, targets).await?;
    tx.update(JOURNAL)
        .value("status_json", encode(&status)?)
        .where_eq("identity", identity)
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(status)
}
async fn reconcile(
    tx: &dyn switchy_database::DatabaseTransaction,
    batch: &WorkflowRunGraphEditBatch,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
    revision: u64,
    targets: Vec<crate::DispatchReceipt>,
) -> Result<Status, Error> {
    if revision != batch.expected_revision {
        return Ok(Status::Conflicted {
            expected_revision: batch.expected_revision,
            current_revision: revision,
        });
    }
    let mut pending = false;
    for receipt in targets {
        let rows = tx
            .select("workflow_attempts_v2")
            .where_eq("dispatch_identity", receipt.dispatch_identity.as_str())
            .limit(1)
            .execute(tx)
            .await?;
        let row = rows.first().ok_or(Error::MaintenanceRequired)?;
        if row.get("receipt_json") != Some(DatabaseValue::String(encode(&receipt)?)) {
            return Err(Error::MaintenanceRequired);
        }
        match row.get("outcome_json") {
            Some(DatabaseValue::Null) => pending = true,
            Some(DatabaseValue::String(value)) if value == "\"Cancelled\"" => {}
            _ => {
                return Ok(Status::Conflicted {
                    expected_revision: batch.expected_revision,
                    current_revision: revision,
                });
            }
        }
    }
    if pending {
        Ok(Status::Pending {
            expected_revision: batch.expected_revision,
        })
    } else {
        Ok(Status::Committed {
            revision: super::publication_in(tx, batch, owner, link).await?,
        })
    }
}

fn decode<T: serde::de::DeserializeOwned>(
    row: &switchy_database::Row,
    key: &str,
) -> Result<T, Error> {
    match row.get(key) {
        Some(DatabaseValue::String(value)) => {
            serde_json::from_str(&value).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
async fn targets(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    link: &WorkflowExecutionSessionLink,
) -> Result<Vec<crate::DispatchReceipt>, Error> {
    let mut result = Vec::new();
    for item in &batch.reconciliation {
        let WorkflowRunGraphReconciliation::Cancel { activation_id } = item else {
            continue;
        };
        if activation_id == &link.activation_id {
            return Err(Error::InvalidAdmission);
        }
        let rows = db
            .select("workflow_attempts_v2")
            .where_eq("run_id", batch.run_id.as_str())
            .where_eq("activation_id", activation_id.as_str())
            .sort(
                "attempt_number",
                switchy_database::query::SortDirection::Desc,
            )
            .limit(1)
            .execute(db)
            .await?;
        let row = rows.first().ok_or(Error::InvalidAdmission)?;
        let receipt: crate::DispatchReceipt = decode(row, "receipt_json")?;
        if row.get("outcome_json") != Some(DatabaseValue::Null)
            || receipt.run_id != batch.run_id
            || receipt.activation_id != *activation_id
            || row.get("dispatch_identity")
                != Some(DatabaseValue::String(receipt.dispatch_identity.clone()))
        {
            return Err(Error::InvalidAdmission);
        }
        if super::super::graph::activation_id(db, &batch.run_id, &receipt.node_id)
            .await?
            .as_ref()
            != Some(activation_id)
        {
            return Err(Error::InvalidAdmission);
        }
        result.push(receipt);
    }
    if result.is_empty() {
        return Err(Error::InvalidAdmission);
    }
    Ok(result)
}

pub(in super::super) async fn cancellation_reconciled(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    activation: &str,
) -> Result<bool, Error> {
    let rows = db
        .select(JOURNAL)
        .where_eq("identity", encode(&(&batch.run_id, &batch.mutation_id))?)
        .limit(1)
        .execute(db)
        .await?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let staged = db
        .select(TABLE)
        .where_eq("identity", encode(&(&batch.run_id, &batch.mutation_id))?)
        .limit(1)
        .execute(db)
        .await?;
    if staged.first().and_then(|row| row.get("request_json"))
        != Some(DatabaseValue::String(encode(batch)?))
    {
        return Ok(false);
    }
    let status: Status = decode(row, "status_json")?;
    if !matches!(status, Status::Pending { .. }) {
        return Ok(false);
    }
    let targets: Vec<crate::DispatchReceipt> = decode(row, "targets_json")?;
    let Some(receipt) = targets.iter().find(|r| r.activation_id == activation) else {
        return Ok(false);
    };
    let rows = db
        .select("workflow_attempts_v2")
        .where_eq("dispatch_identity", receipt.dispatch_identity.as_str())
        .limit(1)
        .execute(db)
        .await?;
    Ok(rows.first().is_some_and(|row| {
        row.get("outcome_json") == Some(DatabaseValue::String("\"Cancelled\"".into()))
    }))
}
