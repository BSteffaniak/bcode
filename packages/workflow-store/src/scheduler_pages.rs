//! Typed, run-scoped scheduler discovery on the store's existing connection.
use super::{
    ActivationDispatchOwner, ActivationDispatchSummary, ControlSettlementSummary,
    NoopWorkflowDispatchFault, PendingActivation, WorkflowExecutionAuthority, WorkflowStore,
    WorkflowStoreError, run_graph, validate_id,
};
use crate::database;
use switchy_database::{
    DatabaseValue, Row,
    query::{FilterableQuery as _, SortDirection, select},
};

fn text(row: &Row, name: &str) -> Result<String, WorkflowStoreError> {
    match row.get(name) {
        Some(DatabaseValue::String(value)) => Ok(value),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "invalid scheduler column: {name}"
        ))),
    }
}
fn optional_json(row: &Row, name: &str) -> Result<Option<serde_json::Value>, WorkflowStoreError> {
    match row.get(name) {
        Some(DatabaseValue::Null) => Ok(None),
        Some(DatabaseValue::String(value)) => Ok(Some(serde_json::from_str(&value)?)),
        _ => Err(WorkflowStoreError::InvalidData(
            "invalid activation input".into(),
        )),
    }
}

fn integer(row: &Row, name: &str) -> Result<u64, WorkflowStoreError> {
    match row.get(name) {
        Some(DatabaseValue::Int64(value)) if value >= 0 => Ok(value.cast_unsigned()),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "invalid scheduler column: {name}"
        ))),
    }
}
fn rows(
    connection: &dyn switchy_database::Database,
    query: &switchy_database::query::SelectQuery<'_>,
) -> Result<Vec<Row>, WorkflowStoreError> {
    super::database::rows(connection, query)
}
/// One consistent read observation of run-level concurrency capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowConcurrencySnapshot {
    pub graph_revision: u64,
    pub concurrency_cap: Option<u32>,
    pub occupied_count: u64,
}

/// A bounded activation observation from the same snapshot as run capacity.
#[derive(Debug, Clone)]
pub struct WorkflowSchedulingActivation {
    pub node_id: String,
    pub activation_id: String,
    pub status: String,
    /// Latest canonical attempt, including whether a durable receipt exists.
    pub latest_attempt: Option<super::AttemptSummary>,
    pub automatic_retry: Option<super::AutomaticRetrySchedule>,
}

/// Consistent run capacity and activation page; separate calls are separate observations.
#[derive(Debug, Clone)]
pub struct WorkflowSchedulingSnapshot {
    pub capacity: WorkflowConcurrencySnapshot,
    pub paused: bool,
    pub activations: Vec<WorkflowSchedulingActivation>,
    pub next_activation_id: Option<String>,
}

impl WorkflowStore {
    /// Observe capacity, activation status, latest attempt and retry from one read transaction.
    ///
    /// This does not establish readiness or resource/dependency completeness.
    /// # Errors
    /// Rejects invalid bounds, missing runs and inconsistent canonical attempt identities.
    pub fn scheduling_snapshot(
        &self,
        run_id: &str,
        page: (Option<&str>, usize),
    ) -> Result<WorkflowSchedulingSnapshot, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let (after, limit) = page;
        if !(1..=128).contains(&limit) {
            return Err(WorkflowStoreError::InvalidData(
                "scheduler page must contain 1..=128 rows".into(),
            ));
        }
        if let Some(after) = after {
            validate_id("activation cursor", after)?;
        }
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let capacity = Self::concurrency_snapshot_in(db, run_id)?;
        let paused = Self::run_summary_in(db, run_id)?
            .is_some_and(|run| run.status == super::RunStatus::Paused);
        let mut query = select("workflow_activations")
            .projections(database::projections(&[
                "node_id",
                "activation_id",
                "status",
            ]))
            .where_eq("run_id", run_id)
            .sort("activation_id", SortDirection::Asc)
            .limit(limit + 1);
        if let Some(after) = after {
            query = query.where_gt("activation_id", after);
        }
        let mut discovered = rows(db, &query)?;
        let more = discovered.len() > limit;
        discovered.truncate(limit);
        let next_activation_id = if more {
            discovered
                .last()
                .map(|row| text(row, "activation_id"))
                .transpose()?
        } else {
            None
        };
        let activations = discovered
            .iter()
            .map(|row| Self::scheduling_activation(db, run_id, row))
            .collect::<Result<Vec<_>, _>>()?;
        super::database::commit(transaction)?;
        Ok(WorkflowSchedulingSnapshot {
            capacity,
            paused,
            activations,
            next_activation_id,
        })
    }

    fn scheduling_activation(
        db: &dyn switchy_database::Database,
        run_id: &str,
        row: &Row,
    ) -> Result<WorkflowSchedulingActivation, WorkflowStoreError> {
        let node_id = text(row, "node_id")?;
        let activation_id = text(row, "activation_id")?;
        let attempt = rows(
            db,
            &select("workflow_attempts")
                .projections(database::projections(&["dispatch_identity"]))
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id.clone())
                .where_eq("activation_id", activation_id.clone())
                .sort("attempt", SortDirection::Desc)
                .limit(1),
        )?;
        let latest_attempt = attempt
            .first()
            .map(|attempt| {
                Self::attempt_by_dispatch_identity_in(db, &text(attempt, "dispatch_identity")?)?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("snapshot attempt disappeared".into())
                    })
            })
            .transpose()?;
        let automatic_retry =
            super::automatic_retry_schedule(db, run_id, &node_id, &activation_id)?;
        Ok(WorkflowSchedulingActivation {
            node_id,
            activation_id,
            status: text(row, "status")?,
            latest_attempt,
            automatic_retry,
        })
    }
    /// Read capacity and graph revision from one `SQLite` snapshot, without authorizing work.
    /// # Errors
    /// Rejects missing or corrupt runs and invalid capacity representations.
    pub fn concurrency_snapshot(
        &self,
        run_id: &str,
    ) -> Result<WorkflowConcurrencySnapshot, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::concurrency_snapshot_in(&*transaction, run_id)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    pub(crate) fn concurrency_snapshot_in(
        db: &dyn switchy_database::Database,
        run_id: &str,
    ) -> Result<WorkflowConcurrencySnapshot, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let stored = rows(
            db,
            &select("workflow_runs")
                .projections(database::projections(&["concurrency_cap"]))
                .where_eq("run_id", run_id)
                .limit(1),
        )?;
        let run = stored
            .first()
            .ok_or_else(|| WorkflowStoreError::InvalidData("capacity run is missing".into()))?;
        let concurrency_cap = match run.get("concurrency_cap") {
            Some(DatabaseValue::Null) => None,
            Some(DatabaseValue::Int64(value)) => Some(
                u32::try_from(value)
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("invalid concurrency cap".into())
                    })?,
            ),
            _ => {
                return Err(WorkflowStoreError::InvalidData(
                    "invalid concurrency cap".into(),
                ));
            }
        };
        let graph_revision = run_graph::graph_revision(db, run_id)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("capacity graph is missing".into()))?;
        let occupied_count =
            super::typed_attempt_count(db, run_id, i64::MAX.cast_unsigned(), true)?;
        Ok(WorkflowConcurrencySnapshot {
            graph_revision,
            concurrency_cap,
            occupied_count,
        })
    }
    fn run_accepts_scheduler_discovery(&self, run_id: &str) -> Result<bool, WorkflowStoreError> {
        let run = rows(
            &self.connection,
            &select("workflow_runs")
                .projections(database::projections(&[
                    "status",
                    "cancellation_requested_at_ms",
                ]))
                .where_eq("run_id", run_id)
                .limit(1),
        )?;
        let Some(run) = run.first() else {
            return Ok(false);
        };
        Ok(text(run, "status")? == "running"
            && run.get("cancellation_requested_at_ms") == Some(DatabaseValue::Null))
    }
    /// Redispatch a bounded prepared read-only page, preserving progress across deferred owners.
    /// # Errors
    /// Returns authority, discovery, or non-deferred dispatch failures.
    pub async fn redispatch_owned_prepared_read_only_page_for_run<
        O: ActivationDispatchOwner + ?Sized,
    >(
        &mut self,
        owner: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        page: (Option<&str>, usize),
        now: u64,
    ) -> Result<(Vec<String>, Option<String>), WorkflowStoreError> {
        self.verify_execution_authority(run_id, authority)?;
        let (after, limit) = page;
        if !(1..=128).contains(&limit) {
            return Err(WorkflowStoreError::InvalidData(
                "scheduler page must contain 1..=128 rows".into(),
            ));
        }
        let mut query = select("workflow_attempts")
            .projections(database::projections(&["dispatch_identity"]))
            .where_eq("run_id", run_id)
            .where_eq("status", "prepared")
            .where_eq("side_effect", "read_only")
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(limit + 1);
        if let Some(after) = after {
            validate_id("dispatch cursor", after)?;
            query = query.where_gt("dispatch_identity", after);
        }
        let mut discovered = rows(&self.connection, &query)?;
        let more = discovered.len() > limit;
        discovered.truncate(limit);
        let cursor = if more {
            discovered
                .last()
                .map(|row| text(row, "dispatch_identity"))
                .transpose()?
        } else {
            None
        };
        let mut admitted = Vec::new();
        for row in discovered {
            let identity = text(&row, "dispatch_identity")?;
            match self
                .redispatch_examined_preparations(owner, run_id, authority, &[identity], now)
                .await
            {
                Ok(values) => admitted.extend(values),
                Err(WorkflowStoreError::OwnerAccessDeferred) => {}
                Err(error) => return Err(error),
            }
        }
        Ok((admitted, cursor))
    }
    /// Discover a bounded run-scoped page, advancing across all pending node kinds.
    /// # Errors
    /// Rejects invalid bounds, identities, or inconsistent executable bindings.
    pub fn pending_activations_page_for_run(
        &self,
        run_id: &str,
        page: (Option<&str>, usize),
    ) -> Result<(Vec<PendingActivation>, Option<String>), WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let (after, limit) = page;
        if !(1..=128).contains(&limit) {
            return Err(WorkflowStoreError::InvalidData(
                "scheduler page must contain 1..=128 rows".into(),
            ));
        }
        if let Some(after) = after {
            validate_id("activation cursor", after)?;
        }
        if !self.run_accepts_scheduler_discovery(run_id)? {
            return Ok((Vec::new(), None));
        }
        let mut query = select("workflow_activations")
            .projections(
                [
                    "node_id",
                    "activation_id",
                    "dependency_generation",
                    "input_json",
                    "created_at_ms",
                ]
                .into_iter()
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
            )
            .where_eq("run_id", run_id)
            .where_eq("status", "pending")
            .sort("activation_id", SortDirection::Asc)
            .limit(limit + 1);
        if let Some(after) = after {
            query = query.where_gt("activation_id", after);
        }
        let mut discovered = rows(&self.connection, &query)?;
        let more = discovered.len() > limit;
        discovered.truncate(limit);
        let cursor = if more {
            discovered
                .last()
                .map(|row| text(row, "activation_id"))
                .transpose()?
        } else {
            None
        };
        let pending = discovered
            .iter()
            .map(|row| self.pending_scheduler_activation(run_id, row))
            .collect::<Result<Vec<_>, WorkflowStoreError>>()?;
        Ok((pending, cursor))
    }
    /// Dispatch one fenced keyset page; the cursor includes blocked work.
    /// # Errors
    /// Returns discovery, authority, admission, or owner dispatch failures.
    pub async fn dispatch_owned_pending_activations_page_for_run<
        O: ActivationDispatchOwner + ?Sized,
    >(
        &mut self,
        owner: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        page: (Option<&str>, usize),
        now: u64,
    ) -> Result<(ActivationDispatchSummary, Option<String>), WorkflowStoreError> {
        self.verify_execution_authority(run_id, authority)?;
        let (pending, cursor) = self.pending_activations_page_for_run(run_id, page)?;
        let summary = self
            .dispatch_activations_with_fault(
                owner,
                &NoopWorkflowDispatchFault,
                pending,
                now,
                Some(authority),
            )
            .await?;
        Ok((summary, cursor))
    }
    /// Settle deterministic nodes from one fenced run-scoped keyset page.
    /// # Errors
    /// Returns discovery, authority, or atomic settlement failures.
    pub fn settle_owned_pending_control_nodes_page(
        &mut self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        page: (Option<&str>, usize),
        now: u64,
    ) -> Result<(ControlSettlementSummary, Option<String>), WorkflowStoreError> {
        self.verify_execution_authority(run_id, authority)?;
        let (pending, cursor) = self.pending_activations_page_for_run(run_id, page)?;
        Ok((self.settle_control_activations(pending, now)?, cursor))
    }
    fn pending_scheduler_activation(
        &self,
        run_id: &str,
        row: &switchy_database::Row,
    ) -> Result<PendingActivation, WorkflowStoreError> {
        let activation_id = text(row, "activation_id")?;
        let node_id = text(row, "node_id")?;
        let member = rows(
            &self.connection,
            &select("workflow_fan_out_members")
                .projections(database::projections(&[
                    "controller_node_id",
                    "controller_activation_id",
                ]))
                .where_eq("run_id", run_id)
                .where_eq("member_activation_id", activation_id.clone())
                .limit(1),
        )?;
        let node = if let Some(member) = member.first() {
            let controller = self
                .activation_graph_node(
                    run_id,
                    &text(member, "controller_node_id")?,
                    &text(member, "controller_activation_id")?,
                )?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData("missing fan-out controller binding".into())
                })?;
            let config: bcode_workflow::WorkflowFanOutConfiguration =
                serde_json::from_value(controller.node.configuration)?;
            let mut node = *config.member_node;
            node.id.clone_from(&node_id);
            node
        } else {
            run_graph::reconciled_activation_exit(
                &self.connection,
                run_id,
                &node_id,
                &activation_id,
            )?;
            self.activation_graph_node(run_id, &node_id, &activation_id)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData("missing executable binding".into())
                })?
                .node
        };
        let input = optional_json(row, "input_json")?;
        Ok(PendingActivation {
            run_id: run_id.into(),
            node_id,
            activation_id,
            dependency_generation: integer(row, "dependency_generation")?,
            input,
            node,
            created_at_ms: integer(row, "created_at_ms")?,
        })
    }
}
