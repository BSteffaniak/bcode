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

fn owner_queue_labels(
    operation: &'static str,
    run_id: &str,
    activation_id: &str,
    dispatch_identity: &str,
) -> bcode_metrics::MetricLabels {
    std::collections::BTreeMap::from([
        ("operation".into(), operation.into()),
        ("run_id".into(), run_id.into()),
        ("activation_id".into(), activation_id.into()),
        ("dispatch_identity".into(), dispatch_identity.into()),
    ])
}

fn owner_stage_span(
    metrics: &bcode_metrics::MetricsRegistry,
    name: &'static str,
    run_id: &str,
    activation_id: &str,
    dispatch_identity: &str,
) -> bcode_metrics::MetricsSpan {
    let labels = std::collections::BTreeMap::from([
        ("run_id".into(), run_id.into()),
        ("activation_id".into(), activation_id.into()),
        ("dispatch_identity".into(), dispatch_identity.into()),
    ]);
    metrics.add_counter_with_labels(format!("{name}.started"), 1, labels.clone());
    metrics.span(name).labels(labels)
}

async fn observe_owner_stage<T>(
    state: &ServerState,
    name: &'static str,
    request: &PreparedActivationDispatch,
    operation: impl std::future::Future<Output = Result<T, WorkflowStoreError>>,
) -> Result<T, WorkflowStoreError> {
    let span = owner_stage_span(
        &state.metrics,
        name,
        &request.activation.run_id,
        &request.activation.activation_id,
        &request.dispatch_identity,
    );
    let result = operation.await;
    span.finish_result(&result);
    result
}

fn store_error(_: impl std::fmt::Display) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData("workflow owner storage is unavailable or inconsistent".into())
}

/// Check a prepared handoff in its canonical representation before external effects.
pub async fn check_owner_admission(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    authority: &WorkflowExecutionAuthority,
) -> Result<Option<serde_json::Value>, WorkflowStoreError> {
    observe_owner_stage(
        state,
        "workflow.owner.admission",
        request,
        check_owner_admission_inner(state, request, authority),
    )
    .await
}

async fn check_owner_admission_inner(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    authority: &WorkflowExecutionAuthority,
) -> Result<Option<serde_json::Value>, WorkflowStoreError> {
    let retained = retained_store(state).await.map_err(store_error)?;
    match lookup_at(retained.path(), &request.activation.run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => retained.check_owner_admission(request, authority),
        RunLookup::Available(_) => {
            let db = open(retained.path(), true).map_err(store_error)?;
            super::backpressure::queued_observed(
                retained.path(),
                &state.metrics,
                owner_queue_labels(
                    "owner_admission",
                    &request.activation.run_id,
                    &request.activation.activation_id,
                    &request.dispatch_identity,
                ),
                || owner_boundary::check_owner_admission(&db, request, authority),
            )
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
    let retained = retained_store(state).await.map_err(store_error)?;
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
    let mut retained = retained_store(state).await.map_err(store_error)?;
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

/// Record shared-parent linkage only in the current representation, which permits
/// multiple exact dispatch links to the same canonical session. Legacy dispatch retains
/// its pre-existing no-link behavior; unknown or degraded representations fail closed.
pub async fn link_shared_execution_session(
    state: &ServerState,
    authority: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
) -> Result<(), WorkflowStoreError> {
    let retained = retained_store(state).await.map_err(store_error)?;
    match lookup_at(retained.path(), &link.run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => {
            retained.verify_execution_authority(&link.run_id, authority)?;
            Ok(())
        }
        RunLookup::Available(_) => link_execution_session(state, authority, link).await,
        _ => Err(store_error("unsupported shared execution storage")),
    }
}

#[cfg(test)]
type ReceiptPublicationWaitKey = (std::path::PathBuf, String);

#[cfg(test)]
static RECEIPT_PUBLICATION_WAITERS: std::sync::Mutex<
    std::collections::BTreeMap<ReceiptPublicationWaitKey, Option<tokio::sync::oneshot::Sender<()>>>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Fixture-owned registration for observing an actual pending receipt write lock.
#[cfg(test)]
pub(super) struct ReceiptPublicationWaitGuard {
    key: ReceiptPublicationWaitKey,
}

#[cfg(test)]
impl Drop for ReceiptPublicationWaitGuard {
    fn drop(&mut self) {
        RECEIPT_PUBLICATION_WAITERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

/// Register one notification scoped to a canonical fixture store and exact dispatch.
/// Duplicate registrations fail without replacing the existing fixture owner.
#[cfg(test)]
pub(super) fn observe_receipt_publication_wait(
    path: &std::path::Path,
    dispatch_identity: &str,
    pending: tokio::sync::oneshot::Sender<()>,
) -> std::io::Result<ReceiptPublicationWaitGuard> {
    let key = (path.canonicalize()?, dispatch_identity.to_owned());
    let mut waiters = RECEIPT_PUBLICATION_WAITERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if waiters.contains_key(&key) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "receipt publication wait already observed",
        ));
    }
    waiters.insert(key.clone(), Some(pending));
    drop(waiters);
    Ok(ReceiptPublicationWaitGuard { key })
}

#[cfg(test)]
fn notify_receipt_publication_pending(key: &ReceiptPublicationWaitKey) {
    let pending = RECEIPT_PUBLICATION_WAITERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(key)
        .and_then(Option::take);
    if let Some(pending) = pending {
        let _ = pending.send(());
    }
}

/// Commit the owner receipt without switching canonical representations on failure.
pub async fn persist_receipt(
    state: &ServerState,
    receipt: &bcode_workflow_store::DispatchReceipt,
    authority: &WorkflowExecutionAuthority,
) -> Result<(), WorkflowStoreError> {
    let span = owner_stage_span(
        &state.metrics,
        "workflow.owner.receipt_publication",
        &receipt.run_id,
        &receipt.activation_id,
        &receipt.dispatch_identity,
    );
    let result = persist_receipt_inner(state, receipt, authority).await;
    span.finish_result(&result);
    result
}

async fn persist_receipt_inner(
    state: &ServerState,
    receipt: &bcode_workflow_store::DispatchReceipt,
    authority: &WorkflowExecutionAuthority,
) -> Result<(), WorkflowStoreError> {
    let retained = retained_store(state).await.map_err(store_error)?;
    match lookup_at(retained.path(), &receipt.run_id)
        .await
        .map_err(store_error)?
    {
        RunLookup::Legacy => retained.persist_owned_dispatch_receipt(receipt, authority),
        RunLookup::Available(_) => {
            let queue = super::backpressure::write_queue(retained.path()).map_err(store_error)?;
            #[cfg(not(test))]
            let publication_lock = queue.lock();
            #[cfg(test)]
            let key = (
                retained.path().canonicalize().map_err(store_error)?,
                receipt.dispatch_identity.clone(),
            );
            #[cfg(test)]
            let publication_lock = async {
                use std::future::Future as _;
                let lock = queue.lock();
                tokio::pin!(lock);
                std::future::poll_fn(|cx| {
                    let result = lock.as_mut().poll(cx);
                    if result.is_pending() {
                        notify_receipt_publication_pending(&key);
                    }
                    result
                })
                .await
            };
            let _write = super::backpressure::observe_write_lock(
                &state.metrics,
                owner_queue_labels(
                    "receipt_publication",
                    &receipt.run_id,
                    &receipt.activation_id,
                    &receipt.dispatch_identity,
                ),
                publication_lock,
            )
            .await;
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

/// Resolve admission time from the exact session-owned acceptance, never recovery time.
///
/// # Errors
/// Rejects missing, mismatched or unavailable canonical acceptance evidence.
pub async fn prompt_acceptance_time(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    receipt: &serde_json::Value,
) -> Result<u64, WorkflowStoreError> {
    let decoded = crate::workflow_receipts::AgentTurnReceipt::decode(receipt)
        .map_err(|_| store_error("invalid acceptance receipt"))?;
    crate::verify_workflow_observation_session(
        state,
        decoded.session_id,
        &bcode_workflow_store::AttemptReconciliationRequest {
            run_id: request.activation.run_id.clone(),
            node_id: request.activation.node_id.clone(),
            activation_id: request.activation.activation_id.clone(),
            attempt: request.attempt,
            dispatch_identity: request.dispatch_identity.clone(),
            side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
            receipt: receipt.clone(),
        },
    )
    .await?;
    let accepted = state
        .sessions
        .turn_receipt(
            decoded.session_id,
            "bcode.workflow",
            &request.dispatch_identity,
        )
        .await
        .map_err(store_error)?
        .ok_or_else(|| store_error("missing acceptance"))?;
    if decoded.turn_id != accepted.turn_id.0
        || receipt
            .get("accepted_event_sequence")
            .is_some_and(|sequence| sequence.as_u64() != Some(accepted.accepted_event_sequence))
        || receipt
            .get("work_id")
            .is_some_and(|work| work.as_str() != Some(accepted.work_id.0.as_str()))
    {
        return Err(store_error("mismatched acceptance"));
    }
    let window = state
        .sessions
        .session_history_around(
            decoded.session_id,
            bcode_session_models::SessionHistoryAroundQuery {
                sequence: accepted.accepted_event_sequence,
                before: 0,
                after: 0,
            },
        )
        .await
        .map_err(store_error)?;
    let acceptance_sequence = accepted.accepted_event_sequence;
    window
        .events
        .into_iter()
        .find_map(|event| {
            (event.session_id == decoded.session_id
                && event.sequence == acceptance_sequence
                && matches!(
                    event.kind,
                    bcode_session_models::SessionEventKind::UserMessage { .. }
                        | bcode_session_models::SessionEventKind::SubmissionAccepted { .. }
                ))
            .then_some(event.timestamp_ms)
        })
        .ok_or_else(|| store_error("canonical acceptance event unavailable"))
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
    let retained = retained_store(state).await?;
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
    observe_owner_stage(
        state,
        "workflow.owner.settlement",
        request,
        settle_observation_inner(state, request, authority, observation),
    )
    .await
}

async fn settle_observation_inner(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    authority: &WorkflowExecutionAuthority,
    observation: bcode_workflow_store::AttemptObservation,
) -> Result<bcode_workflow_store::ReceiptReconciliationSummary, WorkflowStoreError> {
    use bcode_workflow_store::AttemptObservation as Observation;
    use representation::attempts::TerminalOutcome;
    let retained = retained_store(state).await.map_err(store_error)?;
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
    let _write = super::backpressure::observe_write_lock(
        &state.metrics,
        owner_queue_labels(
            "settlement",
            &request.activation.run_id,
            &request.activation.activation_id,
            &request.dispatch_identity,
        ),
        queue.lock(),
    )
    .await;
    let db = open(retained.path(), true).map_err(store_error)?;
    super::backpressure::retry(|| {
        owner_boundary::verify(&db, &request.activation.run_id, authority)
    })
    .await
    .map_err(store_error)?;
    let mut summary = bcode_workflow_store::ReceiptReconciliationSummary::default();
    match observation {
        Observation::Succeeded { output } => {
            complete_successful_observation(&db, request, authority, &output).await?;
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

/// Commit validated success against the current graph revision under the caller's
/// already-held writer guard, preserving the existing replay-safe retry boundaries.
async fn complete_successful_observation(
    db: &dyn switchy_database::Database,
    request: &PreparedActivationDispatch,
    authority: &WorkflowExecutionAuthority,
    output: &bcode_workflow_store::ValidatedOutput,
) -> Result<(), WorkflowStoreError> {
    let revision = super::backpressure::retry(|| {
        representation::graph_pages::revision(db, &request.activation.run_id)
    })
    .await
    .map_err(store_error)?;
    super::backpressure::retry(|| {
        representation::graph::completion::complete(
            db,
            authority,
            revision,
            &request.dispatch_identity,
            output,
        )
    })
    .await
    .map_err(store_error)?;
    Ok(())
}

/// Commit cancellation intent and signal a bounded receipt page through existing operation owners.
/// A signal never settles an attempt; terminal evidence remains receipt-observer owned.
pub async fn cancel(state: &ServerState, run_id: &str) -> Result<Option<bool>, ServerError> {
    let retained = retained_store(state).await?;
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
    super::backpressure::queued(retained.path(), || {
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
            if reconcile_cancellation_receipt(state, &db, &guard.authority, &receipt).await? {
                continue;
            }
            super::backpressure::retry(|| owner_boundary::verify(&db, run_id, &guard.authority))
                .await
                .map_err(|_| unavailable())?;
            if crate::signal_workflow_attempt_cancellation(
                state,
                &bcode_workflow_store::ActiveAttemptCancellation {
                    run_id: receipt.run_id.clone(),
                    node_id: receipt.node_id.clone(),
                    activation_id: receipt.activation_id.clone(),
                    attempt: receipt.attempt,
                    dispatch_identity: receipt.dispatch_identity.clone(),
                    receipt: Some(receipt.receipt.clone()),
                },
            )
            .await
            .is_err()
            {
                // Durable intent remains pending. Receipt observation, not a missing
                // local runtime handle or a failed signal, establishes termination.
                tracing::debug!(
                    stage = "receipt_signal",
                    "workflow cancellation signalling deferred"
                );
            }
            reconcile_cancellation_receipt(state, &db, &guard.authority, &receipt).await?;
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
        tokio::task::yield_now().await;
    }
    finish_terminal_registration(state, run_id).await?;
    Ok(Some(previous.is_none()))
}

/// Observe already-admitted receipts under an existing local owner after failed start.
/// Never admits work, cancels unreceipted effects, or acquires foreign authority.
pub async fn reconcile_failed_start(state: &ServerState, run_id: &str) -> Result<(), ServerError> {
    let guard = settlement_authority(state, run_id)
        .await?
        .ok_or_else(unsupported)?;
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let mut after = None;
    loop {
        super::backpressure::retry(|| owner_boundary::verify(&db, run_id, &guard.authority))
            .await
            .map_err(|_| unavailable())?;
        let attempts = super::backpressure::retry(|| {
            representation::attempts::pending(&db, run_id, after.as_deref(), 32)
        })
        .await
        .map_err(|_| unavailable())?;
        let next = attempts
            .last()
            .map(|attempt| attempt.dispatch_identity.clone());
        for attempt in attempts {
            let receipt = match attempt.receipt {
                Some(receipt) => Some(receipt),
                None => {
                    match super::recovery::recover_acceptance(
                        state,
                        &attempt.prepared,
                        &guard.authority,
                    )
                    .await?
                    {
                        Some(receipt) => Some(receipt),
                        None => {
                            recover_shared_acceptance(state, &attempt.prepared, &guard.authority)
                                .await?
                        }
                    }
                }
            };
            if let Some(receipt) = receipt {
                reconcile_cancellation_receipt(state, &db, &guard.authority, &receipt).await?;
            }
        }
        if next.is_none() {
            break;
        }
        after = next;
        tokio::task::yield_now().await;
    }
    finish_terminal_registration(state, run_id).await
}

async fn recover_shared_acceptance(
    state: &ServerState,
    prepared: &bcode_workflow_store::PreparedAttempt,
    authority: &WorkflowExecutionAuthority,
) -> Result<Option<bcode_workflow_store::DispatchReceipt>, ServerError> {
    if prepared
        .intent
        .get("owner")
        .and_then(serde_json::Value::as_str)
        != Some(crate::workflow_receipts::AGENT_TURN_OWNER)
        || !crate::workflow_operations::authority_targets_current_daemon(state, authority)
    {
        return Ok(None);
    }
    let retained = retained_store(state).await?;
    let db = open(retained.path(), false)?;
    let link = super::backpressure::retry(|| {
        owner_boundary::execution_session_link(
            &db,
            &prepared.run_id,
            &prepared.node_id,
            &prepared.activation_id,
            prepared.attempt,
        )
    })
    .await
    .map_err(|_| unavailable())?;
    let Some(link) = link else {
        return Ok(None);
    };
    if !super::workflow_activity::verify_shared_parent_link(&db, &prepared.run_id, &link)
        .await
        .map_err(super::workflow_activity::SharedVerificationFailure::into_server_error)?
    {
        return Ok(None);
    }
    super::backpressure::retry(|| owner_boundary::verify(&db, &prepared.run_id, authority))
        .await
        .map_err(|_| unavailable())?;
    let session = link.session_id.parse().map_err(|_| unavailable())?;
    let identity = prepared.dispatch_identity();
    let Some(accepted) = state
        .sessions
        .turn_receipt(session, "bcode.workflow", &identity)
        .await?
    else {
        return Ok(None);
    };
    let window = state
        .sessions
        .session_history_around(
            session,
            bcode_session_models::SessionHistoryAroundQuery {
                sequence: accepted.accepted_event_sequence,
                before: 0,
                after: 0,
            },
        )
        .await?;
    let event = window
        .events
        .into_iter()
        .find(|event| {
            event.sequence == accepted.accepted_event_sequence && event.session_id == session
        })
        .ok_or_else(unavailable)?;
    if !matches!(
        event.kind,
        bcode_session_models::SessionEventKind::UserMessage { .. }
            | bcode_session_models::SessionEventKind::SubmissionAccepted { .. }
    ) {
        return Err(unavailable());
    }
    let revision =
        super::backpressure::retry(|| representation::graph_pages::revision(&db, &prepared.run_id))
            .await
            .map_err(|_| unavailable())?;
    let node = super::backpressure::retry(|| {
        representation::graph_pages::node(&db, &prepared.run_id, revision, &prepared.node_id)
    })
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unavailable)?;
    let receipt = bcode_workflow_store::DispatchReceipt {
        run_id: prepared.run_id.clone(),
        node_id: prepared.node_id.clone(),
        activation_id: prepared.activation_id.clone(),
        attempt: prepared.attempt,
        dispatch_identity: identity,
        admitted_at_ms: event.timestamp_ms,
        receipt: crate::recovered_workflow_receipt(session, &accepted, &node.output.type_name),
    };
    crate::workflow_receipts::AgentTurnReceipt::decode(&receipt.receipt)
        .map_err(|_| unavailable())?;
    persist_receipt(state, &receipt, authority).await?;
    Ok(Some(receipt))
}

async fn finish_terminal_registration(
    state: &ServerState,
    run_id: &str,
) -> Result<(), ServerError> {
    if matches!(
        super::status(state, run_id).await?.map(|run| run.status),
        Some(
            bcode_workflow::RunStatus::Completed
                | bcode_workflow::RunStatus::Failed
                | bcode_workflow::RunStatus::Cancelled
        )
    ) {
        crate::settle_workflow_runtime_work(state, run_id).await?;
    }
    Ok(())
}

async fn reconcile_cancellation_receipt(
    state: &ServerState,
    db: &dyn switchy_database::Database,
    authority: &WorkflowExecutionAuthority,
    receipt: &bcode_workflow_store::DispatchReceipt,
) -> Result<bool, ServerError> {
    use bcode_workflow_store::AsyncAttemptStatusObserver;
    super::backpressure::retry(|| owner_boundary::verify(db, &receipt.run_id, authority))
        .await
        .map_err(|_| unavailable())?;
    let prepared = super::backpressure::retry(|| {
        owner_boundary::prepared(db, &receipt.run_id, &receipt.dispatch_identity)
    })
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unavailable)?;
    let observation = crate::WorkflowTurnReceiptObserver { state }
        .observe_async(&bcode_workflow_store::AttemptReconciliationRequest {
            run_id: receipt.run_id.clone(),
            node_id: receipt.node_id.clone(),
            activation_id: receipt.activation_id.clone(),
            attempt: receipt.attempt,
            dispatch_identity: receipt.dispatch_identity.clone(),
            side_effect: prepared.side_effect,
            receipt: receipt.receipt.clone(),
        })
        .await?;
    if !matches!(
        observation,
        bcode_workflow_store::AttemptObservation::Succeeded { .. }
            | bcode_workflow_store::AttemptObservation::Failed { .. }
            | bcode_workflow_store::AttemptObservation::Cancelled
    ) {
        return Ok(false);
    }
    let revision =
        super::backpressure::retry(|| representation::graph_pages::revision(db, &receipt.run_id))
            .await
            .map_err(|_| unavailable())?;
    let node = super::backpressure::retry(|| {
        representation::graph_pages::node(db, &receipt.run_id, revision, &receipt.node_id)
    })
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unavailable)?;
    settle_observation(
        state,
        &PreparedActivationDispatch {
            activation: bcode_workflow_store::PendingActivation {
                run_id: receipt.run_id.clone(),
                node_id: receipt.node_id.clone(),
                activation_id: receipt.activation_id.clone(),
                dependency_generation: 0,
                input: None,
                node,
                created_at_ms: prepared.prepared_at_ms,
            },
            attempt: receipt.attempt,
            dispatch_identity: receipt.dispatch_identity.clone(),
            intent: prepared.intent,
        },
        authority,
        observation,
    )
    .await?;
    Ok(true)
}

/// Read the exact admitted intent even after terminal settlement, without replaying effects.
pub async fn dispatch_intent(
    state: &ServerState,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<serde_json::Value, WorkflowStoreError> {
    let retained = retained_store(state).await.map_err(store_error)?;
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
    let retained = retained_store(state).await.map_err(store_error)?;
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

#[cfg(test)]
mod instrumentation_tests {
    use super::owner_stage_span;
    use std::future::Future as _;

    #[tokio::test]
    async fn held_owner_queue_records_start_then_unfinished_on_drop() {
        let root = tempfile::tempdir().expect("fixture root");
        let path = root.path().join("workflow.db");
        std::fs::write(&path, []).expect("queue identity");
        let queue = super::super::backpressure::write_queue(&path).expect("canonical queue");
        let held = queue.lock().await;
        let metrics = bcode_metrics::MetricsRegistry::default();
        let mut waiting = Box::pin(async {
            let span = owner_stage_span(
                &metrics,
                "workflow.owner.receipt_publication",
                "opaque-run",
                "opaque-activation",
                "opaque-dispatch",
            );
            let _guard = queue.lock().await;
            span.finish_ok();
        });
        assert!(
            std::future::poll_fn(|context| {
                std::task::Poll::Ready(waiting.as_mut().poll(context).is_pending())
            })
            .await
        );
        let before = metrics.report();
        assert_eq!(before.events.len(), 1);
        assert_eq!(
            before.events[0].name,
            "workflow.owner.receipt_publication.started"
        );
        drop(waiting);
        drop(held);
        let events = metrics.report().events;
        assert_eq!(events.len(), 3);
        for event in &events {
            assert_eq!(
                event.labels.get("run_id").map(String::as_str),
                Some("opaque-run")
            );
            assert_eq!(
                event.labels.get("activation_id").map(String::as_str),
                Some("opaque-activation")
            );
            assert_eq!(
                event.labels.get("dispatch_identity").map(String::as_str),
                Some("opaque-dispatch")
            );
            assert!(!event.labels.contains_key("path"));
        }
        assert!(
            events
                .iter()
                .skip(1)
                .all(|event| event.labels.get("outcome").map(String::as_str) == Some("unfinished"))
        );
    }

    #[tokio::test]
    async fn released_owner_queue_records_success_only_after_acquisition() {
        let queue = tokio::sync::Mutex::new(());
        let held = queue.lock().await;
        let metrics = bcode_metrics::MetricsRegistry::default();
        let mut waiting = Box::pin(async {
            let span = owner_stage_span(
                &metrics,
                "workflow.owner.admission",
                "run",
                "activation",
                "dispatch",
            );
            let _guard = queue.lock().await;
            span.finish_ok();
        });
        assert!(
            std::future::poll_fn(|context| std::task::Poll::Ready(
                waiting.as_mut().poll(context).is_pending()
            ))
            .await
        );
        assert_eq!(metrics.report().events.len(), 1);
        drop(held);
        waiting.await;
        assert!(
            metrics.report().events.iter().skip(1).all(|event| event
                .labels
                .get("outcome")
                .map(String::as_str)
                == Some("ok"))
        );
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    async fn admit_missing_runtime_receipt(
        state: &ServerState,
        db: &dyn switchy_database::Database,
        authority: &WorkflowExecutionAuthority,
        parent: crate::SessionId,
    ) -> (
        bcode_workflow_store::PreparedAttempt,
        bcode_workflow_store::DispatchReceipt,
    ) {
        let activation = representation::graph::ready(db, "new", 1, None, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let prepared = bcode_workflow_store::PreparedAttempt {
            run_id: "new".into(),
            node_id: activation.node_id.clone(),
            activation_id: activation.activation_id.clone(),
            attempt: 1,
            side_effect: bcode_workflow::DispatchSideEffect::Mutating,
            intent: serde_json::json!({}),
            prepared_at_ms: 1,
        };
        let dispatch = representation::attempts::prepare(db, authority, &prepared)
            .await
            .unwrap();
        let receipt = bcode_workflow_store::DispatchReceipt {
            run_id: "new".into(),
            node_id: activation.node_id,
            activation_id: activation.activation_id,
            attempt: 1,
            dispatch_identity: dispatch,
            admitted_at_ms: 2,
            receipt: serde_json::json!({
                "owner": crate::workflow_receipts::AGENT_TURN_OWNER,
                "session_id": parent, "turn_id": "lost-runtime-turn",
                "output_schema_id": "output"
            }),
        };
        persist_receipt(state, &receipt, authority).await.unwrap();
        (prepared, receipt)
    }

    fn cancellation_target(
        receipt: &bcode_workflow_store::DispatchReceipt,
    ) -> bcode_workflow_store::ActiveAttemptCancellation {
        bcode_workflow_store::ActiveAttemptCancellation {
            run_id: receipt.run_id.clone(),
            node_id: receipt.node_id.clone(),
            activation_id: receipt.activation_id.clone(),
            attempt: receipt.attempt,
            dispatch_identity: receipt.dispatch_identity.clone(),
            receipt: Some(receipt.receipt.clone()),
        }
    }

    async fn completed_shared_turn_evidence(
        state: &ServerState,
        parent: crate::SessionId,
        accepted: &bcode_session_models::TurnReceipt,
    ) -> (Vec<bcode_session_models::SessionEvent>, u64) {
        let events = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let events = state
                    .sessions
                    .session_turn_evidence(parent, &accepted.turn_id.0)
                    .await
                    .unwrap();
                if events.iter().any(|event| matches!(
                    &event.kind,
                    bcode_session_models::SessionEventKind::ModelTurnFinished { turn_id, outcome, .. }
                    if turn_id == &accepted.turn_id.0 && *outcome == bcode_session_models::ModelTurnOutcome::Completed
                )) {
                    break events;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("offline evaluator completes before receipt recovery");
        let acceptance = state
            .sessions
            .session_history_around(
                parent,
                bcode_session_models::SessionHistoryAroundQuery {
                    sequence: accepted.accepted_event_sequence,
                    before: 0,
                    after: 0,
                },
            )
            .await
            .unwrap()
            .events
            .into_iter()
            .find(|event| event.sequence == accepted.accepted_event_sequence)
            .unwrap();
        (events, acceptance.timestamp_ms)
    }

    async fn assert_unreceipted_completion_rejects_foreign_authority(
        state: &std::sync::Arc<ServerState>,
        db: &dyn switchy_database::Database,
        dispatch: &PreparedActivationDispatch,
        authority: &WorkflowExecutionAuthority,
        receipt: &serde_json::Value,
        observation: bcode_workflow_store::AttemptObservation,
    ) {
        let mut rejected_authority = authority.clone();
        rejected_authority.generation += 1;
        assert!(
            crate::commit_workflow_prompt_observation(
                state,
                dispatch,
                &rejected_authority,
                receipt,
                observation,
            )
            .await
            .is_err()
        );
        assert!(
            owner_boundary::receipt(db, &dispatch.dispatch_identity)
                .await
                .unwrap()
                .is_none()
        );
    }

    async fn admit_unobserved_shared_turn(
        state: &std::sync::Arc<ServerState>,
        dispatch: &PreparedActivationDispatch,
        parent: crate::SessionId,
    ) {
        let metadata = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "bcode.workflow".into(),
                correlation_id: Some(dispatch.dispatch_identity.clone()),
                display_label: None,
            }),
            idempotency_key: Some(dispatch.dispatch_identity.clone()),
            execution: bcode_session_models::TurnExecutionOptions {
                provider_plugin_id: Some("bcode.fake-provider".into()),
                model_id: Some("fake-echo".into()),
                structured_output: Some(bcode_session_models::TurnStructuredOutputRequest {
                    name: bcode_session_models::structured_output_name(
                        &dispatch.activation.node.output.type_name,
                    ),
                    schema: dispatch.activation.node.output.schema.clone(),
                    strict: true,
                    max_corrections: 0,
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        // Admit the real provider turn without installing the workflow completion
        // callback. This fixture models loss of that owner before receipt publication;
        // the normal dispatch callback now independently recovers a failed publication.
        let submitted = crate::submit_session_model_turn_with_admission(
            state,
            parent,
            "Return 9".into(),
            None,
            metadata,
            None,
        )
        .await
        .unwrap();
        let crate::SubmittedModelTurn::Started { completion, .. } = submitted else {
            panic!("new shared turn");
        };
        let completion = completion.await.unwrap();
        assert_eq!(
            completion.outcome,
            crate::ModelTurnOutcome::Completed,
            "{:?}",
            completion.message
        );
    }

    #[tokio::test]
    async fn completed_shared_turn_recovers_receipt_with_original_acceptance_time() {
        let (_root, _sessions, state, _) = super::super::tests::typed_provider_fixture_with_target(
            false,
            bcode_workflow::PromptContextTarget::SharedParentSequential,
        )
        .await;
        let db = open(&crate::workflow_store_path(&state), true).unwrap();
        let guard = local_authority(&state, "new").await.unwrap().unwrap();
        let dispatch =
            super::super::tests::prepare_acceptance_recovery_fixture(&state, &db, &guard.authority)
                .await;
        let parent: crate::SessionId = representation::runs::association(&db, "new")
            .await
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        admit_unobserved_shared_turn(&state, &dispatch, parent).await;
        assert!(
            owner_boundary::receipt(&db, &dispatch.dispatch_identity)
                .await
                .unwrap()
                .is_none()
        );
        let accepted = state
            .sessions
            .turn_receipt(parent, "bcode.workflow", &dispatch.dispatch_identity)
            .await
            .unwrap()
            .unwrap();
        let (events, acceptance_time) =
            completed_shared_turn_evidence(&state, parent, &accepted).await;
        let observation = completion_observation(
            &state,
            &dispatch,
            crate::ModelTurnCompletion::completed()
                .with_output(crate::workflow_turn_output(&events, &accepted.turn_id.0).unwrap()),
        )
        .await
        .unwrap();
        let bcode_workflow_store::AttemptObservation::Succeeded { output } = &observation else {
            panic!("completed evaluator must have a validated output");
        };
        let terminal_at_ms = output.created_at_ms;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(crate::current_unix_millis() > terminal_at_ms);
        let receipt = crate::recovered_workflow_receipt(parent, &accepted, &output.schema_id);
        assert_unreceipted_completion_rejects_foreign_authority(
            &state,
            &db,
            &dispatch,
            &guard.authority,
            &receipt,
            observation.clone(),
        )
        .await;
        let summary = crate::commit_workflow_prompt_observation(
            &state,
            &dispatch,
            &guard.authority,
            &receipt,
            observation.clone(),
        )
        .await
        .expect("delayed receipt recovery must not move acceptance past completion");
        let retained = owner_boundary::receipt(&db, &dispatch.dispatch_identity)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.admitted_at_ms, acceptance_time);
        assert!(retained.admitted_at_ms <= terminal_at_ms);
        assert_eq!(
            owner_boundary::terminal_outcome(&db, "new", &dispatch.dispatch_identity)
                .await
                .unwrap(),
            Some(representation::attempts::TerminalOutcome::Succeeded)
        );
        crate::commit_workflow_prompt_observation(
            &state,
            &dispatch,
            &guard.authority,
            &receipt,
            observation,
        )
        .await
        .expect("duplicate completion is idempotent");
        crate::continue_settled_workflow_prompt(&state, &dispatch, summary).await;
        let run = super::super::status(&state, "new").await.unwrap().unwrap();
        assert_eq!(run.status, bcode_workflow::RunStatus::Completed);
    }

    #[tokio::test]
    async fn shared_parent_admission_failure_retains_receipt_until_cancelled_evidence() {
        let (_root, _sessions, state, _) = super::super::tests::typed_provider_fixture_with_target(
            true,
            bcode_workflow::PromptContextTarget::SharedParentSequential,
        )
        .await;
        let db = open(&crate::workflow_store_path(&state), true).unwrap();
        let guard = local_authority(&state, "new").await.unwrap().unwrap();
        let dispatch =
            super::super::tests::prepare_acceptance_recovery_fixture(&state, &db, &guard.authority)
                .await;
        let parent: crate::SessionId = representation::runs::association(&db, "new")
            .await
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        let work =
            crate::register_workflow_runtime_work(&state, parent, "new", "failed start".into())
                .await;
        let failure =
            crate::dispatch_workflow_prompt_turn_after_admission(&state, &dispatch, || {
                Err(WorkflowStoreError::InvalidData(
                    "injected failed initial execution".into(),
                ))
            })
            .await;
        assert!(matches!(failure, Err(WorkflowStoreError::InvalidData(_))));
        assert!(
            owner_boundary::receipt(&db, &dispatch.dispatch_identity)
                .await
                .unwrap()
                .is_none()
        );
        assert!(super::super::status(&state, "new").await.unwrap().is_some());
        reconcile_failed_start(&state, "new").await.unwrap();
        let receipt = owner_boundary::receipt(&db, &dispatch.dispatch_identity)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.receipt["session_id"], serde_json::json!(parent));
        assert!(
            owner_boundary::terminal_outcome(&db, "new", &dispatch.dispatch_identity)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .runtime_work
                .active_ids_for_session(parent)
                .await
                .contains(&work)
        );
        assert_eq!(cancel(&state, "new").await.unwrap(), Some(true));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                reconcile_failed_start(&state, "new").await.unwrap();
                if owner_boundary::terminal_outcome(&db, "new", &dispatch.dispatch_identity)
                    .await
                    .unwrap()
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real cancelled receipt settles");
        assert!(
            owner_boundary::terminal_outcome(&db, "new", &dispatch.dispatch_identity)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            super::super::status(&state, "new")
                .await
                .unwrap()
                .unwrap()
                .status,
            bcode_workflow::RunStatus::Cancelled
        );
        assert!(
            !state
                .runtime_work
                .active_ids_for_session(parent)
                .await
                .contains(&work)
        );
    }

    #[tokio::test]
    async fn admitted_receipt_without_runtime_defers_signal_until_terminal_evidence() {
        let (_root, _sessions, state, _) = super::super::tests::typed_provider_fixture().await;
        let db = open(&crate::workflow_store_path(&state), true).unwrap();
        let authority = owner_boundary::execution_authority(&db, "new")
            .await
            .unwrap()
            .unwrap();
        let parent: crate::SessionId = representation::runs::association(&db, "new")
            .await
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        let work =
            crate::register_workflow_runtime_work(&state, parent, "new", "failed start".into())
                .await;
        let (_, receipt) = admit_missing_runtime_receipt(&state, &db, &authority, parent).await;
        let dispatch = receipt.dispatch_identity.clone();
        state
            .sessions
            .append_model_turn_started(parent, "lost-runtime-turn".into())
            .await
            .unwrap();
        reconcile_failed_start(&state, "new").await.unwrap();
        assert!(
            representation::safety::cancellation_requested_at(&db, "new")
                .await
                .unwrap()
                .is_none()
        );
        assert!(super::super::status(&state, "new").await.unwrap().is_some());
        let signal =
            crate::signal_workflow_attempt_cancellation(&state, &cancellation_target(&receipt))
                .await;
        assert!(
            matches!(signal, Err(WorkflowStoreError::InvalidData(message))
            if message.starts_with("active runtime work not found for workflow dispatch:"))
        );
        assert_eq!(cancel(&state, "new").await.unwrap(), Some(true));
        assert_eq!(cancel(&state, "new").await.unwrap(), Some(false));
        assert!(
            owner_boundary::terminal_outcome(&db, "new", &dispatch)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .runtime_work
                .active_ids_for_session(parent)
                .await
                .contains(&work)
        );
        state
            .sessions
            .append_model_turn_finished(
                parent,
                "lost-runtime-turn".into(),
                bcode_session_models::ModelTurnOutcome::Cancelled,
                None,
            )
            .await
            .unwrap();
        assert_eq!(cancel(&state, "new").await.unwrap(), Some(false));
        assert!(
            owner_boundary::terminal_outcome(&db, "new", &dispatch)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            !state
                .runtime_work
                .active_ids_for_session(parent)
                .await
                .contains(&work)
        );
        assert_eq!(
            super::super::status(&state, "new")
                .await
                .unwrap()
                .unwrap()
                .status,
            bcode_workflow::RunStatus::Cancelled
        );
    }
}
