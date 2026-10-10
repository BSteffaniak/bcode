//! Bounded representation execution pages using the existing production owner interfaces.
use super::{ServerError, ServerState, open, representation, retained_store, unsupported};
use bcode_workflow_store::{
    ActivationDispatchOwner, AsyncAttemptStatusObserver, PreparedActivationDispatch,
    PreparedAttempt,
};
use representation::attempts::owner_boundary;
#[path = "driver_admission/mod.rs"]
mod admission;
#[path = "driver_scan/mod.rs"]
mod leaf_scan;
use representation::admission_observation::{Category, Phase};

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
    let mut fanouts = leaf_scan::FanOutScan::default();
    let mut leaves = leaf_scan::Scan::default();
    loop {
        if !pass_allowed(state, &db, run_id, &guard.authority).await? {
            return Ok(());
        }
        let (advanced, frontier_pending) =
            advance_frontier(retained.path(), &db, &guard.authority, run_id).await?;
        leaves.observe_progress(advanced);
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
        // Drain lifecycle pages without revisiting unchanged receipt-backed leaves.
        // Actual frontier, leaf or controller progress reopens the cursor after exhaustion.
        leaves.restart_if_progressed();
        if !leaves.exhausted {
            let (revision, page) =
                super::backpressure::queued(&crate::workflow_store_path(state), || {
                    representation::graph::ready_nodes(&db, run_id, leaves.after.as_deref(), 32)
                })
                .await
                .map_err(super::backpressure::normalize)?;
            let next = page
                .last()
                .map(|(activation, _)| activation.node_id.clone());
            let progressed =
                drive_page(state, &db, &guard.authority, run_id, revision, page).await?;
            leaves.finish_page(next, progressed);
        }
        let (fanout_progress, fanout_pending) = fanouts
            .advance(retained.path(), &db, &guard.authority, run_id)
            .await?;
        leaves.observe_progress(fanout_progress);
        leaves.restart_if_progressed();
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
        leaves.observe_progress(control_progress);
        leaves.restart_if_progressed();
        if leaves.exhausted
            && !fanout_pending
            && !control_pending
            && !super::backpressure::queued(&crate::workflow_store_path(state), || {
                representation::graph::terminal_scan::pending(&db, run_id)
            })
            .await
            .map_err(super::backpressure::normalize)?
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    finish_pass(retained.path(), &db, &guard.authority, run_id).await
}

async fn pass_allowed(
    state: &ServerState,
    db: &dyn switchy_database::Database,
    run_id: &str,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> Result<bool, ServerError> {
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Ok(false);
    }
    super::backpressure::queued(&crate::workflow_store_path(state), || {
        owner_boundary::verify(db, run_id, authority)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    let paused = super::backpressure::queued(&crate::workflow_store_path(state), || {
        representation::controls::user_paused(db, run_id)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    Ok(!paused)
}

async fn next_agent_attempt(
    state: &ServerState,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    pending: &bcode_workflow_store::PendingActivation,
) -> Result<Option<u32>, ServerError> {
    super::backpressure::queued(&crate::workflow_store_path(state), || {
        owner_boundary::next_retry_attempt(
            db,
            authority,
            &pending.run_id,
            &pending.node_id,
            &pending.activation_id,
        )
    })
    .await
    .map_err(super::backpressure::normalize)
}

async fn plan_agent_attempt(
    state: &std::sync::Arc<ServerState>,
    pending: &bcode_workflow_store::PendingActivation,
) -> Result<Option<bcode_workflow_store::ActivationDispatchPlan>, ServerError> {
    #[cfg(test)]
    let _timer = super::fanout_stages::StageTimer::new(
        &pending.run_id,
        None,
        None,
        super::fanout_stages::FanoutAdmissionStage::OwnerPlan,
    );
    Ok(crate::WorkflowActivationOwner { state }
        .plan(pending)
        .await?)
}

async fn agent(
    state: &std::sync::Arc<ServerState>,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
    activation: representation::graph::Activation,
    node: bcode_workflow::NodeDefinition,
) -> Result<bool, ServerError> {
    #[cfg(test)]
    let stage_timer = super::fanout_stages::StageTimer::new(
        run_id,
        None,
        None,
        super::fanout_stages::FanoutAdmissionStage::DriverPrepare,
    );
    let pending = pending_activation(run_id, activation, node);
    let retry_attempt = next_agent_attempt(state, db, authority, &pending).await?;
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
        let Some(plan) = plan_agent_attempt(state, &pending).await? else {
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
    #[cfg(test)]
    drop(stage_timer);
    let identity = prepared.dispatch_identity();
    let dispatch = PreparedActivationDispatch {
        activation: pending,
        attempt: prepared.attempt,
        dispatch_identity: identity.clone(),
        intent: prepared.intent.clone(),
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
        if let Some(receipt) =
            super::recovery::recover_acceptance(state, &prepared, authority).await?
        {
            return observe_existing(state, authority, &dispatch, prepared.side_effect, receipt)
                .await;
        }
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
    let receipt = match owner.dispatch(dispatch).await {
        Ok(receipt) => receipt,
        Err(error) => {
            admission::record(
                state,
                authority,
                dispatch,
                Phase::OwnerDispatch,
                admission::owner_category(&error),
            )
            .await?;
            return Err(error.into());
        }
    };
    let persisted = super::owner::persist_receipt(
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
    .await;
    if let Err(error) = persisted {
        admission::record(
            state,
            authority,
            dispatch,
            Phase::ReceiptPersistence,
            Category::ReceiptPersistenceUnavailable,
        )
        .await?;
        return Err(error.into());
    }
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
            // Fan-out lifecycle pages are drained separately from leaf receipt observation.
            return Ok(false);
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
        #[cfg(test)]
        let diagnostic_node = activation.node_id.clone();
        let operation = drive_activation(state, db, authority, run_id, revision, activation, node);
        #[cfg(test)]
        let advanced = {
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => result,
                () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    // Diagnostic only: retain and await the original operation. A slow
                    // admission may already have effects and must not be restarted.
                    eprintln!("workflow activation still awaiting run={run_id}, node={diagnostic_node}");
                    operation.await
                }
            }
        };
        #[cfg(not(test))]
        let advanced = operation.await;
        progressed |= advanced?;
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
