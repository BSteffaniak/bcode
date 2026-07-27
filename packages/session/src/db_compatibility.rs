//! Current session-compatibility projection mutation.

use crate::db::SessionDbResult;
use crate::db_event_store::seq_to_value;
use bcode_session_models::{
    SessionEvent, SessionEventCompatibilityIssue, SessionEventCompatibilityKind,
};
use switchy::database::{Database, DatabaseValue};

pub async fn clear_session_compatibility_issues(db: &dyn Database) -> SessionDbResult<()> {
    db.delete("session_compatibility_issues")
        .execute(db)
        .await?;
    Ok(())
}

pub async fn project_session_compatibility_state(
    db: &dyn Database,
    event: &SessionEvent,
    projection_id: i32,
    schema_version: u32,
) -> SessionDbResult<()> {
    db.upsert("session_compatibility_state")
        .unique(&["projection_id"])
        .value("projection_id", DatabaseValue::Int32(projection_id))
        .value(
            "schema_version",
            DatabaseValue::Int32(i32::try_from(schema_version).unwrap_or(i32::MAX)),
        )
        .value("last_event_seq", seq_to_value(event.sequence))
        .execute(db)
        .await?;
    Ok(())
}

pub async fn project_session_compatibility(
    db: &dyn Database,
    event: &SessionEvent,
    issue: Option<&SessionEventCompatibilityIssue>,
    projection_id: i32,
    schema_version: u32,
) -> SessionDbResult<()> {
    if let Some(issue) = issue {
        db.upsert("session_compatibility_issues")
            .unique(&["event_seq"])
            .value("event_seq", seq_to_value(issue.sequence))
            .value("event_kind", issue.event_kind.clone())
            .value(
                "event_schema_version",
                DatabaseValue::Int32(i32::from(issue.schema_version)),
            )
            .value(
                "compatibility",
                match issue.compatibility {
                    SessionEventCompatibilityKind::UnknownEventKind => "unknown_event_kind",
                    SessionEventCompatibilityKind::FutureSchema => "future_schema",
                },
            )
            .value("remediation", issue.remediation.clone())
            .execute(db)
            .await?;
    }
    project_session_compatibility_state(db, event, projection_id, schema_version).await
}
