//! Guards enforced for old and new writers by the canonical database.
use switchy_database::schema::write_guard::{
    GuardEvent, GuardJoin, GuardPredicate as P, GuardRelation, GuardScalar as S, GuardValue as V,
    WriteGuard,
};

fn new(column: &str) -> V {
    V::NewColumn(column.into())
}
fn col(column: &str) -> V {
    V::Column {
        relation: "other".into(),
        column: column.into(),
    }
}
fn exists(table: &str, predicate: P) -> P {
    P::Exists(GuardRelation {
        table: table.into(),
        alias: "other".into(),
        joins: vec![],
        predicate: Box::new(predicate),
    })
}
fn active(value: V) -> P {
    P::InValues(
        value,
        ["running", "paused", "repair_required"]
            .into_iter()
            .map(|s| S::Text(s.into()))
            .collect(),
    )
}
fn binding(other: &str) -> P {
    P::All(vec![
        active(new("status")),
        exists(
            other,
            P::All(vec![
                active(col("status")),
                P::Equal(new("owner_plugin_id"), col("owner_plugin_id")),
                P::Equal(new("workflow_kind"), col("workflow_kind")),
                P::Equal(new("scope_key"), col("scope_key")),
                P::Equal(new("single_active"), V::Constant(S::Bool(true))),
            ]),
        ),
    ])
}

fn reserved_binding() -> P {
    let predecessor = |column: &str| V::Column {
        relation: "predecessor".into(),
        column: column.into(),
    };
    P::All(vec![
        P::Equal(new("single_active"), V::Constant(S::Bool(true))),
        P::Exists(GuardRelation {
            table: "workflow_replacement_intents".into(),
            alias: "other".into(),
            joins: vec![GuardJoin {
                table: "workflow_runs".into(),
                alias: "predecessor".into(),
                left: col("old_run_id"),
                right: predecessor("run_id"),
            }],
            predicate: Box::new(P::All(vec![
                P::Equal(predecessor("single_active"), V::Constant(S::Bool(true))),
                P::Equal(new("owner_plugin_id"), predecessor("owner_plugin_id")),
                P::Equal(new("workflow_kind"), predecessor("workflow_kind")),
                P::Equal(new("scope_key"), predecessor("scope_key")),
            ])),
        }),
    ])
}

fn replacement_binding() -> P {
    let predecessor = |column: &str| V::Column {
        relation: "predecessor".into(),
        column: column.into(),
    };
    P::Exists(GuardRelation {
        table: super::RUNS.into(),
        alias: "other".into(),
        joins: vec![GuardJoin {
            table: "workflow_runs".into(),
            alias: "predecessor".into(),
            left: col("owner_plugin_id"),
            right: predecessor("owner_plugin_id"),
        }],
        predicate: Box::new(P::All(vec![
            P::Equal(new("old_run_id"), predecessor("run_id")),
            P::Equal(predecessor("single_active"), V::Constant(S::Bool(true))),
            active(col("status")),
            P::Equal(col("owner_plugin_id"), predecessor("owner_plugin_id")),
            P::Equal(col("workflow_kind"), predecessor("workflow_kind")),
            P::Equal(col("scope_key"), predecessor("scope_key")),
        ])),
    })
}

/// Immutable expected contracts, not cached evidence about any database.
/// Every verifier still compares these contracts against live transactional schema state.
pub fn all() -> &'static [WriteGuard] {
    static EXPECTED: std::sync::LazyLock<Vec<WriteGuard>> = std::sync::LazyLock::new(build);
    &EXPECTED
}

fn build() -> Vec<WriteGuard> {
    let mut guards = Vec::new();
    for (table, other) in [
        ("workflow_runs", super::RUNS),
        (super::RUNS, "workflow_runs"),
    ] {
        for (suffix, event) in [
            ("insert", GuardEvent::BeforeInsert),
            ("update", GuardEvent::BeforeUpdate),
        ] {
            guards.push(WriteGuard {
                name: format!("bcode_v2_{table}_{suffix}"),
                table: table.into(),
                event,
                reject_when: P::Any(vec![
                    exists(other, P::Equal(new("run_id"), col("run_id"))),
                    binding(other),
                ]),
                violation: "workflow_representation_identity_or_binding_conflict".into(),
            });
        }
    }
    for (suffix, event) in [
        ("insert", GuardEvent::BeforeInsert),
        ("update", GuardEvent::BeforeUpdate),
    ] {
        guards.push(WriteGuard {
            name: format!("bcode_v2_internal_binding_{suffix}"),
            table: super::RUNS.into(),
            event,
            reject_when: P::All(vec![
                P::Equal(new("single_active"), V::Constant(S::Bool(true))),
                active(new("status")),
                exists(
                    super::RUNS,
                    P::All(vec![
                        P::NotEqual(new("run_id"), col("run_id")),
                        active(col("status")),
                        P::Equal(new("owner_plugin_id"), col("owner_plugin_id")),
                        P::Equal(new("workflow_kind"), col("workflow_kind")),
                        P::Equal(new("scope_key"), col("scope_key")),
                    ]),
                ),
            ]),
            violation: "workflow_representation_binding_conflict".into(),
        });
    }
    // Every legacy replacement reservation must exclude an existing v2 identity.
    for (suffix, event) in [
        ("insert", GuardEvent::BeforeInsert),
        ("update", GuardEvent::BeforeUpdate),
    ] {
        guards.push(WriteGuard {
            name: format!("bcode_v2_replacement_{suffix}"),
            table: "workflow_replacement_intents".into(),
            event,
            reject_when: P::Any(vec![
                exists(
                    super::RUNS,
                    P::Equal(new("successor_run_id"), col("run_id")),
                ),
                replacement_binding(),
            ]),
            violation: "workflow_representation_successor_conflict".into(),
        });
        guards.push(WriteGuard {
            name: format!("bcode_v2_reservation_{suffix}"),
            table: super::RUNS.into(),
            event,
            reject_when: P::Any(vec![
                exists(
                    "workflow_replacement_intents",
                    P::Equal(new("run_id"), col("successor_run_id")),
                ),
                reserved_binding(),
            ]),
            violation: "workflow_representation_identity_reserved".into(),
        });
    }
    guards.push(WriteGuard {
        name: "bcode_v2_predecessor_binding_update".into(),
        table: "workflow_runs".into(),
        event: GuardEvent::BeforeUpdate,
        reject_when: P::All(vec![
            P::Equal(new("single_active"), V::Constant(S::Bool(true))),
            exists(
                "workflow_replacement_intents",
                P::Equal(new("run_id"), col("old_run_id")),
            ),
            exists(
                super::RUNS,
                P::All(vec![
                    active(col("status")),
                    P::Equal(new("owner_plugin_id"), col("owner_plugin_id")),
                    P::Equal(new("workflow_kind"), col("workflow_kind")),
                    P::Equal(new("scope_key"), col("scope_key")),
                ]),
            ),
        ]),
        violation: "workflow_representation_reserved_binding_conflict".into(),
    });
    guards
}
