#![allow(clippy::significant_drop_tightening)]

//! Transient server-owned coordination for explicit bounded bulk session migration.

use super::*;

const MAX_BULK_MIGRATION_WAIT: Duration = Duration::from_secs(30);
pub const BULK_MIGRATION_PAGE_SIZE: usize = 64;

pub fn operation_not_found(operation_id: &str) -> Response {
    Response::Err(ErrorResponse::new(
        "session_bulk_migration_operation_not_found",
        format!(
            "bulk migration operation {operation_id} is unavailable; aggregate operation state is transient and restart recovery requires explicit re-invocation"
        ),
    ))
}

pub async fn handle_session_bulk_migration_start(
    request_id: u64,
    state: &Arc<ServerState>,
    writer: &SharedWriter,
    request: bcode_ipc::SessionBulkMigrationStartRequest,
) -> Result<(), ServerError> {
    if request
        .after_timestamp_ms
        .zip(request.before_timestamp_ms)
        .is_some_and(|(after, before)| after > before)
    {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "session_bulk_migration_range",
                "after timestamp must not exceed before timestamp",
            )),
        )
        .await;
    }
    if matches!(request.mode, bcode_ipc::SessionBulkMigrationMode::Migrate)
        && request.confirmation.as_deref() != Some(bcode_ipc::SESSION_BULK_MIGRATION_CONFIRMATION)
    {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "session_bulk_migration_confirmation_required",
                format!(
                    "confirmed migration requires --confirm {}",
                    bcode_ipc::SESSION_BULK_MIGRATION_CONFIRMATION
                ),
            )),
        )
        .await;
    }
    state
        .sessions
        .wait_catalog_loaded()
        .await
        .map_err(ServerError::SessionStore)?;
    let operation_id = uuid::Uuid::new_v4().to_string();
    let initial = bcode_ipc::SessionBulkMigrationOperationStatus {
        operation_id: operation_id.clone(),
        revision: 0,
        state: bcode_ipc::SessionBulkMigrationState::Running,
        mode: request.mode,
        selected: 0,
        visited: 0,
        migrated: 0,
        blocked: 0,
        failed: 0,
        current_session_id: None,
        outcomes: Vec::new(),
    };
    state.session_bulk_migrations.lock().await.insert(
        operation_id.clone(),
        SessionBulkMigrationOperation {
            status: initial.clone(),
            cancellation_requested: false,
            changed: Arc::new(Notify::new()),
        },
    );
    let task_state = Arc::clone(state);
    tokio::spawn(async move {
        run_session_bulk_migration(task_state, operation_id, request).await;
    });
    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::SessionBulkMigrationOperation { status: initial }),
    )
    .await
}

async fn publish_status(
    state: &ServerState,
    operation_id: &str,
    update: impl FnOnce(&mut bcode_ipc::SessionBulkMigrationOperationStatus),
) -> bool {
    let mut operations = state.session_bulk_migrations.lock().await;
    let Some(operation) = operations.get_mut(operation_id) else {
        return false;
    };
    update(&mut operation.status);
    operation.status.revision = operation.status.revision.saturating_add(1);
    operation.changed.notify_waiters();
    true
}

async fn cancellation_requested(state: &ServerState, operation_id: &str) -> bool {
    state
        .session_bulk_migrations
        .lock()
        .await
        .get(operation_id)
        .is_none_or(|operation| operation.cancellation_requested)
}

#[allow(clippy::too_many_lines)]
pub async fn run_session_bulk_migration(
    state: Arc<ServerState>,
    operation_id: String,
    request: bcode_ipc::SessionBulkMigrationStartRequest,
) {
    let mut cursor = None;
    loop {
        if cancellation_requested(&state, &operation_id).await {
            let _ = publish_status(&state, &operation_id, |status| {
                status.state = bcode_ipc::SessionBulkMigrationState::Cancelled;
                status.current_session_id = None;
            })
            .await;
            return;
        }
        let page = state
            .sessions
            .session_summaries_page(
                &request.session_ids,
                request.after_timestamp_ms,
                request.before_timestamp_ms,
                cursor,
                BULK_MIGRATION_PAGE_SIZE,
            )
            .await;
        if page.is_empty() {
            break;
        }
        let has_more = page.len() > BULK_MIGRATION_PAGE_SIZE;
        for summary in page.iter().take(BULK_MIGRATION_PAGE_SIZE) {
            cursor = Some((summary.updated_at_ms, summary.id));
            if cancellation_requested(&state, &operation_id).await {
                break;
            }
            let entry = classify_session_compatibility(
                state.sessions.session_store_root().as_deref(),
                summary,
            )
            .await;
            let selected = matches!(
                entry.category,
                bcode_ipc::SessionCompatibilityCategory::MigrationRequired
            );
            if selected {
                let _ = publish_status(&state, &operation_id, |status| {
                    status.selected = status.selected.saturating_add(1);
                    status.current_session_id = Some(summary.id);
                })
                .await;
            }
            let outcome = if selected
                && matches!(request.mode, bcode_ipc::SessionBulkMigrationMode::Migrate)
            {
                migrate_one(&state, summary.id).await
            } else {
                entry
            };
            let _ = publish_status(&state, &operation_id, |status| {
                status.visited = status.visited.saturating_add(1);
                match outcome.category {
                    bcode_ipc::SessionCompatibilityCategory::Ready if selected => {
                        status.migrated = status.migrated.saturating_add(1);
                    }
                    bcode_ipc::SessionCompatibilityCategory::OwnerBlocked
                    | bcode_ipc::SessionCompatibilityCategory::TemporarilyLocked => {
                        status.blocked = status.blocked.saturating_add(1);
                    }
                    bcode_ipc::SessionCompatibilityCategory::RepairRequired
                    | bcode_ipc::SessionCompatibilityCategory::FormatIncompatible
                    | bcode_ipc::SessionCompatibilityCategory::Missing => {
                        status.failed = status.failed.saturating_add(1);
                    }
                    bcode_ipc::SessionCompatibilityCategory::Ready
                    | bcode_ipc::SessionCompatibilityCategory::MigrationRequired => {}
                }
                if status.outcomes.len() < bcode_ipc::MAX_SESSION_BULK_MIGRATION_OUTCOMES
                    && (!matches!(
                        outcome.category,
                        bcode_ipc::SessionCompatibilityCategory::Ready
                    ) || selected)
                {
                    status
                        .outcomes
                        .push(bcode_ipc::SessionBulkMigrationOutcome {
                            session_id: outcome.session_id,
                            category: outcome.category,
                            action: outcome.action,
                            message: outcome.message,
                        });
                }
            })
            .await;
        }
        if !has_more {
            break;
        }
    }
    if cancellation_requested(&state, &operation_id).await {
        let _ = publish_status(&state, &operation_id, |status| {
            status.state = bcode_ipc::SessionBulkMigrationState::Cancelled;
            status.current_session_id = None;
        })
        .await;
        return;
    }
    let _ = publish_status(&state, &operation_id, |status| {
        status.state = if status.blocked > 0 || status.failed > 0 {
            bcode_ipc::SessionBulkMigrationState::NeedsAttention
        } else {
            bcode_ipc::SessionBulkMigrationState::Completed
        };
        status.current_session_id = None;
    })
    .await;
}

async fn migrate_one(
    state: &Arc<ServerState>,
    session_id: SessionId,
) -> bcode_ipc::SessionCompatibilityEntry {
    let source_writer_epoch = match state.sessions.session_health(session_id).await {
        bcode_session::SessionHealth::Migratable { source, .. }
        | bcode_session::SessionHealth::BlockedOwner { source, .. }
        | bcode_session::SessionHealth::WriterIncompatible {
            actual: Some(source),
            ..
        } => u32::try_from(source).ok(),
        _ => None,
    };
    let source =
        current_writer_with_released_historical_events(state, session_id, source_writer_epoch)
            .await;
    let Some(source_writer_epoch) = source else {
        let summary = state.sessions.session_summary(session_id).await;
        return match summary {
            Ok(summary) => {
                classify_session_compatibility(
                    state.sessions.session_store_root().as_deref(),
                    &summary,
                )
                .await
            }
            Err(error) => bcode_ipc::SessionCompatibilityEntry {
                session_id,
                updated_at_ms: 0,
                category: bcode_ipc::SessionCompatibilityCategory::Missing,
                action: bcode_ipc::SessionCompatibilityAction::Locate,
                retryable: false,
                source_writer_epoch: None,
                first_historical_event_schema: None,
                message: Some(error.to_string()),
            },
        };
    };
    if let Err(error) = state.session_migrations.plan(source_writer_epoch) {
        return failed_entry(session_id, error.to_string());
    }
    let initial = migrating_session_open_snapshot(session_id, source_writer_epoch);
    let sessions = state.sessions.clone();
    let operation = state
        .session_migrations
        .operations()
        .start_or_join(initial, move |operation| async move {
            let reporter =
                bcode_session_migration::SessionMigrationProgressReporter::new(operation);
            let result = async {
                let root = sessions
                    .session_store_root()
                    .ok_or(bcode_session::SessionError::NotFound(session_id))?;
                let lease_owner = sessions
                    .session_lease_owner()
                    .ok_or(bcode_session::SessionError::NotFound(session_id))?;
                let lease = session_migration_execution::migrate_owned_session_storage(
                    session_id,
                    &root,
                    u64::from(source_writer_epoch),
                    &reporter,
                    &bcode_metrics::MetricsRegistry::disabled(),
                    &lease_owner,
                )
                .await?;
                sessions.adopt_session_lease(session_id, lease).await?;
                sessions.load_current_session(session_id).await
            }
            .await;
            match result {
                Ok(()) => bcode_session_models::SessionOpenTerminalOutcome::Ready,
                Err(error) => session_migration_failure_outcome(&error),
            }
        })
        .await;
    let mut receiver = operation.subscribe();
    let snapshot = receiver
        .wait_for(|snapshot| snapshot.outcome.is_some())
        .await
        .map_or_else(|_| operation.snapshot(), |snapshot| snapshot.clone());
    match snapshot.outcome {
        Some(bcode_session_models::SessionOpenTerminalOutcome::Ready) => {
            bcode_ipc::SessionCompatibilityEntry {
                session_id,
                updated_at_ms: 0,
                category: bcode_ipc::SessionCompatibilityCategory::Ready,
                action: bcode_ipc::SessionCompatibilityAction::None,
                retryable: false,
                source_writer_epoch: Some(source_writer_epoch),
                first_historical_event_schema: None,
                message: None,
            }
        }
        Some(outcome) => failed_entry(session_id, format!("{outcome:?}")),
        None => failed_entry(
            session_id,
            "migration operation ended without an outcome".to_owned(),
        ),
    }
}

const fn failed_entry(
    session_id: SessionId,
    message: String,
) -> bcode_ipc::SessionCompatibilityEntry {
    bcode_ipc::SessionCompatibilityEntry {
        session_id,
        updated_at_ms: 0,
        category: bcode_ipc::SessionCompatibilityCategory::RepairRequired,
        action: bcode_ipc::SessionCompatibilityAction::Repair,
        retryable: false,
        source_writer_epoch: None,
        first_historical_event_schema: None,
        message: Some(message),
    }
}

pub async fn handle_session_bulk_migration_status(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    operation_id: &str,
) -> Result<(), ServerError> {
    let response = state
        .session_bulk_migrations
        .lock()
        .await
        .get(operation_id)
        .map_or_else(
            || operation_not_found(operation_id),
            |operation| {
                Response::Ok(ResponsePayload::SessionBulkMigrationOperation {
                    status: operation.status.clone(),
                })
            },
        );
    send_response(writer, request_id, response).await
}

pub async fn handle_session_bulk_migration_wait(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    operation_id: &str,
    after_revision: u64,
    timeout_ms: u64,
) -> Result<(), ServerError> {
    let changed = {
        let operations = state.session_bulk_migrations.lock().await;
        let Some(operation) = operations.get(operation_id) else {
            return send_response(writer, request_id, operation_not_found(operation_id)).await;
        };
        if operation.status.revision > after_revision
            || !matches!(
                operation.status.state,
                bcode_ipc::SessionBulkMigrationState::Running
                    | bcode_ipc::SessionBulkMigrationState::CancellationRequested
            )
        {
            return send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::SessionBulkMigrationOperation {
                    status: operation.status.clone(),
                }),
            )
            .await;
        }
        Arc::clone(&operation.changed)
    };
    let timeout = Duration::from_millis(timeout_ms).min(MAX_BULK_MIGRATION_WAIT);
    let _ = tokio::time::timeout(timeout, changed.notified()).await;
    handle_session_bulk_migration_status(request_id, state, writer, operation_id).await
}

pub async fn handle_session_bulk_migration_cancel(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    operation_id: &str,
) -> Result<(), ServerError> {
    let response = {
        let mut operations = state.session_bulk_migrations.lock().await;
        let Some(operation) = operations.get_mut(operation_id) else {
            return send_response(writer, request_id, operation_not_found(operation_id)).await;
        };
        if matches!(
            operation.status.state,
            bcode_ipc::SessionBulkMigrationState::Running
        ) {
            operation.cancellation_requested = true;
            operation.status.state = bcode_ipc::SessionBulkMigrationState::CancellationRequested;
            operation.status.revision = operation.status.revision.saturating_add(1);
            operation.changed.notify_waiters();
        }
        Response::Ok(ResponsePayload::SessionBulkMigrationOperation {
            status: operation.status.clone(),
        })
    };
    send_response(writer, request_id, response).await
}
