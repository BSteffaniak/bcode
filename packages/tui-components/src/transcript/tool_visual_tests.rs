//! Shared native adapter projection regressions.
use super::*;

#[test]
fn artifact_projection_preserves_structured_payload_and_stable_identity() {
    let artifact = ToolArtifact {
        artifact_id: "artifact".into(),
        producer_plugin_id: "plugin".into(),
        schema: "plugin.result".into(),
        schema_version: 2,
        tool_call_id: Some("call".into()),
        title: Some("Result".into()),
        metadata: serde_json::json!({"nested": {"values": [1, 2]}, "_bcode_presentation_revision": 9}),
        refs: Vec::new(),
    };
    let CanonicalToolVisual::Plugin(visual) = CanonicalToolVisual::from_artifact(&artifact);
    assert_eq!(
        visual.payload["nested"]["values"],
        serde_json::json!([1, 2])
    );
    assert_eq!(visual.payload["_bcode_runtime"]["live_state_key"], "call");
    assert_eq!(visual.revision, 9);
    assert_eq!(visual.schema_version, 2);
}

#[test]
fn request_projection_rejects_incomplete_json_and_keeps_schema_registry_owned() {
    assert!(
        CanonicalToolVisual::from_request(
            "call",
            None,
            "tool",
            "schema".into(),
            1,
            "{\"partial\":"
        )
        .is_none()
    );
    let CanonicalToolVisual::Plugin(visual) = CanonicalToolVisual::from_request(
        "call",
        Some("plugin"),
        "tool",
        "schema".into(),
        3,
        "{\"value\": [true, null]}",
    )
    .unwrap();
    assert_eq!(visual.payload["value"], serde_json::json!([true, null]));
    assert_eq!(visual.payload["_bcode_invocation"]["tool_name"], "tool");
    assert_eq!(visual.schema, "schema");
    assert_eq!(visual.schema_version, 3);
}
