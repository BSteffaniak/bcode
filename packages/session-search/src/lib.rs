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

/// Maximum providers invoked concurrently for one federated search.
pub const MAX_FEDERATED_PROVIDERS: usize = 8;
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
/// Maximum total UTF-8 payload bytes in one ingestion batch, including record metadata.
pub const MAX_INGEST_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
/// Maximum cumulative normalized text bytes accepted for one canonical session.
pub const MAX_SESSION_INGEST_TEXT_BYTES: u64 = 256 * 1024 * 1024;
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

    /// Return the provider features required to execute this request exactly.
    #[must_use]
    pub fn required_features(&self) -> BTreeSet<SearchFeature> {
        let mut features = BTreeSet::new();
        collect_query_features(&self.query, &mut features);
        if self.filters != SessionSearchFilters::default() {
            features.insert(SearchFeature::StructuredFilters);
        }
        if self.sort == SessionSearchSort::ProviderRelevance {
            features.insert(SearchFeature::RelevanceSort);
        }
        features
    }
}

fn collect_query_features(query: &SessionSearchQuery, features: &mut BTreeSet<SearchFeature>) {
    match query {
        SessionSearchQuery::Text { mode, .. } => {
            features.insert(match mode {
                TextMatchMode::Terms => SearchFeature::Terms,
                TextMatchMode::Phrase => SearchFeature::Phrase,
                TextMatchMode::Prefix => SearchFeature::Prefix,
                TextMatchMode::Regex => SearchFeature::Regex,
                TextMatchMode::Fuzzy => SearchFeature::Fuzzy,
            });
        }
        SessionSearchQuery::And { clauses } | SessionSearchQuery::Or { clauses } => {
            for clause in clauses {
                collect_query_features(clause, features);
            }
        }
        SessionSearchQuery::Not { clause } => collect_query_features(clause, features),
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchHit {
    pub locator: SessionSearchLocator,
    pub content_kind: SearchContentKind,
    pub matched_field: SearchField,
    pub provider_id: String,
    pub provider_rank: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_score: Option<String>,
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

impl SessionSearchCapabilities {
    /// Validate advertised identities and portable contract limits.
    ///
    /// # Errors
    ///
    /// Returns an error when identity, content coverage, or advertised limits are empty or exceed
    /// the portable contract maxima.
    pub fn validate(&self) -> Result<(), ContractValidationError> {
        validate_nonempty_bounded("provider_id", &self.provider_id, MAX_CURSOR_BYTES)?;
        if self.content_kinds.is_empty() {
            return Err(ContractValidationError::EmptyField("content_kinds"));
        }
        validate_positive_limit("max_hits", self.max_hits, MAX_SEARCH_HITS)?;
        validate_positive_limit(
            "max_batch_records",
            self.max_batch_records,
            MAX_INGEST_RECORDS,
        )?;
        validate_positive_limit(
            "max_batch_text_bytes",
            self.max_batch_text_bytes,
            MAX_INGEST_TEXT_BYTES,
        )?;
        Ok(())
    }

    /// Validate that this provider can execute a request without approximation.
    ///
    /// # Errors
    ///
    /// Returns an error when a query feature, content kind, cursor identity, or requested limit is
    /// unsupported by this provider.
    pub fn supports_request(
        &self,
        request: &SessionSearchRequest,
    ) -> Result<(), ContractValidationError> {
        self.validate()?;
        request.validate()?;
        if request.limit > self.max_hits {
            return Err(ContractValidationError::LimitExceeded {
                field: "provider_max_hits",
                actual: request.limit,
                maximum: self.max_hits,
            });
        }
        if let Some(cursor) = &request.cursor
            && cursor.provider_id != self.provider_id
        {
            return Err(ContractValidationError::InvalidProjection(
                "cursor belongs to another provider",
            ));
        }
        if !request.required_features().is_subset(&self.features) {
            return Err(ContractValidationError::InvalidProjection(
                "provider does not support all requested query features",
            ));
        }
        if !request.filters.content_kinds.is_empty()
            && !request.filters.content_kinds.is_subset(&self.content_kinds)
        {
            return Err(ContractValidationError::InvalidProjection(
                "provider does not cover all requested content kinds",
            ));
        }
        Ok(())
    }
}

const fn validate_positive_limit(
    field: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), ContractValidationError> {
    if actual == 0 || actual > maximum {
        return Err(ContractValidationError::LimitExceeded {
            field,
            actual,
            maximum,
        });
    }
    Ok(())
}

/// Provider lifecycle/degraded state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchProviderState {
    Ready,
    CatchingUp,
    Stale,
    Rebuilding,
    Degraded,
    QuotaExceeded,
    Corrupt,
    Disabled,
    Unavailable,
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

impl SessionSearchStatus {
    /// Validate provider identity and shared projection compatibility.
    ///
    /// # Errors
    ///
    /// Returns an error for empty identity, unsupported shared versions, invalid quota accounting,
    /// or incomplete/degraded states without an explanatory reason.
    pub fn validate(&self) -> Result<(), ContractValidationError> {
        validate_nonempty_bounded("provider_id", &self.provider_id, MAX_CURSOR_BYTES)?;
        if self.record_schema_version != CURRENT_SEARCH_RECORD_VERSION
            || self.normalization_version != CURRENT_NORMALIZATION_VERSION
            || self.policy_version != CURRENT_SEARCH_POLICY_VERSION
        {
            return Err(ContractValidationError::InvalidProjection(
                "provider status uses unsupported projection versions",
            ));
        }
        if self.index_bytes > self.quota_bytes {
            return Err(ContractValidationError::InvalidProjection(
                "provider index bytes exceed declared quota",
            ));
        }
        if matches!(
            self.state,
            SearchProviderState::Stale
                | SearchProviderState::Degraded
                | SearchProviderState::QuotaExceeded
                | SearchProviderState::Corrupt
                | SearchProviderState::Unavailable
        ) && self
            .degraded_reason
            .as_deref()
            .is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(ContractValidationError::EmptyField("degraded_reason"));
        }
        Ok(())
    }
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
    #[serde(default)]
    pub stage: SessionSearchProviderStage,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<SearchContentKind>,
}

/// Point in discovery/planning/execution where a provider did not contribute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSearchProviderStage {
    Discovery,
    Planning,
    #[default]
    Execution,
    Hydration,
}

/// Bounded application-level provider discovery response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSessionSearchProvidersResponse {
    pub providers: Vec<SessionSearchProviderInfo>,
    pub failures: Vec<SessionSearchProviderFailure>,
}

/// Query execution class used for provider routing and deadline policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSearchExecutionClass {
    /// Latency-sensitive search that excludes cold scan providers.
    #[default]
    Ordinary,
    /// Explicit search that may invoke scan providers for large/cold content.
    Deep,
}

/// Backend-neutral policy applied while building a provider query plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchPlanPolicy {
    #[serde(default)]
    pub execution_class: SessionSearchExecutionClass,
    /// Maximum age of an incomplete/catching-up checkpoint accepted for indexed providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_staleness_sequences: Option<u64>,
    /// Deadline for each selected provider, bounded again by the request's overall deadline.
    pub per_provider_deadline_ms: u64,
}

impl Default for SessionSearchPlanPolicy {
    fn default() -> Self {
        Self {
            execution_class: SessionSearchExecutionClass::Ordinary,
            maximum_staleness_sequences: Some(0),
            per_provider_deadline_ms: 2_000,
        }
    }
}

impl SessionSearchPlanPolicy {
    /// Validate plan-level execution and deadline bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when the per-provider deadline is zero or exceeds the request deadline.
    pub fn validate(&self, request: &SessionSearchRequest) -> Result<(), ContractValidationError> {
        let overall = request.deadline_ms.unwrap_or(5_000).max(1);
        if self.per_provider_deadline_ms == 0 || self.per_provider_deadline_ms > overall {
            return Err(ContractValidationError::LimitExceeded {
                field: "per_provider_deadline_ms",
                actual: usize::try_from(self.per_provider_deadline_ms).unwrap_or(usize::MAX),
                maximum: usize::try_from(overall).unwrap_or(usize::MAX),
            });
        }
        Ok(())
    }
}

/// Provider selection behavior for one configured content route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSearchRouteMode {
    /// Select only the first eligible provider in configured order.
    Primary,
    /// Select the first currently eligible provider in configured fallback order.
    Fallback,
    /// Select every eligible configured provider for intentional overlapping coverage.
    Parallel,
    /// Select configured providers only when they add requested content not already routed.
    Disjoint,
}

/// Backend-neutral configured route for a set of semantic content kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchContentRoute {
    pub content_kinds: BTreeSet<SearchContentKind>,
    pub mode: SessionSearchRouteMode,
    /// Provider plugin IDs in explicit route priority order.
    pub provider_ids: Vec<String>,
}

impl SessionSearchContentRoute {
    /// Validate route scope and provider ordering.
    ///
    /// # Errors
    ///
    /// Returns an error for empty content/provider sets, duplicate provider IDs, or oversized IDs.
    pub fn validate(&self) -> Result<(), ContractValidationError> {
        if self.content_kinds.is_empty() {
            return Err(ContractValidationError::EmptyField("route_content_kinds"));
        }
        if self.provider_ids.is_empty() {
            return Err(ContractValidationError::EmptyField("route_provider_ids"));
        }
        let mut unique = BTreeSet::new();
        for provider_id in &self.provider_ids {
            validate_nonempty_bounded("route_provider_id", provider_id, MAX_CURSOR_BYTES)?;
            if !unique.insert(provider_id) {
                return Err(ContractValidationError::InvalidProjection(
                    "route contains a duplicate provider identity",
                ));
            }
        }
        Ok(())
    }
}

/// Deterministic provider plan for one exact portable request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchPlan {
    /// Providers selected in stable plugin-ID order.
    pub providers: Vec<SessionSearchProviderInfo>,
    /// Discovery, state, or capability failures excluded from execution.
    pub failures: Vec<SessionSearchProviderFailure>,
    /// Validated per-provider deadline applied by the coordinator.
    pub per_provider_deadline_ms: u64,
}

/// Build a conservative query plan without broadcasting to redundant overlapping providers.
///
/// The first eligible provider for each requested content kind wins in stable plugin-ID order.
/// Providers with disjoint requested coverage may therefore execute together, while redundant
/// overlap is excluded. Empty content filters select one exact-capability provider.
#[must_use]
pub fn plan_session_search(
    request: &SessionSearchRequest,
    discovery: ListSessionSearchProvidersResponse,
) -> SessionSearchPlan {
    plan_session_search_with_policy_and_routes(
        request,
        discovery,
        &SessionSearchPlanPolicy::default(),
        &[],
    )
}

/// Build a deterministic query plan using explicit content routes.
#[must_use]
pub fn plan_session_search_with_routes(
    request: &SessionSearchRequest,
    discovery: ListSessionSearchProvidersResponse,
    routes: &[SessionSearchContentRoute],
) -> SessionSearchPlan {
    plan_session_search_with_policy_and_routes(
        request,
        discovery,
        &SessionSearchPlanPolicy::default(),
        routes,
    )
}

/// Build a deterministic query plan using explicit execution/freshness policy and content routes.
#[must_use]
pub fn plan_session_search_with_policy_and_routes(
    request: &SessionSearchRequest,
    mut discovery: ListSessionSearchProvidersResponse,
    policy: &SessionSearchPlanPolicy,
    routes: &[SessionSearchContentRoute],
) -> SessionSearchPlan {
    if let Err(error) = policy.validate(request) {
        discovery.failures.push(planning_failure(
            "policy".to_owned(),
            SearchErrorCode::InvalidRequest,
            &error.to_string(),
        ));
        return SessionSearchPlan {
            providers: Vec::new(),
            failures: discovery.failures,
            per_provider_deadline_ms: policy.per_provider_deadline_ms,
        };
    }
    discovery = filter_discovery_for_policy(discovery, policy);
    let mut plan = if routes.is_empty() {
        plan_session_search_default(request, discovery)
    } else {
        plan_session_search_routed(request, discovery, routes)
    };
    plan.per_provider_deadline_ms = policy.per_provider_deadline_ms;
    plan
}

fn plan_session_search_routed(
    request: &SessionSearchRequest,
    discovery: ListSessionSearchProvidersResponse,
    routes: &[SessionSearchContentRoute],
) -> SessionSearchPlan {
    let mut available = discovery
        .providers
        .into_iter()
        .map(|provider| (provider.plugin_id.clone(), provider))
        .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeMap::new();
    let mut failures = discovery.failures;
    let requested = &request.filters.content_kinds;
    let mut routed_content = BTreeSet::new();

    for route in routes {
        if let Err(error) = route.validate() {
            failures.push(planning_failure_with_content(
                "route".to_owned(),
                SearchErrorCode::InvalidRequest,
                &error.to_string(),
                Vec::new(),
            ));
            continue;
        }
        let applicable_content = if requested.is_empty() {
            route.content_kinds.clone()
        } else {
            route
                .content_kinds
                .intersection(requested)
                .copied()
                .collect()
        };
        if applicable_content.is_empty() {
            continue;
        }
        let mut eligible = Vec::new();
        for provider_id in &route.provider_ids {
            let Some(provider) = available.get(provider_id) else {
                failures.push(planning_failure(
                    provider_id.clone(),
                    SearchErrorCode::ProviderUnavailable,
                    "configured route provider is unavailable",
                ));
                continue;
            };
            let provider_content = applicable_content
                .intersection(&provider.capabilities.content_kinds)
                .copied()
                .collect::<BTreeSet<_>>();
            if provider_content.is_empty()
                || !provider_is_eligible(request, provider, &provider_content)
            {
                continue;
            }
            eligible.push((provider_id.clone(), provider_content));
        }
        match route.mode {
            SessionSearchRouteMode::Primary | SessionSearchRouteMode::Fallback => {
                if let Some((provider_id, content)) = eligible.into_iter().next() {
                    routed_content.extend(content);
                    if let Some(provider) = available.remove(&provider_id) {
                        selected.insert(provider_id, provider);
                    }
                }
            }
            SessionSearchRouteMode::Parallel => {
                for (provider_id, content) in eligible {
                    routed_content.extend(content);
                    if let Some(provider) = available.remove(&provider_id) {
                        selected.insert(provider_id, provider);
                    }
                }
            }
            SessionSearchRouteMode::Disjoint => {
                for (provider_id, content) in eligible {
                    let added = content
                        .difference(&routed_content)
                        .copied()
                        .collect::<BTreeSet<_>>();
                    if !added.is_empty() {
                        routed_content.extend(added);
                        if let Some(provider) = available.remove(&provider_id) {
                            selected.insert(provider_id, provider);
                        }
                    }
                }
            }
        }
    }
    failures.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    SessionSearchPlan {
        providers: selected.into_values().collect(),
        failures,
        per_provider_deadline_ms: 0,
    }
}

fn provider_is_eligible(
    request: &SessionSearchRequest,
    provider: &SessionSearchProviderInfo,
    content: &BTreeSet<SearchContentKind>,
) -> bool {
    if !matches!(
        provider.status.state,
        SearchProviderState::Ready
            | SearchProviderState::CatchingUp
            | SearchProviderState::Degraded
    ) {
        return false;
    }
    let mut provider_request = request.clone();
    provider_request.filters.content_kinds.clone_from(content);
    provider
        .capabilities
        .supports_request(&provider_request)
        .is_ok()
}

fn plan_session_search_default(
    request: &SessionSearchRequest,
    discovery: ListSessionSearchProvidersResponse,
) -> SessionSearchPlan {
    let mut providers = discovery.providers;
    providers.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    let mut selected = Vec::new();
    let mut failures = discovery.failures;
    failures.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    let mut uncovered = request.filters.content_kinds.clone();

    for provider in providers {
        if !matches!(
            provider.status.state,
            SearchProviderState::Ready
                | SearchProviderState::CatchingUp
                | SearchProviderState::Degraded
        ) {
            failures.push(planning_failure(
                provider.plugin_id,
                SearchErrorCode::ProviderUnavailable,
                "provider state is not queryable",
            ));
            continue;
        }
        let mut provider_request = request.clone();
        if !request.filters.content_kinds.is_empty() {
            provider_request.filters.content_kinds = request
                .filters
                .content_kinds
                .intersection(&provider.capabilities.content_kinds)
                .copied()
                .collect();
            if provider_request.filters.content_kinds.is_empty() {
                continue;
            }
        }
        if let Err(error) = provider.capabilities.supports_request(&provider_request) {
            failures.push(planning_failure(
                provider.plugin_id,
                SearchErrorCode::UnsupportedQuery,
                &error.to_string(),
            ));
            continue;
        }
        if uncovered.is_empty() {
            if request.filters.content_kinds.is_empty() && selected.is_empty() {
                selected.push(provider);
            }
            continue;
        }
        let covered = uncovered
            .intersection(&provider.capabilities.content_kinds)
            .copied()
            .collect::<Vec<_>>();
        if !covered.is_empty() {
            for kind in covered {
                uncovered.remove(&kind);
            }
            selected.push(provider);
        }
    }
    failures.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    SessionSearchPlan {
        providers: selected,
        failures,
        per_provider_deadline_ms: 0,
    }
}

fn filter_discovery_for_policy(
    discovery: ListSessionSearchProvidersResponse,
    policy: &SessionSearchPlanPolicy,
) -> ListSessionSearchProvidersResponse {
    let mut providers = Vec::new();
    let mut failures = discovery.failures;
    for provider in discovery.providers {
        let exclusion = if matches!(provider.status.state, SearchProviderState::Stale) {
            Some((
                SearchErrorCode::StaleIndex,
                "provider index is explicitly stale",
            ))
        } else if matches!(provider.status.state, SearchProviderState::Unavailable) {
            Some((
                SearchErrorCode::ProviderUnavailable,
                "provider is configured but unavailable",
            ))
        } else if matches!(
            policy.execution_class,
            SessionSearchExecutionClass::Ordinary
        ) && matches!(provider.capabilities.execution, SearchExecutionKind::Scan)
        {
            Some((
                SearchErrorCode::UnsupportedQuery,
                "cold scan provider requires explicit deep search",
            ))
        } else if coverage_exceeds_staleness(&provider.status, policy.maximum_staleness_sequences) {
            Some((
                SearchErrorCode::StaleIndex,
                "provider coverage exceeds the configured freshness threshold",
            ))
        } else {
            None
        };
        if let Some((code, message)) = exclusion {
            failures.push(planning_failure(provider.plugin_id, code, message));
        } else {
            providers.push(provider);
        }
    }
    ListSessionSearchProvidersResponse {
        providers,
        failures,
    }
}

fn coverage_exceeds_staleness(
    status: &SessionSearchStatus,
    maximum_staleness_sequences: Option<u64>,
) -> bool {
    let Some(maximum) = maximum_staleness_sequences else {
        return false;
    };
    status.coverage.iter().any(|coverage| {
        coverage.generation.last_sequence.is_some_and(|tail| {
            let indexed = coverage.indexed_through_sequence.unwrap_or_default();
            tail.saturating_sub(indexed) > maximum
        })
    })
}

fn planning_failure(
    plugin_id: String,
    code: SearchErrorCode,
    message: &str,
) -> SessionSearchProviderFailure {
    planning_failure_with_content(plugin_id, code, message, Vec::new())
}

fn planning_failure_with_content(
    plugin_id: String,
    code: SearchErrorCode,
    message: &str,
    content: Vec<SearchContentKind>,
) -> SessionSearchProviderFailure {
    SessionSearchProviderFailure {
        plugin_id,
        error: SessionSearchServiceError {
            code,
            message: message.to_owned(),
            retryable: false,
        },
        stage: SessionSearchProviderStage::Planning,
        elapsed_ms: 0,
        content,
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// One provider's terminal contribution to a federated search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederatedProviderReport {
    pub provider_id: String,
    pub outcome: ProviderSearchOutcome,
    pub elapsed_ms: u64,
    pub query_complete: bool,
    pub coverage_complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub searched_content: Vec<SearchContentKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_content: Vec<SearchContentKind>,
}

/// One validated provider contribution awaiting deterministic federation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederatedProviderContribution {
    pub report: FederatedProviderReport,
    pub hits: Vec<SessionSearchHit>,
}

/// Deterministically aggregate grouped provider contributions and explicit failures.
///
/// Contributions are sorted by provider identity, hits by provider-local rank, and duplicate
/// canonical locators retain the first provider contribution. Provider scores are never compared.
#[must_use]
pub fn aggregate_federated_search(
    mut contributions: Vec<FederatedProviderContribution>,
    mut failures: Vec<SessionSearchProviderFailure>,
    limit: usize,
) -> FederatedSessionSearchResponse {
    contributions.sort_by(|left, right| left.report.provider_id.cmp(&right.report.provider_id));
    failures.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    let mut seen = BTreeSet::new();
    let mut hits = Vec::new();
    let mut providers = Vec::new();
    for mut contribution in contributions {
        contribution.hits.sort_by_key(|hit| hit.provider_rank);
        for hit in contribution.hits {
            if hits.len() == limit {
                break;
            }
            if seen.insert(hit.locator.clone()) {
                hits.push(hit);
            }
        }
        providers.push(contribution.report);
    }
    let query_complete = failures.is_empty()
        && !providers.is_empty()
        && providers.iter().all(|provider| provider.query_complete);
    let coverage_complete = query_complete
        && providers.iter().all(|provider| {
            provider.coverage_complete
                && matches!(provider.outcome, ProviderSearchOutcome::Complete)
        });
    FederatedSessionSearchResponse {
        hits,
        query_complete,
        coverage_complete,
        providers,
        failures,
    }
}

/// Canonical hydration outcome for one provider hit locator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchHitHydrationOutcome {
    Hydrated,
    StaleLocator,
    SessionMissing,
    RepairRequired,
    Incompatible,
    Unavailable,
}

/// One provider hit paired with exact bounded canonical hydration state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydratedSessionSearchHit {
    pub hit: SessionSearchHit,
    pub outcome: SearchHitHydrationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<Box<bcode_session_models::SessionEvent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Deterministic bounded terminal aggregate from multiple providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederatedSessionSearchResponse {
    /// Deduplicated hits grouped in stable provider-ID/rank order.
    pub hits: Vec<SessionSearchHit>,
    pub query_complete: bool,
    pub coverage_complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<FederatedProviderReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<SessionSearchProviderFailure>,
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
    /// Normalized text bytes accepted for this session before this batch.
    ///
    /// Providers compare this value with their atomically retained session accounting. It bounds a
    /// session independently of individual record and batch limits; it is not a durable resume
    /// cursor.
    #[serde(default)]
    pub expected_previous_session_text_bytes: u64,
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
    /// Returns an error when the batch is empty/oversized, identities conflict, text or serialized
    /// payload exceeds its aggregate bound, cumulative session text exceeds its bound, or record
    /// sequences are not monotonic and within the declared generation.
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
            validate_search_record(record)?;
            if !identities.insert(record.record_id.as_str()) {
                return Err(ContractValidationError::InvalidBatch(
                    "duplicate record identity in batch",
                ));
            }
            if previous.is_some_and(|sequence| record.locator.sequence < sequence) {
                return Err(ContractValidationError::InvalidBatch(
                    "record sequences must not move backward",
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
        let session_text_bytes = self
            .expected_previous_session_text_bytes
            .checked_add(u64::try_from(text_bytes).unwrap_or(u64::MAX))
            .ok_or_else(|| ContractValidationError::LimitExceeded {
                field: "session_text_bytes",
                actual: usize::MAX,
                maximum: usize::try_from(MAX_SESSION_INGEST_TEXT_BYTES).unwrap_or(usize::MAX),
            })?;
        if session_text_bytes > MAX_SESSION_INGEST_TEXT_BYTES {
            return Err(ContractValidationError::LimitExceeded {
                field: "session_text_bytes",
                actual: usize::try_from(session_text_bytes).unwrap_or(usize::MAX),
                maximum: usize::try_from(MAX_SESSION_INGEST_TEXT_BYTES).unwrap_or(usize::MAX),
            });
        }
        let payload_bytes = serde_json::to_vec(self)
            .map_err(|_| ContractValidationError::InvalidBatch("batch serialization failed"))?
            .len();
        if payload_bytes > MAX_INGEST_PAYLOAD_BYTES {
            return Err(ContractValidationError::LimitExceeded {
                field: "batch_payload_bytes",
                actual: payload_bytes,
                maximum: MAX_INGEST_PAYLOAD_BYTES,
            });
        }
        Ok(())
    }
}

/// Classify an ingestion delivery against a provider-persisted batch digest.
///
/// Providers persist the returned digest atomically with published derived records. This helper
/// defines duplicate semantics only; it does not provide storage, retention, replay, or durable
/// resume behavior.
#[must_use]
pub fn classify_batch_delivery(
    request: &ApplySearchRecordsRequest,
    persisted_digest: Option<&str>,
) -> BatchDeliveryClassification {
    let operation_digest = request.operation_digest_sha256();
    match persisted_digest {
        None => BatchDeliveryClassification::New { operation_digest },
        Some(existing) if existing == operation_digest => {
            BatchDeliveryClassification::Duplicate { operation_digest }
        }
        Some(_) => BatchDeliveryClassification::ConflictingDuplicate { operation_digest },
    }
}

/// Duplicate-delivery classification for one provider-owned batch identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchDeliveryClassification {
    /// No retained digest exists for this batch identity.
    New { operation_digest: String },
    /// The retained digest describes the same operation facts.
    Duplicate { operation_digest: String },
    /// The retained digest describes different operation facts for the same batch identity.
    ConflictingDuplicate { operation_digest: String },
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

fn validate_search_record(record: &SessionSearchRecord) -> Result<(), ContractValidationError> {
    validate_nonempty_bounded("record_id", &record.record_id, MAX_CURSOR_BYTES)?;
    if record.locator.record_id.as_deref() != Some(record.record_id.as_str()) {
        return Err(ContractValidationError::InvalidBatch(
            "record identity differs from locator identity",
        ));
    }
    if record.normalization_version != CURRENT_NORMALIZATION_VERSION
        || record.policy_version != CURRENT_SEARCH_POLICY_VERSION
    {
        return Err(ContractValidationError::InvalidBatch(
            "record normalization or policy version is unsupported",
        ));
    }
    let text_bytes = record.text.as_ref().map_or(0, String::len);
    if record.indexed_bytes != u64::try_from(text_bytes).unwrap_or(u64::MAX)
        || record.indexed_bytes > record.normalized_bytes
        || (!record.truncated && record.indexed_bytes < record.normalized_bytes)
    {
        return Err(ContractValidationError::InvalidBatch(
            "record byte accounting is inconsistent",
        ));
    }
    match (record.source_range_start, record.source_range_end) {
        (Some(start), Some(end)) if start <= end && end <= record.source_bytes => {}
        (None, None) => {}
        _ => {
            return Err(ContractValidationError::InvalidBatch(
                "record source range is inconsistent",
            ));
        }
    }
    Ok(())
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
            expected_previous_session_text_bytes: 0,
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
    fn batch_validation_allows_distinct_records_at_one_canonical_sequence() {
        let session_id = SessionId::new();
        let record = SessionSearchRecord {
            schema_version: CURRENT_SEARCH_RECORD_VERSION,
            record_id: "1:reasoning-0-0:0".to_owned(),
            locator: SessionSearchLocator {
                session_id,
                sequence: 1,
                record_id: Some("1:reasoning-0-0:0".to_owned()),
            },
            timestamp_ms: 1,
            content_kind: SearchContentKind::AssistantReasoning,
            field: Some(SearchField::Text),
            text: Some("first".to_owned()),
            attributes: BTreeMap::new(),
            source_bytes: 5,
            normalized_bytes: 5,
            indexed_bytes: 5,
            truncated: false,
            source_range_start: Some(0),
            source_range_end: Some(5),
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
        };
        let request = ApplySearchRecordsRequest {
            provider_id: "provider".to_owned(),
            batch_id: "batch".to_owned(),
            generation: SearchCanonicalGeneration {
                session_id,
                fingerprint: "generation".to_owned(),
                last_sequence: Some(1),
            },
            expected_previous_sequence: None,
            expected_previous_session_text_bytes: 0,
            records: vec![
                record.clone(),
                SessionSearchRecord {
                    record_id: "1:reasoning-0-1:0".to_owned(),
                    locator: SessionSearchLocator {
                        record_id: Some("1:reasoning-0-1:0".to_owned()),
                        ..record.locator.clone()
                    },
                    ..record
                },
            ],
        };
        assert_eq!(request.validate(), Ok(()));
    }

    #[test]
    fn capability_negotiation_rejects_unsupported_features_and_content() {
        let capabilities = SessionSearchCapabilities {
            provider_id: "provider".to_owned(),
            execution: SearchExecutionKind::Indexed,
            content_kinds: std::iter::once(SearchContentKind::UserMessage).collect(),
            features: [
                SearchFeature::Terms,
                SearchFeature::StructuredFilters,
                SearchFeature::RelevanceSort,
            ]
            .into_iter()
            .collect(),
            max_hits: 20,
            max_batch_records: 20,
            max_batch_text_bytes: 1024,
        };
        let request = SessionSearchRequest {
            query: SessionSearchQuery::Text {
                text: "needle".to_owned(),
                mode: TextMatchMode::Regex,
                fields: BTreeSet::new(),
            },
            filters: SessionSearchFilters {
                content_kinds: std::iter::once(SearchContentKind::ToolOutput).collect(),
                ..SessionSearchFilters::default()
            },
            sort: SessionSearchSort::ProviderRelevance,
            limit: 10,
            cursor: None,
            deadline_ms: None,
        };
        assert!(matches!(
            capabilities.supports_request(&request),
            Err(ContractValidationError::InvalidProjection(
                "provider does not support all requested query features"
            ))
        ));

        let mut supported = capabilities;
        supported.features.insert(SearchFeature::Regex);
        assert!(matches!(
            supported.supports_request(&request),
            Err(ContractValidationError::InvalidProjection(
                "provider does not cover all requested content kinds"
            ))
        ));
    }

    #[test]
    fn aggregation_is_stable_deduplicated_and_partial_on_failure() {
        let session_id = SessionId::new();
        let hit = |provider_id: &str, sequence: u64, rank: u32| SessionSearchHit {
            locator: SessionSearchLocator {
                session_id,
                sequence,
                record_id: Some(format!("record-{sequence}")),
            },
            content_kind: SearchContentKind::UserMessage,
            matched_field: SearchField::Text,
            provider_id: provider_id.to_owned(),
            provider_rank: rank,
            provider_score: None,
            preview: None,
            preview_truncated: false,
        };
        let contribution =
            |provider_id: &str, hits: Vec<SessionSearchHit>| FederatedProviderContribution {
                report: FederatedProviderReport {
                    provider_id: provider_id.to_owned(),
                    outcome: ProviderSearchOutcome::Complete,
                    elapsed_ms: 1,
                    query_complete: true,
                    coverage_complete: true,
                    searched_content: vec![SearchContentKind::UserMessage],
                    excluded_content: Vec::new(),
                },
                hits,
            };
        let response = aggregate_federated_search(
            vec![
                contribution(
                    "b-provider",
                    vec![hit("b-provider", 1, 0), hit("b-provider", 2, 1)],
                ),
                contribution("a-provider", vec![hit("a-provider", 1, 0)]),
            ],
            vec![planning_failure(
                "c-provider".to_owned(),
                SearchErrorCode::DeadlineExceeded,
                "deadline exceeded",
            )],
            2,
        );
        assert_eq!(response.providers[0].provider_id, "a-provider");
        assert_eq!(response.hits.len(), 2);
        assert_eq!(response.hits[0].provider_id, "a-provider");
        assert_eq!(response.hits[1].locator.sequence, 2);
        assert!(!response.query_complete);
        assert!(!response.coverage_complete);
        assert_eq!(response.failures[0].plugin_id, "c-provider");
    }

    #[test]
    fn explicit_routes_support_fallback_parallel_and_disjoint_selection() {
        let request = SessionSearchRequest {
            query: text_query("needle"),
            filters: SessionSearchFilters {
                content_kinds: [
                    SearchContentKind::UserMessage,
                    SearchContentKind::ShellOutput,
                ]
                .into_iter()
                .collect(),
                ..SessionSearchFilters::default()
            },
            sort: SessionSearchSort::ProviderRelevance,
            limit: 10,
            cursor: None,
            deadline_ms: None,
        };
        let make_provider =
            |id: &str, content: BTreeSet<SearchContentKind>| SessionSearchProviderInfo {
                plugin_id: id.to_owned(),
                capabilities: SessionSearchCapabilities {
                    provider_id: id.to_owned(),
                    execution: SearchExecutionKind::Indexed,
                    content_kinds: content,
                    features: [
                        SearchFeature::Terms,
                        SearchFeature::StructuredFilters,
                        SearchFeature::RelevanceSort,
                    ]
                    .into_iter()
                    .collect(),
                    max_hits: 20,
                    max_batch_records: 20,
                    max_batch_text_bytes: 1024,
                },
                status: SessionSearchStatus {
                    provider_id: id.to_owned(),
                    state: SearchProviderState::Ready,
                    record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
                    normalization_version: CURRENT_NORMALIZATION_VERSION,
                    policy_version: CURRENT_SEARCH_POLICY_VERSION,
                    index_bytes: 0,
                    quota_bytes: 1024,
                    pending_sessions: 0,
                    coverage: Vec::new(),
                    degraded_reason: None,
                },
            };
        let discovery = ListSessionSearchProvidersResponse {
            providers: vec![
                make_provider(
                    "fallback",
                    std::iter::once(SearchContentKind::UserMessage).collect(),
                ),
                make_provider(
                    "parallel",
                    std::iter::once(SearchContentKind::UserMessage).collect(),
                ),
                make_provider(
                    "shell",
                    std::iter::once(SearchContentKind::ShellOutput).collect(),
                ),
            ],
            failures: Vec::new(),
        };
        let routes = vec![
            SessionSearchContentRoute {
                content_kinds: std::iter::once(SearchContentKind::UserMessage).collect(),
                mode: SessionSearchRouteMode::Fallback,
                provider_ids: vec!["missing".to_owned(), "fallback".to_owned()],
            },
            SessionSearchContentRoute {
                content_kinds: std::iter::once(SearchContentKind::UserMessage).collect(),
                mode: SessionSearchRouteMode::Parallel,
                provider_ids: vec!["parallel".to_owned()],
            },
            SessionSearchContentRoute {
                content_kinds: std::iter::once(SearchContentKind::ShellOutput).collect(),
                mode: SessionSearchRouteMode::Disjoint,
                provider_ids: vec!["shell".to_owned()],
            },
        ];
        let plan = plan_session_search_with_routes(&request, discovery, &routes);
        assert_eq!(
            plan.providers
                .iter()
                .map(|provider| provider.plugin_id.as_str())
                .collect::<Vec<_>>(),
            vec!["fallback", "parallel", "shell"]
        );
        assert!(
            plan.failures
                .iter()
                .any(|failure| failure.plugin_id == "missing")
        );
    }

    #[test]
    fn provider_failures_report_stage_elapsed_and_requested_content() {
        let failure = planning_failure_with_content(
            "provider".to_owned(),
            SearchErrorCode::StaleIndex,
            "stale",
            vec![SearchContentKind::UserMessage],
        );
        assert_eq!(failure.stage, SessionSearchProviderStage::Planning);
        assert_eq!(failure.elapsed_ms, 0);
        assert_eq!(failure.content, vec![SearchContentKind::UserMessage]);

        let encoded = serde_json::to_value(&failure).expect("encode provider failure");
        assert_eq!(encoded["stage"], "planning");
        assert_eq!(encoded["elapsed_ms"], 0);
        assert_eq!(encoded["content"][0], "user_message");
    }

    #[test]
    fn status_validation_rejects_incompatible_versions_and_unexplained_degradation() {
        let mut status = SessionSearchStatus {
            provider_id: "provider".to_owned(),
            state: SearchProviderState::Ready,
            record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
            index_bytes: 0,
            quota_bytes: 1024,
            pending_sessions: 0,
            coverage: Vec::new(),
            degraded_reason: None,
        };
        assert_eq!(status.validate(), Ok(()));
        status.record_schema_version = CURRENT_SEARCH_RECORD_VERSION.saturating_add(1);
        assert!(status.validate().is_err());
        status.record_schema_version = CURRENT_SEARCH_RECORD_VERSION;
        status.state = SearchProviderState::Degraded;
        assert!(matches!(
            status.validate(),
            Err(ContractValidationError::EmptyField("degraded_reason"))
        ));
        status.degraded_reason = Some("checkpoint is stale".to_owned());
        assert_eq!(status.validate(), Ok(()));
        status.state = SearchProviderState::Stale;
        status.degraded_reason = None;
        assert!(matches!(
            status.validate(),
            Err(ContractValidationError::EmptyField("degraded_reason"))
        ));
        status.state = SearchProviderState::Unavailable;
        status.degraded_reason = Some("configured binary is unavailable".to_owned());
        assert_eq!(status.validate(), Ok(()));
    }

    #[test]
    fn plan_policy_excludes_cold_and_stale_providers_from_ordinary_search() {
        fn provider(
            id: &str,
            execution: SearchExecutionKind,
            indexed_through_sequence: u64,
        ) -> SessionSearchProviderInfo {
            let session_id = SessionId::new();
            SessionSearchProviderInfo {
                plugin_id: id.to_owned(),
                capabilities: SessionSearchCapabilities {
                    provider_id: id.to_owned(),
                    execution,
                    content_kinds: std::iter::once(SearchContentKind::UserMessage).collect(),
                    features: [
                        SearchFeature::Terms,
                        SearchFeature::StructuredFilters,
                        SearchFeature::RelevanceSort,
                    ]
                    .into_iter()
                    .collect(),
                    max_hits: 20,
                    max_batch_records: 20,
                    max_batch_text_bytes: 1024,
                },
                status: SessionSearchStatus {
                    provider_id: id.to_owned(),
                    state: SearchProviderState::Ready,
                    record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
                    normalization_version: CURRENT_NORMALIZATION_VERSION,
                    policy_version: CURRENT_SEARCH_POLICY_VERSION,
                    index_bytes: 0,
                    quota_bytes: 1024,
                    pending_sessions: 0,
                    coverage: vec![SessionSearchCoverage {
                        generation: SearchCanonicalGeneration {
                            session_id,
                            fingerprint: "generation".to_owned(),
                            last_sequence: Some(10),
                        },
                        content_kinds: std::iter::once(SearchContentKind::UserMessage).collect(),
                        indexed_through_sequence: Some(indexed_through_sequence),
                        complete: indexed_through_sequence == 10,
                        skipped_records: 0,
                        truncated_records: 0,
                        exclusions: Vec::new(),
                    }],
                    degraded_reason: None,
                },
            }
        }

        let request = SessionSearchRequest {
            query: text_query("needle"),
            filters: SessionSearchFilters {
                content_kinds: std::iter::once(SearchContentKind::UserMessage).collect(),
                ..SessionSearchFilters::default()
            },
            sort: SessionSearchSort::ProviderRelevance,
            limit: 10,
            cursor: None,
            deadline_ms: Some(5_000),
        };
        let discovery = ListSessionSearchProvidersResponse {
            providers: vec![
                provider("fresh", SearchExecutionKind::Indexed, 10),
                provider("stale", SearchExecutionKind::Indexed, 7),
                provider("scan", SearchExecutionKind::Scan, 10),
            ],
            failures: Vec::new(),
        };
        let policy = SessionSearchPlanPolicy {
            execution_class: SessionSearchExecutionClass::Ordinary,
            maximum_staleness_sequences: Some(1),
            per_provider_deadline_ms: 1_000,
        };
        let plan =
            plan_session_search_with_policy_and_routes(&request, discovery.clone(), &policy, &[]);
        assert_eq!(
            plan.providers
                .iter()
                .map(|provider| provider.plugin_id.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh"]
        );
        assert_eq!(plan.per_provider_deadline_ms, 1_000);
        assert_eq!(plan.failures.len(), 2);

        let deep = SessionSearchPlanPolicy {
            execution_class: SessionSearchExecutionClass::Deep,
            maximum_staleness_sequences: None,
            per_provider_deadline_ms: 4_000,
        };
        let plan = plan_session_search_with_policy_and_routes(&request, discovery, &deep, &[]);
        assert!(plan.failures.is_empty());
        assert_eq!(plan.per_provider_deadline_ms, 4_000);
    }

    #[test]
    fn query_plan_selects_stable_disjoint_coverage_without_redundant_broadcast() {
        fn provider(
            id: &str,
            content: SearchContentKind,
            state: SearchProviderState,
        ) -> SessionSearchProviderInfo {
            SessionSearchProviderInfo {
                plugin_id: id.to_owned(),
                capabilities: SessionSearchCapabilities {
                    provider_id: id.to_owned(),
                    execution: SearchExecutionKind::Indexed,
                    content_kinds: std::iter::once(content).collect(),
                    features: [
                        SearchFeature::Terms,
                        SearchFeature::StructuredFilters,
                        SearchFeature::RelevanceSort,
                    ]
                    .into_iter()
                    .collect(),
                    max_hits: 20,
                    max_batch_records: 20,
                    max_batch_text_bytes: 1024,
                },
                status: SessionSearchStatus {
                    provider_id: id.to_owned(),
                    state,
                    record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
                    normalization_version: CURRENT_NORMALIZATION_VERSION,
                    policy_version: CURRENT_SEARCH_POLICY_VERSION,
                    index_bytes: 0,
                    quota_bytes: 1024,
                    pending_sessions: 0,
                    coverage: Vec::new(),
                    degraded_reason: None,
                },
            }
        }

        let request = SessionSearchRequest {
            query: text_query("needle"),
            filters: SessionSearchFilters {
                content_kinds: [
                    SearchContentKind::UserMessage,
                    SearchContentKind::ShellOutput,
                ]
                .into_iter()
                .collect(),
                ..SessionSearchFilters::default()
            },
            sort: SessionSearchSort::ProviderRelevance,
            limit: 10,
            cursor: None,
            deadline_ms: None,
        };
        let plan = plan_session_search(
            &request,
            ListSessionSearchProvidersResponse {
                providers: vec![
                    provider(
                        "z-redundant",
                        SearchContentKind::UserMessage,
                        SearchProviderState::Ready,
                    ),
                    provider(
                        "b-shell",
                        SearchContentKind::ShellOutput,
                        SearchProviderState::CatchingUp,
                    ),
                    provider(
                        "a-transcript",
                        SearchContentKind::UserMessage,
                        SearchProviderState::Ready,
                    ),
                    provider(
                        "c-disabled",
                        SearchContentKind::ShellOutput,
                        SearchProviderState::Disabled,
                    ),
                ],
                failures: Vec::new(),
            },
        );
        assert_eq!(
            plan.providers
                .iter()
                .map(|provider| provider.plugin_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-transcript", "b-shell"]
        );
        assert_eq!(plan.failures.len(), 1);
        assert_eq!(plan.failures[0].plugin_id, "c-disabled");
    }

    #[test]
    fn duplicate_delivery_classification_uses_persisted_operation_digest() {
        let fixture = include_str!("../tests/fixtures/apply-search-records-v1.json");
        let request: ApplySearchRecordsRequest =
            serde_json::from_str(fixture).expect("decode retained ingestion fixture");
        let digest = request.operation_digest_sha256();
        assert!(matches!(
            classify_batch_delivery(&request, None),
            BatchDeliveryClassification::New { operation_digest } if operation_digest == digest
        ));
        assert!(matches!(
            classify_batch_delivery(&request, Some(&digest)),
            BatchDeliveryClassification::Duplicate { operation_digest }
                if operation_digest == digest
        ));
        assert!(matches!(
            classify_batch_delivery(&request, Some("different")),
            BatchDeliveryClassification::ConflictingDuplicate { operation_digest }
                if operation_digest == digest
        ));
    }

    #[test]
    fn retained_v1_request_fixture_decodes_validates_and_reencodes() {
        let fixture = include_str!("../tests/fixtures/session-search-request-v1.json");
        let request: SessionSearchRequest =
            serde_json::from_str(fixture).expect("decode retained request fixture");
        request
            .validate()
            .expect("validate retained request fixture");
        let expected: serde_json::Value =
            serde_json::from_str(fixture).expect("decode fixture value");
        let actual = serde_json::to_value(request).expect("encode request fixture");
        assert_eq!(actual, expected);
    }

    #[test]
    fn retained_v1_capabilities_fixture_decodes_validates_and_reencodes() {
        let fixture = include_str!("../tests/fixtures/session-search-capabilities-v1.json");
        let capabilities: SessionSearchCapabilities =
            serde_json::from_str(fixture).expect("decode retained capabilities fixture");
        capabilities
            .validate()
            .expect("validate retained capabilities fixture");
        let expected: serde_json::Value =
            serde_json::from_str(fixture).expect("decode fixture value");
        let actual = serde_json::to_value(capabilities).expect("encode capabilities fixture");
        assert_eq!(actual, expected);
    }

    #[test]
    fn ingestion_validation_bounds_cumulative_session_text() {
        let fixture = include_str!("../tests/fixtures/apply-search-records-v1.json");
        let mut request: ApplySearchRecordsRequest =
            serde_json::from_str(fixture).expect("decode retained ingestion fixture");
        request.expected_previous_session_text_bytes = MAX_SESSION_INGEST_TEXT_BYTES - 4;
        assert!(matches!(
            request.validate(),
            Err(ContractValidationError::LimitExceeded {
                field: "session_text_bytes",
                ..
            })
        ));
    }

    #[test]
    fn ingestion_validation_bounds_serialized_payload_metadata() {
        let fixture = include_str!("../tests/fixtures/apply-search-records-v1.json");
        let mut request: ApplySearchRecordsRequest =
            serde_json::from_str(fixture).expect("decode retained ingestion fixture");
        request.records[0].attributes.insert(
            "oversized-metadata".to_owned(),
            "x".repeat(MAX_INGEST_PAYLOAD_BYTES),
        );
        assert!(matches!(
            request.validate(),
            Err(ContractValidationError::LimitExceeded {
                field: "batch_payload_bytes",
                ..
            })
        ));
    }

    #[test]
    fn retained_v1_ingestion_fixture_decodes_validates_and_reencodes() {
        let fixture = include_str!("../tests/fixtures/apply-search-records-v1.json");
        let request: ApplySearchRecordsRequest =
            serde_json::from_str(fixture).expect("decode retained ingestion fixture");
        request
            .validate()
            .expect("validate retained ingestion fixture");
        let expected: serde_json::Value =
            serde_json::from_str(fixture).expect("decode fixture value");
        let actual = serde_json::to_value(&request).expect("encode ingestion fixture");
        assert_eq!(actual, expected);

        let mut future = request;
        future.records[0].schema_version = CURRENT_SEARCH_RECORD_VERSION.saturating_add(1);
        assert!(matches!(
            future.validate(),
            Err(ContractValidationError::InvalidBatch(
                "record schema version is unsupported"
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
