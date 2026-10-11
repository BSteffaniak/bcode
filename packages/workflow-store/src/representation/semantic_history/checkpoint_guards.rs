//! Persistent checkpoint constraints; no checkpoint can clear a gap or rewrite proof.
use super::{ORIGINS, SOURCE, checkpoints::TABLE};
use switchy_database::schema::write_guard::{
    GuardEvent, GuardPredicate as P, GuardRelation, GuardScalar as S, GuardValue as V,
    ObservationColumn, ObservationEvent, WriteGuard, WriteObservation,
};
fn new(key: &str) -> V {
    V::NewColumn(key.into())
}
fn column(alias: &str, key: &str) -> V {
    V::Column {
        relation: alias.into(),
        column: key.into(),
    }
}
const fn constant(value: i64) -> V {
    V::Constant(S::Integer(value))
}
fn exists(table: &str, alias: &str, predicate: P) -> P {
    P::Exists(GuardRelation {
        table: table.into(),
        alias: alias.into(),
        joins: vec![],
        predicate: Box::new(predicate),
    })
}
fn absent(table: &str, alias: &str, predicate: P) -> P {
    P::NotExists(GuardRelation {
        table: table.into(),
        alias: alias.into(),
        joins: vec![],
        predicate: Box::new(predicate),
    })
}
fn eq(alias: &str, key: &str) -> P {
    P::Equal(column(alias, key), new(key))
}
pub(super) fn origin_condition() -> P {
    absent(
        SOURCE,
        "attempt",
        P::Equal(column("attempt", "run_id"), new("run_id")),
    )
}
pub(super) fn observer() -> WriteObservation {
    WriteObservation {
        name: "attempt_history_checkpoint_origin_v2".into(),
        table: ORIGINS.into(),
        event: ObservationEvent::AfterInsert,
        when: origin_condition(),
        target_table: TABLE.into(),
        columns: vec![
            ObservationColumn {
                column: "run_id".into(),
                value: new("run_id"),
            },
            ObservationColumn {
                column: "event_identity".into(),
                value: V::Constant(S::Text(String::new())),
            },
        ]
        .into_iter()
        .chain(
            ["sequence", "last_verified", "gap", "obligation"]
                .into_iter()
                .map(|key| ObservationColumn {
                    column: key.into(),
                    value: constant(0),
                }),
        )
        .collect(),
    }
}
pub(super) fn guard() -> WriteGuard {
    proof_guard(
        super::checkpoints::ATTEMPTS,
        "attempt_history_checkpoint_proof_v2",
        "workflow_attempt_history_checkpoint_invalid",
        Some(origin_condition()),
    )
}
pub(super) fn proof_guard(
    tables: super::checkpoints::Tables,
    name: &str,
    violation: &str,
    initial_condition: Option<P>,
) -> WriteGuard {
    let has_initial = initial_condition.is_some();
    let origin = exists(tables.origins, "origin", eq("origin", "run_id"));
    let initial = P::All(vec![
        origin.clone(),
        initial_condition.unwrap_or(P::Equal(constant(0), constant(1))),
        P::Equal(new("sequence"), constant(0)),
        P::Equal(new("gap"), constant(0)),
        P::Equal(new("last_verified"), constant(0)),
        P::Equal(new("obligation"), constant(0)),
        P::Equal(new("event_identity"), V::Constant(S::Text(String::new()))),
    ]);
    let previous = exists(
        tables.checkpoints,
        "previous",
        P::All(vec![
            eq("previous", "run_id"),
            P::IntegerSuccessor {
                previous: column("previous", "sequence"),
                next: new("sequence"),
            },
            P::Any(vec![
                P::All(vec![
                    P::Equal(new("gap"), constant(1)),
                    P::Equal(new("last_verified"), column("previous", "last_verified")),
                ]),
                P::All(vec![
                    P::Equal(new("gap"), constant(0)),
                    P::Equal(column("previous", "gap"), constant(0)),
                    P::Equal(new("last_verified"), new("obligation")),
                    origin,
                ]),
            ]),
        ]),
    );
    let older_first = P::All(vec![
        P::Equal(new("sequence"), constant(1)),
        P::Equal(new("gap"), constant(1)),
        P::Equal(new("last_verified"), constant(0)),
        absent(tables.checkpoints, "previous", eq("previous", "run_id")),
        if has_initial {
            absent(tables.origins, "origin", eq("origin", "run_id"))
        } else {
            P::Equal(constant(1), constant(1))
        },
    ]);
    let first_admission = if has_initial {
        P::Equal(constant(0), constant(1))
    } else {
        P::All(vec![
            P::Equal(new("sequence"), constant(1)),
            P::Equal(new("gap"), constant(0)),
            P::Equal(new("last_verified"), new("obligation")),
            absent(tables.checkpoints, "previous", eq("previous", "run_id")),
            exists(tables.origins, "origin", eq("origin", "run_id")),
        ])
    };
    let proof = P::All(vec![
        exists(
            tables.events,
            "event",
            P::All(vec![
                eq("event", "run_id"),
                eq("event", "sequence"),
                P::Equal(column("event", "identity"), new("event_identity")),
            ]),
        ),
        exists(
            tables.acknowledgements,
            "ack",
            P::All(vec![eq("ack", "obligation"), eq("ack", "event_identity")]),
        ),
        exists(
            tables.obligations,
            "source",
            P::All(vec![
                P::Equal(column("source", "sequence"), new("obligation")),
                P::Equal(column("source", "new_run_id"), new("run_id")),
            ]),
        ),
        P::Any(vec![previous, older_first, first_admission]),
    ]);
    WriteGuard {
        name: name.into(),
        table: tables.checkpoints.into(),
        event: GuardEvent::BeforeInsert,
        reject_when: P::Not(Box::new(P::Any(vec![initial, proof]))),
        violation: violation.into(),
    }
}
