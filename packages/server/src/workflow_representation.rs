//! Application routing for exact workflow identities in the canonical workflow database.
//!
//! This adapter never installs or migrates storage, and never dispatches extension work.
use std::path::Path;

use bcode_workflow_store::representation::{self, runs::RunLookup};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};

use crate::{ServerError, ServerState};

mod backpressure;
pub mod catalog;
pub mod child;
pub mod controls;
pub mod discovery;
pub use driver::advisory_authority_current;
mod driver;
mod fanout;
pub mod gate_views;
pub mod graph;
pub mod inspection;
pub mod inspection_graph;
pub mod outputs;
pub mod owner;
mod parallel;
pub mod plugin;
pub mod read_view;
pub mod recovery;
pub mod scheduling;
pub mod views;

#[cfg(test)]
mod tests;

fn unavailable() -> ServerError {
    bcode_workflow_store::WorkflowStoreError::InvalidData(
        "workflow representation is unavailable or requires maintenance".into(),
    )
    .into()
}

fn unsupported() -> ServerError {
    bcode_workflow_store::WorkflowStoreError::InvalidData(
        "workflow representation does not support this application operation".into(),
    )
    .into()
}

fn open(
    path: &Path,
    writable: bool,
) -> Result<switchy_database::rusqlite::RusqliteDatabase, ServerError> {
    open_sqlite_rusqlite(
        path,
        &SqliteOpenOptions {
            access: if writable {
                SqliteAccess::ReadWrite
            } else {
                SqliteAccess::ReadOnly
            },
            allow_uri: false,
            // Contention is handled by bounded async backpressure, not by blocking
            // the executor that may be driving the transaction holding this lock.
            busy_timeout: std::time::Duration::ZERO,
            foreign_keys: true,
            connections: std::num::NonZeroUsize::MIN,
        },
    )
    .map_err(|_| unavailable())
}

/// Retain the domain's shared maintenance fence independently of server handle replacement.
/// Readiness is checked before reading the path so degraded placeholder stores cannot route.
fn retained_store(state: &ServerState) -> Result<bcode_workflow_store::WorkflowStore, ServerError> {
    state.poll_workflow_initialization(false)?;
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !store.path().is_file() {
        return Err(unavailable());
    }
    // Retain the verified handle's shared maintenance fence without schema traversal.
    Ok(store.retained_handle()?)
}

async fn lookup_at(path: &Path, run_id: &str) -> Result<RunLookup, ServerError> {
    let db = open(path, false)?;
    backpressure::retry(|| representation::runs::lookup(&db, run_id))
        .await
        .map_err(|_| unavailable())
}

/// Exact status routing. Only a verified legacy identity reaches the legacy reader.
pub async fn status(
    state: &ServerState,
    run_id: &str,
) -> Result<Option<bcode_workflow::WorkflowRunSummary>, ServerError> {
    let retained = retained_store(state)?;
    let path = retained.path();
    match lookup_at(path, run_id).await? {
        RunLookup::Legacy => Ok(crate::workflow_operations::run_status(state, run_id)?),
        RunLookup::Missing => Ok(None),
        RunLookup::Available(_) => {
            let db = open(path, false)?;
            backpressure::retry(|| representation::runs::summary(&db, run_id))
                .await
                .map_err(|_| unavailable())
        }
        RunLookup::UnsupportedRepresentation => Err(unsupported()),
        RunLookup::Degraded => Err(unavailable()),
    }
}

/// Guard operations which currently have only a legacy implementation.
pub async fn require_legacy(state: &ServerState, run_id: &str) -> Result<(), ServerError> {
    let retained = retained_store(state)?;
    match lookup_at(retained.path(), run_id).await? {
        RunLookup::Legacy | RunLookup::Missing => Ok(()),
        RunLookup::Available(_) | RunLookup::UnsupportedRepresentation => Err(unsupported()),
        RunLookup::Degraded => Err(unavailable()),
    }
}

async fn detach_at(path: &Path, run_id: &str) -> Result<bool, ServerError> {
    let db = open(path, true)?;
    let association = backpressure::retry(|| representation::runs::association(&db, run_id))
        .await
        .map_err(|_| unavailable())?;
    let Some(session) = association else {
        return Ok(false);
    };
    backpressure::retry(|| representation::runs::detach(&db, run_id, &session))
        .await
        .map_err(|_| unavailable())
}

/// Return `None` only for legacy routing. Detachment never obtains execution authority.
/// Authorization is enforced by the invoking application operation, not this store adapter.
pub async fn detach(state: &ServerState, run_id: &str) -> Result<Option<bool>, ServerError> {
    let retained = retained_store(state)?;
    let path = retained.path();
    match lookup_at(path, run_id).await? {
        RunLookup::Legacy => Ok(None),
        RunLookup::Missing => Ok(Some(false)),
        RunLookup::Available(_) | RunLookup::UnsupportedRepresentation => {
            detach_at(path, run_id).await.map(Some)
        }
        RunLookup::Degraded => Err(unavailable()),
    }
}

/// Select the canonical family at the existing authorized admission boundary.
/// Historical stores stay unchanged; unsupported executable semantics fail before installation.
pub async fn admit_prepared(
    state: &ServerState,
    run: &bcode_workflow_store::NewWorkflowRun,
) -> Result<Option<bcode_workflow::WorkflowRunSummary>, ServerError> {
    use representation::AdmissionFamily;
    let retained = retained_store(state)?;
    let db = open(retained.path(), false)?;
    match backpressure::retry(|| representation::admission_family(&db))
        .await
        .map_err(|_| unavailable())?
    {
        AdmissionFamily::Historical43 => return Ok(None),
        AdmissionFamily::Baseline42Installable | AdmissionFamily::Installed => {}
    }
    // Existing identities keep their canonical family. New supported runs, numeric
    // or uncapped, use the typed representation without rewriting historical stores.
    match backpressure::retry(|| representation::runs::lookup(&db, &run.run_id))
        .await
        .map_err(|_| unavailable())?
    {
        RunLookup::Legacy => return Ok(None),
        RunLookup::Degraded => return Err(unavailable()),
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Available(_) | RunLookup::Missing => {}
    }
    let stored = retained
        .definition(&run.definition_id, run.definition_version)?
        .ok_or_else(unavailable)?;
    let definition: bcode_workflow::WorkflowDefinition =
        serde_json::from_str(&stored.definition_json).map_err(|_| unavailable())?;
    if representation::definition_dispatch_availability(&definition).map_err(|_| unsupported())?
        != representation::DispatchAvailability::AgentControlLifecycle
    {
        return Err(unsupported());
    }
    drop(db);
    let db = open(retained.path(), true)?;
    backpressure::retry(|| representation::install(&db))
        .await
        .map_err(|_| unavailable())?;
    backpressure::retry(|| representation::journal::install(&db))
        .await
        .map_err(|_| unavailable())?;
    let capture_observations =
        state.observability.persist_model_requests || state.observability.persist_tool_io;
    backpressure::retry(|| {
        representation::admit_registered_observed(&db, run, &definition, capture_observations)
    })
    .await
    .map_err(|_| unavailable())?;
    backpressure::retry(|| representation::runs::summary(&db, &run.run_id))
        .await
        .map_err(|_| unavailable())
}

/// Route an existing exact run without admitting new work or claiming unknown authority.
pub async fn drive_if_represented(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<bool, ServerError> {
    let retained = retained_store(state)?;
    match lookup_at(retained.path(), run_id).await? {
        RunLookup::Legacy | RunLookup::Missing => Ok(false),
        RunLookup::Available(run) if run.state != representation::runs::RunState::Paused => {
            Ok(true)
        }
        RunLookup::Available(_) => {
            let db = open(retained.path(), false)?;
            let (_, definition) =
                backpressure::retry(|| representation::graph_revision::current(&db, run_id))
                    .await
                    .map_err(|_| unavailable())?;
            if representation::definition_dispatch_availability(&definition)
                .map_err(|_| unsupported())?
                != representation::DispatchAvailability::AgentControlLifecycle
            {
                return Err(unsupported());
            }
            Box::pin(driver::drive(state, run_id)).await?;
            Ok(true)
        }
        RunLookup::UnsupportedRepresentation => Err(unsupported()),
        RunLookup::Degraded => Err(unavailable()),
    }
}
