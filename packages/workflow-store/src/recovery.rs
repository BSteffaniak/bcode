//! Durable recovery-only dispatch barrier. Absence means ordinary execution.
use crate::database::block_on;
use crate::{WorkflowStoreError, database};
use switchy_database::query::{FilterableQuery, SortDirection, schema_predicate, select};
use switchy_database::schema::SchemaExpression as E;
use switchy_database::schema::{
    Column, DataType, SchemaComparison, SchemaExpression, create_index, create_table,
    create_trigger,
};
use switchy_database::{Database, DatabaseValue};

pub fn verify_barriers(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    verify_columns(
        connection,
        "workflow_recovery_barriers",
        &["run_id", "source_artifact_id", "created_at_ms"],
    )?;
    verify_triggers(
        connection,
        &[
            "recovery_blocks_attempt_insert",
            "recovery_blocks_resume",
            "recovery_blocks_handoff",
        ],
        "workflow recovery dispatch barrier is missing; maintenance required",
    )
}

fn verify_columns(
    connection: &dyn Database,
    table: &str,
    columns: &[&str],
) -> Result<(), WorkflowStoreError> {
    // Validate only the contract we need. Full table introspection also loads foreign
    // keys, indexes and autoincrement metadata on every dispatch transaction.
    let query = columns.iter().fold(select(table), |query, column| {
        query.project(switchy_database::query::qualified_column(table, column))
    });
    block_on(connection.validate_select(&query))?;
    Ok(())
}

fn verify_triggers(
    connection: &dyn Database,
    names: &[&str],
    message: &str,
) -> Result<(), WorkflowStoreError> {
    for name in names {
        if !block_on(connection.trigger_exists(name))? {
            return Err(WorkflowStoreError::InvalidData(message.into()));
        }
    }
    Ok(())
}

pub fn verify(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    verify_barriers(connection)?;
    verify_columns(
        connection,
        "workflow_quiescence",
        &[
            "run_id",
            "after_dispatch",
            "after_child",
            "attempts_complete",
            "complete",
        ],
    )?;
    verify_triggers(
        connection,
        &[
            "quiescence_blocks_attempt_insert",
            "quiescence_blocks_attempt_update",
            "quiescence_blocks_attempt_delete",
            "quiescence_blocks_link_insert",
            "quiescence_blocks_link_update",
            "quiescence_blocks_link_delete",
            "quiescence_blocks_reopen",
        ],
        "quiescence guard missing; maintenance required",
    )?;
    // Prepare the exact bounded recovery shapes without executing them. In particular,
    // SQLite must prove the partial receipt index is usable for this predicate.
    for query in [
        select("workflow_run_links")
            .columns(&["child_run_id"])
            .indexed_by("workflow_quiescence_children")
            .where_eq("parent_run_id", "")
            .where_gt("child_run_id", "")
            .sort("child_run_id", SortDirection::Asc)
            .limit(1),
        select("workflow_attempts")
            .columns(&["dispatch_identity"])
            .indexed_by("workflow_quiescence_attempts")
            .where_eq("run_id", "")
            .where_gt("dispatch_identity", "")
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(1),
        select("workflow_attempts")
            .columns(&["dispatch_identity"])
            .indexed_by("workflow_receipt_recovery_page")
            .where_eq("run_id", "")
            .where_gt("dispatch_identity", "")
            .filter(Box::new(schema_predicate(&receipt_predicate())?))
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(1),
    ] {
        block_on(connection.validate_select(&query))?;
    }
    verify_columns(
        connection,
        "workflow_run_links",
        &["parent_run_id", "child_run_id"],
    )?;
    verify_columns(
        connection,
        "workflow_attempts",
        &["run_id", "dispatch_identity", "status", "receipt_json"],
    )?;
    verify_columns(
        connection,
        "workflow_receipt_cursors",
        &["run_id", "after_dispatch_identity"],
    )?;
    verify_columns(
        connection,
        "workflow_replacement_intents",
        &[
            "old_run_id",
            "successor_run_id",
            "successor_json",
            "requested_at_ms",
        ],
    )?;
    verify_columns(
        connection,
        "workflow_discovery_cursors",
        &["artifact_id", "after_run_id"],
    )
}

pub fn require_execution(
    connection: &dyn Database,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    if !database::rows(
        connection,
        &select("workflow_recovery_barriers")
            .columns(&["run_id"])
            .where_eq("run_id", run_id)
            .limit(1),
    )?
    .is_empty()
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow recovery prohibits execution admission".into(),
        ));
    }
    Ok(())
}

fn column(name: &str, data_type: DataType, default: Option<DatabaseValue>) -> Column {
    Column {
        name: name.into(),
        nullable: false,
        auto_increment: false,
        data_type,
        default,
    }
}

fn equal(left: SchemaExpression, right: SchemaExpression) -> SchemaExpression {
    SchemaExpression::Compare(Box::new(left), SchemaComparison::Equal, Box::new(right))
}

fn exists(table: &str, column: &str, row: SchemaExpression) -> SchemaExpression {
    SchemaExpression::Exists {
        table: table.into(),
        predicate: Box::new(equal(SchemaExpression::column(column), row)),
    }
}

fn initialize_quiescence(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(
        create_index("workflow_receipt_recovery_page")
            .if_not_exists(true)
            .table("workflow_attempts")
            .columns(vec!["run_id", "dispatch_identity"])
            .predicate(receipt_predicate())
            .execute(connection),
    )?;
    block_on(
        create_table("workflow_quiescence")
            .if_not_exists(true)
            .columns(vec![
                column("run_id", DataType::Text, None),
                column("after_dispatch", DataType::Text, Some("".into())),
                column("after_child", DataType::Text, Some("".into())),
                column("attempts_complete", DataType::BigInt, Some(0.into())),
                column("complete", DataType::BigInt, Some(0.into())),
            ])
            .primary_key("run_id")
            .foreign_key_column("run_id", "workflow_runs", "run_id")
            .execute(connection),
    )?;
    for (name, table, row_column, message) in [
        (
            "quiescence_blocks_attempt",
            "workflow_attempts",
            "run_id",
            "quiescence verification freezes attempts",
        ),
        (
            "quiescence_blocks_link",
            "workflow_run_links",
            "parent_run_id",
            "quiescence verification freezes children",
        ),
    ] {
        let new = exists("workflow_quiescence", "run_id", E::new_column(row_column));
        let old = exists("workflow_quiescence", "run_id", E::old_column(row_column));
        block_on(
            create_trigger(format!("{name}_insert"), table)
                .if_not_exists(true)
                .before_insert()
                .when(new.clone())
                .raise_abort(message)
                .execute(connection),
        )?;
        block_on(
            create_trigger(format!("{name}_update"), table)
                .if_not_exists(true)
                .before_update()
                .when(E::Or(vec![old.clone(), new]))
                .raise_abort(message)
                .execute(connection),
        )?;
        block_on(
            create_trigger(format!("{name}_delete"), table)
                .if_not_exists(true)
                .before_delete()
                .when(old)
                .raise_abort(message)
                .execute(connection),
        )?;
    }
    block_on(
        create_trigger("quiescence_blocks_reopen", "workflow_runs")
            .if_not_exists(true)
            .before_update_of(["status"])
            .when(E::And(vec![
                E::Compare(
                    Box::new(E::new_column("status")),
                    SchemaComparison::NotEqual,
                    Box::new(E::old_column("status")),
                ),
                exists("workflow_quiescence", "run_id", E::old_column("run_id")),
            ]))
            .raise_abort("quiescent terminal outcome is stable")
            .execute(connection),
    )?;
    for (name, table, columns) in [
        (
            "workflow_quiescence_children",
            "workflow_run_links",
            vec!["parent_run_id", "child_run_id"],
        ),
        (
            "workflow_quiescence_attempts",
            "workflow_attempts",
            vec!["run_id", "dispatch_identity"],
        ),
    ] {
        block_on(
            create_index(name)
                .if_not_exists(true)
                .table(table)
                .columns(columns)
                .execute(connection),
        )?;
    }
    Ok(())
}

fn initialize_replacement_cursors(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(
        create_table("workflow_replacement_intents")
            .if_not_exists(true)
            .columns(vec![
                column("old_run_id", DataType::Text, None),
                column("successor_run_id", DataType::Text, None),
                column("successor_json", DataType::Text, None),
                column("requested_at_ms", DataType::BigInt, None),
            ])
            .primary_key("old_run_id")
            .unique_columns(["successor_run_id"])
            .foreign_key_column("old_run_id", "workflow_runs", "run_id")
            .execute(connection),
    )?;
    for (table, key, cursor, foreign) in [
        (
            "workflow_discovery_cursors",
            "artifact_id",
            "after_run_id",
            false,
        ),
        (
            "workflow_receipt_cursors",
            "run_id",
            "after_dispatch_identity",
            true,
        ),
    ] {
        let statement = create_table(table)
            .if_not_exists(true)
            .columns(vec![
                column(key, DataType::Text, None),
                column(cursor, DataType::Text, None),
            ])
            .primary_key(key);
        let statement = if foreign {
            statement.foreign_key_column(key, "workflow_runs", "run_id")
        } else {
            statement
        };
        block_on(statement.execute(connection))?;
    }
    Ok(())
}

fn initialize_recovery_barriers(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    block_on(
        create_table("workflow_recovery_barriers")
            .if_not_exists(true)
            .columns(vec![
                column("run_id", DataType::Text, None),
                column("source_artifact_id", DataType::Text, None),
                column("created_at_ms", DataType::BigInt, None),
            ])
            .primary_key("run_id")
            .foreign_key_column("run_id", "workflow_runs", "run_id")
            .execute(connection),
    )?;
    block_on(
        create_trigger("recovery_blocks_attempt_insert", "workflow_attempts")
            .if_not_exists(true)
            .before_insert()
            .when(exists(
                "workflow_recovery_barriers",
                "run_id",
                E::new_column("run_id"),
            ))
            .raise_abort("workflow recovery prohibits new attempts")
            .execute(connection),
    )?;
    block_on(
        create_trigger("recovery_blocks_resume", "workflow_runs")
            .if_not_exists(true)
            .before_update_of(["status"])
            .when(E::And(vec![
                equal(E::new_column("status"), E::Text("running".into())),
                exists(
                    "workflow_recovery_barriers",
                    "run_id",
                    E::new_column("run_id"),
                ),
            ]))
            .raise_abort("workflow recovery prohibits resume")
            .execute(connection),
    )?;
    // Nested correlated existence is equivalent to the former attempts/barriers join.
    let handoff = E::Exists {
        table: "workflow_attempts".into(),
        predicate: Box::new(E::And(vec![
            equal(
                E::column("dispatch_identity"),
                E::new_column("dispatch_identity"),
            ),
            E::Exists {
                table: "workflow_recovery_barriers".into(),
                predicate: Box::new(equal(
                    E::Column {
                        table: Some("workflow_recovery_barriers".into()),
                        column: "run_id".into(),
                    },
                    E::Column {
                        table: Some("workflow_attempts".into()),
                        column: "run_id".into(),
                    },
                )),
            },
        ])),
    };
    block_on(
        create_trigger("recovery_blocks_handoff", "workflow_dispatch_handoffs")
            .if_not_exists(true)
            .before_update_of(["handed_off"])
            .when(E::And(vec![
                equal(E::new_column("handed_off"), E::Integer(1)),
                handoff,
            ]))
            .raise_abort("workflow recovery prohibits dispatch")
            .execute(connection),
    )?;
    Ok(())
}

pub fn receipt_predicate() -> SchemaExpression {
    use SchemaExpression as E;
    E::And(vec![
        E::In(
            Box::new(E::column("status")),
            ["admitted", "running", "cancelling", "sibling_cancelling"]
                .into_iter()
                .map(|status| E::Text(status.into()))
                .collect(),
        ),
        E::column("receipt_json").compare(SchemaComparison::IsNot, E::Null),
    ])
}

pub fn initialize(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    initialize_quiescence(connection)?;
    initialize_replacement_cursors(connection)?;
    initialize_recovery_barriers(connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchy_database::schema::{create_index, drop_index};

    #[test]
    fn required_columns_reject_missing_columns_and_tables() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::WorkflowStore::initialize_in_state_dir(temp.path(), 1).unwrap();
        let db: &dyn Database = &store.connection;
        verify_columns(
            db,
            "workflow_recovery_barriers",
            &["run_id", "source_artifact_id"],
        )
        .unwrap();
        assert!(verify_columns(db, "workflow_recovery_barriers", &["absent_column"]).is_err());
        assert!(verify_columns(db, "absent_table", &["run_id"]).is_err());
        assert!(store.list_runs(10).unwrap().is_empty());
    }

    #[test]
    fn existing_in_receipt_index_remains_usable_and_incompatible_index_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::WorkflowStore::initialize_in_state_dir(temp.path(), 1).unwrap();
        let db: &dyn Database = &store.connection;
        block_on(drop_index("workflow_receipt_recovery_page", "workflow_attempts").execute(db))
            .unwrap();
        // Independently recreate the historical IN/IS NOT NULL index shape using typed DDL.
        let historical = SchemaExpression::And(vec![
            SchemaExpression::In(
                Box::new(SchemaExpression::column("status")),
                vec![
                    SchemaExpression::Text("admitted".into()),
                    SchemaExpression::Text("running".into()),
                    SchemaExpression::Text("cancelling".into()),
                    SchemaExpression::Text("sibling_cancelling".into()),
                ],
            ),
            SchemaExpression::column("receipt_json")
                .compare(SchemaComparison::IsNot, SchemaExpression::Null),
        ]);
        block_on(
            create_index("workflow_receipt_recovery_page")
                .table("workflow_attempts")
                .columns(vec!["run_id", "dispatch_identity"])
                .predicate(historical)
                .execute(db),
        )
        .unwrap();
        verify(db).unwrap();
        block_on(drop_index("workflow_receipt_recovery_page", "workflow_attempts").execute(db))
            .unwrap();
        block_on(
            create_index("workflow_receipt_recovery_page")
                .table("workflow_attempts")
                .columns(vec!["run_id", "dispatch_identity"])
                .predicate(SchemaExpression::column("status").compare(
                    SchemaComparison::Equal,
                    SchemaExpression::Text("succeeded".into()),
                ))
                .execute(db),
        )
        .unwrap();
        assert!(verify(db).is_err());
    }
}
