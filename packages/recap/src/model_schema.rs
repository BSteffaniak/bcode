//! Compatibility forwarding to the engine-owned structured output schemas.

/// Return the portable typed analysis response schema for one stage.
#[must_use]
pub fn analysis_response_schema(
    stage: crate::analysis_contracts::AnalysisStage,
) -> serde_json::Value {
    crate::analysis_contracts::analysis_response_schema(stage)
}
/// Return the portable editorial model response schema.
#[must_use]
pub fn response_schema() -> serde_json::Value {
    crate::model::response_schema()
}
