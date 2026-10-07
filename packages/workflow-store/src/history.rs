//! Bounded workflow run, activation, attempt and event reads.
use super::*;
use switchy_database::query::{
    FilterableQuery, SelectQuery, SortDirection, bounded_text, byte_length, coalesce, count_all,
    identifier, join_columns, max, qualified_column, select, storage_type, where_and, where_eq,
    where_eq_expression, where_gt, where_lt, where_not_eq, where_or,
};
use switchy_database::{Database, DatabaseValue as V, Row, TransactionMode};

// All projections are typed: adding project_as replaces the legacy column list.
fn history_columns<'a>(table: &'a str, columns: &[&str]) -> SelectQuery<'a> {
    columns.iter().fold(select(table), |query, column| {
        query.project_as(qualified_column(table, column), *column)
    })
}

fn history_text<'a>(
    query: SelectQuery<'a>,
    table: &str,
    column: &str,
    alias: &str,
    limit: usize,
) -> SelectQuery<'a> {
    query
        .project_as(
            bounded_text(qualified_column(table, column), limit as u64, V::Null),
            alias,
        )
        .project_as(
            Box::new(where_eq_expression(
                qualified_column(table, column),
                V::Null,
            )) as Box<dyn switchy_database::query::Expression>,
            format!("{alias}_null"),
        )
}

fn history_optional_text(row: &Row, column: &str) -> Result<Option<String>, WorkflowStoreError> {
    let value = database::optional_text(row, column)?;
    if value.is_none() && database::unsigned(row, &format!("{column}_null"))? != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "invalid workflow history text size or storage class: {column}"
        )));
    }
    Ok(value)
}

fn history_required_text(row: &Row, column: &str) -> Result<String, WorkflowStoreError> {
    history_optional_text(row, column)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!("missing workflow history text: {column}"))
    })
}

fn history_limit(limit: i64) -> Result<usize, WorkflowStoreError> {
    usize::try_from(limit)
        .map_err(|_| WorkflowStoreError::InvalidData("invalid workflow history page limit".into()))
}

fn history_u32(row: &Row, column: &str) -> Result<u32, WorkflowStoreError> {
    u32::try_from(database::unsigned(row, column)?).map_err(|_| {
        WorkflowStoreError::InvalidData(format!("workflow history integer exceeds u32: {column}"))
    })
}

fn history_flag(row: &Row, column: &str) -> Result<bool, WorkflowStoreError> {
    match database::unsigned(row, column)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "invalid workflow history flag: {column}"
        ))),
    }
}

fn history_runs_query() -> SelectQuery<'static> {
    let mut query = history_columns(
        "workflow_runs",
        &[
            "definition_version",
            "parent_session_generation",
            "single_active",
            "cancellation_requested_at_ms",
            "created_at_ms",
            "updated_at_ms",
        ],
    );
    for (column, limit) in [
        ("run_id", MAX_ID_BYTES),
        ("definition_id", MAX_ID_BYTES),
        ("workspace_snapshot", MAX_ID_BYTES),
        ("parent_session_id", MAX_ID_BYTES),
        ("owner_plugin_id", MAX_ID_BYTES),
        ("workflow_kind", MAX_ID_BYTES),
        ("scope_key", MAX_ID_BYTES),
        ("display_label", MAX_DISPLAY_LABEL_BYTES),
        ("authored_provenance_json", MAX_INLINE_JSON_BYTES),
        ("terminal_output_id", MAX_ID_BYTES),
        ("terminal_output_checksum_sha256", MAX_ID_BYTES),
        ("authorization_profile_json", MAX_INLINE_JSON_BYTES),
        ("authorization_ceiling", MAX_ID_BYTES),
        ("status", MAX_ID_BYTES),
    ] {
        query = history_text(query, "workflow_runs", column, column, limit);
    }
    query
}

fn history_run(row: &Row) -> Result<WorkflowRunSummary, WorkflowStoreError> {
    for column in [
        "parent_session_id",
        "owner_plugin_id",
        "workflow_kind",
        "scope_key",
        "display_label",
        "authored_provenance_json",
        "terminal_output_id",
        "terminal_output_checksum_sha256",
    ] {
        history_optional_text(row, column)?;
    }
    for column in [
        "run_id",
        "definition_id",
        "workspace_snapshot",
        "authorization_profile_json",
        "authorization_ceiling",
        "status",
    ] {
        history_required_text(row, column)?;
    }
    parse_run_summary(run_summary_from_row(row)?)
}

fn history_attempt_query() -> SelectQuery<'static> {
    let mut query = history_columns(
        "workflow_attempts",
        &[
            "attempt",
            "prepared_at_ms",
            "admitted_at_ms",
            "terminal_at_ms",
        ],
    )
    .project_as(
        Box::new(where_not_eq("receipt_json", V::Null))
            as Box<dyn switchy_database::query::Expression>,
        "has_receipt",
    );
    for column in [
        "run_id",
        "node_id",
        "activation_id",
        "dispatch_identity",
        "side_effect",
        "status",
    ] {
        query = history_text(query, "workflow_attempts", column, column, MAX_ID_BYTES);
    }
    query
}

fn history_attempt(row: &Row) -> Result<AttemptSummary, WorkflowStoreError> {
    for column in [
        "run_id",
        "node_id",
        "activation_id",
        "dispatch_identity",
        "side_effect",
        "status",
    ] {
        history_required_text(row, column)?;
    }
    parse_attempt_summary(attempt_summary_from_row(row)?)
}

fn history_event_query() -> SelectQuery<'static> {
    let mut query = history_columns("workflow_events", &["event_seq", "created_at_ms"]);
    for (column, limit) in [
        ("run_id", MAX_ID_BYTES),
        ("event_type", MAX_ID_BYTES),
        ("payload_json", MAX_INLINE_JSON_BYTES),
    ] {
        query = history_text(query, "workflow_events", column, column, limit);
    }
    query
}

fn history_event(row: &Row) -> Result<WorkflowEventRow, WorkflowStoreError> {
    Ok(WorkflowEventRow {
        event_seq: database::unsigned(row, "event_seq")?,
        run_id: history_required_text(row, "run_id")?,
        event_type: history_required_text(row, "event_type")?,
        payload: serde_json::from_str(&history_required_text(row, "payload_json")?)?,
        created_at_ms: database::unsigned(row, "created_at_ms")?,
    })
}

fn history_count_row(db: &dyn Database, query: SelectQuery<'_>) -> Result<Row, WorkflowStoreError> {
    database::rows(db, &query.project_as(count_all(), "count"))?
        .into_iter()
        .next()
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow history aggregate is missing".into())
        })
}

impl WorkflowStore {
    /// Return one run summary without replaying workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded row query fails or contains invalid status data.
    pub fn run_summary(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
        Self::run_summary_in(&self.connection, run_id)
    }

    /// Read and validate the canonical successful terminal output for one run.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, inconsistent terminal state, missing output,
    /// malformed persisted JSON, checksum mismatch, or database failure.
    pub fn canonical_terminal_output(
        &self,
        run_id: &str,
    ) -> Result<Option<ValidatedOutput>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let db: &dyn Database = transaction.as_ref();
        let mut query = select("workflow_runs").where_eq("run_id", run_id).limit(1);
        for column in [
            "status",
            "terminal_output_id",
            "terminal_output_checksum_sha256",
        ] {
            query = history_text(query, "workflow_runs", column, column, MAX_ID_BYTES);
        }
        let terminal = database::rows(db, &query)?;
        let Some(row) = terminal.first() else {
            database::commit(transaction)?;
            return Ok(None);
        };
        let status = history_required_text(row, "status")?;
        let output_id = history_optional_text(row, "terminal_output_id")?;
        let checksum = history_optional_text(row, "terminal_output_checksum_sha256")?;
        if status != RunStatus::Completed.as_str() {
            if output_id.is_some() || checksum.is_some() {
                return Err(WorkflowStoreError::InvalidData(
                    "non-completed workflow run exposes terminal output identity".to_string(),
                ));
            }
            return Ok(None);
        }
        let output_id = output_id.ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "completed workflow run has no canonical terminal output".to_string(),
            )
        })?;
        let checksum = checksum.ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "completed workflow run has no terminal output checksum".to_string(),
            )
        })?;
        let mut query = history_columns("workflow_outputs", &["schema_version", "created_at_ms"])
            .where_eq("output_id", output_id.as_str())
            .where_eq("run_id", run_id)
            .limit(1);
        for (column, limit) in [
            ("output_id", MAX_ID_BYTES),
            ("run_id", MAX_ID_BYTES),
            ("node_id", MAX_ID_BYTES),
            ("activation_id", MAX_ID_BYTES),
            ("schema_id", MAX_ID_BYTES),
            ("value_json", MAX_INLINE_JSON_BYTES),
            ("artifact_reference", MAX_INLINE_JSON_BYTES),
        ] {
            query = history_text(query, "workflow_outputs", column, column, limit);
        }
        let rows = database::rows(db, &query)?;
        let row = rows.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData("canonical terminal output row is missing".into())
        })?;
        let output = (
            history_required_text(row, "output_id")?,
            history_required_text(row, "run_id")?,
            history_required_text(row, "node_id")?,
            history_required_text(row, "activation_id")?,
            history_required_text(row, "schema_id")?,
            history_u32(row, "schema_version")?,
            history_required_text(row, "value_json")?,
            history_optional_text(row, "artifact_reference")?,
            database::unsigned(row, "created_at_ms")?,
        );
        database::commit(transaction)?;
        if sha256_hex(output.6.as_bytes()) != checksum {
            return Err(WorkflowStoreError::InvalidData(
                "canonical terminal output checksum is inconsistent".to_string(),
            ));
        }
        Ok(Some(ValidatedOutput {
            output_id: output.0,
            run_id: output.1,
            node_id: output.2,
            activation_id: output.3,
            schema_id: output.4,
            schema_version: output.5,
            value: serde_json::from_str(&output.6)?,
            artifact_reference: output.7,
            created_at_ms: output.8,
        }))
    }

    /// Return the latest canonical workflow event sequence across all runs.
    ///
    /// # Errors
    ///
    /// Returns an error when the database query fails.
    pub fn latest_global_event_sequence(&self) -> Result<u64, WorkflowStoreError> {
        let rows = database::rows(
            &self.connection,
            &select("workflow_events").project_as(
                Box::new(coalesce(vec![
                    Box::new(max(identifier("event_seq"))),
                    Box::new(V::Int64(0)),
                ])) as Box<dyn switchy_database::query::Expression>,
                "event_seq",
            ),
        )?;
        database::unsigned(
            rows.first().ok_or_else(|| {
                WorkflowStoreError::InvalidData("workflow event aggregate is missing".into())
            })?,
            "event_seq",
        )
    }

    /// Return a bounded ordered page of canonical workflow event positions across all runs.
    ///
    /// This post-commit query is intended for live notification publication. It does not load event
    /// payloads or mutate canonical/derived state.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit is invalid or the database query fails.
    pub fn event_positions_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<(u64, String, u64)>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let query = history_text(
            history_columns("workflow_events", &["event_seq", "created_at_ms"]),
            "workflow_events",
            "run_id",
            "run_id",
            MAX_ID_BYTES,
        )
        .where_gt("event_seq", database::integer_value(after_sequence)?)
        .sort("event_seq", SortDirection::Asc)
        .limit(history_limit(limit)?);
        database::rows(&self.connection, &query)?
            .iter()
            .map(|row| {
                Ok((
                    database::unsigned(row, "event_seq")?,
                    history_required_text(row, "run_id")?,
                    database::unsigned(row, "created_at_ms")?,
                ))
            })
            .collect()
    }

    /// Return the latest canonical event sequence and timestamp for one workflow run.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity or database failure.
    pub fn latest_event_position(
        &self,
        run_id: &str,
    ) -> Result<Option<(u64, u64)>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        database::rows(
            &self.connection,
            &history_columns("workflow_events", &["event_seq", "created_at_ms"])
                .where_eq("run_id", run_id)
                .sort("event_seq", SortDirection::Desc)
                .limit(1),
        )?
        .first()
        .map(|row| {
            Ok((
                database::unsigned(row, "event_seq")?,
                database::unsigned(row, "created_at_ms")?,
            ))
        })
        .transpose()
    }

    /// Return bounded activations for one run without event replay.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/limit or query failure.
    pub fn activations_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowActivationSummary>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let mut query = history_columns(
            "workflow_activations",
            &["dependency_generation", "created_at_ms"],
        )
        .project_as(
            Box::new(where_not_eq("output_id", V::Null))
                as Box<dyn switchy_database::query::Expression>,
            "has_output",
        )
        .project_as(byte_length(identifier("input_json")), "input_bytes")
        .project_as(storage_type(identifier("input_json")), "input_type")
        .where_eq("run_id", run_id)
        .sort("dependency_generation", SortDirection::Desc)
        .sort("created_at_ms", SortDirection::Desc)
        .sort("node_id", SortDirection::Asc)
        .limit(history_limit(limit)?);
        for column in ["node_id", "activation_id", "status"] {
            query = history_text(query, "workflow_activations", column, column, MAX_ID_BYTES);
        }
        query = history_text(
            query,
            "workflow_activations",
            "input_json",
            "input_json",
            WORKFLOW_ACTIVATION_INPUT_SUMMARY_MAX_BYTES,
        );
        database::rows(&self.connection, &query)?
            .iter()
            .map(|row| {
                let input_summary = match database::text(row, "input_type")?.as_str() {
                    "null" => WorkflowActivationInputSummary::Absent,
                    "text" => {
                        let bytes = database::unsigned(row, "input_bytes")?;
                        if bytes > WORKFLOW_ACTIVATION_INPUT_SUMMARY_MAX_BYTES as u64 {
                            WorkflowActivationInputSummary::Omitted {
                                byte_count: usize::try_from(bytes).map_err(|_| {
                                    WorkflowStoreError::InvalidData(
                                        "activation input size exceeds usize".into(),
                                    )
                                })?,
                            }
                        } else {
                            WorkflowActivationInputSummary::Inline {
                                value: serde_json::from_str(&history_required_text(
                                    row,
                                    "input_json",
                                )?)?,
                            }
                        }
                    }
                    _ => {
                        return Err(WorkflowStoreError::InvalidData(
                            "invalid workflow activation input storage class".into(),
                        ));
                    }
                };
                Ok(WorkflowActivationSummary {
                    run_id: run_id.into(),
                    node_id: history_required_text(row, "node_id")?,
                    activation_id: history_required_text(row, "activation_id")?,
                    dependency_generation: database::unsigned(row, "dependency_generation")?,
                    status: history_required_text(row, "status")?,
                    has_output: history_flag(row, "has_output")?,
                    input_summary,
                    created_at_ms: database::unsigned(row, "created_at_ms")?,
                })
            })
            .collect()
    }

    /// Return bounded pending activations with exact compiled node definitions.
    ///
    /// This normal scheduler read uses normalized rows and one exact stored definition per run. It
    /// performs no event replay, repair, or external dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, persisted definitions are malformed, a node is
    /// missing, or the bounded query fails.
    pub fn pending_activations(
        &self,
        limit: usize,
    ) -> Result<Vec<PendingActivation>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        self.pending_activations_matching(None, limit)
    }

    /// Return bounded pending activations for one exact run.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity or bound is invalid, persisted definitions are
    /// malformed, a node is missing, or the bounded query fails.
    pub fn pending_activations_for_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<PendingActivation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        self.pending_activations_matching(Some(run_id), limit)
    }

    pub(super) fn pending_activations_matching(
        &self,
        run_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PendingActivation>, WorkflowStoreError> {
        bounded_limit(history_limit(limit)?)?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let mut activations =
            Self::pending_standard_activations_matching_in(transaction.as_ref(), run_id, limit)?;
        let remaining = limit.saturating_sub(i64::try_from(activations.len()).unwrap_or(limit));
        if remaining > 0 {
            activations.extend(Self::pending_fan_out_members_matching_in(
                transaction.as_ref(),
                run_id,
                remaining,
            )?);
        }
        activations.sort_by(|left, right| {
            (&left.created_at_ms, &left.run_id, &left.node_id).cmp(&(
                &right.created_at_ms,
                &right.run_id,
                &right.node_id,
            ))
        });
        activations.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        database::commit(transaction)?;
        Ok(activations)
    }

    fn pending_standard_activations_matching_in(
        db: &dyn Database,
        run_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PendingActivation>, WorkflowStoreError> {
        let activations = "workflow_activations";
        let runs = "workflow_runs";
        let members = "workflow_fan_out_members";
        let mut query = history_columns(activations, &["dependency_generation", "created_at_ms"])
            .joins(vec![join_columns(
                runs,
                qualified_column(runs, "run_id"),
                qualified_column(activations, "run_id"),
                false,
            )])
            .filter(Box::new(where_eq_expression(
                qualified_column(activations, "status"),
                V::String("pending".into()),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column(runs, "status"),
                V::String("running".into()),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column(runs, "cancellation_requested_at_ms"),
                V::Null,
            )))
            .where_not_in(
                "activation_id",
                history_columns(members, &["member_activation_id"]).filter(Box::new(
                    where_eq_expression(
                        qualified_column(members, "run_id"),
                        qualified_column(activations, "run_id"),
                    ),
                )),
            )
            .sorts(vec![switchy_database::query::sort(
                qualified_column(activations, "created_at_ms"),
                SortDirection::Asc,
            )])
            .sorts(vec![switchy_database::query::sort(
                qualified_column(activations, "run_id"),
                SortDirection::Asc,
            )])
            .sorts(vec![switchy_database::query::sort(
                qualified_column(activations, "node_id"),
                SortDirection::Asc,
            )])
            .limit(history_limit(limit)?);
        if let Some(run_id) = run_id.filter(|run_id| !run_id.is_empty()) {
            query = query.filter(Box::new(where_eq_expression(
                qualified_column(activations, "run_id"),
                V::String(run_id.into()),
            )));
        }
        for (column, limit) in [
            ("run_id", MAX_ID_BYTES),
            ("node_id", MAX_ID_BYTES),
            ("activation_id", MAX_ID_BYTES),
            ("input_json", MAX_INLINE_JSON_BYTES),
        ] {
            query = history_text(query, activations, column, column, limit);
        }
        database::rows(db, &query)?
            .iter()
            .map(|row| {
                let run_id = history_required_text(row, "run_id")?;
                let node_id = history_required_text(row, "node_id")?;
                let activation_id = history_required_text(row, "activation_id")?;
                let dependency_generation = database::unsigned(row, "dependency_generation")?;
                let input_json = history_optional_text(row, "input_json")?;
                let created_at_ms = database::unsigned(row, "created_at_ms")?;
                if run_graph::graph_revision(db, &run_id)? == Some(1) {
                    Self::run_graph_node_in(db, &run_id, &node_id)?.ok_or_else(|| {
                        WorkflowStoreError::InvalidData(format!(
                            "workflow activation references missing run-graph node: {node_id}"
                        ))
                    })?;
                }
                run_graph::reconciled_activation_exit(db, &run_id, &node_id, &activation_id)?;
                let node = Self::activation_graph_node_in(db, &run_id, &node_id, &activation_id)?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "pending activation executable binding is missing".to_string(),
                        )
                    })?
                    .node;
                Ok(PendingActivation {
                    run_id,
                    node_id,
                    activation_id,
                    dependency_generation,
                    input: input_json
                        .map(|json| serde_json::from_str(&json))
                        .transpose()?,
                    node,
                    created_at_ms,
                })
            })
            .collect()
    }

    fn pending_fan_out_members_matching_in(
        db: &dyn Database,
        run_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PendingActivation>, WorkflowStoreError> {
        let members = "workflow_fan_out_members";
        let runs = "workflow_runs";
        let mut query = history_columns(members, &["member_index", "created_at_ms"])
            .joins(vec![join_columns(
                runs,
                qualified_column(runs, "run_id"),
                qualified_column(members, "run_id"),
                false,
            )])
            .filter(Box::new(where_eq_expression(
                qualified_column(members, "status"),
                V::String("pending".into()),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column(runs, "status"),
                V::String("running".into()),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column(runs, "cancellation_requested_at_ms"),
                V::Null,
            )))
            .sorts(vec![switchy_database::query::sort(
                qualified_column(members, "created_at_ms"),
                SortDirection::Asc,
            )])
            .sorts(vec![switchy_database::query::sort(
                qualified_column(members, "run_id"),
                SortDirection::Asc,
            )])
            .sorts(vec![switchy_database::query::sort(
                qualified_column(members, "controller_activation_id"),
                SortDirection::Asc,
            )])
            .sorts(vec![switchy_database::query::sort(
                qualified_column(members, "member_index"),
                SortDirection::Asc,
            )])
            .limit(history_limit(limit)?);
        if let Some(run_id) = run_id.filter(|run_id| !run_id.is_empty()) {
            query = query.filter(Box::new(where_eq_expression(
                qualified_column(members, "run_id"),
                V::String(run_id.into()),
            )));
        }
        for (column, limit) in [
            ("run_id", MAX_ID_BYTES),
            ("controller_node_id", MAX_ID_BYTES),
            ("member_node_id", MAX_ID_BYTES),
            ("member_activation_id", MAX_ID_BYTES),
            ("input_json", MAX_INLINE_JSON_BYTES),
            ("controller_activation_id", MAX_ID_BYTES),
        ] {
            query = history_text(query, members, column, column, limit);
        }
        database::rows(db, &query)?
            .iter()
            .map(|row| {
                let run_id = history_required_text(row, "run_id")?;
                let controller_node_id = history_required_text(row, "controller_node_id")?;
                let node_id = history_required_text(row, "member_node_id")?;
                let activation_id = history_required_text(row, "member_activation_id")?;
                let index = history_u32(row, "member_index")?;
                let input_json = history_required_text(row, "input_json")?;
                let created_at_ms = database::unsigned(row, "created_at_ms")?;
                let controller_activation_id =
                    history_required_text(row, "controller_activation_id")?;
                Self::run_graph_node_in(db, &run_id, &controller_node_id)?.ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "fan-out run-graph controller node is missing".to_string(),
                    )
                })?;
                let controller = Self::activation_graph_node_in(
                    db,
                    &run_id,
                    &controller_node_id,
                    &controller_activation_id,
                )?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "fan-out controller executable binding is missing".to_string(),
                    )
                })?
                .node;
                let configuration: bcode_workflow::WorkflowFanOutConfiguration =
                    serde_json::from_value(controller.configuration)?;
                let mut node = *configuration.member_node;
                node.id.clone_from(&node_id);
                Ok(PendingActivation {
                    run_id,
                    node_id,
                    activation_id,
                    dependency_generation: u64::from(index),
                    input: Some(serde_json::from_str(&input_json)?),
                    node,
                    created_at_ms,
                })
            })
            .collect()
    }

    /// Return a bounded newest-first run list without replaying workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid or the bounded query fails.
    pub fn list_runs(&self, limit: usize) -> Result<Vec<WorkflowRunSummary>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let query = history_runs_query()
            .sort("updated_at_ms", SortDirection::Desc)
            .sort("run_id", SortDirection::Asc)
            .limit(history_limit(limit)?);
        database::rows(&self.connection, &query)?
            .iter()
            .map(history_run)
            .collect()
    }

    /// Return bounded catalog counts without replaying workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity or inconsistent derived rows.
    pub fn run_catalog_summary(
        &self,
        run_id: &str,
    ) -> Result<WorkflowRunCatalogSummary, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let summary = Self::run_catalog_summary_in(transaction.as_ref(), run_id)?;
        database::commit(transaction)?;
        Ok(summary)
    }

    fn run_catalog_summary_in(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<WorkflowRunCatalogSummary, WorkflowStoreError> {
        let definition = Self::catalog_definition_in(db, run_id)?;
        let mut summary = WorkflowRunCatalogSummary {
            total_nodes: u32::try_from(definition.nodes.len()).unwrap_or(u32::MAX),
            not_started: u32::try_from(definition.nodes.len()).unwrap_or(u32::MAX),
            ..WorkflowRunCatalogSummary::default()
        };
        let activations = "workflow_activations";
        let latest = select(activations)
            .table_alias("latest_activation")
            .project(qualified_column("latest_activation", "activation_id"))
            .filter(Box::new(where_eq_expression(
                qualified_column("latest_activation", "run_id"),
                qualified_column(activations, "run_id"),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column("latest_activation", "node_id"),
                qualified_column(activations, "node_id"),
            )))
            .sorts(vec![switchy_database::query::sort(
                qualified_column("latest_activation", "dependency_generation"),
                SortDirection::Desc,
            )])
            .sorts(vec![switchy_database::query::sort(
                qualified_column("latest_activation", "created_at_ms"),
                SortDirection::Desc,
            )])
            .sorts(vec![switchy_database::query::sort(
                qualified_column("latest_activation", "activation_id"),
                SortDirection::Desc,
            )])
            .limit(1);
        let query = history_text(
            history_text(
                select(activations),
                activations,
                "node_id",
                "node_id",
                MAX_ID_BYTES,
            ),
            activations,
            "status",
            "status",
            MAX_ID_BYTES,
        )
        .where_eq("run_id", run_id)
        .where_in("activation_id", latest)
        .sort("node_id", SortDirection::Asc);
        for row in database::rows(db, &query)? {
            history_required_text(&row, "node_id")?;
            let status = history_required_text(&row, "status")?;
            summary.not_started = summary.not_started.saturating_sub(1);
            match status.as_str() {
                "pending" | "running" => summary.active = summary.active.saturating_add(1),
                "waiting_input" => {
                    summary.blocked = summary.blocked.saturating_add(1);
                    summary.pending_inputs = summary.pending_inputs.saturating_add(1);
                }
                "waiting_approval" => {
                    summary.blocked = summary.blocked.saturating_add(1);
                    summary.pending_approvals = summary.pending_approvals.saturating_add(1);
                }
                "waiting_mutation_approval" => {
                    summary.blocked = summary.blocked.saturating_add(1);
                }
                "completed" => summary.completed = summary.completed.saturating_add(1),
                "failed" => summary.failed = summary.failed.saturating_add(1),
                "cancelled" => summary.cancelled = summary.cancelled.saturating_add(1),
                "skipped" => summary.skipped = summary.skipped.saturating_add(1),
                "repair_required" => {
                    summary.repair_required = summary.repair_required.saturating_add(1);
                }
                _ => {
                    return Err(WorkflowStoreError::InvalidData(format!(
                        "invalid workflow activation status: {status}"
                    )));
                }
            }
        }
        summary.pending_mutation_approvals = history_u32(
            &history_count_row(
                db,
                select("workflow_mutation_approvals")
                    .where_eq("run_id", run_id)
                    .where_eq("status", "pending"),
            )?,
            "count",
        )?;
        summary.retryable_failures = Self::catalog_retryable_failure_count_in(db, run_id)?;
        summary.descendant_count = history_u32(
            &history_count_row(
                db,
                select("workflow_run_links").where_eq("root_run_id", run_id),
            )?,
            "count",
        )?;
        Ok(summary)
    }

    fn catalog_definition_in(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<WorkflowDefinition, WorkflowStoreError> {
        let runs = "workflow_runs";
        let definitions = "workflow_definitions";
        let query = history_text(
            select(runs),
            definitions,
            "definition_json",
            "definition_json",
            bcode_workflow::MAX_REGISTERED_WORKFLOW_DEFINITION_BYTES,
        )
        .joins(vec![join_columns(
            definitions,
            qualified_column(definitions, "definition_id"),
            qualified_column(runs, "definition_id"),
            false,
        )])
        .filter(Box::new(where_eq_expression(
            qualified_column(definitions, "version"),
            qualified_column(runs, "definition_version"),
        )))
        .where_eq("run_id", run_id)
        .limit(1);
        let rows = database::rows(db, &query)?;
        let row = rows.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow catalog definition is missing".into())
        })?;
        let definition: WorkflowDefinition =
            serde_json::from_str(&history_required_text(row, "definition_json")?)?;
        Ok(definition)
    }

    fn catalog_retryable_failure_count_in(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<u32, WorkflowStoreError> {
        let activations = "workflow_activations";
        let attempts = "workflow_attempts";
        let latest = select(attempts)
            .table_alias("latest_attempt")
            .project(max(qualified_column("latest_attempt", "attempt")))
            .filter(Box::new(where_eq_expression(
                qualified_column("latest_attempt", "run_id"),
                qualified_column(attempts, "run_id"),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column("latest_attempt", "node_id"),
                qualified_column(attempts, "node_id"),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column("latest_attempt", "activation_id"),
                qualified_column(attempts, "activation_id"),
            )));
        let query = select(attempts)
            .joins(vec![join_columns(
                activations,
                qualified_column(activations, "run_id"),
                qualified_column(attempts, "run_id"),
                false,
            )])
            .filter(Box::new(where_eq_expression(
                qualified_column(activations, "node_id"),
                qualified_column(attempts, "node_id"),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column(activations, "activation_id"),
                qualified_column(attempts, "activation_id"),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column(attempts, "run_id"),
                V::String(run_id.into()),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column(attempts, "status"),
                V::String("failed".into()),
            )))
            .filter(Box::new(where_eq_expression(
                qualified_column(activations, "status"),
                V::String("failed".into()),
            )))
            .where_in("attempt", latest);
        history_u32(&history_count_row(db, query)?, "count")
    }

    /// Return one bounded deterministic workflow catalog page.
    ///
    /// Filtering is performed over bounded rows and approved display metadata. The query never
    /// replays canonical workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds, cursors, searches, or inconsistent derived state.
    #[allow(clippy::too_many_lines)]
    pub fn workflow_run_catalog_page(
        &self,
        request: &WorkflowRunCatalogQuery,
    ) -> Result<WorkflowRunCatalogPage, WorkflowStoreError> {
        self.workflow_run_catalog_page_with_attention(request, &BTreeMap::new())
    }

    /// Query a catalog page with application-owned live tool-attention counts.
    ///
    /// The overlay affects filtering before pagination; it grants no execution authority
    /// and is not persisted as canonical workflow state.
    ///
    /// # Errors
    /// Returns the same validation and storage errors as [`Self::workflow_run_catalog_page`].
    #[allow(clippy::too_many_lines)]
    pub fn workflow_run_catalog_page_with_attention(
        &self,
        request: &WorkflowRunCatalogQuery,
        tool_attention: &BTreeMap<String, u32>,
    ) -> Result<WorkflowRunCatalogPage, WorkflowStoreError> {
        let query_limit = authoring_page_query_limit(request.limit)?;
        if request
            .search
            .as_ref()
            .is_some_and(|search| search.is_empty() || search.len() > MAX_DISPLAY_LABEL_BYTES)
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow catalog search must contain 1..=512 bytes".to_string(),
            ));
        }
        if let Some(cursor) = &request.cursor {
            validate_id("workflow catalog cursor run_id", &cursor.run_id)?;
            if cursor.sort != request.sort {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow catalog cursor sort does not match request".to_string(),
                ));
            }
        }
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let db: &dyn Database = transaction.as_ref();
        let query = history_runs_query()
            .sort("updated_at_ms", SortDirection::Desc)
            .sort("run_id", SortDirection::Asc)
            .limit(1000);
        let mut runs = database::rows(db, &query)?
            .iter()
            .map(history_run)
            .collect::<Result<Vec<_>, _>>()?;
        let summaries = runs
            .iter()
            .map(|run| {
                Ok((
                    run.run_id.clone(),
                    Self::run_catalog_summary_in(db, &run.run_id)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, WorkflowStoreError>>()?;
        let search = request.search.as_ref().map(|value| value.to_lowercase());
        runs.retain(|run| {
            let summary = summaries.get(&run.run_id);
            let filter_matches = match request.filter {
                WorkflowRunCatalogFilter::Active => {
                    matches!(run.status, RunStatus::Running | RunStatus::Paused)
                }
                WorkflowRunCatalogFilter::NeedsAttention => {
                    summary.is_some_and(|summary| {
                        summary.pending_inputs > 0
                            || summary.pending_approvals > 0
                            || summary.pending_mutation_approvals > 0
                            || summary.retryable_failures > 0
                            || summary.repair_required > 0
                    }) || run.status == RunStatus::RepairRequired
                        || tool_attention
                            .get(&run.run_id)
                            .is_some_and(|count| *count > 0)
                }
                WorkflowRunCatalogFilter::Failed => {
                    matches!(run.status, RunStatus::Failed | RunStatus::RepairRequired)
                }
                WorkflowRunCatalogFilter::Completed => run.status == RunStatus::Completed,
                WorkflowRunCatalogFilter::All => true,
            };
            let search_matches = search.as_ref().is_none_or(|search| {
                run.run_id.to_lowercase().contains(search)
                    || run.definition_id.to_lowercase().contains(search)
                    || run
                        .binding
                        .as_ref()
                        .and_then(|binding| binding.display_label.as_ref())
                        .is_some_and(|label| label.to_lowercase().contains(search))
                    || run
                        .authored_provenance
                        .as_ref()
                        .is_some_and(|source| source.workflow_id.to_lowercase().contains(search))
            });
            filter_matches && search_matches
        });
        runs.sort_by(|left, right| match request.sort {
            WorkflowRunCatalogSort::UpdatedAt => right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.run_id.cmp(&right.run_id)),
            WorkflowRunCatalogSort::CreatedAt => right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| left.run_id.cmp(&right.run_id)),
            WorkflowRunCatalogSort::Status => run_status_rank(left.status)
                .cmp(&run_status_rank(right.status))
                .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
                .then_with(|| left.run_id.cmp(&right.run_id)),
        });
        if let Some(cursor) = &request.cursor {
            runs.retain(|run| run_follows_catalog_cursor(run, cursor));
        }
        let requested = usize::try_from(query_limit).unwrap_or(1_001);
        runs.truncate(requested);
        let has_more = runs.len() > request.limit;
        runs.truncate(request.limit);
        let entries = runs
            .into_iter()
            .map(|run| {
                Ok(WorkflowRunCatalogEntry {
                    summary: summaries.get(&run.run_id).cloned().ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "workflow catalog summary is missing".into(),
                        )
                    })?,
                    run,
                })
            })
            .collect::<Result<Vec<_>, WorkflowStoreError>>()?;
        database::commit(transaction)?;
        Ok(WorkflowRunCatalogPage { entries, has_more })
    }

    /// Return bounded nonterminal runs (`running`, `paused`, `repair_required`) oldest-first.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit is invalid, a row is malformed, or the bounded query fails.
    pub fn nonterminal_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<WorkflowRunSummary>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let query = history_runs_query()
            .where_in(
                "status",
                vec![
                    V::String("running".into()),
                    V::String("paused".into()),
                    V::String("repair_required".into()),
                ],
            )
            .sort("updated_at_ms", SortDirection::Asc)
            .sort("run_id", SortDirection::Asc)
            .limit(history_limit(limit)?);
        database::rows(&self.connection, &query)?
            .iter()
            .map(history_run)
            .collect()
    }

    /// Return bounded quiescent runs that belong to persisted parent sessions.
    ///
    /// Quiescent runs hold no live process resources: every terminal status plus `paused`.
    /// Callers use this to reconcile run-level runtime work registrations after a restart.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit is invalid, a row is malformed, or the bounded query fails.
    pub fn quiescent_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<WorkflowRunSummary>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let query = history_runs_query()
            .where_in(
                "status",
                vec![
                    V::String("completed".into()),
                    V::String("failed".into()),
                    V::String("cancelled".into()),
                    V::String("repair_required".into()),
                    V::String("paused".into()),
                ],
            )
            .where_not_eq("parent_session_id", V::Null)
            .sort("updated_at_ms", SortDirection::Desc)
            .sort("run_id", SortDirection::Asc)
            .limit(history_limit(limit)?);
        database::rows(&self.connection, &query)?
            .iter()
            .map(history_run)
            .collect()
    }

    /// Return bounded terminal runs that belong to persisted parent sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit is invalid, a row is malformed, or the bounded query fails.
    pub fn terminal_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<WorkflowRunSummary>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let query = history_runs_query()
            .where_in(
                "status",
                vec![
                    V::String("completed".into()),
                    V::String("failed".into()),
                    V::String("cancelled".into()),
                    V::String("repair_required".into()),
                ],
            )
            .where_not_eq("parent_session_id", V::Null)
            .sort("updated_at_ms", SortDirection::Desc)
            .sort("run_id", SortDirection::Asc)
            .limit(history_limit(limit)?);
        database::rows(&self.connection, &query)?
            .iter()
            .map(history_run)
            .collect()
    }

    /// Return bounded run IDs whose pending activations or due retries are safe to continue after
    /// startup.
    ///
    /// Eligible runs are either running with pending work or failed with a due durable retry, and
    /// have no cancellation intent, active attempts, or unresolved input/approval waits. The query
    /// is projection-backed and does not replay workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit is invalid or the bounded query fails.
    pub fn resumable_pending_run_ids(
        &self,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let pending =
            history_columns("workflow_activations", &["run_id"]).where_eq("status", "pending");
        let retries = history_columns("workflow_retry_schedules", &["run_id"]);
        let active = history_columns("workflow_attempts", &["run_id"]).where_in(
            "status",
            vec![
                V::String("prepared".into()),
                V::String("admitted".into()),
                V::String("running".into()),
                V::String("cancelling".into()),
                V::String("sibling_cancelling".into()),
            ],
        );
        let waiting = history_columns("workflow_activations", &["run_id"]).where_in(
            "status",
            vec![
                V::String("waiting_input".into()),
                V::String("waiting_approval".into()),
            ],
        );
        let query = history_text(
            select("workflow_runs"),
            "workflow_runs",
            "run_id",
            "run_id",
            MAX_ID_BYTES,
        )
        .distinct()
        .where_eq("cancellation_requested_at_ms", V::Null)
        .filter(Box::new(where_or(vec![
            Box::new(where_and(vec![
                Box::new(where_eq("status", "running")),
                Box::new(switchy_database::query::where_in("run_id", pending)),
            ])),
            Box::new(where_and(vec![
                Box::new(where_eq("status", "failed")),
                Box::new(switchy_database::query::where_in("run_id", retries)),
            ])),
        ])))
        .where_not_in("run_id", active)
        .where_not_in("run_id", waiting)
        .sort("run_id", SortDirection::Asc)
        .limit(history_limit(limit)?);
        database::rows(&self.connection, &query)?
            .iter()
            .map(|row| history_required_text(row, "run_id"))
            .collect()
    }

    /// Return bounded durable input/approval waits ordered deterministically.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid limit, malformed persisted JSON, or database failure.
    pub fn waiting_activations(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WaitingActivation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let mut query = history_columns("workflow_activations", &["created_at_ms"])
            .where_eq("run_id", run_id)
            .where_in(
                "status",
                vec![
                    V::String("waiting_input".into()),
                    V::String("waiting_approval".into()),
                ],
            )
            .sort("created_at_ms", SortDirection::Asc)
            .sort("node_id", SortDirection::Asc)
            .sort("activation_id", SortDirection::Asc)
            .limit(history_limit(limit)?);
        for (column, limit) in [
            ("run_id", MAX_ID_BYTES),
            ("node_id", MAX_ID_BYTES),
            ("activation_id", MAX_ID_BYTES),
            ("status", MAX_ID_BYTES),
            ("input_json", MAX_INLINE_JSON_BYTES),
        ] {
            query = history_text(query, "workflow_activations", column, column, limit);
        }
        database::rows(&self.connection, &query)?
            .iter()
            .map(|row| {
                let run_id = history_required_text(row, "run_id")?;
                let node_id = history_required_text(row, "node_id")?;
                let activation_id = history_required_text(row, "activation_id")?;
                let status = history_required_text(row, "status")?;
                let input_json = history_optional_text(row, "input_json")?;
                let requested_at_ms = database::unsigned(row, "created_at_ms")?;
                let kind = match status.as_str() {
                    "waiting_input" => WorkflowWaitKind::Input,
                    "waiting_approval" => WorkflowWaitKind::Approval,
                    _ => {
                        return Err(WorkflowStoreError::InvalidData(format!(
                            "invalid workflow wait status: {status}"
                        )));
                    }
                };
                Ok(WaitingActivation {
                    run_id,
                    node_id,
                    activation_id,
                    kind,
                    input: input_json
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?,
                    requested_at_ms,
                })
            })
            .collect()
    }

    /// Return a keyset-paged bounded attempt history for one run.
    ///
    /// Reads do not repair or mutate stored attempts. Invalid data in the selected
    /// page fails the entire request rather than returning a partial history.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    /// * The run ID, limit, or cursor is invalid.
    /// * A selected attempt has an invalid identity, zero attempt number, or an
    ///   unsupported status or side-effect value.
    /// * A selected row cannot be decoded or the database query fails.
    pub fn attempt_history(
        &self,
        run_id: &str,
        cursor: Option<&AttemptCursor>,
        limit: usize,
    ) -> Result<Vec<AttemptSummary>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        if let Some(cursor) = cursor {
            validate_id("cursor.dispatch_identity", &cursor.dispatch_identity)?;
            i64::try_from(cursor.prepared_at_ms).map_err(|_| {
                WorkflowStoreError::InvalidData(
                    "attempt history cursor timestamp exceeds storage range".to_string(),
                )
            })?;
        }
        let mut query = history_attempt_query()
            .where_eq("run_id", run_id)
            .sort("prepared_at_ms", SortDirection::Desc)
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(history_limit(limit)?);
        if let Some(cursor) = cursor {
            let timestamp = database::integer_value(cursor.prepared_at_ms)?;
            query = query.filter(Box::new(where_or(vec![
                Box::new(where_lt("prepared_at_ms", timestamp.clone())),
                Box::new(where_and(vec![
                    Box::new(where_eq("prepared_at_ms", timestamp)),
                    Box::new(where_gt(
                        "dispatch_identity",
                        cursor.dispatch_identity.clone(),
                    )),
                ])),
            ])));
        }
        database::rows(&self.connection, &query)?
            .iter()
            .map(history_attempt)
            .collect()
    }

    /// Read one exact admitted dispatch intent, verifying its bounded representation and checksum.
    ///
    /// # Errors
    /// Returns an error for missing, oversized, corrupt, or invalid intent data.
    pub fn dispatch_intent(&self, identity: &str) -> Result<serde_json::Value, WorkflowStoreError> {
        validate_id("dispatch_identity", identity)?;
        let mut query = select("workflow_attempts")
            .where_eq("dispatch_identity", identity)
            .limit(1);
        query = history_text(
            query,
            "workflow_attempts",
            "intent_json",
            "intent_json",
            MAX_INLINE_JSON_BYTES,
        );
        query = history_text(
            query,
            "workflow_attempts",
            "intent_checksum",
            "intent_checksum",
            MAX_ID_BYTES,
        );
        let rows = database::rows(&self.connection, &query)?;
        let row = rows
            .first()
            .ok_or_else(|| WorkflowStoreError::InvalidData("dispatch intent is missing".into()))?;
        let json = history_required_text(row, "intent_json")?;
        let checksum = history_required_text(row, "intent_checksum")?;
        if sha256_hex(json.as_bytes()) != checksum {
            return Err(WorkflowStoreError::InvalidData(
                "dispatch intent checksum mismatch".into(),
            ));
        }
        Ok(serde_json::from_str(&json)?)
    }

    /// Look up one attempt by its indexed dispatch identity and verify its coordinates.
    ///
    /// # Errors
    /// Returns invalid-data or database errors without repairing stored state.
    pub(super) fn attempt_by_dispatch_identity_in(
        db: &dyn switchy_database::Database,
        identity: &str,
    ) -> Result<Option<AttemptSummary>, WorkflowStoreError> {
        validate_id("dispatch_identity", identity)?;
        database::rows(
            db,
            &history_attempt_query()
                .where_eq("dispatch_identity", identity)
                .limit(1),
        )?
        .first()
        .map(history_attempt)
        .transpose()
    }

    /// Read one attempt by its exact indexed dispatch identity.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity, malformed persisted attempt data, or database failure.
    pub fn attempt_by_dispatch_identity(
        &self,
        identity: &str,
    ) -> Result<Option<AttemptSummary>, WorkflowStoreError> {
        Self::attempt_by_dispatch_identity_in(&self.connection, identity)
    }

    /// Return the newest bounded canonical failure events for one run in chronological order.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, JSON is malformed, or the query fails.
    pub fn failure_history(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowEventRow>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let query = history_event_query()
            .where_eq("run_id", run_id)
            .where_in(
                "event_type",
                vec![
                    V::String("attempt_failed".into()),
                    V::String("fan_out_member_failed".into()),
                    V::String("run_failed".into()),
                    V::String("parallel_join_failed".into()),
                    V::String("mutation_approval_denied".into()),
                    V::String("mutation_approval_expired".into()),
                    V::String("run_deadline_elapsed".into()),
                ],
            )
            .sort("event_seq", SortDirection::Desc)
            .limit(history_limit(limit)?);
        let mut events = database::rows(&self.connection, &query)?
            .iter()
            .map(history_event)
            .collect::<Result<Vec<_>, _>>()?;
        events.reverse();
        Ok(events)
    }

    /// Return keyset-paged workflow events after `after_sequence`.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, JSON is malformed, or the query fails.
    pub fn event_history(
        &self,
        run_id: &str,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<WorkflowEventRow>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let after = after_sequence.unwrap_or(0);
        let query = history_event_query()
            .where_eq("run_id", run_id)
            .where_gt("event_seq", database::integer_value(after)?)
            .sort("event_seq", SortDirection::Asc)
            .limit(history_limit(limit)?);
        database::rows(&self.connection, &query)?
            .iter()
            .map(history_event)
            .collect()
    }
}
