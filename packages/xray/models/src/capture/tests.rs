use super::*;
use crate::XrayCorrelation;

fn reference() -> XrayPayloadRef {
    XrayPayloadRef {
        schema_version: XRAY_SCHEMA_VERSION,
        payload_id: "payload-1".into(),
        session_id: "session-1".into(),
        content_type: "application/json".into(),
        byte_len: 0,
        sha256: "0".repeat(64),
    }
}

fn event(data: serde_json::Value, pointers: &[&str]) -> XrayEvent {
    let mut event = XrayEvent::new(
        "future-domain",
        "future-event",
        XrayCorrelation {
            session_id: "session-1".into(),
            ..XrayCorrelation::default()
        },
        data,
        true,
    );
    event.capture = Some(XrayCaptureAttachments::new(
        pointers
            .iter()
            .map(|pointer| XrayCaptureAttachment::PayloadRef {
                pointer: (*pointer).into(),
            })
            .collect(),
    ));
    event
}

#[test]
fn legacy_collision_shapes_remain_opaque_and_round_trip_without_metadata() {
    let mut source = event(
        serde_json::json!({
            "tool": {"payload_id": "ordinary-user-input"},
            "provider": [{"sha256": "not-a-digest", "byte_len": 123}],
            "valid_looking": reference(),
            "capture": {"schema_version": 999, "attachments": [{"kind": "future"}]}
        }),
        &[],
    );
    source.capture = None;
    let encoded = serde_json::to_value(&source).unwrap();
    assert!(encoded.get("capture").is_none());
    let decoded: XrayEvent = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, source);
    assert_eq!(
        decoded.payload_references().unwrap(),
        [] as [XrayPayloadRef; 0]
    );
}

#[test]
fn declarations_ignore_arbitrary_collisions_and_resolve_escaped_keys_and_arrays() {
    let source = event(
        serde_json::json!({
            "ordinary": {"payload_id": "not-a-reference", "sha256": 3, "byte_len": []},
            "a/b~c": [reference()],
            "also_ordinary": {"schema_version": 999, "payload_id": "anything"}
        }),
        &["/a~1b~0c/0"],
    );
    let round_trip: XrayEvent =
        serde_json::from_slice(&serde_json::to_vec(&source).unwrap()).unwrap();
    assert_eq!(round_trip.payload_references().unwrap(), vec![reference()]);
    assert_eq!(round_trip, source);
}

#[test]
fn empty_attachment_list_does_not_walk_even_valid_reference_shapes() {
    assert_eq!(
        event(serde_json::json!([reference()]), &[])
            .payload_references()
            .unwrap(),
        [] as [XrayPayloadRef; 0]
    );
}

#[test]
fn root_reference_is_explicit_and_identical_identities_are_deduplicated() {
    assert_eq!(
        event(serde_json::json!(reference()), &[""])
            .payload_references()
            .unwrap(),
        vec![reference()]
    );
    assert_eq!(
        event(serde_json::json!([reference(), reference()]), &["/0", "/1"])
            .payload_references()
            .unwrap(),
        vec![reference()]
    );
}

#[test]
fn missing_malformed_and_duplicate_locations_fail_without_partial_results() {
    for pointers in [
        vec!["/missing"],
        vec!["payload"],
        vec!["#/payload"],
        vec!["/~"],
        vec!["/~2"],
        vec!["/payload", "/payload"],
        vec!["/payload", "/missing"],
    ] {
        assert_eq!(
            event(serde_json::json!({"payload": reference()}), &pointers).payload_references(),
            Err(XrayCaptureError::InvalidLocation),
            "{pointers:?}",
        );
    }
    // JSON pointer array indexes are strict, not guessed from malformed syntax.
    for pointer in ["/01", "/-", "/+0"] {
        assert_eq!(
            event(serde_json::json!([reference()]), &[pointer]).payload_references(),
            Err(XrayCaptureError::InvalidLocation)
        );
    }
}

#[test]
fn malformed_declared_reference_and_future_fields_fail_closed() {
    let valid = serde_json::to_value(reference()).unwrap();
    let mut variants = vec![
        serde_json::json!(null),
        serde_json::json!({"payload_id": "p"}),
    ];
    for (key, value) in [
        ("schema_version", serde_json::json!(999)),
        ("payload_id", serde_json::json!("")),
        ("content_type", serde_json::json!("")),
        ("sha256", serde_json::json!("A".repeat(64))),
        ("sha256", serde_json::json!("0".repeat(63))),
        ("byte_len", serde_json::json!(-1)),
        ("future_semantics", serde_json::json!(true)),
    ] {
        let mut changed = valid.clone();
        changed[key] = value;
        variants.push(changed);
    }
    for data in variants {
        assert_eq!(
            event(data, &[""]).payload_references(),
            Err(XrayCaptureError::InvalidReference)
        );
    }
}

#[test]
fn foreign_session_and_conflicting_reference_metadata_fail_closed() {
    let mut foreign = reference();
    foreign.session_id = "other-session".into();
    assert_eq!(
        event(serde_json::json!(foreign), &[""]).payload_references(),
        Err(XrayCaptureError::ForeignSession)
    );
    let mut conflict = reference();
    conflict.byte_len = 42;
    assert_eq!(
        event(serde_json::json!([reference(), conflict]), &["/0", "/1"]).payload_references(),
        Err(XrayCaptureError::ConflictingReference)
    );
}

#[test]
fn unknown_capture_metadata_versions_kinds_and_fields_are_rejected_at_decode() {
    for capture in [
        serde_json::json!({"schema_version": 2, "attachments": []}),
        serde_json::json!({"schema_version": 0, "attachments": []}),
        serde_json::json!({"attachments": []}),
        serde_json::json!({"schema_version": 1}),
        serde_json::json!({"schema_version": 1, "attachments": [], "future": true}),
        serde_json::json!({"schema_version": 1, "attachments": [{"kind": "future", "pointer": ""}]}),
        serde_json::json!({"schema_version": 1, "attachments": [{"pointer": ""}]}),
        serde_json::json!({"schema_version": 1, "attachments": [{"kind": "payload_ref", "pointer": "", "recursive": true}]}),
    ] {
        let mut wire = serde_json::to_value(event(serde_json::json!(null), &[])).unwrap();
        wire["capture"] = capture;
        assert!(serde_json::from_value::<XrayEvent>(wire).is_err());
    }
}

#[test]
fn envelope_decode_rejects_future_capture_even_with_supported_envelope_version() {
    let mut wire = serde_json::to_value(envelope(event(serde_json::json!(null), &[]))).unwrap();
    wire["event"]["capture"]["schema_version"] = serde_json::json!(999);
    assert!(serde_json::from_value::<XrayEnvelope>(wire).is_err());
}

fn envelope(event: XrayEvent) -> XrayEnvelope {
    XrayEnvelope {
        schema_version: XRAY_SCHEMA_VERSION,
        observation_id: "observation-1".into(),
        sequence: 1,
        timestamp_unix_ms: 0,
        event,
    }
}

#[test]
fn in_memory_versions_are_checked_before_reference_interpretation() {
    let mut source = event(serde_json::json!(reference()), &[""]);
    source.capture.as_mut().unwrap().schema_version = 999;
    assert_eq!(
        source.payload_references(),
        Err(XrayCaptureError::UnsupportedCaptureVersion)
    );
    let mut outer = envelope(source);
    outer.schema_version = 999;
    assert_eq!(
        outer.payload_references(),
        Err(XrayCaptureError::UnsupportedEnvelopeVersion)
    );
}

#[test]
fn error_messages_do_not_echo_untrusted_paths_or_reference_secrets() {
    let error = event(serde_json::json!(null), &["/private-secret-value"])
        .payload_references()
        .unwrap_err();
    assert_eq!(error.to_string(), "invalid X-ray attachment location");
}
