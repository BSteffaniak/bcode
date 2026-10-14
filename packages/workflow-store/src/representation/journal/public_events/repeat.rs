//! Independently guarded repeat-emission source. No counter-derived or full-history claims.
use super::{Database, DatabaseValue, Error};
use bcode_workflow::{WorkflowRepeatOutcomeSummary, WorkflowSemanticHistoryCoverage as Coverage};
use switchy_database::{
    DatabaseTransaction,
    query::FilterableQuery,
    schema::{
        Column, DataType, create_table,
        write_guard::{
            GuardEvent, GuardPredicate as P, GuardRelation, GuardScalar as S, GuardValue as V,
            WriteGuard,
        },
    },
};

const ORIGINS: &str = "workflow_repeat_event_origins_v1";
const PROOFS: &str = "workflow_repeat_event_proofs_v1";
const SOURCE: &str = "workflow_repeat_outcomes_v1";
const EVENT_TYPE: &str = "repeat_emission_v1";

fn columns(table: &str) -> Vec<Column> {
    let names = if table == ORIGINS {
        vec![("run_id", DataType::Text), ("version", DataType::Int)]
    } else {
        vec![
            ("output_id", DataType::Text),
            ("run_id", DataType::Text),
            ("identity", DataType::Text),
            ("summary_json", DataType::Text),
            ("recorded_at_ms", DataType::Int),
        ]
    };
    names
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
fn eq(alias: &str, column: &str) -> P {
    P::Equal(
        V::Column {
            relation: alias.into(),
            column: column.into(),
        },
        V::NewColumn(column.into()),
    )
}
const fn constant(value: i64) -> V {
    V::Constant(S::Integer(value))
}
fn guards() -> Vec<WriteGuard> {
    let mut result = Vec::new();
    for table in [ORIGINS, PROOFS] {
        for event in [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete] {
            result.push(WriteGuard {
                name: format!("immutable_{table}_{event:?}"),
                table: table.into(),
                event,
                reject_when: P::Equal(constant(1), constant(1)),
                violation: "workflow_repeat_event_immutable".into(),
            });
        }
    }
    result.push(WriteGuard {
        name: "repeat_event_origin_requires_new_run_v1".into(),
        table: ORIGINS.into(),
        event: GuardEvent::BeforeInsert,
        reject_when: P::Any(vec![
            P::Not(Box::new(P::Equal(
                V::NewColumn("version".into()),
                constant(1),
            ))),
            P::Exists(relation("workflow_runs_v2", "run", eq("run", "run_id"))),
            P::Exists(relation("workflow_runs", "legacy", eq("legacy", "run_id"))),
            P::NotExists(relation(super::ORIGINS, "public", eq("public", "run_id"))),
            P::Exists(relation(SOURCE, "source", eq("source", "run_id"))),
        ]),
        violation: "workflow_repeat_event_origin_invalid".into(),
    });
    result.push(WriteGuard {
        name: "repeat_event_proof_requires_fact_v1".into(),
        table: PROOFS.into(),
        event: GuardEvent::BeforeInsert,
        reject_when: P::Any(vec![
            P::NotExists(relation(ORIGINS, "origin", eq("origin", "run_id"))),
            P::NotExists(relation(
                super::EVENTS,
                "fact",
                P::All(vec![
                    eq("fact", "run_id"),
                    eq("fact", "identity"),
                    eq("fact", "recorded_at_ms"),
                    P::Equal(
                        V::Column {
                            relation: "fact".into(),
                            column: "payload".into(),
                        },
                        V::NewColumn("summary_json".into()),
                    ),
                    P::Equal(
                        V::Column {
                            relation: "fact".into(),
                            column: "event_type".into(),
                        },
                        V::Constant(S::Text(EVENT_TYPE.into())),
                    ),
                ]),
            )),
        ]),
        violation: "workflow_repeat_event_missing_fact".into(),
    });
    result.push(WriteGuard {
        name: "repeat_event_source_requires_proof_v1".into(),
        table: SOURCE.into(),
        event: GuardEvent::BeforeInsert,
        reject_when: P::All(vec![
            P::Exists(relation(ORIGINS, "origin", eq("origin", "run_id"))),
            P::Any(vec![
                P::Not(Box::new(P::Equal(
                    V::NewColumn("version".into()),
                    constant(1),
                ))),
                P::NotExists(relation(
                    PROOFS,
                    "proof",
                    P::All(vec![
                        eq("proof", "run_id"),
                        eq("proof", "output_id"),
                        eq("proof", "summary_json"),
                    ]),
                )),
            ]),
        ]),
        violation: "workflow_repeat_event_missing_proof".into(),
    });
    result.extend(immutable_source_guards());
    result
}

fn immutable_source_guards() -> Vec<WriteGuard> {
    let mut result = Vec::new();
    for event in [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete] {
        let mut origins = vec![P::Exists(relation(
            ORIGINS,
            "old_origin",
            P::Equal(
                V::Column {
                    relation: "old_origin".into(),
                    column: "run_id".into(),
                },
                V::OldColumn("run_id".into()),
            ),
        ))];
        if event == GuardEvent::BeforeUpdate {
            // Prevent moving an unmarked historical row into a covered run.
            origins.push(P::Exists(relation(
                ORIGINS,
                "new_origin",
                eq("new_origin", "run_id"),
            )));
        }
        result.push(WriteGuard {
            name: format!("repeat_event_source_immutable_v1_{event:?}"),
            table: SOURCE.into(),
            event,
            reject_when: P::Any(origins),
            violation: "workflow_repeat_event_source_immutable".into(),
        });
    }
    result
}

/// Install this optional source contract in the caller's coordinated installation transaction.
/// Does not opt in existing runs or change the shared execution contract.
/// # Errors
/// Rejects partial/incompatible schema, unavailable typed guards or database failures.
pub async fn install_in(tx: &dyn DatabaseTransaction) -> Result<(), Error> {
    super::installation::verify(tx).await?;
    if tx.get_table_info(ORIGINS).await?.is_some() {
        return verify(tx).await;
    }
    if tx.get_table_info(PROOFS).await?.is_some() || tx.get_table_info(SOURCE).await?.is_none() {
        return Err(Error::MaintenanceRequired);
    }
    for table in [ORIGINS, PROOFS] {
        let mut statement = create_table(table);
        for column in columns(table) {
            statement = statement.column(column);
        }
        statement
            .primary_key(if table == ORIGINS {
                "run_id"
            } else {
                "output_id"
            })
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
    for name in [ORIGINS, PROOFS] {
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
            let data_type = if expected.data_type == DataType::Int {
                DataType::BigInt
            } else {
                expected.data_type
            };
            if actual.data_type != data_type
                || actual.nullable
                || actual.auto_increment
                || actual.default_value.is_some()
                || actual.is_primary_key
                    != (expected.name
                        == if name == ORIGINS {
                            "run_id"
                        } else {
                            "output_id"
                        })
            {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    if !db.verify_write_guards(&guards()).await? {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}

/// Opt in only before run insertion, after public-event origin insertion, in admission's transaction.
/// # Errors
/// Rejects historical/duplicate origins, missing installation or incompatible guards.
pub async fn enable_in(tx: &dyn DatabaseTransaction, run: &str) -> Result<(), Error> {
    verify(tx).await?;
    if run.is_empty() || run.len() > 256 {
        return Err(Error::InvalidAdmission);
    }
    tx.insert(ORIGINS)
        .value("run_id", run)
        .value("version", 1_i64)
        .execute(tx)
        .await?;
    Ok(())
}

/// Verify coverage of repeat-emission rows only, never broader control or workflow semantics.
/// # Errors
/// Rejects incompatible schema/origins and missing source guards.
pub async fn coverage(db: &dyn Database, run: &str) -> Result<Coverage, Error> {
    if db.get_table_info(ORIGINS).await?.is_none() {
        if db.get_table_info(PROOFS).await?.is_some() {
            return Err(Error::MaintenanceRequired);
        }
        return Ok(Coverage::Unsupported);
    }
    verify(db).await?;
    let rows = db
        .select(ORIGINS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    match rows.first() {
        None => Ok(Coverage::HistoricalGap),
        Some(row) if row.get("version") == Some(DatabaseValue::Int64(1)) => {
            Ok(Coverage::CompleteFromAdmission)
        }
        Some(_) => Err(Error::MaintenanceRequired),
    }
}

/// Record the exact validated repeat summary before its source insertion, in the same fenced transaction.
///
/// Caller owns output/outcome validation and authority; this helper never dispatches or authorizes work.
/// Uses only `summary.created_at_ms`; conflicting duplicates, including changed time, fail closed.
/// # Errors
/// Rejects malformed summaries, conflicting identities, damaged coverage or database failures.
pub async fn record_in(
    tx: &dyn DatabaseTransaction,
    summary: &WorkflowRepeatOutcomeSummary,
) -> Result<(), Error> {
    if coverage(tx, &summary.run_id).await? != Coverage::CompleteFromAdmission {
        return Ok(());
    }
    let json = encode(summary)?;
    let identity = operation_identity(summary)?;
    super::append_encoded(
        tx,
        &summary.run_id,
        &identity,
        summary.created_at_ms,
        EVENT_TYPE,
        &json,
    )
    .await?;
    let rows = tx
        .select(PROOFS)
        .where_eq("output_id", summary.output_id.as_str())
        .limit(1)
        .execute(tx)
        .await?;
    if let Some(row) = rows.first() {
        if row.get("run_id") != Some(DatabaseValue::String(summary.run_id.clone()))
            || row.get("identity") != Some(DatabaseValue::String(identity))
            || row.get("summary_json") != Some(DatabaseValue::String(json))
            || row.get("recorded_at_ms")
                != Some(DatabaseValue::Int64(
                    i64::try_from(summary.created_at_ms).map_err(|_| Error::InvalidAdmission)?,
                ))
        {
            return Err(Error::MaintenanceRequired);
        }
        return Ok(());
    }
    tx.insert(PROOFS)
        .value("output_id", summary.output_id.as_str())
        .value("run_id", summary.run_id.as_str())
        .value("identity", identity)
        .value("summary_json", json)
        .value(
            "recorded_at_ms",
            i64::try_from(summary.created_at_ms).map_err(|_| Error::InvalidAdmission)?,
        )
        .execute(tx)
        .await?;
    Ok(())
}
fn operation_identity(summary: &WorkflowRepeatOutcomeSummary) -> Result<String, Error> {
    Ok(crate::sha256_hex(
        &serde_json::to_vec(&(EVENT_TYPE, &summary.run_id, &summary.output_id))
            .map_err(|_| Error::InvalidAdmission)?,
    ))
}
pub(super) fn validate_retained(
    summary: &WorkflowRepeatOutcomeSummary,
    run: &str,
    identity: &str,
    time: u64,
) -> Result<(), Error> {
    encode(summary).map_err(|_| Error::MaintenanceRequired)?;
    if summary.run_id != run
        || summary.created_at_ms != time
        || operation_identity(summary)? != identity
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}
fn encode(summary: &WorkflowRepeatOutcomeSummary) -> Result<String, Error> {
    for (value, bound) in [
        (&summary.run_id, 256),
        (&summary.node_id, 256),
        (&summary.activation_id, 512),
        (&summary.output_id, 512),
    ] {
        if value.is_empty() || value.len() > bound {
            return Err(Error::InvalidAdmission);
        }
    }
    if summary.checksum_sha256.len() != 64
        || !summary
            .checksum_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || summary.effective_iteration_bound != summary.max_iterations.min(summary.cycle_cap)
        || summary.iterations_completed > summary.effective_iteration_bound
    {
        return Err(Error::InvalidAdmission);
    }
    let json = serde_json::to_string(summary).map_err(|_| Error::InvalidAdmission)?;
    if json.len() > 8192 {
        return Err(Error::InvalidAdmission);
    }
    Ok(json)
}

/// Separate repeat-source coverage and a bounded pin scan of public events.
/// Existing page scope declarations remain unchanged; the extra coverage concerns repeat emissions only.
#[derive(Debug)]
pub struct Page {
    /// Coverage checked in the same read snapshot as the event pin.
    pub repeat_coverage: Coverage,
    /// Existing independently scoped public diagnostic page, including safely withheld new event kinds.
    pub events: super::Page,
}

/// Read repeat coverage and public events in one snapshot; never install or repair.
/// # Errors
/// Rejects invalid cursors, incompatible origins/guards or damaged events.
pub async fn page(
    db: &dyn Database,
    version: u32,
    run: &str,
    after: u64,
    through: Option<u64>,
    limit: u16,
) -> Result<Page, Error> {
    let tx = db.begin_transaction().await?;
    let repeat_coverage = coverage(&*tx, run).await?;
    let events = super::page_in(&*tx, version, run, after, through, limit).await?;
    tx.commit().await?;
    Ok(Page {
        repeat_coverage,
        events,
    })
}

#[cfg(test)]
mod tests;
