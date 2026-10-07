//! Bounded representation execution pages using the existing production owner interfaces.
use super::{ServerError, ServerState, open, representation, retained_store, unsupported};
use bcode_workflow_store::{
    ActivationDispatchOwner, AsyncAttemptStatusObserver, PreparedActivationDispatch,
    PreparedAttempt,
};
use representation::attempts::owner_boundary;

/// Drive a canonical new run under an already-verified local authority. No admission bypass.
pub async fn drive(state: &std::sync::Arc<ServerState>, run_id: &str) -> Result<(), ServerError> {
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let guard = super::owner::local_authority(state, run_id)
        .await?
        .ok_or_else(unsupported)?;
    if super::backpressure::queued(&crate::workflow_store_path(state), || {
        representation::safety::recovery_required(&db, run_id)
    })
    .await
    .map_err(super::backpressure::normalize)?
    {
        return Box::pin(super::recovery::reconcile(state, run_id, &guard.authority)).await;
    }
    let mut cancellation_scanned = false;
    let mut cancellation_cursor = None;
    let mut controllers = super::parallel::Scan::default();
    let mut after = None;
    let mut scan_progress = false;
    loop {
        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        super::backpressure::queued(&crate::workflow_store_path(state), || {
            owner_boundary::verify(&db, run_id, &guard.authority)
        })
        .await
        .map_err(super::backpressure::normalize)?;
        if super::backpressure::queued(&crate::workflow_store_path(state), || {
            representation::controls::user_paused(&db, run_id)
        })
        .await
        .map_err(super::backpressure::normalize)?
        {
            return Ok(());
        }
        let (advanced, frontier_pending) =
            advance_frontier(retained.path(), &db, &guard.authority, run_id).await?;
        scan_progress |= advanced;
        if frontier_pending {
            tokio::task::yield_now().await;
            continue;
        }
        if super::backpressure::queued(&crate::workflow_store_path(state), || {
            representation::runs::terminalize(&db, &guard.authority, run_id)
        })
        .await
        .map_err(super::backpressure::normalize)?
        .is_some()
        {
            return Ok(());
        }
        let (revision, page) =
            super::backpressure::queued(&crate::workflow_store_path(state), || {
                representation::graph::ready_nodes(&db, run_id, after.as_deref(), 32)
            })
            .await
            .map_err(super::backpressure::normalize)?;
        let next = page
            .last()
            .map(|(activation, _)| activation.node_id.clone());
        let progressed = drive_page(state, &db, &guard.authority, run_id, revision, page).await?;
        scan_progress |= progressed;
        let (control_progress, control_pending) = advance_controllers(
            state,
            &db,
            &guard.authority,
            run_id,
            &mut controllers,
            &mut cancellation_cursor,
            &mut cancellation_scanned,
        )
        .await?;
        scan_progress |= control_progress;
        if next.is_none() {
            if !scan_progress
                && !control_pending
                && !super::backpressure::queued(&crate::workflow_store_path(state), || {
                    representation::graph::terminal_scan::pending(&db, run_id)
                })
                .await
                .map_err(super::backpressure::normalize)?
            {
                break;
            }
            scan_progress = false;
            after = None;
        } else {
            after = next;
        }
        tokio::task::yield_now().await;
    }
    finish_pass(retained.path(), &db, &guard.authority, run_id).await
}

async fn agent(
    state: &std::sync::Arc<ServerState>,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
    activation: representation::graph::Activation,
    node: bcode_workflow::NodeDefinition,
) -> Result<bool, ServerError> {
    let pending = pending_activation(run_id, activation, node);
    let retry_attempt = super::backpressure::queued(&crate::workflow_store_path(state), || {
        owner_boundary::next_retry_attempt(
            db,
            authority,
            run_id,
            &pending.node_id,
            &pending.activation_id,
        )
    })
    .await
    .map_err(super::backpressure::normalize)?;
    let existing = super::backpressure::queued(&crate::workflow_store_path(state), || {
        owner_boundary::latest_prepared(db, run_id, &pending.node_id, &pending.activation_id)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    let existing = existing.filter(|_| retry_attempt.is_none());
    let recovered = existing.is_some();
    let prepared = if let Some(existing) = existing {
        let existing_identity = existing.dispatch_identity();
        if super::backpressure::queued(&crate::workflow_store_path(state), || {
            owner_boundary::terminal_outcome(db, run_id, &existing_identity)
        })
        .await
        .map_err(super::backpressure::normalize)?
        .is_some()
        {
            // Failed/cancelled activations can remain output-less. A terminal ledger row
            // is not new work, a retry grant, or progress on another scheduler pass.
            return Ok(false);
        }
        existing
    } else {
        let Some(plan) = crate::WorkflowActivationOwner { state }
            .plan(&pending)
            .await?
        else {
            return Ok(false);
        };
        let prepared = PreparedAttempt {
            run_id: run_id.into(),
            node_id: pending.node_id.clone(),
            activation_id: pending.activation_id.clone(),
            attempt: retry_attempt.unwrap_or(1),
            side_effect: plan.side_effect,
            intent: plan.intent,
            prepared_at_ms: crate::current_unix_millis(),
        };
        match super::backpressure::queued(&crate::workflow_store_path(state), || {
            representation::attempts::prepare(db, authority, &prepared)
        })
        .await
        {
            Ok(_) => {}
            Err(representation::RepresentationError::CapacityUnavailable) => return Ok(false),
            Err(error) => return Err(super::backpressure::normalize(error)),
        }
        prepared
    };
    let identity = prepared.dispatch_identity();
    let dispatch = PreparedActivationDispatch {
        activation: pending,
        attempt: prepared.attempt,
        dispatch_identity: identity.clone(),
        intent: prepared.intent,
    };
    if let Some(receipt) = super::backpressure::queued(&crate::workflow_store_path(state), || {
        owner_boundary::receipt(db, &identity)
    })
    .await
    .map_err(super::backpressure::normalize)?
    {
        return observe_existing(state, authority, &dispatch, prepared.side_effect, receipt).await;
    }
    // Never replay an ambiguous mutating preparation without a committed receipt.
    if recovered && prepared.side_effect == bcode_workflow::DispatchSideEffect::Mutating {
        return Err(unsupported());
    }
    super::backpressure::queued(&crate::workflow_store_path(state), || {
        owner_boundary::verify(db, run_id, authority)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    dispatch_and_record(
        state,
        authority,
        &dispatch,
        &crate::WorkflowActivationOwner { state },
    )
    .await?;
    Ok(true)
}

pub async fn dispatch_and_record(
    state: &ServerState,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    dispatch: &PreparedActivationDispatch,
    owner: &impl ActivationDispatchOwner,
) -> Result<(), ServerError> {
    // Admission time denotes the handoff, not the later receipt persistence time.
    // Synchronous plugin owners may already return a terminal result from dispatch.
    let admitted_at_ms = crate::current_unix_millis();
    let receipt = owner.dispatch(dispatch).await?;
    super::owner::persist_receipt(
        state,
        &bcode_workflow_store::DispatchReceipt {
            run_id: dispatch.activation.run_id.clone(),
            node_id: dispatch.activation.node_id.clone(),
            activation_id: dispatch.activation.activation_id.clone(),
            attempt: dispatch.attempt,
            dispatch_identity: dispatch.dispatch_identity.clone(),
            receipt,
            admitted_at_ms,
        },
        authority,
    )
    .await?;
    Ok(())
}

/// Advance one transaction quantum before interpreting graph readiness or quiescence.
async fn advance_frontier(
    path: &std::path::Path,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
) -> Result<(bool, bool), ServerError> {
    if !super::backpressure::queued(path, || {
        representation::graph::frontier::pending(db, run_id)
    })
    .await
    .map_err(super::backpressure::normalize)?
    {
        return Ok((false, false));
    }
    let advanced = super::backpressure::queued(path, || {
        representation::graph::frontier::advance(db, authority, run_id, 32)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    let pending = super::backpressure::queued(path, || {
        representation::graph::frontier::pending(db, run_id)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    Ok((advanced > 0, pending))
}

async fn drive_activation(
    state: &std::sync::Arc<ServerState>,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
    revision: u64,
    activation: representation::graph::Activation,
    node: bcode_workflow::NodeDefinition,
) -> Result<bool, ServerError> {
    match node.kind {
        bcode_workflow::NodeKind::Branch | bcode_workflow::NodeKind::Repeat => {
            return super::backpressure::retry(|| {
                representation::graph::control::settle(
                    db,
                    authority,
                    run_id,
                    &activation.node_id,
                    &activation.activation_id,
                    revision,
                )
            })
            .await
            .map_err(super::backpressure::normalize);
        }
        bcode_workflow::NodeKind::Agent
        | bcode_workflow::NodeKind::PluginBlock
        | bcode_workflow::NodeKind::WorkflowCall => {
            return agent(state, db, authority, run_id, activation, node).await;
        }
        bcode_workflow::NodeKind::FanOut => {
            return super::fanout::advance(
                &crate::workflow_store_path(state),
                db,
                authority,
                run_id,
                revision,
                &activation,
            )
            .await;
        }
        // Parallel joins settle atomically with predecessor outputs in the store.
        bcode_workflow::NodeKind::Input
        | bcode_workflow::NodeKind::Approval
        | bcode_workflow::NodeKind::Parallel => {
            // Human gates remain pending until an explicit application resolution.
            // Their presence must not prevent unrelated ready work from being scanned.
        }
        _ => return Err(unsupported()),
    }
    Ok(false)
}

async fn activation_ready(
    db: &dyn switchy_database::Database,
    run_id: &str,
    activation: &representation::graph::Activation,
) -> Result<bool, ServerError> {
    Ok(super::backpressure::retry(|| {
        representation::graph::revalidate(
            db,
            run_id,
            &activation.node_id,
            &activation.activation_id,
        )
    })
    .await
    .map_err(super::backpressure::normalize)?
        == representation::graph::ActivationValidity::Ready)
}

async fn advance_controllers(
    state: &ServerState,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
    controllers: &mut super::parallel::Scan,
    cancellation_cursor: &mut Option<String>,
    cancellation_scanned: &mut bool,
) -> Result<(bool, bool), ServerError> {
    let (controller_progress, controller_pending) = controllers
        .advance(&crate::workflow_store_path(state), db, authority, run_id)
        .await?;
    if controller_progress {
        *cancellation_scanned = false;
    }
    let cancellation_pending = if *cancellation_scanned {
        false
    } else {
        let pending = super::parallel::signal_cancellations(
            state,
            db,
            authority,
            run_id,
            cancellation_cursor,
        )
        .await?;
        *cancellation_scanned = !pending;
        pending
    };
    Ok((
        controller_progress,
        controller_pending || cancellation_pending,
    ))
}

fn pending_activation(
    run_id: &str,
    activation: representation::graph::Activation,
    node: bcode_workflow::NodeDefinition,
) -> bcode_workflow_store::PendingActivation {
    bcode_workflow_store::PendingActivation {
        run_id: run_id.into(),
        node_id: activation.node_id,
        activation_id: activation.activation_id,
        dependency_generation: activation.generation,
        input: Some(activation.input),
        node,
        created_at_ms: activation.created_at_ms,
    }
}

async fn drive_page(
    state: &std::sync::Arc<ServerState>,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
    revision: u64,
    page: Vec<(
        representation::graph::Activation,
        bcode_workflow::NodeDefinition,
    )>,
) -> Result<bool, ServerError> {
    let mut progressed = false;
    for (activation, node) in page {
        if !activation_ready(db, run_id, &activation).await? {
            progressed = true;
            continue;
        }
        let started = std::time::Instant::now();
        let diagnostic_node = activation.node_id.clone();
        progressed |=
            drive_activation(state, db, authority, run_id, revision, activation, node).await?;
        #[cfg(test)]
        if run_id == "normal-fanout" {
            eprintln!("admission-cost {diagnostic_node} {:?}", started.elapsed());
        }
        #[cfg(not(test))]
        let _ = (started, diagnostic_node);
    }
    Ok(progressed)
}

async fn finish_pass(
    path: &std::path::Path,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
) -> Result<(), ServerError> {
    super::backpressure::queued(path, || {
        representation::runs::terminalize(db, authority, run_id)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    Ok(())
}

pub async fn advisory_authority_current(
    state: &ServerState,
    run_id: &str,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> bool {
    if !crate::workflow_operations::authority_targets_current_daemon(state, authority) {
        return false;
    }
    let Ok(retained) = retained_store(state).await else {
        return false;
    };
    let Ok(db) = open(retained.path(), false) else {
        return false;
    };
    if super::backpressure::queued(retained.path(), || {
        owner_boundary::verify(&db, run_id, authority)
    })
    .await
    .is_err()
    {
        return false;
    }
    matches!(
        super::backpressure::queued(retained.path(), || {
            representation::safety::cancellation_requested_at(&db, run_id)
        })
        .await,
        Ok(None)
    )
}

async fn observe_existing(
    state: &std::sync::Arc<ServerState>,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    dispatch: &PreparedActivationDispatch,
    side_effect: bcode_workflow::DispatchSideEffect,
    receipt: bcode_workflow_store::DispatchReceipt,
) -> Result<bool, ServerError> {
    let observation = crate::WorkflowTurnReceiptObserver { state }
        .observe_async(&bcode_workflow_store::AttemptReconciliationRequest {
            run_id: receipt.run_id,
            node_id: receipt.node_id,
            activation_id: receipt.activation_id,
            attempt: receipt.attempt,
            dispatch_identity: receipt.dispatch_identity,
            side_effect,
            receipt: receipt.receipt,
        })
        .await?;
    let progressed = matches!(
        observation,
        bcode_workflow_store::AttemptObservation::Succeeded { .. }
            | bcode_workflow_store::AttemptObservation::Failed { .. }
            | bcode_workflow_store::AttemptObservation::Cancelled
    );
    let failed = matches!(
        observation,
        bcode_workflow_store::AttemptObservation::Failed { .. }
    );
    let cancelled = matches!(
        observation,
        bcode_workflow_store::AttemptObservation::Cancelled
    );
    super::owner::settle_observation(state, dispatch, authority, observation).await?;
    if progressed
        && !cancelled
        && crate::workflow_operations::authority_targets_current_daemon(state, authority)
    {
        let retained = retained_store(state).await?;
        let db = open(retained.path(), false)?;
        let run_id = &dispatch.activation.run_id;
        if super::backpressure::queued(retained.path(), || {
            owner_boundary::verify(&db, run_id, authority)
        })
        .await
        .is_ok()
        {
            let session = super::backpressure::queued(retained.path(), || {
                representation::runs::association(&db, run_id)
            })
            .await;
            if let Ok(Some(session)) = session
                && let Ok(session_id) = session.parse::<crate::SessionId>()
            {
                Box::pin(crate::observe_settled_workflow_activation(
                    state, session_id, dispatch, authority, failed,
                ))
                .await;
            }
        }
    }
    Ok(progressed)
}

#[cfg(test)]
mod advisory_fence_tests {
    #[tokio::test]
    async fn missing_foreign_and_stale_workflow_owners_cannot_dispatch_advice() {
        let state = crate::tests::test_server_state(crate::SessionManager::default());
        let foreign = bcode_workflow_store::WorkflowExecutionAuthority {
            target_artifact_id: "foreign-artifact".into(),
            daemon_instance_id: "foreign-daemon".into(),
            generation: 1,
            fencing_token: "foreign-token".into(),
        };
        assert!(!super::advisory_authority_current(&state, "absent", &foreign).await);
        let current = bcode_workflow_store::WorkflowExecutionAuthority {
            target_artifact_id: crate::workflow_operations::current_artifact_id(&state),
            daemon_instance_id: state.daemon_status.instance_id.clone(),
            generation: 1,
            fencing_token: "stale-token".into(),
        };
        assert!(!super::advisory_authority_current(&state, "absent", &current).await);
        assert!(state.workflow_progress_windows.lock().unwrap().is_empty());
        drop(state);
    }
}
