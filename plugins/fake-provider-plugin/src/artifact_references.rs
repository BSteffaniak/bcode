//! Reference-metadata-only fixture model. Never reads paths or claims pixel understanding.

use std::collections::{BTreeMap, BTreeSet};

use bcode_model::{
    CapabilitySource, CapabilitySupport, ContentBlock, ImageRefContent, MediaInputFeature,
    MessageRole, ModelFeatureSupport, ModelTurnRequest, ToolResultContent,
};

/// Explicit test-only metadata interpretation contract; not an image vision model.
pub const MODEL: &str = "fake-tool-artifact-references";

/// Advertise only artifact-backed reference metadata, never inline image understanding.
pub fn support(features: &mut ModelFeatureSupport) {
    features.media_input.insert(
        MediaInputFeature::ImageReference,
        CapabilitySupport::supported(CapabilitySource::TestContract),
    );
}

fn bounded(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn valid_reference(image: &ImageRefContent) -> bool {
    // Paths are opaque diagnostics only. In particular, neither this validation
    // nor the acknowledgment dereferences them or treats them as authority.
    bounded(&image.path, 4096)
        && image
            .artifact_id
            .as_deref()
            .is_some_and(|id| bounded(id, 4096))
        && image
            .reference_key
            .as_deref()
            .is_some_and(|key| bounded(key, 512))
        && matches!(
            image.mime_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp"
        )
        && image
            .metadata
            .width
            .is_some_and(|width| (1..=65_536).contains(&width))
        && image
            .metadata
            .height
            .is_some_and(|height| (1..=65_536).contains(&height))
        && image
            .metadata
            .byte_len
            .is_some_and(|bytes| (1..=64 * 1024 * 1024).contains(&bytes))
        && image
            .metadata
            .source_path
            .as_deref()
            .is_none_or(|source| bounded(source, 4096) && source == image.path)
}

/// Validate all references, including retained history, without accessing their source.
/// Session ownership and byte integrity remain the host artifact boundary's responsibility.
///
/// # Errors
/// Rejects incomplete, oversized, conflicting or misplaced reference metadata and
/// invalid or duplicate current tool-result identities.
pub fn validate(request: &ModelTurnRequest) -> Result<(), &'static str> {
    let mut identities = BTreeMap::new();
    let mut count = 0;
    for message in &request.messages {
        for block in &message.content {
            let ContentBlock::ToolResult { result } = block else {
                continue;
            };
            for content in &result.content {
                let ToolResultContent::ImageRef { image } = content else {
                    continue;
                };
                count += 1;
                if count > 64
                    || message.role != MessageRole::Tool
                    || !bounded(&result.call_id, 512)
                    || !valid_reference(image)
                {
                    return Err("invalid bounded tool artifact image reference metadata");
                }
                let key = (image.artifact_id.as_deref(), image.reference_key.as_deref());
                if let Some(previous) = identities.insert(key, image)
                    && previous != image
                {
                    return Err("conflicting tool artifact image reference metadata");
                }
            }
        }
    }
    let mut calls = BTreeSet::new();
    if let Some(start) = request
        .messages
        .iter()
        .rposition(|message| message.role == MessageRole::User)
    {
        for message in &request.messages[start + 1..] {
            for block in &message.content {
                if let ContentBlock::ToolResult { result } = block
                    && (message.role != MessageRole::Tool
                        || !bounded(&result.call_id, 512)
                        || calls.len() >= 64
                        || !calls.insert(&result.call_id))
                {
                    return Err("invalid or duplicate current tool result identity");
                }
            }
        }
    }
    Ok(())
}

/// Acknowledge every result in the current user activation, not merely its last tool.
/// No pixel answer, path, tool output text or untrusted diagnostic is echoed.
pub fn acknowledgment(request: &ModelTurnRequest) -> Option<String> {
    validate(request).ok()?;
    let start = request
        .messages
        .iter()
        .rposition(|message| message.role == MessageRole::User)?
        + 1;
    let mut has_reference = false;
    let mut calls = BTreeSet::new();
    let mut results = Vec::new();
    for message in &request.messages[start..] {
        for block in &message.content {
            let ContentBlock::ToolResult { result } = block else {
                continue;
            };
            if message.role != MessageRole::Tool
                || results.len() >= 64
                || !bounded(&result.call_id, 512)
                || !calls.insert(&result.call_id)
            {
                return None;
            }
            let references = result
                .content
                .iter()
                .filter(|content| matches!(content, ToolResultContent::ImageRef { .. }))
                .count();
            has_reference |= references > 0;
            results.push(serde_json::json!({"call_id":result.call_id, "is_error":result.is_error, "image_reference_count":references}));
        }
    }
    has_reference.then(|| serde_json::json!({"type":"fake_tool_artifact_reference_ack", "pixel_understanding":false, "results":results}).to_string())
}

#[cfg(test)]
mod tests;
