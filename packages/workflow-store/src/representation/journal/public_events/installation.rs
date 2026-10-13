//! Additive independently versioned append-only timed facts; no representation migration.
use super::{CONTRACT, Database, DatabaseValue, EVENTS, Error, ORIGINS};
use switchy_database::schema::{
    Column, DataType, create_index, create_table,
    write_guard::{GuardEvent, GuardPredicate, GuardScalar, GuardValue, WriteGuard},
};
fn columns(table: &str) -> Vec<Column> {
    let column = |name: &str, data_type, auto_increment| Column {
        name: name.into(),
        data_type,
        nullable: false,
        auto_increment,
        default: None,
    };
    match table {
        CONTRACT => vec![column("version", DataType::Int, false)],
        ORIGINS => vec![
            column("run_id", DataType::Text, false),
            column("version", DataType::Int, false),
        ],
        _ => vec![
            column("sequence", DataType::Int, true),
            column("identity", DataType::Text, false),
            column("run_id", DataType::Text, false),
            column("payload", DataType::Text, false),
            column("event_type", DataType::Text, false),
            column("recorded_at_ms", DataType::Int, false),
        ],
    }
}
fn primary(table: &str) -> &str {
    match table {
        CONTRACT => "version",
        ORIGINS => "run_id",
        _ => "sequence",
    }
}
fn guards() -> Vec<WriteGuard> {
    let mut guards: Vec<_> = [CONTRACT, ORIGINS, EVENTS]
        .into_iter()
        .flat_map(|table| {
            [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete]
                .into_iter()
                .map(move |event| WriteGuard {
                    name: format!(
                        "immutable_{table}_{}",
                        if event == GuardEvent::BeforeUpdate {
                            "update"
                        } else {
                            "delete"
                        }
                    ),
                    table: table.into(),
                    event,
                    reject_when: GuardPredicate::Equal(
                        GuardValue::Constant(GuardScalar::Integer(1)),
                        GuardValue::Constant(GuardScalar::Integer(1)),
                    ),
                    violation: "workflow_public_events_append_only".into(),
                })
        })
        .collect();
    guards.push(WriteGuard {
        name: "workflow_public_events_origin_before_admission_v1".into(),
        table: ORIGINS.into(),
        event: GuardEvent::BeforeInsert,
        reject_when: GuardPredicate::Exists(switchy_database::schema::write_guard::GuardRelation {
            table: "workflow_runs_v2".into(),
            alias: "admitted".into(),
            joins: vec![],
            predicate: Box::new(GuardPredicate::Equal(
                GuardValue::Column {
                    relation: "admitted".into(),
                    column: "run_id".into(),
                },
                GuardValue::NewColumn("run_id".into()),
            )),
        }),
        violation: "workflow_public_events_origin_requires_new_run".into(),
    });
    for source in [
        "workflow_attempt_history_events_v1",
        "workflow_lifecycle_history_events_v1",
    ] {
        let relation = |table: &str, alias: &str, predicate| {
            switchy_database::schema::write_guard::GuardRelation {
                table: table.into(),
                alias: alias.into(),
                joins: vec![],
                predicate: Box::new(predicate),
            }
        };
        let equal = |alias: &str, column: &str| {
            GuardPredicate::Equal(
                GuardValue::Column {
                    relation: alias.into(),
                    column: column.into(),
                },
                GuardValue::NewColumn(column.into()),
            )
        };
        guards.push(WriteGuard {
            name: format!("workflow_public_events_requires_{source}"),
            table: source.into(),
            event: GuardEvent::BeforeInsert,
            reject_when: GuardPredicate::All(vec![
                GuardPredicate::Exists(relation(ORIGINS, "origin", equal("origin", "run_id"))),
                GuardPredicate::NotExists(relation(
                    EVENTS,
                    "timed",
                    GuardPredicate::All(vec![equal("timed", "run_id"), equal("timed", "identity")]),
                )),
            ]),
            violation: "workflow_public_event_missing_for_committed_source".into(),
        });
    }
    guards
}
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    if db.get_table_info(CONTRACT).await?.is_some() {
        return verify(db).await;
    }
    if db.get_table_info(ORIGINS).await?.is_some() || db.get_table_info(EVENTS).await?.is_some() {
        return Err(Error::MaintenanceRequired);
    }
    for table in [CONTRACT, ORIGINS, EVENTS] {
        let mut statement = create_table(table);
        for column in columns(table) {
            statement = statement.column(column);
        }
        statement.primary_key(primary(table)).execute(db).await?;
    }
    create_index("workflow_public_events_run_sequence_v1")
        .table(EVENTS)
        .columns(vec!["run_id", "sequence"])
        .execute(db)
        .await?;
    create_index("workflow_public_events_identity_v1")
        .table(EVENTS)
        .columns(vec!["identity"])
        .unique(true)
        .execute(db)
        .await?;
    db.insert(CONTRACT)
        .value("version", 1_i64)
        .execute(db)
        .await?;
    for guard in guards() {
        db.install_write_guard(&guard).await?;
    }
    verify(db).await
}
pub(super) async fn verify(db: &dyn Database) -> Result<(), Error> {
    for name in [CONTRACT, ORIGINS, EVENTS] {
        let table = db
            .get_table_info(name)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        if table.columns.len() != columns(name).len() {
            return Err(Error::MaintenanceRequired);
        }
        for expected in columns(name) {
            let actual = table
                .columns
                .get(&expected.name)
                .ok_or(Error::MaintenanceRequired)?;
            let expected_type = if expected.data_type == DataType::Int {
                DataType::BigInt
            } else {
                expected.data_type
            };
            if actual.data_type != expected_type
                || actual.nullable
                || actual.auto_increment != expected.auto_increment
                || actual.is_primary_key != (expected.name == primary(name))
                || actual.default_value.is_some()
            {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    let versions = db.select(CONTRACT).limit(2).execute(db).await?;
    if versions.len() != 1 || versions[0].get("version") != Some(DatabaseValue::Int64(1)) {
        return Err(Error::MaintenanceRequired);
    }
    for (name, expected_columns, unique) in [
        (
            "workflow_public_events_run_sequence_v1",
            vec!["run_id".to_owned(), "sequence".to_owned()],
            false,
        ),
        (
            "workflow_public_events_identity_v1",
            vec!["identity".to_owned()],
            true,
        ),
    ] {
        let table = db
            .get_table_info(EVENTS)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        let index = table.indexes.get(name).ok_or(Error::MaintenanceRequired)?;
        if index.columns != expected_columns || index.unique != unique {
            return Err(Error::MaintenanceRequired);
        }
    }
    if !db.verify_write_guards(&guards()).await? {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}
