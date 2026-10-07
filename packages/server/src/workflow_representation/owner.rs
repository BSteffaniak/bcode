//! Canonical storage routing used by the existing prompt execution owner.
use super::{
    RunLookup, ServerError, ServerState, lookup_at, open, representation, retained_store,
    unavailable, unsupported,
};
use bcode_workflow_store::{
    PreparedActivationDispatch, WorkflowExecutionAuthority, WorkflowExecutionSessionLink,
    WorkflowStoreError,
};
use representation::attempts::owner_boundary;

fn store_error(_: impl std::fmt::Display) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData("workflow owner storage is unavailable or inconsistent".into())
}

/// Check a prepared handoff in its canonical representation before external effects.
pub async fn check_owner_admission(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    authority: &WorkflowExecutionAuthority,
) -> Result<Option<serde_json::Value>, WorkflowStoreError> {
    let retained = retained_store(state).map_err(store_error)?;
    match lookup_at(retained.path(), &request.activation.run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => retained.check_owner_admission(request, authority),
        RunLookup::Available(_) => {
            let db = open(retained.path(), true).map_err(store_error)?;
            super::backpressure::retry(|| {
                owner_boundary::check_owner_admission(&db, request, authority)
            })
            .await
            .map_err(store_error)
        }
        _ => Err(store_error("unsupported owner storage")),
    }
}

/// Read the exact execution-session link without inferring authority from discovery.
pub async fn execution_session_link(
    state: &ServerState,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    attempt: u32,
) -> Result<Option<WorkflowExecutionSessionLink>, WorkflowStoreError> {
    let retained = retained_store(state).map_err(store_error)?;
    match lookup_at(retained.path(), run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Missing => Ok(None),
        RunLookup::Legacy => {
            retained.execution_session_link(run_id, node_id, activation_id, attempt)
        }
        RunLookup::Available(_) => {
            let db = open(retained.path(), false).map_err(store_error)?;
            super::backpressure::retry(|| {
                owner_boundary::execution_session_link(&db, run_id, node_id, activation_id, attempt)
            })
            .await
            .map_err(store_error)
        }
        _ => Err(store_error("unsupported owner storage")),
    }
}

/// Persist the execution-session identity under the same representation and authority.
pub async fn link_execution_session(
    state: &ServerState,
    authority: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
) -> Result<(), WorkflowStoreError> {
    let mut retained = retained_store(state).map_err(store_error)?;
    match lookup_at(retained.path(), &link.run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => {
            retained.verify_execution_authority(&link.run_id, authority)?;
            retained.link_execution_session(link)?;
            Ok(())
        }
        RunLookup::Available(_) => {
            let queue = super::backpressure::write_queue(retained.path()).map_err(store_error)?;
            let _write = queue.lock().await;
            let db = open(retained.path(), true).map_err(store_error)?;
            super::backpressure::retry(|| {
                owner_boundary::persist_execution_session_link(&db, authority, link)
            })
            .await
            .map_err(store_error)
        }
        _ => Err(store_error("unsupported owner storage")),
    }
}

/// Commit the owner receipt without switching canonical representations on failure.
pub async fn persist_receipt(
    state: &ServerState,
    receipt: &bcode_workflow_store::DispatchReceipt,
    authority: &WorkflowExecutionAuthority,
) -> Result<(), WorkflowStoreError> {
    let retained = retained_store(state).map_err(store_error)?;
    match lookup_at(retained.path(), &receipt.run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => retained.persist_owned_dispatch_receipt(receipt, authority),
        RunLookup::Available(_) => {
            let queue = super::backpressure::write_queue(retained.path()).map_err(store_error)?;
            let _write = queue.lock().await;
            let db = open(retained.path(), true).map_err(store_error)?;
            if let Some(existing) = super::backpressure::retry(|| {
                owner_boundary::receipt(&db, &receipt.dispatch_identity)
            })
            .await
            .map_err(store_error)?
            {
                // Callback observation time is not a second admission timestamp. Retain
                // the first canonical receipt only when every owner identity/payload agrees.
                if existing.run_id != receipt.run_id
                    || existing.node_id != receipt.node_id
                    || existing.activation_id != receipt.activation_id
                    || existing.attempt != receipt.attempt
                    || existing.receipt != receipt.receipt
                {
                    return Err(store_error("conflicting owner receipt"));
                }
                return super::backpressure::retry(|| {
                    representation::attempts::attach_receipt(&db, authority, &existing)
                })
                .await
                .map_err(store_error);
            }
            super::backpressure::retry(|| {
                representation::attempts::attach_receipt(&db, authority, receipt)
            })
            .await
            .map_err(store_error)
        }
        _ => Err(store_error("unsupported owner storage")),
    }
}

/// Resolve only an already-recorded local owner; foreign/unclaimed extension work fails closed.
/// Returns `None` for the legacy authority resolver, never as permission to claim new work.
pub async fn local_authority(
    state: &ServerState,
    run_id: &str,
) -> Result<Option<crate::workflow_operations::AuthorityGuard>, ServerError> {
    recorded_authority(state, run_id, true).await
}

/// Verify only an already-local owner without claiming or taking over foreign work.
///
/// # Errors
/// Returns an error for incompatible state, invalid associations or stale authority.
/// Foreign or unclaimed authority returns `None` without durable mutation.
pub async fn settlement_authority(
    state: &ServerState,
    run_id: &str,
) -> Result<Option<crate::workflow_operations::AuthorityGuard>, ServerError> {
    recorded_authority(state, run_id, false).await
}

async fn recorded_authority(
    state: &ServerState,
    run_id: &str,
    allow_takeover: bool,
) -> Result<Option<crate::workflow_operations::AuthorityGuard>, ServerError> {
    let retained = retained_store(state)?;
    match lookup_at(retained.path(), run_id).await? {
        RunLookup::Legacy | RunLookup::Missing => return Ok(None),
        RunLookup::Available(_) => {}
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Degraded => return Err(unavailable()),
    }
    let db = open(retained.path(), false)?;
    let Some(authority) =
        super::backpressure::retry(|| owner_boundary::execution_authority(&db, run_id))
            .await
            .map_err(|_| unavailable())?
    else {
        return if allow_takeover {
            Err(unsupported())
        } else {
            Ok(None)
        };
    };
    if !crate::workflow_operations::authority_targets_current_daemon(state, &authority) {
        if !allow_takeover {
            return Ok(None);
        }
        return Box::pin(super::recovery::takeover(state, run_id))
            .await
            .map(Some);
    }
    let session_id = super::backpressure::retry(|| representation::runs::association(&db, run_id))
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unsupported)?
        .parse::<crate::SessionId>()
        .map_err(|_| unavailable())?;
    let ownership = state
        .sessions
        .acquire_session_ownership(session_id, bcode_session::SessionOwnershipKind::RuntimeWork)
        .await
        .map_err(|_| unavailable())?;
    super::backpressure::retry(|| owner_boundary::verify(&db, run_id, &authority))
        .await
        .map_err(|_| unavailable())?;
    Ok(Some(crate::workflow_operations::AuthorityGuard {
        authority,
        _session_ownership: Some(ownership),
    }))
}

/// Persist an existing owner's observation. Success uses the atomic output/attempt commit.
/// Ambiguous and unsupported observations never fabricate terminal state or repeat effects.
pub async fn settle_observation(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    authority: &WorkflowExecutionAuthority,
    observation: bcode_workflow_store::AttemptObservation,
) -> Result<bcode_workflow_store::ReceiptReconciliationSummary, WorkflowStoreError> {
    use bcode_workflow_store::AttemptObservation as Observation;
    use representation::attempts::TerminalOutcome;
    let retained = retained_store(state).map_err(store_error)?;
    match lookup_at(retained.path(), &request.activation.run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => {
            return crate::persist_workflow_prompt_completion(
                retained.path(),
                &request.dispatch_identity,
                authority,
                observation,
            )
            .await;
        }
        RunLookup::Available(_) => {}
        _ => return Err(store_error("unsupported owner storage")),
    }
    let queue = super::backpressure::write_queue(retained.path()).map_err(store_error)?;
    let _write = queue.lock().await;
    let db = open(retained.path(), true).map_err(store_error)?;
    super::backpressure::retry(|| {
        owner_boundary::verify(&db, &request.activation.run_id, authority)
    })
    .await
    .map_err(store_error)?;
    let mut summary = bcode_workflow_store::ReceiptReconciliationSummary::default();
    match observation {
        Observation::Succeeded { output } => {
            let revision = super::backpressure::retry(|| {
                representation::graph_pages::revision(&db, &request.activation.run_id)
            })
            .await
            .map_err(store_error)?;
            let completion = super::backpressure::retry(|| {
                representation::graph::completion::complete(
                    &db,
                    authority,
                    revision,
                    &request.dispatch_identity,
                    &output,
                )
            })
            .await;
            completion.map_err(store_error)?;
            summary.succeeded.push(request.dispatch_identity.clone());
        }
        Observation::Cancelled | Observation::Failed { .. } => {
            let cancelled = matches!(observation, Observation::Cancelled);
            super::backpressure::retry(|| {
                representation::attempts::settle(
                    &db,
                    authority,
                    &request.activation.run_id,
                    &request.dispatch_identity,
                    if cancelled {
                        TerminalOutcome::Cancelled
                    } else {
                        TerminalOutcome::Failed
                    },
                    crate::current_unix_millis(),
                )
            })
            .await
            .map_err(store_error)?;
            if cancelled {
                summary.cancelled.push(request.dispatch_identity.clone());
            } else {
                summary.failed.push(request.dispatch_identity.clone());
            }
        }
        Observation::Admitted | Observation::Running | Observation::Deferred { .. } => {
            return Ok(summary);
        }
        Observation::Paused { .. }
        | Observation::RetryableFailure { .. }
        | Observation::Unknown => {
            return Err(store_error("observation requires lifecycle reconciliation"));
        }
    }
    if !super::backpressure::retry(|| {
        representation::safety::recovery_required(&db, &request.activation.run_id)
    })
    .await
    .map_err(store_error)?
    {
        super::backpressure::retry(|| {
            representation::runs::terminalize(&db, authority, &request.activation.run_id)
        })
        .await
        .map_err(store_error)?;
    }
    Ok(summary)
}

/// Commit cancellation intent and signal a bounded receipt page through existing operation owners.
/// A signal never settles an attempt; terminal evidence remains receipt-observer owned.
pub async fn cancel(state: &ServerState, run_id: &str) -> Result<Option<bool>, ServerError> {
    let retained = retained_store(state)?;
    match lookup_at(retained.path(), run_id).await? {
        RunLookup::Legacy | RunLookup::Missing => return Ok(None),
        RunLookup::Available(_) => {}
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Degraded => return Err(unavailable()),
    }
    let guard = local_authority(state, run_id)
        .await?
        .ok_or_else(unsupported)?;
    let db = open(retained.path(), true)?;
    let previous = super::backpressure::retry(|| {
        representation::safety::cancellation_requested_at(&db, run_id)
    })
    .await
    .map_err(|_| unavailable())?;
    super::backpressure::retry(|| {
        representation::safety::request_cancellation(
            &db,
            run_id,
            &guard.authority,
            crate::current_unix_millis(),
        )
    })
    .await
    .map_err(|_| unavailable())?;
    let mut cursor = None;
    loop {
        let (receipts, next) = super::backpressure::retry(|| {
            owner_boundary::cancellation_receipts(
                &db,
                run_id,
                &guard.authority,
                cursor.as_deref(),
                100,
            )
        })
        .await
        .map_err(|_| unavailable())?;
        for receipt in receipts {
            super::backpressure::retry(|| owner_boundary::verify(&db, run_id, &guard.authority))
                .await
                .map_err(|_| unavailable())?;
            crate::signal_workflow_attempt_cancellation(
                state,
                &bcode_workflow_store::ActiveAttemptCancellation {
                    run_id: receipt.run_id,
                    node_id: receipt.node_id,
                    activation_id: receipt.activation_id,
                    attempt: receipt.attempt,
                    dispatch_identity: receipt.dispatch_identity,
                    receipt: Some(receipt.receipt),
                },
            )
            .await?;
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
        tokio::task::yield_now().await;
    }
    Ok(Some(previous.is_none()))
}

/// Read the exact admitted intent even after terminal settlement, without replaying effects.
pub async fn dispatch_intent(
    state: &ServerState,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<serde_json::Value, WorkflowStoreError> {
    let retained = retained_store(state).map_err(store_error)?;
    match lookup_at(retained.path(), run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => retained.dispatch_intent(dispatch_identity),
        RunLookup::Available(_) => {
            let db = open(retained.path(), false).map_err(store_error)?;
            let result = super::backpressure::retry(|| {
                owner_boundary::dispatch_intent(&db, run_id, dispatch_identity)
            })
            .await;
            result.map_err(store_error)
        }
        _ => Err(store_error("unsupported owner storage")),
    }
}

/// Classify confirmed owner cancellation against canonical intent, never against a signal alone.
pub async fn cancellation_intended(
    state: &ServerState,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<bool, WorkflowStoreError> {
    let retained = retained_store(state).map_err(store_error)?;
    match lookup_at(retained.path(), run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => {
            let run = retained
                .run_summary(run_id)?
                .ok_or_else(|| store_error("missing run"))?;
            Ok(run.cancellation_requested_at_ms.is_some()
                || retained.has_publication_cancellation_intent(run_id, dispatch_identity)?)
        }
        RunLookup::Available(_) => {
            let db = open(retained.path(), false).map_err(store_error)?;
            let guard = local_authority(state, run_id)
                .await
                .map_err(store_error)?
                .ok_or_else(|| store_error("missing owner"))?;
            super::backpressure::retry(|| {
                owner_boundary::cancellation_intended(
                    &db,
                    run_id,
                    dispatch_identity,
                    &guard.authority,
                )
            })
            .await
            .map_err(store_error)
        }
        _ => Err(store_error("unsupported cancellation observation")),
    }
}

/// Normalize a model completion through canonical cancellation routing before generic output handling.
pub async fn completion_observation(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    completion: crate::ModelTurnCompletion,
) -> Result<bcode_workflow_store::AttemptObservation, WorkflowStoreError> {
    if completion.outcome == bcode_session_models::ModelTurnOutcome::Cancelled {
        return if cancellation_intended(
            state,
            &request.activation.run_id,
            &request.dispatch_identity,
        )
        .await?
        {
            Ok(bcode_workflow_store::AttemptObservation::Cancelled)
        } else {
            Ok(bcode_workflow_store::AttemptObservation::Paused {
                reason: bcode_workflow_store::AttemptPauseReason::Steering,
                message: completion.message.unwrap_or_else(|| "workflow model turn was cancelled for steering; explicit resume is required".into()),
            })
        };
    }
    crate::workflow_attempt_observation_from_completion(state, request, completion)
}
