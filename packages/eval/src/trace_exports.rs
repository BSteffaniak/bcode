//! Explicit sensitive trace capture export through the public application boundary.
use bcode_client::BcodeClient;
use bcode_session_models::{
    MAX_SESSION_TRACE_BLOB_BYTES, ReadSessionTraceBlobRequest, SESSION_TRACE_EXPORT_VERSION,
    SessionEvent,
};
use serde_json::json;
use std::path::Path;

#[cfg(test)]
mod tests;

/// Prioritize model inputs across all observed sessions before optional tool I/O.
/// Receipts remain session-scoped and unavailable boundaries are retained explicitly.
///
/// # Errors
/// Returns an error if retained bytes or receipts cannot be written.
pub fn export_cli_batch(
    sessions: &[(std::path::PathBuf, Vec<SessionEvent>)],
    invoke: &impl Fn(
        &[String],
        &str,
        std::time::Duration,
    ) -> Result<serde_json::Value, super::EvalError>,
) -> Result<(), super::EvalError> {
    let mut receipts = vec![Vec::new(); sessions.len()];
    let mut captures = sessions
        .iter()
        .enumerate()
        .flat_map(|(index, (_, events))| {
            super::introspection::referenced_blobs(events)
                .into_iter()
                .map(move |capture| (index, capture))
        })
        .collect::<Vec<_>>();
    captures.sort_by_key(|(_, capture)| capture.kind != "model_request");
    let mut outcomes: std::collections::BTreeMap<
        _,
        (serde_json::Value, Option<std::path::PathBuf>),
    > = std::collections::BTreeMap::new();
    for (index, capture) in captures {
        let root = sessions[index].0.with_extension("captures");
        let key = (
            capture.session_id,
            capture.sequence,
            serde_json::to_string(&capture.blob)?,
        );
        let label = format!(
            "capture-{index}-{}-{}",
            capture.session_id, capture.sequence
        );
        let (receipt, source) = if let Some((receipt, source)) = outcomes.get(&key) {
            (receipt.clone(), source.clone())
        } else {
            let (receipt, source) = match read_cli_capture(&capture, &label, invoke) {
                Ok(range) => {
                    std::fs::create_dir_all(&root)?;
                    let name = format!(
                        "{}-{}-{}.blob",
                        capture.session_id, capture.sequence, range.sha256
                    );
                    let source = root.join(&name);
                    std::fs::write(&source, &range.bytes)?;
                    (
                        json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"exported","file":name,"sha256":range.sha256,"byte_len":range.byte_len,"completeness":range.completeness,"redaction":range.redaction,"observation":label}),
                        Some(source),
                    )
                }
                Err(failure) => (
                    json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"unavailable","failure":failure,"observation":label}),
                    None,
                ),
            };
            outcomes.insert(key, (receipt.clone(), source.clone()));
            (receipt, source)
        };
        if let Some(source) = source {
            let destination = root.join(receipt["file"].as_str().expect("exported filename"));
            if source != destination {
                // Reuse only exact, fully verified bytes, never an earlier partial range.
                use sha2::{Digest as _, Sha256};
                use std::io::Read as _;
                let mut bytes = Vec::new();
                std::fs::File::open(&source)?
                    .take(capture.blob.byte_len + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 != capture.blob.byte_len
                    || hex::encode(Sha256::digest(&bytes)) != capture.blob.sha256
                {
                    receipts[index].push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"unavailable","failure":CaptureFailure::Integrity,"observation":receipt["observation"]}));
                    continue;
                }
                std::fs::create_dir_all(&root)?;
                std::fs::write(destination, bytes)?;
            }
        }
        receipts[index].push(receipt);
    }
    for ((path, events), mut receipts) in sessions.iter().zip(receipts) {
        validate_context_exports(events, &path.with_extension("captures"), &mut receipts);
        super::write_json(
            path.with_extension("captures.json"),
            &json!({"schema":"bcode.eval.trace_exports.v1","sensitive":true,"receipts":receipts}),
        )?;
    }
    Ok(())
}

// Reads only already-exported eval artifacts with exact generated filenames; never trace locators.
fn exported_bytes(
    root: &Path,
    capture: &super::introspection::CaptureReference,
    receipts: &[serde_json::Value],
) -> Result<Vec<u8>, &'static str> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let matches: Vec<_> = receipts
        .iter()
        .filter(|receipt| {
            receipt["session_id"] == json!(capture.session_id)
                && receipt["sequence"] == json!(capture.sequence)
        })
        .collect();
    let [receipt] = matches.as_slice() else {
        return Err("missing_or_conflicting_export");
    };
    if receipt["status"] != "exported"
        || capture.blob.completeness != bcode_session_models::TraceBlobCompleteness::Complete
        || capture.blob.byte_len > u64::from(MAX_SESSION_TRACE_BLOB_BYTES)
        || capture.blob.content_type != "application/json"
        || receipt["sha256"] != capture.blob.sha256
        || receipt["byte_len"] != capture.blob.byte_len
        || receipt["completeness"] != json!(capture.blob.completeness)
        || receipt["redaction"] != json!(capture.blob.redaction)
    {
        return Err("incomplete_or_unverified_export");
    }
    let name = receipt["file"].as_str().ok_or("missing_export_file")?;
    if name != format!("{}-{}.blob", capture.session_id, capture.sequence)
        && name
            != format!(
                "{}-{}-{}.blob",
                capture.session_id, capture.sequence, capture.blob.sha256
            )
    {
        return Err("invalid_export_filename");
    }
    let path = root.join(name);
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| "unavailable_export_file")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("invalid_export_file");
    }
    let canonical_root = root.canonicalize().map_err(|_| "unavailable_export_root")?;
    let canonical_path = path.canonicalize().map_err(|_| "unavailable_export_file")?;
    if canonical_path.parent() != Some(canonical_root.as_path()) {
        return Err("invalid_export_file");
    }
    let mut bytes = Vec::new();
    std::fs::File::open(canonical_path)
        .map_err(|_| "unavailable_export_file")?
        .take(capture.blob.byte_len + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "unavailable_export_file")?;
    if bytes.len() as u64 != capture.blob.byte_len
        || hex::encode(Sha256::digest(&bytes)) != capture.blob.sha256
    {
        return Err("export_integrity_failed");
    }
    Ok(bytes)
}

// Report contract: full_receipt_validated establishes an exact complete exported artifact pair,
// not delivery. optional_sources_certified means nonempty optional lineage, no lineage omissions,
// and every optional contribution explicitly represented during conversion. Missing receipt entries
// are Unknown. Entries expose host numeric ordinals only; no authored identities or content.
// mandatory/optional counts partition mapped lineage; lineage_omitted has no inferred class.
fn context_report(
    reference: &bcode_session_models::RequestContextObservationReference,
    session: bcode_session_models::SessionId,
    turn: &str,
    request_bytes: &[u8],
    observation_bytes: &[u8],
) -> Result<serde_json::Value, &'static str> {
    use bcode_model::lineage::{
        ContextRepresentationDisposition as D, ContextRepresentationObservation,
    };
    let request: bcode_model::ModelTurnRequest =
        serde_json::from_slice(request_bytes).map_err(|_| "invalid_request_artifact")?;
    if request.session_id != session
        || request.turn_id != reference.provider_request_id
        || request
            .context_lineage
            .as_ref()
            .is_none_or(|lineage| lineage.request_ordinal != reference.request_ordinal)
    {
        return Err("foreign_request_artifact");
    }
    let observation = ContextRepresentationObservation::parse(
        observation_bytes,
        &request,
        turn,
        reference.model_request_sequence,
    )
    .map_err(|_| "invalid_or_foreign_observation_artifact")?;
    let mut mandatory_counts = [0_u64; 3];
    let mut optional_counts = [0_u64; 3];
    let mut entries = Vec::new();
    for source in &observation.lineage.entries {
        let disposition = observation
            .receipt
            .entries
            .iter()
            .find(|entry| entry.source_ordinal == source.source_ordinal)
            .map_or(D::Unknown, |entry| entry.disposition);
        let index = match disposition {
            D::Represented => 0,
            D::Dropped => 1,
            D::Unknown => 2,
        };
        let counts = if source.mandatory {
            &mut mandatory_counts
        } else {
            &mut optional_counts
        };
        counts[index] += 1;
        entries.push(json!({"source_ordinal":source.source_ordinal,"operation_ordinal":source.operation_ordinal,
            "mandatory":source.mandatory,"disposition":disposition}));
    }
    let optional_count: u64 = optional_counts.iter().sum();
    let optional_sources_certified = optional_count > 0
        && observation.lineage.omitted == 0
        && optional_counts[0] == optional_count;
    let optional_representation = if optional_sources_certified {
        "all_represented"
    } else if observation.lineage.omitted > 0 {
        "incomplete_lineage"
    } else if optional_count == 0 {
        "no_optional_sources"
    } else if optional_counts[1] > 0 {
        "dropped"
    } else {
        "unknown"
    };
    let counts = |values: [u64; 3]| json!({"count":values.iter().sum::<u64>(),"represented":values[0],"dropped":values[1],"unknown":values[2]});
    Ok(
        json!({"status":"full_receipt_validated","scope":"representation_only","delivery_claim":false,"quality_claim":false,
        "provider_request_id":reference.provider_request_id,"request_ordinal":reference.request_ordinal,
        "model_request_sequence":reference.model_request_sequence,"represented":mandatory_counts[0]+optional_counts[0],
        "dropped":mandatory_counts[1]+optional_counts[1],"unknown":mandatory_counts[2]+optional_counts[2],
        "mandatory":counts(mandatory_counts),"optional":counts(optional_counts),"entries":entries,
        "receipt_omitted":observation.receipt.omitted,"lineage_omitted":observation.lineage.omitted,
        "optional_representation":optional_representation,"optional_sources_certified":optional_sources_certified}),
    )
}

fn validate_context_event(
    event: &SessionEvent,
    events: &[SessionEvent],
    root: &Path,
    receipts: &[serde_json::Value],
    references: &[super::introspection::CaptureReference],
    duplicate_identity: bool,
) -> Result<serde_json::Value, &'static str> {
    use bcode_session_models::{
        RequestContextObservationReference, SessionEventKind, SessionTracePayload,
    };
    let SessionEventKind::TraceEvent { trace } = &event.kind else {
        return Err("invalid_observation_event");
    };
    let SessionTracePayload::ProviderEvent {
        event_type,
        detail: Some(detail),
    } = &trace.payload
    else {
        return Err("invalid_observation_event");
    };
    if duplicate_identity {
        return Err("conflicting_observation_identity");
    }
    if event.schema_version != bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION {
        return Err("unsupported_event_schema");
    }
    let reference = RequestContextObservationReference::parse(event_type, detail)
        .map_err(|_| "invalid_observation_reference")?;
    let turn = trace.turn_id.as_deref().ok_or("missing_canonical_turn")?;
    let producer_sequence = reference.model_request_sequence;
    let producers: Vec<_> = events
        .iter()
        .filter(|producer| {
            producer.session_id == event.session_id && producer.sequence == producer_sequence
        })
        .collect();
    let [producer] = producers.as_slice() else {
        return Err("missing_or_conflicting_producer_event");
    };
    if producer.sequence >= event.sequence || producer.schema_version != event.schema_version {
        return Err("invalid_producer_event_order");
    }
    let SessionEventKind::TraceEvent {
        trace: producer_trace,
    } = &producer.kind
    else {
        return Err("invalid_producer_event");
    };
    if producer_trace.turn_id.as_deref() != Some(turn)
        || !matches!(
            producer_trace.payload,
            SessionTracePayload::ModelRequestBuilt { .. }
        )
    {
        return Err("foreign_producer_event");
    }
    let capture = references
        .iter()
        .find(|capture| {
            capture.session_id == event.session_id
                && capture.sequence == event.sequence
                && capture.kind == "request_context_representation"
        })
        .ok_or("missing_observation_capture")?;
    let producer_capture = references
        .iter()
        .find(|capture| {
            capture.session_id == event.session_id
                && capture.sequence == producer.sequence
                && capture.kind == "model_request"
        })
        .ok_or("missing_request_capture")?;
    context_report(
        &reference,
        event.session_id,
        turn,
        &exported_bytes(root, producer_capture, receipts)?,
        &exported_bytes(root, capture, receipts)?,
    )
}

fn validate_context_exports(
    events: &[SessionEvent],
    root: &Path,
    receipts: &mut [serde_json::Value],
) {
    use bcode_session_models::{
        REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE, RequestContextObservationReference,
        SessionEventKind, SessionTracePayload,
    };
    let references = super::introspection::referenced_blobs(events);
    let mut reports = Vec::new();
    for event in events {
        let SessionEventKind::TraceEvent { trace } = &event.kind else {
            continue;
        };
        let SessionTracePayload::ProviderEvent {
            event_type,
            detail: Some(detail),
        } = &trace.payload
        else {
            continue;
        };
        if event_type != REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE {
            continue;
        }
        let duplicate_identity = RequestContextObservationReference::parse(event_type, detail)
            .is_ok_and(|reference| {
                events
                    .iter()
                    .filter(|other| {
                        if other.session_id != event.session_id {
                            return false;
                        }
                        let SessionEventKind::TraceEvent { trace: other_trace } = &other.kind
                        else {
                            return false;
                        };
                        let SessionTracePayload::ProviderEvent {
                            event_type,
                            detail: Some(detail),
                        } = &other_trace.payload
                        else {
                            return false;
                        };
                        RequestContextObservationReference::parse(event_type, detail).is_ok_and(
                            |other_reference| {
                                other_reference.provider_request_id == reference.provider_request_id
                                    && other_reference.request_ordinal == reference.request_ordinal
                            },
                        )
                    })
                    .count()
                    != 1
            });
        let result = validate_context_event(
            event,
            events,
            root,
            receipts,
            &references,
            duplicate_identity,
        );
        reports.push((event.session_id, event.sequence, result.unwrap_or_else(|status| json!({"status":status,"scope":"representation_only","delivery_claim":false,"quality_claim":false}))));
    }
    for receipt in receipts {
        let matching: Vec<_> = reports
            .iter()
            .filter(|(session, sequence, _)| {
                receipt["session_id"] == json!(session) && receipt["sequence"] == json!(sequence)
            })
            .collect();
        if let [(_, _, report)] = matching.as_slice() {
            receipt["context_representation_validation"] = report.clone();
        } else if !matching.is_empty() {
            receipt["context_representation_validation"] =
                json!({"status":"conflicting_observation_events"});
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CaptureFailure {
    Deadline,
    Invocation,
    Decode,
    Integrity,
}

impl CaptureFailure {
    fn invocation(error: &super::EvalError) -> Self {
        // Exact local capture-budget sentinel, never provider or tool display text.
        if matches!(error, super::EvalError::Validation(message)
            if message == "post-execution capture allowance exhausted")
        {
            Self::Deadline
        } else if matches!(error, super::EvalError::Json(_)) {
            Self::Decode
        } else {
            Self::Invocation
        }
    }
}

fn read_cli_capture(
    capture: &super::introspection::CaptureReference,
    label: &str,
    invoke: &impl Fn(
        &[String],
        &str,
        std::time::Duration,
    ) -> Result<serde_json::Value, super::EvalError>,
) -> Result<bcode_session_models::SessionTraceBlobRange, CaptureFailure> {
    use sha2::{Digest as _, Sha256};
    // Pretty-printed byte arrays expand on the CLI wire. Keep each response below
    // the subprocess reader's budget independently of the retained blob budget.
    const CHUNK: u32 = 64 * 1024;
    if capture.blob.byte_len > u64::from(MAX_SESSION_TRACE_BLOB_BYTES) {
        return Err(CaptureFailure::Integrity);
    }
    let mut bytes = Vec::new();
    loop {
        let offset = bytes.len() as u64;
        let value = invoke(
            &[
                "session".into(),
                "trace-range".into(),
                capture.session_id.to_string(),
                capture.sequence.to_string(),
                "--offset".into(),
                offset.to_string(),
                "--length".into(),
                CHUNK.to_string(),
            ],
            &format!("{label}-{offset}"),
            std::time::Duration::from_secs(15),
        )
        .map_err(|error| CaptureFailure::invocation(&error))?;
        let mut range: bcode_session_models::SessionTraceBlobRange =
            serde_json::from_value(value).map_err(|_| CaptureFailure::Decode)?;
        if range.version != SESSION_TRACE_EXPORT_VERSION
            || range.session_id != capture.session_id
            || range.sequence != capture.sequence
            || range.sha256 != capture.blob.sha256
            || range.content_type != capture.blob.content_type
            || range.byte_len != capture.blob.byte_len
            || range.completeness != capture.blob.completeness
            || range.redaction != capture.blob.redaction
            || range.offset != offset
            || range.bytes.len() > CHUNK as usize
        {
            return Err(CaptureFailure::Integrity);
        }
        let end = offset
            .checked_add(range.bytes.len() as u64)
            .ok_or(CaptureFailure::Integrity)?;
        if end > range.byte_len || range.eof != (end == range.byte_len) {
            return Err(CaptureFailure::Integrity);
        }
        bytes.extend_from_slice(&range.bytes);
        if range.eof {
            if hex::encode(Sha256::digest(&bytes)) != capture.blob.sha256 {
                return Err(CaptureFailure::Integrity);
            }
            range.offset = 0;
            range.bytes = bytes;
            return Ok(range);
        }
        if end == offset {
            return Err(CaptureFailure::Integrity);
        }
    }
}

pub async fn export(
    client: &BcodeClient,
    path: &Path,
    events: &[SessionEvent],
) -> Result<(), super::EvalError> {
    export_document(client, path, events).await.map(|_| ())
}

async fn export_document(
    client: &BcodeClient,
    path: &Path,
    events: &[SessionEvent],
) -> Result<serde_json::Value, super::EvalError> {
    use sha2::{Digest as _, Sha256};
    let root = path.with_extension("captures");
    let mut receipts = Vec::new();
    for capture in super::introspection::referenced_blobs(events) {
        let reference = &capture.blob;
        let request = ReadSessionTraceBlobRequest {
            version: SESSION_TRACE_EXPORT_VERSION,
            session_id: capture.session_id,
            sequence: capture.sequence,
            offset: 0,
            length: MAX_SESSION_TRACE_BLOB_BYTES,
        };
        match client.session_trace_blob_range(request).await {
            Ok(range) if range.version == SESSION_TRACE_EXPORT_VERSION
                && range.session_id == capture.session_id && range.sequence == capture.sequence
                && range.offset == 0 && range.sha256 == reference.sha256
                && range.byte_len == reference.byte_len && range.content_type == reference.content_type
                && range.completeness == reference.completeness && range.redaction == reference.redaction
                && range.eof && range.bytes.len() as u64 == range.byte_len
                && hex::encode(Sha256::digest(&range.bytes)) == reference.sha256 => {
                std::fs::create_dir_all(&root)?;
                let name = format!("{}-{}.blob", capture.session_id, capture.sequence);
                std::fs::write(root.join(&name), &range.bytes)?;
                receipts.push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"exported","file":name,"sha256":range.sha256,"byte_len":range.byte_len,"completeness":range.completeness,"redaction":range.redaction}));
            }
            Ok(_) => receipts.push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"verification_failed"})),
            Err(_) => receipts.push(json!({"session_id":capture.session_id,"sequence":capture.sequence,"status":"unavailable"})),
        }
    }
    validate_context_exports(events, &root, &mut receipts);
    let document =
        json!({"schema":"bcode.eval.trace_exports.v1","sensitive":true,"receipts":receipts});
    super::write_json(path.with_extension("captures.json"), &document)?;
    Ok(document)
}

fn validate_context_window(
    session: bcode_session_models::SessionId,
    events: &[SessionEvent],
) -> Result<(), super::EvalError> {
    if events.len() > 3200 {
        return Err(super::EvalError::Validation(
            "context observation window exceeds 3200 events".into(),
        ));
    }
    let mut sequences = std::collections::BTreeSet::new();
    for event in events {
        if event.session_id != session
            || !sequences.insert(event.sequence)
            || event.schema_version != bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
        {
            return Err(super::EvalError::Validation(
                "foreign, duplicate or unsupported context observation event".into(),
            ));
        }
    }
    Ok(())
}

pub async fn inspect_context_observations(
    client: &BcodeClient,
    session: bcode_session_models::SessionId,
    events: &[SessionEvent],
    output_directory: &Path,
) -> Result<serde_json::Value, super::EvalError> {
    validate_context_window(session, events)?;
    // Context export needs only request producers and reserved observations, not unrelated tool I/O.
    let context_events: Vec<_> = events.iter().filter(|event| matches!(&event.kind,
        bcode_session_models::SessionEventKind::TraceEvent { trace } if matches!(&trace.payload,
            bcode_session_models::SessionTracePayload::ModelRequestBuilt { .. }) || matches!(&trace.payload,
            bcode_session_models::SessionTracePayload::ProviderEvent { event_type, .. } if event_type == bcode_session_models::REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE)
    )).cloned().collect();
    let directory = output_directory.join(session.to_string());
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("context-observations.jsonl");
    let document = export_document(client, &path, &context_events).await?;
    let summary = super::introspection::summary_with_exports(&context_events, &document);
    let retained = document["receipts"].as_array().map_or(0, |receipts| {
        receipts
            .iter()
            .filter(|receipt| receipt["status"] == "exported")
            .count()
    });
    Ok(
        json!({"schema":"bcode.eval.context_observations.v1","session_id":session,
        "scope":"caller_retained_event_window","events_supplied":events.len(),"window_complete":false,
        "export_completed":true,"captures_retained":retained,"delivery_claim":false,"quality_claim":false,
        "context_representation_reports":summary["context_representation_reports"],
        "export_outcomes":summary["export_outcomes"],"context_representation_scope":summary["context_representation_scope"]}),
    )
}
