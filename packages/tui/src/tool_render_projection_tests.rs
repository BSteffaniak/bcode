use crate::tool_render_projection::CanonicalToolVisual;
use serde_json::Value;

#[test]
fn canonical_artifact_payload_keeps_stable_live_state_identity() {
    let artifact = bcode_session_models::ToolArtifact {
        artifact_id: "artifact".to_owned(),
        producer_plugin_id: "plugin".to_owned(),
        schema: "plugin.recording".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call".to_owned()),
        title: None,
        metadata: serde_json::Value::Null,
        refs: Vec::new(),
    };
    let CanonicalToolVisual::Plugin(visual) = CanonicalToolVisual::from_artifact(&artifact);
    assert_eq!(
        visual
            .payload
            .pointer("/_bcode_artifact/artifact_id")
            .and_then(Value::as_str),
        Some("artifact")
    );
    assert_eq!(
        visual
            .payload
            .pointer("/_bcode_runtime/live_state_key")
            .and_then(Value::as_str),
        Some("call")
    );
}
