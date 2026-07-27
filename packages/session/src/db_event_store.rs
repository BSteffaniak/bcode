//! Current canonical event-row insertion.

use crate::db::{SessionDbResult, event_created_at_ms, event_kind_name, seq_to_value};
use crate::persisted::encode_session_event;
use bcode_session_models::SessionEvent;
use switchy::database::{Database, DatabaseValue};

pub async fn insert_event(
    db: &dyn Database,
    event: &SessionEvent,
    activity_timestamp_ms: Option<u64>,
) -> SessionDbResult<()> {
    db.insert("events")
        .value("event_seq", seq_to_value(event.sequence))
        .value("event_type", event_kind_name(&event.kind))
        .value(
            "schema_version",
            DatabaseValue::Int32(i32::from(event.schema_version)),
        )
        .value(
            "created_at_ms",
            seq_to_value(activity_timestamp_ms.unwrap_or_else(|| event_created_at_ms(event))),
        )
        .value("payload", encode_session_event(event)?)
        .execute(db)
        .await?;
    Ok(())
}
