//! Current canonical event-row reads and writes.

use crate::db::{
    SessionDbError, SessionDbResult, event_created_at_ms, event_kind_name, seq_to_value,
};
use crate::persisted::{
    CompatibleSessionEvent, decode_session_event, decode_session_event_compatible,
    encode_session_event,
};
use bcode_session_models::{SessionEvent, SessionId};
use switchy::database::{Database, DatabaseValue, query::SortDirection};

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

pub async fn strict_events(db: &dyn Database) -> SessionDbResult<Vec<SessionEvent>> {
    let rows = db
        .select("events")
        .columns(&["payload"])
        .sort("event_seq", SortDirection::Asc)
        .execute(db)
        .await?;
    rows.into_iter()
        .map(|row| decode_session_event(&required_string(&row, "payload")?).map_err(Into::into))
        .collect()
}

pub fn compatible_event_from_row(
    row: &switchy::database::Row,
    session_id: SessionId,
) -> SessionDbResult<CompatibleSessionEvent> {
    let event_seq = required_non_negative_u64(row, "event_seq")?;
    let payload = required_string(row, "payload")?;
    let decoded = decode_session_event_compatible(&payload)?;
    let event = decoded.event();
    if event.sequence != event_seq {
        return Err(SessionDbError::InvalidCanonicalSequence {
            expected: event_seq,
            actual: event.sequence,
        });
    }
    if event.session_id != session_id {
        return Err(SessionDbError::InvalidRow {
            column: "events.session_id".to_owned(),
        });
    }
    Ok(decoded)
}

pub async fn last_sequence(db: &dyn Database) -> SessionDbResult<Option<u64>> {
    let row = db
        .query_raw("SELECT MAX(event_seq) AS event_seq FROM events")
        .await?
        .into_iter()
        .next();
    let value = row
        .as_ref()
        .and_then(|row| row.get("event_seq"))
        .and_then(|value| value.as_i64());
    match value {
        Some(value) if value.is_negative() => Err(SessionDbError::InvalidRow {
            column: "event_seq".to_owned(),
        }),
        Some(value) => Ok(Some(value.cast_unsigned())),
        None => Ok(None),
    }
}

fn required_string(row: &switchy::database::Row, column: &str) -> SessionDbResult<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_owned(),
        })
}

fn required_non_negative_u64(row: &switchy::database::Row, column: &str) -> SessionDbResult<u64> {
    let value = row
        .get(column)
        .and_then(|value| value.as_i64())
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_owned(),
        })?;
    if value.is_negative() {
        return Err(SessionDbError::InvalidRow {
            column: column.to_owned(),
        });
    }
    Ok(value.cast_unsigned())
}
