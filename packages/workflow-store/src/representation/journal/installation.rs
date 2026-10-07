//! Atomic additive schema and exact typed observer verification.
use super::{CONTRACT, COVERAGE, Database, EVENTS, Error, VERSION, observers};
use switchy_database::{
    DatabaseValue,
    schema::write_guard::{GuardEvent, GuardPredicate, GuardScalar, GuardValue, WriteGuard},
    schema::{Column, DataType, create_index, create_table},
};

fn immutable_guards() -> Vec<WriteGuard> {
    [CONTRACT, COVERAGE, EVENTS]
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
                    violation: "workflow_observation_append_only".into(),
                })
        })
        .collect()
}

fn schema(table: &str) -> Vec<Column> {
    let col = |name: &str, data_type, nullable| Column {
        name: name.into(),
        data_type,
        nullable,
        auto_increment: false,
        default: None,
    };
    match table {
        CONTRACT => vec![col("version", DataType::Int, false)],
        COVERAGE => vec![
            col("run_id", DataType::Text, false),
            col("version", DataType::Int, false),
        ],
        EVENTS => {
            let mut sequence = col("sequence", DataType::Int, false);
            sequence.auto_increment = true;
            let mut columns = vec![
                sequence,
                col("run_id", DataType::Text, false),
                col("source_table", DataType::Text, false),
                col("operation", DataType::Text, false),
            ];
            for name in observers::FACTS {
                let data_type = if [
                    "revision",
                    "generation",
                    "count",
                    "current",
                    "skipped",
                    "active_attempts",
                    "timestamp_ms",
                ]
                .contains(name)
                {
                    DataType::Int
                } else {
                    DataType::Text
                };
                columns.push(col(name, data_type, true));
                columns.push(col(&format!("{name}_status"), DataType::Int, true));
            }
            columns.push(col("timestamp_source", DataType::Text, true));
            columns.push(col("previous_timestamp_ms", DataType::Int, true));
            columns.push(col("previous_timestamp_ms_status", DataType::Int, true));
            columns
        }
        _ => unreachable!("finite journal schema"),
    }
}

pub async fn install_in(db: &dyn Database) -> Result<(), Error> {
    if db.get_table_info(CONTRACT).await?.is_some() {
        return verify(db).await;
    }
    for table in [CONTRACT, COVERAGE, EVENTS] {
        let mut statement = create_table(table);
        for column in schema(table) {
            statement = statement.column(column);
        }
        statement
            .primary_key(match table {
                CONTRACT => "version",
                COVERAGE => "run_id",
                _ => "sequence",
            })
            .execute(db)
            .await?;
    }
    create_index("idx_workflow_observations_v1_run_sequence")
        .table(EVENTS)
        .columns(vec!["run_id", "sequence"])
        .execute(db)
        .await?;
    for observer in observers::definitions() {
        db.install_write_observation(&observer).await?;
    }
    db.insert(CONTRACT)
        .value("version", VERSION)
        .execute(db)
        .await?;
    for guard in immutable_guards() {
        db.install_write_guard(&guard).await?;
    }
    verify(db).await
}

pub(super) async fn verify(db: &dyn Database) -> Result<(), Error> {
    for table in [CONTRACT, COVERAGE, EVENTS] {
        let info = db
            .get_table_info(table)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        let expected = schema(table);
        if info.columns.len() != expected.len() || !info.foreign_keys.is_empty() {
            return Err(Error::MaintenanceRequired);
        }
        let primary = match table {
            CONTRACT => "version",
            COVERAGE => "run_id",
            _ => "sequence",
        };
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
                || actual.is_primary_key != (column.name == primary)
                || actual.auto_increment != column.auto_increment
                || actual.default_value.is_some()
            {
                return Err(Error::MaintenanceRequired);
            }
        }
        if table == EVENTS {
            let index = info
                .indexes
                .get("idx_workflow_observations_v1_run_sequence")
                .ok_or(Error::MaintenanceRequired)?;
            if index.unique || index.columns != ["run_id", "sequence"] {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    let versions = db.select(CONTRACT).limit(2).execute(db).await?;
    if versions.len() != 1 || versions[0].get("version") != Some(DatabaseValue::Int64(VERSION)) {
        return Err(Error::MaintenanceRequired);
    }
    for source in observers::coverage() {
        let table = db
            .get_table_info(&source.table)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        for name in &source.columns {
            let actual = table.columns.get(name).ok_or(Error::MaintenanceRequired)?;
            let integer = [
                "revision",
                "generation",
                "consumed",
                "active_attempts",
                "attempt_number",
                "value",
                "updated_at_ms",
                "created_at_ms",
                "terminal_at_ms",
                "current",
                "skipped",
            ]
            .contains(&name.as_str());
            let expected = if integer {
                DataType::BigInt
            } else {
                DataType::Text
            };
            let bool_column = ["current", "skipped"].contains(&name.as_str());
            if actual.data_type != expected && !(bool_column && actual.data_type == DataType::Bool)
            {
                return Err(Error::MaintenanceRequired);
            }
            if actual.nullable != (name == "terminal_at_ms") {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    if !db.verify_write_guards(&immutable_guards()).await? {
        return Err(Error::MaintenanceRequired);
    }
    if !db
        .verify_write_observations(&observers::definitions())
        .await?
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}
