//! Explicit, bounded session working-document operations (compatibility version 1).
use serde::{Deserialize, Serialize};

/// Maximum UTF-8 working document size accepted by preparation and bounded reads.
pub const MAX_WORKING_DOCUMENT_BYTES: usize = 65_536;

/// Prepare or inspect one mutable document scoped by an opaque UUID.
/// `initial_text: None` is strictly read-only; an existing file is never overwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkingDocumentRequest {
    /// Independently versioned operation contract; currently 1.
    pub version: u32,
    /// Owning canonical session.
    pub session_id: crate::SessionId,
    /// Stable UUID chosen once by the caller (for example a workflow run ID).
    pub scope_id: String,
    /// Explicit creation authorization and initial content, or read-only inspection.
    pub initial_text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_roundtrip_preserves_read_only_intent_and_rejects_unknown_fields() {
        let request = SessionWorkingDocumentRequest {
            version: 1,
            session_id: crate::SessionId::new(),
            scope_id: "2d6ec148-3cf2-432a-8574-18ed0e744c87".into(),
            initial_text: None,
        };
        let mut value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionWorkingDocumentRequest>(value.clone()).unwrap(),
            request
        );
        value["path"] = "/arbitrary/path".into();
        assert!(serde_json::from_value::<SessionWorkingDocumentRequest>(value).is_err());
    }
}

/// Bounded snapshot of a mutable working file; not canonical execution state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorkingDocument {
    /// Canonical owning session.
    pub session_id: crate::SessionId,
    /// Stable scope identity.
    pub scope_id: String,
    /// Owner-resolved local path; ordinary file permissions still apply.
    pub path: String,
    /// Current bounded UTF-8 content, not an immutable artifact snapshot.
    pub text: String,
}
