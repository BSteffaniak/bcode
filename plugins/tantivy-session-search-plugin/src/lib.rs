#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Optional Tantivy-backed session transcript search provider.
//!
//! This plugin indexes only bounded, normalized [`SessionSearchRecord`] values delivered through
//! the versioned session-search service. It never opens or understands canonical Bcode session
//! storage.

use bcode_plugin_sdk::{ServiceCancellation, prelude::*};
use bcode_session_models::SessionId;
use bcode_session_search::{
    ApplyBatchOutcome, ApplySearchRecordsRequest, ApplySearchRecordsResponse,
    BatchDeliveryClassification, CURRENT_NORMALIZATION_VERSION, CURRENT_SEARCH_POLICY_VERSION,
    CURRENT_SEARCH_RECORD_VERSION, OP_APPLY_BATCH, OP_CAPABILITIES, OP_PURGE, OP_REBUILD,
    OP_REMOVE_SESSION, OP_SEARCH, OP_STATUS, ProviderSearchOutcome, PurgeSessionSearchRequest,
    RebuildSessionSearchRequest, RebuildSessionSearchResponse, RemoveSessionSearchRequest,
    SESSION_SEARCH_INTERFACE_ID, SearchContentKind, SearchCursor, SearchExecutionKind,
    SearchFeature, SearchField, SearchProviderState, SessionSearchCapabilities, SessionSearchHit,
    SessionSearchLocator, SessionSearchRequest, SessionSearchResponse, SessionSearchSort,
    SessionSearchStatus, TextMatchMode, classify_batch_delivery,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery, TermSetQuery};
use tantivy::schema::{
    FAST, Field, IndexRecordOption, STORED, STRING, Schema, TantivyDocument, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Term};

const PLUGIN_ID: &str = "bcode.tantivy-session-search";
const INDEX_SCHEMA_VERSION: u16 = 2;
const TOKENIZER_VERSION: u16 = 1;
const DEFAULT_WRITER_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MIN_WRITER_MEMORY_BYTES: usize = 15 * 1024 * 1024;
const DEFAULT_QUOTA_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_PREVIEW_BYTES: usize = bcode_session_search::MAX_HIT_PREVIEW_BYTES;
const PURGE_CONFIRMATION: &str = "purge-bcode.tantivy-session-search";
const REBUILD_CONFIRMATION: &str = "rebuild-bcode.tantivy-session-search";
const CHECKPOINT_FILE: &str = "provider-state.json";
const REBUILD_MARKER_FILE: &str = "rebuild-in-progress";
const INDEX_DIRECTORY: &str = "index";
/// Minimum interval between engine open attempts after a failed open.
///
/// A failed open is remembered so status and ingestion stay bounded, but it is not permanent:
/// the most common failure on a shared state root is a transient `LockBusy` from another process
/// still holding the index writer lock, which resolves once that process exits.
const ENGINE_OPEN_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderConfig {
    storage_root: Option<PathBuf>,
    quota_bytes: u64,
    writer_memory_bytes: usize,
    sensitive_content: BTreeSet<SearchContentKind>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            storage_root: None,
            quota_bytes: DEFAULT_QUOTA_BYTES,
            writer_memory_bytes: DEFAULT_WRITER_MEMORY_BYTES,
            sensitive_content: BTreeSet::new(),
        }
    }
}

impl ProviderConfig {
    fn validate(&self) -> Result<(), ProviderError> {
        if self.quota_bytes == 0 {
            return Err(ProviderError::configuration(
                "quota_bytes must be greater than zero".to_owned(),
            ));
        }
        if self.writer_memory_bytes < MIN_WRITER_MEMORY_BYTES {
            return Err(ProviderError::configuration(format!(
                "writer_memory_bytes must be at least {MIN_WRITER_MEMORY_BYTES}"
            )));
        }
        if self.sensitive_content.iter().any(|kind| {
            matches!(
                kind,
                SearchContentKind::ShellOutput | SearchContentKind::ToolOutput
            )
        }) {
            return Err(ProviderError::configuration(
                "large shell/tool output is not supported by the transcript provider; enable a measured deep-search provider instead".to_owned(),
            ));
        }
        Ok(())
    }

    fn allowed_content(&self) -> BTreeSet<SearchContentKind> {
        let mut content = BTreeSet::from([
            SearchContentKind::SessionTitle,
            SearchContentKind::UserMessage,
            SearchContentKind::AssistantMessage,
            SearchContentKind::SystemMessage,
            SearchContentKind::ShellCommand,
            SearchContentKind::ToolError,
            SearchContentKind::Compaction,
        ]);
        content.extend(self.sensitive_content.iter().copied());
        content
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedProviderState {
    index_schema_version: u16,
    tokenizer_version: u16,
    record_schema_version: u16,
    normalization_version: u16,
    policy_version: u16,
    quota_bytes: u64,
    #[serde(default)]
    content_kinds: BTreeSet<SearchContentKind>,
    sessions: BTreeMap<SessionId, PersistedSessionState>,
    batch_digests: BTreeMap<String, String>,
    /// Canonical generations explicitly removed while stale ingestion may still be in flight.
    #[serde(default)]
    removed_sessions: BTreeMap<SessionId, Option<String>>,
}

impl PersistedProviderState {
    fn new(config: &ProviderConfig) -> Self {
        Self {
            index_schema_version: INDEX_SCHEMA_VERSION,
            tokenizer_version: TOKENIZER_VERSION,
            record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
            quota_bytes: config.quota_bytes,
            content_kinds: config.allowed_content(),
            sessions: BTreeMap::new(),
            batch_digests: BTreeMap::new(),
            removed_sessions: BTreeMap::new(),
        }
    }

    fn validate(&self, config: &ProviderConfig) -> Result<(), ProviderError> {
        if self.index_schema_version != INDEX_SCHEMA_VERSION
            || self.tokenizer_version != TOKENIZER_VERSION
            || self.record_schema_version != CURRENT_SEARCH_RECORD_VERSION
            || self.normalization_version != CURRENT_NORMALIZATION_VERSION
            || self.policy_version != CURRENT_SEARCH_POLICY_VERSION
        {
            return Err(ProviderError::incompatible(
                "provider state uses unsupported schema, tokenizer, normalization, or policy versions"
                    .to_owned(),
            ));
        }
        if self.quota_bytes != config.quota_bytes {
            return Err(ProviderError::incompatible(
                "configured quota differs from retained provider state; explicit rebuild is required"
                    .to_owned(),
            ));
        }
        if self.content_kinds != config.allowed_content() {
            return Err(ProviderError::incompatible(
                "configured content policy differs from retained provider state; explicit rebuild is required"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSessionState {
    generation_fingerprint: String,
    canonical_tail_sequence: Option<u64>,
    indexed_through_sequence: u64,
    session_text_bytes: u64,
    content_kinds: BTreeSet<SearchContentKind>,
    record_count: u64,
    truncated_records: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum CommitMarker {
    ApplyBatch {
        version: u16,
        batch_id: String,
        operation_digest: String,
        session_id: SessionId,
        session: PersistedSessionState,
    },
    RemoveSession {
        version: u16,
        session_id: SessionId,
        #[serde(default)]
        expected_generation_fingerprint: Option<String>,
    },
}

impl CommitMarker {
    fn apply_to(&self, state: &mut PersistedProviderState) -> Result<(), ProviderError> {
        match self {
            Self::ApplyBatch {
                version,
                batch_id,
                operation_digest,
                session_id,
                session,
            } => {
                if *version != 1 {
                    return Err(ProviderError::incompatible(
                        "Tantivy commit marker uses an unsupported version".to_owned(),
                    ));
                }
                state.sessions.insert(*session_id, session.clone());
                state.removed_sessions.remove(session_id);
                state
                    .batch_digests
                    .insert(batch_id.clone(), operation_digest.clone());
            }
            Self::RemoveSession {
                version,
                session_id,
                expected_generation_fingerprint,
            } => {
                if *version != 1 {
                    return Err(ProviderError::incompatible(
                        "Tantivy commit marker uses an unsupported version".to_owned(),
                    ));
                }
                state.sessions.remove(session_id);
                state
                    .removed_sessions
                    .insert(*session_id, expected_generation_fingerprint.clone());
            }
        }
        Ok(())
    }
}

struct SearchEngine {
    root: PathBuf,
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: Fields,
    state: Mutex<PersistedProviderState>,
}

#[derive(Debug, Clone, Copy)]
struct Fields {
    record_id: Field,
    session_id: Field,
    sequence: Field,
    timestamp_ms: Field,
    content_kind: Field,
    matched_field: Field,
    text: Field,
    preview: Field,
    preview_truncated: Field,
    working_directory: Field,
    role: Field,
    tool_name: Field,
    tool_status: Field,
    provider: Field,
    model: Field,
    agent: Field,
    source: Field,
}

impl Fields {
    fn from_schema(schema: &Schema) -> Result<Self, ProviderError> {
        let field = |name| {
            schema
                .get_field(name)
                .map_err(|error| ProviderError::incompatible(error.to_string()))
        };
        Ok(Self {
            record_id: field("record_id")?,
            session_id: field("session_id")?,
            sequence: field("sequence")?,
            timestamp_ms: field("timestamp_ms")?,
            content_kind: field("content_kind")?,
            matched_field: field("matched_field")?,
            text: field("text")?,
            preview: field("preview")?,
            preview_truncated: field("preview_truncated")?,
            working_directory: field("working_directory")?,
            role: field("role")?,
            tool_name: field("tool_name")?,
            tool_status: field("tool_status")?,
            provider: field("provider")?,
            model: field("model")?,
            agent: field("agent")?,
            source: field("source")?,
        })
    }
}

enum EngineState {
    Uninitialized,
    Ready(Arc<SearchEngine>),
    Failed {
        message: String,
        failed_at: std::time::Instant,
    },
}

impl EngineState {
    fn failed(error: &impl std::fmt::Display) -> Self {
        Self::Failed {
            message: bounded_message(&error.to_string()),
            failed_at: std::time::Instant::now(),
        }
    }
}

#[derive(Debug)]
struct ProviderError {
    code: &'static str,
    message: String,
}

impl ProviderError {
    const fn configuration(message: String) -> Self {
        Self {
            code: "invalid_configuration",
            message,
        }
    }

    const fn incompatible(message: String) -> Self {
        Self {
            code: "incompatible_index",
            message,
        }
    }

    const fn invalid_request(message: String) -> Self {
        Self {
            code: "invalid_request",
            message,
        }
    }

    fn index(error: impl std::fmt::Display) -> Self {
        Self {
            code: "index_error",
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub struct TantivySessionSearchPlugin {
    lifecycle: RwLock<()>,
    engine: RwLock<EngineState>,
}

impl Default for TantivySessionSearchPlugin {
    fn default() -> Self {
        Self {
            lifecycle: RwLock::new(()),
            engine: RwLock::new(EngineState::Uninitialized),
        }
    }
}

impl RustPlugin for TantivySessionSearchPlugin {}

impl ConcurrentRustPlugin for TantivySessionSearchPlugin {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != SESSION_SEARCH_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported Tantivy session-search interface",
            );
        }
        let mut config = match context.config_or_default::<ProviderConfig>() {
            Ok(config) => config,
            Err(error) => {
                return ServiceResponse::error("invalid_configuration", error.to_string());
            }
        };
        if config.storage_root.is_none() {
            config.storage_root.clone_from(&context.config.state_root);
        }
        if let Err(error) = config.validate() {
            return error_response(&error);
        }
        match context.request.operation.as_str() {
            OP_CAPABILITIES => json_response(&capabilities(&config)),
            OP_STATUS => json_response(&self.status(&config)),
            OP_SEARCH => decode_request(&context, |request: SessionSearchRequest| {
                self.search(&config, &request, &context.cancellation)
            }),
            OP_APPLY_BATCH => decode_request(&context, |request: ApplySearchRecordsRequest| {
                self.apply_batch(&config, &request, &context.cancellation)
            }),
            OP_REMOVE_SESSION => decode_request(&context, |request: RemoveSessionSearchRequest| {
                self.remove_session(&config, &request)
            }),
            OP_PURGE => decode_request(&context, |request: PurgeSessionSearchRequest| {
                self.purge(&config, &request)
            }),
            OP_REBUILD => decode_request(&context, |request: RebuildSessionSearchRequest| {
                self.rebuild(&config, &request)
            }),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported Tantivy session-search operation",
            ),
        }
    }
}

impl TantivySessionSearchPlugin {
    fn status(&self, config: &ProviderConfig) -> SessionSearchStatus {
        let _lifecycle = self
            .lifecycle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut response = empty_status(config);
        if let Some(configured_root) = &config.storage_root
            && let Ok(root) = confined_storage_root(configured_root)
            && root.join(REBUILD_MARKER_FILE).is_file()
        {
            response.state = SearchProviderState::Rebuilding;
            response.index_bytes = directory_size(&root.join(INDEX_DIRECTORY));
            return response;
        }
        if config.storage_root.is_some()
            && let Err(error) = self.ready_engine(config)
        {
            response.state = SearchProviderState::Degraded;
            response.degraded_reason = Some(bounded_message(&error.to_string()));
            return response;
        }
        let guard = self
            .engine
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            EngineState::Uninitialized => {
                if config.storage_root.is_none() {
                    response.state = SearchProviderState::Disabled;
                }
            }
            EngineState::Ready(engine) => {
                let state = engine
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                response.index_bytes = directory_size(&engine.root.join(INDEX_DIRECTORY));
                response.document_count = state.sessions.values().fold(0_u64, |total, session| {
                    total.saturating_add(session.record_count)
                });
                response.coverage = state
                    .sessions
                    .iter()
                    .map(
                        |(session_id, session)| bcode_session_search::SessionSearchCoverage {
                            generation: bcode_session_search::SearchCanonicalGeneration {
                                session_id: *session_id,
                                fingerprint: session.generation_fingerprint.clone(),
                                last_sequence: session.canonical_tail_sequence,
                            },
                            content_kinds: session.content_kinds.clone(),
                            indexed_through_sequence: Some(session.indexed_through_sequence),
                            complete: session
                                .canonical_tail_sequence
                                .is_some_and(|tail| tail == session.indexed_through_sequence),
                            indexed_text_bytes: session.session_text_bytes,
                            skipped_records: 0,
                            truncated_records: session.truncated_records,
                            exclusions: Vec::new(),
                        },
                    )
                    .collect();
            }
            EngineState::Failed { message, .. } => {
                response.state = SearchProviderState::Degraded;
                response.degraded_reason = Some(bounded_message(message));
            }
        }
        drop(guard);
        response
    }

    fn search(
        &self,
        config: &ProviderConfig,
        request: &SessionSearchRequest,
        cancellation: &ServiceCancellation,
    ) -> ServiceResponse {
        let _lifecycle = self
            .lifecycle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = request.validate() {
            return ServiceResponse::error("invalid_request", error.to_string());
        }
        if let Err(error) = capabilities(config).supports_request(request) {
            return ServiceResponse::error("unsupported_query", error.to_string());
        }
        let offset = match decode_search_offset(request) {
            Ok(offset) => offset,
            Err(error) => return error_response(&error),
        };
        if cancellation.is_cancelled() {
            return ServiceResponse::error("cancelled", "search cancelled before execution");
        }
        let engine = match self.ready_engine(config) {
            Ok(guard) => guard,
            Err(error) => return error_response(&error),
        };
        let query = match build_query(&engine, request) {
            Ok(query) => query,
            Err(error) => return error_response(&error),
        };
        let searcher = engine.reader.searcher();
        let fields = engine.fields;
        let collector = TopDocs::with_limit(request.limit.saturating_add(1))
            .and_offset(offset)
            .order_by_score();
        let results = match searcher.search(&query, &collector) {
            Ok(results) => results,
            Err(error) => return error_response(&ProviderError::index(error)),
        };
        let has_more = results.len() > request.limit;
        let mut hits = Vec::with_capacity(results.len().min(request.limit));
        for (rank, (score, address)) in results.into_iter().take(request.limit).enumerate() {
            if cancellation.is_cancelled() {
                return ServiceResponse::error("cancelled", "search cancelled during result load");
            }
            let document = match searcher.doc::<TantivyDocument>(address) {
                Ok(document) => document,
                Err(error) => return error_response(&ProviderError::index(error)),
            };
            match document_hit(fields, &document, rank, score) {
                Ok(hit) => hits.push(hit),
                Err(error) => return error_response(&error),
            }
        }
        match request.sort {
            SessionSearchSort::ProviderRelevance => {}
            SessionSearchSort::NewestFirst => hits.sort_by(|left, right| {
                right
                    .locator
                    .sequence
                    .cmp(&left.locator.sequence)
                    .then(left.provider_rank.cmp(&right.provider_rank))
            }),
            SessionSearchSort::OldestFirst | SessionSearchSort::SessionThenSequence => {
                hits.sort_by(|left, right| left.locator.cmp(&right.locator));
            }
        }
        for (index, hit) in hits.iter_mut().enumerate() {
            hit.provider_rank = u32::try_from(index + 1).unwrap_or(u32::MAX);
        }
        let next_cursor = has_more.then(|| SearchCursor {
            provider_id: PLUGIN_ID.to_owned(),
            query_fingerprint: search_query_fingerprint(request),
            value: offset.saturating_add(request.limit).to_string(),
        });
        json_response(&SessionSearchResponse {
            provider_id: PLUGIN_ID.to_owned(),
            outcome: ProviderSearchOutcome::Complete,
            hits,
            next_cursor,
            query_complete: true,
            coverage_complete: coverage_complete_for_request(engine.as_ref(), request),
            searched_content: requested_content(request, config),
            excluded_content: Vec::new(),
            message: None,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn apply_batch(
        &self,
        config: &ProviderConfig,
        request: &ApplySearchRecordsRequest,
        cancellation: &ServiceCancellation,
    ) -> ServiceResponse {
        let _lifecycle = self
            .lifecycle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if request.provider_id != PLUGIN_ID {
            return ServiceResponse::error("invalid_request", "provider identity mismatch");
        }
        if let Err(error) = request.validate() {
            return ServiceResponse::error("invalid_request", error.to_string());
        }
        if cancellation.is_cancelled() {
            return ServiceResponse::error("cancelled", "ingestion cancelled before execution");
        }
        let allowed = config.allowed_content();
        if request
            .records
            .iter()
            .any(|record| !allowed.contains(&record.content_kind))
        {
            return ServiceResponse::error(
                "content_disabled",
                "ingestion batch contains content disabled by provider policy",
            );
        }
        let engine = match self.ready_engine(config) {
            Ok(guard) => guard,
            Err(error) => return error_response(&error),
        };
        let mut state = engine
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .removed_sessions
            .get(&request.generation.session_id)
            .is_some_and(|removed_generation| {
                removed_generation
                    .as_ref()
                    .is_none_or(|generation| generation == &request.generation.fingerprint)
            })
        {
            return ServiceResponse::error(
                "stale_generation",
                "canonical session was removed; stale ingestion is rejected",
            );
        }
        let last_sequence = request.indexed_through_sequence.unwrap_or_else(|| {
            request
                .records
                .last()
                .map_or(0, |record| record.locator.sequence)
        });
        let classification = classify_batch_delivery(
            request,
            state
                .batch_digests
                .get(&request.batch_id)
                .map(String::as_str),
        );
        let operation_digest = match classification {
            BatchDeliveryClassification::Duplicate { .. } => {
                return json_response(&ApplySearchRecordsResponse {
                    batch_id: request.batch_id.clone(),
                    outcome: ApplyBatchOutcome::Duplicate,
                    applied_records: 0,
                    indexed_through_sequence: last_sequence,
                });
            }
            BatchDeliveryClassification::ConflictingDuplicate { .. } => {
                return json_response(&ApplySearchRecordsResponse {
                    batch_id: request.batch_id.clone(),
                    outcome: ApplyBatchOutcome::ConflictingDuplicate,
                    applied_records: 0,
                    indexed_through_sequence: last_sequence,
                });
            }
            BatchDeliveryClassification::New { operation_digest } => operation_digest,
        };
        if let Some(existing) = state.sessions.get(&request.generation.session_id) {
            if existing.generation_fingerprint != request.generation.fingerprint {
                return ServiceResponse::error(
                    "stale_generation",
                    "canonical generation changed; explicit rebuild is required",
                );
            }
            if existing.indexed_through_sequence != request.expected_previous_sequence.unwrap_or(0)
                || existing.session_text_bytes != request.expected_previous_session_text_bytes
            {
                return ServiceResponse::error(
                    "checkpoint_conflict",
                    "batch checkpoint does not match retained provider state",
                );
            }
        } else if request.expected_previous_sequence.is_some()
            || request.expected_previous_session_text_bytes != 0
        {
            return ServiceResponse::error(
                "checkpoint_conflict",
                "initial batch declares non-empty previous provider state",
            );
        }
        let estimated_bytes = request.records.iter().fold(0_u64, |total, record| {
            total.saturating_add(record.indexed_bytes)
        });
        let current_bytes = directory_size(&engine.root.join(INDEX_DIRECTORY));
        if current_bytes.saturating_add(estimated_bytes) > config.quota_bytes {
            return ServiceResponse::error("quota_exceeded", "provider storage quota exceeded");
        }
        let mut writer = engine
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for record in &request.records {
            if cancellation.is_cancelled() {
                let _ = writer.rollback();
                return ServiceResponse::error("cancelled", "ingestion cancelled before commit");
            }
            writer.delete_term(Term::from_field_text(
                engine.fields.record_id,
                &provider_document_id(record.locator.session_id, &record.record_id),
            ));
            let document = record_document(engine.fields, record);
            if let Err(error) = writer.add_document(document) {
                let _ = writer.rollback();
                return error_response(&ProviderError::index(error));
            }
        }
        let batch_text_bytes = request.records.iter().fold(0_u64, |total, record| {
            total.saturating_add(record.normalized_bytes)
        });
        let session = state
            .sessions
            .get(&request.generation.session_id)
            .cloned()
            .unwrap_or_else(|| PersistedSessionState {
                generation_fingerprint: request.generation.fingerprint.clone(),
                canonical_tail_sequence: request.generation.last_sequence,
                indexed_through_sequence: 0,
                session_text_bytes: 0,
                content_kinds: BTreeSet::new(),
                record_count: 0,
                truncated_records: 0,
            });
        let mut session = session;
        session.indexed_through_sequence = last_sequence;
        session.canonical_tail_sequence = request.generation.last_sequence;
        session.session_text_bytes = session.session_text_bytes.saturating_add(batch_text_bytes);
        session
            .content_kinds
            .extend(request.records.iter().map(|record| record.content_kind));
        session.record_count = session
            .record_count
            .saturating_add(u64::try_from(request.records.len()).unwrap_or(u64::MAX));
        session.truncated_records = session.truncated_records.saturating_add(
            u64::try_from(
                request
                    .records
                    .iter()
                    .filter(|record| record.truncated)
                    .count(),
            )
            .unwrap_or(u64::MAX),
        );
        let marker = CommitMarker::ApplyBatch {
            version: 1,
            batch_id: request.batch_id.clone(),
            operation_digest,
            session_id: request.generation.session_id,
            session,
        };
        let marker_payload = match serde_json::to_string(&marker) {
            Ok(payload) => payload,
            Err(error) => {
                let _ = writer.rollback();
                return error_response(&ProviderError::index(error));
            }
        };
        let mut prepared = match writer.prepare_commit() {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = writer.rollback();
                return error_response(&ProviderError::index(error));
            }
        };
        prepared.set_payload(&marker_payload);
        if let Err(error) = prepared.commit() {
            let _ = writer.rollback();
            return error_response(&ProviderError::index(error));
        }
        if let Err(error) = engine.reader.reload() {
            return error_response(&ProviderError::index(error));
        }
        if let Err(error) = marker.apply_to(&mut state) {
            return error_response(&error);
        }
        if let Err(error) = persist_state(&engine.root, &state) {
            return error_response(&error);
        }
        drop(state);
        drop(writer);
        json_response(&ApplySearchRecordsResponse {
            batch_id: request.batch_id.clone(),
            outcome: ApplyBatchOutcome::Applied,
            applied_records: request.records.len(),
            indexed_through_sequence: last_sequence,
        })
    }

    fn remove_session(
        &self,
        config: &ProviderConfig,
        request: &RemoveSessionSearchRequest,
    ) -> ServiceResponse {
        let _lifecycle = self
            .lifecycle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let engine = match self.ready_engine(config) {
            Ok(guard) => guard,
            Err(error) => return error_response(&error),
        };
        let mut state = engine
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(expected) = &request.expected_generation_fingerprint
            && state
                .sessions
                .get(&request.session_id)
                .is_some_and(|session| &session.generation_fingerprint != expected)
        {
            return ServiceResponse::error(
                "generation_conflict",
                "session generation does not match remove request",
            );
        }
        let mut writer = engine
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.delete_term(Term::from_field_text(
            engine.fields.session_id,
            &request.session_id.to_string(),
        ));
        let marker = CommitMarker::RemoveSession {
            version: 1,
            session_id: request.session_id,
            expected_generation_fingerprint: request.expected_generation_fingerprint.clone(),
        };
        let marker_payload = match serde_json::to_string(&marker) {
            Ok(payload) => payload,
            Err(error) => {
                let _ = writer.rollback();
                return error_response(&ProviderError::index(error));
            }
        };
        let mut prepared = match writer.prepare_commit() {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = writer.rollback();
                return error_response(&ProviderError::index(error));
            }
        };
        prepared.set_payload(&marker_payload);
        if let Err(error) = prepared.commit() {
            let _ = writer.rollback();
            return error_response(&ProviderError::index(error));
        }
        if let Err(error) = engine.reader.reload() {
            return error_response(&ProviderError::index(error));
        }
        if let Err(error) = marker.apply_to(&mut state) {
            return error_response(&error);
        }
        if let Err(error) = persist_state(&engine.root, &state) {
            return error_response(&error);
        }
        drop(state);
        drop(writer);
        ServiceResponse::empty()
    }

    fn rebuild(
        &self,
        config: &ProviderConfig,
        request: &RebuildSessionSearchRequest,
    ) -> ServiceResponse {
        if request.provider_id != PLUGIN_ID || request.confirmation != REBUILD_CONFIRMATION {
            return ServiceResponse::error(
                "confirmation_required",
                format!("rebuild requires exact confirmation '{REBUILD_CONFIRMATION}'"),
            );
        }
        let Some(configured_root) = &config.storage_root else {
            return ServiceResponse::error(
                "invalid_configuration",
                "provider storage_root is not configured",
            );
        };
        let _lifecycle = self
            .lifecycle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = match confined_storage_root(configured_root) {
            Ok(root) => root,
            Err(error) => return error_response(&error),
        };
        let mut guard = self
            .engine
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = EngineState::Uninitialized;
        if root.exists()
            && let Err(error) = std::fs::remove_dir_all(&root)
        {
            return ServiceResponse::error("rebuild_failed", error.to_string());
        }
        if let Err(error) = std::fs::create_dir_all(&root)
            .and_then(|()| std::fs::write(root.join(REBUILD_MARKER_FILE), b"rebuild in progress\n"))
        {
            return ServiceResponse::error("rebuild_failed", error.to_string());
        }
        match SearchEngine::open(root.clone(), config) {
            Ok(engine) => {
                if let Err(error) = std::fs::remove_file(root.join(REBUILD_MARKER_FILE)) {
                    *guard = EngineState::failed(&error);
                    drop(guard);
                    return ServiceResponse::error("rebuild_failed", error.to_string());
                }
                *guard = EngineState::Ready(Arc::new(engine));
                drop(guard);
                json_response(&RebuildSessionSearchResponse {
                    provider_id: PLUGIN_ID.to_owned(),
                    record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
                    normalization_version: CURRENT_NORMALIZATION_VERSION,
                    policy_version: CURRENT_SEARCH_POLICY_VERSION,
                })
            }
            Err(error) => {
                *guard = EngineState::failed(&error);
                drop(guard);
                error_response(&error)
            }
        }
    }

    fn purge(
        &self,
        config: &ProviderConfig,
        request: &PurgeSessionSearchRequest,
    ) -> ServiceResponse {
        if request.provider_id != PLUGIN_ID || request.confirmation != PURGE_CONFIRMATION {
            return ServiceResponse::error(
                "confirmation_required",
                format!("purge requires exact confirmation '{PURGE_CONFIRMATION}'"),
            );
        }
        let Some(root) = &config.storage_root else {
            return ServiceResponse::empty();
        };
        let _lifecycle = self
            .lifecycle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = match confined_storage_root(root) {
            Ok(root) => root,
            Err(error) => return error_response(&error),
        };
        {
            let mut guard = self
                .engine
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = EngineState::Uninitialized;
        }
        if root.exists()
            && let Err(error) = std::fs::remove_dir_all(&root)
        {
            return ServiceResponse::error("purge_failed", error.to_string());
        }
        ServiceResponse::empty()
    }

    fn ready_engine(&self, config: &ProviderConfig) -> Result<Arc<SearchEngine>, ProviderError> {
        let root = config.storage_root.as_ref().ok_or_else(|| {
            ProviderError::configuration("provider storage_root is not configured".to_owned())
        })?;
        let root = confined_storage_root(root)?;
        if root.join(REBUILD_MARKER_FILE).is_file() {
            return Err(ProviderError::incompatible(
                "provider rebuild was interrupted; retry explicit rebuild".to_owned(),
            ));
        }
        let mut guard = self
            .engine
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let should_open = match &*guard {
            EngineState::Uninitialized => true,
            EngineState::Failed { failed_at, .. } => {
                failed_at.elapsed() >= ENGINE_OPEN_RETRY_COOLDOWN
            }
            EngineState::Ready(_) => false,
        };
        if should_open {
            match SearchEngine::open(root, config) {
                Ok(engine) => *guard = EngineState::Ready(Arc::new(engine)),
                Err(error) => {
                    *guard = EngineState::failed(&error);
                    return Err(error);
                }
            }
        }
        match &*guard {
            EngineState::Ready(engine) => Ok(Arc::clone(engine)),
            EngineState::Failed { message, .. } => Err(ProviderError::index(message)),
            EngineState::Uninitialized => Err(ProviderError::index(
                "provider initialization did not reach a terminal state",
            )),
        }
    }
}

impl SearchEngine {
    fn open(root: PathBuf, config: &ProviderConfig) -> Result<Self, ProviderError> {
        std::fs::create_dir_all(&root).map_err(ProviderError::index)?;
        let index_dir = root.join(INDEX_DIRECTORY);
        std::fs::create_dir_all(&index_dir).map_err(ProviderError::index)?;
        let schema = build_schema();
        let directory = MmapDirectory::open(&index_dir).map_err(ProviderError::index)?;
        let index =
            Index::open_or_create(directory, schema.clone()).map_err(ProviderError::index)?;
        if index.schema() != schema {
            return Err(ProviderError::incompatible(
                "Tantivy schema changed; explicit rebuild is required".to_owned(),
            ));
        }
        let fields = Fields::from_schema(&schema)?;
        let writer = index
            .writer::<TantivyDocument>(config.writer_memory_bytes)
            .map_err(ProviderError::index)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(ProviderError::index)?;
        let mut state = load_state(&root, config)?;
        reconcile_commit_marker(&index, &root, &mut state)?;
        Ok(Self {
            root,
            index,
            reader,
            writer: Mutex::new(writer),
            fields,
            state: Mutex::new(state),
        })
    }
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    let text_indexing = TextFieldIndexing::default()
        .set_tokenizer("default")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text_options = TextOptions::default()
        .set_indexing_options(text_indexing)
        .set_stored();
    builder.add_text_field("record_id", STRING | STORED);
    builder.add_text_field("session_id", STRING | STORED);
    builder.add_u64_field("sequence", FAST | STORED);
    builder.add_u64_field("timestamp_ms", FAST | STORED);
    builder.add_text_field("content_kind", STRING | STORED);
    builder.add_text_field("matched_field", STRING | STORED);
    builder.add_text_field("text", text_options);
    builder.add_text_field("preview", STORED);
    builder.add_u64_field("preview_truncated", FAST | STORED);
    for field in [
        "working_directory",
        "role",
        "tool_name",
        "tool_status",
        "provider",
        "model",
        "agent",
        "source",
    ] {
        builder.add_text_field(field, STRING | STORED);
    }
    builder.build()
}

fn provider_document_id(session_id: SessionId, record_id: &str) -> String {
    format!("{session_id}:{record_id}")
}

fn record_document(
    fields: Fields,
    record: &bcode_session_search::SessionSearchRecord,
) -> TantivyDocument {
    let mut document = TantivyDocument::default();
    document.add_text(
        fields.record_id,
        provider_document_id(record.locator.session_id, &record.record_id),
    );
    document.add_text(fields.session_id, record.locator.session_id.to_string());
    document.add_u64(fields.sequence, record.locator.sequence);
    document.add_u64(fields.timestamp_ms, record.timestamp_ms);
    document.add_text(fields.content_kind, content_kind_name(record.content_kind));
    document.add_text(
        fields.matched_field,
        field_name(record.field.unwrap_or(SearchField::Text)),
    );
    if let Some(text) = &record.text {
        document.add_text(fields.text, text);
        let (preview, preview_truncated) = bounded_preview(text);
        document.add_text(fields.preview, preview);
        document.add_u64(fields.preview_truncated, u64::from(preview_truncated));
    }
    for (key, field) in attribute_fields(fields) {
        if let Some(value) = record.attributes.get(key) {
            document.add_text(field, value);
        }
    }
    document
}

const fn attribute_fields(fields: Fields) -> [(&'static str, Field); 8] {
    [
        ("working_directory", fields.working_directory),
        ("role", fields.role),
        ("tool_name", fields.tool_name),
        ("tool_status", fields.tool_status),
        ("provider", fields.provider),
        ("model", fields.model),
        ("agent", fields.agent),
        ("source", fields.source),
    ]
}

fn build_query(
    engine: &SearchEngine,
    request: &SessionSearchRequest,
) -> Result<Box<dyn Query>, ProviderError> {
    let query = build_text_query(engine, &request.query)?;
    let mut clauses = vec![(Occur::Must, query)];
    add_set_filter(
        &mut clauses,
        engine.fields.session_id,
        request.filters.session_ids.iter().map(ToString::to_string),
    );
    add_set_filter(
        &mut clauses,
        engine.fields.content_kind,
        request
            .filters
            .content_kinds
            .iter()
            .copied()
            .map(content_kind_name),
    );
    add_exact_filter(
        &mut clauses,
        engine.fields.working_directory,
        request
            .filters
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
    );
    add_set_filter(
        &mut clauses,
        engine.fields.role,
        request.filters.roles.iter().map(|role| format!("{role:?}")),
    );
    for (field, values) in [
        (engine.fields.tool_name, &request.filters.tool_names),
        (engine.fields.tool_status, &request.filters.tool_statuses),
        (engine.fields.provider, &request.filters.providers),
        (engine.fields.model, &request.filters.models),
        (engine.fields.agent, &request.filters.agents),
        (engine.fields.source, &request.filters.sources),
    ] {
        add_set_filter(&mut clauses, field, values.iter().cloned());
    }
    if request.filters.after_timestamp_ms.is_some() || request.filters.before_timestamp_ms.is_some()
    {
        use std::ops::Bound::{Included, Unbounded};
        let lower = request
            .filters
            .after_timestamp_ms
            .map_or(Unbounded, |value| {
                Included(Term::from_field_u64(engine.fields.timestamp_ms, value))
            });
        let upper = request
            .filters
            .before_timestamp_ms
            .map_or(Unbounded, |value| {
                Included(Term::from_field_u64(engine.fields.timestamp_ms, value))
            });
        clauses.push((
            Occur::Must,
            Box::new(tantivy::query::RangeQuery::new(lower, upper)),
        ));
    }
    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn build_text_query(
    engine: &SearchEngine,
    query: &bcode_session_search::SessionSearchQuery,
) -> Result<Box<dyn Query>, ProviderError> {
    use bcode_session_search::SessionSearchQuery;
    match query {
        SessionSearchQuery::Text { text, mode, fields } => {
            if !fields.is_empty() && !fields.contains(&SearchField::Text) {
                return Err(ProviderError::configuration(
                    "this provider searches normalized record text only".to_owned(),
                ));
            }
            let mut parser = QueryParser::for_index(&engine.index, vec![engine.fields.text]);
            if *mode == TextMatchMode::Regex {
                parser.allow_regexes();
            }
            if *mode == TextMatchMode::Fuzzy {
                parser.set_field_fuzzy(engine.fields.text, false, 1, true);
            }
            let encoded = match mode {
                TextMatchMode::Terms => text.clone(),
                TextMatchMode::Phrase => format!("\"{}\"", escape_query_text(text)),
                TextMatchMode::Prefix => format!("{}*", escape_query_text(text)),
                TextMatchMode::Regex => format!("text:/{text}/"),
                TextMatchMode::Fuzzy => escape_query_text(text),
            };
            parser.parse_query(&encoded).map_err(ProviderError::index)
        }
        SessionSearchQuery::And { clauses } => Ok(Box::new(BooleanQuery::intersection(
            clauses
                .iter()
                .map(|clause| build_text_query(engine, clause))
                .collect::<Result<Vec<_>, _>>()?,
        ))),
        SessionSearchQuery::Or { clauses } => Ok(Box::new(BooleanQuery::union(
            clauses
                .iter()
                .map(|clause| build_text_query(engine, clause))
                .collect::<Result<Vec<_>, _>>()?,
        ))),
        SessionSearchQuery::Not { clause } => Ok(Box::new(BooleanQuery::new(vec![
            (Occur::Must, Box::new(tantivy::query::AllQuery)),
            (Occur::MustNot, build_text_query(engine, clause)?),
        ]))),
    }
}

fn add_set_filter<T>(clauses: &mut Vec<(Occur, Box<dyn Query>)>, field: Field, values: T)
where
    T: IntoIterator,
    T::Item: AsRef<str>,
{
    let terms = values
        .into_iter()
        .map(|value| Term::from_field_text(field, value.as_ref()))
        .collect::<Vec<_>>();
    if !terms.is_empty() {
        clauses.push((Occur::Must, Box::new(TermSetQuery::new(terms))));
    }
}

fn add_exact_filter(
    clauses: &mut Vec<(Occur, Box<dyn Query>)>,
    field: Field,
    value: Option<String>,
) {
    if let Some(value) = value {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(field, &value),
                IndexRecordOption::Basic,
            )),
        ));
    }
}

fn document_hit(
    fields: Fields,
    document: &TantivyDocument,
    rank: usize,
    score: f32,
) -> Result<SessionSearchHit, ProviderError> {
    let string = |field, name| {
        document
            .get_first(field)
            .and_then(|value| value.as_value().as_str())
            .map(str::to_owned)
            .ok_or_else(|| ProviderError::incompatible(format!("stored document lacks {name}")))
    };
    let u64_value = |field, name| {
        document
            .get_first(field)
            .and_then(|value| value.as_value().as_u64())
            .ok_or_else(|| ProviderError::incompatible(format!("stored document lacks {name}")))
    };
    let session_id = string(fields.session_id, "session_id")?
        .parse::<SessionId>()
        .map_err(ProviderError::index)?;
    let stored_record_id = string(fields.record_id, "record_id")?;
    let record_id = stored_record_id
        .strip_prefix(&format!("{session_id}:"))
        .unwrap_or(&stored_record_id)
        .to_owned();
    Ok(SessionSearchHit {
        locator: SessionSearchLocator {
            session_id,
            sequence: u64_value(fields.sequence, "sequence")?,
            record_id: Some(record_id),
        },
        content_kind: parse_content_kind(&string(fields.content_kind, "content_kind")?)?,
        matched_field: parse_field(&string(fields.matched_field, "matched_field")?)?,
        provider_id: PLUGIN_ID.to_owned(),
        provider_rank: u32::try_from(rank + 1).unwrap_or(u32::MAX),
        provider_score: Some(format!("{score:.6}")),
        preview: document
            .get_first(fields.preview)
            .and_then(|value| value.as_value().as_str())
            .map(str::to_owned),
        preview_truncated: document
            .get_first(fields.preview_truncated)
            .and_then(|value| value.as_value().as_u64())
            .is_some_and(|value| value != 0),
    })
}

fn capabilities(config: &ProviderConfig) -> SessionSearchCapabilities {
    SessionSearchCapabilities {
        provider_id: PLUGIN_ID.to_owned(),
        execution: SearchExecutionKind::Indexed,
        content_kinds: config.allowed_content(),
        features: BTreeSet::from([
            SearchFeature::Terms,
            SearchFeature::Phrase,
            SearchFeature::Prefix,
            SearchFeature::Regex,
            SearchFeature::Fuzzy,
            SearchFeature::StructuredFilters,
            SearchFeature::RelevanceSort,
            SearchFeature::IncrementalIngestion,
            SearchFeature::HistoricalBackfill,
            SearchFeature::RemoveSession,
            SearchFeature::Rebuild,
            SearchFeature::Purge,
        ]),
        max_hits: bcode_session_search::MAX_SEARCH_HITS,
        max_batch_records: bcode_session_search::MAX_INGEST_RECORDS,
        max_batch_text_bytes: bcode_session_search::MAX_INGEST_TEXT_BYTES,
    }
}

fn empty_status(config: &ProviderConfig) -> SessionSearchStatus {
    SessionSearchStatus {
        provider_id: PLUGIN_ID.to_owned(),
        state: if config.storage_root.is_some() {
            SearchProviderState::Ready
        } else {
            SearchProviderState::Disabled
        },
        record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
        normalization_version: CURRENT_NORMALIZATION_VERSION,
        policy_version: CURRENT_SEARCH_POLICY_VERSION,
        index_bytes: 0,
        quota_bytes: config.quota_bytes,
        document_count: 0,
        pending_sessions: 0,
        coverage: Vec::new(),
        degraded_reason: None,
    }
}

fn coverage_complete_for_request(engine: &SearchEngine, request: &SessionSearchRequest) -> bool {
    let state = engine
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let complete = if request.filters.session_ids.is_empty() {
        !state.sessions.is_empty()
            && state.sessions.values().all(|session| {
                session
                    .canonical_tail_sequence
                    .is_some_and(|tail| tail == session.indexed_through_sequence)
            })
    } else {
        request.filters.session_ids.iter().all(|session_id| {
            state.sessions.get(session_id).is_some_and(|session| {
                session
                    .canonical_tail_sequence
                    .is_some_and(|tail| tail == session.indexed_through_sequence)
            })
        })
    };
    drop(state);
    complete
}

fn requested_content(
    request: &SessionSearchRequest,
    config: &ProviderConfig,
) -> Vec<SearchContentKind> {
    if request.filters.content_kinds.is_empty() {
        config.allowed_content().into_iter().collect()
    } else {
        request.filters.content_kinds.iter().copied().collect()
    }
}

fn reconcile_commit_marker(
    index: &Index,
    root: &Path,
    state: &mut PersistedProviderState,
) -> Result<(), ProviderError> {
    let Some(payload) = index.load_metas().map_err(ProviderError::index)?.payload else {
        return Ok(());
    };
    let marker = serde_json::from_str::<CommitMarker>(&payload).map_err(|error| {
        ProviderError::incompatible(format!("Tantivy commit marker is corrupt: {error}"))
    })?;
    marker.apply_to(state)?;
    persist_state(root, state)
}

fn load_state(
    root: &Path,
    config: &ProviderConfig,
) -> Result<PersistedProviderState, ProviderError> {
    let path = root.join(CHECKPOINT_FILE);
    if !path.exists() {
        return Ok(PersistedProviderState::new(config));
    }
    let bytes = std::fs::read(&path).map_err(ProviderError::index)?;
    let state =
        serde_json::from_slice::<PersistedProviderState>(&bytes).map_err(ProviderError::index)?;
    state.validate(config)?;
    Ok(state)
}

fn persist_state(root: &Path, state: &PersistedProviderState) -> Result<(), ProviderError> {
    let path = root.join(CHECKPOINT_FILE);
    let temporary = root.join(format!("{CHECKPOINT_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(state).map_err(ProviderError::index)?;
    std::fs::write(&temporary, bytes).map_err(ProviderError::index)?;
    std::fs::rename(&temporary, &path).map_err(ProviderError::index)
}

fn confined_storage_root(configured: &Path) -> Result<PathBuf, ProviderError> {
    if !configured.is_absolute() {
        return Err(ProviderError::configuration(
            "storage_root must be absolute".to_owned(),
        ));
    }
    if configured.file_name().is_none() {
        return Err(ProviderError::configuration(
            "storage_root must identify a provider-owned directory".to_owned(),
        ));
    }
    if configured
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ProviderError::configuration(
            "storage_root must not contain parent traversal".to_owned(),
        ));
    }
    reject_symlink_components(configured)?;
    if configured.exists() {
        let metadata = std::fs::symlink_metadata(configured).map_err(ProviderError::index)?;
        if !metadata.is_dir() {
            return Err(ProviderError::configuration(
                "storage_root must identify a directory".to_owned(),
            ));
        }
        configured.canonicalize().map_err(ProviderError::index)
    } else {
        let mut existing = configured.parent().ok_or_else(|| {
            ProviderError::configuration("storage_root has no parent directory".to_owned())
        })?;
        let mut missing = Vec::new();
        while !existing.exists() {
            let name = existing.file_name().ok_or_else(|| {
                ProviderError::configuration(
                    "storage_root has no existing confined ancestor".to_owned(),
                )
            })?;
            missing.push(name.to_os_string());
            existing = existing.parent().ok_or_else(|| {
                ProviderError::configuration(
                    "storage_root has no existing confined ancestor".to_owned(),
                )
            })?;
        }
        let metadata = std::fs::symlink_metadata(existing).map_err(ProviderError::index)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(ProviderError::configuration(
                "storage_root ancestor must be a non-symlink directory".to_owned(),
            ));
        }
        let mut confined = existing.canonicalize().map_err(ProviderError::index)?;
        for component in missing.into_iter().rev() {
            confined.push(component);
        }
        confined.push(configured.file_name().ok_or_else(|| {
            ProviderError::configuration("storage_root has no final component".to_owned())
        })?);
        Ok(confined)
    }
}

fn reject_symlink_components(configured: &Path) -> Result<(), ProviderError> {
    for path in [Some(configured), configured.parent()]
        .into_iter()
        .flatten()
    {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ProviderError::configuration(
                    "storage_root and its provider parent must not be symbolic links".to_owned(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ProviderError::index(error)),
        }
    }
    Ok(())
}

fn directory_size(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries.flatten().fold(0_u64, |total, entry| {
        let path = entry.path();
        match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => total.saturating_add(directory_size(&path)),
            Ok(metadata) => total.saturating_add(metadata.len()),
            Err(_) => total,
        }
    })
}

fn search_query_fingerprint(request: &SessionSearchRequest) -> String {
    let mut stable = request.clone();
    stable.cursor = None;
    let bytes = serde_json::to_vec(&stable).expect("validated search request serializes");
    format!("{:x}", Sha256::digest(bytes))
}

fn decode_search_offset(request: &SessionSearchRequest) -> Result<usize, ProviderError> {
    let Some(cursor) = &request.cursor else {
        return Ok(0);
    };
    let expected = search_query_fingerprint(request);
    if cursor.query_fingerprint != expected {
        return Err(ProviderError::invalid_request(
            "search cursor does not match this query".to_owned(),
        ));
    }
    cursor
        .value
        .parse::<usize>()
        .map_err(|_| ProviderError::invalid_request("search cursor is invalid".to_owned()))
}

fn bounded_preview(text: &str) -> (&str, bool) {
    if text.len() <= MAX_PREVIEW_BYTES {
        return (text, false);
    }
    let mut end = MAX_PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (&text[..end], true)
}

fn bounded_message(message: &str) -> String {
    let (bounded, truncated) = bounded_preview(message);
    if truncated {
        format!("{bounded}…")
    } else {
        bounded.to_owned()
    }
}

fn escape_query_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '+' | '^' | '`' | ':' | '{' | '}' | '"' | '[' | ']' | '(' | ')' | '\\' | '!' | '*'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

const fn content_kind_name(kind: SearchContentKind) -> &'static str {
    match kind {
        SearchContentKind::SessionTitle => "session_title",
        SearchContentKind::UserMessage => "user_message",
        SearchContentKind::AssistantMessage => "assistant_message",
        SearchContentKind::AssistantReasoning => "assistant_reasoning",
        SearchContentKind::SystemMessage => "system_message",
        SearchContentKind::ShellCommand => "shell_command",
        SearchContentKind::ShellOutput => "shell_output",
        SearchContentKind::ToolArguments => "tool_arguments",
        SearchContentKind::ToolOutput => "tool_output",
        SearchContentKind::ToolError => "tool_error",
        SearchContentKind::Permission => "permission",
        SearchContentKind::RuntimeDiagnostic => "runtime_diagnostic",
        SearchContentKind::Compaction => "compaction",
        SearchContentKind::TraceMetadata => "trace_metadata",
        SearchContentKind::ArtifactMetadata => "artifact_metadata",
    }
}

fn parse_content_kind(value: &str) -> Result<SearchContentKind, ProviderError> {
    [
        SearchContentKind::SessionTitle,
        SearchContentKind::UserMessage,
        SearchContentKind::AssistantMessage,
        SearchContentKind::AssistantReasoning,
        SearchContentKind::SystemMessage,
        SearchContentKind::ShellCommand,
        SearchContentKind::ShellOutput,
        SearchContentKind::ToolArguments,
        SearchContentKind::ToolOutput,
        SearchContentKind::ToolError,
        SearchContentKind::Permission,
        SearchContentKind::RuntimeDiagnostic,
        SearchContentKind::Compaction,
        SearchContentKind::TraceMetadata,
        SearchContentKind::ArtifactMetadata,
    ]
    .into_iter()
    .find(|kind| content_kind_name(*kind) == value)
    .ok_or_else(|| ProviderError::incompatible(format!("unknown stored content kind '{value}'")))
}

const fn field_name(field: SearchField) -> &'static str {
    match field {
        SearchField::Title => "title",
        SearchField::Text => "text",
        SearchField::Command => "command",
        SearchField::StandardOutput => "standard_output",
        SearchField::StandardError => "standard_error",
        SearchField::ToolName => "tool_name",
        SearchField::ToolArguments => "tool_arguments",
        SearchField::ErrorMessage => "error_message",
        SearchField::WorkingDirectory => "working_directory",
        SearchField::Provider => "provider",
        SearchField::Model => "model",
        SearchField::Agent => "agent",
        SearchField::Source => "source",
    }
}

fn parse_field(value: &str) -> Result<SearchField, ProviderError> {
    [
        SearchField::Title,
        SearchField::Text,
        SearchField::Command,
        SearchField::StandardOutput,
        SearchField::StandardError,
        SearchField::ToolName,
        SearchField::ToolArguments,
        SearchField::ErrorMessage,
        SearchField::WorkingDirectory,
        SearchField::Provider,
        SearchField::Model,
        SearchField::Agent,
        SearchField::Source,
    ]
    .into_iter()
    .find(|field| field_name(*field) == value)
    .ok_or_else(|| ProviderError::incompatible(format!("unknown stored search field '{value}'")))
}

fn decode_request<T>(
    context: &NativeServiceContext,
    operation: impl FnOnce(T) -> ServiceResponse,
) -> ServiceResponse
where
    T: serde::de::DeserializeOwned,
{
    match context.request.payload_json::<T>() {
        Ok(request) => operation(request),
        Err(error) => ServiceResponse::error("invalid_request", error.to_string()),
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}

fn error_response(error: &ProviderError) -> ServiceResponse {
    ServiceResponse::error(error.code, bounded_message(&error.message))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        TantivySessionSearchPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(
    TantivySessionSearchPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::{
        NativeServiceContext, PluginConfigContext, ServiceBridge, ServiceCancellation,
        ServiceEventEmitter, ServiceRequest, TransientProgressLimits,
    };
    use bcode_session_search::{
        SearchCanonicalGeneration, SessionSearchFilters, SessionSearchQuery, SessionSearchRecord,
    };

    fn config(root: &Path) -> ProviderConfig {
        ProviderConfig {
            storage_root: Some(root.to_path_buf()),
            quota_bytes: 32 * 1024 * 1024,
            writer_memory_bytes: MIN_WRITER_MEMORY_BYTES,
            ..ProviderConfig::default()
        }
    }

    fn record(session_id: SessionId, sequence: u64, text: &str) -> SessionSearchRecord {
        SessionSearchRecord {
            schema_version: CURRENT_SEARCH_RECORD_VERSION,
            record_id: format!("{session_id}-{sequence}"),
            locator: SessionSearchLocator {
                session_id,
                sequence,
                record_id: Some(format!("{session_id}-{sequence}")),
            },
            timestamp_ms: sequence,
            content_kind: SearchContentKind::UserMessage,
            field: Some(SearchField::Text),
            text: Some(text.to_owned()),
            attributes: BTreeMap::from([("role".to_owned(), "User".to_owned())]),
            source_bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
            normalized_bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
            indexed_bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
            truncated: false,
            source_range_start: None,
            source_range_end: None,
            chunk_ordinal: None,
            chunk_count: None,
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
        }
    }

    fn request_with_mode(text: &str, mode: TextMatchMode) -> SessionSearchRequest {
        SessionSearchRequest {
            query: SessionSearchQuery::Text {
                text: text.to_owned(),
                mode,
                fields: BTreeSet::new(),
            },
            filters: SessionSearchFilters {
                content_kinds: BTreeSet::from([SearchContentKind::UserMessage]),
                ..SessionSearchFilters::default()
            },
            sort: SessionSearchSort::ProviderRelevance,
            limit: 10,
            cursor: None,
            deadline_ms: Some(1_000),
        }
    }

    fn request(text: &str) -> SessionSearchRequest {
        request_with_mode(text, TextMatchMode::Terms)
    }

    fn index_records(engine: &SearchEngine, records: &[SessionSearchRecord]) {
        let mut writer = engine.writer.lock().expect("writer");
        for record in records {
            writer
                .add_document(record_document(engine.fields, record))
                .expect("add");
        }
        writer.commit().expect("commit");
        drop(writer);
        engine.reader.reload().expect("reload");
    }

    fn search_count(engine: &SearchEngine, request: &SessionSearchRequest) -> usize {
        let query = build_query(engine, request).expect("query");
        engine
            .reader
            .searcher()
            .search(&query, &TopDocs::with_limit(20).order_by_score())
            .expect("search")
            .len()
    }

    fn service_context(configured_root: &Path, managed_root: &Path) -> NativeServiceContext {
        NativeServiceContext {
            plugin_id: PLUGIN_ID.to_owned(),
            request: ServiceRequest {
                interface_id: SESSION_SEARCH_INTERFACE_ID.to_owned(),
                operation: OP_STATUS.to_owned(),
                payload: Vec::new(),
            },
            config: PluginConfigContext {
                config: serde_json::json!({
                    "storage_root": configured_root,
                    "writer_memory_bytes": MIN_WRITER_MEMORY_BYTES,
                }),
                state_root: Some(managed_root.to_path_buf()),
                ..PluginConfigContext::default()
            },
            events: ServiceEventEmitter::default(),
            cancellation: ServiceCancellation::default(),
            bridge: ServiceBridge::default(),
            transient_progress_limits: TransientProgressLimits::default(),
        }
    }

    #[test]
    fn explicit_storage_root_takes_precedence_over_managed_root() {
        let parent = tempfile::tempdir().expect("parent");
        let configured_root = parent.path().join("configured");
        let managed_root = parent.path().join("managed");
        let plugin = TantivySessionSearchPlugin::default();

        let response =
            plugin.invoke_service_concurrent(service_context(&configured_root, &managed_root));

        assert!(response.error.is_none());
        assert!(configured_root.exists());
        assert!(!managed_root.exists());
    }

    #[test]
    fn supported_text_modes_convert_to_real_tantivy_queries() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
        let session_id = SessionId::new();
        index_records(
            &engine,
            &[
                record(session_id, 1, "database locking failure"),
                record(session_id, 2, "unrelated transcript"),
            ],
        );
        for (mode, text) in [
            (TextMatchMode::Terms, "database failure"),
            (TextMatchMode::Phrase, "database locking"),
            (TextMatchMode::Prefix, "locking"),
            (TextMatchMode::Regex, "lock.*"),
            (TextMatchMode::Fuzzy, "databaze"),
        ] {
            assert!(
                search_count(&engine, &request_with_mode(text, mode)) >= 1,
                "mode {mode:?}"
            );
        }
    }

    #[test]
    fn boolean_queries_and_structured_filters_are_applied_without_approximation() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let mut first = record(first_session, 1, "database locking failure");
        first.timestamp_ms = 100;
        first
            .attributes
            .insert("working_directory".to_owned(), "/workspace/one".to_owned());
        first
            .attributes
            .insert("tool_name".to_owned(), "shell.run".to_owned());
        let mut second = record(second_session, 2, "database unrelated");
        second.timestamp_ms = 200;
        second
            .attributes
            .insert("working_directory".to_owned(), "/workspace/two".to_owned());
        index_records(&engine, &[first, second]);

        let mut filtered = request("database");
        filtered.query = SessionSearchQuery::And {
            clauses: vec![
                SessionSearchQuery::Text {
                    text: "database".to_owned(),
                    mode: TextMatchMode::Terms,
                    fields: BTreeSet::new(),
                },
                SessionSearchQuery::Not {
                    clause: Box::new(SessionSearchQuery::Text {
                        text: "unrelated".to_owned(),
                        mode: TextMatchMode::Terms,
                        fields: BTreeSet::new(),
                    }),
                },
            ],
        };
        filtered.filters.session_ids.insert(first_session);
        filtered.filters.working_directory = Some(PathBuf::from("/workspace/one"));
        filtered.filters.tool_names.insert("shell.run".to_owned());
        filtered.filters.after_timestamp_ms = Some(100);
        filtered.filters.before_timestamp_ms = Some(150);
        assert_eq!(search_count(&engine, &filtered), 1);
    }

    #[test]
    fn unsupported_field_targeting_fails_explicitly() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
        let mut request = request("needle");
        request.query = SessionSearchQuery::Text {
            text: "needle".to_owned(),
            mode: TextMatchMode::Terms,
            fields: BTreeSet::from([SearchField::Command]),
        };
        assert!(matches!(
            build_query(&engine, &request),
            Err(ProviderError {
                code: "invalid_configuration",
                ..
            })
        ));
    }

    #[test]
    fn event_level_documents_round_trip_through_real_tantivy_index() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
        let session_id = SessionId::new();
        let projected = record(session_id, 7, "needle transcript text");
        {
            let mut writer = engine.writer.lock().expect("writer");
            writer
                .add_document(record_document(engine.fields, &projected))
                .expect("add");
            writer.commit().expect("commit");
        }
        engine.reader.reload().expect("reload");
        let query = build_query(&engine, &request("needle")).expect("query");
        let results = engine
            .reader
            .searcher()
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .expect("search");
        assert_eq!(results.len(), 1);
        let document = engine
            .reader
            .searcher()
            .doc::<TantivyDocument>(results[0].1)
            .expect("document");
        let hit = document_hit(engine.fields, &document, 0, results[0].0).expect("hit");
        assert_eq!(hit.locator.session_id, session_id);
        assert_eq!(hit.locator.sequence, 7);
        assert_eq!(hit.preview.as_deref(), Some("needle transcript text"));
    }

    #[test]
    fn duplicate_and_conflicting_batches_are_classified_before_index_mutation() {
        let session_id = SessionId::new();
        let request = ApplySearchRecordsRequest {
            provider_id: PLUGIN_ID.to_owned(),
            batch_id: "batch-1".to_owned(),
            generation: SearchCanonicalGeneration {
                session_id,
                fingerprint: "generation".to_owned(),
                last_sequence: Some(1),
            },
            expected_previous_sequence: None,
            expected_previous_session_text_bytes: 0,
            indexed_through_sequence: None,
            records: vec![record(session_id, 1, "hello")],
        };
        let BatchDeliveryClassification::New { operation_digest } =
            classify_batch_delivery(&request, None)
        else {
            panic!("new batch");
        };
        assert!(matches!(
            classify_batch_delivery(&request, Some(&operation_digest)),
            BatchDeliveryClassification::Duplicate { .. }
        ));
        let mut conflict = request;
        conflict.records[0].text = Some("different".to_owned());
        assert!(matches!(
            classify_batch_delivery(&conflict, Some(&operation_digest)),
            BatchDeliveryClassification::ConflictingDuplicate { .. }
        ));
    }

    fn apply_batch_request(
        session_id: SessionId,
        batch_id: &str,
        sequence: u64,
        text: &str,
    ) -> ApplySearchRecordsRequest {
        ApplySearchRecordsRequest {
            provider_id: PLUGIN_ID.to_owned(),
            batch_id: batch_id.to_owned(),
            generation: SearchCanonicalGeneration {
                session_id,
                fingerprint: "generation".to_owned(),
                last_sequence: Some(sequence),
            },
            expected_previous_sequence: None,
            expected_previous_session_text_bytes: 0,
            indexed_through_sequence: None,
            records: vec![record(session_id, sequence, text)],
        }
    }

    fn apply_to_engine(engine: &SearchEngine, request: &ApplySearchRecordsRequest) -> CommitMarker {
        let mut writer = engine.writer.lock().expect("writer");
        for record in &request.records {
            writer.delete_term(Term::from_field_text(
                engine.fields.record_id,
                &provider_document_id(record.locator.session_id, &record.record_id),
            ));
            writer
                .add_document(record_document(engine.fields, record))
                .expect("add document");
        }
        let operation_digest = request.operation_digest_sha256();
        let session = PersistedSessionState {
            generation_fingerprint: request.generation.fingerprint.clone(),
            canonical_tail_sequence: request.generation.last_sequence,
            indexed_through_sequence: request.records.last().expect("record").locator.sequence,
            session_text_bytes: request.records.iter().fold(0, |total, record| {
                total.saturating_add(record.normalized_bytes)
            }),
            content_kinds: request
                .records
                .iter()
                .map(|record| record.content_kind)
                .collect(),
            record_count: u64::try_from(request.records.len()).unwrap_or(u64::MAX),
            truncated_records: 0,
        };
        let marker = CommitMarker::ApplyBatch {
            version: 1,
            batch_id: request.batch_id.clone(),
            operation_digest,
            session_id: request.generation.session_id,
            session,
        };
        let payload = serde_json::to_string(&marker).expect("marker");
        let mut prepared = writer.prepare_commit().expect("prepare commit");
        prepared.set_payload(&payload);
        prepared.commit().expect("commit");
        drop(writer);
        marker
    }

    #[test]
    fn conflicting_duplicate_reports_the_requested_checkpoint_without_advancing_state() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let plugin = TantivySessionSearchPlugin::default();
        let session_id = SessionId::new();
        let request = apply_batch_request(session_id, "batch-conflict", 1, "original");
        let first = plugin.apply_batch(&config, &request, &ServiceCancellation::default());
        assert!(first.error.is_none());

        let mut conflicting = request;
        conflicting.records[0].text = Some("different".to_owned());
        conflicting.records[0].indexed_bytes = 9;
        conflicting.records[0].normalized_bytes = 9;
        let response = plugin.apply_batch(&config, &conflicting, &ServiceCancellation::default());
        let acknowledgment: ApplySearchRecordsResponse =
            response.payload_json().expect("conflict acknowledgment");
        assert_eq!(
            acknowledgment.outcome,
            ApplyBatchOutcome::ConflictingDuplicate
        );
        assert_eq!(acknowledgment.indexed_through_sequence, 1);
        assert_eq!(
            plugin.status(&config).coverage[0].indexed_through_sequence,
            Some(1)
        );
    }

    #[test]
    fn deletion_tombstone_rejects_stale_inflight_ingestion_across_restart() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let session_id = SessionId::new();
        let request = apply_batch_request(session_id, "batch-before-delete", 1, "retained");
        let plugin = TantivySessionSearchPlugin::default();
        assert!(
            plugin
                .apply_batch(&config, &request, &ServiceCancellation::default())
                .error
                .is_none()
        );
        assert!(
            plugin
                .remove_session(
                    &config,
                    &RemoveSessionSearchRequest {
                        session_id,
                        expected_generation_fingerprint: Some("generation".to_owned()),
                    },
                )
                .error
                .is_none()
        );
        drop(plugin);

        let restarted = TantivySessionSearchPlugin::default();
        let mut stale = request;
        stale.batch_id = "batch-after-delete".to_owned();
        stale.expected_previous_sequence = None;
        stale.expected_previous_session_text_bytes = 0;
        let response = restarted.apply_batch(&config, &stale, &ServiceCancellation::default());
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("stale_generation")
        );
        assert!(restarted.status(&config).coverage.is_empty());
    }

    #[test]
    fn restart_preserves_completed_checkpoint_and_classifies_duplicate_delivery() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let session_id = SessionId::new();
        let request = apply_batch_request(session_id, "batch-complete-restart", 1, "needle");
        {
            let plugin = TantivySessionSearchPlugin::default();
            let response = plugin.apply_batch(&config, &request, &ServiceCancellation::default());
            assert!(response.error.is_none());
            let status = plugin.status(&config);
            assert!(status.coverage[0].complete);
            assert_eq!(status.document_count, 1);
        }

        let before = TantivySessionSearchPlugin::default().status(&config);
        let restarted = TantivySessionSearchPlugin::default();
        let response = restarted.apply_batch(&config, &request, &ServiceCancellation::default());
        let acknowledgment: ApplySearchRecordsResponse =
            response.payload_json().expect("duplicate acknowledgment");
        assert_eq!(acknowledgment.outcome, ApplyBatchOutcome::Duplicate);
        let status = restarted.status(&config);
        assert_eq!(status.coverage.len(), 1);
        assert!(status.coverage[0].complete);
        assert_eq!(status.coverage[0].indexed_through_sequence, Some(1));
        assert_eq!(status.document_count, 1);
        assert_eq!(status.index_bytes, before.index_bytes);
        assert_eq!(status.coverage, before.coverage);
    }

    #[test]
    fn restart_reconciles_committed_index_marker_into_provider_state() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let session_id = SessionId::new();
        let request = apply_batch_request(session_id, "batch-restart", 1, "restart needle");
        {
            let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
            let _marker = apply_to_engine(&engine, &request);
            assert!(!root.path().join(CHECKPOINT_FILE).exists());
        }

        let reopened = SearchEngine::open(root.path().to_path_buf(), &config).expect("reopen");
        let state = reopened.state.lock().expect("state");
        assert_eq!(
            state.batch_digests.get("batch-restart"),
            Some(&request.operation_digest_sha256())
        );
        assert_eq!(state.sessions[&session_id].indexed_through_sequence, 1);
        drop(state);
        assert!(root.path().join(CHECKPOINT_FILE).exists());
    }

    #[test]
    fn coverage_is_incomplete_until_checkpoint_reaches_known_canonical_tail() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let session_id = SessionId::new();
        let mut batch = apply_batch_request(session_id, "batch-partial", 1, "partial needle");
        batch.generation.last_sequence = Some(2);
        {
            let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
            let marker = apply_to_engine(&engine, &batch);
            let mut state = engine.state.lock().expect("state");
            marker.apply_to(&mut state).expect("apply marker");
            persist_state(root.path(), &state).expect("persist");
        }
        let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("reopen");
        let mut search = request("partial");
        search.filters.session_ids.insert(session_id);
        assert!(!coverage_complete_for_request(&engine, &search));
    }

    #[test]
    fn corrupt_commit_marker_fails_closed_on_restart() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
        {
            let mut writer = engine.writer.lock().expect("writer");
            let mut prepared = writer.prepare_commit().expect("prepare");
            prepared.set_payload("not-json");
            prepared.commit().expect("commit");
            drop(writer);
        }
        drop(engine);
        assert!(matches!(
            SearchEngine::open(root.path().to_path_buf(), &config),
            Err(ProviderError {
                code: "incompatible_index",
                ..
            })
        ));
    }

    #[test]
    fn disabled_status_opens_no_index_or_writer_resources() {
        let plugin = TantivySessionSearchPlugin::default();
        let config = ProviderConfig::default();
        let status = plugin.status(&config);
        assert_eq!(status.state, SearchProviderState::Disabled);
        assert!(matches!(
            *plugin.engine.read().expect("engine state"),
            EngineState::Uninitialized
        ));
    }

    #[test]
    fn configured_status_opens_existing_state_and_reports_document_count() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let session_id = SessionId::new();
        let batch = apply_batch_request(session_id, "batch-status", 1, "status needle");
        {
            let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
            let marker = apply_to_engine(&engine, &batch);
            let mut state = engine.state.lock().expect("state");
            marker.apply_to(&mut state).expect("apply marker");
            persist_state(root.path(), &state).expect("persist");
        }
        let plugin = TantivySessionSearchPlugin::default();
        let status = plugin.status(&config);
        assert_eq!(status.state, SearchProviderState::Ready);
        assert_eq!(status.document_count, 1);
        assert_eq!(status.coverage.len(), 1);
    }

    #[test]
    fn configured_status_surfaces_corrupt_state_as_degraded() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join(CHECKPOINT_FILE), b"not-json").expect("corrupt state");
        let plugin = TantivySessionSearchPlugin::default();
        let status = plugin.status(&config(root.path()));
        assert_eq!(status.state, SearchProviderState::Degraded);
        assert!(status.degraded_reason.is_some());
    }

    #[test]
    fn failed_engine_open_is_remembered_but_retried_after_cooldown() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        // Hold the index writer lock from a second engine so the plugin's open fails with
        // `LockBusy`, the same transient failure another daemon on a shared state root produces.
        let holder = SearchEngine::open(root.path().to_path_buf(), &config).expect("lock holder");
        let plugin = TantivySessionSearchPlugin::default();
        assert!(plugin.ready_engine(&config).is_err());
        assert_eq!(plugin.status(&config).state, SearchProviderState::Degraded);

        // Within the cooldown the remembered failure is returned without reopening.
        drop(holder);
        assert!(plugin.ready_engine(&config).is_err());

        // Once the cooldown elapses the next call reopens and recovers instead of staying degraded
        // until an explicit rebuild.
        {
            let mut guard = plugin.engine.write().expect("engine state");
            if let EngineState::Failed { failed_at, .. } = &mut *guard {
                *failed_at = std::time::Instant::now()
                    .checked_sub(ENGINE_OPEN_RETRY_COOLDOWN)
                    .expect("cooldown fits within process uptime");
            } else {
                panic!("engine must remember the failed open");
            }
        }
        assert!(plugin.ready_engine(&config).is_ok());
        assert_eq!(plugin.status(&config).state, SearchProviderState::Ready);
    }

    #[test]
    fn content_policy_change_requires_explicit_rebuild() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        {
            let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
            let state = engine.state.lock().expect("state");
            persist_state(root.path(), &state).expect("persist initial state");
        }

        let mut changed = config;
        changed
            .sensitive_content
            .insert(SearchContentKind::AssistantReasoning);
        let plugin = TantivySessionSearchPlugin::default();
        let status = plugin.status(&changed);
        assert_eq!(status.state, SearchProviderState::Degraded);
        assert!(
            status
                .degraded_reason
                .as_deref()
                .is_some_and(
                    |reason| reason.contains("content policy") && reason.contains("rebuild")
                )
        );
    }

    #[test]
    fn quota_exhaustion_rejects_before_index_commit() {
        let root = tempfile::tempdir().expect("root");
        let mut config = config(root.path());
        config.quota_bytes = 1;
        let plugin = TantivySessionSearchPlugin::default();
        let session_id = SessionId::new();
        let batch = apply_batch_request(session_id, "batch-quota", 1, "quota needle");
        let response = plugin.apply_batch(&config, &batch, &ServiceCancellation::default());
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("quota_exceeded")
        );
        assert!(!root.path().join(CHECKPOINT_FILE).exists());
    }

    #[test]
    fn quota_exhaustion_preserves_committed_data_and_incomplete_coverage() {
        let root = tempfile::tempdir().expect("root");
        let mut config = config(root.path());
        let empty_bytes = {
            let engine = SearchEngine::open(root.path().to_path_buf(), &config).expect("engine");
            let bytes = directory_size(&engine.root.join(INDEX_DIRECTORY));
            drop(engine);
            bytes
        };
        config.quota_bytes = empty_bytes.saturating_add(32 * 1024);
        let plugin = TantivySessionSearchPlugin::default();
        let session_id = SessionId::new();
        let mut first = apply_batch_request(session_id, "batch-old", 1, "retained needle");
        first.generation.last_sequence = Some(2);
        let first_response = plugin.apply_batch(&config, &first, &ServiceCancellation::default());
        assert!(first_response.error.is_none());

        let mut second = apply_batch_request(
            session_id,
            "batch-quota-later",
            2,
            &"x".repeat(bcode_session_search::DEFAULT_MAX_TEXT_BYTES_PER_RECORD),
        );
        second.expected_previous_sequence = Some(1);
        second.expected_previous_session_text_bytes =
            u64::try_from("retained needle".len()).unwrap_or(u64::MAX);
        let second_response = plugin.apply_batch(&config, &second, &ServiceCancellation::default());
        assert_eq!(
            second_response
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("quota_exceeded")
        );

        let search_response = plugin.search(
            &config,
            &request("retained"),
            &ServiceCancellation::default(),
        );
        let search: SessionSearchResponse =
            search_response.payload_json().expect("search response");
        assert_eq!(search.hits.len(), 1);
        assert_eq!(search.hits[0].locator.sequence, 1);
        assert!(!search.coverage_complete);
        let status = plugin.status(&config);
        assert_eq!(status.document_count, 1);
        assert_eq!(status.coverage[0].indexed_through_sequence, Some(1));
        assert!(!status.coverage[0].complete);
    }

    #[test]
    fn prior_document_identity_schema_requires_explicit_rebuild() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let mut state = PersistedProviderState::new(&config);
        state.index_schema_version = INDEX_SCHEMA_VERSION - 1;
        persist_state(root.path(), &state).expect("legacy provider state");
        let plugin = TantivySessionSearchPlugin::default();

        let status = plugin.status(&config);
        assert_eq!(status.state, SearchProviderState::Degraded);
        assert!(
            status
                .degraded_reason
                .as_deref()
                .is_some_and(|message| message.contains("unsupported schema"))
        );
    }

    #[test]
    fn repeated_canonical_record_ids_remain_isolated_by_session() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let plugin = TantivySessionSearchPlugin::default();
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let first = apply_batch_request(first_session, "first-batch", 1, "shared marker first");
        let second = apply_batch_request(second_session, "second-batch", 1, "shared marker second");
        assert!(
            plugin
                .apply_batch(&config, &first, &ServiceCancellation::default())
                .error
                .is_none()
        );
        assert!(
            plugin
                .apply_batch(&config, &second, &ServiceCancellation::default())
                .error
                .is_none()
        );

        let response: SessionSearchResponse = plugin
            .search(&config, &request("marker"), &ServiceCancellation::default())
            .payload_json()
            .expect("search response");
        assert_eq!(response.hits.len(), 2);
        assert_eq!(
            response
                .hits
                .iter()
                .map(|hit| hit.locator.session_id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([first_session, second_session])
        );
    }

    #[test]
    fn search_cursor_pages_without_duplicates_and_rejects_query_mismatch() {
        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let plugin = TantivySessionSearchPlugin::default();
        let session_id = SessionId::new();
        let engine = plugin.ready_engine(&config).expect("engine");
        index_records(
            &engine,
            &(1..=5)
                .map(|sequence| record(session_id, sequence, "pagination needle"))
                .collect::<Vec<_>>(),
        );
        drop(engine);

        let mut first_request = request("needle");
        first_request.limit = 2;
        let first: SessionSearchResponse = plugin
            .search(&config, &first_request, &ServiceCancellation::default())
            .payload_json()
            .expect("first page");
        assert_eq!(first.hits.len(), 2);
        let cursor = first.next_cursor.expect("next cursor");

        let mut second_request = first_request;
        second_request.cursor = Some(cursor.clone());
        let second: SessionSearchResponse = plugin
            .search(&config, &second_request, &ServiceCancellation::default())
            .payload_json()
            .expect("second page");
        assert_eq!(second.hits.len(), 2);
        assert!(second.next_cursor.is_some());
        assert!(first.hits.iter().all(|first_hit| {
            second
                .hits
                .iter()
                .all(|second_hit| second_hit.locator != first_hit.locator)
        }));

        let mut mismatched = request("different");
        mismatched.limit = 2;
        mismatched.cursor = Some(cursor);
        let error = plugin.search(&config, &mismatched, &ServiceCancellation::default());
        assert_eq!(
            error.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_request")
        );
    }

    #[test]
    fn pre_cancelled_ingestion_publishes_nothing() {
        use std::sync::atomic::AtomicBool;

        let root = tempfile::tempdir().expect("root");
        let config = config(root.path());
        let cancelled = Arc::new(AtomicBool::new(true));
        let cancellation = ServiceCancellation::new(cancelled);
        let plugin = TantivySessionSearchPlugin::default();
        let batch = apply_batch_request(SessionId::new(), "batch-cancel", 1, "cancel needle");
        let response = plugin.apply_batch(&config, &batch, &cancellation);
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("cancelled")
        );
        assert!(!root.path().join(CHECKPOINT_FILE).exists());
    }

    #[test]
    fn explicit_purge_removes_provider_state_without_touching_parent() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("tantivy-provider");
        std::fs::create_dir(&root).expect("root");
        std::fs::write(root.join("derived"), b"derived").expect("derived state");
        let sentinel = parent.path().join("canonical-sentinel");
        std::fs::write(&sentinel, b"canonical").expect("sentinel");
        let plugin = TantivySessionSearchPlugin::default();
        let request = PurgeSessionSearchRequest {
            provider_id: PLUGIN_ID.to_owned(),
            confirmation: PURGE_CONFIRMATION.to_owned(),
        };
        let response = plugin.purge(&config(&root), &request);
        assert!(response.error.is_none());
        assert!(!root.exists());
        assert!(sentinel.exists());
    }

    #[test]
    fn purge_requires_exact_confirmation() {
        let root = tempfile::tempdir().expect("root");
        let plugin = TantivySessionSearchPlugin::default();
        let request = PurgeSessionSearchRequest {
            provider_id: PLUGIN_ID.to_owned(),
            confirmation: "wrong".to_owned(),
        };
        let response = plugin.purge(&config(root.path()), &request);
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("confirmation_required")
        );
        assert!(root.path().exists());
    }

    #[test]
    #[cfg(unix)]
    fn path_confinement_rejects_symlink_roots() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("parent");
        let target = tempfile::tempdir().expect("target");
        let link = parent.path().join("provider-link");
        symlink(target.path(), &link).expect("symlink");
        assert!(confined_storage_root(&link).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn path_confinement_rejects_symlink_ancestors_and_traversal() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("parent");
        let target = tempfile::tempdir().expect("target");
        let link = parent.path().join("provider-parent-link");
        symlink(target.path(), &link).expect("symlink");
        assert!(confined_storage_root(&link.join("provider")).is_err());
        assert!(confined_storage_root(&parent.path().join("safe/../provider")).is_err());
    }

    #[test]
    fn path_confinement_rejects_files_and_ambiguous_roots() {
        let parent = tempfile::tempdir().expect("parent");
        let file = parent.path().join("provider-file");
        std::fs::write(&file, b"not a directory").expect("file");
        assert!(confined_storage_root(&file).is_err());
        assert!(confined_storage_root(Path::new("/")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn purge_rejects_symlink_ancestor_without_touching_target() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("parent");
        let target = tempfile::tempdir().expect("target");
        let sentinel = target.path().join("derived");
        std::fs::write(&sentinel, b"retained").expect("sentinel");
        let link = parent.path().join("provider-parent-link");
        symlink(target.path(), &link).expect("symlink");
        let plugin = TantivySessionSearchPlugin::default();
        let response = plugin.purge(
            &config(&link),
            &PurgeSessionSearchRequest {
                provider_id: PLUGIN_ID.to_owned(),
                confirmation: PURGE_CONFIRMATION.to_owned(),
            },
        );
        assert!(response.error.is_some());
        assert!(sentinel.exists());
        assert!(target.path().exists());
    }

    #[test]
    fn explicit_rebuild_replaces_corrupt_state_with_empty_compatible_index() {
        let root = tempfile::tempdir().expect("root");
        let provider_config = config(root.path());
        let session_id = SessionId::new();
        let batch = apply_batch_request(session_id, "batch-rebuild", 1, "rebuild needle");
        {
            let engine =
                SearchEngine::open(root.path().to_path_buf(), &provider_config).expect("engine");
            let marker = apply_to_engine(&engine, &batch);
            let mut state = engine.state.lock().expect("state");
            marker.apply_to(&mut state).expect("apply marker");
            persist_state(root.path(), &state).expect("persist");
        }
        std::fs::write(root.path().join(CHECKPOINT_FILE), b"not-json").expect("corrupt state");
        let sentinel = root
            .path()
            .parent()
            .expect("parent")
            .join("rebuild-sentinel");
        std::fs::write(&sentinel, b"canonical").expect("sentinel");

        let plugin = TantivySessionSearchPlugin::default();
        let response = plugin.rebuild(
            &provider_config,
            &RebuildSessionSearchRequest {
                provider_id: PLUGIN_ID.to_owned(),
                confirmation: REBUILD_CONFIRMATION.to_owned(),
            },
        );
        assert!(response.error.is_none());
        let result: RebuildSessionSearchResponse = response.payload_json().expect("response");
        assert_eq!(result.provider_id, PLUGIN_ID);
        assert_eq!(result.record_schema_version, CURRENT_SEARCH_RECORD_VERSION);
        assert!(sentinel.exists());

        let status = plugin.status(&provider_config);
        assert_eq!(status.state, SearchProviderState::Ready);
        assert_eq!(status.document_count, 0);
        assert!(status.coverage.is_empty());
        let search = plugin.search(
            &provider_config,
            &request("rebuild"),
            &ServiceCancellation::default(),
        );
        let search: SessionSearchResponse = search.payload_json().expect("search");
        assert!(search.hits.is_empty());
    }

    #[test]
    fn rebuild_requires_exact_confirmation_and_configured_storage() {
        let root = tempfile::tempdir().expect("root");
        let plugin = TantivySessionSearchPlugin::default();
        let response = plugin.rebuild(
            &config(root.path()),
            &RebuildSessionSearchRequest {
                provider_id: PLUGIN_ID.to_owned(),
                confirmation: "wrong".to_owned(),
            },
        );
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("confirmation_required")
        );
        assert!(root.path().exists());

        let response = plugin.rebuild(
            &ProviderConfig::default(),
            &RebuildSessionSearchRequest {
                provider_id: PLUGIN_ID.to_owned(),
                confirmation: REBUILD_CONFIRMATION.to_owned(),
            },
        );
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_configuration")
        );
    }

    #[test]
    #[ignore = "manual deterministic Tantivy provider performance baseline"]
    #[allow(clippy::too_many_lines)]
    fn benchmark_tantivy_provider_query_ingestion_open_and_amplification() {
        use std::time::Instant;

        const DEFAULT_RECORDS: usize = 25_000;
        const MAX_RECORDS: usize = 100_000;
        const BATCH_SIZE: usize = 250;
        const QUERY_RUNS: usize = 100;
        const QUERY_P95_BUDGET_US: u128 = 100_000;
        const MAX_AMPLIFICATION_PERMILLE: u64 = 500;

        let records = std::env::var("BCODE_SESSION_SEARCH_BENCH_RECORDS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_RECORDS);
        assert!(
            records > 0 && records <= MAX_RECORDS && records.is_multiple_of(BATCH_SIZE),
            "benchmark record count must be a non-zero multiple of {BATCH_SIZE} no larger than {MAX_RECORDS}"
        );
        let root = std::env::var_os("BCODE_SESSION_SEARCH_BENCH_OUTPUT")
            .map(PathBuf::from)
            .map_or_else(
                || tempfile::tempdir().expect("root").keep(),
                |root| {
                    let root = if root.is_absolute() {
                        root
                    } else {
                        std::env::current_dir()
                            .expect("benchmark current directory")
                            .join(root)
                    };
                    std::fs::create_dir_all(&root).expect("benchmark output root");
                    root
                },
            );
        let mut provider_config = config(&root);
        provider_config.quota_bytes = DEFAULT_QUOTA_BYTES;
        let plugin = TantivySessionSearchPlugin::default();
        let session_id = SessionId::new();
        let mut normalized_bytes = 0_u64;
        let ingestion_started = Instant::now();
        let mut commit_durations = Vec::with_capacity(records / BATCH_SIZE);
        let mut previous_sequence = None;
        let mut previous_session_text_bytes = 0_u64;

        for batch_index in 0..(records / BATCH_SIZE) {
            let first = batch_index * BATCH_SIZE + 1;
            let records = (first..first + BATCH_SIZE)
                .map(|sequence| {
                    let text = format!(
                        "benchmark transcript event {sequence} searchable-token-{} {}",
                        sequence % 97,
                        "representative assistant response content with repeated natural-language context and bounded metadata ".repeat(10)
                    );
                    normalized_bytes = normalized_bytes
                        .saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
                    record(
                        session_id,
                        u64::try_from(sequence).unwrap_or(u64::MAX),
                        &text,
                    )
                })
                .collect::<Vec<_>>();
            let last_sequence = u64::try_from(first + BATCH_SIZE - 1).unwrap_or(u64::MAX);
            let request = ApplySearchRecordsRequest {
                provider_id: PLUGIN_ID.to_owned(),
                batch_id: format!("benchmark-{batch_index}"),
                generation: SearchCanonicalGeneration {
                    session_id,
                    fingerprint: "benchmark-generation".to_owned(),
                    last_sequence: Some(last_sequence),
                },
                expected_previous_sequence: previous_sequence,
                expected_previous_session_text_bytes: previous_session_text_bytes,
                indexed_through_sequence: None,
                records,
            };
            let started = Instant::now();
            let response =
                plugin.apply_batch(&provider_config, &request, &ServiceCancellation::default());
            commit_durations.push(started.elapsed().as_micros());
            assert!(response.error.is_none(), "benchmark batch failed");
            previous_sequence = Some(last_sequence);
            previous_session_text_bytes = normalized_bytes;
        }
        let ingestion_us = ingestion_started.elapsed().as_micros();
        let index_bytes = directory_size(&root.join(INDEX_DIRECTORY));
        let amplification_permille = index_bytes
            .saturating_mul(1_000)
            .checked_div(normalized_bytes)
            .unwrap_or(u64::MAX);

        let mut query_durations = Vec::with_capacity(QUERY_RUNS);
        for _ in 0..QUERY_RUNS {
            let started = Instant::now();
            let response = plugin.search(
                &provider_config,
                &request("searchable-token-42"),
                &ServiceCancellation::default(),
            );
            query_durations.push(started.elapsed().as_micros());
            assert!(response.error.is_none(), "benchmark query failed");
        }
        query_durations.sort_unstable();
        commit_durations.sort_unstable();
        let query_p50_us = query_durations[QUERY_RUNS / 2];
        let query_p95_us = query_durations[QUERY_RUNS * 95 / 100];
        let query_p99_us = query_durations[QUERY_RUNS * 99 / 100];
        let commit_p50_us = commit_durations[commit_durations.len() / 2];
        let commit_p95_us = commit_durations[commit_durations.len() * 95 / 100];
        let commit_p99_us = commit_durations[commit_durations.len() * 99 / 100];

        drop(plugin);
        let open_started = Instant::now();
        let reopened = SearchEngine::open(root, &provider_config).expect("reopen benchmark index");
        let open_us = open_started.elapsed().as_micros();
        let writer_memory_bytes = provider_config.writer_memory_bytes;
        drop(reopened);

        eprintln!(
            "tantivy_session_search_benchmark records={records} batches={} normalized_bytes={normalized_bytes} index_bytes={index_bytes} amplification_permille={amplification_permille} ingestion_us={ingestion_us} records_per_second={} commit_p50_us={commit_p50_us} commit_p95_us={commit_p95_us} commit_p99_us={commit_p99_us} query_runs={QUERY_RUNS} query_p50_us={query_p50_us} query_p95_us={query_p95_us} query_p99_us={query_p99_us} open_us={open_us} configured_writer_memory_bytes={writer_memory_bytes} hydration=not_measured_provider_has_no_canonical_access",
            records / BATCH_SIZE,
            u128::try_from(records)
                .unwrap_or(u128::MAX)
                .saturating_mul(1_000_000)
                .checked_div(ingestion_us)
                .unwrap_or(u128::MAX),
        );
        assert!(
            query_p95_us <= QUERY_P95_BUDGET_US,
            "ordinary query p95 {query_p95_us} us exceeds 100 ms budget"
        );
        assert!(
            amplification_permille <= MAX_AMPLIFICATION_PERMILLE,
            "index amplification {amplification_permille} permille exceeds 50% budget"
        );
    }

    #[test]
    fn interrupted_rebuild_marker_surfaces_rebuilding_and_blocks_use() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(
            root.path().join(REBUILD_MARKER_FILE),
            b"rebuild in progress\n",
        )
        .expect("marker");
        let plugin = TantivySessionSearchPlugin::default();
        let config = config(root.path());

        let status = plugin.status(&config);
        assert_eq!(status.state, SearchProviderState::Rebuilding);
        assert!(plugin.ready_engine(&config).is_err());
    }

    #[test]
    fn transcript_provider_rejects_unmeasured_large_output_categories() {
        for content_kind in [
            SearchContentKind::ShellOutput,
            SearchContentKind::ToolOutput,
        ] {
            let mut config = ProviderConfig::default();
            config.sensitive_content.insert(content_kind);
            let error = config
                .validate()
                .expect_err("large output must fail closed");
            assert_eq!(error.code, "invalid_configuration");
            assert!(error.message.contains("measured deep-search provider"));
        }
    }

    #[test]
    fn sensitive_content_is_disabled_by_default_and_relative_storage_is_rejected() {
        let config = ProviderConfig::default();
        let content = config.allowed_content();
        assert!(!content.contains(&SearchContentKind::AssistantReasoning));
        assert!(!content.contains(&SearchContentKind::ShellOutput));
        assert!(!content.contains(&SearchContentKind::ToolArguments));
        assert!(!content.contains(&SearchContentKind::ToolOutput));
        assert!(confined_storage_root(Path::new("relative")).is_err());
    }

    #[test]
    fn enablement_and_purge_are_independent_lifecycle_controls() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("provider-state");
        std::fs::create_dir(&root).expect("provider root");
        std::fs::write(root.join("retained-marker"), b"derived").expect("retained state");

        let plugin = TantivySessionSearchPlugin::default();
        assert_eq!(
            plugin.status(&ProviderConfig::default()).state,
            SearchProviderState::Disabled
        );
        assert!(root.join("retained-marker").exists());

        let enabled = config(&root);
        assert_eq!(plugin.status(&enabled).state, SearchProviderState::Ready);
        assert!(root.join("retained-marker").exists());

        assert_eq!(
            plugin.status(&ProviderConfig::default()).state,
            SearchProviderState::Disabled
        );
        assert!(root.join("retained-marker").exists());

        let response = plugin.purge(
            &enabled,
            &PurgeSessionSearchRequest {
                provider_id: PLUGIN_ID.to_owned(),
                confirmation: PURGE_CONFIRMATION.to_owned(),
            },
        );
        assert!(response.error.is_none());
        assert!(!root.exists());
        assert!(parent.path().exists());
    }
}
