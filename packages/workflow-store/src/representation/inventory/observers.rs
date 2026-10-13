//! Finite scalar membership observations, including both sides of key updates.
use super::{EVENTS, RepresentationError as Error};
use switchy_database::schema::write_guard::{
    GuardPredicate, GuardScalar, GuardValue, ObservationColumn, ObservationEvent,
    ObservationLookupKey, UniqueScalarLookup, WriteObservation,
};
pub(super) const DIRECT: &[(&str, &[&str])] = &[
    ("workflow_runs_v2", &["run_id"]),
    ("workflow_graphs_v2", &["run_id"]),
    ("workflow_activations_v2", &["run_id"]),
    ("workflow_attempts_v2", &["run_id"]),
    ("workflow_outputs_v2", &["run_id"]),
    ("workflow_graph_revisions_v2", &["run_id"]),
    ("workflow_lineage_v2", &["run_id", "parent_run_id"]),
];
pub(super) fn definitions() -> Vec<WriteObservation> {
    let mut result = Vec::new();
    for (table, keys) in DIRECT {
        for key in *keys {
            append(&mut result, table, key, false);
        }
    }
    append(
        &mut result,
        "workflow_execution_sessions_v2",
        "dispatch_identity",
        true,
    );
    // No extraction of run identity from request/link JSON or serialized candidate keys.
    append(
        &mut result,
        "workflow_execution_graph_candidates_v2",
        "unmapped",
        false,
    );
    result
}
fn append(result: &mut Vec<WriteObservation>, table: &str, key: &str, lookup: bool) {
    for (event, name, old) in [
        (ObservationEvent::AfterInsert, "insert", false),
        (ObservationEvent::AfterUpdate, "update_old", true),
        (ObservationEvent::AfterUpdate, "update_new", false),
        (ObservationEvent::AfterDelete, "delete", true),
    ] {
        let row = |column: &str| {
            if old {
                GuardValue::OldColumn(column.into())
            } else {
                GuardValue::NewColumn(column.into())
            }
        };
        let constant = |n| GuardValue::Constant(GuardScalar::Integer(n));
        let (value, status) = if lookup {
            let selector = UniqueScalarLookup {
                table: "workflow_attempts_v2".into(),
                key_column: "dispatch_identity".into(),
                key: if old {
                    ObservationLookupKey::OldColumn(key.into())
                } else {
                    ObservationLookupKey::NewColumn(key.into())
                },
                value_column: "run_id".into(),
                max_bytes: 512,
            };
            (
                GuardValue::UniqueScalarLookup(selector.clone()),
                GuardValue::UniqueScalarLookupStatus(selector),
            )
        } else if key == "unmapped" {
            (GuardValue::Constant(GuardScalar::Null), constant(0))
        } else {
            (
                GuardValue::BoundedText {
                    value: Box::new(row(key)),
                    max_bytes: 512,
                },
                GuardValue::BoundedTextStatus {
                    value: Box::new(row(key)),
                    max_bytes: 512,
                },
            )
        };
        result.push(WriteObservation {
            name: format!("inventory_{table}_{key}_{name}"),
            table: table.into(),
            event,
            when: if key == "parent_run_id" {
                GuardPredicate::IsNotNull(row(key))
            } else {
                GuardPredicate::Equal(constant(1), constant(1))
            },
            target_table: EVENTS.into(),
            columns: vec![
                ObservationColumn {
                    column: "source_table".into(),
                    value: GuardValue::Constant(GuardScalar::Text(table.into())),
                },
                ObservationColumn {
                    column: "run_id".into(),
                    value,
                },
                ObservationColumn {
                    column: "attribution_status".into(),
                    value: status,
                },
            ],
        });
    }
}
pub(super) async fn verify_sources(db: &dyn switchy_database::Database) -> Result<(), Error> {
    for (table, keys) in DIRECT {
        let info = db
            .get_table_info(table)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        for key in *keys {
            let column = info.columns.get(*key).ok_or(Error::MaintenanceRequired)?;
            if column.data_type != switchy_database::schema::DataType::Text
                || column.nullable != (*key == "parent_run_id")
            {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    Ok(())
}
