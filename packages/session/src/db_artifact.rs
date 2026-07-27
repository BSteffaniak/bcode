//! Generic finalized-artifact projection value helpers.

use crate::db::{FinalizedArtifactReference, SessionDbError, SessionDbResult};
use crate::db_row::{i64_to_u64, optional_i64, optional_string, required_i64, required_string};
use bcode_session_models::ToolArtifactRef;

#[must_use]
pub fn generic_artifact_reference_metadata(
    reference: &ToolArtifactRef,
) -> (Option<String>, Option<bool>, Option<String>) {
    let metadata = reference.metadata.as_ref();
    let availability = metadata
        .and_then(|metadata| metadata.get("availability"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let complete = metadata
        .and_then(|metadata| metadata.get("complete"))
        .and_then(serde_json::Value::as_bool);
    let checksum_sha256 = metadata
        .and_then(|metadata| metadata.get("checksum_sha256"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    (availability, complete, checksum_sha256)
}

pub fn finalized_artifact_reference_from_row(
    row: &switchy::database::Row,
) -> SessionDbResult<FinalizedArtifactReference> {
    Ok(FinalizedArtifactReference {
        artifact_id: required_string(row, "artifact_id")?,
        reference_key: required_string(row, "reference_key")?,
        producer_plugin_id: required_string(row, "producer_plugin_id")?,
        schema: required_string(row, "schema")?,
        schema_version: u32::try_from(required_i64(row, "schema_version")?).map_err(|_| {
            SessionDbError::InvalidRow {
                column: "schema_version".to_owned(),
            }
        })?,
        storage_uri: optional_string(row, "storage_uri"),
        content_type: optional_string(row, "content_type"),
        byte_len: optional_i64(row, "byte_len").map(i64_to_u64),
        availability: optional_string(row, "availability"),
        complete: optional_i64(row, "complete").map(|value| value != 0),
        checksum_sha256: optional_string(row, "checksum_sha256"),
        finalized_event_seq: required_i64(row, "finalized_event_seq").map(i64_to_u64)?,
    })
}
