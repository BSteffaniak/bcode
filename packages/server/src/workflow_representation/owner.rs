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
            super::backpressure::retry(|| owner_boundary::verify(&db, run_id, &guard.authority))
                .await
                .map_err(|_| unavailable())?;
            if crate::signal_workflow_attempt_cancellation(
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
    use bcode_workflow_store::AsyncAttemptStatusObserver;

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

    #[tokio::test]
    async fn admitted_receipt_without_runtime_defers_signal_until_terminal_evidence() {
        let (_root, _sessions, state, definition) =
            super::super::tests::typed_provider_fixture().await;
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
        let (prepared, receipt) =
            admit_missing_runtime_receipt(&state, &db, &authority, parent).await;
        let dispatch = receipt.dispatch_identity.clone();
        state
            .sessions
            .append_model_turn_started(parent, "lost-runtime-turn".into())
            .await
            .unwrap();
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
        let observation = crate::WorkflowTurnReceiptObserver { state: &state }
            .observe_async(&bcode_workflow_store::AttemptReconciliationRequest {
                run_id: receipt.run_id.clone(),
                node_id: receipt.node_id.clone(),
                activation_id: receipt.activation_id.clone(),
                attempt: 1,
                dispatch_identity: dispatch.clone(),
                side_effect: prepared.side_effect,
                receipt: receipt.receipt.clone(),
            })
            .await
            .unwrap();
        assert!(matches!(
            observation,
            bcode_workflow_store::AttemptObservation::Cancelled
        ));
        settle_observation(
            &state,
            &PreparedActivationDispatch {
                activation: bcode_workflow_store::PendingActivation {
                    run_id: "new".into(),
                    node_id: receipt.node_id.clone(),
                    activation_id: receipt.activation_id.clone(),
                    dependency_generation: 0,
                    input: None,
                    node: definition.node(&receipt.node_id).unwrap().clone(),
                    created_at_ms: 1,
                },
                attempt: 1,
                dispatch_identity: dispatch.clone(),
                intent: prepared.intent,
            },
            &authority,
            observation,
        )
        .await
        .unwrap();
        assert!(
            owner_boundary::terminal_outcome(&db, "new", &dispatch)
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
    }
}
