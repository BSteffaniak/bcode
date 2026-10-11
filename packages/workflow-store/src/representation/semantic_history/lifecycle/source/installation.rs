//! Additive lifecycle observers retain facts even for older writers.
use super::{CONTRACT, Database, DatabaseValue, Error, FIELDS, OBLIGATIONS, ORIGINS, RUNS};
use switchy_database::schema::write_guard::{
    GuardEvent, GuardPredicate as P, GuardScalar as S, GuardValue as V, ObservationColumn,
    ObservationEvent, WriteGuard, WriteObservation,
};
use switchy_database::schema::{Column, DataType, create_index, create_table};
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
        CONTRACT => vec![col("version", DataType::Int, false)],
        ORIGINS => vec![col("run_id", DataType::Text, false)],
        _ => {
            let mut sequence = col("sequence", DataType::Int, false);
            sequence.auto_increment = true;
            let mut result = vec![sequence, col("operation", DataType::Text, false)];
            for prefix in ["old", "new"] {
                for field in FIELDS {
                    result.push(col(
                        &format!("{prefix}_{field}"),
                        if *field == "updated_at_ms" {
                            DataType::Int
                        } else {
                            DataType::Text
                        },
                        true,
                    ));
                    result.push(col(
                        &format!("{prefix}_{field}_status"),
                        DataType::Int,
                        false,
                    ));
                }
            }
            result
        }
    }
}
fn key(table: &str) -> &str {
    match table {
        CONTRACT => "version",
        ORIGINS => "run_id",
        _ => "sequence",
    }
}
const fn always() -> P {
    P::Equal(V::Constant(S::Integer(1)), V::Constant(S::Integer(1)))
}
fn status_changed() -> P {
    // NULL and unsupported representations must not silently suppress an obligation.
    P::Any(vec![
        P::NotEqual(V::OldColumn("status".into()), V::NewColumn("status".into())),
        P::IsNull(V::OldColumn("status".into())),
        P::IsNull(V::NewColumn("status".into())),
        P::Not(Box::new(P::ByteLengthAtMost(
            V::OldColumn("status".into()),
            64,
        ))),
        P::Not(Box::new(P::ByteLengthAtMost(
            V::NewColumn("status".into()),
            64,
        ))),
        P::NotEqual(V::OldColumn("run_id".into()), V::NewColumn("run_id".into())),
        P::IsNull(V::OldColumn("run_id".into())),
        P::IsNull(V::NewColumn("run_id".into())),
    ])
}
fn observations() -> Vec<WriteObservation> {
    let mut result = Vec::new();
    for (operation, event) in [
        ("insert", ObservationEvent::AfterInsert),
        ("update", ObservationEvent::AfterUpdate),
        ("delete", ObservationEvent::AfterDelete),
    ] {
        let mut columns = vec![ObservationColumn {
            column: "operation".into(),
            value: V::Constant(S::Text(operation.into())),
        }];
        for prefix in ["old", "new"] {
            for field in FIELDS {
                let leaf = if (operation == "insert" && prefix == "old")
                    || (operation == "delete" && prefix == "new")
                {
                    V::Constant(S::Null)
                } else if prefix == "old" {
                    V::OldColumn((*field).into())
                } else {
                    V::NewColumn((*field).into())
                };
                let numeric = *field == "updated_at_ms";
                let bound = if *field == "status" { 64 } else { 1_048_576 };
                columns.push(ObservationColumn {
                    column: format!("{prefix}_{field}"),
                    value: if numeric {
                        V::IntegerOnly {
                            value: Box::new(leaf.clone()),
                        }
                    } else {
                        V::BoundedText {
                            value: Box::new(leaf.clone()),
                            max_bytes: bound,
                        }
                    },
                });
                columns.push(ObservationColumn {
                    column: format!("{prefix}_{field}_status"),
                    value: if numeric {
                        V::IntegerStatus {
                            value: Box::new(leaf),
                        }
                    } else {
                        V::BoundedTextStatus {
                            value: Box::new(leaf),
                            max_bytes: bound,
                        }
                    },
                });
            }
        }
        result.push(WriteObservation {
            name: format!("lifecycle_history_{operation}_v1"),
            table: RUNS.into(),
            event,
            when: if operation == "update" {
                status_changed()
            } else {
                always()
            },
            target_table: OBLIGATIONS.into(),
            columns,
        });
    }
    // Origin is tied to the durable insert obligation, not to a later snapshot.
    result.push(WriteObservation {
        name: "lifecycle_history_origin_v1".into(),
        table: OBLIGATIONS.into(),
        event: ObservationEvent::AfterInsert,
        when: P::All(vec![
            P::Equal(
                V::NewColumn("operation".into()),
                V::Constant(S::Text("insert".into())),
            ),
            P::Equal(
                V::NewColumn("new_run_id_status".into()),
                V::Constant(S::Integer(1)),
            ),
        ]),
        target_table: ORIGINS.into(),
        columns: vec![ObservationColumn {
            column: "run_id".into(),
            value: V::NewColumn("new_run_id".into()),
        }],
    });
    result
}
fn guards() -> Vec<WriteGuard> {
    [CONTRACT, ORIGINS, OBLIGATIONS]
        .into_iter()
        .flat_map(|table| {
            [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete]
                .into_iter()
                .map(move |event| WriteGuard {
                    name: format!("immutable_{table}_{event:?}"),
                    table: table.into(),
                    event,
                    reject_when: always(),
                    violation: "workflow_lifecycle_history_append_only".into(),
                })
        })
        .collect()
}
pub(super) async fn install_in(db: &dyn Database) -> Result<(), Error> {
    if db.get_table_info(CONTRACT).await?.is_some() {
        return verify(db).await;
    }
    for table in [CONTRACT, ORIGINS, OBLIGATIONS] {
        let mut statement = create_table(table);
        for column in columns(table) {
            statement = statement.column(column);
        }
        statement.primary_key(key(table)).execute(db).await?;
    }
    for field in ["old_run_id", "new_run_id"] {
        create_index(&format!("idx_lifecycle_history_{field}_v1"))
            .table(OBLIGATIONS)
            .columns(vec![field, "sequence"])
            .execute(db)
            .await?;
    }
    for observation in observations() {
        db.install_write_observation(&observation).await?;
    }
    db.insert(CONTRACT)
        .value("version", 1_i64)
        .execute(db)
        .await?;
    for guard in guards() {
        db.install_write_guard(&guard).await?;
    }
    verify(db).await
}
pub(in crate::representation::semantic_history) async fn verify(
    db: &dyn Database,
) -> Result<(), Error> {
    for table in [CONTRACT, ORIGINS, OBLIGATIONS] {
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
            let data_type = if column.data_type == DataType::Int {
                DataType::BigInt
            } else {
                column.data_type
            };
            if actual.data_type != data_type
                || actual.nullable != column.nullable
                || actual.auto_increment != column.auto_increment
                || actual.is_primary_key != (column.name == key(table))
            {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    for field in ["old_run_id", "new_run_id"] {
        super::super::super::checkpoints::verify_index(
            db,
            OBLIGATIONS,
            &format!("idx_lifecycle_history_{field}_v1"),
            &[field, "sequence"],
            false,
        )
        .await?;
    }
    let versions = db.select(CONTRACT).limit(2).execute(db).await?;
    if versions.len() != 1
        || versions[0].get("version") != Some(DatabaseValue::Int64(1))
        || !db.verify_write_guards(&guards()).await?
        || !db.verify_write_observations(&observations()).await?
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}
