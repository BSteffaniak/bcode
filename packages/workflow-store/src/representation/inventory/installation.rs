//! Explicit atomic installation, never repair during inventory reads.
use super::{CONTRACT, EVENTS, VERSION, observers};
use crate::representation::{Database, DatabaseValue, RepresentationError as Error, column};
use switchy_database::schema::write_guard::{
    GuardEvent, GuardPredicate, GuardScalar, GuardValue, WriteGuard,
};
use switchy_database::schema::{Column, DataType, create_index, create_table};
const INDEXES: &[(&str, &str, &[&str])] = &[
    (
        "idx_inventory_run_sequence",
        EVENTS,
        &["run_id", "sequence"],
    ),
    (
        "idx_inventory_status_sequence",
        EVENTS,
        &["attribution_status", "sequence"],
    ),
    (
        "idx_inventory_outputs_run_key",
        "workflow_outputs_v2",
        &["run_id", "output_id"],
    ),
    (
        "idx_inventory_attempts_run_key",
        "workflow_attempts_v2",
        &["run_id", "dispatch_identity"],
    ),
    (
        "idx_workflow_lineage_parent_run",
        "workflow_lineage_v2",
        &["parent_run_id", "run_id"],
    ),
    (
        "idx_inventory_run_source_sequence",
        EVENTS,
        &["run_id", "source_table", "sequence"],
    ),
];
fn columns(table: &str) -> Vec<Column> {
    if table == CONTRACT {
        vec![column("version", DataType::Int, false)]
    } else {
        vec![
            column("sequence", DataType::Int, false),
            column("source_table", DataType::Text, false),
            column("run_id", DataType::Text, true),
            column("attribution_status", DataType::Int, false),
        ]
    }
}
fn guards() -> Vec<WriteGuard> {
    [CONTRACT, EVENTS]
        .into_iter()
        .flat_map(|table| {
            [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete]
                .into_iter()
                .map(move |event| WriteGuard {
                    name: format!(
                        "inventory_immutable_{table}_{}",
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
                    violation: "workflow_inventory_immutable".into(),
                })
        })
        .collect()
}
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    if db.get_table_info(CONTRACT).await?.is_some() {
        return verify(db).await;
    }
    for table in [CONTRACT, EVENTS] {
        let mut query = create_table(table);
        for column in columns(table) {
            query = query.column(column);
        }
        query
            .primary_key(if table == CONTRACT {
                "version"
            } else {
                "sequence"
            })
            .execute(db)
            .await?;
    }
    for (name, table, keys) in INDEXES {
        // Preserve the existing lineage index, verifying rather than replacing it.
        if *table != "workflow_lineage_v2" {
            create_index(name)
                .table(table)
                .columns(keys.to_vec())
                .execute(db)
                .await?;
        }
    }
    // Existing candidates have no typed run attribution. Preserve that initial
    // uncertainty permanently, even if an old writer later deletes the row.
    if !db
        .select("workflow_execution_graph_candidates_v2")
        .columns(&["identity"])
        .limit(1)
        .execute(db)
        .await?
        .is_empty()
    {
        db.insert(EVENTS)
            .value("run_id", DatabaseValue::Null)
            .value("source_table", "workflow_execution_graph_candidates_v2")
            .value("attribution_status", 0_i64)
            .execute(db)
            .await?;
    }
    // Until a bounded typed anti-join proves existing session attribution, an
    // existing link also records a conservative permanent initialization gap.
    if !db
        .select("workflow_execution_sessions_v2")
        .columns(&["dispatch_identity"])
        .limit(1)
        .execute(db)
        .await?
        .is_empty()
    {
        db.insert(EVENTS)
            .value("run_id", DatabaseValue::Null)
            .value("source_table", "workflow_execution_sessions_v2")
            .value("attribution_status", 0_i64)
            .execute(db)
            .await?;
    }
    for observer in observers::definitions() {
        db.install_write_observation(&observer).await?;
    }
    db.insert(CONTRACT)
        .value("version", VERSION)
        .execute(db)
        .await?;
    for guard in guards() {
        db.install_write_guard(&guard).await?;
    }
    verify(db).await
}
pub(super) async fn verify(db: &dyn Database) -> Result<(), Error> {
    for table in [CONTRACT, EVENTS] {
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
                || actual.default_value.is_some()
                || actual.auto_increment
                || actual.is_primary_key
                    != (column.name
                        == if table == CONTRACT {
                            "version"
                        } else {
                            "sequence"
                        })
            {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    for (name, table, keys) in INDEXES {
        let info = db
            .get_table_info(table)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        let index = info.indexes.get(*name).ok_or(Error::MaintenanceRequired)?;
        if index.unique || index.columns != *keys {
            return Err(Error::MaintenanceRequired);
        }
    }
    let versions = db.select(CONTRACT).limit(2).execute(db).await?;
    if versions.len() != 1 || versions[0].get("version") != Some(DatabaseValue::Int64(VERSION)) {
        return Err(Error::MaintenanceRequired);
    }
    observers::verify_sources(db).await?;
    if !db.verify_write_guards(&guards()).await?
        || !db
            .verify_write_observations(&observers::definitions())
            .await?
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}
