//! Guarded control-source commitments, separately admitted from earlier event families.
use super::{Database, DatabaseValue, Error};
use serde::{Deserialize, Serialize};
use switchy_database::{
    query::FilterableQuery,
    schema::write_guard::{
        GuardEvent, GuardPredicate as P, GuardRelation, GuardScalar as S, GuardValue as V,
        WriteGuard,
    },
    schema::{Column, DataType, create_table},
};
const ORIGINS: &str = "workflow_control_event_origins_v1";
const PROOFS: &str = "workflow_control_event_proofs_v1";

/// Exact committing operation, not an inferred execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    OutputCommitted,
    GateResolved,
    GateDenied,
    RepeatAdvanced,
    RepeatFailed,
    ChildReconciled,
}
/// Normalized result from the committing producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
    Continued,
}

fn columns(table: &str) -> Vec<Column> {
    let column = |name: &str, data_type| Column {
        name: name.into(),
        data_type,
        nullable: false,
        auto_increment: false,
        default: None,
    };
    if table == ORIGINS {
        vec![
            column("run_id", DataType::Text),
            column("version", DataType::Int),
        ]
    } else {
        vec![
            column("identity", DataType::Text),
            column("run_id", DataType::Text),
            column("source_table", DataType::Text),
            column("source_key", DataType::Text),
            column("source_count", DataType::Int),
        ]
    }
}
fn relation(table: &str, alias: &str, predicate: P) -> GuardRelation {
    GuardRelation {
        table: table.into(),
        alias: alias.into(),
        joins: vec![],
        predicate: Box::new(predicate),
    }
}
fn eq(alias: &str, target: &str, source: &str) -> P {
    P::Equal(
        V::Column {
            relation: alias.into(),
            column: target.into(),
        },
        V::NewColumn(source.into()),
    )
}
fn guards() -> Vec<WriteGuard> {
    let mut guards = Vec::new();
    for table in [ORIGINS, PROOFS] {
        for event in [GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete] {
            guards.push(WriteGuard {
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
                reject_when: P::Equal(V::Constant(S::Integer(1)), V::Constant(S::Integer(1))),
                violation: "workflow_control_event_immutable".into(),
            });
        }
    }
    guards.push(WriteGuard {
        name: "workflow_control_event_origin_new_run".into(),
        table: ORIGINS.into(),
        event: GuardEvent::BeforeInsert,
        reject_when: P::Exists(relation(
            "workflow_runs_v2",
            "admitted",
            eq("admitted", "run_id", "run_id"),
        )),
        violation: "workflow_control_event_origin_requires_new_run".into(),
    });
    guards.extend(source_guards());
    guards
}

fn source_guards() -> Vec<WriteGuard> {
    let mut guards = Vec::new();
    for (table, key, count, events) in [
        (
            "workflow_outputs_v2",
            "output_id",
            false,
            vec![GuardEvent::BeforeInsert],
        ),
        (
            "workflow_control_failures_v2",
            "identity",
            true,
            vec![GuardEvent::BeforeInsert],
        ),
        (
            "workflow_repeat_cycles_v2",
            "identity",
            true,
            vec![GuardEvent::BeforeInsert, GuardEvent::BeforeUpdate],
        ),
    ] {
        for event in events {
            let mut matches = vec![
                eq("proof", "run_id", "run_id"),
                eq("proof", "source_key", key),
                P::Equal(
                    V::Column {
                        relation: "proof".into(),
                        column: "source_table".into(),
                    },
                    V::Constant(S::Text(table.into())),
                ),
            ];
            if count {
                matches.push(eq("proof", "source_count", "value"));
            }
            guards.push(WriteGuard {
                name: format!(
                    "control_event_required_{table}_{}",
                    if event == GuardEvent::BeforeInsert {
                        "insert"
                    } else {
                        "update"
                    }
                ),
                table: table.into(),
                event,
                reject_when: P::All(vec![
                    P::Exists(relation(
                        ORIGINS,
                        "origin",
                        eq("origin", "run_id", "run_id"),
                    )),
                    P::NotExists(relation(PROOFS, "proof", P::All(matches))),
                ]),
                violation: "workflow_control_event_missing".into(),
            });
        }
    }
    guards.push(WriteGuard {
        name: "control_proof_requires_public_event".into(),
        table: PROOFS.into(),
        event: GuardEvent::BeforeInsert,
        reject_when: P::NotExists(relation(
            super::EVENTS,
            "event",
            P::All(vec![
                eq("event", "run_id", "run_id"),
                eq("event", "identity", "identity"),
            ]),
        )),
        violation: "workflow_control_proof_missing_event".into(),
    });
    guards.extend(immutable_source_guards());
    guards
}

fn immutable_source_guards() -> Vec<WriteGuard> {
    let mut guards = Vec::new();
    for table in [
        "workflow_outputs_v2",
        "workflow_control_failures_v2",
        "workflow_repeat_cycles_v2",
    ] {
        let events = if table == "workflow_repeat_cycles_v2" {
            vec![GuardEvent::BeforeDelete]
        } else {
            vec![GuardEvent::BeforeUpdate, GuardEvent::BeforeDelete]
        };
        for event in events {
            guards.push(WriteGuard {
                name: format!(
                    "immutable_control_source_{table}_{}",
                    if event == GuardEvent::BeforeUpdate {
                        "update"
                    } else {
                        "delete"
                    }
                ),
                table: table.into(),
                event,
                reject_when: P::Exists(relation(
                    ORIGINS,
                    "origin",
                    P::Equal(
                        V::Column {
                            relation: "origin".into(),
                            column: "run_id".into(),
                        },
                        V::OldColumn("run_id".into()),
                    ),
                )),
                violation: "workflow_control_source_immutable".into(),
            });
        }
    }
    guards.push(WriteGuard {
        name: "workflow_control_cycle_no_regression_v1".into(),
        table: "workflow_repeat_cycles_v2".into(),
        event: GuardEvent::BeforeUpdate,
        reject_when: P::All(vec![
            P::Exists(relation(
                ORIGINS,
                "origin",
                eq("origin", "run_id", "run_id"),
            )),
            P::Not(Box::new(P::Any(vec![
                P::Equal(V::NewColumn("value".into()), V::OldColumn("value".into())),
                P::IntegerSuccessor {
                    next: V::NewColumn("value".into()),
                    previous: V::OldColumn("value".into()),
                },
            ]))),
        ]),
        violation: "workflow_control_cycle_regression".into(),
    });
    guards
}
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    if db.get_table_info(ORIGINS).await?.is_some() {
        return verify(db).await;
    }
    if db.get_table_info(PROOFS).await?.is_some() {
        return Err(Error::MaintenanceRequired);
    }
    for table in [ORIGINS, PROOFS] {
        let mut schema = create_table(table);
        for column in columns(table) {
            schema = schema.column(column);
        }
        schema
            .primary_key(if table == ORIGINS {
                "run_id"
            } else {
                "identity"
            })
            .execute(db)
            .await?;
    }
    for guard in guards() {
        db.install_write_guard(&guard).await?;
    }
    verify(db).await
}
async fn verify(db: &dyn Database) -> Result<(), Error> {
    for table in [ORIGINS, PROOFS] {
        let actual = db
            .get_table_info(table)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        if actual.columns.len() != columns(table).len() {
            return Err(Error::MaintenanceRequired);
        }
        for column in columns(table) {
            let stored = actual
                .columns
                .get(&column.name)
                .ok_or(Error::MaintenanceRequired)?;
            let expected = if column.data_type == DataType::Int {
                DataType::BigInt
            } else {
                column.data_type
            };
            if stored.data_type != expected
                || stored.nullable
                || stored.auto_increment
                || stored.default_value.is_some()
                || stored.is_primary_key
                    != (column.name
                        == if table == ORIGINS {
                            "run_id"
                        } else {
                            "identity"
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
pub(super) async fn enable(db: &dyn Database, run: &str) -> Result<(), Error> {
    install(db).await?;
    db.insert(ORIGINS)
        .value("run_id", run)
        .value("version", 1_i64)
        .execute(db)
        .await?;
    Ok(())
}
/// Verify complete coverage only for guarded control source rows, never all semantics.
/// # Errors
/// Rejects incompatible origins, source guards or damaged proof schema.
pub async fn coverage(
    db: &dyn Database,
    run: &str,
) -> Result<bcode_workflow::WorkflowSemanticHistoryCoverage, Error> {
    use bcode_workflow::WorkflowSemanticHistoryCoverage as Coverage;
    if db.get_table_info(ORIGINS).await?.is_none() {
        return Ok(Coverage::Unsupported);
    }
    verify(db).await?;
    let rows = db
        .select(ORIGINS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    if let Some(row) = rows.first() {
        if row.get("version") != Some(DatabaseValue::Int64(1)) {
            return Err(Error::MaintenanceRequired);
        }
        Ok(Coverage::CompleteFromAdmission)
    } else {
        Ok(Coverage::HistoricalGap)
    }
}
/// Exact caller-owned source transaction inputs for a control commitment.
pub struct Commitment<'a> {
    pub run: &'a str,
    pub node: &'a str,
    pub activation: &'a str,
    pub operation: Operation,
    pub outcome: Outcome,
    pub time: u64,
    pub source: Option<(&'a str, &'a str, i64)>,
}

/// Append a typed control fact in the caller's actual source transaction.
///
/// # Errors
/// Rejects malformed identities, incompatible coverage, conflicting duplicates or DB errors.
pub async fn record(db: &dyn Database, commitment: Commitment<'_>) -> Result<(), Error> {
    let Commitment {
        run,
        node,
        activation,
        operation,
        outcome,
        time,
        source,
    } = commitment;
    if coverage(db, run).await?
        != bcode_workflow::WorkflowSemanticHistoryCoverage::CompleteFromAdmission
    {
        return Ok(());
    }
    if node.len() > 256 || activation.len() > 512 {
        return Err(Error::InvalidAdmission);
    }
    let fact = super::Fact::Control {
        node_id: node.into(),
        activation_id: activation.into(),
        operation,
        outcome,
    };
    let identity = crate::sha256_hex(
        serde_json::to_vec(&(run, node, activation, operation, outcome, source))
            .map_err(|_| Error::InvalidAdmission)?
            .as_slice(),
    );
    let existing = db
        .select(super::EVENTS)
        .where_eq("identity", identity.as_str())
        .limit(1)
        .execute(db)
        .await?;
    let recorded_time = existing
        .first()
        .map(|row| super::super::integer(row, "recorded_at_ms"))
        .transpose()?
        .unwrap_or(time);
    super::append_in(db, run, &identity, recorded_time, fact).await?;
    if let Some((table, key, count)) = source {
        if key.len() > 512
            || ![
                "workflow_outputs_v2",
                "workflow_control_failures_v2",
                "workflow_repeat_cycles_v2",
            ]
            .contains(&table)
        {
            return Err(Error::InvalidAdmission);
        }
        if db
            .select(PROOFS)
            .where_eq("identity", identity.as_str())
            .limit(1)
            .execute(db)
            .await?
            .is_empty()
        {
            db.insert(PROOFS)
                .value("identity", identity)
                .value("run_id", run)
                .value("source_table", table)
                .value("source_key", key)
                .value("source_count", count)
                .execute(db)
                .await?;
        }
    }
    Ok(())
}
