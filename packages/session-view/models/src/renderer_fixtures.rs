//! Shared renderer conformance fixtures.
//!
//! This module is test-only so production renderer-neutral models do not carry fixture APIs or
//! compile fixture data into binaries.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct RendererTranscriptFixture {
    pub name: String,
    pub expected: Vec<String>,
    pub item: super::TranscriptViewItem,
}

pub fn renderer_tool_presentation_fixtures() -> Vec<RendererTranscriptFixture> {
    serde_json::from_str(include_str!("../fixtures/renderer-tool-presentations.json"))
        .expect("renderer tool presentation fixtures must match session-view models")
}
