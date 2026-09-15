//! Generic finalized-artifact projection value helpers.

use crate::db::{FinalizedArtifactReference, SessionDbError, SessionDbResult};
use crate::db_event_store::seq_to_value;
use crate::db_row::{i64_to_u64, optional_i64, optional_string, required_i64, required_string};
use bcode_session_models::{ToolArtifact, ToolArtifactRef};
use switchy::database::query::FilterableQuery as _;
use switchy::database::{Database, DatabaseValue};

pub async fn project_artifact_references(
    db: &dyn Database,
    finalized_event_seq: u64,
    artifact: &ToolArtifact,
) -> SessionDbResult<()> {
    for reference in &artifact.refs {
        let (availability, complete, checksum_sha256) =
            generic_artifact_reference_metadata(reference);
        db.upsert("artifact_references")
            .where_eq("artifact_id", artifact.artifact_id.clone())
            .where_eq("reference_key", reference.key.clone())
            .value("artifact_id", artifact.artifact_id.clone())
            .value("reference_key", reference.key.clone())
            .value("producer_plugin_id", artifact.producer_plugin_id.clone())
            .value("schema", artifact.schema.clone())
            .value(
                "schema_version",
                DatabaseValue::Int64(i64::from(artifact.schema_version)),
            )
            .value("storage_uri", reference.storage_uri.clone())
            .value("content_type", reference.content_type.clone())
            .value("byte_len", reference.byte_len.map(seq_to_value))
            .value("availability", availability)
            .value(
                "complete",
                complete.map(|value| DatabaseValue::Int32(i32::from(value))),
            )
            .value("checksum_sha256", checksum_sha256)
            .value("finalized_event_seq", seq_to_value(finalized_event_seq))
            .execute(db)
            .await?;
    }
    Ok(())
}

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
        .and_then(|metadata| metadata.get("content_checksum_sha256"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    (availability, complete, checksum_sha256)
}

/// Resolve an explicitly scoped full logical-content checksum from canonical finalization.
/// Plugin-defined `checksum_sha256` metadata never implies whole-file semantics.
pub fn whole_content_checksum(
    event: Option<&bcode_session_models::SessionEvent>,
    artifact_id: &str,
    reference_key: &str,
) -> SessionDbResult<Option<String>> {
    let invalid = || SessionDbError::InvalidRow {
        column: "artifact.content_checksum_sha256".into(),
    };
    let Some(bcode_session_models::SessionEvent {
        kind: bcode_session_models::SessionEventKind::ToolInvocationResultRecorded { record },
        ..
    }) = event
    else {
        return Err(invalid());
    };
    let Some(bcode_session_models::ToolInvocationResult::Artifact { artifact }) = &record.result
    else {
        return Err(invalid());
    };
    if artifact.artifact_id != artifact_id {
        return Err(invalid());
    }
    let mut references = artifact
        .refs
        .iter()
        .filter(|reference| reference.key == reference_key);
    let reference = references.next().ok_or_else(invalid)?;
    if references.next().is_some() {
        return Err(invalid());
    }
    let Some(value) = reference
        .metadata
        .as_ref()
        .and_then(|m| m.get("content_checksum_sha256"))
    else {
        return Ok(None);
    };
    let checksum = value.as_str().ok_or_else(invalid)?;
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok(Some(checksum.to_owned()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_whole_content_digest_is_enforced() {
        use bcode_session_models::*;
        let mut artifact = ToolArtifact {
            artifact_id: "a".into(),
            producer_plugin_id: "any-plugin".into(),
            schema: "any".into(),
            schema_version: 1,
            tool_call_id: None,
            title: None,
            metadata: serde_json::Value::Null,
            refs: vec![ToolArtifactRef {
                key: "r".into(),
                content_type: None,
                storage_uri: None,
                byte_len: None,
                metadata: Some(serde_json::json!({"checksum_sha256": "plugin-specific"})),
            }],
        };
        let make_event = |artifact| SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            provenance: None,
            session_id: SessionId::new(),
            sequence: 0,
            timestamp_ms: 0,
            kind: SessionEventKind::ToolInvocationResultRecorded {
                record: ToolInvocationResultRecord {
                    invocation_id: "i".into(),
                    model_output: String::new(),
                    is_error: false,
                    presentation: None,
                    content: vec![],
                    result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(artifact),
                    }),
                },
            },
        };
        assert_eq!(
            whole_content_checksum(Some(&make_event(artifact.clone())), "a", "r").unwrap(),
            None
        );
        artifact.refs[0].metadata =
            Some(serde_json::json!({"content_checksum_sha256": "a".repeat(64)}));
        assert_eq!(
            whole_content_checksum(Some(&make_event(artifact.clone())), "a", "r").unwrap(),
            Some("a".repeat(64))
        );
        artifact.refs[0].metadata = Some(serde_json::json!({"content_checksum_sha256": 123}));
        assert!(whole_content_checksum(Some(&make_event(artifact)), "a", "r").is_err());
    }
}
