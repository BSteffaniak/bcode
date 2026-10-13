//! Fenced bounded branch/repeat control settlement using shared predicate semantics.
use super::{
    ACTIVATIONS, Database, DatabaseValue, Error, FilterableQuery, GRAPHS, NodeKind, RUNS,
    TransactionMode, Value, WorkflowExecutionAuthority, activation, decode, encode,
    propagate_paged, schema, text,
};

/// Settle one exact control activation; repeats create fresh body generations atomically.
///
/// Each call advances at most one control and its bounded admitted body. Input/approval and
/// nested repeat bodies remain explicitly unsupported, not treated as successful leaves.
/// # Errors
/// Rejects stale identities/authority, unsupported control topology, exhausted fail-policy cycles,
/// invalid transforms/schemas or storage failures.
pub async fn settle(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    id: &str,
    revision: u64,
) -> Result<bool, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let root = owned_run(&*tx, run_id, owner).await?;
    let current = super::super::graph_pages::revision(&*tx, run_id).await?;
    if current != revision {
        return Err(Error::InvalidAdmission);
    }
    let Some(row) = pending_control(&*tx, run_id, node_id, id).await? else {
        return Ok(false);
    };
    if text(&root, "status")? != "paused" {
        return Err(Error::InvalidAdmission);
    }
    super::super::safety::ensure_admission_allowed(&*tx, run_id).await?;
    if super::restart::pending(&*tx, run_id).await? {
        return Err(Error::UnsupportedLifecycle);
    }
    let node = super::super::graph_pages::node(&*tx, run_id, current, node_id)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let input: Value = decode(&row, "input_json")?;
    let run: crate::NewWorkflowRun = decode(&root, "admission_json")?;
    let mut emitted_outcome = None;
    let value = match node.kind {
        NodeKind::Branch => {
            skip_branch(&*tx, run_id, &node, &input).await?;
            input.clone()
        }
        NodeKind::Repeat => {
            let generation = super::control_state::completed(&*tx, run_id, node_id)
                .await?
                .checked_add(1)
                .ok_or(Error::MaintenanceRequired)?;
            let predicate: bcode_workflow::PredicateExpression = serde_json::from_value(
                node.configuration
                    .get("predicate")
                    .cloned()
                    .ok_or(Error::InvalidAdmission)?,
            )
            .map_err(|_| Error::InvalidAdmission)?;
            let repeat = predicate
                .evaluate_value(&input)
                .map_err(|_| Error::InvalidAdmission)?;
            let max = node
                .configuration
                .get("max_iterations")
                .and_then(Value::as_u64)
                .ok_or(Error::InvalidAdmission)?;
            let bound = max.min(u64::from(run.limits.cycle_cap));
            record_repeat(&*tx, run_id, node_id, id, generation, false).await?;
            super::control_state::record(&*tx, run_id, node_id, generation).await?;
            if repeat && generation < bound {
                restart(&*tx, &run, current, node_id, &input).await?;
                tx.commit().await?;
                return Ok(true);
            }
            let policy = repeat_policy(&node)?;
            if repeat && policy == bcode_workflow::WorkflowRepeatExhaustionPolicy::Fail {
                record_repeat(&*tx, run_id, node_id, id, generation, true).await?;
                super::control_state::fail(&*tx, run_id, id).await?;
                tx.commit().await?;
                return Ok(true);
            }
            if policy == bcode_workflow::WorkflowRepeatExhaustionPolicy::EmitOutcome {
                let outcome = bcode_workflow::WorkflowRepeatOutcome {
                    version: bcode_workflow::WORKFLOW_REPEAT_OUTCOME_VERSION,
                    outcome: if repeat {
                        bcode_workflow::WorkflowRepeatOutcomeKind::IterationLimitReached
                    } else {
                        bcode_workflow::WorkflowRepeatOutcomeKind::ConditionCleared
                    },
                    iterations_completed: generation,
                    max_iterations: max,
                    cycle_cap: u64::from(run.limits.cycle_cap),
                    effective_iteration_bound: bound,
                    value: input.clone(),
                };
                let value = serde_json::to_value(&outcome).map_err(|_| Error::InvalidAdmission)?;
                emitted_outcome = Some(outcome);
                value
            } else {
                input.clone()
            }
        }
        _ => return Err(Error::UnsupportedLifecycle),
    };
    commit_control(&*tx, &run, &node, id, current, &value).await?;
    if let Some(outcome) = emitted_outcome {
        super::control_state::repeat_outcome::record_control_in(&*tx, owner, run_id, id, &outcome)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

fn repeat_policy(
    node: &bcode_workflow::NodeDefinition,
) -> Result<bcode_workflow::WorkflowRepeatExhaustionPolicy, Error> {
    node.configuration
        .get("exhaustion_policy")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| Error::InvalidAdmission)
        .map(Option::unwrap_or_default)
}

async fn record_repeat(
    db: &dyn Database,
    run: &str,
    node: &str,
    activation: &str,
    generation: u64,
    failed: bool,
) -> Result<(), Error> {
    use super::super::journal::public_events::control::{Commitment, Operation, Outcome, record};
    let key = encode(&(run, node))?;
    let source = if failed {
        ("workflow_control_failures_v2", activation, 1)
    } else {
        (
            "workflow_repeat_cycles_v2",
            key.as_str(),
            i64::try_from(generation).map_err(|_| Error::InvalidAdmission)?,
        )
    };
    record(
        db,
        Commitment {
            run,
            node,
            activation,
            operation: if failed {
                Operation::RepeatFailed
            } else {
                Operation::RepeatAdvanced
            },
            outcome: if failed {
                Outcome::Failed
            } else {
                Outcome::Continued
            },
            time: u64::try_from(super::super::runs::timestamp()?)
                .map_err(|_| Error::InvalidAdmission)?,
            source: Some(source),
        },
    )
    .await
}

async fn restart(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    revision: u64,
    repeat: &str,
    input: &Value,
) -> Result<(), Error> {
    super::restart::begin(db, run, revision, repeat, input).await?;
    super::restart::drain_in(db, &run.run_id, 32).await?;
    Ok(())
}

async fn owned_run(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<switchy_database::Row, Error> {
    let rows = db
        .select(RUNS)
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(db)
        .await?;
    let root = rows.into_iter().next().ok_or(Error::InvalidAdmission)?;
    if text(&root, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    Ok(root)
}

async fn pending_control(
    db: &dyn Database,
    run: &str,
    node: &str,
    id: &str,
) -> Result<Option<switchy_database::Row>, Error> {
    let row = activation(db, run, node)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    if text(&row, "identity")? != id {
        return Err(Error::InvalidAdmission);
    }
    Ok((row.get("output_json") == Some(DatabaseValue::Null)).then_some(row))
}

async fn skip_branch(
    db: &dyn Database,
    run: &str,
    node: &bcode_workflow::NodeDefinition,
    input: &Value,
) -> Result<(), Error> {
    let predicate: bcode_workflow::PredicateExpression = serde_json::from_value(
        node.configuration
            .get("predicate")
            .cloned()
            .ok_or(Error::InvalidAdmission)?,
    )
    .map_err(|_| Error::InvalidAdmission)?;
    let selected = predicate
        .evaluate_value(input)
        .map_err(|_| Error::InvalidAdmission)?;
    let list = node
        .configuration
        .get(if selected {
            "false_nodes"
        } else {
            "true_nodes"
        })
        .and_then(Value::as_array)
        .ok_or(Error::InvalidAdmission)?;
    for id in list {
        let id = id.as_str().ok_or(Error::InvalidAdmission)?;
        if activation(db, run, id).await?.is_some() {
            return Err(Error::UnsupportedLifecycle);
        }
        // A skipped activation has no supplied executable input. Preserve that distinction.
        super::insert_skipped(db, run, id).await?;
    }
    Ok(())
}

async fn terminal_control(
    db: &dyn Database,
    revision: u64,
    run_id: &str,
    node_id: &str,
    value: &Value,
) -> Result<(), Error> {
    let definition = super::super::graph_edges::boundary(db, run_id, revision).await?;
    if definition.exits.iter().any(|id| id == node_id) {
        schema(&definition.output.schema, value)?;
        db.update(GRAPHS)
            .value("terminal_json", encode(value)?)
            .where_eq("run_id", run_id)
            .execute(db)
            .await?;
    }
    Ok(())
}

async fn commit_control(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    node: &bcode_workflow::NodeDefinition,
    id: &str,
    revision: u64,
    value: &Value,
) -> Result<(), Error> {
    let run_id = run.run_id.as_str();
    let node_id = node.id.as_str();
    schema(&node.output.schema, value)?;
    super::terminal_scan::invalidate(db, run_id).await?;
    db.update(ACTIVATIONS)
        .value("output_json", encode(value)?)
        .where_eq("identity", id)
        .execute(db)
        .await?;
    super::completion::retain_control(db, run_id, node, id, value).await?;
    propagate_paged(db, run, revision, node_id, value).await?;
    terminal_control(db, revision, run_id, node_id, value).await?;
    Ok(())
}
