//! Independent additive lifecycle event/checkpoint installation.
use super::{ACKS, CHECKPOINTS, Database, EVENTS, Error, TABLES, source};
use switchy_database::schema::write_guard::{
    GuardEvent, GuardPredicate as P, GuardScalar as S, GuardValue as V, WriteGuard,
};
use switchy_database::schema::{Column, DataType, create_index, create_table};
fn col(name: &str, data_type: DataType) -> Column {
    Column {
        name: name.into(),
        data_type,
        nullable: false,
        auto_increment: false,
        default: None,
    }
}
fn columns(table: &str) -> Vec<Column> {
    match table {
        EVENTS => vec![
            col("identity", DataType::Text),
            col("run_id", DataType::Text),
            col("sequence", DataType::Int),
            col("event_json", DataType::Text),
        ],
        ACKS => vec![
            col("obligation", DataType::Int),
            col("event_identity", DataType::Text),
        ],
        _ => {
            let mut id = col("id", DataType::Int);
            id.auto_increment = true;
            vec![
                id,
                col("run_id", DataType::Text),
                col("sequence", DataType::Int),
                col("last_verified", DataType::Int),
                col("gap", DataType::Int),
                col("obligation", DataType::Int),
                col("event_identity", DataType::Text),
            ]
        }
    }
}
fn key(table: &str) -> &str {
    match table {
        EVENTS => "identity",
        ACKS => "obligation",
        _ => "id",
    }
}
fn guards() -> Vec<WriteGuard> {
    let mut guards: Vec<_> = [EVENTS, ACKS, CHECKPOINTS]
        .into_iter()
        .flat_map(|table| {
            [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete]
                .into_iter()
                .map(move |event| WriteGuard {
                    name: format!("immutable_{table}_{event:?}"),
                    table: table.into(),
                    event,
                    reject_when: P::Equal(V::Constant(S::Integer(1)), V::Constant(S::Integer(1))),
                    violation: "workflow_lifecycle_history_append_only".into(),
                })
        })
        .collect();
    guards.push(super::super::checkpoint_guards::proof_guard(
        TABLES,
        "lifecycle_history_checkpoint_proof_v1",
        "workflow_lifecycle_history_checkpoint_invalid",
        None,
    ));
    guards
}
pub(super) async fn install_in(db: &dyn Database) -> Result<(), Error> {
    if db.get_table_info(source::CONTRACT).await?.is_some() {
        return verify(db).await;
    }
    source::install_in(db).await?;
    for table in [EVENTS, ACKS, CHECKPOINTS] {
        let mut statement = create_table(table);
        for column in columns(table) {
            statement = statement.column(column);
        }
        statement.primary_key(key(table)).execute(db).await?;
    }
    for (table, index) in [
        (EVENTS, "idx_lifecycle_history_event_run_sequence_v1"),
        (
            CHECKPOINTS,
            "idx_lifecycle_history_checkpoint_run_sequence_v1",
        ),
    ] {
        create_index(index)
            .table(table)
            .columns(vec!["run_id", "sequence"])
            .unique(true)
            .execute(db)
            .await?;
    }
    for guard in guards() {
        db.install_write_guard(&guard).await?;
    }
    verify(db).await
}
pub(super) async fn verify(db: &dyn Database) -> Result<(), Error> {
    source::verify(db).await?;
    for table in [EVENTS, ACKS, CHECKPOINTS] {
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
                || actual.nullable
                || actual.auto_increment != column.auto_increment
                || actual.is_primary_key != (column.name == key(table))
            {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    for (table, name) in [
        (EVENTS, "idx_lifecycle_history_event_run_sequence_v1"),
        (
            CHECKPOINTS,
            "idx_lifecycle_history_checkpoint_run_sequence_v1",
        ),
    ] {
        super::super::checkpoints::verify_index(db, table, name, &["run_id", "sequence"], true)
            .await?;
    }
    if !db.verify_write_guards(&guards()).await? {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}
