//! Compatibility forwarding to the engine-owned structured output schema.
/// Return the portable editorial model response schema.
#[must_use]
pub fn response_schema() -> serde_json::Value {
    crate::model::response_schema()
}
