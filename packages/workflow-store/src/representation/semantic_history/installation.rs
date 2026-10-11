//! Additive, transactional installation. No shared execution contract is changed.
use super::{
    ACKS, CONTRACT, Database, DatabaseValue, EVENTS, Error, FIELDS, OBLIGATIONS, ORIGINS, SOURCE,
};
use super::{checkpoint_guards, checkpoints::TABLE as CHECKPOINTS};
use switchy_database::{
    schema::write_guard::{
        GuardEvent, GuardPredicate as P, GuardScalar as S, GuardValue as V, ObservationColumn,
        ObservationEvent, WriteGuard, WriteObservation,
    },
    schema::{Column, DataType, create_index, create_table},
};

fn col(name: &str, data_type: DataType, nullable: bool) -> Column {
    Column {
        name: name.into(),
        data_type,
        nullable,
        auto_increment: false,
        default: None,
    }
}
fn columns(table: &str) -> Vec<Column> {
    match table {
        CHECKPOINTS => {
            let mut id = col("id", DataType::Int, false);
            id.auto_increment = true;
            vec![
                id,
                col("run_id", DataType::Text, false),
                col("sequence", DataType::Int, false),
                col("last_verified", DataType::Int, false),
                col("gap", DataType::Int, false),
                col("obligation", DataType::Int, false),
                col("event_identity", DataType::Text, false),
            ]
        }
        CONTRACT => vec![col("version", DataType::Int, false)],
        ORIGINS => vec![col("run_id", DataType::Text, false)],
        ACKS => vec![
            col("obligation", DataType::Int, false),
            col("event_identity", DataType::Text, false),
        ],
        EVENTS => vec![
            col("identity", DataType::Text, false),
            col("run_id", DataType::Text, false),
            col("sequence", DataType::Int, false),
            col("event_json", DataType::Text, false),
        ],
        _ => {
            let mut sequence = col("sequence", DataType::Int, false);
            sequence.auto_increment = true;
            let mut result = vec![sequence, col("operation", DataType::Text, false)];
            for prefix in ["old", "new"] {
                for field in FIELDS {
                    let numeric = matches!(*field, "attempt_number" | "terminal_at_ms");
                    result.push(col(
                        &format!("{prefix}_{field}"),
                        if numeric {
                            DataType::Int
                        } else {
                            DataType::Text
                        },
                        true,
                    ));
                    if !numeric {
                        result.push(col(
                            &format!("{prefix}_{field}_status"),
                            DataType::Int,
                            false,
                        ));
                    }
                }
            }
            result
        }
    }
}
fn key(table: &str) -> &str {
    match table {
        CHECKPOINTS => "id",
        CONTRACT => "version",
        ORIGINS => "run_id",
        ACKS => "obligation",
        EVENTS => "identity",
        _ => "sequence",
    }
}
fn observers() -> Vec<WriteObservation> {
    let mut result = vec![WriteObservation {
        name: "attempt_history_origin_v1".into(),
        table: "workflow_runs_v2".into(),
        event: ObservationEvent::AfterInsert,
        when: checkpoint_guards::origin_condition(),
        target_table: ORIGINS.into(),
        columns: vec![ObservationColumn {
            column: "run_id".into(),
            value: V::NewColumn("run_id".into()),
        }],
    }];
    for (operation, event) in [
        ("insert", ObservationEvent::AfterInsert),
        ("update", ObservationEvent::AfterUpdate),
        ("delete", ObservationEvent::AfterDelete),
    ] {
        let mut mappings = vec![ObservationColumn {
            column: "operation".into(),
            value: V::Constant(S::Text(operation.into())),
        }];
        for prefix in ["old", "new"] {
            for field in FIELDS {
                let leaf = if (prefix == "old" && operation == "insert")
                    || (prefix == "new" && operation == "delete")
                {
                    V::Constant(S::Null)
                } else if prefix == "old" {
                    V::OldColumn((*field).into())
                } else {
                    V::NewColumn((*field).into())
                };
                let numeric = matches!(*field, "attempt_number" | "terminal_at_ms");
                mappings.push(ObservationColumn {
                    column: format!("{prefix}_{field}"),
                    value: if numeric {
                        leaf.clone()
                    } else {
                        V::BoundedText {
                            value: Box::new(leaf.clone()),
                            max_bytes: 1_048_576,
                        }
                    },
                });
                if !numeric {
                    mappings.push(ObservationColumn {
                        column: format!("{prefix}_{field}_status"),
                        value: V::BoundedTextStatus {
                            value: Box::new(leaf),
                            max_bytes: 1_048_576,
                        },
                    });
                }
            }
        }
        result.push(WriteObservation {
            name: format!("attempt_history_{operation}_v1"),
            table: SOURCE.into(),
            event,
            when: P::Equal(V::Constant(S::Integer(1)), V::Constant(S::Integer(1))),
            target_table: OBLIGATIONS.into(),
            columns: mappings,
        });
    }
    result.push(checkpoint_guards::observer());
    result
}
fn guards() -> Vec<WriteGuard> {
    let mut result: Vec<_> = [CONTRACT, ORIGINS, OBLIGATIONS, EVENTS, ACKS, CHECKPOINTS]
        .into_iter()
        .flat_map(|table| {
            [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete]
                .into_iter()
                .map(move |event| WriteGuard {
                    name: format!("immutable_{table}_{event:?}"),
                    table: table.into(),
                    event,
                    reject_when: P::Equal(V::Constant(S::Integer(1)), V::Constant(S::Integer(1))),
                    violation: "workflow_attempt_history_append_only".into(),
                })
        })
        .collect();
    result.push(checkpoint_guards::guard());
    result
}
pub(super) async fn install_in(db: &dyn Database) -> Result<(), Error> {
    if db.get_table_info(CONTRACT).await?.is_some() {
        return verify(db).await;
    }
    for table in [CONTRACT, ORIGINS, OBLIGATIONS, EVENTS, ACKS, CHECKPOINTS] {
        let mut statement = create_table(table);
        for column in columns(table) {
            statement = statement.column(column);
        }
        statement.primary_key(key(table)).execute(db).await?;
    }
    create_index("idx_attempt_history_checkpoint_run_sequence_v2")
        .table(CHECKPOINTS)
        .columns(vec!["run_id", "sequence"])
        .unique(true)
        .execute(db)
        .await?;
    create_index("idx_attempt_history_source_run_v2")
        .table(SOURCE)
        .columns(vec!["run_id"])
        .execute(db)
        .await?;
    create_index("idx_attempt_history_run_sequence_v1")
        .table(EVENTS)
        .columns(vec!["run_id", "sequence"])
        .unique(true)
        .execute(db)
        .await?;
    for field in ["old_run_id", "new_run_id"] {
        create_index(&format!("idx_attempt_history_{field}_v1"))
            .table(OBLIGATIONS)
            .columns(vec![field, "sequence"])
            .execute(db)
            .await?;
    }
    for observer in observers() {
        db.install_write_observation(&observer).await?;
    }
    db.insert(CONTRACT)
        .value("version", 2_i64)
        .execute(db)
        .await?;
    for guard in guards() {
        db.install_write_guard(&guard).await?;
    }
    verify(db).await
}
pub(super) async fn verify(db: &dyn Database) -> Result<(), Error> {
    for table in [CONTRACT, ORIGINS, OBLIGATIONS, EVENTS, ACKS, CHECKPOINTS] {
        let info = db
            .get_table_info(table)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        let expected = columns(table);
        if info.columns.len() != expected.len() || !info.foreign_keys.is_empty() {
            return Err(Error::MaintenanceRequired);
        }
        for column in expected {
            let actual = info
                .columns
                .get(&column.name)
                .ok_or(Error::MaintenanceRequired)?;
            let expected_type = if column.data_type == DataType::Int {
                DataType::BigInt
            } else {
                column.data_type
            };
            if actual.data_type != expected_type
                || actual.nullable != column.nullable
                || actual.is_primary_key != (column.name == key(table))
                || actual.auto_increment != column.auto_increment
            {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    for (table, name, fields, unique) in [
        (
            EVENTS,
            "idx_attempt_history_run_sequence_v1",
            vec!["run_id", "sequence"],
            true,
        ),
        (
            CHECKPOINTS,
            "idx_attempt_history_checkpoint_run_sequence_v2",
            vec!["run_id", "sequence"],
            true,
        ),
        (
            OBLIGATIONS,
            "idx_attempt_history_old_run_id_v1",
            vec!["old_run_id", "sequence"],
            false,
        ),
        (
            OBLIGATIONS,
            "idx_attempt_history_new_run_id_v1",
            vec!["new_run_id", "sequence"],
            false,
        ),
    ] {
        super::checkpoints::verify_index(db, table, name, &fields, unique).await?;
    }
    let versions = db.select(CONTRACT).limit(2).execute(db).await?;
    if versions.len() != 1 || versions[0].get("version") != Some(DatabaseValue::Int64(2)) {
        return Err(Error::MaintenanceRequired);
    }
    if !db.verify_write_guards(&guards()).await?
        || !db.verify_write_observations(&observers()).await?
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}
