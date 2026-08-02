#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Provider-owned compressed deep search for normalized shell and tool output.
//!
//! The provider accepts only portable projected records. It never opens canonical session storage.

use bcode_plugin_sdk::{ServiceCancellation, prelude::*};
use bcode_session_models::SessionId;
use bcode_session_search::{
    ApplyBatchOutcome, ApplySearchRecordsRequest, ApplySearchRecordsResponse,
    BatchDeliveryClassification, CURRENT_NORMALIZATION_VERSION, CURRENT_SEARCH_POLICY_VERSION,
    CURRENT_SEARCH_RECORD_VERSION, OP_APPLY_BATCH, OP_CAPABILITIES, OP_PURGE, OP_REBUILD,
    OP_REMOVE_SESSION, OP_SEARCH, OP_STATUS, ProviderSearchOutcome, PurgeSessionSearchRequest,
    RebuildSessionSearchRequest, RebuildSessionSearchResponse, RemoveSessionSearchRequest,
    SESSION_SEARCH_INTERFACE_ID, SearchContentKind, SearchExecutionKind, SearchFeature,
    SearchField, SearchProviderState, SessionSearchCapabilities, SessionSearchCoverage,
    SessionSearchHit, SessionSearchQuery, SessionSearchRecord, SessionSearchRequest,
    SessionSearchResponse, SessionSearchSort, SessionSearchStatus, TextMatchMode,
    classify_batch_delivery,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const PLUGIN_ID: &str = "bcode.compressed-session-search";
const FORMAT_VERSION: u16 = 1;
const DEFAULT_QUOTA_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_SESSION_QUOTA_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 256 * 1024;
const MAX_CHUNK_RECORDS: usize = 64;
const MAX_SCAN_CHUNKS: usize = 128;
const MAX_HITS: usize = 200;
const MAX_PREVIEW_BYTES: usize = 4 * 1024;
const MAX_DECOMPRESSION_RATIO: usize = 1_024;
const MAX_CONCURRENT_SCANS: usize = 2;
const PURGE_CONFIRMATION: &str = "purge-bcode.compressed-session-search";
const REBUILD_CONFIRMATION: &str = "rebuild-bcode.compressed-session-search";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderConfig {
    storage_root: Option<PathBuf>,
    quota_bytes: u64,
    session_quota_bytes: u64,
    cache_bytes: usize,
    compression_level: i32,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            storage_root: None,
            quota_bytes: DEFAULT_QUOTA_BYTES,
            session_quota_bytes: DEFAULT_SESSION_QUOTA_BYTES,
            cache_bytes: DEFAULT_CACHE_BYTES,
            compression_level: 3,
        }
    }
}

impl ProviderConfig {
    fn validate(&self) -> Result<(), ProviderError> {
        if self.quota_bytes == 0 || self.session_quota_bytes == 0 {
            return Err(ProviderError::new(
                "invalid_configuration",
                "quotas must be non-zero",
            ));
        }
        if self.cache_bytes < MAX_CHUNK_BYTES || self.cache_bytes > DEFAULT_CACHE_BYTES {
            return Err(ProviderError::new(
                "invalid_configuration",
                "cache_bytes must be between 256 KiB and 64 MiB",
            ));
        }
        if !(-7..=22).contains(&self.compression_level) {
            return Err(ProviderError::new(
                "invalid_configuration",
                "compression_level must be between -7 and 22",
            ));
        }
        if let Some(root) = &self.storage_root {
            validate_root(root)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    format_version: u16,
    record_schema_version: u16,
    normalization_version: u16,
    policy_version: u16,
    quota_bytes: u64,
    session_quota_bytes: u64,
    next_chunk_id: u64,
    chunks: Vec<ChunkMetadata>,
    sessions: BTreeMap<SessionId, SessionState>,
    batch_digests: BTreeMap<String, String>,
}

impl Manifest {
    const fn empty(config: &ProviderConfig) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
            quota_bytes: config.quota_bytes,
            session_quota_bytes: config.session_quota_bytes,
            next_chunk_id: 0,
            chunks: Vec::new(),
            sessions: BTreeMap::new(),
            batch_digests: BTreeMap::new(),
        }
    }

    fn validate(&self, config: &ProviderConfig) -> Result<(), ProviderError> {
        if self.format_version != FORMAT_VERSION
            || self.record_schema_version != CURRENT_SEARCH_RECORD_VERSION
            || self.normalization_version != CURRENT_NORMALIZATION_VERSION
            || self.policy_version != CURRENT_SEARCH_POLICY_VERSION
        {
            return Err(ProviderError::new(
                "incompatible_index",
                "unsupported retained format",
            ));
        }
        if self.quota_bytes != config.quota_bytes
            || self.session_quota_bytes != config.session_quota_bytes
        {
            return Err(ProviderError::new(
                "incompatible_index",
                "configured quotas differ from retained state; rebuild is required",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionState {
    generation_fingerprint: String,
    canonical_tail_sequence: Option<u64>,
    indexed_through_sequence: u64,
    indexed_text_bytes: u64,
    record_count: u64,
    truncated_records: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkMetadata {
    id: u64,
    file_name: String,
    session_id: SessionId,
    generation_fingerprint: String,
    first_sequence: u64,
    last_sequence: u64,
    record_count: u32,
    normalized_bytes: u64,
    compressed_bytes: u64,
    normalized_sha256: String,
    compressed_sha256: String,
    content_kinds: BTreeSet<SearchContentKind>,
}

struct ProviderState {
    root: PathBuf,
    manifest: Manifest,
    cache: Arc<Mutex<ChunkCache>>,
}

#[derive(Clone)]
struct ScanSnapshot {
    root: PathBuf,
    chunks: Vec<ChunkMetadata>,
    cache: Arc<Mutex<ChunkCache>>,
}

impl ProviderState {
    fn scan_snapshot(&self) -> ScanSnapshot {
        ScanSnapshot {
            root: self.root.clone(),
            chunks: self.manifest.chunks.clone(),
            cache: Arc::clone(&self.cache),
        }
    }
}

#[derive(Default)]
struct ChunkCache {
    entries: BTreeMap<u64, Arc<Vec<SessionSearchRecord>>>,
    order: Vec<u64>,
    bytes: usize,
    limit: usize,
}

impl ChunkCache {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    fn get(&self, id: u64) -> Option<Arc<Vec<SessionSearchRecord>>> {
        self.entries.get(&id).cloned()
    }

    fn insert(&mut self, id: u64, records: Arc<Vec<SessionSearchRecord>>, bytes: usize) {
        while self.bytes.saturating_add(bytes) > self.limit && !self.order.is_empty() {
            let oldest = self.order.remove(0);
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self
                    .bytes
                    .saturating_sub(serialized_records_bytes(&removed));
            }
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.order.push(id);
        self.entries.insert(id, records);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

#[derive(Default)]
struct ScanLimiter {
    active: Mutex<usize>,
    wakeup: std::sync::Condvar,
}

struct ScanPermit<'a> {
    limiter: &'a ScanLimiter,
}

impl ScanLimiter {
    fn acquire(
        &self,
        cancellation: &ServiceCancellation,
        deadline: Option<Duration>,
    ) -> Result<ScanPermit<'_>, ProviderSearchOutcome> {
        let started = Instant::now();
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if cancellation.is_cancelled() {
                return Err(ProviderSearchOutcome::Cancelled);
            }
            if deadline.is_some_and(|limit| started.elapsed() >= limit) {
                return Err(ProviderSearchOutcome::TimedOut);
            }
            if *active < MAX_CONCURRENT_SCANS {
                *active += 1;
                return Ok(ScanPermit { limiter: self });
            }
            let remaining = deadline
                .map_or(Duration::from_millis(25), |limit| {
                    limit.saturating_sub(started.elapsed())
                })
                .min(Duration::from_millis(25));
            let (next, _) = self
                .wakeup
                .wait_timeout(active, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active = next;
        }
    }
}

impl Drop for ScanPermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .limiter
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active = active.saturating_sub(1);
        drop(active);
        self.limiter.wakeup.notify_one();
    }
}

#[derive(Default)]
pub struct CompressedSessionSearchPlugin {
    lifecycle: RwLock<()>,
    state: Mutex<Option<ProviderState>>,
    scans: ScanLimiter,
}

impl RustPlugin for CompressedSessionSearchPlugin {}

impl ConcurrentRustPlugin for CompressedSessionSearchPlugin {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != SESSION_SEARCH_INTERFACE_ID {
            return ServiceResponse::error("unsupported_interface", "unsupported interface");
        }
        let config = match context.config_or_default::<ProviderConfig>() {
            Ok(config) => config,
            Err(error) => {
                return ServiceResponse::error("invalid_configuration", error.to_string());
            }
        };
        if let Err(error) = config.validate() {
            return error_response(&error);
        }
        match context.request.operation.as_str() {
            OP_CAPABILITIES => json_response(&capabilities()),
            OP_STATUS => json_response(&self.status(&config)),
            OP_SEARCH => decode_request(&context, |request| {
                self.search(&config, &request, &context.cancellation)
            }),
            OP_APPLY_BATCH => decode_request(&context, |request| {
                self.apply_batch(&config, &request, &context.cancellation)
            }),
            OP_REMOVE_SESSION => {
                decode_request(&context, |request| self.remove_session(&config, &request))
            }
            OP_PURGE => decode_request(&context, |request| self.purge(&config, &request)),
            OP_REBUILD => decode_request(&context, |request| self.rebuild(&config, &request)),
            _ => ServiceResponse::error("unsupported_operation", "unsupported operation"),
        }
    }
}

impl CompressedSessionSearchPlugin {
    /// The provider state mutex intentionally remains held while `operation` runs so manifest,
    /// chunks, checkpoints, and cache mutate as one in-process critical section.
    #[allow(clippy::significant_drop_tightening)]
    fn with_state<T>(
        &self,
        config: &ProviderConfig,
        operation: impl FnOnce(&mut ProviderState) -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let root = config.storage_root.as_ref().ok_or_else(|| {
            ProviderError::new("provider_disabled", "storage_root is not configured")
        })?;
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.as_ref().is_none_or(|state| state.root != *root) {
            *guard = Some(open_state(root, config)?);
        }
        let state = guard.as_mut().expect("state initialized");
        state.manifest.validate(config)?;
        operation(state)
    }

    fn status(&self, config: &ProviderConfig) -> SessionSearchStatus {
        let mut status = SessionSearchStatus {
            provider_id: PLUGIN_ID.to_owned(),
            state: SearchProviderState::Disabled,
            record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
            index_bytes: 0,
            quota_bytes: config.quota_bytes,
            document_count: 0,
            pending_sessions: 0,
            coverage: Vec::new(),
            degraded_reason: None,
        };
        if config.storage_root.is_none() {
            return status;
        }
        match self.with_state(config, |state| {
            let index_bytes = state
                .manifest
                .chunks
                .iter()
                .map(|chunk| chunk.compressed_bytes)
                .sum();
            let coverage = state
                .manifest
                .sessions
                .iter()
                .map(|(session_id, session)| SessionSearchCoverage {
                    generation: bcode_session_search::SearchCanonicalGeneration {
                        session_id: *session_id,
                        fingerprint: session.generation_fingerprint.clone(),
                        last_sequence: session.canonical_tail_sequence,
                    },
                    content_kinds: large_content_kinds(),
                    indexed_through_sequence: Some(session.indexed_through_sequence),
                    complete: session.canonical_tail_sequence
                        == Some(session.indexed_through_sequence),
                    indexed_text_bytes: session.indexed_text_bytes,
                    skipped_records: 0,
                    truncated_records: session.truncated_records,
                    exclusions: Vec::new(),
                })
                .collect::<Vec<_>>();
            Ok((
                index_bytes,
                coverage,
                state
                    .manifest
                    .chunks
                    .iter()
                    .map(|c| u64::from(c.record_count))
                    .sum(),
            ))
        }) {
            Ok((index_bytes, coverage, documents)) => {
                status.state = SearchProviderState::Ready;
                status.index_bytes = index_bytes;
                status.coverage = coverage;
                status.document_count = documents;
            }
            Err(error) => {
                status.state = if error.code == "corrupt_index" {
                    SearchProviderState::Corrupt
                } else {
                    SearchProviderState::Degraded
                };
                status.degraded_reason = Some(bounded_message(&error.message));
            }
        }
        status
    }

    fn apply_batch(
        &self,
        config: &ProviderConfig,
        request: &ApplySearchRecordsRequest,
        cancellation: &ServiceCancellation,
    ) -> ServiceResponse {
        if request.provider_id != PLUGIN_ID {
            return ServiceResponse::error("invalid_request", "provider identity mismatch");
        }
        if let Err(error) = request.validate() {
            return ServiceResponse::error("invalid_request", error.to_string());
        }
        if request.records.iter().any(|record| {
            !matches!(
                record.content_kind,
                SearchContentKind::ShellOutput | SearchContentKind::ToolOutput
            )
        }) {
            return ServiceResponse::error(
                "unsupported_content",
                "provider accepts only shell_output and tool_output",
            );
        }
        let _lifecycle = self
            .lifecycle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.with_state(config, |state| {
            apply_batch(state, config, request, cancellation)
        }) {
            Ok(response) => json_response(&response),
            Err(error) => error_response(&error),
        }
    }

    fn search(
        &self,
        config: &ProviderConfig,
        request: &SessionSearchRequest,
        cancellation: &ServiceCancellation,
    ) -> ServiceResponse {
        if let Err(error) = request.validate() {
            return ServiceResponse::error("invalid_request", error.to_string());
        }
        if request.cursor.is_some() {
            return ServiceResponse::error(
                "unsupported_cursor",
                "scan cursors are not implemented",
            );
        }
        if !matches!(
            request.sort,
            SessionSearchSort::ProviderRelevance | SessionSearchSort::SessionThenSequence
        ) {
            return ServiceResponse::error("unsupported_query", "requested sort is not supported");
        }
        if !request.filters.roles.is_empty() || !request.filters.inspection_categories.is_empty() {
            return ServiceResponse::error(
                "unsupported_query",
                "compressed provider does not support role or inspection-category filters",
            );
        }
        let matcher = match CompiledQuery::compile(&request.query) {
            Ok(matcher) => matcher,
            Err(error) => return error_response(&error),
        };
        let _permit = match self
            .scans
            .acquire(cancellation, request.deadline_ms.map(Duration::from_millis))
        {
            Ok(permit) => permit,
            Err(outcome) => {
                let message = match outcome {
                    ProviderSearchOutcome::Cancelled => "scan cancelled while waiting for capacity",
                    ProviderSearchOutcome::TimedOut => {
                        "scan deadline reached while waiting for capacity"
                    }
                    _ => "scan capacity unavailable",
                };
                return json_response(&search_response(
                    Vec::new(),
                    outcome,
                    false,
                    false,
                    Some(message),
                ));
            }
        };
        let lifecycle = self
            .lifecycle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = match self.with_state(config, |state| Ok(state.scan_snapshot())) {
            Ok(snapshot) => snapshot,
            Err(error) => return error_response(&error),
        };
        drop(lifecycle);
        match scan(&snapshot, request, &matcher, cancellation) {
            Ok(response) => json_response(&response),
            Err(error) => error_response(&error),
        }
    }

    fn remove_session(
        &self,
        config: &ProviderConfig,
        request: &RemoveSessionSearchRequest,
    ) -> ServiceResponse {
        let _lifecycle = self
            .lifecycle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.with_state(config, |state| {
            if let Some(expected) = &request.expected_generation_fingerprint
                && state
                    .manifest
                    .sessions
                    .get(&request.session_id)
                    .is_some_and(|session| &session.generation_fingerprint != expected)
            {
                return Err(ProviderError::new(
                    "stale_generation",
                    "session generation mismatch",
                ));
            }
            let removed = state
                .manifest
                .chunks
                .iter()
                .filter(|chunk| chunk.session_id == request.session_id)
                .cloned()
                .collect::<Vec<_>>();
            state
                .manifest
                .chunks
                .retain(|chunk| chunk.session_id != request.session_id);
            state.manifest.sessions.remove(&request.session_id);
            publish_manifest(&state.root, &state.manifest)?;
            for chunk in removed {
                let _ = fs::remove_file(state.root.join("chunks").join(chunk.file_name));
            }
            state
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            Ok(())
        }) {
            Ok(()) => json_response(&serde_json::json!({"removed": true})),
            Err(error) => error_response(&error),
        }
    }

    fn purge(
        &self,
        config: &ProviderConfig,
        request: &PurgeSessionSearchRequest,
    ) -> ServiceResponse {
        if request.provider_id != PLUGIN_ID || request.confirmation != PURGE_CONFIRMATION {
            return ServiceResponse::error("confirmation_required", PURGE_CONFIRMATION);
        }
        let _lifecycle = self
            .lifecycle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(root) = &config.storage_root else {
            return ServiceResponse::error("provider_disabled", "storage_root is not configured");
        };
        if let Err(error) = validate_root(root) {
            return error_response(&error);
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
        drop(guard);
        match fs::remove_dir_all(root) {
            Ok(()) => json_response(&serde_json::json!({"purged": true})),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                json_response(&serde_json::json!({"purged": true}))
            }
            Err(error) => error_response(&ProviderError::io(&error)),
        }
    }

    fn rebuild(
        &self,
        config: &ProviderConfig,
        request: &RebuildSessionSearchRequest,
    ) -> ServiceResponse {
        if request.provider_id != PLUGIN_ID || request.confirmation != REBUILD_CONFIRMATION {
            return ServiceResponse::error("confirmation_required", REBUILD_CONFIRMATION);
        }
        let _lifecycle = self
            .lifecycle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(root) = &config.storage_root else {
            return ServiceResponse::error("provider_disabled", "storage_root is not configured");
        };
        if let Err(error) = validate_root(root) {
            return error_response(&error);
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
        drop(guard);
        if let Err(error) = fs::remove_dir_all(root)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return error_response(&ProviderError::io(&error));
        }
        match open_state(root, config) {
            Ok(state) => {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *guard = Some(state);
                drop(guard);
                json_response(&RebuildSessionSearchResponse {
                    provider_id: PLUGIN_ID.to_owned(),
                    record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
                    normalization_version: CURRENT_NORMALIZATION_VERSION,
                    policy_version: CURRENT_SEARCH_POLICY_VERSION,
                })
            }
            Err(error) => error_response(&error),
        }
    }
}

fn capabilities() -> SessionSearchCapabilities {
    SessionSearchCapabilities {
        provider_id: PLUGIN_ID.to_owned(),
        execution: SearchExecutionKind::Scan,
        content_kinds: large_content_kinds(),
        features: BTreeSet::from([
            SearchFeature::Terms,
            SearchFeature::Phrase,
            SearchFeature::Regex,
            SearchFeature::StructuredFilters,
            SearchFeature::IncrementalIngestion,
            SearchFeature::HistoricalBackfill,
            SearchFeature::RemoveSession,
            SearchFeature::Rebuild,
            SearchFeature::Purge,
        ]),
        max_hits: MAX_HITS,
        max_batch_records: bcode_session_search::MAX_INGEST_RECORDS,
        max_batch_text_bytes: bcode_session_search::MAX_INGEST_TEXT_BYTES,
    }
}

fn large_content_kinds() -> BTreeSet<SearchContentKind> {
    BTreeSet::from([
        SearchContentKind::ShellOutput,
        SearchContentKind::ToolOutput,
    ])
}

fn open_state(root: &Path, config: &ProviderConfig) -> Result<ProviderState, ProviderError> {
    validate_root(root)?;
    fs::create_dir_all(root.join("chunks")).map_err(|error| ProviderError::io(&error))?;
    let manifest_path = root.join("manifest.json");
    let manifest = if manifest_path.exists() {
        let bytes = fs::read(&manifest_path).map_err(|error| ProviderError::io(&error))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::new("corrupt_index", error.to_string()))?;
        manifest.validate(config)?;
        manifest
    } else {
        let manifest = Manifest::empty(config);
        publish_manifest(root, &manifest)?;
        manifest
    };
    Ok(ProviderState {
        root: root.to_path_buf(),
        manifest,
        cache: Arc::new(Mutex::new(ChunkCache::new(config.cache_bytes))),
    })
}

#[allow(clippy::too_many_lines)]
fn apply_batch(
    state: &mut ProviderState,
    config: &ProviderConfig,
    request: &ApplySearchRecordsRequest,
    cancellation: &ServiceCancellation,
) -> Result<ApplySearchRecordsResponse, ProviderError> {
    let operation_digest = request.operation_digest_sha256();
    match classify_batch_delivery(
        request,
        state
            .manifest
            .batch_digests
            .get(&request.batch_id)
            .map(String::as_str),
    ) {
        BatchDeliveryClassification::Duplicate { .. } => {
            let session = state
                .manifest
                .sessions
                .get(&request.generation.session_id)
                .ok_or_else(|| {
                    ProviderError::new("corrupt_index", "duplicate batch has no session state")
                })?;
            return Ok(ApplySearchRecordsResponse {
                batch_id: request.batch_id.clone(),
                outcome: ApplyBatchOutcome::Duplicate,
                applied_records: 0,
                indexed_through_sequence: session.indexed_through_sequence,
            });
        }
        BatchDeliveryClassification::ConflictingDuplicate { .. } => {
            return Ok(ApplySearchRecordsResponse {
                batch_id: request.batch_id.clone(),
                outcome: ApplyBatchOutcome::ConflictingDuplicate,
                applied_records: 0,
                indexed_through_sequence: state
                    .manifest
                    .sessions
                    .get(&request.generation.session_id)
                    .map_or(0, |session| session.indexed_through_sequence),
            });
        }
        BatchDeliveryClassification::New { .. } => {}
    }
    if cancellation.is_cancelled() {
        return Err(ProviderError::new("cancelled", "ingestion cancelled"));
    }
    let prior = state
        .manifest
        .sessions
        .get(&request.generation.session_id)
        .cloned();
    if let Some(prior) = &prior {
        if prior.generation_fingerprint != request.generation.fingerprint {
            return Err(ProviderError::new(
                "stale_generation",
                "canonical generation changed; rebuild session state",
            ));
        }
        if request.expected_previous_sequence != Some(prior.indexed_through_sequence)
            || request.expected_previous_session_text_bytes != prior.indexed_text_bytes
        {
            return Err(ProviderError::new(
                "stale_checkpoint",
                "provider checkpoint differs from request",
            ));
        }
    } else if request.expected_previous_sequence.is_some()
        || request.expected_previous_session_text_bytes != 0
    {
        return Err(ProviderError::new(
            "stale_checkpoint",
            "request expects missing provider state",
        ));
    }
    let added_text = request
        .records
        .iter()
        .map(|record| record.text.as_ref().map_or(0_u64, |text| text.len() as u64))
        .sum::<u64>();
    let prior_text = prior
        .as_ref()
        .map_or(0, |session| session.indexed_text_bytes);
    if prior_text.saturating_add(added_text) > config.session_quota_bytes {
        return Err(ProviderError::new(
            "quota_exceeded",
            "session normalized-text quota exceeded",
        ));
    }
    let original_manifest = state.manifest.clone();
    let mut chunks = Vec::new();
    let chunk_groups = partition_records(&request.records)?;
    for records in chunk_groups {
        if cancellation.is_cancelled() {
            state.manifest = original_manifest;
            cleanup_chunks(&state.root, &chunks);
            return Err(ProviderError::new("cancelled", "ingestion cancelled"));
        }
        match write_chunk(state, request, &records, config.compression_level) {
            Ok(chunk) => chunks.push(chunk),
            Err(error) => {
                state.manifest = original_manifest;
                cleanup_chunks(&state.root, &chunks);
                return Err(error);
            }
        }
    }
    let retained_bytes = state
        .manifest
        .chunks
        .iter()
        .map(|chunk| chunk.compressed_bytes)
        .sum::<u64>();
    let added_bytes = chunks
        .iter()
        .map(|chunk| chunk.compressed_bytes)
        .sum::<u64>();
    if retained_bytes.saturating_add(added_bytes) > config.quota_bytes {
        state.manifest = original_manifest;
        cleanup_chunks(&state.root, &chunks);
        return Err(ProviderError::new(
            "quota_exceeded",
            "provider compressed quota exceeded",
        ));
    }
    let indexed_through = request
        .indexed_through_sequence
        .or_else(|| request.records.last().map(|record| record.locator.sequence))
        .ok_or_else(|| ProviderError::new("invalid_request", "batch has no checkpoint"))?;
    let session = SessionState {
        generation_fingerprint: request.generation.fingerprint.clone(),
        canonical_tail_sequence: request.generation.last_sequence,
        indexed_through_sequence: indexed_through,
        indexed_text_bytes: prior_text.saturating_add(added_text),
        record_count: prior
            .as_ref()
            .map_or(0, |session| session.record_count)
            .saturating_add(request.records.len() as u64),
        truncated_records: prior
            .as_ref()
            .map_or(0, |session| session.truncated_records)
            .saturating_add(
                request
                    .records
                    .iter()
                    .filter(|record| record.truncated)
                    .count() as u64,
            ),
    };
    state.manifest.chunks.extend(chunks.clone());
    state
        .manifest
        .sessions
        .insert(request.generation.session_id, session);
    state
        .manifest
        .batch_digests
        .insert(request.batch_id.clone(), operation_digest);
    if let Err(error) = publish_manifest(&state.root, &state.manifest) {
        state.manifest = original_manifest;
        cleanup_chunks(&state.root, &chunks);
        return Err(error);
    }
    Ok(ApplySearchRecordsResponse {
        batch_id: request.batch_id.clone(),
        outcome: ApplyBatchOutcome::Applied,
        applied_records: request.records.len(),
        indexed_through_sequence: indexed_through,
    })
}

fn partition_records(
    records: &[SessionSearchRecord],
) -> Result<Vec<Vec<SessionSearchRecord>>, ProviderError> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    for record in records {
        let single_size = serde_json::to_vec(std::slice::from_ref(record))
            .map_err(|error| ProviderError::new("encode_failed", error.to_string()))?
            .len();
        if single_size > MAX_CHUNK_BYTES {
            return Err(ProviderError::new(
                "record_too_large",
                "serialized record exceeds 256 KiB",
            ));
        }
        current.push(record.clone());
        let current_size = serde_json::to_vec(&current)
            .map_err(|error| ProviderError::new("encode_failed", error.to_string()))?
            .len();
        if current.len() > MAX_CHUNK_RECORDS || current_size > MAX_CHUNK_BYTES {
            let overflow = current.pop().expect("current contains inserted record");
            chunks.push(std::mem::take(&mut current));
            current.push(overflow);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

fn write_chunk(
    state: &mut ProviderState,
    request: &ApplySearchRecordsRequest,
    records: &[SessionSearchRecord],
    compression_level: i32,
) -> Result<ChunkMetadata, ProviderError> {
    let normalized = serde_json::to_vec(records)
        .map_err(|error| ProviderError::new("encode_failed", error.to_string()))?;
    if normalized.len() > MAX_CHUNK_BYTES {
        return Err(ProviderError::new(
            "record_too_large",
            "serialized chunk exceeds 256 KiB",
        ));
    }
    let compressed = zstd::stream::encode_all(normalized.as_slice(), compression_level)
        .map_err(|error| ProviderError::io(&error))?;
    let id = state.manifest.next_chunk_id;
    state.manifest.next_chunk_id = state.manifest.next_chunk_id.saturating_add(1);
    let file_name = format!("chunk-{id:020}.zst");
    atomic_write(&state.root.join("chunks").join(&file_name), &compressed)?;
    Ok(ChunkMetadata {
        id,
        file_name,
        session_id: request.generation.session_id,
        generation_fingerprint: request.generation.fingerprint.clone(),
        first_sequence: records.first().map_or(0, |record| record.locator.sequence),
        last_sequence: records.last().map_or(0, |record| record.locator.sequence),
        record_count: u32::try_from(records.len())
            .map_err(|_| ProviderError::new("encode_failed", "record count overflow"))?,
        normalized_bytes: normalized.len() as u64,
        compressed_bytes: compressed.len() as u64,
        normalized_sha256: digest(&normalized),
        compressed_sha256: digest(&compressed),
        content_kinds: records.iter().map(|record| record.content_kind).collect(),
    })
}

fn scan(
    state: &ScanSnapshot,
    request: &SessionSearchRequest,
    matcher: &CompiledQuery,
    cancellation: &ServiceCancellation,
) -> Result<SessionSearchResponse, ProviderError> {
    let started = Instant::now();
    let deadline = request.deadline_ms.map(Duration::from_millis);
    let mut hits = Vec::new();
    let mut scanned = 0;
    let mut incomplete = false;
    if deadline.is_some_and(|duration| duration.is_zero()) {
        return Ok(search_response(
            hits,
            ProviderSearchOutcome::TimedOut,
            false,
            false,
            Some("scan deadline reached"),
        ));
    }
    let chunks = state.chunks.clone();
    for chunk in chunks {
        if scanned >= MAX_SCAN_CHUNKS {
            incomplete = true;
            break;
        }
        if cancellation.is_cancelled() {
            return Ok(search_response(
                hits,
                ProviderSearchOutcome::Cancelled,
                false,
                false,
                Some("scan cancelled"),
            ));
        }
        if deadline.is_some_and(|deadline| started.elapsed() >= deadline) {
            return Ok(search_response(
                hits,
                ProviderSearchOutcome::TimedOut,
                false,
                false,
                Some("scan deadline reached"),
            ));
        }
        if !chunk_selected(&chunk, request) {
            continue;
        }
        scanned += 1;
        let records = load_chunk(state, &chunk)?;
        for record in records.iter() {
            if cancellation.is_cancelled() {
                return Ok(search_response(
                    hits,
                    ProviderSearchOutcome::Cancelled,
                    false,
                    false,
                    Some("scan cancelled"),
                ));
            }
            if record_selected(record, request) && matcher.matches(record) {
                let text = record.text.as_deref().unwrap_or_default();
                let (preview, truncated) = bounded_preview(text);
                hits.push(SessionSearchHit {
                    locator: record.locator.clone(),
                    content_kind: record.content_kind,
                    matched_field: record.field.unwrap_or(SearchField::Text),
                    provider_id: PLUGIN_ID.to_owned(),
                    provider_rank: u32::try_from(hits.len() + 1).unwrap_or(u32::MAX),
                    provider_score: None,
                    preview: Some(preview),
                    preview_truncated: truncated,
                });
                if hits.len() >= request.limit.min(MAX_HITS) {
                    return Ok(search_response(
                        hits,
                        ProviderSearchOutcome::Complete,
                        true,
                        !incomplete,
                        None,
                    ));
                }
            }
        }
    }
    let outcome = if incomplete {
        ProviderSearchOutcome::Partial
    } else {
        ProviderSearchOutcome::Complete
    };
    Ok(search_response(
        hits,
        outcome,
        true,
        !incomplete,
        incomplete.then_some("scan chunk limit reached"),
    ))
}

fn load_chunk(
    state: &ScanSnapshot,
    chunk: &ChunkMetadata,
) -> Result<Arc<Vec<SessionSearchRecord>>, ProviderError> {
    let cached = state
        .cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(chunk.id);
    if let Some(records) = cached {
        return Ok(records);
    }
    if chunk.normalized_bytes > MAX_CHUNK_BYTES as u64
        || chunk.record_count as usize > MAX_CHUNK_RECORDS
    {
        return Err(ProviderError::new(
            "corrupt_index",
            "chunk declares out-of-bounds size",
        ));
    }
    let compressed =
        fs::read(state.root.join("chunks").join(&chunk.file_name)).map_err(|error| {
            ProviderError::new(
                "corrupt_index",
                format!("missing chunk {}: {error}", chunk.id),
            )
        })?;
    if compressed.len() as u64 != chunk.compressed_bytes
        || digest(&compressed) != chunk.compressed_sha256
    {
        return Err(ProviderError::new(
            "corrupt_index",
            "compressed chunk checksum mismatch",
        ));
    }
    let normalized_bytes = usize::try_from(chunk.normalized_bytes)
        .map_err(|_| ProviderError::new("corrupt_index", "chunk size cannot fit in memory"))?;
    if compressed.is_empty() || normalized_bytes / compressed.len().max(1) > MAX_DECOMPRESSION_RATIO
    {
        return Err(ProviderError::new(
            "corrupt_index",
            "chunk compression ratio exceeds safety bound",
        ));
    }
    let mut decoder = zstd::stream::read::Decoder::new(compressed.as_slice())
        .map_err(|error| ProviderError::io(&error))?;
    let mut normalized = Vec::with_capacity(normalized_bytes);
    decoder
        .by_ref()
        .take((MAX_CHUNK_BYTES + 1) as u64)
        .read_to_end(&mut normalized)
        .map_err(|error| ProviderError::io(&error))?;
    if normalized.len() as u64 != chunk.normalized_bytes
        || digest(&normalized) != chunk.normalized_sha256
    {
        return Err(ProviderError::new(
            "corrupt_index",
            "normalized chunk checksum mismatch",
        ));
    }
    let records: Vec<SessionSearchRecord> = serde_json::from_slice(&normalized)
        .map_err(|error| ProviderError::new("corrupt_index", error.to_string()))?;
    if records.len() != chunk.record_count as usize {
        return Err(ProviderError::new(
            "corrupt_index",
            "chunk record count mismatch",
        ));
    }
    let records = Arc::new(records);
    state
        .cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(chunk.id, Arc::clone(&records), normalized.len());
    Ok(records)
}

#[derive(Debug)]
enum CompiledQuery {
    Text {
        needle: String,
        mode: TextMatchMode,
        fields: BTreeSet<SearchField>,
        regex: Option<Regex>,
    },
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
}

impl CompiledQuery {
    fn compile(query: &SessionSearchQuery) -> Result<Self, ProviderError> {
        match query {
            SessionSearchQuery::Text { text, mode, fields } => {
                if !matches!(
                    mode,
                    TextMatchMode::Terms | TextMatchMode::Phrase | TextMatchMode::Regex
                ) {
                    return Err(ProviderError::new(
                        "unsupported_query",
                        "scan provider supports terms, phrase, and regex",
                    ));
                }
                let regex = if *mode == TextMatchMode::Regex {
                    if text.len() > 1024 || text.contains("(?") {
                        return Err(ProviderError::new(
                            "unsupported_query",
                            "regex exceeds bounded Rust-regex policy",
                        ));
                    }
                    Some(Regex::new(text).map_err(|error| {
                        ProviderError::new("unsupported_query", error.to_string())
                    })?)
                } else {
                    None
                };
                Ok(Self::Text {
                    needle: text.clone(),
                    mode: *mode,
                    fields: fields.clone(),
                    regex,
                })
            }
            SessionSearchQuery::And { clauses } => Ok(Self::And(
                clauses
                    .iter()
                    .map(Self::compile)
                    .collect::<Result<_, _>>()?,
            )),
            SessionSearchQuery::Or { clauses } => Ok(Self::Or(
                clauses
                    .iter()
                    .map(Self::compile)
                    .collect::<Result<_, _>>()?,
            )),
            SessionSearchQuery::Not { clause } => Ok(Self::Not(Box::new(Self::compile(clause)?))),
        }
    }

    fn matches(&self, record: &SessionSearchRecord) -> bool {
        match self {
            Self::Text {
                needle,
                mode,
                fields,
                regex,
            } => {
                if !fields.is_empty() && record.field.is_none_or(|field| !fields.contains(&field)) {
                    return false;
                }
                let text = record.text.as_deref().unwrap_or_default();
                match mode {
                    TextMatchMode::Terms => needle
                        .split_whitespace()
                        .all(|term| text.to_lowercase().contains(&term.to_lowercase())),
                    TextMatchMode::Phrase => text.to_lowercase().contains(&needle.to_lowercase()),
                    TextMatchMode::Regex => {
                        regex.as_ref().is_some_and(|regex| regex.is_match(text))
                    }
                    TextMatchMode::Prefix | TextMatchMode::Fuzzy => false,
                }
            }
            Self::And(clauses) => clauses.iter().all(|clause| clause.matches(record)),
            Self::Or(clauses) => clauses.iter().any(|clause| clause.matches(record)),
            Self::Not(clause) => !clause.matches(record),
        }
    }
}

fn chunk_selected(chunk: &ChunkMetadata, request: &SessionSearchRequest) -> bool {
    (request.filters.session_ids.is_empty()
        || request.filters.session_ids.contains(&chunk.session_id))
        && (request.filters.content_kinds.is_empty()
            || !request
                .filters
                .content_kinds
                .is_disjoint(&chunk.content_kinds))
}

fn record_selected(record: &SessionSearchRecord, request: &SessionSearchRequest) -> bool {
    let filters = &request.filters;
    (filters.session_ids.is_empty() || filters.session_ids.contains(&record.locator.session_id))
        && (filters.content_kinds.is_empty()
            || filters.content_kinds.contains(&record.content_kind))
        && filters
            .after_timestamp_ms
            .is_none_or(|after| record.timestamp_ms >= after)
        && filters
            .before_timestamp_ms
            .is_none_or(|before| record.timestamp_ms <= before)
        && filters.working_directory.as_ref().is_none_or(|directory| {
            record
                .attributes
                .get("working_directory")
                .is_some_and(|value| Path::new(value) == directory)
        })
        && string_filter(&filters.tool_names, record.attributes.get("tool_name"))
        && string_filter(&filters.tool_statuses, record.attributes.get("tool_status"))
        && string_filter(&filters.providers, record.attributes.get("provider"))
        && string_filter(&filters.models, record.attributes.get("model"))
        && string_filter(&filters.agents, record.attributes.get("agent"))
        && string_filter(&filters.sources, record.attributes.get("source"))
}

fn string_filter(filters: &BTreeSet<String>, value: Option<&String>) -> bool {
    filters.is_empty() || value.is_some_and(|value| filters.contains(value))
}

fn search_response(
    hits: Vec<SessionSearchHit>,
    outcome: ProviderSearchOutcome,
    query_complete: bool,
    coverage_complete: bool,
    message: Option<&str>,
) -> SessionSearchResponse {
    SessionSearchResponse {
        provider_id: PLUGIN_ID.to_owned(),
        outcome,
        hits,
        next_cursor: None,
        query_complete,
        coverage_complete,
        searched_content: large_content_kinds().into_iter().collect(),
        excluded_content: Vec::new(),
        message: message.map(str::to_owned),
    }
}

fn bounded_preview(text: &str) -> (String, bool) {
    if text.len() <= MAX_PREVIEW_BYTES {
        return (text.to_owned(), false);
    }
    let mut end = MAX_PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn publish_manifest(root: &Path, manifest: &Manifest) -> Result<(), ProviderError> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| ProviderError::new("encode_failed", error.to_string()))?;
    atomic_write(&root.join("manifest.json"), &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| ProviderError::new("path_confinement", "path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| ProviderError::io(&error))?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| ProviderError::io(&error))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(ProviderError::io(&error));
    }
    fs::rename(&temporary, path).map_err(|error| ProviderError::io(&error))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| ProviderError::io(&error))
}

fn validate_root(root: &Path) -> Result<(), ProviderError> {
    if !root.is_absolute()
        || root.parent().is_none()
        || root == Path::new("/")
        || root
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ProviderError::new(
            "path_confinement",
            "storage_root must be absolute, confined, and non-root",
        ));
    }
    for ancestor in root.ancestors().take(2) {
        if fs::symlink_metadata(ancestor).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(ProviderError::new(
                "path_confinement",
                "storage_root or its parent is a symlink",
            ));
        }
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn serialized_records_bytes(records: &[SessionSearchRecord]) -> usize {
    serde_json::to_vec(records).map_or(0, |bytes| bytes.len())
}
fn cleanup_chunks(root: &Path, chunks: &[ChunkMetadata]) {
    for chunk in chunks {
        let _ = fs::remove_file(root.join("chunks").join(&chunk.file_name));
    }
}
fn bounded_message(message: &str) -> String {
    message.chars().take(512).collect()
}

#[derive(Debug)]
struct ProviderError {
    code: &'static str,
    message: String,
}
impl ProviderError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    fn io(error: &std::io::Error) -> Self {
        Self::new("storage_error", error.to_string())
    }
}
fn error_response(error: &ProviderError) -> ServiceResponse {
    ServiceResponse::error(error.code, bounded_message(&error.message))
}
fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}
fn decode_request<T: serde::de::DeserializeOwned>(
    context: &NativeServiceContext,
    operation: impl FnOnce(T) -> ServiceResponse,
) -> ServiceResponse {
    match context.request.payload_json::<T>() {
        Ok(request) => operation(request),
        Err(error) => ServiceResponse::error("invalid_request", error.to_string()),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        CompressedSessionSearchPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(
    CompressedSessionSearchPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_search::{
        SearchCanonicalGeneration, SessionSearchFilters, SessionSearchLocator,
    };

    fn config(root: &Path) -> ProviderConfig {
        ProviderConfig {
            storage_root: Some(root.to_path_buf()),
            quota_bytes: 16 * 1024 * 1024,
            session_quota_bytes: 64 * 1024 * 1024,
            ..ProviderConfig::default()
        }
    }
    fn record(session_id: SessionId, sequence: u64, text: &str) -> SessionSearchRecord {
        SessionSearchRecord {
            schema_version: CURRENT_SEARCH_RECORD_VERSION,
            record_id: format!("r-{sequence}"),
            locator: SessionSearchLocator {
                session_id,
                sequence,
                record_id: Some(format!("r-{sequence}")),
            },
            timestamp_ms: sequence,
            content_kind: SearchContentKind::ShellOutput,
            field: Some(SearchField::StandardOutput),
            text: Some(text.to_owned()),
            attributes: BTreeMap::from([("stream".to_owned(), "stdout".to_owned())]),
            source_bytes: text.len() as u64,
            normalized_bytes: text.len() as u64,
            indexed_bytes: text.len() as u64,
            truncated: false,
            source_range_start: Some(0),
            source_range_end: Some(text.len() as u64),
            chunk_ordinal: Some(0),
            chunk_count: Some(1),
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
        }
    }
    fn batch(
        session_id: SessionId,
        records: Vec<SessionSearchRecord>,
    ) -> ApplySearchRecordsRequest {
        let last = records.last().map(|record| record.locator.sequence);
        ApplySearchRecordsRequest {
            provider_id: PLUGIN_ID.to_owned(),
            batch_id: "batch-1".to_owned(),
            generation: SearchCanonicalGeneration {
                session_id,
                fingerprint: "generation".to_owned(),
                last_sequence: last,
            },
            expected_previous_sequence: None,
            expected_previous_session_text_bytes: 0,
            indexed_through_sequence: last,
            records,
        }
    }
    fn query(text: &str, mode: TextMatchMode) -> SessionSearchRequest {
        SessionSearchRequest {
            query: SessionSearchQuery::Text {
                text: text.to_owned(),
                mode,
                fields: BTreeSet::new(),
            },
            filters: SessionSearchFilters {
                content_kinds: large_content_kinds(),
                ..SessionSearchFilters::default()
            },
            sort: SessionSearchSort::ProviderRelevance,
            limit: 20,
            cursor: None,
            deadline_ms: Some(5_000),
        }
    }

    #[test]
    fn apply_restart_search_and_duplicate_are_durable() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        let session = SessionId::new();
        let plugin = CompressedSessionSearchPlugin::default();
        let request = batch(session, vec![record(session, 1, "fatal database locked")]);
        let applied = plugin
            .with_state(&config, |state| {
                apply_batch(state, &config, &request, &ServiceCancellation::default())
            })
            .expect("apply");
        assert_eq!(applied.outcome, ApplyBatchOutcome::Applied);
        drop(plugin);
        let plugin = CompressedSessionSearchPlugin::default();
        let response = plugin
            .with_state(&config, |state| {
                scan(
                    &state.scan_snapshot(),
                    &query("database locked", TextMatchMode::Phrase),
                    &CompiledQuery::compile(
                        &query("database locked", TextMatchMode::Phrase).query,
                    )?,
                    &ServiceCancellation::default(),
                )
            })
            .expect("search");
        assert_eq!(response.hits.len(), 1);
        let duplicate = plugin
            .with_state(&config, |state| {
                apply_batch(state, &config, &request, &ServiceCancellation::default())
            })
            .expect("duplicate");
        assert_eq!(duplicate.outcome, ApplyBatchOutcome::Duplicate);
    }

    #[test]
    fn corruption_missing_partial_and_high_ratio_fail_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        let session = SessionId::new();
        let plugin = CompressedSessionSearchPlugin::default();
        let request = batch(session, vec![record(session, 1, "searchable output")]);
        plugin
            .with_state(&config, |state| {
                apply_batch(state, &config, &request, &ServiceCancellation::default())
            })
            .expect("apply");
        let chunk = plugin
            .with_state(&config, |state| Ok(state.manifest.chunks[0].clone()))
            .expect("chunk");
        fs::write(
            directory.path().join("chunks").join(&chunk.file_name),
            b"partial",
        )
        .expect("corrupt");
        let error = plugin
            .with_state(&config, |state| load_chunk(&state.scan_snapshot(), &chunk))
            .expect_err("corrupt");
        assert_eq!(error.code, "corrupt_index");
        fs::remove_file(directory.path().join("chunks").join(&chunk.file_name)).expect("remove");
        let error = plugin
            .with_state(&config, |state| load_chunk(&state.scan_snapshot(), &chunk))
            .expect_err("missing");
        assert_eq!(error.code, "corrupt_index");
        let mut ratio = chunk;
        ratio.compressed_bytes = 1;
        ratio.normalized_bytes = MAX_CHUNK_BYTES as u64;
        fs::write(directory.path().join("chunks").join(&ratio.file_name), [0]).expect("ratio");
        ratio.compressed_sha256 = digest(&[0]);
        let error = plugin
            .with_state(&config, |state| load_chunk(&state.scan_snapshot(), &ratio))
            .expect_err("ratio");
        assert_eq!(error.code, "corrupt_index");
    }

    #[test]
    fn cancellation_timeout_regex_pathology_huge_line_and_quota_are_bounded() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        let session = SessionId::new();
        let plugin = CompressedSessionSearchPlugin::default();
        let huge = "x".repeat(bcode_session_search::DEFAULT_MAX_TEXT_BYTES_PER_RECORD);
        let huge_request = batch(session, vec![record(session, 1, &huge)]);
        let applied = plugin
            .with_state(&config, |state| {
                apply_batch(
                    state,
                    &config,
                    &huge_request,
                    &ServiceCancellation::default(),
                )
            })
            .expect("bounded huge record is partitioned and accepted");
        assert_eq!(applied.outcome, ApplyBatchOutcome::Applied);
        assert!(
            plugin
                .with_state(&config, |state| Ok(state.manifest.chunks.len()))
                .expect("chunk count")
                >= 1
        );
        assert!(CompiledQuery::compile(&query("(?=x)", TextMatchMode::Regex).query).is_err());
        let quota_directory = tempfile::tempdir().expect("quota tempdir");
        let quota_config = ProviderConfig {
            storage_root: Some(quota_directory.path().to_path_buf()),
            quota_bytes: 16 * 1024 * 1024,
            session_quota_bytes: 4,
            ..ProviderConfig::default()
        };
        let quota_plugin = CompressedSessionSearchPlugin::default();
        let quota_session = SessionId::new();
        let error = quota_plugin
            .with_state(&quota_config, |state| {
                apply_batch(
                    state,
                    &quota_config,
                    &batch(quota_session, vec![record(quota_session, 1, "quota")]),
                    &ServiceCancellation::default(),
                )
            })
            .expect_err("quota");
        assert_eq!(error.code, "quota_exceeded");
        let cancellation = ServiceCancellation::default();
        cancellation.cancel();
        let cancellation_request = ApplySearchRecordsRequest {
            batch_id: "batch-2".to_owned(),
            expected_previous_sequence: Some(1),
            expected_previous_session_text_bytes: huge.len() as u64,
            ..batch(session, vec![record(session, 2, "cancel")])
        };
        let error = plugin
            .with_state(&config, |state| {
                apply_batch(state, &config, &cancellation_request, &cancellation)
            })
            .expect_err("cancel");
        assert_eq!(error.code, "cancelled");
        let mut timed = query("x", TextMatchMode::Terms);
        timed.deadline_ms = Some(0);
        let response = plugin
            .with_state(&config, |state| {
                scan(
                    &state.scan_snapshot(),
                    &timed,
                    &CompiledQuery::compile(&timed.query)?,
                    &ServiceCancellation::default(),
                )
            })
            .expect("timeout");
        assert_eq!(response.outcome, ProviderSearchOutcome::TimedOut);
        let mut unsupported = query("x", TextMatchMode::Terms);
        unsupported
            .filters
            .roles
            .insert(bcode_session_search::SearchMessageRole::User);
        let response = plugin.search(&config, &unsupported, &ServiceCancellation::default());
        assert!(response.error.is_some());
    }

    #[test]
    fn scan_limiter_allows_two_and_bounds_a_waiting_third() {
        let limiter = Arc::new(ScanLimiter::default());
        let first = limiter
            .acquire(
                &ServiceCancellation::default(),
                Some(Duration::from_secs(1)),
            )
            .expect("first permit");
        let second = limiter
            .acquire(
                &ServiceCancellation::default(),
                Some(Duration::from_secs(1)),
            )
            .expect("second permit");
        let waiting = Arc::clone(&limiter);
        let started = Instant::now();
        let third = std::thread::spawn(move || {
            matches!(
                waiting.acquire(
                    &ServiceCancellation::default(),
                    Some(Duration::from_millis(30)),
                ),
                Err(ProviderSearchOutcome::TimedOut)
            )
        })
        .join()
        .expect("third waiter");
        assert!(third);
        assert!(started.elapsed() >= Duration::from_millis(20));
        drop(first);
        drop(second);
        assert!(
            limiter
                .acquire(
                    &ServiceCancellation::default(),
                    Some(Duration::from_secs(1))
                )
                .is_ok()
        );
    }

    #[test]
    #[ignore = "manual compressed provider performance baseline"]
    #[allow(clippy::too_many_lines)]
    fn benchmark_compressed_provider() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        let session = SessionId::new();
        let plugin = CompressedSessionSearchPlugin::default();
        let records = std::env::var("BCODE_COMPRESSED_SEARCH_BENCH_RECORDS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(256);
        assert!((1..=25_000).contains(&records));
        let records = (1..=records)
            .map(|sequence| {
                let sequence = u64::try_from(sequence).expect("bounded sequence");
                record(
                    session,
                    sequence,
                    &format!(
                        "repeated output line {sequence} searchable-token {}",
                        "context ".repeat(100)
                    ),
                )
            })
            .collect::<Vec<_>>();
        let normalized = records
            .iter()
            .map(|record| record.text.as_ref().map_or(0, String::len))
            .sum::<usize>();
        let started = Instant::now();
        let request = batch(session, records);
        let record_count = request.records.len();
        plugin
            .with_state(&config, |state| {
                apply_batch(state, &config, &request, &ServiceCancellation::default())
            })
            .expect("apply");
        let ingestion = started.elapsed();
        let matcher =
            CompiledQuery::compile(&query("searchable-token", TextMatchMode::Terms).query)
                .expect("matcher");
        let mut durations = Vec::new();
        for _ in 0..100 {
            let started = Instant::now();
            let response = plugin
                .with_state(&config, |state| {
                    scan(
                        &state.scan_snapshot(),
                        &query("searchable-token", TextMatchMode::Terms),
                        &matcher,
                        &ServiceCancellation::default(),
                    )
                })
                .expect("scan");
            assert_eq!(response.hits.len(), 20);
            durations.push(started.elapsed().as_micros());
        }
        durations.sort_unstable();
        let mut cold_durations = Vec::new();
        for _ in 0..20 {
            let snapshot = plugin
                .with_state(&config, |state| {
                    state
                        .cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clear();
                    Ok(state.scan_snapshot())
                })
                .expect("cold snapshot");
            let started = Instant::now();
            let response = scan(
                &snapshot,
                &query("searchable-token", TextMatchMode::Terms),
                &matcher,
                &ServiceCancellation::default(),
            )
            .expect("cold scan");
            assert_eq!(response.hits.len(), 20);
            cold_durations.push(started.elapsed().as_micros());
        }
        cold_durations.sort_unstable();
        let parallel_started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let snapshot = plugin
                    .with_state(&config, |state| Ok(state.scan_snapshot()))
                    .expect("parallel snapshot");
                scope.spawn(move || {
                    let matcher = CompiledQuery::compile(
                        &query("searchable-token", TextMatchMode::Terms).query,
                    )
                    .expect("parallel matcher");
                    let response = scan(
                        &snapshot,
                        &query("searchable-token", TextMatchMode::Terms),
                        &matcher,
                        &ServiceCancellation::default(),
                    )
                    .expect("parallel scan");
                    assert_eq!(response.hits.len(), 20);
                });
            }
        });
        let parallel_us = parallel_started.elapsed().as_micros();
        let compressed = plugin
            .with_state(&config, |state| {
                Ok(state
                    .manifest
                    .chunks
                    .iter()
                    .map(|chunk| chunk.compressed_bytes)
                    .sum::<u64>())
            })
            .expect("bytes");
        println!(
            "compressed_session_search_benchmark records={record_count} normalized_bytes={normalized} compressed_bytes={compressed} ratio_permille={} ingestion_us={} warm_query_p50_us={} warm_query_p95_us={} warm_query_p99_us={} cold_query_p50_us={} cold_query_p95_us={} cold_query_p99_us={} parallel_two_scan_us={parallel_us}",
            compressed * 1000 / normalized as u64,
            ingestion.as_micros(),
            durations[50],
            durations[95],
            durations[99],
            cold_durations[10],
            cold_durations[19],
            cold_durations[19]
        );
        assert!(compressed < normalized as u64 / 2);
    }
}
