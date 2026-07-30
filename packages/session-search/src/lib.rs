#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Backend-neutral contracts for session-search provider plugins.
//!
//! These types describe session-domain search semantics, provider capabilities, coverage, and
//! derived-record ingestion without exposing a backend query language, schema, score type, or
//! pagination token. Providers never receive canonical storage paths or raw session event payloads.

pub mod projection;

use bcode_session_models::{SessionId, SessionInspectionCategory};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Versioned plugin service interface for session-search providers.
pub const SESSION_SEARCH_INTERFACE_ID: &str = "bcode.session_search/v1";

/// Query a provider for bounded search hits.
pub const OP_SEARCH: &str = "search";
/// Return provider capabilities.
pub const OP_CAPABILITIES: &str = "capabilities";
/// Return provider state, freshness, and coverage.
pub const OP_STATUS: &str = "status";
/// Apply an idempotent bounded batch of derived search records.
pub const OP_APPLY_BATCH: &str = "apply_batch";
/// Remove all derived records for one canonical session.
pub const OP_REMOVE_SESSION: &str = "remove_session";
/// Explicitly purge provider-owned derived state.
pub const OP_PURGE: &str = "purge";

/// Current terminal-text normalization algorithm version.
pub const CURRENT_NORMALIZATION_VERSION: u16 = 1;
/// Current allowlisted search projection policy version.
pub const CURRENT_SEARCH_POLICY_VERSION: u16 = 1;
/// Default maximum normalized text bytes projected from one canonical event.
pub const DEFAULT_MAX_TEXT_BYTES_PER_RECORD: usize = 64 * 1024;
/// Current search-record projection contract version.
pub const CURRENT_SEARCH_RECORD_VERSION: u16 = 1;
/// Maximum clauses in one provider-neutral query tree.
pub const MAX_QUERY_CLAUSES: usize = 64;
/// Maximum UTF-8 bytes in one text query clause.
pub const MAX_QUERY_TEXT_BYTES: usize = 4 * 1024;
/// Maximum requested hits from one provider.
pub const MAX_SEARCH_HITS: usize = 200;
/// Maximum UTF-8 bytes in one returned preview.
pub const MAX_HIT_PREVIEW_BYTES: usize = 4 * 1024;
/// Maximum records in one ingestion batch.
pub const MAX_INGEST_RECORDS: usize = 256;
/// Maximum normalized text bytes in one ingestion batch.
pub const MAX_INGEST_TEXT_BYTES: usize = 4 * 1024 * 1024;
/// Maximum UTF-8 bytes in one opaque cursor.
pub const MAX_CURSOR_BYTES: usize = 2 * 1024;

/// Errors produced while validating portable search requests and batches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractValidationError {
    /// Text or cursor field is empty when content is required.
    EmptyField(&'static str),
    /// One bounded collection or string exceeds its portable maximum.
    LimitExceeded {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    /// A query tree contains no effective text predicate.
    EmptyQuery,
    /// A query tree exceeds the maximum nesting depth.
    QueryDepthExceeded { maximum: usize },
    /// Search projection policy is internally inconsistent or unsupported.
    InvalidProjection(&'static str),
    /// Batch identities or checkpoints do not agree.
    InvalidBatch(&'static str),
}

impl std::fmt::Display for ContractValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "{field} must not be empty"),
            Self::LimitExceeded {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "{field} size {actual} exceeds portable maximum {maximum}"
            ),
            Self::EmptyQuery => formatter.write_str("search query has no text predicate"),
            Self::QueryDepthExceeded { maximum } => {
                write!(formatter, "search query nesting exceeds maximum {maximum}")
            }
            Self::InvalidProjection(message) => {
                write!(formatter, "invalid search projection: {message}")
            }
            Self::InvalidBatch(message) => write!(formatter, "invalid search batch: {message}"),
        }
    }
}

impl std::error::Error for ContractValidationError {}

/// Semantic content category that may be projected and searched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchContentKind {
    SessionTitle,
    UserMessage,
    AssistantMessage,
    AssistantReasoning,
    SystemMessage,
    ShellCommand,
    ShellOutput,
    ToolArguments,
    ToolOutput,
    ToolError,
    Permission,
    RuntimeDiagnostic,
    Compaction,
    TraceMetadata,
    ArtifactMetadata,
}

/// Stable semantic field that matched inside one derived record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchField {
    Title,
    Text,
    Command,
    StandardOutput,
    StandardError,
    ToolName,
    ToolArguments,
    ErrorMessage,
    WorkingDirectory,
    Provider,
    Model,
    Agent,
    Source,
}

/// Portable matching behavior for one text clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMatchMode {
    Terms,
    Phrase,
    Prefix,
    Regex,
    Fuzzy,
}

/// Backend-neutral bounded query tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionSearchQuery {
    Text {
        text: String,
        mode: TextMatchMode,
        #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
        fields: BTreeSet<SearchField>,
    },
    And {
        clauses: Vec<Self>,
    },
    Or {
        clauses: Vec<Self>,
    },
    Not {
        clause: Box<Self>,
    },
}

impl SessionSearchQuery {
    fn validate(
        &self,
        depth: usize,
        clause_count: &mut usize,
    ) -> Result<(), ContractValidationError> {
        const MAX_DEPTH: usize = 16;
        if depth > MAX_DEPTH {
            return Err(ContractValidationError::QueryDepthExceeded { maximum: MAX_DEPTH });
        }
        *clause_count = clause_count.saturating_add(1);
        if *clause_count > MAX_QUERY_CLAUSES {
            return Err(ContractValidationError::LimitExceeded {
                field: "query_clauses",
                actual: *clause_count,
                maximum: MAX_QUERY_CLAUSES,
            });
        }
        match self {
            Self::Text { text, .. } => {
                validate_nonempty_bounded("query_text", text, MAX_QUERY_TEXT_BYTES)
            }
            Self::And { clauses } | Self::Or { clauses } => {
                if clauses.is_empty() {
                    return Err(ContractValidationError::EmptyQuery);
                }
                for clause in clauses {
                    clause.validate(depth.saturating_add(1), clause_count)?;
                }
                Ok(())
            }
            Self::Not { clause } => clause.validate(depth.saturating_add(1), clause_count),
        }
    }
}

/// Stable message role filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMessageRole {
    User,
    Assistant,
    System,
    Reasoning,
}

/// Structured filters interpreted using normalized session semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchFilters {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub session_ids: BTreeSet<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_timestamp_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_timestamp_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub content_kinds: BTreeSet<SearchContentKind>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub roles: BTreeSet<SearchMessageRole>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub inspection_categories: BTreeSet<SessionInspectionCategory>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tool_names: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tool_statuses: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub providers: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub models: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub agents: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub sources: BTreeSet<String>,
}

/// Deterministic portable result ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSearchSort {
    #[default]
    ProviderRelevance,
    NewestFirst,
    OldestFirst,
    SessionThenSequence,
}

/// Opaque provider cursor scoped to one provider and query fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchCursor {
    pub provider_id: String,
    pub query_fingerprint: String,
    pub value: String,
}

/// Bounded provider search request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchRequest {
    pub query: SessionSearchQuery,
    #[serde(default)]
    pub filters: SessionSearchFilters,
    #[serde(default)]
    pub sort: SessionSearchSort,
    pub limit: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<SearchCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

impl SessionSearchRequest {
    /// Validate portable query and payload limits before provider invocation.
    ///
    /// # Errors
    ///
    /// Returns an error for empty queries, excessive query trees/text, excessive hit limits, or
    /// oversized cursors.
    pub fn validate(&self) -> Result<(), ContractValidationError> {
        let mut clauses = 0;
        self.query.validate(0, &mut clauses)?;
        if self.limit == 0 || self.limit > MAX_SEARCH_HITS {
            return Err(ContractValidationError::LimitExceeded {
                field: "limit",
                actual: self.limit,
                maximum: MAX_SEARCH_HITS,
            });
        }
        if let Some(cursor) = &self.cursor {
            validate_nonempty_bounded("cursor_provider_id", &cursor.provider_id, MAX_CURSOR_BYTES)?;
            validate_nonempty_bounded(
                "cursor_query_fingerprint",
                &cursor.query_fingerprint,
                MAX_CURSOR_BYTES,
            )?;
            validate_nonempty_bounded("cursor_value", &cursor.value, MAX_CURSOR_BYTES)?;
        }
        Ok(())
    }
}

/// Stable canonical locator returned by every search provider.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionSearchLocator {
    pub session_id: SessionId,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
}

/// One bounded provider hit. `provider_score` is opaque and comparable only within this provider's
/// response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSearchHit {
    pub locator: SessionSearchLocator,
    pub content_kind: SearchContentKind,
    pub matched_field: SearchField,
    pub provider_id: String,
    pub provider_rank: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default)]
    pub preview_truncated: bool,
}

/// Search execution behavior advertised by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchExecutionKind {
    Indexed,
    Scan,
    Remote,
}

/// Backend-neutral optional feature advertised by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchFeature {
    Terms,
    Phrase,
    Prefix,
    Regex,
    Fuzzy,
    StructuredFilters,
    Highlighting,
    RelevanceSort,
    IncrementalIngestion,
    HistoricalBackfill,
    RemoveSession,
    Purge,
}

/// Provider capability response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchCapabilities {
    pub provider_id: String,
    pub execution: SearchExecutionKind,
    pub content_kinds: BTreeSet<SearchContentKind>,
    pub features: BTreeSet<SearchFeature>,
    pub max_hits: usize,
    pub max_batch_records: usize,
    pub max_batch_text_bytes: usize,
}

/// Provider lifecycle/degraded state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchProviderState {
    Ready,
    CatchingUp,
    Rebuilding,
    Degraded,
    QuotaExceeded,
    Corrupt,
    Disabled,
}

/// Trustworthy canonical generation associated with one provider checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchCanonicalGeneration {
    pub session_id: SessionId,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sequence: Option<u64>,
}

/// Provider coverage for one session/content set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchCoverage {
    pub generation: SearchCanonicalGeneration,
    pub content_kinds: BTreeSet<SearchContentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_through_sequence: Option<u64>,
    pub complete: bool,
    #[serde(default)]
    pub skipped_records: u64,
    #[serde(default)]
    pub truncated_records: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusions: Vec<String>,
}

/// Provider status response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchStatus {
    pub provider_id: String,
    pub state: SearchProviderState,
    pub record_schema_version: u16,
    pub normalization_version: u16,
    pub policy_version: u16,
    pub index_bytes: u64,
    pub quota_bytes: u64,
    pub pending_sessions: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<SessionSearchCoverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
}

/// One discovered provider with normalized capabilities and current status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchProviderInfo {
    /// Plugin registration selected for this provider.
    pub plugin_id: String,
    pub capabilities: SessionSearchCapabilities,
    pub status: SessionSearchStatus,
}

/// One provider that was discovered but could not report usable typed state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchProviderFailure {
    pub plugin_id: String,
    pub error: SessionSearchServiceError,
}

/// Bounded application-level provider discovery response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSessionSearchProvidersResponse {
    pub providers: Vec<SessionSearchProviderInfo>,
    pub failures: Vec<SessionSearchProviderFailure>,
}

/// Provider execution status for one terminal search response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSearchOutcome {
    Complete,
    Partial,
    TimedOut,
    Cancelled,
    ConflictingDuplicate,
    Unsupported,
    Stale,
    Degraded,
}

/// Bounded terminal provider response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSearchResponse {
    pub provider_id: String,
    pub outcome: ProviderSearchOutcome,
    pub hits: Vec<SessionSearchHit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<SearchCursor>,
    pub query_complete: bool,
    pub coverage_complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub searched_content: Vec<SearchContentKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_content: Vec<SearchContentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Backend-neutral normalized record projected from finalized canonical semantic state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchRecord {
    pub schema_version: u16,
    pub record_id: String,
    pub locator: SessionSearchLocator,
    pub timestamp_ms: u64,
    pub content_kind: SearchContentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<SearchField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
    pub source_bytes: u64,
    pub normalized_bytes: u64,
    pub indexed_bytes: u64,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_range_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_range_end: Option<u64>,
    pub normalization_version: u16,
    pub policy_version: u16,
}

/// Idempotent bounded batch of derived search records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplySearchRecordsRequest {
    pub provider_id: String,
    pub batch_id: String,
    pub generation: SearchCanonicalGeneration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_previous_sequence: Option<u64>,
    pub records: Vec<SessionSearchRecord>,
}

impl ApplySearchRecordsRequest {
    /// Return a stable digest of canonical operation facts for conflicting-duplicate detection.
    ///
    /// # Panics
    ///
    /// Panics only if serializing this owned portable request unexpectedly fails.
    #[must_use]
    pub fn operation_digest_sha256(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("serializing owned search batch cannot fail");
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Validate record count, byte limits, identities, and monotonic checkpoint facts.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch is empty/oversized, identities conflict, text exceeds the
    /// aggregate bound, or record sequences are not monotonic and within the declared generation.
    pub fn validate(&self) -> Result<(), ContractValidationError> {
        validate_nonempty_bounded("provider_id", &self.provider_id, MAX_CURSOR_BYTES)?;
        validate_nonempty_bounded("batch_id", &self.batch_id, MAX_CURSOR_BYTES)?;
        validate_nonempty_bounded(
            "generation_fingerprint",
            &self.generation.fingerprint,
            MAX_CURSOR_BYTES,
        )?;
        if self.records.is_empty() {
            return Err(ContractValidationError::InvalidBatch(
                "records must not be empty",
            ));
        }
        if self.records.len() > MAX_INGEST_RECORDS {
            return Err(ContractValidationError::LimitExceeded {
                field: "records",
                actual: self.records.len(),
                maximum: MAX_INGEST_RECORDS,
            });
        }
        let mut text_bytes = 0usize;
        let mut previous = self.expected_previous_sequence;
        let mut identities = BTreeSet::new();
        for record in &self.records {
            if record.schema_version != CURRENT_SEARCH_RECORD_VERSION {
                return Err(ContractValidationError::InvalidBatch(
                    "record schema version is unsupported",
                ));
            }
            if record.locator.session_id != self.generation.session_id {
                return Err(ContractValidationError::InvalidBatch(
                    "record session differs from generation session",
                ));
            }
            if !identities.insert(record.record_id.as_str()) {
                return Err(ContractValidationError::InvalidBatch(
                    "duplicate record identity in batch",
                ));
            }
            if previous.is_some_and(|sequence| record.locator.sequence <= sequence) {
                return Err(ContractValidationError::InvalidBatch(
                    "record sequences must advance monotonically",
                ));
            }
            if self
                .generation
                .last_sequence
                .is_some_and(|tail| record.locator.sequence > tail)
            {
                return Err(ContractValidationError::InvalidBatch(
                    "record sequence exceeds declared generation tail",
                ));
            }
            previous = Some(record.locator.sequence);
            text_bytes = text_bytes.saturating_add(record.text.as_ref().map_or(0, String::len));
        }
        if text_bytes > MAX_INGEST_TEXT_BYTES {
            return Err(ContractValidationError::LimitExceeded {
                field: "record_text_bytes",
                actual: text_bytes,
                maximum: MAX_INGEST_TEXT_BYTES,
            });
        }
        Ok(())
    }
}

/// Durable provider acknowledgment after idempotent batch publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplySearchRecordsResponse {
    pub batch_id: String,
    pub outcome: ApplyBatchOutcome,
    pub applied_records: usize,
    pub indexed_through_sequence: u64,
}

/// Explicit duplicate-delivery outcome for one ingestion batch identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyBatchOutcome {
    Applied,
    Duplicate,
    ConflictingDuplicate,
}

/// Remove one session's provider-owned derived records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveSessionSearchRequest {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation_fingerprint: Option<String>,
}

/// Explicit provider-owned derived-state purge request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeSessionSearchRequest {
    pub provider_id: String,
    pub confirmation: String,
}

/// Typed provider-owned service error returned inside a successful plugin transport response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchServiceError {
    pub code: SearchErrorCode,
    pub message: String,
    pub retryable: bool,
}

/// Normalized session-search provider error classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchErrorCode {
    ProviderUnavailable,
    UnsupportedQuery,
    InvalidRequest,
    DeadlineExceeded,
    Cancelled,
    StaleIndex,
    QuotaExceeded,
    CorruptIndex,
    FutureVersion,
    ConflictingDuplicate,
    Internal,
}

fn validate_nonempty_bounded(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), ContractValidationError> {
    if value.trim().is_empty() {
        return Err(ContractValidationError::EmptyField(field));
    }
    if value.len() > maximum {
        return Err(ContractValidationError::LimitExceeded {
            field,
            actual: value.len(),
            maximum,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_query(text: &str) -> SessionSearchQuery {
        SessionSearchQuery::Text {
            text: text.to_owned(),
            mode: TextMatchMode::Terms,
            fields: BTreeSet::new(),
        }
    }

    #[test]
    fn request_round_trips_and_validates_without_backend_types() {
        let request = SessionSearchRequest {
            query: SessionSearchQuery::And {
                clauses: vec![
                    text_query("database locked"),
                    SessionSearchQuery::Not {
                        clause: Box::new(text_query("unrelated")),
                    },
                ],
            },
            filters: SessionSearchFilters {
                content_kinds: [SearchContentKind::UserMessage, SearchContentKind::ToolError]
                    .into_iter()
                    .collect(),
                ..SessionSearchFilters::default()
            },
            sort: SessionSearchSort::NewestFirst,
            limit: 20,
            cursor: None,
            deadline_ms: Some(1_000),
        };
        request.validate().expect("valid request");
        let encoded = serde_json::to_vec(&request).expect("encode request");
        let decoded: SessionSearchRequest =
            serde_json::from_slice(&encoded).expect("decode request");
        assert_eq!(decoded, request);
    }

    #[test]
    fn unsupported_query_features_remain_explicit_capabilities() {
        let capabilities = SessionSearchCapabilities {
            provider_id: "bcode.example-search".to_owned(),
            execution: SearchExecutionKind::Indexed,
            content_kinds: std::iter::once(SearchContentKind::UserMessage).collect(),
            features: [SearchFeature::Terms, SearchFeature::Phrase]
                .into_iter()
                .collect(),
            max_hits: 100,
            max_batch_records: 128,
            max_batch_text_bytes: 1_048_576,
        };
        assert!(!capabilities.features.contains(&SearchFeature::Regex));
        assert!(!capabilities.features.contains(&SearchFeature::Fuzzy));
    }

    #[test]
    fn batch_validation_rejects_conflicting_duplicate_identity() {
        let session_id = SessionId::new();
        let record = SessionSearchRecord {
            schema_version: CURRENT_SEARCH_RECORD_VERSION,
            record_id: "record-1".to_owned(),
            locator: SessionSearchLocator {
                session_id,
                sequence: 1,
                record_id: Some("record-1".to_owned()),
            },
            timestamp_ms: 1,
            content_kind: SearchContentKind::UserMessage,
            field: Some(SearchField::Text),
            text: Some("hello".to_owned()),
            attributes: BTreeMap::new(),
            source_bytes: 5,
            normalized_bytes: 5,
            indexed_bytes: 5,
            truncated: false,
            source_range_start: Some(0),
            source_range_end: Some(5),
            normalization_version: 1,
            policy_version: 1,
        };
        let request = ApplySearchRecordsRequest {
            provider_id: "provider".to_owned(),
            batch_id: "batch-1".to_owned(),
            generation: SearchCanonicalGeneration {
                session_id,
                fingerprint: "generation".to_owned(),
                last_sequence: Some(2),
            },
            expected_previous_sequence: Some(0),
            records: vec![
                record.clone(),
                SessionSearchRecord {
                    locator: SessionSearchLocator {
                        sequence: 2,
                        ..record.locator.clone()
                    },
                    ..record
                },
            ],
        };
        assert!(matches!(
            request.validate(),
            Err(ContractValidationError::InvalidBatch(
                "duplicate record identity in batch"
            ))
        ));
    }

    #[test]
    fn unknown_future_enum_variant_is_not_guessed() {
        let error = serde_json::from_str::<SearchProviderState>("\"future_ready\"")
            .expect_err("future state must not decode as known");
        assert!(error.to_string().contains("unknown variant"));
    }
}
