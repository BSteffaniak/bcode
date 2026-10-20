#![allow(clippy::significant_drop_tightening)]

//! Transient daemon-owned coordination for long-running worktree creation.

use super::{
    Arc, Duration, ErrorResponse, Notify, PathBuf, ServerState, WorktreeCreateOperation,
    WorktreeCreateRequest,
};

const MAX_WAIT: Duration = Duration::from_secs(30);
const MAX_RETAINED_OPERATIONS: usize = 128;

pub fn operation_not_found_response() -> ErrorResponse {
    ErrorResponse::new(
        "worktree_create_operation_not_found",
        "worktree creation operation is unavailable; daemon operation state is transient and creation will not be retried automatically",
    )
}

async fn publish(
    state: &ServerState,
    operation_id: &str,
    update: impl FnOnce(&mut bcode_worktree_models::WorktreeCreateOperationStatus),
) {
    let mut operations = state.worktree_creations.lock().await;
    let Some(operation) = operations.get_mut(operation_id) else {
        return;
    };
    if operation.status.is_terminal() {
        return;
    }
    update(&mut operation.status);
    operation.status.revision = operation.status.revision.saturating_add(1);
    operation.changed.notify_waiters();
}

fn failure(
    code: &str,
    message: &str,
) -> bcode_worktree_models::application::WorktreeApplicationFailure {
    bcode_worktree_models::application::WorktreeApplicationFailure {
        code: code.into(),
        message: message.into(),
    }
}

/// Admit one transient creation, preserving idempotent and conflicting-duplicate semantics.
///
/// # Errors
/// Rejects empty identities, conflicting duplicates, or exhausted active capacity before spawn.
pub async fn start(
    state: &Arc<ServerState>,
    operation_id: String,
    request: WorktreeCreateRequest,
) -> Result<
    bcode_worktree_models::WorktreeCreateOperationStatus,
    bcode_worktree_models::application::WorktreeApplicationFailure,
> {
    if operation_id.trim().is_empty() {
        return Err(failure(
            "worktree_create_operation_id_required",
            "worktree creation operation id must not be empty",
        ));
    }
    let initial = {
        let mut operations = state.worktree_creations.lock().await;
        if let Some(existing) = operations.get(&operation_id) {
            if existing.request != request {
                return Err(failure(
                    "worktree_create_operation_conflict",
                    "the operation id is already associated with a different request",
                ));
            }
            existing.status.clone()
        } else {
            if operations.len() >= MAX_RETAINED_OPERATIONS {
                let Some(expired) = operations
                    .iter()
                    .find(|(_, operation)| operation.status.is_terminal())
                    .map(|(id, _)| id.clone())
                else {
                    return Err(failure(
                        "worktree_create_coordinator_busy",
                        "too many worktree creation operations are active; wait for one to finish",
                    ));
                };
                operations.remove(&expired);
            }
            let status = bcode_worktree_models::WorktreeCreateOperationStatus {
                operation_id: operation_id.clone(),
                revision: 0,
                state: bcode_worktree_models::WorktreeCreateOperationState::Queued,
                response: None,
                error: None,
            };
            operations.insert(
                operation_id.clone(),
                WorktreeCreateOperation {
                    request: request.clone(),
                    status: status.clone(),
                    changed: Arc::new(Notify::new()),
                },
            );
            let task_state = Arc::clone(state);
            tokio::spawn(async move {
                run(task_state, operation_id, request).await;
            });
            status
        }
    };
    Ok(initial)
}

#[allow(clippy::too_many_lines)]
async fn run(state: Arc<ServerState>, operation_id: String, request: WorktreeCreateRequest) {
    let Ok(_permit) = Arc::clone(&state.worktree_creation_gate)
        .acquire_owned()
        .await
    else {
        fail(
            &state,
            &operation_id,
            "worktree_create_cancelled",
            "worktree creation coordinator stopped",
            None,
        )
        .await;
        return;
    };
    publish(&state, &operation_id, |status| {
        status.state = bcode_worktree_models::WorktreeCreateOperationState::Creating;
    })
    .await;

    let Some(cwd) = request.cwd.clone() else {
        fail(
            &state,
            &operation_id,
            "worktree_cwd_required",
            "worktree requests must include the caller working directory",
            None,
        )
        .await;
        return;
    };
    if let Some(session_id) = request.attach_session_id
        && state.session_has_active_turn(session_id).await
    {
        fail(
            &state,
            &operation_id,
            "session_busy",
            "session has an active model turn",
            None,
        )
        .await;
        return;
    }
    let config_paths = bcode_config::default_config_paths_from(&cwd);
    let Ok(config) = bcode_config::load_config_from_paths(&config_paths) else {
        fail(
            &state,
            &operation_id,
            "worktree_config_failed",
            "worktree configuration is unavailable",
            None,
        )
        .await;
        return;
    };
    let blocking_request = request.clone();
    let blocking_cwd = cwd.clone();
    let created = tokio::task::spawn_blocking(move || {
        bcode_worktree::create_worktree(&config, &blocking_request, &blocking_cwd)
    })
    .await;
    let mut response = match created {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            fail(
                &state,
                &operation_id,
                "worktree_create_command_failed",
                "worktree creation command failed",
                None,
            )
            .await;
            return;
        }
        Err(_) => {
            fail(
                &state,
                &operation_id,
                "worktree_create_task_failed",
                "worktree creation task failed",
                None,
            )
            .await;
            return;
        }
    };
    let created_path = response.path.clone();

    publish(&state, &operation_id, |status| {
        status.state = bcode_worktree_models::WorktreeCreateOperationState::FinalizingSession;
    })
    .await;
    let finalized = async {
        if let Some(session_id) = request.attach_session_id {
            if state.session_has_active_turn(session_id).await {
                return Err("session has an active model turn".to_owned());
            }
            let changed = state
                .sessions
                .change_session_working_directory(session_id, response.path.clone())
                .await
                .map_err(|error| error.to_string())?
                .is_some();
            let session = state
                .sessions
                .session_summary(session_id)
                .await
                .map_err(|error| error.to_string())?;
            if changed {
                state
                    .session_catalog
                    .upsert_native_session(session.clone())
                    .await;
            }
            response.session = Some(session);
        } else if request.new_session {
            let session = state
                .sessions
                .create_session(Some(request.name), response.path.clone())
                .await
                .map_err(|error| error.to_string())?;
            state
                .session_catalog
                .upsert_native_session(session.clone())
                .await;
            response.session = Some(session);
        }
        Ok::<(), String>(())
    }
    .await;
    if finalized.is_err() {
        fail(
            &state,
            &operation_id,
            "worktree_session_finalize_failed",
            "worktree was created but session finalization failed",
            Some(created_path),
        )
        .await;
        return;
    }

    publish(&state, &operation_id, |status| {
        status.state = bcode_worktree_models::WorktreeCreateOperationState::Succeeded;
        status.response = Some(response);
    })
    .await;
}

async fn fail(
    state: &ServerState,
    operation_id: &str,
    code: &str,
    message: &str,
    created_path: Option<PathBuf>,
) {
    publish(state, operation_id, |status| {
        status.state = bcode_worktree_models::WorktreeCreateOperationState::Failed;
        status.error = Some(bcode_worktree_models::WorktreeCreateOperationError {
            code: code.to_owned(),
            message: message.to_owned(),
            created_path,
        });
    })
    .await;
}

/// Read one retained transient operation without retrying unavailable work.
///
/// # Errors
/// Returns an unavailable error when daemon-local operation state is absent.
pub async fn status(
    state: &ServerState,
    operation_id: &str,
) -> Result<
    bcode_worktree_models::WorktreeCreateOperationStatus,
    bcode_worktree_models::application::WorktreeApplicationFailure,
> {
    state
        .worktree_creations
        .lock()
        .await
        .get(operation_id)
        .map(|operation| operation.status.clone())
        .ok_or_else(|| {
            let error = operation_not_found_response();
            failure(&error.code, &error.message)
        })
}

/// Wait for a revision or terminal status with the existing bounded wait policy.
///
/// # Errors
/// Returns unavailable when transient state is absent or expires while waiting.
pub async fn wait(
    state: &ServerState,
    operation_id: &str,
    after_revision: u64,
    timeout_ms: u64,
) -> Result<
    bcode_worktree_models::WorktreeCreateOperationStatus,
    bcode_worktree_models::application::WorktreeApplicationFailure,
> {
    let (snapshot, changed) = {
        let operations = state.worktree_creations.lock().await;
        let Some(operation) = operations.get(operation_id) else {
            let error = operation_not_found_response();
            return Err(failure(&error.code, &error.message));
        };
        (operation.status.clone(), Arc::clone(&operation.changed))
    };
    // Register before the re-read to avoid missing a publish between snapshot and await.
    let notified = changed.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let current = status(state, operation_id).await?;
    if current.revision > after_revision
        || current.is_terminal()
        || current.revision != snapshot.revision
    {
        return Ok(current);
    }
    let duration = Duration::from_millis(timeout_ms).min(MAX_WAIT);
    let _ = tokio::time::timeout(duration, notified).await;
    status(state, operation_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_worktree_models::{WorktreeCreateOperationState, WorktreeCreateOperationStatus};

    fn request() -> WorktreeCreateRequest {
        serde_json::from_value(serde_json::json!({"name":"test"})).unwrap()
    }

    fn retained(operation_id: &str, request: WorktreeCreateRequest) -> WorktreeCreateOperation {
        WorktreeCreateOperation {
            request,
            status: WorktreeCreateOperationStatus {
                operation_id: operation_id.into(),
                revision: 1,
                state: WorktreeCreateOperationState::Queued,
                response: None,
                error: None,
            },
            changed: Arc::new(Notify::new()),
        }
    }

    fn state() -> Arc<ServerState> {
        Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ))
    }

    #[tokio::test]
    async fn duplicate_admission_reuses_retained_snapshot_and_rejects_conflict() {
        let state = state();
        let request = request();
        state
            .worktree_creations
            .lock()
            .await
            .insert("one".into(), retained("one", request.clone()));
        let initial = status(&state, "one").await.unwrap();
        assert_eq!(
            start(&state, "one".into(), request.clone()).await.unwrap(),
            initial
        );
        let mut changed = request;
        changed.name = "different".into();
        assert_eq!(
            start(&state, "one".into(), changed).await.unwrap_err().code,
            "worktree_create_operation_conflict"
        );
        assert_eq!(status(&state, "one").await.unwrap(), initial);
        assert_eq!(
            start(&state, " ".into(), self::request())
                .await
                .unwrap_err()
                .code,
            "worktree_create_operation_id_required"
        );
        assert_eq!(state.worktree_creations.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn unavailable_and_zero_wait_do_not_spawn_or_retry() {
        let state = state();
        assert_eq!(
            wait(&state, "missing", 0, 0).await.unwrap_err().code,
            "worktree_create_operation_not_found"
        );
        assert!(state.worktree_creations.lock().await.is_empty());
        state
            .worktree_creations
            .lock()
            .await
            .insert("one".into(), retained("one", request()));
        let initial = status(&state, "one").await.unwrap();
        assert_eq!(
            wait(&state, "one", initial.revision, 0).await.unwrap(),
            initial
        );
        publish(&state, "one", |snapshot| {
            snapshot.state = WorktreeCreateOperationState::Failed;
        })
        .await;
        let terminal = status(&state, "one").await.unwrap();
        assert!(terminal.is_terminal());
        assert_eq!(
            wait(&state, "one", terminal.revision, 30_000)
                .await
                .unwrap(),
            terminal
        );
        publish(&state, "one", |snapshot| {
            snapshot.state = WorktreeCreateOperationState::Queued;
        })
        .await;
        assert_eq!(status(&state, "one").await.unwrap(), terminal);
    }
}
