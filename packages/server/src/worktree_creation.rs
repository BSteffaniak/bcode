#![allow(clippy::significant_drop_tightening)]

//! Transient daemon-owned coordination for long-running worktree creation.

use super::{
    Arc, Duration, ErrorResponse, Notify, PathBuf, Response, ResponsePayload, ServerError,
    ServerState, SharedWriter, WorktreeCreateOperation, WorktreeCreateRequest, send_response,
};

const MAX_WAIT: Duration = Duration::from_secs(30);
const MAX_RETAINED_OPERATIONS: usize = 128;

fn operation_not_found(operation_id: &str) -> Response {
    Response::Err(ErrorResponse::new(
        "worktree_create_operation_not_found",
        format!(
            "worktree creation operation {operation_id} is unavailable; daemon operation state is transient and creation will not be retried automatically"
        ),
    ))
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

pub async fn handle_start(
    request_id: u64,
    state: Arc<ServerState>,
    writer: &SharedWriter,
    operation_id: String,
    request: WorktreeCreateRequest,
) -> Result<(), ServerError> {
    if operation_id.trim().is_empty() {
        return send_response(
            writer,
            request_id,
            Response::Err(ErrorResponse::new(
                "worktree_create_operation_id_required",
                "worktree creation operation id must not be empty",
            )),
        )
        .await;
    }

    let initial = {
        let mut operations = state.worktree_creations.lock().await;
        if let Some(existing) = operations.get(&operation_id) {
            if existing.request != request {
                return send_response(
                    writer,
                    request_id,
                    Response::Err(ErrorResponse::new(
                        "worktree_create_operation_conflict",
                        "the operation id is already associated with a different request",
                    )),
                )
                .await;
            }
            existing.status.clone()
        } else {
            if operations.len() >= MAX_RETAINED_OPERATIONS {
                let Some(expired) = operations
                    .iter()
                    .find(|(_, operation)| operation.status.is_terminal())
                    .map(|(id, _)| id.clone())
                else {
                    drop(operations);
                    return send_response(
                        writer,
                        request_id,
                        Response::Err(ErrorResponse::new(
                            "worktree_create_coordinator_busy",
                            "too many worktree creation operations are active; wait for one to finish",
                        )),
                    )
                    .await;
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
            let task_state = Arc::clone(&state);
            tokio::spawn(async move {
                run(task_state, operation_id, request).await;
            });
            status
        }
    };

    send_response(
        writer,
        request_id,
        Response::Ok(ResponsePayload::WorktreeCreateOperation { status: initial }),
    )
    .await
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
            &format!("session has an active model turn: {session_id}"),
            None,
        )
        .await;
        return;
    }
    let config_paths = bcode_config::default_config_paths_from(&cwd);
    let config = match bcode_config::load_config_from_paths(&config_paths) {
        Ok(config) => config,
        Err(error) => {
            fail(
                &state,
                &operation_id,
                "worktree_config_failed",
                &error.to_string(),
                None,
            )
            .await;
            return;
        }
    };
    let blocking_request = request.clone();
    let blocking_cwd = cwd.clone();
    let created = tokio::task::spawn_blocking(move || {
        bcode_worktree::create_worktree(&config, &blocking_request, &blocking_cwd)
    })
    .await;
    let mut response = match created {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            fail(
                &state,
                &operation_id,
                "worktree_create_command_failed",
                &error.to_string(),
                None,
            )
            .await;
            return;
        }
        Err(error) => {
            fail(
                &state,
                &operation_id,
                "worktree_create_task_failed",
                &error.to_string(),
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
                return Err(format!("session has an active model turn: {session_id}"));
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
    if let Err(error) = finalized {
        fail(
            &state,
            &operation_id,
            "worktree_session_finalize_failed",
            &error,
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

pub async fn handle_status(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    operation_id: &str,
) -> Result<(), ServerError> {
    let status = state
        .worktree_creations
        .lock()
        .await
        .get(operation_id)
        .map(|operation| operation.status.clone());
    match status {
        Some(status) => {
            send_response(
                writer,
                request_id,
                Response::Ok(ResponsePayload::WorktreeCreateOperation { status }),
            )
            .await
        }
        None => send_response(writer, request_id, operation_not_found(operation_id)).await,
    }
}

pub async fn handle_wait(
    request_id: u64,
    state: &ServerState,
    writer: &SharedWriter,
    operation_id: &str,
    after_revision: u64,
    timeout_ms: u64,
) -> Result<(), ServerError> {
    let (status, changed) = {
        let operations = state.worktree_creations.lock().await;
        let Some(operation) = operations.get(operation_id) else {
            return send_response(writer, request_id, operation_not_found(operation_id)).await;
        };
        (operation.status.clone(), Arc::clone(&operation.changed))
    };
    if status.revision > after_revision || status.is_terminal() {
        return send_response(
            writer,
            request_id,
            Response::Ok(ResponsePayload::WorktreeCreateOperation { status }),
        )
        .await;
    }
    let wait = Duration::from_millis(timeout_ms).min(MAX_WAIT);
    let notified = changed.notified();
    let _ = tokio::time::timeout(wait, notified).await;
    handle_status(request_id, state, writer, operation_id).await
}
