//! Public current-inventory adapter; payloads and boundaries share one transaction.
use super::{EVENTS, generation, installation, text};
use crate::representation::{Database, DatabaseValue, RepresentationError as Error};
use bcode_workflow::{
    WorkflowInventoryCoverage as Coverage, WorkflowInventoryCursor as Cursor,
    WorkflowInventoryDiagnosticApplicability as Diagnostic, WorkflowInventoryEntry as Entry,
    WorkflowInventoryKey as Key, WorkflowInventoryPage as Page, WorkflowInventoryQuery as Query,
    WorkflowInventorySessionLink as Session, WorkflowInventorySource as Source,
    WorkflowInventoryUnavailableReason as Reason,
};
use switchy_database::query::{FilterableQuery, SortDirection};
const INSTALLATION: &str = "workflow_inventory_v1_scalar_observers_2";
const fn sources(source: Source) -> &'static [&'static str] {
    match source {
        Source::Outputs => &["workflow_outputs_v2"],
        Source::AttemptContributors => &[
            "workflow_attempts_v2",
            "workflow_execution_sessions_v2",
            "workflow_activations_v2",
            "workflow_graph_revisions_v2",
        ],
        Source::ChildRuns => &["workflow_lineage_v2"],
    }
}
async fn source_generation(db: &dyn Database, run: &str, source: Source) -> Result<u64, Error> {
    let mut maximum = 0;
    for table in sources(source) {
        let rows = db
            .select(EVENTS)
            .columns(&["sequence"])
            .where_eq("run_id", run)
            .where_eq("source_table", *table)
            .sort("sequence", SortDirection::Desc)
            .limit(1)
            .execute(db)
            .await?;
        if let Some(row) = rows.first() {
            let Some(DatabaseValue::Int64(n)) = row.get("sequence") else {
                return Err(Error::MaintenanceRequired);
            };
            maximum = maximum.max(u64::try_from(n).map_err(|_| Error::MaintenanceRequired)?);
        }
    }
    Ok(maximum)
}
fn after(key: &Key) -> &str {
    match key {
        Key::Output(id) | Key::Dispatch(id) | Key::ChildRun(id) => id,
    }
}
/// Read atomic, source-generation-fenced public inventory facts.
///
/// # Errors
/// Rejects invalid requests and stale pins. Missing or inconsistent installation
/// produces explicit unavailable coverage, never a complete empty inventory.
pub async fn page(db: &dyn Database, query: &Query) -> Result<Page, Error> {
    query.validate().map_err(|_| Error::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    crate::representation::verified(&*tx).await?;
    let revision = crate::representation::graph_pages::revision(&*tx, &query.run_id).await?;
    let installed = tx.get_table_info(super::CONTRACT).await?.is_some();
    let reason = if !installed {
        Some(Reason::MissingInstallation)
    } else if installation::verify(&*tx).await.is_err()
        || generation(&*tx, &query.run_id).await.is_err()
    {
        Some(Reason::Inconsistent)
    } else {
        None
    };
    let generation = if reason.is_none() {
        source_generation(&*tx, &query.run_id, query.source).await?
    } else {
        0
    };
    let cursor = query.cursor.clone().unwrap_or_else(|| Cursor {
        version: query.version,
        run_id: query.run_id.clone(),
        source: query.source,
        installation_id: INSTALLATION.into(),
        graph_revision: revision,
        source_generation: generation,
        after: None,
    });
    if cursor.installation_id != INSTALLATION
        || cursor.graph_revision != revision
        || (reason.is_none() && cursor.source_generation != generation)
    {
        return Err(Error::InventoryStalePage);
    }
    if let Some(reason) = reason {
        tx.commit().await?;
        return Ok(Page {
            cursor,
            coverage: Coverage::Unavailable { reason },
            entries: vec![],
            next_cursor: None,
        });
    }
    let (table, run_column, key_column) = match query.source {
        Source::Outputs => ("workflow_outputs_v2", "run_id", "output_id"),
        Source::AttemptContributors => ("workflow_attempts_v2", "run_id", "dispatch_identity"),
        Source::ChildRuns => ("workflow_lineage_v2", "parent_run_id", "run_id"),
    };
    let selected = [key_column];
    let mut select = tx
        .select(table)
        .columns(&selected)
        .where_eq(run_column, query.run_id.as_str())
        .sort(key_column, SortDirection::Asc)
        .limit(query.limit + 1);
    if let Some(key) = &cursor.after {
        select = select.where_gt(key_column, after(key));
    }
    let mut rows = select.execute(&*tx).await?;
    let more = rows.len() > query.limit;
    rows.truncate(query.limit);
    let mut entries = Vec::new();
    for row in rows {
        let id = text(&row, key_column)?;
        let previous = entries
            .last()
            .map(Entry::key)
            .or_else(|| cursor.after.clone());
        entries.push(entry(&*tx, query, id, previous.as_ref()).await?);
    }
    let next_cursor = if more {
        Some(Cursor {
            after: entries.last().map(Entry::key),
            ..cursor.clone()
        })
    } else {
        None
    };
    let page = Page {
        cursor,
        coverage: Coverage::CurrentInventoryComplete,
        entries,
        next_cursor,
    };
    query
        .validate_page(&page)
        .map_err(|_| Error::MaintenanceRequired)?;
    tx.commit().await?;
    Ok(page)
}

async fn entry(
    tx: &dyn switchy_database::DatabaseTransaction,
    query: &Query,
    id: String,
    previous: Option<&Key>,
) -> Result<Entry, Error> {
    Ok(match query.source {
        Source::Outputs => super::super::graph::completion::proven_output(tx, &query.run_id, &id)
            .await?
            .map_or(
                Entry::OutputUnavailable {
                    output_id: id,
                    reason: Reason::MissingProof,
                },
                |output| Entry::Output { output },
            ),
        Source::AttemptContributors => {
            let attempts = crate::representation::inspection_attempts::page_in(
                tx,
                &query.run_id,
                previous.map(after),
                1,
            )
            .await?;
            let attempt = attempts
                .into_iter()
                .next()
                .ok_or(Error::MaintenanceRequired)?;
            if attempt.dispatch_identity != id {
                return Err(Error::MaintenanceRequired);
            }
            let links = tx
                .select("workflow_execution_sessions_v2")
                .columns(&["link_json"])
                .where_eq("dispatch_identity", id.as_str())
                .limit(2)
                .execute(tx)
                .await?;
            let session = match links.as_slice() {
                [] => missing_session(tx, &query.run_id, &attempt).await?,
                [row] => {
                    let Some(DatabaseValue::String(json)) = row.get("link_json") else {
                        return Err(Error::MaintenanceRequired);
                    };
                    if json.len() > crate::representation::MAX_ADMISSION_BYTES {
                        return Err(Error::MaintenanceRequired);
                    }
                    let link: crate::WorkflowExecutionSessionLink =
                        serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
                    crate::validate_execution_session_link(&link)
                        .map_err(|_| Error::MaintenanceRequired)?;
                    if link.run_id != query.run_id
                        || link.node_id != attempt.node_id
                        || link.activation_id != attempt.activation_id
                        || link.attempt != attempt.attempt
                        || crate::dispatch_identity(
                            &link.run_id,
                            &link.node_id,
                            &link.activation_id,
                            link.attempt,
                        ) != id
                    {
                        return Err(Error::MaintenanceRequired);
                    }
                    Session::Verified {
                        session_id: link.session_id,
                        workspace_snapshot: link.workspace_snapshot,
                        created_at_ms: link.created_at_ms,
                    }
                }
                _ => return Err(Error::MaintenanceRequired),
            };
            Entry::AttemptContributor {
                attempt,
                session,
                diagnostic_applicability: Diagnostic::Unsupported,
            }
        }
        Source::ChildRuns => child_entry(tx, query, id).await?,
    })
}

async fn missing_session(
    db: &dyn Database,
    run: &str,
    attempt: &bcode_workflow::AttemptSummary,
) -> Result<Session, Error> {
    let revision = super::super::graph_pages::revision(db, run).await?;
    let node = super::super::graph_pages::node(db, run, revision, &attempt.node_id).await?;
    // Only deterministic built-in controls have an authoritative no-session capability.
    // Task/plugin/call operations may dispatch work; absent receipt/link does not prove
    // preadmission rejection, including failed/cancelled attempts.
    if node.is_some_and(|node| {
        matches!(
            node.kind,
            bcode_workflow::NodeKind::Branch
                | bcode_workflow::NodeKind::Repeat
                | bcode_workflow::NodeKind::Parallel
                | bcode_workflow::NodeKind::FanOut
                | bcode_workflow::NodeKind::Input
                | bcode_workflow::NodeKind::Approval
        )
    }) {
        return Ok(Session::NotApplicable {
            reason:
                bcode_workflow::WorkflowInventorySessionNotApplicableReason::CanonicalNonSessionNode,
        });
    }
    Ok(Session::Unavailable {
        reason: Reason::MissingProof,
    })
}

async fn child_entry(
    tx: &dyn switchy_database::DatabaseTransaction,
    query: &Query,
    id: String,
) -> Result<Entry, Error> {
    let rows = tx
        .select("workflow_lineage_v2")
        .columns(&[
            "root_run_id",
            "parent_node_id",
            "parent_activation_id",
            "parent_attempt",
        ])
        .where_eq("run_id", id.as_str())
        .where_eq("parent_run_id", query.run_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    let row = rows.first().ok_or(Error::MaintenanceRequired)?;
    let Some(DatabaseValue::Int64(number)) = row.get("parent_attempt") else {
        return Err(Error::MaintenanceRequired);
    };
    Ok(Entry::ChildRun {
        child_run_id: id,
        root_run_id: text(row, "root_run_id")?,
        parent: bcode_workflow::WorkflowHistoryAttemptCorrelation {
            run_id: query.run_id.clone(),
            node_id: text(row, "parent_node_id")?,
            activation_id: text(row, "parent_activation_id")?,
            attempt: u32::try_from(number).map_err(|_| Error::MaintenanceRequired)?,
        },
    })
}
