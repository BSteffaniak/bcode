//! Optional provider-owned remote history retrieval contracts.

use serde::{Deserialize, Serialize};

/// Credential-free, network-free history support discovery.
pub const OP_HISTORY_CAPABILITIES: &str = "history_capabilities";

/// Request static history support without resolving credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryCapabilitiesRequest {
    /// Wire compatibility version (1).
    pub schema_version: u32,
}

/// Truthful scope coverage; unknown future values must never imply support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryScopeSupport {
    /// Adapter implements retrieval, but upstream coverage remains unverified.
    Unverified,
    /// No retrieval implementation exists for this scope.
    Unsupported,
    /// Retrieval and upstream scope coverage have been verified.
    Verified,
}

/// Static adapter capabilities, not account authorization or sync completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCapabilities {
    /// Wire compatibility version (1).
    pub schema_version: u32,
    /// Auth schemes accepted by this history adapter.
    pub auth_schemes: std::collections::BTreeSet<String>,
    /// Ordinary conversation coverage.
    pub ordinary: HistoryScopeSupport,
    /// Archived conversation coverage.
    pub archived: HistoryScopeSupport,
    /// Project conversation coverage.
    pub projects: HistoryScopeSupport,
}

// Request context can contain credentials and environment secrets. Never delegate
// diagnostic formatting to its Debug implementation or include source identities.
impl std::fmt::Debug for ListHistoryPageRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListHistoryPageRequest")
            .field("schema_version", &self.schema_version)
            .field("offset", &self.offset)
            .field("limit", &self.limit)
            .field("archived", &self.archived)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for LoadHistorySnapshotRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadHistorySnapshotRequest")
            .field("schema_version", &self.schema_version)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_diagnostics_omit_context_and_source_identity() {
        let context = crate::ProviderRequestContext {
            env: [("PRIVATE_TOKEN".into(), "secret-marker".into())].into(),
            auth_profile: Some("private-profile".into()),
            ..Default::default()
        };
        let load = LoadHistorySnapshotRequest {
            schema_version: 1,
            provider_context: context.clone(),
            conversation_id: "private-conversation".into(),
            selected_node: Some("private-branch".into()),
        };
        let list = ListHistoryPageRequest {
            schema_version: 1,
            provider_context: context,
            offset: 0,
            limit: 20,
            archived: false,
        };
        for diagnostic in [format!("{load:?}"), format!("{list:#?}")] {
            for private in [
                "PRIVATE_TOKEN",
                "secret-marker",
                "private-profile",
                "private-conversation",
                "private-branch",
            ] {
                assert!(!diagnostic.contains(private));
            }
        }
        // Redaction is diagnostic-only; the authorized service still receives context.
        let wire = serde_json::to_vec(&load).unwrap();
        let decoded: LoadHistorySnapshotRequest = serde_json::from_slice(&wire).unwrap();
        assert_eq!(decoded.provider_context, load.provider_context);
        assert_eq!(decoded.conversation_id, load.conversation_id);
        assert_eq!(decoded.selected_node, load.selected_node);
        let mut legacy = serde_json::to_value(&load).unwrap();
        legacy.as_object_mut().unwrap().remove("selected_node");
        let decoded: LoadHistorySnapshotRequest = serde_json::from_value(legacy).unwrap();
        assert!(decoded.selected_node.is_none());
    }
}

/// Optional operation on model-provider interfaces; unsupported providers reject it.
/// Successful responses use `bcode_session_import::ImportableHistorySnapshot`.
pub const OP_LOAD_HISTORY_SNAPSHOT: &str = "load_history_snapshot";

/// Optional metadata payload accompanying a `history_rate_limited` service error.
///
/// The service error remains authoritative: this payload is never a successful
/// page or snapshot. Consumers must ignore unsupported metadata versions and use
/// their normal backoff when the delay is absent; absence does not mean zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRateLimitDetails {
    /// Metadata compatibility version (1).
    pub schema_version: u32,
    /// Provider-requested minimum delay in seconds, when available.
    pub retry_after_seconds: Option<u64>,
}

/// Optional bounded discovery operation; unsupported providers reject it.
pub const OP_LIST_HISTORY_PAGE: &str = "list_history_page";

/// Discover one page using the same explicit-profile rules as snapshot retrieval.
#[derive(Clone, Serialize, Deserialize)]
pub struct ListHistoryPageRequest {
    /// Wire compatibility version (1).
    pub schema_version: u32,
    /// Canonically resolved explicit profile; no pool fallback.
    pub provider_context: crate::ProviderRequestContext,
    /// Source offset. Callers must overlap and reconcile changing pages.
    pub offset: u64,
    /// Per-operation item budget, validated by the adapter.
    pub limit: u16,
    /// Request archived rather than ordinary conversations.
    pub archived: bool,
}

/// Metadata-only discovery result; not imported or searchable content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPageEntry {
    /// Opaque API identity within a separately verified remote account scope.
    pub conversation_id: String,
    /// Source title, not trusted instructions.
    pub title: Option<String>,
    /// Source update timestamp in seconds, not an exclusive incremental cursor.
    pub updated_at: Option<f64>,
}

/// One bounded page. Exhaustion does not establish archive/project coverage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListHistoryPageResponse {
    /// Wire compatibility version (1).
    pub schema_version: u32,
    /// Metadata-only entries in source order.
    pub entries: Vec<HistoryPageEntry>,
    /// Continuation offset; absence only indicates this scan returned a short page.
    pub next_offset: Option<u64>,
}

/// Request one complete selected-branch snapshot without publishing it.
///
/// Version 1 requires one explicitly resolved auth profile, without an auth pool
/// or fallback candidates. Credentials remain request-scoped and must not be
/// persisted with imported content. This operation neither verifies durable
/// remote account identity nor authorizes canonical publication by itself.
#[derive(Clone, Serialize, Deserialize)]
pub struct LoadHistorySnapshotRequest {
    /// Wire compatibility version. Unsupported versions must fail before retrieval.
    pub schema_version: u32,
    /// Canonically resolved provider and explicit profile selection.
    pub provider_context: crate::ProviderRequestContext,
    /// Opaque API conversation identifier, not a website URL or converted web ID.
    pub conversation_id: String,
    /// Optional source graph leaf. Missing nodes fail rather than selecting another branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_node: Option<String>,
}
