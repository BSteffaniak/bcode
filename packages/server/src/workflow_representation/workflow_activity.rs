//! Public authority-neutral workflow inventory and local live progress adaptation.
use super::{ServerError, ServerState, open, retained_store, unavailable};
use bcode_workflow_view_models::{
    SESSION_PROGRESS_VERSION, SessionProgressObservation, SessionProgressSnapshot,
    SessionProgressUnavailableReason, WORKFLOW_ACTIVITY_VERSION, WorkflowActivityAttempt,
    WorkflowActivityChild, WorkflowActivityPage, WorkflowActivityPageRequest,
    WorkflowActivitySection,
};

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};
use switchy_database::{Database, DatabaseTransaction};

struct Scan {
    root: String,
    started: Instant,
    started_at_ms: u64,
    transaction: Box<dyn DatabaseTransaction>,
    _database: switchy_database::rusqlite::RusqliteDatabase,
    inventory: BTreeMap<String, RunCoverage>,
}

#[derive(Default)]
struct RunCoverage {
    attempts_done: bool,
    children_done: bool,
    attempts_after: Option<String>,
    children_after: Option<String>,
    observed_children: BTreeSet<String>,
}

fn inventory_complete(inventory: &BTreeMap<String, RunCoverage>) -> bool {
    !inventory.is_empty()
        && inventory
            .values()
            .all(|coverage| coverage.attempts_done && coverage.children_done)
}

/// Resource-bounded ephemeral read snapshots, shared across recursive run pages.
///
/// The coordinator must call `expire` periodically, even without incoming requests,
/// and `close` when a consumer completes or abandons a scan. Lifetime is absolute:
/// pagination never refreshes it. There is no total graph/page-count ceiling.
pub struct WorkflowActivityScans {
    capacity: NonZeroUsize,
    lifetime: Duration,
    inventory_budget: NonZeroUsize,
    scans: tokio::sync::Mutex<BTreeMap<String, Scan>>,
}

impl std::fmt::Debug for WorkflowActivityScans {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowActivityScans")
            .field("capacity", &self.capacity)
            .field("lifetime", &self.lifetime)
            .field("inventory_budget", &self.inventory_budget)
            .finish_non_exhaustive()
    }
}

impl WorkflowActivityScans {
    /// Release all retained read transactions during shutdown.
    /// # Errors
    /// Returns an error if any rollback fails, after attempting every release.
    pub async fn close_all(&self) -> Result<(), ServerError> {
        let scans = std::mem::take(&mut *self.scans.lock().await);
        let mut failed = false;
        for scan in scans.into_values() {
            failed |= scan.transaction.rollback().await.is_err();
        }
        if failed { Err(unavailable()) } else { Ok(()) }
    }
    /// Create a positive retention/lifetime policy; zero lifetime is rejected.
    /// # Errors
    /// Rejects zero lifetime.
    pub fn new(capacity: NonZeroUsize, lifetime: Duration) -> Result<Self, ServerError> {
        if lifetime.is_zero() {
            return Err(unavailable());
        }
        Self::with_inventory_budget(
            capacity,
            lifetime,
            NonZeroUsize::new(4096).expect("positive inventory budget"),
        )
    }

    /// Set the per-scan retained run-identity budget. Exhaustion fails observation,
    /// never classifies idle or caps workflow execution/graph size.
    /// # Errors
    /// Rejects zero lifetime.
    pub fn with_inventory_budget(
        capacity: NonZeroUsize,
        lifetime: Duration,
        inventory_budget: NonZeroUsize,
    ) -> Result<Self, ServerError> {
        if lifetime.is_zero() {
            return Err(unavailable());
        }
        Ok(Self {
            capacity,
            lifetime,
            inventory_budget,
            scans: tokio::sync::Mutex::new(BTreeMap::new()),
        })
    }

    /// Explicitly release one ephemeral snapshot. Unknown identities are harmless.
    /// # Errors
    /// Returns a normalized error if rollback fails.
    pub async fn close(&self, scan_id: &str) -> Result<(), ServerError> {
        let scan = self.scans.lock().await.remove(scan_id);
        if let Some(scan) = scan {
            scan.transaction
                .rollback()
                .await
                .map_err(|_| unavailable())?;
        }
        Ok(())
    }

    /// Release expired transactions. Polling cleanup does not record progress.
    /// # Errors
    /// Returns a normalized error if rollback fails.
    pub async fn expire(&self) -> Result<(), ServerError> {
        let mut scans = self.scans.lock().await;
        while let Some(id) = scans
            .iter()
            .find(|(_, scan)| scan.started.elapsed() >= self.lifetime)
            .map(|(id, _)| id.clone())
        {
            if let Some(scan) = scans.remove(&id) {
                scan.transaction
                    .rollback()
                    .await
                    .map_err(|_| unavailable())?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    scan_id: String,
    version: u32,
    run_id: String,
    revision: u64,
    section: WorkflowActivitySection,
    after: String,
}

fn decode(request: &WorkflowActivityPageRequest) -> Result<Option<Cursor>, ServerError> {
    if request.version != WORKFLOW_ACTIVITY_VERSION
        || !(1..=100).contains(&request.limit)
        || request.expected_revision == Some(0)
    {
        return Err(unavailable());
    }
    request
        .cursor
        .as_ref()
        .map(|token| {
            if token.len() > 16384 {
                return Err(unavailable());
            }
            let cursor: Cursor =
                serde_json::from_slice(&hex::decode(token).map_err(|_| unavailable())?)
                    .map_err(|_| unavailable())?;
            if request.scan_id.as_deref() != Some(cursor.scan_id.as_str())
                || cursor.version != WORKFLOW_ACTIVITY_VERSION
                || cursor.run_id != request.run_id
                || cursor.section != request.section
                || cursor.revision == 0
                || cursor.after.is_empty()
                || request.expected_revision != Some(cursor.revision)
            {
                return Err(unavailable());
            }
            Ok(cursor)
        })
        .transpose()
}

const fn unknown(session_id: String) -> SessionProgressSnapshot {
    SessionProgressSnapshot {
        version: SESSION_PROGRESS_VERSION,
        session_id,
        observation: SessionProgressObservation::Unavailable {
            reason: SessionProgressUnavailableReason::CoverageUnavailable,
        },
    }
}

/// Observe a bounded page without execution authority, repair or full history replay.
///
/// Inventory pages share a retained read snapshot, including recursively expanded
/// descendants. Live markers are later non-owning observations. Callers must exhaust
/// both sections of every discovered run before snapshot coverage is certified.
/// This endpoint does not certify current subtree idleness.
/// # Errors
/// Rejects unsupported identities, invalid/stale cursors, damaged provenance and storage failure.
async fn start_scan(
    state: &ServerState,
    scans: &WorkflowActivityScans,
    request: &WorkflowActivityPageRequest,
) -> Result<String, ServerError> {
    let scan_id = if let Some(id) = &request.scan_id {
        id.clone()
    } else {
        if request.run_id != request.root_run_id || request.cursor.is_some() {
            return Err(unavailable());
        }
        let retained = retained_store(state).await?;
        let db = open(retained.path(), false)?;
        let transaction = db.begin_transaction().await.map_err(|_| unavailable())?;
        let started = Instant::now();
        let started_at_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| unavailable())?
                .as_millis(),
        )
        .map_err(|_| unavailable())?;
        // First read establishes SQLite's snapshot before exposing its token.
        bcode_workflow_store::representation::graph_pages::revision(
            &*transaction,
            &request.root_run_id,
        )
        .await
        .map_err(|_| unavailable())?;
        let mut retained = scans.scans.lock().await;
        if retained.len() >= scans.capacity.get() {
            let oldest = retained
                .iter()
                .min_by_key(|(_, scan)| scan.started)
                .map(|(id, _)| id.clone())
                .ok_or_else(unavailable)?;
            if let Some(evicted) = retained.remove(&oldest) {
                evicted
                    .transaction
                    .rollback()
                    .await
                    .map_err(|_| unavailable())?;
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        retained.insert(
            id.clone(),
            Scan {
                root: request.root_run_id.clone(),
                started,
                started_at_ms,
                transaction,
                _database: db,
                inventory: BTreeMap::from([(request.root_run_id.clone(), RunCoverage::default())]),
            },
        );
        id
    };
    Ok(scan_id)
}

async fn read_inventory(
    scans: &WorkflowActivityScans,
    request: &WorkflowActivityPageRequest,
    scan_id: &str,
    cursor: Option<&Cursor>,
) -> Result<
    (
        bcode_workflow_store::representation::activity::InventoryPage,
        u64,
        bool,
    ),
    ServerError,
> {
    let section = match request.section {
        WorkflowActivitySection::Attempts => {
            bcode_workflow_store::representation::activity::Section::Attempts
        }
        WorkflowActivitySection::Children => {
            bcode_workflow_store::representation::activity::Section::Children
        }
    };
    let mut retained = scans.scans.lock().await;
    let scan = retained.get_mut(scan_id).ok_or_else(unavailable)?;
    if scan.root != request.root_run_id || scan.started.elapsed() >= scans.lifetime {
        return Err(unavailable());
    }
    if !bcode_workflow_store::representation::activity::in_subtree(
        &*scan.transaction,
        &request.root_run_id,
        &request.run_id,
    )
    .await
    .map_err(|_| unavailable())?
    {
        return Err(unavailable());
    }
    // Coverage describes this retained snapshot only. It does not fence later
    // admission or settlement and must not certify current subtree idleness.
    let coverage = scan
        .inventory
        .get(&request.run_id)
        .ok_or_else(unavailable)?;
    let (done, expected_after) = match request.section {
        WorkflowActivitySection::Attempts => (coverage.attempts_done, &coverage.attempts_after),
        WorkflowActivitySection::Children => (coverage.children_done, &coverage.children_after),
    };
    if done || cursor.as_ref().map(|cursor| &cursor.after) != expected_after.as_ref() {
        return Err(unavailable());
    }
    let snapshot_started_at_ms = scan.started_at_ms;
    let inventory = bcode_workflow_store::representation::activity::page_in(
        &*scan.transaction,
        &request.run_id,
        request.expected_revision,
        section,
        cursor.as_ref().map(|cursor| cursor.after.as_str()),
        request.limit,
    )
    .await
    .map_err(|_| unavailable())?;
    if let Err(error) = advance_coverage(scan, request, &inventory, scans.inventory_budget) {
        let failed = retained.remove(scan_id).ok_or_else(unavailable)?;
        failed
            .transaction
            .rollback()
            .await
            .map_err(|_| unavailable())?;
        return Err(error);
    }
    let subtree_coverage_complete = inventory_complete(&scan.inventory);
    drop(retained);
    Ok((inventory, snapshot_started_at_ms, subtree_coverage_complete))
}

fn advance_coverage(
    scan: &mut Scan,
    request: &WorkflowActivityPageRequest,
    inventory: &bcode_workflow_store::representation::activity::InventoryPage,
    budget: NonZeroUsize,
) -> Result<(), ServerError> {
    let coverage = scan
        .inventory
        .get_mut(&request.run_id)
        .ok_or_else(unavailable)?;
    match request.section {
        WorkflowActivitySection::Attempts => {
            coverage.attempts_done = inventory.next_after.is_none();
            coverage.attempts_after.clone_from(&inventory.next_after);
        }
        WorkflowActivitySection::Children => {
            coverage.children_done = inventory.next_after.is_none();
            coverage.children_after.clone_from(&inventory.next_after);
            for child in &inventory.children {
                if !coverage
                    .observed_children
                    .insert(child.child_run_id.clone())
                {
                    return Err(unavailable());
                }
            }
        }
    }
    let new_children = inventory
        .children
        .iter()
        .filter(|child| !scan.inventory.contains_key(&child.child_run_id))
        .count();
    if scan.inventory.len().saturating_add(new_children) > budget.get() {
        return Err(unavailable());
    }
    for child in &inventory.children {
        if scan.inventory.contains_key(&child.child_run_id) {
            return Err(unavailable());
        }
        scan.inventory
            .insert(child.child_run_id.clone(), RunCoverage::default());
    }
    Ok(())
}

/// Observe recursive workflow inventory through one retained snapshot, without ownership mutation.
/// # Errors
/// Rejects invalid/expired scans, stale cursors, malformed provenance and storage failures.
pub async fn page(
    state: &ServerState,
    scans: &WorkflowActivityScans,
    request: WorkflowActivityPageRequest,
) -> Result<WorkflowActivityPage, ServerError> {
    let cursor = decode(&request)?;
    scans.expire().await?;
    let scan_id = start_scan(state, scans, &request).await?;
    let (inventory, snapshot_started_at_ms, subtree_coverage_complete) =
        read_inventory(scans, &request, &scan_id, cursor.as_ref()).await?;
    let next_cursor = inventory
        .next_after
        .map(|after| {
            serde_json::to_vec(&Cursor {
                scan_id: scan_id.clone(),
                version: WORKFLOW_ACTIVITY_VERSION,
                run_id: request.run_id.clone(),
                revision: inventory.revision,
                section: request.section,
                after,
            })
            .map(hex::encode)
            .map_err(|_| unavailable())
        })
        .transpose()?;
    let mut issues = vec![
        "Complete page inventory is as-of the retained snapshot; live progress is observed later and current subtree idleness is not certified".into(),
    ];
    let mut attempts = Vec::with_capacity(inventory.attempts.len());
    for attempt in inventory.attempts {
        attempts.push(observe_attempt(state, &request.run_id, attempt, &mut issues).await?);
    }
    // Do not publish coverage for a scan evicted or expired during live adaptation.
    let retained = scans.scans.lock().await;
    let scan = retained.get(&scan_id).ok_or_else(unavailable)?;
    let age = scan.started.elapsed();
    if age >= scans.lifetime {
        return Err(unavailable());
    }
    let snapshot_age_ms = u64::try_from(age.as_millis()).map_err(|_| unavailable())?;
    let scan_lifetime_ms = u64::try_from(scans.lifetime.as_millis()).map_err(|_| unavailable())?;
    drop(retained);
    issues.sort();
    issues.dedup();
    Ok(WorkflowActivityPage {
        version: WORKFLOW_ACTIVITY_VERSION,
        scan_id,
        root_run_id: request.root_run_id,
        snapshot_started_at_ms,
        snapshot_age_ms,
        scan_lifetime_ms,
        current_idle_eligible: false,
        run_id: request.run_id,
        revision: inventory.revision,
        section: request.section,
        attempts,
        children: inventory
            .children
            .into_iter()
            .map(|link| WorkflowActivityChild {
                child_run_id: link.child_run_id,
                parent_run_id: link.parent_run_id,
            })
            .collect(),
        inventory_exhausted: next_cursor.is_none(),
        next_cursor,
        subtree_coverage_complete,
        issues,
    })
}

async fn observe_attempt(
    state: &ServerState,
    run_id: &str,
    attempt: bcode_workflow_store::representation::activity::Attempt,
    issues: &mut Vec<String>,
) -> Result<WorkflowActivityAttempt, ServerError> {
    let session_id = attempt.session.as_ref().map(|link| link.session_id.clone());
    let progress_marker = stage_progress(&attempt);
    let source = dispatch_source(attempt.dispatch_owner.as_deref());
    let mut progress = None;
    let observation = if attempt.terminal {
        bcode_workflow_view_models::AttemptObservation::Terminal {
            stage: progress_marker,
        }
    } else {
        match source {
            Some(bcode_workflow_view_models::WorkflowAttemptSource::LinkedSession) => {
                if let Some(link) = attempt.session {
                    let snapshot = observe_linked_session(state, run_id, link).await?;
                    progress = Some(snapshot.clone());
                    bcode_workflow_view_models::AttemptObservation::LinkedSession {
                        progress: snapshot,
                        stage: progress_marker,
                    }
                } else {
                    bcode_workflow_view_models::AttemptObservation::PendingReadiness {
                        expected_source:
                            bcode_workflow_view_models::WorkflowAttemptSource::LinkedSession,
                        stage: progress_marker,
                    }
                }
            }
            Some(bcode_workflow_view_models::WorkflowAttemptSource::RuntimeStage) => {
                if attempt.admitted_at_ms.is_some() {
                    bcode_workflow_view_models::AttemptObservation::RuntimeStage {
                        progress: progress_marker,
                    }
                } else {
                    bcode_workflow_view_models::AttemptObservation::PendingReadiness {
                        expected_source:
                            bcode_workflow_view_models::WorkflowAttemptSource::RuntimeStage,
                        stage: progress_marker,
                    }
                }
            }
            Some(bcode_workflow_view_models::WorkflowAttemptSource::ChildWorkflow) => {
                if let Some(child_run_id) = attempt.child_run_id {
                    bcode_workflow_view_models::AttemptObservation::ChildWorkflow {
                        child_run_id,
                        stage: progress_marker,
                    }
                } else {
                    bcode_workflow_view_models::AttemptObservation::PendingReadiness {
                        expected_source:
                            bcode_workflow_view_models::WorkflowAttemptSource::ChildWorkflow,
                        stage: progress_marker,
                    }
                }
            }
            None => {
                issues.push("Unknown canonical dispatch progress source".into());
                bcode_workflow_view_models::AttemptObservation::Unavailable {
                    reason: bcode_workflow_view_models::WorkflowAttemptUnavailableReason::UnknownDispatchSource,
                }
            }
        }
    };
    Ok(WorkflowActivityAttempt {
        dispatch_identity: attempt.dispatch_identity,
        node_id: attempt.node_id,
        activation_id: attempt.activation_id,
        attempt: attempt.attempt,
        terminal: attempt.terminal,
        session_id,
        observation,
        progress,
    })
}

fn dispatch_source(
    owner: Option<&str>,
) -> Option<bcode_workflow_view_models::WorkflowAttemptSource> {
    use bcode_workflow_view_models::WorkflowAttemptSource;
    match owner? {
        "bcode.server.agent-turn/v1" => Some(WorkflowAttemptSource::LinkedSession),
        bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID => Some(WorkflowAttemptSource::RuntimeStage),
        "bcode.server.workflow-child/v1" => Some(WorkflowAttemptSource::ChildWorkflow),
        _ => None,
    }
}

fn stage_progress(
    attempt: &bcode_workflow_store::representation::activity::Attempt,
) -> bcode_workflow_view_models::WorkflowStageProgress {
    let (sequence, last_progress_at_ms) = attempt.terminal_at_ms.map_or_else(
        || {
            attempt
                .admitted_at_ms
                .map_or((1, attempt.prepared_at_ms), |timestamp| (2, timestamp))
        },
        |timestamp| (3, timestamp),
    );
    bcode_workflow_view_models::WorkflowStageProgress {
        source_id: attempt.dispatch_identity.clone(),
        epoch: attempt.dispatch_identity.clone(),
        sequence,
        last_progress_at_ms,
    }
}

async fn observe_linked_session(
    state: &ServerState,
    run_id: &str,
    link: bcode_workflow_store::WorkflowExecutionSessionLink,
) -> Result<SessionProgressSnapshot, ServerError> {
    let id: bcode_session_models::SessionId = link.session_id.parse().map_err(|_| unavailable())?;
    let summary = state.sessions.session_summary_non_owning(id).await;
    let provenance_verified = summary.as_ref().is_ok_and(|summary| {
        summary.execution.as_ref().is_some_and(|execution| {
            execution.provenance.run_id == run_id
                && execution.provenance.node_id == link.node_id
                && execution.provenance.activation_id.as_deref()
                    == Some(link.activation_id.as_str())
                && execution.provenance.attempt == link.attempt
        })
    });
    let shared_active = summary.is_ok()
        && link.run_id == run_id
        && state
            .runtime_work
            .observes_shared_execution(id, &link)
            .await;
    let shared_verified = if shared_active {
        let retained = super::retained_store(state).await?;
        let db = super::open(retained.path(), false)?;
        super::inspection::shared_parent_link(&db, run_id, &link).await?
    } else {
        false
    };
    let owned = state.sessions.session_is_owned(id).await;
    let resident = state.session_runtimes.lock().await.contains_key(&id);
    Ok(
        if (provenance_verified || shared_verified) && owned && resident {
            state.progress_observations.lock().map_or_else(
                |_| unknown(link.session_id.clone()),
                |registry| registry.snapshot(id),
            )
        } else {
            unknown(link.session_id)
        },
    )
}

#[cfg(test)]
#[path = "workflow_activity_tests.rs"]
mod tests;
