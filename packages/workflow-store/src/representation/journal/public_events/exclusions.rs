//! Version-one canonical permission-storage exclusions, not absence of authorization semantics.
use super::{Database, DatabaseValue, Error};
use switchy_database::{
    DatabaseTransaction,
    query::FilterableQuery,
    schema::{
        Column, DataType, create_index, create_table,
        write_guard::{
            GuardEvent, GuardPredicate as P, GuardRelation, GuardScalar as S, GuardValue as V,
            WriteGuard,
        },
    },
};

const ORIGINS: &str = "workflow_permission_exclusion_origins_v1";
const SOURCES: [&str; 3] = [
    "workflow_mutation_approvals",
    "workflow_decisions",
    "workflow_grants",
];

/// Coverage of exactly the three excluded legacy permission collections under contract version one.
/// Does not describe session/tool permissions, canonical approval gates or complete workflow history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The optional exclusion contract has not been installed.
    Unsupported,
    /// No admission origin exists; current emptiness cannot prove historical exclusion.
    HistoricalGap,
    /// All three legacy collections are guarded and verified empty from admission.
    VerifiedExcludedV1,
}

fn columns() -> Vec<Column> {
    [("run_id", DataType::Text), ("version", DataType::Int)]
        .into_iter()
        .map(|(name, data_type)| Column {
            name: name.into(),
            data_type,
            nullable: false,
            auto_increment: false,
            default: None,
        })
        .collect()
}
fn relation(table: &str, alias: &str, predicate: P) -> GuardRelation {
    GuardRelation {
        table: table.into(),
        alias: alias.into(),
        joins: vec![],
        predicate: Box::new(predicate),
    }
}
fn same_run(alias: &str, value: V) -> P {
    P::Equal(
        V::Column {
            relation: alias.into(),
            column: "run_id".into(),
        },
        value,
    )
}
fn new_run() -> V {
    V::NewColumn("run_id".into())
}
fn old_run() -> V {
    V::OldColumn("run_id".into())
}
fn origin_guards() -> Vec<WriteGuard> {
    let mut guards = Vec::new();
    for event in [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete] {
        guards.push(WriteGuard {
            name: format!("immutable_{ORIGINS}_{event:?}"),
            table: ORIGINS.into(),
            event,
            reject_when: P::Equal(V::Constant(S::Integer(1)), V::Constant(S::Integer(1))),
            violation: "workflow_permission_exclusion_immutable".into(),
        });
    }
    let mut invalid = vec![
        P::Not(Box::new(P::Equal(
            V::NewColumn("version".into()),
            V::Constant(S::Integer(1)),
        ))),
        P::Exists(relation(
            "workflow_runs_v2",
            "canonical",
            same_run("canonical", new_run()),
        )),
        P::Exists(relation(
            "workflow_runs",
            "legacy",
            same_run("legacy", new_run()),
        )),
        P::NotExists(relation(
            super::ORIGINS,
            "public",
            same_run("public", new_run()),
        )),
    ];
    for table in SOURCES {
        invalid.push(P::Exists(relation(
            table,
            "prior",
            same_run("prior", new_run()),
        )));
    }
    guards.push(WriteGuard {
        name: "workflow_permission_exclusion_new_origin_v1".into(),
        table: ORIGINS.into(),
        event: GuardEvent::BeforeInsert,
        reject_when: P::Any(invalid),
        violation: "workflow_permission_exclusion_requires_new_empty_run".into(),
    });
    guards
}
fn guards() -> Vec<WriteGuard> {
    let mut guards = origin_guards();
    for table in SOURCES {
        for event in [
            GuardEvent::BeforeInsert,
            GuardEvent::BeforeUpdate,
            GuardEvent::BeforeDelete,
        ] {
            let mut covered = Vec::new();
            if event != GuardEvent::BeforeInsert {
                covered.push(P::Exists(relation(
                    ORIGINS,
                    "old_origin",
                    same_run("old_origin", old_run()),
                )));
            }
            if event != GuardEvent::BeforeDelete {
                covered.push(P::Exists(relation(
                    ORIGINS,
                    "new_origin",
                    same_run("new_origin", new_run()),
                )));
            }
            guards.push(WriteGuard {
                name: format!("permission_excluded_{table}_v1_{event:?}"),
                table: table.into(),
                event,
                reject_when: P::Any(covered),
                violation: "workflow_permission_legacy_collection_excluded_v1".into(),
            });
        }
    }
    guards
}

/// Install optional version-one exclusions in the caller's coordinated installation transaction.
/// Does not opt in runs, change authorization policy or backfill historical origins.
/// # Errors
/// Rejects missing prerequisite/source schema, incompatible origin schema or unavailable typed guards.
pub async fn install_in(tx: &dyn DatabaseTransaction) -> Result<(), Error> {
    super::installation::verify(tx).await?;
    for source in SOURCES {
        if tx.get_table_info(source).await?.is_none() {
            return Err(Error::MaintenanceRequired);
        }
    }
    if tx.get_table_info(ORIGINS).await?.is_some() {
        return verify(tx).await;
    }
    let mut schema = create_table(ORIGINS);
    for column in columns() {
        schema = schema.column(column);
    }
    schema.primary_key("run_id").execute(tx).await?;
    for source in SOURCES {
        create_index(&format!("permission_exclusion_{source}_run_v1"))
            .table(source)
            .columns(vec!["run_id"])
            .execute(tx)
            .await?;
    }
    for guard in guards() {
        tx.install_write_guard(&guard).await?;
    }
    verify(tx).await
}
async fn verify(db: &dyn Database) -> Result<(), Error> {
    super::installation::verify(db).await?;
    let table = db
        .get_table_info(ORIGINS)
        .await?
        .ok_or(Error::MaintenanceRequired)?;
    if table.columns.len() != columns().len() {
        return Err(Error::MaintenanceRequired);
    }
    for expected in columns() {
        let actual = table
            .columns
            .get(&expected.name)
            .ok_or(Error::MaintenanceRequired)?;
        let data_type = if expected.data_type == DataType::Int {
            DataType::BigInt
        } else {
            expected.data_type
        };
        if actual.data_type != data_type
            || actual.nullable
            || actual.auto_increment
            || actual.default_value.is_some()
            || actual.is_primary_key != (expected.name == "run_id")
        {
            return Err(Error::MaintenanceRequired);
        }
    }
    for source in SOURCES {
        let table = db
            .get_table_info(source)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        let run_column = table
            .columns
            .get("run_id")
            .ok_or(Error::MaintenanceRequired)?;
        if run_column.data_type != DataType::Text || run_column.nullable {
            return Err(Error::MaintenanceRequired);
        }
        let index = table
            .indexes
            .get(&format!("permission_exclusion_{source}_run_v1"))
            .ok_or(Error::MaintenanceRequired)?;
        if index.columns != vec!["run_id".to_owned()] || index.unique {
            return Err(Error::MaintenanceRequired);
        }
    }
    if !db.verify_write_guards(&guards()).await? {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}

/// Enable only in a new run's admission transaction, after public-event opt-in and before run insertion.
/// Existing runs/rows and duplicate origins are refused rather than adopted.
/// # Errors
/// Rejects malformed IDs, incompatible installation, historical origins or nonempty legacy sources.
pub async fn enable_in(tx: &dyn DatabaseTransaction, run: &str) -> Result<(), Error> {
    if run.is_empty() || run.len() > 256 {
        return Err(Error::InvalidAdmission);
    }
    verify(tx).await?;
    // Explicit bounded reads complement guards and fail before any origin insertion.
    for table in ["workflow_runs", "workflow_runs_v2"]
        .into_iter()
        .chain(SOURCES)
    {
        if !tx
            .select(table)
            .columns(&["run_id"])
            .where_eq("run_id", run)
            .limit(1)
            .execute(tx)
            .await?
            .is_empty()
        {
            return Err(Error::InvalidAdmission);
        }
    }
    tx.insert(ORIGINS)
        .value("run_id", run)
        .value("version", 1_i64)
        .execute(tx)
        .await?;
    Ok(())
}

/// Verify exclusion and indexed source absence in one read snapshot; never initialize or repair.
/// # Errors
/// Rejects malformed IDs, unknown origins, damaged schema/guards or unexpected source rows.
pub async fn coverage(db: &dyn Database, run: &str) -> Result<Coverage, Error> {
    let tx = db.begin_transaction().await?;
    let result = coverage_in(&*tx, run).await?;
    tx.commit().await?;
    Ok(result)
}

/// Verify within an existing caller-owned read snapshot, such as aggregate inspection.
/// # Errors
/// Rejects malformed IDs, unknown origins, damaged schema/guards or unexpected source rows.
pub async fn coverage_in(tx: &dyn DatabaseTransaction, run: &str) -> Result<Coverage, Error> {
    if run.is_empty() || run.len() > 256 {
        return Err(Error::InvalidAdmission);
    }
    if tx.get_table_info(ORIGINS).await?.is_none() {
        for source in SOURCES {
            if tx.get_table_info(source).await?.is_some_and(|table| {
                table
                    .indexes
                    .contains_key(&format!("permission_exclusion_{source}_run_v1"))
            }) {
                return Err(Error::MaintenanceRequired);
            }
        }
        return Ok(Coverage::Unsupported);
    }
    verify(tx).await?;
    let origins = tx
        .select(ORIGINS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(tx)
        .await?;
    let Some(origin) = origins.first() else {
        return Ok(Coverage::HistoricalGap);
    };
    if origin.get("version") != Some(DatabaseValue::Int64(1)) {
        return Err(Error::MaintenanceRequired);
    }
    // An orphan pre-admission origin is not a canonical run coverage proof.
    if tx
        .select("workflow_runs_v2")
        .columns(&["run_id"])
        .where_eq("run_id", run)
        .limit(1)
        .execute(tx)
        .await?
        .is_empty()
        || !tx
            .select("workflow_runs")
            .columns(&["run_id"])
            .where_eq("run_id", run)
            .limit(1)
            .execute(tx)
            .await?
            .is_empty()
    {
        return Err(Error::MaintenanceRequired);
    }
    for source in SOURCES {
        if !tx
            .select(source)
            .columns(&["run_id"])
            .where_eq("run_id", run)
            .limit(1)
            .execute(tx)
            .await?
            .is_empty()
        {
            return Err(Error::MaintenanceRequired);
        }
    }
    Ok(Coverage::VerifiedExcludedV1)
}

#[cfg(test)]
mod tests;
