//! Finite scalar observer definitions. No private JSON is selected.
use super::{
    COVERAGE, EVENTS, Error, FactUnavailableReason, ScalarFact, SourceCoverage,
    TransitionObservation,
};
use switchy_database::{
    DatabaseValue, Row,
    schema::write_guard::{
        GuardPredicate, GuardRelation, GuardScalar, GuardValue, ObservationColumn,
        ObservationEvent, WriteObservation,
    },
};

pub(super) const FACTS: &[&str] = &[
    "identity",
    "node_id",
    "activation_id",
    "dispatch_identity",
    "status",
    "revision",
    "generation",
    "count",
    "current",
    "skipped",
    "active_attempts",
    "timestamp_ms",
];

pub(super) fn is_text(name: &str) -> bool {
    [
        "identity",
        "node_id",
        "activation_id",
        "dispatch_identity",
        "status",
    ]
    .contains(&name)
}

struct Source {
    table: &'static str,
    columns: &'static [(&'static str, &'static str)],
    timestamp: Option<&'static str>,
}
const SOURCES: &[Source] = &[
    Source {
        table: "workflow_runs_v2",
        columns: &[
            ("status", "status"),
            ("count", "consumed"),
            ("active_attempts", "active_attempts"),
        ],
        timestamp: Some("updated_at_ms"),
    },
    Source {
        table: "workflow_graphs_v2",
        columns: &[("revision", "revision")],
        timestamp: None,
    },
    Source {
        table: "workflow_activations_v2",
        columns: &[
            ("identity", "identity"),
            ("node_id", "node_id"),
            ("revision", "revision"),
            ("generation", "generation"),
            ("current", "current"),
            ("skipped", "skipped"),
        ],
        timestamp: Some("created_at_ms"),
    },
    Source {
        table: "workflow_attempts_v2",
        columns: &[
            ("dispatch_identity", "dispatch_identity"),
            ("node_id", "node_id"),
            ("activation_id", "activation_id"),
            ("count", "attempt_number"),
        ],
        timestamp: Some("terminal_at_ms"),
    },
    Source {
        table: "workflow_outputs_v2",
        columns: &[
            ("identity", "output_id"),
            ("activation_id", "activation_id"),
        ],
        timestamp: None,
    },
    Source {
        table: "workflow_repeat_cycles_v2",
        columns: &[("identity", "identity"), ("count", "value")],
        timestamp: None,
    },
    Source {
        table: "workflow_control_failures_v2",
        columns: &[("identity", "identity"), ("count", "value")],
        timestamp: None,
    },
    Source {
        table: "workflow_graph_revisions_v2",
        columns: &[("identity", "identity"), ("revision", "revision")],
        timestamp: None,
    },
    Source {
        table: "workflow_user_pause_v2",
        columns: &[],
        timestamp: None,
    },
];

pub(super) fn coverage() -> Vec<SourceCoverage> {
    SOURCES
        .iter()
        .map(|source| {
            let mut columns = vec!["run_id".into()];
            columns.extend(source.columns.iter().map(|(_, name)| (*name).into()));
            if let Some(timestamp) = source.timestamp {
                columns.push(timestamp.into());
            }
            SourceCoverage {
                table: source.table.into(),
                columns,
                operations: vec!["insert".into(), "update".into(), "delete".into()],
            }
        })
        .collect()
}

fn integer_columns(target: &str, value: GuardValue) -> [ObservationColumn; 2] {
    [
        ObservationColumn {
            column: target.into(),
            value: GuardValue::IntegerOnly {
                value: Box::new(value.clone()),
            },
        },
        ObservationColumn {
            column: format!("{target}_status"),
            value: GuardValue::IntegerStatus {
                value: Box::new(value),
            },
        },
    ]
}

fn integer_fact(row: &Row, target: &str) -> Result<ScalarFact, Error> {
    match (
        super::integer(row, &format!("{target}_status"))?,
        row.get(target),
    ) {
        (0, Some(DatabaseValue::Null)) => Ok(ScalarFact::Null),
        (1, Some(DatabaseValue::Int64(value))) => Ok(ScalarFact::Integer(value)),
        (2, Some(DatabaseValue::Null)) => Ok(ScalarFact::Unavailable(
            FactUnavailableReason::UnsupportedType,
        )),
        _ => Err(Error::MaintenanceRequired),
    }
}

pub(super) fn definitions() -> Vec<WriteObservation> {
    SOURCES
        .iter()
        .flat_map(|source| {
            [
                ObservationEvent::AfterInsert,
                ObservationEvent::AfterUpdate,
                ObservationEvent::AfterDelete,
            ]
            .map(|event| {
                let operation = match event {
                    ObservationEvent::AfterInsert => "insert",
                    ObservationEvent::AfterUpdate => "update",
                    ObservationEvent::AfterDelete => "delete",
                };
                let row = |name: &str| {
                    if event == ObservationEvent::AfterDelete {
                        GuardValue::OldColumn(name.into())
                    } else {
                        GuardValue::NewColumn(name.into())
                    }
                };
                let constant = |value: &str| GuardValue::Constant(GuardScalar::Text(value.into()));
                let mut columns = vec![
                    ObservationColumn {
                        column: "run_id".into(),
                        value: row("run_id"),
                    },
                    ObservationColumn {
                        column: "source_table".into(),
                        value: constant(source.table),
                    },
                    ObservationColumn {
                        column: "operation".into(),
                        value: constant(operation),
                    },
                ];
                for (target, name) in source.columns {
                    let value = row(name);
                    if is_text(target) {
                        columns.push(ObservationColumn {
                            column: (*target).into(),
                            value: GuardValue::BoundedText {
                                value: Box::new(value.clone()),
                                max_bytes: 512,
                            },
                        });
                        columns.push(ObservationColumn {
                            column: format!("{target}_status"),
                            value: GuardValue::BoundedTextStatus {
                                value: Box::new(value),
                                max_bytes: 512,
                            },
                        });
                    } else {
                        columns.extend(integer_columns(target, value));
                    }
                }
                // Insert time may be actual; delete time is not insertion/last-update time. Updates
                // retain a row timestamp as a fact only: decoding only calls changed times transition times.
                if event != ObservationEvent::AfterDelete
                    && let Some(timestamp) = source.timestamp
                {
                    columns.extend(integer_columns("timestamp_ms", row(timestamp)));
                    columns.push(ObservationColumn {
                        column: "timestamp_source".into(),
                        value: constant(timestamp),
                    });
                    if event == ObservationEvent::AfterUpdate {
                        columns.extend(integer_columns(
                            "previous_timestamp_ms",
                            GuardValue::OldColumn(timestamp.into()),
                        ));
                    }
                }
                WriteObservation {
                    name: format!("observe_v1_{}_{operation}", source.table),
                    table: source.table.into(),
                    event,
                    when: GuardPredicate::Exists(GuardRelation {
                        table: COVERAGE.into(),
                        alias: "opted_in".into(),
                        joins: vec![],
                        predicate: Box::new(GuardPredicate::Equal(
                            GuardValue::Column {
                                relation: "opted_in".into(),
                                column: "run_id".into(),
                            },
                            row("run_id"),
                        )),
                    }),
                    target_table: EVENTS.into(),
                    columns,
                }
            })
        })
        .collect()
}

pub(super) fn decode(row: &Row) -> Result<TransitionObservation, Error> {
    let source_table = super::text(row, "source_table")?;
    let source = SOURCES
        .iter()
        .find(|source| source.table == source_table)
        .ok_or(Error::MaintenanceRequired)?;
    let operation = super::text(row, "operation")?;
    if !["insert", "update", "delete"].contains(&operation.as_str()) {
        return Err(Error::MaintenanceRequired);
    }
    let mut facts = std::collections::BTreeMap::new();
    for (target, name) in source.columns {
        let value = if is_text(target) {
            match (
                super::integer(row, &format!("{target}_status"))?,
                row.get(target),
            ) {
                (0, Some(DatabaseValue::Null)) => ScalarFact::Null,
                (1, Some(DatabaseValue::String(value))) if value.len() <= 512 => {
                    ScalarFact::Text(value)
                }
                (2, Some(DatabaseValue::Null)) => {
                    ScalarFact::Unavailable(FactUnavailableReason::Oversized)
                }
                (3, Some(DatabaseValue::Null)) => {
                    ScalarFact::Unavailable(FactUnavailableReason::UnsupportedType)
                }
                _ => return Err(Error::MaintenanceRequired),
            }
        } else {
            integer_fact(row, target)?
        };
        facts.insert((*name).into(), value);
    }
    let current_fact = if source.timestamp.is_some() && operation != "delete" {
        Some(integer_fact(row, "timestamp_ms")?)
    } else {
        None
    };
    let previous_fact = if source.timestamp.is_some() && operation == "update" {
        Some(integer_fact(row, "previous_timestamp_ms")?)
    } else {
        None
    };
    if let Some(name) = source.timestamp {
        if let Some(value) = &current_fact {
            facts.insert(name.into(), value.clone());
        }
        if matches!(previous_fact, Some(ScalarFact::Unavailable(_))) {
            facts.insert(
                format!("previous_{name}"),
                ScalarFact::Unavailable(FactUnavailableReason::UnsupportedType),
            );
        }
    }
    let timestamp_value = |fact: &Option<ScalarFact>| match fact {
        Some(ScalarFact::Integer(value)) => u64::try_from(*value).ok(),
        _ => None,
    };
    let current = timestamp_value(&current_fact);
    let previous = timestamp_value(&previous_fact);
    let invalid_timestamp = matches!(current_fact, Some(ScalarFact::Unavailable(_)))
        || matches!(previous_fact, Some(ScalarFact::Unavailable(_)))
        || matches!(current_fact, Some(ScalarFact::Integer(value)) if value < 0)
        || matches!(previous_fact, Some(ScalarFact::Integer(value)) if value < 0);
    let timestamp_ms = if invalid_timestamp
        || (source.timestamp == Some("created_at_ms") && operation != "insert")
    {
        None
    } else if operation == "insert" || (operation == "update" && current != previous) {
        current
    } else {
        None
    };
    let timestamp_source = if timestamp_ms.is_some() {
        Some(super::text(row, "timestamp_source")?)
    } else {
        None
    };
    let sequence = super::integer(row, "sequence")?;
    if sequence == 0 {
        return Err(Error::MaintenanceRequired);
    }
    if let Some(ref name) = timestamp_source
        && source.timestamp != Some(name.as_str())
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(TransitionObservation {
        sequence,
        run_id: super::text(row, "run_id")?,
        source_table,
        operation,
        facts,
        timestamp_ms,
        timestamp_source,
    })
}
