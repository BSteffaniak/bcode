//! Shared renderer conformance fixtures for `HyperChad` tests.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct RendererTranscriptFixture {
    pub name: String,
    pub expected: Vec<String>,
    #[serde(default)]
    pub forbidden: Vec<String>,
    pub item: bcode_session_view_models::TranscriptViewItem,
}

pub fn renderer_tool_presentation_fixtures() -> Vec<RendererTranscriptFixture> {
    serde_json::from_str(include_str!(
        "../../../session-view/models/fixtures/renderer-tool-presentations.json"
    ))
    .expect("renderer tool presentation fixtures must match session-view models")
}
