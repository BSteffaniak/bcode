//! Session-search provider discovery and typed service routing.

use bcode_session_search::{
    ApplySearchRecordsRequest, ApplySearchRecordsResponse, BackfillSessionSearchRequest,
    FederatedProviderContribution, FederatedProviderReport, FederatedSessionSearchResponse,
    HydratedSessionSearchHit, ListSessionSearchProvidersResponse, MAX_BACKFILL_BATCHES_PER_SESSION,
    MAX_BACKFILL_SESSIONS, MAX_FEDERATED_PROVIDERS, OP_APPLY_BATCH, OP_CAPABILITIES, OP_PURGE,
    OP_REBUILD, OP_REMOVE_SESSION, OP_SEARCH, OP_STATUS, PurgeSessionSearchRequest,
    RebuildSessionSearchRequest, RemoveSessionSearchRequest, SESSION_SEARCH_INTERFACE_ID,
    SearchCanonicalGeneration, SearchErrorCode, SearchFeature, SearchHitHydrationOutcome,
    SessionSearchBackfillOutcome, SessionSearchBackfillResponse,
    SessionSearchBackfillSessionResult, SessionSearchCapabilities, SessionSearchContentRoute,
    SessionSearchMaintenanceResponse, SessionSearchProviderFailure, SessionSearchProviderInfo,
    SessionSearchRequest, SessionSearchResponse, SessionSearchServiceError, SessionSearchStatus,
    aggregate_federated_search, plan_session_search_with_policy_and_routes,
};
use futures::future::join_all;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::ServerState;

const MAX_DIRTY_SESSION_SEARCH_SESSIONS: usize = 1_024;
const MAX_INCREMENTAL_BATCHES_PER_SESSION: usize = 16;
const INCREMENTAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(30);
const MAINTENANCE_PROVIDER_SLICES_BEFORE_YIELD: usize = 1;

/// Cooperative scheduler that prevents explicit maintenance from monopolizing provider work.
///
/// Maintenance is serialized by `maintenance_gate` and excludes ordinary work through the
/// write side of `work_gate`. Tokio's `RwLock` acquires fairly, so a pending writer already
/// blocks newly arriving readers; maintenance must not additionally spin waiting for the
/// ordinary-waiter count to reach zero. Ordinary ingestion legitimately requeues retryable
/// work indefinitely, so such a spin can livelock and never admit maintenance.
#[derive(Debug, Default)]
pub(crate) struct SessionSearchWorkScheduler {
    maintenance_gate: Mutex<()>,
    work_gate: RwLock<()>,
}

impl SessionSearchWorkScheduler {
    async fn run_ordinary<T>(&self, work: impl std::future::Future<Output = T>) -> T {
        let guard = self.work_gate.read().await;
        let result = work.await;
        drop(guard);
        result
    }

    async fn run_maintenance<T>(&self, work: impl std::future::Future<Output = T>) -> T {
        let maintenance_guard = self.maintenance_gate.lock().await;
        let work_guard = self.work_gate.write().await;
        let result = work.await;
        drop(work_guard);
        drop(maintenance_guard);
        result
    }
}

/// Run an explicit provider-owned purge and return fresh provider status.
///
/// # Errors
///
/// Returns a normalized service error when the provider is unavailable, does not advertise purge,
/// rejects the confirmation, or returns invalid status after the operation.
pub async fn purge_provider(
    state: &ServerState,
    provider_id: &str,
    confirmation: String,
) -> Result<SessionSearchMaintenanceResponse, SessionSearchServiceError> {
    invoke_provider_maintenance(
        state,
        provider_id,
        SearchFeature::Purge,
        OP_PURGE,
        &PurgeSessionSearchRequest {
            provider_id: provider_id.to_owned(),
            confirmation,
        },
    )
    .await
}

/// Run an explicit provider-owned rebuild and return fresh provider status.
///
/// Rebuild recreates empty derived state. Incremental ingestion is scheduled only for sessions
/// that subsequently commit canonical mutations; historical backfill remains a separate explicit
/// maintenance workflow.
///
/// # Errors
///
/// Returns a normalized service error when the provider is unavailable, does not advertise
/// rebuild, rejects the confirmation, or returns invalid status after the operation.
pub async fn rebuild_provider(
    state: &ServerState,
    provider_id: &str,
    confirmation: String,
) -> Result<SessionSearchMaintenanceResponse, SessionSearchServiceError> {
    invoke_provider_maintenance(
        state,
        provider_id,
        SearchFeature::Rebuild,
        OP_REBUILD,
        &RebuildSessionSearchRequest {
            provider_id: provider_id.to_owned(),
            confirmation,
        },
    )
    .await
}

pub(crate) fn merge_complete_backfill_issues(
    aggregate: &mut Vec<bcode_session_search::CompleteSessionSearchBackfillIssueSummary>,
    sessions: &[bcode_session_search::SessionSearchBackfillSessionResult],
) {
    for session in sessions {
        let Some(error) = &session.error else {
            continue;
        };
        if let Some(issue) = aggregate.iter_mut().find(|issue| issue.code == error.code) {
            issue.count = issue.count.saturating_add(1);
            if issue.sample_session_ids.len()
                < bcode_session_search::MAX_COMPLETE_BACKFILL_ISSUE_SAMPLES
            {
                issue.sample_session_ids.push(session.session_id);
            }
            continue;
        }
        if aggregate.len() >= bcode_session_search::MAX_COMPLETE_BACKFILL_ISSUE_SAMPLES {
            continue;
        }
        aggregate.push(
            bcode_session_search::CompleteSessionSearchBackfillIssueSummary {
                code: error.code,
                count: 1,
                retryable: error.retryable,
                sample_session_ids: vec![session.session_id],
                sample_message: Some(bounded_message(&error.message)),
            },
        );
    }
}

const MAX_COMPLETE_BACKFILL_CONVERGENCE_PASSES: usize =
    bcode_session_search::MAX_COMPLETE_BACKFILL_CONVERGENCE_PASSES;

fn complete_backfill_progress(
    provider_ids: &[String],
    current_provider_id: Option<&str>,
    catalog_revision_started: u64,
    convergence_pass: usize,
    completed_providers: &[bcode_session_search::CompleteSessionSearchBackfillProviderResult],
    current_provider: Option<&bcode_session_search::CompleteSessionSearchBackfillProviderResult>,
) -> bcode_session_search::CompleteSessionSearchBackfillProgress {
    let mut providers = completed_providers.to_vec();
    if let Some(current_provider) = current_provider {
        providers.push(current_provider.clone());
    }
    let selected_sessions = providers.iter().fold(0usize, |total, provider| {
        total.saturating_add(provider.selected_sessions)
    });
    let completed_sessions = providers.iter().fold(0usize, |total, provider| {
        total.saturating_add(provider.completed_sessions)
    });
    let incomplete_sessions = providers.iter().fold(0usize, |total, provider| {
        total.saturating_add(provider.incomplete_sessions)
    });
    let failed_sessions = providers.iter().fold(0usize, |total, provider| {
        total.saturating_add(provider.failed_sessions)
    });
    bcode_session_search::CompleteSessionSearchBackfillProgress {
        provider_ids: provider_ids.to_vec(),
        current_provider_id: current_provider_id.map(str::to_owned),
        catalog_revision_started,
        convergence_pass,
        providers_completed: completed_providers.len(),
        selected_sessions,
        visited_sessions: completed_sessions
            .saturating_add(incomplete_sessions)
            .saturating_add(failed_sessions),
        completed_sessions,
        incomplete_sessions,
        failed_sessions,
        providers,
    }
}

fn terminal_complete_backfill_response(
    provider_ids: &[String],
    revision_started: u64,
    revision_completed: u64,
    convergence_passes: usize,
    cancelled: bool,
    mut providers: Vec<bcode_session_search::CompleteSessionSearchBackfillProviderResult>,
) -> bcode_session_search::CompleteSessionSearchBackfillResponse {
    if cancelled {
        let completed_ids = providers
            .iter()
            .map(|provider| provider.provider_id.clone())
            .collect::<BTreeSet<_>>();
        providers.extend(
            provider_ids
                .iter()
                .filter(|provider_id| !completed_ids.contains(*provider_id))
                .map(|provider_id| {
                    bcode_session_search::CompleteSessionSearchBackfillProviderResult {
                        provider_id: provider_id.clone(),
                        selected_sessions: 0,
                        completed_sessions: 0,
                        incomplete_sessions: 0,
                        failed_sessions: 0,
                        catalog_pages: 0,
                        issues: Vec::new(),
                        error: None,
                    }
                }),
        );
    }
    bcode_session_search::CompleteSessionSearchBackfillResponse {
        provider_ids: provider_ids.to_vec(),
        catalog_revision_started: revision_started,
        catalog_revision_completed: revision_completed,
        convergence_passes,
        cancelled,
        providers,
    }
}

fn backfill_continuation_cursor(
    starting_cursor: Option<bcode_session_search::SessionSearchBackfillCursor>,
    summaries: &[bcode_session_models::SessionSummary],
    sessions: &[SessionSearchBackfillSessionResult],
    deadline_reached: bool,
    catalog_cursor: Option<bcode_session_search::SessionSearchBackfillCursor>,
) -> Option<bcode_session_search::SessionSearchBackfillCursor> {
    if !deadline_reached {
        return catalog_cursor;
    }
    sessions
        .iter()
        .rev()
        .find(|result| result.outcome == SessionSearchBackfillOutcome::Complete)
        .and_then(|result| {
            summaries
                .iter()
                .find(|summary| summary.id == result.session_id)
        })
        .map(
            |summary| bcode_session_search::SessionSearchBackfillCursor {
                updated_at_ms: summary.updated_at_ms,
                session_id: summary.id,
            },
        )
        .or(starting_cursor)
}

/// Run explicit complete historical backfill across every selected provider.
///
/// Provider and catalog work remains bounded by the existing single-provider request. Reissuing
/// this operation is safe because each provider owns durable sequence/text checkpoints.
///
/// # Errors
///
/// Returns a normalized error when selection is invalid, search is disabled, no eligible provider
/// exists, or catalog discovery fails.
#[allow(clippy::too_many_lines)]
pub async fn complete_backfill(
    state: &ServerState,
    request: bcode_session_search::CompleteSessionSearchBackfillRequest,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    progress: Option<
        &(dyn Fn(bcode_session_search::CompleteSessionSearchBackfillProgress) + Send + Sync),
    >,
) -> Result<bcode_session_search::CompleteSessionSearchBackfillResponse, SessionSearchServiceError>
{
    request
        .validate()
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: bounded_message(&error.to_string()),
            retryable: false,
        })?;
    if !state.session_search_enabled {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: "session search is globally disabled".to_owned(),
            retryable: false,
        });
    }
    let inventory = list_providers(state).await;
    let mut provider_ids = inventory
        .providers
        .into_iter()
        .filter(|provider| {
            provider
                .capabilities
                .features
                .contains(&SearchFeature::HistoricalBackfill)
                && request
                    .provider_id
                    .as_ref()
                    .is_none_or(|selected| selected == &provider.plugin_id)
        })
        .map(|provider| provider.plugin_id)
        .collect::<Vec<_>>();
    provider_ids.sort();
    if provider_ids.len() > bcode_session_search::MAX_COMPLETE_BACKFILL_PROVIDERS {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::QuotaExceeded,
            message: "too many enabled session-search providers for one complete backfill"
                .to_owned(),
            retryable: false,
        });
    }
    if provider_ids.is_empty() {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: request.provider_id.map_or_else(
                || "no enabled session-search provider supports historical backfill".to_owned(),
                |provider_id| {
                    format!("session-search provider '{provider_id}' is not enabled and ready")
                },
            ),
            retryable: false,
        });
    }

    state
        .sessions
        .wait_catalog_loaded()
        .await
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: bounded_message(&error.to_string()),
            retryable: true,
        })?;
    let revision_started = state.session_catalog.revision();
    let mut pass_started_revision = revision_started;
    let mut convergence_passes = 0;
    let mut providers = Vec::new();
    loop {
        convergence_passes += 1;
        providers.clear();
        if let Some(progress) = progress {
            progress(complete_backfill_progress(
                &provider_ids,
                None,
                revision_started,
                convergence_passes,
                &providers,
                None,
            ));
        }
        for provider_id in &provider_ids {
            if let Some(progress) = progress {
                progress(complete_backfill_progress(
                    &provider_ids,
                    Some(provider_id),
                    revision_started,
                    convergence_passes,
                    &providers,
                    None,
                ));
            }
            let mut cursor = None;
            let mut aggregate = bcode_session_search::CompleteSessionSearchBackfillProviderResult {
                provider_id: provider_id.clone(),
                selected_sessions: 0,
                completed_sessions: 0,
                incomplete_sessions: 0,
                failed_sessions: 0,
                catalog_pages: 0,
                issues: Vec::new(),
                error: None,
            };
            let mut selected_session_ids = BTreeSet::new();
            let mut completed_session_ids = BTreeSet::new();
            let mut incomplete_session_ids = BTreeSet::new();
            let mut failed_session_ids = BTreeSet::new();
            loop {
                if cancellation.is_cancelled() {
                    break;
                }
                let response = state
                    .session_search_work
                    .run_maintenance(backfill_provider_with_cancellation(
                        state,
                        BackfillSessionSearchRequest {
                            provider_id: provider_id.clone(),
                            session_ids: request.session_ids.clone(),
                            after_timestamp_ms: request.after_timestamp_ms,
                            before_timestamp_ms: request.before_timestamp_ms,
                            cursor,
                            deadline_ms: request.slice_deadline_ms,
                        },
                        Some(cancellation),
                    ))
                    .await;
                if cancellation.is_cancelled() {
                    break;
                }
                match response {
                    Ok(response) => {
                        for session in &response.sessions {
                            selected_session_ids.insert(session.session_id);
                            match session.outcome {
                                SessionSearchBackfillOutcome::Complete => {
                                    completed_session_ids.insert(session.session_id);
                                    incomplete_session_ids.remove(&session.session_id);
                                    failed_session_ids.remove(&session.session_id);
                                }
                                SessionSearchBackfillOutcome::Incomplete => {
                                    if !completed_session_ids.contains(&session.session_id) {
                                        incomplete_session_ids.insert(session.session_id);
                                    }
                                    failed_session_ids.remove(&session.session_id);
                                }
                                SessionSearchBackfillOutcome::Failed => {
                                    if !completed_session_ids.contains(&session.session_id) {
                                        failed_session_ids.insert(session.session_id);
                                    }
                                    incomplete_session_ids.remove(&session.session_id);
                                }
                            }
                        }
                        aggregate.selected_sessions = selected_session_ids.len();
                        aggregate.completed_sessions = completed_session_ids.len();
                        aggregate.incomplete_sessions = incomplete_session_ids.len();
                        aggregate.failed_sessions = failed_session_ids.len();
                        merge_complete_backfill_issues(&mut aggregate.issues, &response.sessions);
                        aggregate.catalog_pages = aggregate.catalog_pages.saturating_add(1);
                        let needs_continuation = response.next_cursor.is_some()
                            || response.deadline_reached
                            || response.incomplete_sessions > 0;
                        let made_progress = response.sessions.iter().any(|session| {
                            session.batches_applied > 0
                                || matches!(session.outcome, SessionSearchBackfillOutcome::Complete)
                        });
                        cursor = response.next_cursor;
                        if needs_continuation && !made_progress {
                            aggregate.error = Some(SessionSearchServiceError {
                                code: SearchErrorCode::DeadlineExceeded,
                                message: "backfill slice reached its deadline before making progress; increase the per-slice deadline and explicitly re-invoke backfill"
                                    .to_owned(),
                                retryable: true,
                            });
                        }
                        if let Some(progress) = progress {
                            progress(complete_backfill_progress(
                                &provider_ids,
                                Some(provider_id),
                                revision_started,
                                convergence_passes,
                                &providers,
                                Some(&aggregate),
                            ));
                        }
                        if !needs_continuation || aggregate.error.is_some() {
                            break;
                        }
                        for _ in 0..MAINTENANCE_PROVIDER_SLICES_BEFORE_YIELD {
                            tokio::task::yield_now().await;
                        }
                    }
                    Err(error) => {
                        aggregate.error = Some(error);
                        if let Some(progress) = progress {
                            progress(complete_backfill_progress(
                                &provider_ids,
                                Some(provider_id),
                                revision_started,
                                convergence_passes,
                                &providers,
                                Some(&aggregate),
                            ));
                        }
                        break;
                    }
                }
            }
            providers.push(aggregate);
            if let Some(progress) = progress {
                progress(complete_backfill_progress(
                    &provider_ids,
                    None,
                    revision_started,
                    convergence_passes,
                    &providers,
                    None,
                ));
            }
            if cancellation.is_cancelled() {
                break;
            }
        }
        let revision_completed = state.session_catalog.revision();
        let terminal_provider_outcome = providers.iter().any(|provider| {
            provider.error.is_some()
                || provider.failed_sessions > 0
                || provider.incomplete_sessions > 0
        });
        if cancellation.is_cancelled()
            || terminal_provider_outcome
            || revision_completed == pass_started_revision
            || convergence_passes >= MAX_COMPLETE_BACKFILL_CONVERGENCE_PASSES
        {
            if !cancellation.is_cancelled()
                && !terminal_provider_outcome
                && revision_completed != pass_started_revision
            {
                for provider in &mut providers {
                    provider.error = Some(SessionSearchServiceError {
                        code: SearchErrorCode::StaleIndex,
                        message: "session catalog continued changing during bounded convergence; rerun explicit backfill"
                            .to_owned(),
                        retryable: true,
                    });
                }
            }
            let response = terminal_complete_backfill_response(
                &provider_ids,
                revision_started,
                revision_completed,
                convergence_passes,
                cancellation.is_cancelled(),
                providers,
            );
            response
                .validate()
                .map_err(|error| SessionSearchServiceError {
                    code: SearchErrorCode::Internal,
                    message: bounded_message(&error.to_string()),
                    retryable: false,
                })?;
            return Ok(response);
        }
        pass_started_revision = revision_completed;
    }
}

/// Explicitly backfill selected or bounded catalog sessions into one provider.
///
/// The daemon owns canonical selection and bounded forward reads. Progress resumes from the
/// provider's durable coverage checkpoints and no full-history work is started by normal paths.
///
/// # Errors
///
/// Returns a normalized service error for an invalid request, unavailable/incompatible provider,
/// catalog failure, or inability to obtain fresh terminal provider status.
#[allow(clippy::too_many_lines)]
pub async fn backfill_provider(
    state: &ServerState,
    request: BackfillSessionSearchRequest,
) -> Result<SessionSearchBackfillResponse, SessionSearchServiceError> {
    backfill_provider_with_cancellation(state, request, None).await
}

/// Run explicit historical backfill with caller-owned cancellation.
///
/// # Errors
///
/// Returns the same normalized errors as [`backfill_provider`].
#[allow(clippy::too_many_lines)]
pub async fn backfill_provider_with_cancellation(
    state: &ServerState,
    request: BackfillSessionSearchRequest,
    cancellation: Option<&bcode_plugin_sdk::ServiceCancellation>,
) -> Result<SessionSearchBackfillResponse, SessionSearchServiceError> {
    request
        .validate()
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: bounded_message(&error.to_string()),
            retryable: false,
        })?;
    if !state.session_search_enabled {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: "session search is globally disabled".to_owned(),
            retryable: false,
        });
    }
    let inventory = list_providers(state).await;
    let provider = inventory
        .providers
        .into_iter()
        .find(|provider| provider.plugin_id == request.provider_id)
        .ok_or_else(|| SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: format!(
                "session-search provider '{}' is not loaded and ready",
                request.provider_id
            ),
            retryable: false,
        })?;
    if !provider
        .capabilities
        .features
        .contains(&SearchFeature::IncrementalIngestion)
    {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::UnsupportedQuery,
            message: format!(
                "session-search provider '{}' does not support bounded ingestion",
                request.provider_id
            ),
            retryable: false,
        });
    }

    state
        .sessions
        .wait_catalog_loaded()
        .await
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: bounded_message(&error.to_string()),
            retryable: true,
        })?;
    let starting_cursor = request.cursor.clone();
    let mut summaries = state
        .sessions
        .session_summaries_page(
            &request.session_ids,
            request.after_timestamp_ms,
            request.before_timestamp_ms,
            request
                .cursor
                .as_ref()
                .map(|cursor| (cursor.updated_at_ms, cursor.session_id)),
            MAX_BACKFILL_SESSIONS,
        )
        .await;
    let selection_truncated = summaries.len() > MAX_BACKFILL_SESSIONS;
    summaries.truncate(MAX_BACKFILL_SESSIONS);
    let next_cursor = selection_truncated
        .then(|| summaries.last())
        .flatten()
        .map(
            |summary| bcode_session_search::SessionSearchBackfillCursor {
                updated_at_ms: summary.updated_at_ms,
                session_id: summary.id,
            },
        );

    let started = Instant::now();
    let deadline = started + Duration::from_millis(request.deadline_ms);
    let selected_sessions = summaries.len();
    let mut sessions = Vec::with_capacity(selected_sessions);
    for summary in &summaries {
        if cancellation.is_some_and(bcode_plugin_sdk::ServiceCancellation::is_cancelled)
            || Instant::now() >= deadline
        {
            break;
        }
        let session_id = summary.id;
        match state.sessions.session_health(session_id).await {
            bcode_session::SessionHealth::Migratable { source, target }
            | bcode_session::SessionHealth::BlockedOwner { source, target, .. } => {
                sessions.push(SessionSearchBackfillSessionResult {
                    session_id,
                    outcome: SessionSearchBackfillOutcome::Failed,
                    batches_applied: 0,
                    indexed_through_sequence: provider
                        .status
                        .coverage
                        .iter()
                        .find(|coverage| coverage.generation.session_id == session_id)
                        .and_then(|coverage| coverage.indexed_through_sequence),
                    canonical_tail_sequence: None,
                    error: Some(SessionSearchServiceError {
                        code: SearchErrorCode::MigrationRequired,
                        message: format!(
                            "canonical session writer epoch {source} requires explicit migration to {target} before search backfill"
                        ),
                        retryable: false,
                    }),
                });
                continue;
            }
            bcode_session::SessionHealth::WriterIncompatible { actual, expected } => {
                sessions.push(SessionSearchBackfillSessionResult {
                    session_id,
                    outcome: SessionSearchBackfillOutcome::Failed,
                    batches_applied: 0,
                    indexed_through_sequence: None,
                    canonical_tail_sequence: None,
                    error: Some(SessionSearchServiceError {
                        code: SearchErrorCode::FutureVersion,
                        message: format!(
                            "canonical session writer epoch {actual:?} is incompatible with expected epoch {expected}; upgrade Bcode before search backfill"
                        ),
                        retryable: false,
                    }),
                });
                continue;
            }
            bcode_session::SessionHealth::NotFound => {
                sessions.push(SessionSearchBackfillSessionResult {
                    session_id,
                    outcome: SessionSearchBackfillOutcome::Failed,
                    batches_applied: 0,
                    indexed_through_sequence: None,
                    canonical_tail_sequence: None,
                    error: Some(SessionSearchServiceError {
                        code: SearchErrorCode::ProviderUnavailable,
                        message: "canonical session is missing; locate or restore it before search backfill".to_owned(),
                        retryable: false,
                    }),
                });
                continue;
            }
            bcode_session::SessionHealth::Ready
            | bcode_session::SessionHealth::DegradedReadOnly { .. }
            | bcode_session::SessionHealth::ProjectionStale { .. }
            | bcode_session::SessionHealth::RepairRequired { .. } => {}
        }
        if state.session_migrations.is_active(session_id).await {
            sessions.push(SessionSearchBackfillSessionResult {
                session_id,
                outcome: SessionSearchBackfillOutcome::Failed,
                batches_applied: 0,
                indexed_through_sequence: provider
                    .status
                    .coverage
                    .iter()
                    .find(|coverage| coverage.generation.session_id == session_id)
                    .and_then(|coverage| coverage.indexed_through_sequence),
                canonical_tail_sequence: None,
                error: Some(SessionSearchServiceError {
                    code: SearchErrorCode::ProviderUnavailable,
                    message: "canonical session migration is active; retry backfill after migration reaches a terminal outcome".to_owned(),
                    retryable: true,
                }),
            });
            continue;
        }
        if let Some(root) = state.sessions.session_store_root()
            && bcode_session::lease::session_maintenance_is_active(&root, session_id).map_err(
                |error| SessionSearchServiceError {
                    code: SearchErrorCode::ProviderUnavailable,
                    message: bounded_message(&error.to_string()),
                    retryable: true,
                },
            )?
        {
            sessions.push(SessionSearchBackfillSessionResult {
                session_id,
                outcome: SessionSearchBackfillOutcome::Failed,
                batches_applied: 0,
                indexed_through_sequence: provider
                    .status
                    .coverage
                    .iter()
                    .find(|coverage| coverage.generation.session_id == session_id)
                    .and_then(|coverage| coverage.indexed_through_sequence),
                canonical_tail_sequence: None,
                error: Some(SessionSearchServiceError {
                    code: SearchErrorCode::ProviderUnavailable,
                    message: "canonical session maintenance is active; retry backfill after repair/reindex reaches a terminal outcome".to_owned(),
                    retryable: true,
                }),
            });
            continue;
        }
        let result = ingest_provider_pages(
            state,
            session_id,
            summary,
            &provider,
            MAX_BACKFILL_BATCHES_PER_SESSION,
            Some(deadline),
            false,
            cancellation,
        )
        .await;
        let session_result = match result {
            Ok(progress) => SessionSearchBackfillSessionResult {
                session_id,
                outcome: if progress.complete {
                    SessionSearchBackfillOutcome::Complete
                } else {
                    SessionSearchBackfillOutcome::Incomplete
                },
                batches_applied: progress.batches_applied,
                indexed_through_sequence: progress.indexed_through_sequence,
                canonical_tail_sequence: progress.canonical_tail_sequence,
                error: None,
            },
            Err(error) if error.deadline_reached() => {
                let indexed_before = provider
                    .status
                    .coverage
                    .iter()
                    .find(|coverage| coverage.generation.session_id == session_id)
                    .and_then(|coverage| coverage.indexed_through_sequence);
                let indexed_after = provider_status(state, &request.provider_id)
                    .await
                    .ok()
                    .and_then(|status| {
                        status
                            .coverage
                            .into_iter()
                            .find(|coverage| coverage.generation.session_id == session_id)
                            .and_then(|coverage| coverage.indexed_through_sequence)
                    });
                SessionSearchBackfillSessionResult {
                    session_id,
                    outcome: SessionSearchBackfillOutcome::Incomplete,
                    batches_applied: usize::from(indexed_after > indexed_before),
                    indexed_through_sequence: indexed_after.or(indexed_before),
                    canonical_tail_sequence: canonical_session_tail(state, session_id)
                        .await
                        .ok()
                        .flatten(),
                    error: None,
                }
            }
            Err(error) => SessionSearchBackfillSessionResult {
                session_id,
                outcome: SessionSearchBackfillOutcome::Failed,
                batches_applied: 0,
                indexed_through_sequence: provider
                    .status
                    .coverage
                    .iter()
                    .find(|coverage| coverage.generation.session_id == session_id)
                    .and_then(|coverage| coverage.indexed_through_sequence),
                canonical_tail_sequence: None,
                error: Some(SessionSearchServiceError {
                    code: if error.retryable_error() {
                        SearchErrorCode::ProviderUnavailable
                    } else {
                        SearchErrorCode::InvalidRequest
                    },
                    message: bounded_message(&error.to_string()),
                    retryable: error.retryable_error(),
                }),
            },
        };
        let incomplete = session_result.outcome == SessionSearchBackfillOutcome::Incomplete;
        sessions.push(session_result);
        if incomplete {
            break;
        }
    }
    let deadline_reached = sessions.len() < selected_sessions || Instant::now() >= deadline;
    let continuation_cursor = backfill_continuation_cursor(
        starting_cursor,
        &summaries,
        &sessions,
        deadline_reached,
        next_cursor,
    );
    let next_cursor = continuation_cursor;
    let completed_sessions = sessions
        .iter()
        .filter(|result| result.outcome == SessionSearchBackfillOutcome::Complete)
        .count();
    let incomplete_sessions = sessions
        .iter()
        .filter(|result| result.outcome == SessionSearchBackfillOutcome::Incomplete)
        .count()
        .saturating_add(selected_sessions.saturating_sub(sessions.len()));
    let failed_sessions = sessions
        .iter()
        .filter(|result| result.outcome == SessionSearchBackfillOutcome::Failed)
        .count();
    state.metrics.record_histogram(
        "server.session_search.backfill_selected_sessions",
        u64::try_from(selected_sessions).unwrap_or(u64::MAX),
    );
    state.metrics.record_histogram(
        "server.session_search.backfill_elapsed_ms",
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    if deadline_reached {
        state
            .metrics
            .increment_counter("server.session_search.backfill_deadline_total");
    }
    let status = provider_status(state, &request.provider_id).await?;
    Ok(SessionSearchBackfillResponse {
        provider_id: request.provider_id,
        selected_sessions,
        selection_truncated,
        next_cursor,
        completed_sessions,
        incomplete_sessions,
        failed_sessions,
        deadline_reached,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        sessions,
        status,
    })
}

async fn invoke_provider_maintenance<Q>(
    state: &ServerState,
    provider_id: &str,
    required_feature: SearchFeature,
    operation: &'static str,
    request: &Q,
) -> Result<SessionSearchMaintenanceResponse, SessionSearchServiceError>
where
    Q: serde::Serialize + Sync,
{
    if !state.session_search_enabled {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: "session search is globally disabled".to_owned(),
            retryable: false,
        });
    }
    let capabilities = provider_capabilities(state, provider_id).await?;
    if !capabilities.features.contains(&required_feature) {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::UnsupportedQuery,
            message: format!(
                "session-search provider '{provider_id}' does not support {operation} maintenance"
            ),
            retryable: false,
        });
    }
    let payload = serde_json::to_vec(request).map_err(|error| SessionSearchServiceError {
        code: SearchErrorCode::InvalidRequest,
        message: bounded_message(&error.to_string()),
        retryable: false,
    })?;
    let response = state
        .plugins
        .invoke_service_scoped_with_timeout(
            provider_id,
            SESSION_SEARCH_INTERFACE_ID,
            operation,
            payload,
            bcode_plugin::PluginInvocationScope::Global,
            MAINTENANCE_TIMEOUT,
        )
        .await
        .map_err(|error| {
            let error = bcode_plugin::PluginServiceCallError::from(error);
            maintenance_call_error(&error)
        })?;
    if let Some(error) = response.error {
        return Err(maintenance_call_error(
            &bcode_plugin::PluginServiceCallError::Service {
                code: error.code,
                message: error.message,
            },
        ));
    }
    let status = provider_status(state, provider_id).await?;
    Ok(SessionSearchMaintenanceResponse {
        provider_id: provider_id.to_owned(),
        operation: operation.to_owned(),
        status,
    })
}

async fn provider_capabilities(
    state: &ServerState,
    provider_id: &str,
) -> Result<SessionSearchCapabilities, SessionSearchServiceError> {
    let registered = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(SESSION_SEARCH_INTERFACE_ID)
        .is_some_and(|providers| providers.contains(provider_id));
    if !registered {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: format!("session-search provider '{provider_id}' is not loaded"),
            retryable: false,
        });
    }
    let capabilities = state
        .plugins
        .invoke_service_json::<_, SessionSearchCapabilities>(
            provider_id,
            SESSION_SEARCH_INTERFACE_ID,
            OP_CAPABILITIES,
            &(),
        )
        .await
        .map_err(|error| maintenance_call_error(&error))?;
    if capabilities.provider_id != provider_id {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider capability identity does not match selected plugin".to_owned(),
            retryable: false,
        });
    }
    capabilities
        .validate()
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::FutureVersion,
            message: bounded_message(&error.to_string()),
            retryable: false,
        })?;
    Ok(capabilities)
}

async fn provider_status(
    state: &ServerState,
    provider_id: &str,
) -> Result<SessionSearchStatus, SessionSearchServiceError> {
    let status = state
        .plugins
        .invoke_service_json::<_, SessionSearchStatus>(
            provider_id,
            SESSION_SEARCH_INTERFACE_ID,
            OP_STATUS,
            &(),
        )
        .await
        .map_err(|error| maintenance_call_error(&error))?;
    if status.provider_id != provider_id {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider status identity does not match selected plugin".to_owned(),
            retryable: false,
        });
    }
    status
        .validate()
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::FutureVersion,
            message: bounded_message(&error.to_string()),
            retryable: false,
        })?;
    Ok(status)
}

fn maintenance_call_error(
    error: &bcode_plugin::PluginServiceCallError,
) -> SessionSearchServiceError {
    let (code, retryable) = match &error {
        bcode_plugin::PluginServiceCallError::ResponseDecode(_) => {
            (SearchErrorCode::FutureVersion, false)
        }
        bcode_plugin::PluginServiceCallError::RequestEncode(_) => {
            (SearchErrorCode::InvalidRequest, false)
        }
        bcode_plugin::PluginServiceCallError::Service { code, .. }
            if code == "confirmation_required" =>
        {
            (SearchErrorCode::InvalidRequest, false)
        }
        bcode_plugin::PluginServiceCallError::Invoke(
            bcode_plugin::PluginLoadError::ServiceInvocationTimeout { .. },
        ) => (SearchErrorCode::DeadlineExceeded, true),
        bcode_plugin::PluginServiceCallError::Service { .. }
        | bcode_plugin::PluginServiceCallError::Invoke(_) => {
            (SearchErrorCode::ProviderUnavailable, true)
        }
    };
    SessionSearchServiceError {
        code,
        message: bounded_message(&error.to_string()),
        retryable,
    }
}

pub(crate) async fn remove_session_from_providers(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    expected_generation_fingerprint: Option<String>,
) {
    if !state.session_search_enabled {
        return;
    }
    let provider_ids = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(SESSION_SEARCH_INTERFACE_ID)
        .cloned()
        .unwrap_or_default();
    for provider_id in provider_ids {
        let payload = match serde_json::to_vec(&RemoveSessionSearchRequest {
            session_id,
            expected_generation_fingerprint: expected_generation_fingerprint.clone(),
        }) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(
                    target: "bcode_server::session_search",
                    %session_id,
                    provider_id,
                    error = %bounded_message(&error.to_string()),
                    "canonical session was deleted but provider cleanup request could not encode"
                );
                continue;
            }
        };
        let result = state
            .plugins
            .invoke_service(
                &provider_id,
                SESSION_SEARCH_INTERFACE_ID,
                OP_REMOVE_SESSION,
                payload,
            )
            .await;
        match result {
            Ok(response) if response.error.is_none() => state
                .metrics
                .increment_counter("server.session_search.remove_session_completed_total"),
            Ok(response) => {
                state
                    .metrics
                    .increment_counter("server.session_search.remove_session_failed_total");
                let error = response.error.map_or_else(
                    || "unknown provider error".to_owned(),
                    |error| format!("{}: {}", error.code, error.message),
                );
                tracing::warn!(
                    target: "bcode_server::session_search",
                    %session_id,
                    provider_id,
                    error = %bounded_message(&error),
                    "canonical session was deleted but provider cleanup was rejected"
                );
            }
            Err(error) => {
                state
                    .metrics
                    .increment_counter("server.session_search.remove_session_failed_total");
                tracing::warn!(
                    target: "bcode_server::session_search",
                    %session_id,
                    provider_id,
                    error = %bounded_message(&error.to_string()),
                    "canonical session was deleted but provider cleanup failed"
                );
            }
        }
    }
}

pub(crate) fn generation_fingerprint(summary: &bcode_session_models::SessionSummary) -> String {
    canonical_generation_fingerprint(summary)
}

#[derive(Debug, Default)]
struct DirtySessionSearchState {
    sessions: BTreeSet<bcode_session_models::SessionId>,
    rescan_required: bool,
}

/// Bounded, coalescing handoff from committed canonical mutations to asynchronous search work.
#[derive(Debug, Default)]
pub(crate) struct SessionSearchDirtyQueue {
    state: Mutex<DirtySessionSearchState>,
    notify: Notify,
}

impl SessionSearchDirtyQueue {
    pub(crate) async fn mark_committed(&self, session_id: bcode_session_models::SessionId) {
        let mut state = self.state.lock().await;
        if state.sessions.contains(&session_id) {
            return;
        }
        if state.sessions.len() >= MAX_DIRTY_SESSION_SEARCH_SESSIONS {
            state.rescan_required = true;
            return;
        }
        state.sessions.insert(session_id);
        drop(state);
        self.notify.notify_one();
    }

    pub(crate) async fn mark_rescan_required(&self) {
        self.state.lock().await.rescan_required = true;
    }

    async fn take(&self) -> Vec<bcode_session_models::SessionId> {
        let mut state = self.state.lock().await;
        std::mem::take(&mut state.sessions).into_iter().collect()
    }

    pub(crate) async fn notified(&self) {
        self.notify.notified().await;
    }

    #[cfg(test)]
    async fn snapshot(&self) -> (Vec<bcode_session_models::SessionId>, bool) {
        let state = self.state.lock().await;
        (
            state.sessions.iter().copied().collect(),
            state.rescan_required,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngestionErrorKind {
    Retryable,
    Permanent,
    Deadline,
}

#[derive(Debug)]
struct IngestionError {
    message: String,
    kind: IngestionErrorKind,
}

impl IngestionError {
    fn retryable(error: impl std::fmt::Display) -> Self {
        Self {
            message: error.to_string(),
            kind: IngestionErrorKind::Retryable,
        }
    }

    fn permanent(error: impl std::fmt::Display) -> Self {
        Self {
            message: error.to_string(),
            kind: IngestionErrorKind::Permanent,
        }
    }

    fn deadline(error: impl std::fmt::Display) -> Self {
        Self {
            message: error.to_string(),
            kind: IngestionErrorKind::Deadline,
        }
    }

    const fn retryable_error(&self) -> bool {
        matches!(
            self.kind,
            IngestionErrorKind::Retryable | IngestionErrorKind::Deadline
        )
    }

    const fn deadline_reached(&self) -> bool {
        matches!(self.kind, IngestionErrorKind::Deadline)
    }
}

impl std::fmt::Display for IngestionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn classify_ingestion_call_error(error: bcode_plugin::PluginServiceCallError) -> IngestionError {
    match &error {
        bcode_plugin::PluginServiceCallError::Invoke(
            bcode_plugin::PluginLoadError::ServiceInvocationTimeout { .. },
        ) => IngestionError::deadline(error),
        bcode_plugin::PluginServiceCallError::Service { code, .. }
            if matches!(
                code.as_str(),
                "stale_generation"
                    | "checkpoint_conflict"
                    | "quota_exceeded"
                    | "content_disabled"
                    | "invalid_request"
            ) =>
        {
            IngestionError::permanent(error)
        }
        bcode_plugin::PluginServiceCallError::RequestEncode(_)
        | bcode_plugin::PluginServiceCallError::ResponseDecode(_) => {
            IngestionError::permanent(error)
        }
        bcode_plugin::PluginServiceCallError::Service { .. }
        | bcode_plugin::PluginServiceCallError::Invoke(_) => IngestionError::retryable(error),
    }
}

pub(crate) async fn process_dirty_sessions(state: &ServerState) {
    let session_ids = state.session_search_dirty.take().await;
    state.metrics.record_histogram(
        "server.session_search.dirty_batch_sessions",
        u64::try_from(session_ids.len()).unwrap_or(u64::MAX),
    );
    for session_id in session_ids {
        let started = Instant::now();
        match ingest_session_tail(state, session_id).await {
            Ok(()) => {
                state
                    .metrics
                    .increment_counter("server.session_search.ingestion_session_completed_total");
            }
            Err(error) => {
                state
                    .metrics
                    .increment_counter("server.session_search.ingestion_session_failed_total");
                tracing::warn!(
                    target: "bcode_server::session_search",
                    %session_id,
                    retryable = error.retryable_error(),
                    error = %bounded_message(&error.to_string()),
                    "asynchronous session-search ingestion failed"
                );
                if error.retryable_error() {
                    tokio::time::sleep(INCREMENTAL_RETRY_DELAY).await;
                    state.session_search_dirty.mark_committed(session_id).await;
                    state.metrics.increment_counter(
                        "server.session_search.ingestion_session_requeued_total",
                    );
                } else {
                    state.metrics.increment_counter(
                        "server.session_search.ingestion_session_terminal_failed_total",
                    );
                }
            }
        }
        state.metrics.record_histogram(
            "server.session_search.ingestion_session_duration_ms",
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
    }
}

#[allow(clippy::too_many_lines)]
async fn ingest_session_tail(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
) -> Result<(), IngestionError> {
    let inventory = list_providers(state).await;
    let providers = inventory
        .providers
        .into_iter()
        .filter(|provider| {
            provider
                .capabilities
                .features
                .contains(&SearchFeature::IncrementalIngestion)
        })
        .collect::<Vec<_>>();
    if providers.is_empty() {
        return Ok(());
    }
    let summary = state
        .sessions
        .session_summary(session_id)
        .await
        .map_err(IngestionError::retryable)?;
    match state.sessions.session_health(session_id).await {
        bcode_session::SessionHealth::Ready
        | bcode_session::SessionHealth::DegradedReadOnly { .. } => {}
        bcode_session::SessionHealth::Migratable { source, target }
        | bcode_session::SessionHealth::BlockedOwner { source, target, .. } => {
            return Err(IngestionError::permanent(format!(
                "canonical session writer epoch {source} requires explicit migration to {target} before search ingestion"
            )));
        }
        bcode_session::SessionHealth::WriterIncompatible { actual, expected } => {
            return Err(IngestionError::permanent(format!(
                "canonical session writer epoch {actual:?} is incompatible with expected epoch {expected}; upgrade Bcode before search ingestion"
            )));
        }
        bcode_session::SessionHealth::NotFound => {
            return Err(IngestionError::permanent(
                "canonical session is missing; locate or restore it before search ingestion",
            ));
        }
        bcode_session::SessionHealth::ProjectionStale { .. }
        | bcode_session::SessionHealth::RepairRequired { .. } => {
            return Err(IngestionError::permanent(
                "canonical session requires explicit repair or reindex before search ingestion",
            ));
        }
    }
    for provider in providers {
        state
            .session_search_work
            .run_ordinary(ingest_provider_pages(
                state,
                session_id,
                &summary,
                &provider,
                MAX_INCREMENTAL_BATCHES_PER_SESSION,
                None,
                true,
                None,
            ))
            .await?;
    }
    Ok(())
}

#[derive(Debug)]
struct ProviderIngestionProgress {
    batches_applied: usize,
    indexed_through_sequence: Option<u64>,
    canonical_tail_sequence: Option<u64>,
    complete: bool,
}

#[allow(clippy::too_many_arguments)]
async fn ingest_provider_pages(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    summary: &bcode_session_models::SessionSummary,
    provider: &SessionSearchProviderInfo,
    max_batches: usize,
    deadline: Option<Instant>,
    requeue_incomplete: bool,
    cancellation: Option<&bcode_plugin_sdk::ServiceCancellation>,
) -> Result<ProviderIngestionProgress, IngestionError> {
    if state.session_migrations.is_active(session_id).await {
        return Err(IngestionError::permanent(
            "canonical session migration is active; retry search ingestion after migration reaches a terminal outcome",
        ));
    }
    let checkpoint = provider
        .status
        .coverage
        .iter()
        .find(|coverage| coverage.generation.session_id == session_id);
    let generation = canonical_generation_fingerprint(summary);
    if checkpoint.is_some_and(|coverage| coverage.generation.fingerprint != generation) {
        return Err(IngestionError::permanent(
            "provider checkpoint generation differs from canonical session identity; explicit rebuild is required",
        ));
    }
    let canonical_tail_before_ingestion = canonical_session_tail(state, session_id).await?;
    if checkpoint.is_some_and(|coverage| {
        coverage.indexed_through_sequence.is_some_and(|indexed| {
            canonical_tail_before_ingestion.is_none_or(|canonical_tail| indexed > canonical_tail)
        })
    }) {
        return Err(IngestionError::permanent(
            "provider checkpoint is ahead of the canonical session tail; explicit rebuild is required after canonical repair or truncation",
        ));
    }
    let mut previous_sequence = checkpoint.and_then(|coverage| coverage.indexed_through_sequence);
    let mut previous_text_bytes = checkpoint.map_or(0, |coverage| coverage.indexed_text_bytes);
    let mut batches_applied = 0;
    for _ in 0..max_batches {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let Some((indexed_through_sequence, indexed_text_bytes)) = ingest_provider_page(
            state,
            session_id,
            summary,
            provider,
            &generation,
            previous_sequence,
            previous_text_bytes,
            deadline,
            cancellation,
        )
        .await?
        else {
            let canonical_tail_sequence = canonical_session_tail(state, session_id).await?;
            return Ok(ProviderIngestionProgress {
                batches_applied,
                indexed_through_sequence: previous_sequence,
                canonical_tail_sequence,
                complete: previous_sequence == canonical_tail_sequence,
            });
        };
        batches_applied += 1;
        previous_sequence = Some(indexed_through_sequence);
        previous_text_bytes = indexed_text_bytes;
    }
    let canonical_tail_sequence = canonical_session_tail(state, session_id).await?;
    let complete = previous_sequence == canonical_tail_sequence;
    if !complete && requeue_incomplete {
        state.session_search_dirty.mark_committed(session_id).await;
        state
            .metrics
            .increment_counter("server.session_search.ingestion_slice_requeued_total");
    }
    Ok(ProviderIngestionProgress {
        batches_applied,
        indexed_through_sequence: previous_sequence,
        canonical_tail_sequence,
        complete,
    })
}

async fn canonical_session_tail(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
) -> Result<Option<u64>, IngestionError> {
    Ok(state
        .sessions
        .session_history_page(
            session_id,
            bcode_session_models::SessionHistoryQuery {
                cursor: None,
                limit: 1,
                direction: bcode_session_models::SessionHistoryDirection::Backward,
            },
        )
        .await
        .map_err(IngestionError::retryable)?
        .events
        .last()
        .map(|event| event.sequence))
}

fn canonical_generation_fingerprint(summary: &bcode_session_models::SessionSummary) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bcode-session-search-generation-v1\0");
    digest.update(summary.id.to_string().as_bytes());
    digest.update(summary.created_at_ms.to_le_bytes());
    digest.update(bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH.to_le_bytes());
    digest.update(bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION.to_le_bytes());
    digest.update(bcode_session_search::CURRENT_SEARCH_RECORD_VERSION.to_le_bytes());
    digest.update(bcode_session_search::CURRENT_NORMALIZATION_VERSION.to_le_bytes());
    digest.update(bcode_session_search::CURRENT_SEARCH_POLICY_VERSION.to_le_bytes());
    if let Some(import) = &summary.import {
        digest.update(b"import\0");
        digest.update(import.source_id.as_bytes());
        digest.update(b"\0");
        digest.update(import.external_session_id.as_bytes());
        digest.update(import.imported_at_ms.to_le_bytes());
    } else {
        digest.update(b"native\0");
    }
    format!("{:x}", digest.finalize())
}

fn apply_session_summary_attributes(
    mut record: bcode_session_search::SessionSearchRecord,
    summary: &bcode_session_models::SessionSummary,
) -> bcode_session_search::SessionSearchRecord {
    record.attributes.insert(
        "working_directory".to_owned(),
        summary.working_directory.to_string_lossy().into_owned(),
    );
    if let Some(import) = &summary.import {
        record
            .attributes
            .insert("source".to_owned(), import.source_id.clone());
    }
    record
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn ingest_provider_page(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    summary: &bcode_session_models::SessionSummary,
    provider: &SessionSearchProviderInfo,
    generation: &str,
    previous_sequence: Option<u64>,
    previous_text_bytes: u64,
    deadline: Option<Instant>,
    cancellation: Option<&bcode_plugin_sdk::ServiceCancellation>,
) -> Result<Option<(u64, u64)>, IngestionError> {
    let page = state
        .sessions
        .session_history_page(
            session_id,
            bcode_session_models::SessionHistoryQuery {
                cursor: Some(bcode_session_models::SessionHistoryCursor {
                    sequence: previous_sequence.map_or(0, |sequence| sequence.saturating_add(1)),
                }),
                limit: provider.capabilities.max_batch_records,
                direction: bcode_session_models::SessionHistoryDirection::Forward,
            },
        )
        .await
        .map_err(IngestionError::retryable)?;
    state.metrics.record_histogram(
        "server.session_search.ingestion_page_events",
        u64::try_from(page.events.len()).unwrap_or(u64::MAX),
    );
    if page.events.is_empty() {
        return Ok(None);
    }
    let canonical_tail = state
        .sessions
        .session_history_page(
            session_id,
            bcode_session_models::SessionHistoryQuery {
                cursor: None,
                limit: 1,
                direction: bcode_session_models::SessionHistoryDirection::Backward,
            },
        )
        .await
        .map_err(IngestionError::retryable)?
        .events
        .last()
        .map(|event| event.sequence);
    let generation = generation.to_owned();
    let projection_policy =
        bcode_session_search::projection::SearchProjectionPolicy::for_content_kinds(
            &provider.capabilities.content_kinds,
        );
    let mut records = Vec::new();
    let mut batch_text_bytes = 0_usize;
    for event in &page.events {
        let projected =
            match bcode_session_search::projection::project_event(event, &projection_policy)
                .map_err(IngestionError::retryable)?
            {
                bcode_session_search::projection::EventProjection::Records(projected) => projected
                    .into_iter()
                    .filter(|record| {
                        provider
                            .capabilities
                            .content_kinds
                            .contains(&record.content_kind)
                    })
                    .map(|record| apply_session_summary_attributes(record, summary))
                    .collect::<Vec<_>>(),
                bcode_session_search::projection::EventProjection::Excluded(_) => continue,
            };
        let projected_text_bytes = projected.iter().fold(0_usize, |total, record| {
            total.saturating_add(usize::try_from(record.indexed_bytes).unwrap_or(usize::MAX))
        });
        if records.len().saturating_add(projected.len()) > provider.capabilities.max_batch_records
            || batch_text_bytes.saturating_add(projected_text_bytes)
                > provider.capabilities.max_batch_text_bytes
        {
            break;
        }
        records.extend(projected);
        batch_text_bytes = batch_text_bytes.saturating_add(projected_text_bytes);
    }
    if records.is_empty() {
        let Some(indexed_through_sequence) = page.events.last().map(|event| event.sequence) else {
            return Ok(None);
        };
        let request = ApplySearchRecordsRequest {
            provider_id: provider.plugin_id.clone(),
            batch_id: format!(
                "{session_id}:{}:{indexed_through_sequence}",
                previous_sequence.map_or(0, |sequence| sequence.saturating_add(1))
            ),
            generation: SearchCanonicalGeneration {
                session_id,
                fingerprint: generation.clone(),
                last_sequence: canonical_tail,
            },
            expected_previous_sequence: previous_sequence,
            expected_previous_session_text_bytes: previous_text_bytes,
            indexed_through_sequence: Some(indexed_through_sequence),
            records,
        };
        request.validate().map_err(IngestionError::permanent)?;
        invoke_apply_batch(state, provider, &request, deadline, cancellation).await?;
        return Ok(Some((indexed_through_sequence, previous_text_bytes)));
    }
    let last_sequence = records.last().map(|record| record.locator.sequence);
    let request = ApplySearchRecordsRequest {
        provider_id: provider.plugin_id.clone(),
        batch_id: format!(
            "{session_id}:{}:{}",
            previous_sequence.map_or(0, |sequence| sequence.saturating_add(1)),
            last_sequence.unwrap_or(0)
        ),
        generation: SearchCanonicalGeneration {
            session_id,
            fingerprint: generation,
            last_sequence: canonical_tail,
        },
        expected_previous_sequence: previous_sequence,
        expected_previous_session_text_bytes: previous_text_bytes,
        indexed_through_sequence: page.events.last().map(|event| event.sequence),
        records,
    };
    request.validate().map_err(IngestionError::permanent)?;
    let indexed_through_sequence = request.indexed_through_sequence.unwrap_or(0);
    state.metrics.record_histogram(
        "server.session_search.ingestion_batch_records",
        u64::try_from(request.records.len()).unwrap_or(u64::MAX),
    );
    state.metrics.record_histogram(
        "server.session_search.ingestion_batch_text_bytes",
        u64::try_from(batch_text_bytes).unwrap_or(u64::MAX),
    );
    let indexed_text_bytes =
        previous_text_bytes.saturating_add(request.records.iter().fold(0_u64, |total, record| {
            total.saturating_add(record.normalized_bytes)
        }));
    invoke_apply_batch(state, provider, &request, deadline, cancellation).await?;
    Ok(Some((indexed_through_sequence, indexed_text_bytes)))
}

async fn invoke_apply_batch(
    state: &ServerState,
    provider: &SessionSearchProviderInfo,
    request: &ApplySearchRecordsRequest,
    deadline: Option<Instant>,
    cancellation: Option<&bcode_plugin_sdk::ServiceCancellation>,
) -> Result<ApplySearchRecordsResponse, IngestionError> {
    let response = if let Some(deadline) = deadline {
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            return Err(IngestionError::deadline(
                "historical backfill deadline reached",
            ));
        };
        if let Some(cancellation) = cancellation {
            state
                .plugins
                .invoke_service_json_scoped_cancellable(
                    &provider.plugin_id,
                    SESSION_SEARCH_INTERFACE_ID,
                    OP_APPLY_BATCH,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                    timeout,
                    cancellation,
                )
                .await
                .map_err(classify_ingestion_call_error)?
        } else {
            state
                .plugins
                .invoke_service_json_scoped_with_timeout(
                    &provider.plugin_id,
                    SESSION_SEARCH_INTERFACE_ID,
                    OP_APPLY_BATCH,
                    request,
                    bcode_plugin::PluginInvocationScope::Global,
                    timeout,
                )
                .await
                .map_err(classify_ingestion_call_error)?
        }
    } else {
        state
            .plugins
            .invoke_service_json(
                &provider.plugin_id,
                SESSION_SEARCH_INTERFACE_ID,
                OP_APPLY_BATCH,
                request,
            )
            .await
            .map_err(classify_ingestion_call_error)?
    };
    validate_apply_batch_response(request, response)
}

fn validate_apply_batch_response(
    request: &ApplySearchRecordsRequest,
    response: ApplySearchRecordsResponse,
) -> Result<ApplySearchRecordsResponse, IngestionError> {
    let expected_sequence = request.indexed_through_sequence.unwrap_or_else(|| {
        request
            .records
            .last()
            .map_or(0, |record| record.locator.sequence)
    });
    if response.batch_id != request.batch_id
        || response.indexed_through_sequence != expected_sequence
    {
        return Err(IngestionError::permanent(
            "provider apply-batch acknowledgment does not match the requested batch",
        ));
    }
    match response.outcome {
        bcode_session_search::ApplyBatchOutcome::Applied
            if response.applied_records == request.records.len() => {}
        bcode_session_search::ApplyBatchOutcome::Duplicate if response.applied_records == 0 => {}
        bcode_session_search::ApplyBatchOutcome::ConflictingDuplicate => {
            return Err(IngestionError::permanent(
                "provider rejected a conflicting duplicate batch identity",
            ));
        }
        bcode_session_search::ApplyBatchOutcome::Applied
        | bcode_session_search::ApplyBatchOutcome::Duplicate => {
            return Err(IngestionError::permanent(
                "provider apply-batch acknowledgment has inconsistent record accounting",
            ));
        }
    }
    Ok(response)
}

/// Discover loaded session-search providers and query their typed capabilities/status.
///
/// Provider-local failures are returned as normalized entries so one malformed or unavailable
/// provider does not conceal healthy providers.
#[allow(clippy::too_many_lines)]
pub async fn list_providers(state: &ServerState) -> ListSessionSearchProvidersResponse {
    if !state.session_search_enabled {
        return ListSessionSearchProvidersResponse {
            providers: Vec::new(),
            failures: Vec::new(),
        };
    }
    let provider_ids = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(SESSION_SEARCH_INTERFACE_ID)
        .cloned()
        .unwrap_or_default();
    let mut providers = Vec::new();
    let mut failures = Vec::new();
    for plugin_id in provider_ids.iter().cloned() {
        let capabilities = state
            .plugins
            .invoke_service_json::<_, SessionSearchCapabilities>(
                &plugin_id,
                SESSION_SEARCH_INTERFACE_ID,
                OP_CAPABILITIES,
                &(),
            )
            .await;
        let capabilities = match capabilities {
            Ok(capabilities) if capabilities.provider_id == plugin_id => {
                if let Err(error) = capabilities.validate() {
                    failures.push(provider_failure(
                        plugin_id,
                        SearchErrorCode::InvalidRequest,
                        &error.to_string(),
                        false,
                    ));
                    continue;
                }
                capabilities
            }
            Ok(_) => {
                failures.push(provider_failure(
                    plugin_id,
                    SearchErrorCode::InvalidRequest,
                    "provider capability identity does not match plugin registration",
                    false,
                ));
                continue;
            }
            Err(error) => {
                let (code, retryable) = classify_provider_call_error(&error);
                failures.push(provider_failure(
                    plugin_id,
                    code,
                    &error.to_string(),
                    retryable,
                ));
                continue;
            }
        };
        let status = state
            .plugins
            .invoke_service_json::<_, SessionSearchStatus>(
                &plugin_id,
                SESSION_SEARCH_INTERFACE_ID,
                OP_STATUS,
                &(),
            )
            .await;
        match status {
            Ok(status) if status.provider_id == plugin_id => {
                if let Err(error) = status.validate() {
                    failures.push(provider_failure(
                        plugin_id,
                        SearchErrorCode::FutureVersion,
                        &error.to_string(),
                        false,
                    ));
                    continue;
                }
                providers.push(SessionSearchProviderInfo {
                    plugin_id,
                    capabilities,
                    status,
                });
            }
            Ok(_) => failures.push(provider_failure(
                plugin_id,
                SearchErrorCode::InvalidRequest,
                "provider status identity does not match plugin registration",
                false,
            )),
            Err(error) => {
                let (code, retryable) = classify_provider_call_error(&error);
                failures.push(provider_failure(
                    plugin_id,
                    code,
                    &error.to_string(),
                    retryable,
                ));
            }
        }
    }
    for plugin_id in &state.plugins.selection().disabled {
        if !provider_ids.contains(plugin_id)
            && looks_like_session_search_provider(plugin_id, state.plugins.configs().get(plugin_id))
        {
            failures.push(provider_failure(
                plugin_id.clone(),
                SearchErrorCode::ProviderUnavailable,
                "session-search provider is explicitly disabled by configuration",
                false,
            ));
        }
    }
    for plugin_id in &state.plugins.selection().enabled {
        if !provider_ids.contains(plugin_id)
            && !state.plugins.plugin_ids().contains(plugin_id)
            && looks_like_session_search_provider(plugin_id, state.plugins.configs().get(plugin_id))
        {
            failures.push(provider_failure(
                plugin_id.clone(),
                SearchErrorCode::ProviderUnavailable,
                "configured session-search provider is unavailable or failed to load",
                true,
            ));
        }
    }
    failures.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    ListSessionSearchProvidersResponse {
        providers,
        failures,
    }
}

const fn classify_provider_call_error(
    error: &bcode_plugin::PluginServiceCallError,
) -> (SearchErrorCode, bool) {
    match error {
        bcode_plugin::PluginServiceCallError::ResponseDecode(_) => {
            (SearchErrorCode::FutureVersion, false)
        }
        bcode_plugin::PluginServiceCallError::RequestEncode(_) => {
            (SearchErrorCode::InvalidRequest, false)
        }
        bcode_plugin::PluginServiceCallError::Service { .. }
        | bcode_plugin::PluginServiceCallError::Invoke(_) => {
            (SearchErrorCode::ProviderUnavailable, true)
        }
    }
}

fn looks_like_session_search_provider(
    plugin_id: &str,
    config: Option<&bcode_plugin::ResolvedPluginConfig>,
) -> bool {
    plugin_id.contains("session-search")
        || config.is_some_and(|config| {
            config
                .config
                .get("session_search_provider")
                .is_some_and(|value| matches!(value, serde_json::Value::Bool(true)))
        })
}

/// Invoke one exact provider through the backend-neutral search contract.
///
/// # Errors
///
/// Returns a normalized service error when validation fails, the selected plugin is not a loaded
/// session-search provider, invocation fails, or the response identity/bounds are invalid.
pub async fn search_provider(
    state: &ServerState,
    plugin_id: &str,
    request: &SessionSearchRequest,
) -> Result<SessionSearchResponse, SessionSearchServiceError> {
    search_provider_with_timeout(state, plugin_id, request, Duration::from_secs(30)).await
}

async fn search_provider_with_timeout(
    state: &ServerState,
    plugin_id: &str,
    request: &SessionSearchRequest,
    timeout: Duration,
) -> Result<SessionSearchResponse, SessionSearchServiceError> {
    if !state.session_search_enabled {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: "session search is globally disabled".to_owned(),
            retryable: false,
        });
    }
    request
        .validate()
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: error.to_string(),
            retryable: false,
        })?;
    let registered = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(SESSION_SEARCH_INTERFACE_ID)
        .is_some_and(|providers| providers.contains(plugin_id));
    if !registered {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: format!("session-search provider '{plugin_id}' is not loaded"),
            retryable: false,
        });
    }
    let capabilities = state
        .plugins
        .invoke_service_json::<_, SessionSearchCapabilities>(
            plugin_id,
            SESSION_SEARCH_INTERFACE_ID,
            OP_CAPABILITIES,
            &(),
        )
        .await
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: bounded_message(&error.to_string()),
            retryable: true,
        })?;
    if capabilities.provider_id != plugin_id {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider capability identity does not match selected plugin".to_owned(),
            retryable: false,
        });
    }
    capabilities
        .supports_request(request)
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::UnsupportedQuery,
            message: bounded_message(&error.to_string()),
            retryable: false,
        })?;
    let payload = serde_json::to_vec(request).map_err(|error| SessionSearchServiceError {
        code: SearchErrorCode::InvalidRequest,
        message: bounded_message(&error.to_string()),
        retryable: false,
    })?;
    let response = state
        .plugins
        .invoke_service_scoped_with_timeout(
            plugin_id,
            SESSION_SEARCH_INTERFACE_ID,
            OP_SEARCH,
            payload,
            bcode_plugin::PluginInvocationScope::Global,
            timeout,
        )
        .await
        .map_err(|error| {
            let deadline = matches!(
                error,
                bcode_plugin::PluginLoadError::ServiceInvocationTimeout { .. }
            );
            SessionSearchServiceError {
                code: if deadline {
                    SearchErrorCode::DeadlineExceeded
                } else {
                    SearchErrorCode::ProviderUnavailable
                },
                message: bounded_message(&error.to_string()),
                retryable: true,
            }
        })?;
    let response = bcode_plugin::decode_service_response::<SessionSearchResponse>(response)
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: bounded_message(&error.to_string()),
            retryable: true,
        })?;
    validate_provider_response(plugin_id, request, &response)?;
    Ok(response)
}

fn validate_provider_response(
    plugin_id: &str,
    request: &SessionSearchRequest,
    response: &SessionSearchResponse,
) -> Result<(), SessionSearchServiceError> {
    if response.provider_id != plugin_id {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider response identity does not match selected plugin".to_owned(),
            retryable: false,
        });
    }
    if response.hits.len() > request.limit {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: format!(
                "provider returned {} hits for requested limit {}",
                response.hits.len(),
                request.limit
            ),
            retryable: false,
        });
    }
    validate_provider_hits(plugin_id, response)?;
    validate_provider_diagnostics_and_cursor(plugin_id, response)?;
    Ok(())
}

fn validate_provider_hits(
    plugin_id: &str,
    response: &SessionSearchResponse,
) -> Result<(), SessionSearchServiceError> {
    if response.hits.iter().any(|hit| hit.provider_id != plugin_id) {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider returned a hit owned by another provider".to_owned(),
            retryable: false,
        });
    }
    if response.hits.iter().any(|hit| {
        hit.preview
            .as_ref()
            .is_some_and(|preview| preview.len() > bcode_session_search::MAX_HIT_PREVIEW_BYTES)
    }) {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider returned an oversized hit preview".to_owned(),
            retryable: false,
        });
    }
    for (index, hit) in response.hits.iter().enumerate() {
        if hit.provider_rank == 0
            || usize::try_from(hit.provider_rank).map_or(true, |rank| rank > response.hits.len())
        {
            return Err(SessionSearchServiceError {
                code: SearchErrorCode::InvalidRequest,
                message: "provider returned an invalid hit rank".to_owned(),
                retryable: false,
            });
        }
        if index > 0 && hit.provider_rank <= response.hits[index - 1].provider_rank {
            return Err(SessionSearchServiceError {
                code: SearchErrorCode::InvalidRequest,
                message: "provider hit ranks are not strictly increasing".to_owned(),
                retryable: false,
            });
        }
        if hit.provider_score.as_ref().is_some_and(|score| {
            score.is_empty() || score.len() > bcode_session_search::MAX_CURSOR_BYTES
        }) {
            return Err(SessionSearchServiceError {
                code: SearchErrorCode::InvalidRequest,
                message: "provider returned an invalid opaque score".to_owned(),
                retryable: false,
            });
        }
        if hit.locator.record_id.as_ref().is_some_and(|record_id| {
            record_id.is_empty() || record_id.len() > bcode_session_search::MAX_CURSOR_BYTES
        }) {
            return Err(SessionSearchServiceError {
                code: SearchErrorCode::InvalidRequest,
                message: "provider returned an invalid record identity".to_owned(),
                retryable: false,
            });
        }
    }
    Ok(())
}

fn validate_provider_diagnostics_and_cursor(
    plugin_id: &str,
    response: &SessionSearchResponse,
) -> Result<(), SessionSearchServiceError> {
    if response
        .message
        .as_ref()
        .is_some_and(|message| message.len() > bcode_session_search::MAX_HIT_PREVIEW_BYTES)
    {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider returned an oversized diagnostic message".to_owned(),
            retryable: false,
        });
    }
    if response
        .next_cursor
        .as_ref()
        .is_some_and(|cursor| cursor.provider_id != plugin_id)
    {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider returned a cursor owned by another provider".to_owned(),
            retryable: false,
        });
    }
    if response.next_cursor.as_ref().is_some_and(|cursor| {
        cursor.query_fingerprint.is_empty()
            || cursor.query_fingerprint.len() > bcode_session_search::MAX_CURSOR_BYTES
            || cursor.value.is_empty()
            || cursor.value.len() > bcode_session_search::MAX_CURSOR_BYTES
    }) {
        return Err(SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: "provider returned an invalid cursor payload".to_owned(),
            retryable: false,
        });
    }
    Ok(())
}

/// Execute one bounded terminal federated search using deterministic grouped provider ordering.
///
/// Canonical hit hydration is intentionally separate: this function returns provider locators and
/// bounded previews only. Per-provider timeouts propagate cancellation through the generic plugin
/// runtime and ABI. This terminal aggregate does not claim durable resume semantics.
///
/// # Errors
///
/// Returns an invalid-request error when the portable request is invalid. Provider-specific
/// failures are retained in the successful aggregate response.
pub async fn search_federated(
    state: &ServerState,
    request: &SessionSearchRequest,
) -> Result<FederatedSessionSearchResponse, SessionSearchServiceError> {
    search_federated_with_routes(state, request, &[]).await
}

/// Execute a bounded terminal federated search using explicit backend-neutral content routes.
///
/// # Errors
///
/// Returns an invalid-request error when the request is invalid; provider failures remain in the
/// terminal aggregate.
pub async fn search_federated_with_routes(
    state: &ServerState,
    request: &SessionSearchRequest,
    routes: &[SessionSearchContentRoute],
) -> Result<FederatedSessionSearchResponse, SessionSearchServiceError> {
    search_federated_with_policy_and_routes(
        state,
        request,
        &bcode_session_search::SessionSearchPlanPolicy::default(),
        routes,
    )
    .await
}

/// Execute a bounded terminal federated search with explicit execution/freshness policy.
///
/// # Errors
///
/// Returns an invalid-request error when request or plan policy validation fails; provider failures
/// remain in the terminal aggregate.
pub async fn search_federated_with_policy_and_routes(
    state: &ServerState,
    request: &SessionSearchRequest,
    policy: &bcode_session_search::SessionSearchPlanPolicy,
    routes: &[SessionSearchContentRoute],
) -> Result<FederatedSessionSearchResponse, SessionSearchServiceError> {
    request
        .validate()
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: bounded_message(&error.to_string()),
            retryable: false,
        })?;
    let discovery = list_providers(state).await;
    let plan = plan_session_search_with_policy_and_routes(request, discovery, policy, routes);
    let per_provider_deadline = Duration::from_millis(plan.per_provider_deadline_ms.max(1));
    let mut failures = plan.failures;
    let mut providers = plan.providers;
    if providers.len() > MAX_FEDERATED_PROVIDERS {
        for provider in providers.drain(MAX_FEDERATED_PROVIDERS..) {
            failures.push(provider_failure(
                provider.plugin_id,
                SearchErrorCode::QuotaExceeded,
                "federated provider concurrency limit exceeded",
                false,
            ));
        }
    }
    let deadline = Duration::from_millis(request.deadline_ms.unwrap_or(5_000).max(1));
    let started = Instant::now();
    let calls = providers.into_iter().map(|provider| {
        let provider_request = request_for_provider(request, &provider, policy);
        async move {
            let provider_started = Instant::now();
            let remaining = deadline.saturating_sub(started.elapsed());
            let call_deadline = remaining.min(per_provider_deadline);
            let result = if call_deadline.is_zero() {
                Err(timeout_error())
            } else {
                state
                    .session_search_work
                    .run_ordinary(search_provider_with_timeout(
                        state,
                        &provider.plugin_id,
                        &provider_request,
                        call_deadline,
                    ))
                    .await
            };
            (
                provider.plugin_id,
                u64::try_from(provider_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                result,
            )
        }
    });
    let mut completed = join_all(calls).await;
    completed.sort_by(|left, right| left.0.cmp(&right.0));

    let mut contributions = Vec::new();
    for (provider_id, elapsed_ms, result) in completed {
        match result {
            Ok(response) => {
                contributions.push(FederatedProviderContribution {
                    report: FederatedProviderReport {
                        provider_id,
                        outcome: response.outcome,
                        elapsed_ms,
                        query_complete: response.query_complete,
                        coverage_complete: response.coverage_complete,
                        next_cursor: response.next_cursor,
                        searched_content: response.searched_content,
                        excluded_content: response.excluded_content,
                    },
                    hits: response.hits,
                });
            }
            Err(error) => failures.push(SessionSearchProviderFailure {
                plugin_id: provider_id,
                error,
                stage: bcode_session_search::SessionSearchProviderStage::Execution,
                elapsed_ms,
                content: provider_request_content_for_failure(request),
            }),
        }
    }
    Ok(aggregate_federated_search(
        contributions,
        failures,
        request.limit,
    ))
}

pub fn report_hydration_outcomes(
    response: &mut FederatedSessionSearchResponse,
    hydrated_hits: &[HydratedSessionSearchHit],
    elapsed_ms: u64,
) {
    use bcode_session_search::SessionSearchProviderStage;

    let mut affected = std::collections::BTreeMap::<
        String,
        std::collections::BTreeSet<bcode_session_search::SearchContentKind>,
    >::new();
    for hydrated in hydrated_hits {
        if hydrated.outcome != SearchHitHydrationOutcome::Hydrated {
            affected
                .entry(hydrated.hit.provider_id.clone())
                .or_default()
                .insert(hydrated.hit.content_kind);
        }
    }
    for (provider_id, content) in affected {
        response.failures.push(SessionSearchProviderFailure {
            plugin_id: provider_id,
            error: SessionSearchServiceError {
                code: SearchErrorCode::StaleIndex,
                message: "one or more provider locators could not be hydrated canonically"
                    .to_owned(),
                retryable: false,
            },
            stage: SessionSearchProviderStage::Hydration,
            elapsed_ms,
            content: content.into_iter().collect(),
        });
    }
    if !response.failures.is_empty() {
        response.query_complete = false;
        response.coverage_complete = false;
    }
    response.failures.sort_by(|left, right| {
        left.plugin_id
            .cmp(&right.plugin_id)
            .then(left.stage.cmp(&right.stage))
    });
}

/// Hydrate provider locators through exact bounded canonical reads.
///
/// Hits are grouped by session and read through bounded inclusive sequence ranges, with gaps split
/// so sparse locators cannot widen one read into unbounded canonical work. A missing exact sequence
/// is stale and never substituted with another event. The returned vector preserves grouped hit
/// order.
pub async fn hydrate_hits(
    state: &ServerState,
    hits: Vec<bcode_session_search::SessionSearchHit>,
) -> Vec<HydratedSessionSearchHit> {
    const MAX_HYDRATION_RANGE_SPAN: u64 = 64;
    let mut hydrated = vec![None; hits.len()];
    let mut by_session = BTreeMap::<bcode_session_models::SessionId, Vec<(usize, u64)>>::new();
    for (index, hit) in hits.iter().enumerate() {
        by_session
            .entry(hit.locator.session_id)
            .or_default()
            .push((index, hit.locator.sequence));
    }
    for (session_id, mut locators) in by_session {
        locators.sort_by_key(|(_, sequence)| *sequence);
        let mut start = 0;
        while start < locators.len() {
            let range_start = locators[start].1;
            let mut end = start + 1;
            while end < locators.len()
                && locators[end].1.saturating_sub(range_start) <= MAX_HYDRATION_RANGE_SPAN
            {
                end += 1;
            }
            let range_end = locators[end - 1].1;
            match state
                .sessions
                .session_events_range(session_id, range_start, range_end, end - start)
                .await
            {
                Ok(events) => {
                    let events = events
                        .into_iter()
                        .map(|event| (event.sequence, event))
                        .collect::<BTreeMap<_, _>>();
                    for &(index, sequence) in &locators[start..end] {
                        let hit = hits[index].clone();
                        hydrated[index] = Some(if let Some(event) = events.get(&sequence) {
                            HydratedSessionSearchHit {
                                hit,
                                outcome: SearchHitHydrationOutcome::Hydrated,
                                event: Some(Box::new(event.clone())),
                                message: None,
                            }
                        } else {
                            stale_hydration(hit)
                        });
                    }
                }
                Err(error) => {
                    for &(index, _) in &locators[start..end] {
                        hydrated[index] = Some(hydration_error(hits[index].clone(), &error));
                    }
                }
            }
            start = end;
        }
    }
    hydrated
        .into_iter()
        .zip(hits)
        .map(|(hydrated, hit)| hydrated.unwrap_or_else(|| stale_hydration(hit)))
        .collect()
}

fn stale_hydration(hit: bcode_session_search::SessionSearchHit) -> HydratedSessionSearchHit {
    HydratedSessionSearchHit {
        hit,
        outcome: SearchHitHydrationOutcome::StaleLocator,
        event: None,
        message: Some("canonical event locator is no longer present".to_owned()),
    }
}

fn hydration_error(
    hit: bcode_session_search::SessionSearchHit,
    error: &bcode_session::SessionError,
) -> HydratedSessionSearchHit {
    let outcome = match error {
        bcode_session::SessionError::NotFound(_) => SearchHitHydrationOutcome::SessionMissing,
        bcode_session::SessionError::ProjectionStale { .. }
        | bcode_session::SessionError::Db(
            bcode_session::db::SessionDbError::ProjectionStale { .. }
            | bcode_session::db::SessionDbError::MigrationHistoryIncompatible { .. }
            | bcode_session::db::SessionDbError::InvalidCanonicalSequence { .. }
            | bcode_session::db::SessionDbError::InvalidCompactionMarker { .. }
            | bcode_session::db::SessionDbError::InvalidRow { .. },
        )
        | bcode_session::SessionError::DbUnavailable(_) => {
            SearchHitHydrationOutcome::RepairRequired
        }
        bcode_session::SessionError::StorageMigrationRequired { .. }
        | bcode_session::SessionError::Db(
            bcode_session::db::SessionDbError::ProjectionIncompatible { .. }
            | bcode_session::db::SessionDbError::WriterIncompatible { .. }
            | bcode_session::db::SessionDbError::PersistedEvent(
                bcode_session::persisted::PersistedSessionEventError::UnsupportedSchemaVersion {
                    ..
                }
                | bcode_session::persisted::PersistedSessionEventError::UnsupportedEventKind {
                    ..
                },
            ),
        )
        | bcode_session::SessionError::Lease(_) => SearchHitHydrationOutcome::Incompatible,
        bcode_session::SessionError::Db(_) => SearchHitHydrationOutcome::RepairRequired,
        _ => SearchHitHydrationOutcome::Unavailable,
    };
    HydratedSessionSearchHit {
        hit,
        outcome,
        event: None,
        message: Some(bounded_message(&error.to_string())),
    }
}

fn provider_request_content_for_failure(
    request: &SessionSearchRequest,
) -> Vec<bcode_session_search::SearchContentKind> {
    request.filters.content_kinds.iter().copied().collect()
}

fn request_for_provider(
    request: &SessionSearchRequest,
    provider: &SessionSearchProviderInfo,
    policy: &bcode_session_search::SessionSearchPlanPolicy,
) -> SessionSearchRequest {
    let mut provider_request = request.clone();
    if request.filters.content_kinds.is_empty() {
        provider_request.filters.content_kinds = provider
            .capabilities
            .content_kinds
            .iter()
            .copied()
            .filter(|content| {
                matches!(
                    policy.execution_class,
                    bcode_session_search::SessionSearchExecutionClass::Deep
                ) || !matches!(
                    content,
                    bcode_session_search::SearchContentKind::ShellOutput
                        | bcode_session_search::SearchContentKind::ToolOutput
                )
            })
            .collect();
    } else {
        provider_request.filters.content_kinds = request
            .filters
            .content_kinds
            .intersection(&provider.capabilities.content_kinds)
            .copied()
            .collect();
    }
    provider_request
}

fn timeout_error() -> SessionSearchServiceError {
    SessionSearchServiceError {
        code: SearchErrorCode::DeadlineExceeded,
        message: "session-search provider deadline exceeded".to_owned(),
        retryable: true,
    }
}

fn provider_failure(
    plugin_id: String,
    code: SearchErrorCode,
    message: &str,
    retryable: bool,
) -> SessionSearchProviderFailure {
    SessionSearchProviderFailure {
        plugin_id,
        error: SessionSearchServiceError {
            code,
            message: bounded_message(message),
            retryable,
        },
        stage: bcode_session_search::SessionSearchProviderStage::Discovery,
        elapsed_ms: 0,
        content: Vec::new(),
    }
}

fn bounded_message(message: &str) -> String {
    bcode_model_provider_runtime::sanitize_provider_diagnostic(message)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bcode_plugin::{PluginHost, PluginManifest};
    use bcode_plugin_sdk::{
        NativeServiceContext, ServiceCancellationWaitCallback, ServiceEventCallback,
        ServiceResponse, StaticPluginVtable,
    };
    use bcode_session_models::SessionId;
    use bcode_session_search::{
        CURRENT_NORMALIZATION_VERSION, CURRENT_SEARCH_POLICY_VERSION,
        CURRENT_SEARCH_RECORD_VERSION, ProviderSearchOutcome, SearchContentKind,
        SearchExecutionKind, SearchFeature, SearchField, SearchProviderState,
        SessionSearchCapabilities, SessionSearchContentRoute, SessionSearchHit,
        SessionSearchLocator, SessionSearchPlanPolicy, SessionSearchRouteMode, SessionSearchStatus,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use switchy::database::query::FilterableQuery as _;

    const FAST_PROVIDER_ID: &str = "test.fast-session-search";
    const SLOW_PROVIDER_ID: &str = "test.slow-session-search";
    const MALFORMED_PROVIDER_ID: &str = "test.malformed-session-search";
    const FUTURE_PROVIDER_ID: &str = "test.future-session-search";
    const FUTURE_CAPABILITY_PROVIDER_ID: &str = "test.future-capability-session-search";
    const CRASH_PROVIDER_ID: &str = "test.crash-session-search";
    const UNAVAILABLE_PROVIDER_ID: &str = "test.unavailable-session-search";
    const FAST_PROVIDER_MANIFEST: &str = concat!(
        "id = \"test.fast-session-search\"\n",
        "name = \"Session Search Integration Provider\"\n",
        "version = \"0.0.1\"\n\n",
        "[[services]]\n",
        "interface_id = \"bcode.session_search/v1\"\n",
        "name = \"Session Search Integration Provider\"\n\n",
        "[concurrency]\n",
        "type = \"concurrent\"\n\n",
        "[runtime]\n",
        "type = \"native\"\n",
        "library = \"libsession_search_integration_provider.dylib\"\n",
        "abi_version = 3\n",
        "event_symbol = \"bcode_plugin_handle_event_v1\"\n"
    );
    const SLOW_PROVIDER_MANIFEST: &str = concat!(
        "id = \"test.slow-session-search\"\n",
        "name = \"Session Search Integration Provider\"\n",
        "version = \"0.0.1\"\n\n",
        "[[services]]\n",
        "interface_id = \"bcode.session_search/v1\"\n",
        "name = \"Session Search Integration Provider\"\n\n",
        "[concurrency]\n",
        "type = \"concurrent\"\n\n",
        "[runtime]\n",
        "type = \"native\"\n",
        "library = \"libsession_search_integration_provider.dylib\"\n",
        "abi_version = 3\n",
        "event_symbol = \"bcode_plugin_handle_event_v1\"\n"
    );
    const MALFORMED_PROVIDER_MANIFEST: &str = concat!(
        "id = \"test.malformed-session-search\"\n",
        "name = \"Session Search Integration Provider\"\n",
        "version = \"0.0.1\"\n\n",
        "[[services]]\n",
        "interface_id = \"bcode.session_search/v1\"\n",
        "name = \"Session Search Integration Provider\"\n\n",
        "[concurrency]\n",
        "type = \"concurrent\"\n\n",
        "[runtime]\n",
        "type = \"native\"\n",
        "library = \"libsession_search_integration_provider.dylib\"\n",
        "abi_version = 3\n",
        "event_symbol = \"bcode_plugin_handle_event_v1\"\n"
    );
    const FAST_SEQUENCE: u64 = 11;
    static SLOW_SEARCH_STARTED: AtomicBool = AtomicBool::new(false);
    static SLOW_SEARCH_CANCELLED: AtomicBool = AtomicBool::new(false);
    static SLOW_SEARCH_FINISHED: AtomicBool = AtomicBool::new(false);
    static APPLY_BATCH_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SLOW_APPLY_STARTED: AtomicBool = AtomicBool::new(false);
    static SLOW_APPLY_CANCELLED: AtomicBool = AtomicBool::new(false);
    static REMOVE_SESSION_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SEARCH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    const FUTURE_PROVIDER_MANIFEST: &str = concat!(
        "id = \"test.future-session-search\"\n",
        "name = \"Session Search Future Provider\"\n",
        "version = \"0.0.1\"\n\n",
        "[[services]]\n",
        "interface_id = \"bcode.session_search/v1\"\n",
        "name = \"Session Search Future Provider\"\n\n",
        "[concurrency]\n",
        "type = \"concurrent\"\n\n",
        "[runtime]\n",
        "type = \"native\"\n",
        "library = \"libsession_search_future_provider.dylib\"\n",
        "abi_version = 3\n",
        "event_symbol = \"bcode_plugin_handle_event_v1\"\n"
    );
    const FUTURE_CAPABILITY_PROVIDER_MANIFEST: &str = concat!(
        "id = \"test.future-capability-session-search\"\n",
        "name = \"Session Search Future Capability Provider\"\n",
        "version = \"0.0.1\"\n\n",
        "[[services]]\n",
        "interface_id = \"bcode.session_search/v1\"\n",
        "name = \"Session Search Future Capability Provider\"\n\n",
        "[concurrency]\n",
        "type = \"concurrent\"\n\n",
        "[runtime]\n",
        "type = \"native\"\n",
        "library = \"libsession_search_future_capability_provider.dylib\"\n",
        "abi_version = 3\n",
        "event_symbol = \"bcode_plugin_handle_event_v1\"\n"
    );
    const CRASH_PROVIDER_MANIFEST: &str = concat!(
        "id = \"test.crash-session-search\"\n",
        "name = \"Session Search Crash Provider\"\n",
        "version = \"0.0.1\"\n\n",
        "[[services]]\n",
        "interface_id = \"bcode.session_search/v1\"\n",
        "name = \"Session Search Crash Provider\"\n\n",
        "[concurrency]\n",
        "type = \"concurrent\"\n\n",
        "[runtime]\n",
        "type = \"native\"\n",
        "library = \"libsession_search_crash_provider.dylib\"\n",
        "abi_version = 3\n",
        "event_symbol = \"bcode_plugin_handle_event_v1\"\n"
    );

    const UNAVAILABLE_PROVIDER_MANIFEST: &str = concat!(
        "id = \"test.unavailable-session-search\"\n",
        "name = \"Session Search Unavailable Provider\"\n",
        "version = \"0.0.1\"\n\n",
        "[[services]]\n",
        "interface_id = \"bcode.session_search/v1\"\n",
        "name = \"Session Search Unavailable Provider\"\n\n",
        "[concurrency]\n",
        "type = \"concurrent\"\n\n",
        "[runtime]\n",
        "type = \"native\"\n",
        "library = \"libsession_search_unavailable_provider.dylib\"\n",
        "abi_version = 3\n",
        "event_symbol = \"bcode_plugin_handle_event_v1\"\n"
    );

    #[derive(Debug, Clone, Copy)]
    pub enum TestProviderBehavior {
        Fast,
        Slow,
        Malformed,
        FutureStatus,
        FutureCapability,
        Crash,
        RejectCleanup,
        Stateful,
        UnavailableApply,
        QuotaApply,
        CorruptApply,
        StaleApply,
        IncompatiblePolicy,
    }

    #[derive(Debug, Clone)]
    struct StatefulProviderCheckpoint {
        generation: SearchCanonicalGeneration,
        indexed_through_sequence: u64,
        indexed_text_bytes: u64,
    }

    static STATEFUL_CHECKPOINTS: std::sync::LazyLock<
        std::sync::Mutex<BTreeMap<&'static str, StatefulProviderCheckpoint>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));
    static STATEFUL_APPLY_PUBLISHED: AtomicBool = AtomicBool::new(false);
    static STATEFUL_APPLY_RELEASE: std::sync::LazyLock<(
        std::sync::Mutex<bool>,
        std::sync::Condvar,
    )> = std::sync::LazyLock::new(|| (std::sync::Mutex::new(false), std::sync::Condvar::new()));

    #[derive(Debug)]
    struct TestProviderInstance {
        provider_id: &'static str,
        behavior: TestProviderBehavior,
    }

    fn provider_manifest(provider_id: &str) -> PluginManifest {
        toml::from_str(match provider_id {
            FAST_PROVIDER_ID => FAST_PROVIDER_MANIFEST,
            SLOW_PROVIDER_ID => SLOW_PROVIDER_MANIFEST,
            MALFORMED_PROVIDER_ID => MALFORMED_PROVIDER_MANIFEST,
            FUTURE_PROVIDER_ID => FUTURE_PROVIDER_MANIFEST,
            FUTURE_CAPABILITY_PROVIDER_ID => FUTURE_CAPABILITY_PROVIDER_MANIFEST,
            CRASH_PROVIDER_ID => CRASH_PROVIDER_MANIFEST,
            UNAVAILABLE_PROVIDER_ID => UNAVAILABLE_PROVIDER_MANIFEST,
            _ => panic!("unknown test provider {provider_id}"),
        })
        .expect("test provider manifest")
    }

    fn fast_provider_manifest_export(
        cached: &'static OnceLock<Option<std::ffi::CString>>,
    ) -> *const std::ffi::c_char {
        bcode_plugin_sdk::static_manifest_export(FAST_PROVIDER_MANIFEST, cached)
    }

    fn slow_provider_manifest_export(
        cached: &'static OnceLock<Option<std::ffi::CString>>,
    ) -> *const std::ffi::c_char {
        bcode_plugin_sdk::static_manifest_export(SLOW_PROVIDER_MANIFEST, cached)
    }

    fn malformed_provider_manifest_export(
        cached: &'static OnceLock<Option<std::ffi::CString>>,
    ) -> *const std::ffi::c_char {
        bcode_plugin_sdk::static_manifest_export(MALFORMED_PROVIDER_MANIFEST, cached)
    }

    fn future_provider_manifest_export(
        cached: &'static OnceLock<Option<std::ffi::CString>>,
    ) -> *const std::ffi::c_char {
        bcode_plugin_sdk::static_manifest_export(FUTURE_PROVIDER_MANIFEST, cached)
    }

    fn future_capability_provider_manifest_export(
        cached: &'static OnceLock<Option<std::ffi::CString>>,
    ) -> *const std::ffi::c_char {
        bcode_plugin_sdk::static_manifest_export(FUTURE_CAPABILITY_PROVIDER_MANIFEST, cached)
    }

    fn crash_provider_manifest_export(
        cached: &'static OnceLock<Option<std::ffi::CString>>,
    ) -> *const std::ffi::c_char {
        bcode_plugin_sdk::static_manifest_export(CRASH_PROVIDER_MANIFEST, cached)
    }

    fn unavailable_provider_manifest_export(
        cached: &'static OnceLock<Option<std::ffi::CString>>,
    ) -> *const std::ffi::c_char {
        bcode_plugin_sdk::static_manifest_export(UNAVAILABLE_PROVIDER_MANIFEST, cached)
    }

    fn provider_capabilities(provider_id: &str) -> SessionSearchCapabilities {
        SessionSearchCapabilities {
            provider_id: provider_id.to_owned(),
            execution: SearchExecutionKind::Indexed,
            content_kinds: BTreeSet::from([SearchContentKind::UserMessage]),
            features: BTreeSet::from([
                SearchFeature::Terms,
                SearchFeature::StructuredFilters,
                SearchFeature::RelevanceSort,
                SearchFeature::IncrementalIngestion,
                SearchFeature::HistoricalBackfill,
                SearchFeature::Rebuild,
                SearchFeature::Purge,
            ]),
            max_hits: bcode_session_search::MAX_SEARCH_HITS,
            max_batch_records: bcode_session_search::MAX_INGEST_RECORDS,
            max_batch_text_bytes: bcode_session_search::MAX_INGEST_TEXT_BYTES,
        }
    }

    fn provider_status(provider_id: &str) -> SessionSearchStatus {
        SessionSearchStatus {
            provider_id: provider_id.to_owned(),
            state: SearchProviderState::Ready,
            record_schema_version: CURRENT_SEARCH_RECORD_VERSION,
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
            index_bytes: 0,
            quota_bytes: 1,
            document_count: 0,
            pending_sessions: 0,
            coverage: Vec::new(),
            degraded_reason: None,
        }
    }

    fn provider_search_response(
        provider_id: &str,
        request: &SessionSearchRequest,
    ) -> SessionSearchResponse {
        SessionSearchResponse {
            provider_id: provider_id.to_owned(),
            outcome: ProviderSearchOutcome::Complete,
            hits: vec![SessionSearchHit {
                locator: SessionSearchLocator {
                    session_id: SessionId::new(),
                    sequence: FAST_SEQUENCE,
                    record_id: Some(format!("{provider_id}-record")),
                },
                content_kind: SearchContentKind::UserMessage,
                matched_field: SearchField::Text,
                provider_id: provider_id.to_owned(),
                provider_rank: 1,
                provider_score: Some("opaque".to_owned()),
                preview: Some("needle".to_owned()),
                preview_truncated: false,
            }],
            next_cursor: None,
            query_complete: true,
            coverage_complete: true,
            searched_content: request.filters.content_kinds.iter().copied().collect(),
            excluded_content: Vec::new(),
            message: None,
        }
    }

    fn service_response(value: &impl serde::Serialize) -> ServiceResponse {
        ServiceResponse::json(value).expect("test service response encodes")
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn test_provider_service(
        instance: *const c_void,
        input_ptr: *const u8,
        input_len: usize,
        output: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        event_callback: Option<ServiceEventCallback>,
        event_user_data: *mut c_void,
        _bridge_callback: Option<bcode_plugin_sdk::ServiceBridgeCallback>,
        _bridge_user_data: *mut c_void,
        cancellation_callback: Option<ServiceCancellationWaitCallback>,
        cancellation_user_data: *mut c_void,
    ) -> i32 {
        let provider = unsafe { &*instance.cast::<TestProviderInstance>() };
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let Ok(context) = serde_json::from_slice::<NativeServiceContext>(input) else {
            return bcode_plugin_sdk::SERVICE_STATUS_DECODE_FAILED;
        };
        let response = match context.request.operation.as_str() {
            OP_CAPABILITIES
                if matches!(provider.behavior, TestProviderBehavior::FutureCapability) =>
            {
                let mut value = serde_json::to_value(provider_capabilities(provider.provider_id))
                    .expect("capabilities encode");
                value["features"] = serde_json::json!(["terms", "future_semantic_search"]);
                service_response(&value)
            }
            OP_CAPABILITIES => service_response(&provider_capabilities(provider.provider_id)),
            OP_STATUS => {
                let mut status = provider_status(provider.provider_id);
                if matches!(provider.behavior, TestProviderBehavior::IncompatiblePolicy) {
                    status.state = SearchProviderState::Degraded;
                    status.degraded_reason = Some(
                        "configured content policy is incompatible; explicit rebuild required"
                            .to_owned(),
                    );
                }
                if matches!(provider.behavior, TestProviderBehavior::FutureStatus) {
                    status.record_schema_version = CURRENT_SEARCH_RECORD_VERSION.saturating_add(1);
                }
                if matches!(provider.behavior, TestProviderBehavior::Stateful)
                    && let Some(checkpoint) = STATEFUL_CHECKPOINTS
                        .lock()
                        .expect("stateful checkpoints")
                        .get(provider.provider_id)
                        .cloned()
                {
                    status.document_count = 1;
                    status.coverage = vec![bcode_session_search::SessionSearchCoverage {
                        generation: checkpoint.generation,
                        content_kinds: BTreeSet::from([SearchContentKind::UserMessage]),
                        indexed_through_sequence: Some(checkpoint.indexed_through_sequence),
                        complete: true,
                        indexed_text_bytes: checkpoint.indexed_text_bytes,
                        skipped_records: 0,
                        truncated_records: 0,
                        exclusions: Vec::new(),
                    }];
                }
                service_response(&status)
            }
            OP_APPLY_BATCH => {
                if matches!(provider.behavior, TestProviderBehavior::UnavailableApply) {
                    return bcode_plugin_sdk::SERVICE_STATUS_PLUGIN_UNAVAILABLE;
                }
                if let Some((code, message)) = match provider.behavior {
                    TestProviderBehavior::QuotaApply => {
                        Some(("quota_exceeded", "provider quota exhausted"))
                    }
                    TestProviderBehavior::CorruptApply => {
                        Some(("corrupt_index", "provider index is corrupt"))
                    }
                    TestProviderBehavior::StaleApply => {
                        Some(("stale_generation", "provider generation is stale"))
                    }
                    TestProviderBehavior::IncompatiblePolicy => Some((
                        "content_disabled",
                        "provider content policy is incompatible; explicit rebuild required",
                    )),
                    TestProviderBehavior::Fast
                    | TestProviderBehavior::Slow
                    | TestProviderBehavior::Malformed
                    | TestProviderBehavior::FutureStatus
                    | TestProviderBehavior::FutureCapability
                    | TestProviderBehavior::Crash
                    | TestProviderBehavior::RejectCleanup
                    | TestProviderBehavior::Stateful
                    | TestProviderBehavior::UnavailableApply => None,
                } {
                    return bcode_plugin_sdk::write_service_response(
                        &ServiceResponse::error(code, message),
                        output,
                        output_capacity,
                        output_len,
                        bcode_plugin_sdk::ServiceEventEmitter::new(event_callback, event_user_data),
                    );
                }
                let request = context
                    .request
                    .payload_json::<ApplySearchRecordsRequest>()
                    .expect("apply batch request");
                if matches!(provider.behavior, TestProviderBehavior::Slow) {
                    SLOW_APPLY_STARTED.store(true, Ordering::SeqCst);
                    let cancelled = cancellation_callback
                        .is_some_and(|callback| callback(5_000, cancellation_user_data));
                    SLOW_APPLY_CANCELLED.store(cancelled, Ordering::SeqCst);
                    return bcode_plugin_sdk::write_service_response(
                        &if cancelled {
                            ServiceResponse::error("cancelled", "cancelled by host")
                        } else {
                            ServiceResponse::error(
                                "not_cancelled",
                                "host cancellation was not observed",
                            )
                        },
                        output,
                        output_capacity,
                        output_len,
                        bcode_plugin_sdk::ServiceEventEmitter::new(event_callback, event_user_data),
                    );
                }
                assert!(request.records.iter().all(|record| {
                    matches!(
                        record.content_kind,
                        SearchContentKind::SessionTitle
                            | SearchContentKind::UserMessage
                            | SearchContentKind::AssistantMessage
                            | SearchContentKind::SystemMessage
                            | SearchContentKind::ShellCommand
                            | SearchContentKind::ToolError
                            | SearchContentKind::Compaction
                    )
                }));
                let duplicate = if matches!(provider.behavior, TestProviderBehavior::Stateful) {
                    let mut checkpoints =
                        STATEFUL_CHECKPOINTS.lock().expect("stateful checkpoints");
                    let duplicate =
                        checkpoints
                            .get(provider.provider_id)
                            .is_some_and(|checkpoint| {
                                checkpoint.generation.fingerprint == request.generation.fingerprint
                                    && request.indexed_through_sequence.is_some_and(|sequence| {
                                        sequence <= checkpoint.indexed_through_sequence
                                    })
                            });
                    if !duplicate {
                        checkpoints.insert(
                            provider.provider_id,
                            StatefulProviderCheckpoint {
                                generation: request.generation.clone(),
                                indexed_through_sequence: request
                                    .indexed_through_sequence
                                    .unwrap_or(0),
                                indexed_text_bytes: request
                                    .expected_previous_session_text_bytes
                                    .saturating_add(
                                        request
                                            .records
                                            .iter()
                                            .map(|record| record.indexed_bytes)
                                            .sum(),
                                    ),
                            },
                        );
                        drop(checkpoints);
                        STATEFUL_APPLY_PUBLISHED.store(true, Ordering::SeqCst);
                        let (release, changed) = &*STATEFUL_APPLY_RELEASE;
                        let mut release = release.lock().expect("stateful apply release");
                        while !*release {
                            release = changed.wait(release).expect("stateful apply release wait");
                        }
                        drop(release);
                    }
                    duplicate
                } else {
                    false
                };
                APPLY_BATCH_CALLS.fetch_add(1, Ordering::SeqCst);
                service_response(&ApplySearchRecordsResponse {
                    batch_id: request.batch_id,
                    outcome: if duplicate {
                        bcode_session_search::ApplyBatchOutcome::Duplicate
                    } else {
                        bcode_session_search::ApplyBatchOutcome::Applied
                    },
                    applied_records: request.records.len(),
                    indexed_through_sequence: request.indexed_through_sequence.unwrap_or_else(
                        || {
                            request
                                .records
                                .last()
                                .map_or(0, |record| record.locator.sequence)
                        },
                    ),
                })
            }
            OP_REMOVE_SESSION => {
                let _request = context
                    .request
                    .payload_json::<RemoveSessionSearchRequest>()
                    .expect("remove session request");
                REMOVE_SESSION_CALLS.fetch_add(1, Ordering::SeqCst);
                if matches!(provider.behavior, TestProviderBehavior::RejectCleanup) {
                    ServiceResponse::error(
                        "cleanup_rejected",
                        "credential=super-secret cleanup rejection",
                    )
                } else {
                    service_response(&serde_json::json!({}))
                }
            }
            OP_PURGE => {
                let request = context
                    .request
                    .payload_json::<PurgeSessionSearchRequest>()
                    .expect("purge request");
                if request.provider_id != provider.provider_id || request.confirmation != "purge" {
                    ServiceResponse::error("confirmation_required", "exact confirmation required")
                } else {
                    ServiceResponse::empty()
                }
            }
            OP_REBUILD => {
                let request = context
                    .request
                    .payload_json::<RebuildSessionSearchRequest>()
                    .expect("rebuild request");
                if request.provider_id != provider.provider_id || request.confirmation != "rebuild"
                {
                    ServiceResponse::error("confirmation_required", "exact confirmation required")
                } else {
                    service_response(&serde_json::json!({
                        "provider_id": provider.provider_id,
                        "record_schema_version": CURRENT_SEARCH_RECORD_VERSION,
                        "normalization_version": bcode_session_search::CURRENT_NORMALIZATION_VERSION,
                        "policy_version": bcode_session_search::CURRENT_SEARCH_POLICY_VERSION
                    }))
                }
            }
            OP_SEARCH => match provider.behavior {
                TestProviderBehavior::Malformed
                | TestProviderBehavior::FutureStatus
                | TestProviderBehavior::FutureCapability => ServiceResponse::text("not-json"),
                TestProviderBehavior::Crash => {
                    return bcode_plugin_sdk::SERVICE_STATUS_PLUGIN_UNAVAILABLE;
                }
                TestProviderBehavior::Fast
                | TestProviderBehavior::RejectCleanup
                | TestProviderBehavior::Stateful
                | TestProviderBehavior::UnavailableApply
                | TestProviderBehavior::QuotaApply
                | TestProviderBehavior::CorruptApply
                | TestProviderBehavior::StaleApply
                | TestProviderBehavior::IncompatiblePolicy => {
                    match context.request.payload_json::<SessionSearchRequest>() {
                        Ok(request) => service_response(&provider_search_response(
                            provider.provider_id,
                            &request,
                        )),
                        Err(error) => ServiceResponse::error("invalid_request", error.to_string()),
                    }
                }
                TestProviderBehavior::Slow => {
                    SLOW_SEARCH_STARTED.store(true, Ordering::SeqCst);
                    let cancelled = cancellation_callback
                        .is_some_and(|callback| callback(5_000, cancellation_user_data));
                    SLOW_SEARCH_CANCELLED.store(cancelled, Ordering::SeqCst);
                    SLOW_SEARCH_FINISHED.store(true, Ordering::SeqCst);
                    if cancelled {
                        ServiceResponse::error("cancelled", "cancelled by host")
                    } else {
                        ServiceResponse::error(
                            "not_cancelled",
                            "host cancellation was not observed",
                        )
                    }
                }
            },
            operation => ServiceResponse::error(
                "unsupported_operation",
                format!("unsupported operation {operation}"),
            ),
        };
        bcode_plugin_sdk::write_service_response(
            &response,
            output,
            output_capacity,
            output_len,
            bcode_plugin_sdk::ServiceEventEmitter::new(event_callback, event_user_data),
        )
    }

    fn test_activate(_: *const c_void) -> i32 {
        bcode_plugin_sdk::EXIT_OK
    }

    fn test_deactivate(_: *const c_void) -> i32 {
        bcode_plugin_sdk::EXIT_OK
    }

    fn test_handle_event(_: *const c_void, _: *const u8, _: usize) -> i32 {
        bcode_plugin_sdk::EVENT_STATUS_OK
    }

    fn provider_vtable(
        provider_id: &'static str,
        behavior: TestProviderBehavior,
    ) -> StaticPluginVtable {
        let instance = Box::leak(Box::new(TestProviderInstance {
            provider_id,
            behavior,
        }));
        let manifest = match provider_id {
            FAST_PROVIDER_ID => fast_provider_manifest_export,
            SLOW_PROVIDER_ID => slow_provider_manifest_export,
            MALFORMED_PROVIDER_ID => malformed_provider_manifest_export,
            FUTURE_PROVIDER_ID => future_provider_manifest_export,
            FUTURE_CAPABILITY_PROVIDER_ID => future_capability_provider_manifest_export,
            CRASH_PROVIDER_ID => crash_provider_manifest_export,
            UNAVAILABLE_PROVIDER_ID => unavailable_provider_manifest_export,
            _ => panic!("unknown test provider {provider_id}"),
        };
        StaticPluginVtable {
            instance: std::ptr::from_ref(instance).cast(),
            manifest,
            activate: test_activate,
            register_commands: None,
            register_auth_providers: None,
            deactivate: test_deactivate,
            invoke_service_streaming: test_provider_service,
            cli_registration: None,
            handle_event: test_handle_event,
        }
    }

    pub async fn lock_search_tests() -> tokio::sync::MutexGuard<'static, ()> {
        SEARCH_TEST_LOCK.lock().await
    }

    pub fn slow_apply_started() -> bool {
        SLOW_APPLY_STARTED.load(Ordering::SeqCst)
    }

    pub fn reset_slow_apply_state() {
        SLOW_APPLY_STARTED.store(false, Ordering::SeqCst);
        SLOW_APPLY_CANCELLED.store(false, Ordering::SeqCst);
    }

    pub fn state_with_static_provider_vtables(
        providers: &[(PluginManifest, StaticPluginVtable)],
        configs: BTreeMap<String, bcode_plugin::ResolvedPluginConfig>,
    ) -> crate::ServerState {
        let static_plugins = providers
            .iter()
            .map(|(manifest, vtable)| {
                bcode_plugin::StaticBundledPlugin::new(
                    Box::leak(
                        toml::to_string(manifest)
                            .expect("static provider manifest serializes")
                            .into_boxed_str(),
                    ),
                    *vtable,
                )
            })
            .collect::<Vec<_>>();
        let selection = bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: providers
                .iter()
                .map(|(manifest, _)| manifest.id.clone())
                .collect(),
            disabled: BTreeSet::new(),
        };
        let plugins = PluginHost::load_defaults_with_static_bundled_and_config(
            &selection,
            &static_plugins,
            configs,
        )
        .expect("load real static providers")
        .into();
        crate::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::persistent_lazy(
                tempfile::tempdir().expect("session root").keep(),
            ),
            plugins,
        )
    }

    pub fn state_with_providers(
        providers: &[(&'static str, TestProviderBehavior)],
    ) -> crate::ServerState {
        let loaded = providers
            .iter()
            .map(|(provider_id, behavior)| {
                (
                    provider_manifest(provider_id),
                    provider_vtable(provider_id, *behavior),
                )
            })
            .collect::<Vec<_>>();
        let plugins = PluginHost::load_static_plugins(&loaded)
            .expect("load test providers")
            .into();
        crate::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::persistent_lazy(
                tempfile::tempdir().expect("session root").keep(),
            ),
            plugins,
        )
    }

    fn parallel_routes() -> Vec<SessionSearchContentRoute> {
        vec![SessionSearchContentRoute {
            content_kinds: BTreeSet::from([SearchContentKind::UserMessage]),
            mode: SessionSearchRouteMode::Parallel,
            provider_ids: vec![FAST_PROVIDER_ID.to_owned(), SLOW_PROVIDER_ID.to_owned()],
        }]
    }

    fn reset_slow_provider_state() {
        SLOW_SEARCH_STARTED.store(false, Ordering::SeqCst);
        SLOW_SEARCH_CANCELLED.store(false, Ordering::SeqCst);
        SLOW_SEARCH_FINISHED.store(false, Ordering::SeqCst);
    }

    async fn wait_for_slow_provider_to_finish() {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !SLOW_SEARCH_FINISHED.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn request() -> SessionSearchRequest {
        SessionSearchRequest {
            query: bcode_session_search::SessionSearchQuery::Text {
                text: "needle".to_owned(),
                mode: bcode_session_search::TextMatchMode::Terms,
                fields: std::collections::BTreeSet::new(),
            },
            filters: bcode_session_search::SessionSearchFilters::default(),
            sort: bcode_session_search::SessionSearchSort::ProviderRelevance,
            limit: 2,
            cursor: None,
            deadline_ms: Some(100),
        }
    }

    fn response() -> SessionSearchResponse {
        SessionSearchResponse {
            provider_id: "provider".to_owned(),
            outcome: ProviderSearchOutcome::Complete,
            hits: vec![SessionSearchHit {
                locator: SessionSearchLocator {
                    session_id: SessionId::new(),
                    sequence: 1,
                    record_id: Some("record-1".to_owned()),
                },
                content_kind: SearchContentKind::UserMessage,
                matched_field: SearchField::Text,
                provider_id: "provider".to_owned(),
                provider_rank: 1,
                provider_score: Some("opaque".to_owned()),
                preview: Some("match".to_owned()),
                preview_truncated: false,
            }],
            next_cursor: None,
            query_complete: true,
            coverage_complete: true,
            searched_content: vec![SearchContentKind::UserMessage],
            excluded_content: Vec::new(),
            message: None,
        }
    }

    #[test]
    fn ordinary_empty_content_request_excludes_large_output_from_provider_payload() {
        let mut provider = bcode_session_search::SessionSearchProviderInfo {
            plugin_id: "provider".to_owned(),
            capabilities: provider_capabilities("provider"),
            status: provider_status("provider"),
        };
        provider
            .capabilities
            .content_kinds
            .insert(SearchContentKind::ShellOutput);
        provider
            .capabilities
            .content_kinds
            .insert(SearchContentKind::ToolOutput);
        let ordinary =
            request_for_provider(&request(), &provider, &SessionSearchPlanPolicy::default());
        assert!(
            !ordinary
                .filters
                .content_kinds
                .contains(&SearchContentKind::ShellOutput)
        );
        assert!(
            !ordinary
                .filters
                .content_kinds
                .contains(&SearchContentKind::ToolOutput)
        );
        assert!(
            ordinary
                .filters
                .content_kinds
                .contains(&SearchContentKind::UserMessage)
        );

        let deep = request_for_provider(
            &request(),
            &provider,
            &SessionSearchPlanPolicy {
                execution_class: bcode_session_search::SessionSearchExecutionClass::Deep,
                ..SessionSearchPlanPolicy::default()
            },
        );
        assert!(
            deep.filters
                .content_kinds
                .contains(&SearchContentKind::ShellOutput)
        );
        assert!(
            deep.filters
                .content_kinds
                .contains(&SearchContentKind::ToolOutput)
        );
    }

    #[test]
    fn ordinary_plan_never_invokes_scan_provider_without_explicit_deep_intent() {
        let mut indexed = bcode_session_search::SessionSearchProviderInfo {
            plugin_id: "indexed".to_owned(),
            capabilities: provider_capabilities("indexed"),
            status: provider_status("indexed"),
        };
        indexed
            .capabilities
            .content_kinds
            .insert(SearchContentKind::UserMessage);
        let mut scan = bcode_session_search::SessionSearchProviderInfo {
            plugin_id: "bcode.compressed-session-search".to_owned(),
            capabilities: provider_capabilities("bcode.compressed-session-search"),
            status: provider_status("bcode.compressed-session-search"),
        };
        scan.capabilities.execution = SearchExecutionKind::Scan;
        scan.capabilities.content_kinds = BTreeSet::from([
            SearchContentKind::ShellOutput,
            SearchContentKind::ToolOutput,
        ]);
        let discovery = bcode_session_search::ListSessionSearchProvidersResponse {
            providers: vec![indexed, scan],
            failures: Vec::new(),
        };

        let ordinary_request = request();
        let ordinary = bcode_session_search::plan_session_search_with_policy_and_routes(
            &ordinary_request,
            discovery.clone(),
            &SessionSearchPlanPolicy {
                per_provider_deadline_ms: 100,
                ..SessionSearchPlanPolicy::default()
            },
            &[],
        );
        assert!(
            ordinary
                .providers
                .iter()
                .all(|provider| provider.plugin_id != "bcode.compressed-session-search")
        );
        assert!(
            ordinary
                .failures
                .iter()
                .all(|failure| failure.plugin_id != "indexed")
        );
        assert!(
            ordinary
                .failures
                .iter()
                .all(|failure| failure.plugin_id != "bcode.compressed-session-search"),
            "deep-only providers are intentionally ineligible, not ordinary query failures"
        );

        let mut deep_request = request();
        deep_request.filters.content_kinds = BTreeSet::from([
            SearchContentKind::ShellOutput,
            SearchContentKind::ToolOutput,
        ]);
        let deep = bcode_session_search::plan_session_search_with_policy_and_routes(
            &deep_request,
            discovery,
            &SessionSearchPlanPolicy {
                execution_class: bcode_session_search::SessionSearchExecutionClass::Deep,
                per_provider_deadline_ms: 100,
                ..SessionSearchPlanPolicy::default()
            },
            &[],
        );
        assert!(
            deep.providers
                .iter()
                .any(|provider| { provider.plugin_id == "bcode.compressed-session-search" }),
            "providers={:?}, failures={:?}",
            deep.providers,
            deep.failures
        );
    }

    #[test]
    fn malformed_provider_response_fields_are_rejected() {
        let request = request();
        let mut malformed = response();
        malformed.hits[0].provider_rank = 0;
        assert!(validate_provider_response("provider", &request, &malformed).is_err());

        let mut malformed = response();
        malformed.hits[0].provider_score =
            Some("x".repeat(bcode_session_search::MAX_CURSOR_BYTES + 1));
        assert!(validate_provider_response("provider", &request, &malformed).is_err());

        let mut malformed = response();
        malformed.message = Some("x".repeat(bcode_session_search::MAX_HIT_PREVIEW_BYTES + 1));
        assert!(validate_provider_response("provider", &request, &malformed).is_err());

        let mut malformed = response();
        malformed.next_cursor = Some(bcode_session_search::SearchCursor {
            provider_id: "provider".to_owned(),
            query_fingerprint: String::new(),
            value: "cursor".to_owned(),
        });
        assert!(validate_provider_response("provider", &request, &malformed).is_err());
    }

    #[test]
    fn valid_provider_response_is_accepted() {
        assert_eq!(
            validate_provider_response("provider", &request(), &response()),
            Ok(())
        );
    }

    #[test]
    fn stale_hydration_is_reported_as_provider_failure() {
        let provider_response = response();
        let hit = provider_response.hits[0].clone();
        let mut aggregate = bcode_session_search::FederatedSessionSearchResponse {
            hits: vec![hit.clone()],
            query_complete: true,
            coverage_complete: true,
            providers: Vec::new(),
            failures: Vec::new(),
        };
        let hydrated = vec![HydratedSessionSearchHit {
            hit,
            outcome: SearchHitHydrationOutcome::StaleLocator,
            event: None,
            message: Some("stale".to_owned()),
        }];
        report_hydration_outcomes(&mut aggregate, &hydrated, 12);
        assert!(!aggregate.query_complete);
        assert!(!aggregate.coverage_complete);
        assert_eq!(aggregate.failures.len(), 1);
        assert_eq!(
            aggregate.failures[0].stage,
            bcode_session_search::SessionSearchProviderStage::Hydration
        );
        assert_eq!(aggregate.failures[0].elapsed_ms, 12);
        assert_eq!(
            aggregate.failures[0].content,
            vec![SearchContentKind::UserMessage]
        );
    }

    #[test]
    fn configured_session_search_provider_inventory_is_identified_conservatively() {
        let config = bcode_plugin::ResolvedPluginConfig::new(
            serde_json::json!({"session_search_provider": true}),
            serde_json::json!({"session_search_provider": true}),
        );
        assert!(looks_like_session_search_provider(
            "custom.provider",
            Some(&config)
        ));
        assert!(looks_like_session_search_provider(
            "example.session-search",
            None
        ));
        assert!(!looks_like_session_search_provider(
            "unrelated.plugin",
            None
        ));
    }

    #[tokio::test]
    async fn explicit_provider_maintenance_requires_capability_and_confirmation() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);

        let error = purge_provider(&state, FAST_PROVIDER_ID, "wrong".to_owned())
            .await
            .expect_err("wrong confirmation must fail");
        assert_eq!(error.code, SearchErrorCode::InvalidRequest);

        let purged = purge_provider(&state, FAST_PROVIDER_ID, "purge".to_owned())
            .await
            .expect("purge succeeds");
        assert_eq!(purged.provider_id, FAST_PROVIDER_ID);
        assert_eq!(purged.operation, OP_PURGE);
        assert_eq!(purged.status.state, SearchProviderState::Ready);

        let rebuilt = rebuild_provider(&state, FAST_PROVIDER_ID, "rebuild".to_owned())
            .await
            .expect("rebuild succeeds");
        assert_eq!(rebuilt.provider_id, FAST_PROVIDER_ID);
        assert_eq!(rebuilt.operation, OP_REBUILD);
    }

    #[tokio::test]
    async fn global_search_disablement_prevents_inventory_ingestion_query_and_maintenance() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let mut state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        state.session_search_enabled = false;

        let inventory = list_providers(&state).await;
        assert!(inventory.providers.is_empty());
        assert!(inventory.failures.is_empty());

        let error = search_provider(&state, FAST_PROVIDER_ID, &request())
            .await
            .expect_err("query must be disabled");
        assert_eq!(error.code, SearchErrorCode::ProviderUnavailable);
        assert!(!error.retryable);

        let error = purge_provider(&state, FAST_PROVIDER_ID, "purge".to_owned())
            .await
            .expect_err("maintenance must be disabled");
        assert_eq!(error.code, SearchErrorCode::ProviderUnavailable);

        state
            .session_search_dirty
            .mark_committed(SessionId::new())
            .await;
        process_dirty_sessions(&state).await;
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn federated_runtime_preserves_fast_results_when_slow_provider_times_out() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        reset_slow_provider_state();
        let state = state_with_providers(&[
            (FAST_PROVIDER_ID, TestProviderBehavior::Fast),
            (SLOW_PROVIDER_ID, TestProviderBehavior::Slow),
        ]);
        let mut request = request();
        request.deadline_ms = Some(250);
        let policy = SessionSearchPlanPolicy {
            per_provider_deadline_ms: 40,
            ..SessionSearchPlanPolicy::default()
        };

        let started = std::time::Instant::now();
        let response =
            search_federated_with_policy_and_routes(&state, &request, &policy, &parallel_routes())
                .await
                .expect("federated search");

        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(SLOW_SEARCH_STARTED.load(Ordering::SeqCst));
        wait_for_slow_provider_to_finish().await;
        assert!(SLOW_SEARCH_CANCELLED.load(Ordering::SeqCst));
        assert!(SLOW_SEARCH_FINISHED.load(Ordering::SeqCst));
        assert_eq!(response.hits.len(), 1);
        assert_eq!(response.hits[0].provider_id, FAST_PROVIDER_ID);
        assert!(response.providers.iter().any(|provider| {
            provider.provider_id == FAST_PROVIDER_ID
                && provider.outcome == ProviderSearchOutcome::Complete
        }));
        assert!(response.failures.iter().any(|failure| {
            failure.plugin_id == SLOW_PROVIDER_ID
                && failure.error.code == SearchErrorCode::DeadlineExceeded
                && failure.stage == bcode_session_search::SessionSearchProviderStage::Execution
        }));
        assert!(!response.query_complete);
        assert!(!response.coverage_complete);
    }

    #[tokio::test]
    async fn federated_runtime_rejects_malformed_provider_without_losing_healthy_results() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        let state = state_with_providers(&[
            (FAST_PROVIDER_ID, TestProviderBehavior::Fast),
            (MALFORMED_PROVIDER_ID, TestProviderBehavior::Malformed),
        ]);
        let mut routes = parallel_routes();
        routes[0].provider_ids[1] = MALFORMED_PROVIDER_ID.to_owned();
        let response = search_federated_with_policy_and_routes(
            &state,
            &request(),
            &SessionSearchPlanPolicy {
                per_provider_deadline_ms: 100,
                ..SessionSearchPlanPolicy::default()
            },
            &routes,
        )
        .await
        .expect("federated search");

        assert_eq!(response.hits.len(), 1);
        assert_eq!(response.hits[0].provider_id, FAST_PROVIDER_ID);
        assert!(response.failures.iter().any(|failure| {
            failure.plugin_id == MALFORMED_PROVIDER_ID
                && failure.error.code == SearchErrorCode::ProviderUnavailable
        }));
        assert!(!response.query_complete);
        assert!(!response.coverage_complete);
    }

    #[test]
    fn canonical_hydration_classifies_future_and_damaged_storage_without_guessing() {
        let hit = response().hits.remove(0);
        let future = hydration_error(
            hit.clone(),
            &bcode_session::SessionError::Db(bcode_session::db::SessionDbError::PersistedEvent(
                bcode_session::persisted::PersistedSessionEventError::UnsupportedSchemaVersion {
                    actual: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
                        .saturating_add(1),
                    current: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                },
            )),
        );
        assert_eq!(future.outcome, SearchHitHydrationOutcome::Incompatible);
        assert!(future.event.is_none());

        let damaged = hydration_error(
            hit,
            &bcode_session::SessionError::Db(
                bcode_session::db::SessionDbError::MigrationHistoryIncompatible {
                    reason: "unknown migration tail".to_owned(),
                },
            ),
        );
        assert_eq!(damaged.outcome, SearchHitHydrationOutcome::RepairRequired);
        assert!(damaged.event.is_none());
    }

    #[tokio::test]
    async fn canonical_deletion_cleanup_is_explicit_and_provider_scoped() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        REMOVE_SESSION_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let session_id = SessionId::new();
        remove_session_from_providers(&state, session_id, Some("generation".to_owned())).await;
        assert_eq!(REMOVE_SESSION_CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provider_cleanup_rejection_is_non_fatal() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        REMOVE_SESSION_CALLS.store(0, Ordering::SeqCst);
        let state =
            state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::RejectCleanup)]);
        remove_session_from_providers(&state, SessionId::new(), Some("generation".to_owned()))
            .await;
        assert_eq!(REMOVE_SESSION_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn projected_records_use_canonical_summary_scope_without_source_persistence_details() {
        let session_id = SessionId::new();
        let summary = bcode_session_models::SessionSummary {
            id: session_id,
            name: None,
            explicit_name: None,
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::EmptyDraft,
            client_count: 0,
            created_at_ms: 10,
            updated_at_ms: 20,
            working_directory: std::path::PathBuf::from("/workspace/imported"),
            import: Some(bcode_session_models::SessionImportSummary {
                source_id: "opencode".to_owned(),
                source_display_name: "OpenCode".to_owned(),
                external_session_id: "source-native-id".to_owned(),
                imported_at_ms: 30,
            }),
            execution: None,
        };
        let record = bcode_session_search::SessionSearchRecord {
            schema_version: CURRENT_SEARCH_RECORD_VERSION,
            record_id: "1:user-message:0".to_owned(),
            locator: SessionSearchLocator {
                session_id,
                sequence: 1,
                record_id: Some("1:user-message:0".to_owned()),
            },
            timestamp_ms: 1,
            content_kind: SearchContentKind::UserMessage,
            field: Some(SearchField::Text),
            text: Some("hello".to_owned()),
            attributes: std::collections::BTreeMap::new(),
            source_bytes: 5,
            normalized_bytes: 5,
            indexed_bytes: 5,
            truncated: false,
            source_range_start: Some(0),
            source_range_end: Some(5),
            chunk_ordinal: None,
            chunk_count: None,
            normalization_version: CURRENT_NORMALIZATION_VERSION,
            policy_version: CURRENT_SEARCH_POLICY_VERSION,
        };

        let record = apply_session_summary_attributes(record, &summary);

        assert_eq!(
            record
                .attributes
                .get("working_directory")
                .map(String::as_str),
            Some("/workspace/imported")
        );
        assert_eq!(
            record.attributes.get("source").map(String::as_str),
            Some("opencode")
        );
        assert!(
            !record
                .attributes
                .values()
                .any(|value| value == "source-native-id")
        );
        assert!(!record.attributes.contains_key("source_database"));
        assert!(!record.attributes.contains_key("source_path"));
    }

    #[test]
    fn canonical_generation_fingerprint_binds_identity_import_and_fork_but_not_mutable_metadata() {
        let session_id = SessionId::new();
        let mut summary = bcode_session_models::SessionSummary {
            id: session_id,
            name: None,
            explicit_name: None,
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::EmptyDraft,
            client_count: 0,
            created_at_ms: 10,
            updated_at_ms: 20,
            working_directory: std::path::PathBuf::new(),
            import: None,
            execution: None,
        };
        let native = canonical_generation_fingerprint(&summary);
        summary.updated_at_ms = 999;
        summary.name = Some("renamed".to_owned());
        summary.explicit_name = Some("renamed".to_owned());
        summary.derived_title = Some("derived".to_owned());
        summary.working_directory = std::path::PathBuf::from("/renamed/workspace");
        assert_eq!(canonical_generation_fingerprint(&summary), native);
        summary.import = Some(bcode_session_models::SessionImportSummary {
            source_id: "source".to_owned(),
            source_display_name: "Source".to_owned(),
            external_session_id: "external".to_owned(),
            imported_at_ms: 30,
        });
        let imported = canonical_generation_fingerprint(&summary);
        assert_ne!(imported, native);
    }

    #[tokio::test]
    async fn dirty_session_processing_projects_bounded_records_and_applies_provider_batch() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("ingestion".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "searchable bounded summary".to_owned(), 0)
            .await
            .expect("append event");
        state.session_search_dirty.mark_committed(session.id).await;

        process_dirty_sessions(&state).await;

        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 1);
        assert!(state.session_search_dirty.snapshot().await.0.is_empty());
    }

    #[tokio::test]
    async fn grouped_hydration_preserves_order_and_splits_sparse_session_ranges() {
        let state = state_with_providers(&[]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("grouped hydration".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        for sequence in 0..70 {
            state
                .sessions
                .append_context_compacted(session.id, format!("event {sequence}"), 0)
                .await
                .expect("append event");
        }
        let sequences = [70, 2, 69, 10_000];
        let hits = sequences
            .into_iter()
            .enumerate()
            .map(|(rank, sequence)| SessionSearchHit {
                locator: SessionSearchLocator {
                    session_id: session.id,
                    sequence,
                    record_id: Some(format!("record-{sequence}")),
                },
                content_kind: SearchContentKind::Compaction,
                matched_field: SearchField::Text,
                provider_id: FAST_PROVIDER_ID.to_owned(),
                provider_rank: u32::try_from(rank + 1).expect("rank"),
                provider_score: None,
                preview: None,
                preview_truncated: false,
            })
            .collect();

        let hydrated = hydrate_hits(&state, hits).await;

        assert_eq!(
            hydrated
                .iter()
                .map(|hit| hit.hit.locator.sequence)
                .collect::<Vec<_>>(),
            sequences
        );
        assert_eq!(hydrated[0].outcome, SearchHitHydrationOutcome::Hydrated);
        assert_eq!(hydrated[1].outcome, SearchHitHydrationOutcome::Hydrated);
        assert_eq!(hydrated[2].outcome, SearchHitHydrationOutcome::Hydrated);
        assert_eq!(hydrated[3].outcome, SearchHitHydrationOutcome::StaleLocator);
    }

    #[tokio::test]
    #[ignore = "manual canonical hydration performance baseline"]
    async fn benchmark_canonical_hit_hydration() {
        const DEFAULT_EVENTS: usize = 200;
        const MAX_EVENTS: usize = 100_000;
        const HITS: usize = 20;
        const RUNS: usize = 20;
        const HYDRATION_P95_BUDGET_US: u128 = 100_000;

        let events = std::env::var("BCODE_SESSION_SEARCH_HYDRATION_EVENTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_EVENTS);
        assert!(events > 0 && events <= MAX_EVENTS);
        let state = state_with_providers(&[]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("hydration benchmark".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        for sequence in 0..events {
            state
                .sessions
                .append_context_compacted(
                    session.id,
                    format!("hydration benchmark event {sequence}"),
                    0,
                )
                .await
                .expect("append event");
        }
        let tail = state
            .sessions
            .session_history_page(
                session.id,
                bcode_session_models::SessionHistoryQuery {
                    cursor: None,
                    limit: HITS,
                    direction: bcode_session_models::SessionHistoryDirection::Backward,
                },
            )
            .await
            .expect("read hydration locators");
        assert_eq!(tail.events.len(), HITS);
        let hits = tail
            .events
            .into_iter()
            .enumerate()
            .map(|(index, event)| SessionSearchHit {
                locator: SessionSearchLocator {
                    session_id: session.id,
                    sequence: event.sequence,
                    record_id: Some(format!("hydration-{index}")),
                },
                content_kind: SearchContentKind::Compaction,
                matched_field: SearchField::Text,
                provider_id: FAST_PROVIDER_ID.to_owned(),
                provider_rank: u32::try_from(index + 1).unwrap_or(u32::MAX),
                provider_score: None,
                preview: None,
                preview_truncated: false,
            })
            .collect::<Vec<_>>();
        let mut durations = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = Instant::now();
            let hydrated = hydrate_hits(&state, hits.clone()).await;
            durations.push(started.elapsed().as_micros());
            assert!(hydrated.iter().all(|hit| {
                hit.outcome == SearchHitHydrationOutcome::Hydrated && hit.event.is_some()
            }));
        }
        durations.sort_unstable();
        let p50_us = durations[RUNS / 2];
        let p95_us = durations[(RUNS * 95 / 100).min(RUNS - 1)];
        let p99_us = durations[(RUNS * 99 / 100).min(RUNS - 1)];
        println!(
            "session_search_hydration_benchmark events={events} hits={HITS} runs={RUNS} p50_us={p50_us} p95_us={p95_us} p99_us={p99_us}"
        );
        assert!(
            p95_us <= HYDRATION_P95_BUDGET_US,
            "canonical hydration p95 {p95_us} us exceeds 100 ms budget"
        );
    }

    #[test]
    fn apply_batch_acknowledgment_rejects_conflicts_and_mismatches() {
        let request = ApplySearchRecordsRequest {
            provider_id: "provider".to_owned(),
            batch_id: "batch".to_owned(),
            generation: SearchCanonicalGeneration {
                session_id: SessionId::new(),
                fingerprint: "generation".to_owned(),
                last_sequence: Some(1),
            },
            expected_previous_sequence: None,
            expected_previous_session_text_bytes: 0,
            indexed_through_sequence: Some(1),
            records: Vec::new(),
        };
        let valid_duplicate = ApplySearchRecordsResponse {
            batch_id: "batch".to_owned(),
            outcome: bcode_session_search::ApplyBatchOutcome::Duplicate,
            applied_records: 0,
            indexed_through_sequence: 1,
        };
        assert!(validate_apply_batch_response(&request, valid_duplicate).is_ok());

        let conflict = ApplySearchRecordsResponse {
            batch_id: "batch".to_owned(),
            outcome: bcode_session_search::ApplyBatchOutcome::ConflictingDuplicate,
            applied_records: 0,
            indexed_through_sequence: 1,
        };
        assert!(validate_apply_batch_response(&request, conflict).is_err());

        let wrong_batch = ApplySearchRecordsResponse {
            batch_id: "other".to_owned(),
            outcome: bcode_session_search::ApplyBatchOutcome::Duplicate,
            applied_records: 0,
            indexed_through_sequence: 1,
        };
        assert!(validate_apply_batch_response(&request, wrong_batch).is_err());
    }

    #[test]
    fn deadline_continuation_resumes_after_last_complete_session() {
        let first = bcode_session_models::SessionSummary {
            id: SessionId::new(),
            name: Some("first".to_owned()),
            explicit_name: None,
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::Explicit,
            client_count: 0,
            created_at_ms: 1,
            updated_at_ms: 10,
            working_directory: "/tmp".into(),
            import: None,
            execution: None,
        };
        let second = bcode_session_models::SessionSummary {
            id: SessionId::new(),
            name: Some("second".to_owned()),
            explicit_name: None,
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::Explicit,
            client_count: 0,
            created_at_ms: 2,
            updated_at_ms: 20,
            working_directory: "/tmp".into(),
            import: None,
            execution: None,
        };
        let results = vec![
            SessionSearchBackfillSessionResult {
                session_id: first.id,
                outcome: SessionSearchBackfillOutcome::Complete,
                batches_applied: 1,
                indexed_through_sequence: Some(1),
                canonical_tail_sequence: Some(1),
                error: None,
            },
            SessionSearchBackfillSessionResult {
                session_id: second.id,
                outcome: SessionSearchBackfillOutcome::Incomplete,
                batches_applied: 1,
                indexed_through_sequence: Some(1),
                canonical_tail_sequence: Some(2),
                error: None,
            },
        ];

        assert_eq!(
            backfill_continuation_cursor(None, &[first.clone(), second], &results, true, None),
            Some(bcode_session_search::SessionSearchBackfillCursor {
                updated_at_ms: first.updated_at_ms,
                session_id: first.id,
            })
        );
    }

    #[test]
    fn cancellation_before_first_provider_produces_valid_terminal_aggregates() {
        let provider_ids = vec!["first".to_owned(), "second".to_owned()];
        let response =
            terminal_complete_backfill_response(&provider_ids, 1, 1, 1, true, Vec::new());
        assert!(response.cancelled);
        assert_eq!(response.providers.len(), 2);
        assert!(response.validate().is_ok());
    }

    #[tokio::test]
    async fn maintenance_scheduler_yields_to_waiting_ordinary_work_between_slices() {
        let scheduler = std::sync::Arc::new(SessionSearchWorkScheduler::default());
        let first_slice_started = std::sync::Arc::new(Notify::new());
        let release_first_slice = std::sync::Arc::new(Notify::new());
        let order = std::sync::Arc::new(Mutex::new(Vec::new()));

        let maintenance_scheduler = std::sync::Arc::clone(&scheduler);
        let maintenance_started = std::sync::Arc::clone(&first_slice_started);
        let maintenance_release = std::sync::Arc::clone(&release_first_slice);
        let maintenance_order = std::sync::Arc::clone(&order);
        let first = tokio::spawn(async move {
            maintenance_scheduler
                .run_maintenance(async {
                    maintenance_order.lock().await.push("maintenance-1");
                    maintenance_started.notify_waiters();
                    maintenance_release.notified().await;
                })
                .await;
        });
        first_slice_started.notified().await;

        let ordinary_scheduler = std::sync::Arc::clone(&scheduler);
        let ordinary_order = std::sync::Arc::clone(&order);
        let ordinary = tokio::spawn(async move {
            ordinary_scheduler
                .run_ordinary(async {
                    ordinary_order.lock().await.push("ordinary");
                })
                .await;
        });
        tokio::task::yield_now().await;
        release_first_slice.notify_waiters();
        first.await.expect("first maintenance slice");

        scheduler
            .run_maintenance(async {
                order.lock().await.push("maintenance-2");
            })
            .await;
        ordinary.await.expect("ordinary work");

        assert_eq!(
            *order.lock().await,
            vec!["maintenance-1", "ordinary", "maintenance-2"]
        );
    }

    #[tokio::test]
    async fn complete_backfill_selects_all_enabled_providers_deterministically() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[
            (SLOW_PROVIDER_ID, TestProviderBehavior::Fast),
            (FAST_PROVIDER_ID, TestProviderBehavior::Fast),
        ]);
        let workspace = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("complete all providers".to_owned()),
                workspace.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "index me".to_owned(), 0)
            .await
            .expect("append event");
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let progress = std::sync::Mutex::new(Vec::new());
        let collect_progress = |update| progress.lock().expect("progress").push(update);

        let response = complete_backfill(
            &state,
            bcode_session_search::CompleteSessionSearchBackfillRequest {
                provider_id: None,
                session_ids: BTreeSet::from([session.id]),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                slice_deadline_ms: 5_000,
            },
            &cancellation,
            Some(&collect_progress),
        )
        .await
        .expect("complete backfill");

        assert_eq!(
            response.provider_ids,
            vec![FAST_PROVIDER_ID.to_owned(), SLOW_PROVIDER_ID.to_owned()]
        );
        assert_eq!(response.providers.len(), 2);
        assert!(response.providers.iter().all(|provider| {
            provider.completed_sessions == 1
                && provider.failed_sessions == 0
                && provider.incomplete_sessions == 0
        }));
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 2);
        let progress = progress.lock().expect("progress");
        assert!(
            progress
                .iter()
                .any(|update| update.providers_completed == 2)
        );
        assert!(progress.iter().all(|update| update.validate().is_ok()));
        let terminal_progress = progress.last().expect("terminal progress");
        assert_eq!(terminal_progress.selected_sessions, 2);
        assert_eq!(terminal_progress.visited_sessions, 2);
        assert_eq!(terminal_progress.completed_sessions, 2);
        assert_eq!(terminal_progress.incomplete_sessions, 0);
        assert_eq!(terminal_progress.failed_sessions, 0);
        assert_eq!(terminal_progress.providers.len(), 2);
        drop(progress);
    }

    #[tokio::test]
    async fn complete_backfill_traverses_more_than_two_catalog_pages_without_skipping_sessions() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let workspace = tempfile::tempdir().expect("workspace");
        let session_count = MAX_BACKFILL_SESSIONS * 2 + 1;
        for index in 0..session_count {
            let session = state
                .sessions
                .create_session(
                    Some(format!("multi-page backfill {index}")),
                    workspace.path().to_path_buf(),
                )
                .await
                .expect("create session");
            state
                .sessions
                .append_context_compacted(session.id, format!("content {index}"), 0)
                .await
                .expect("append event");
        }

        let response = complete_backfill(
            &state,
            bcode_session_search::CompleteSessionSearchBackfillRequest {
                provider_id: Some(FAST_PROVIDER_ID.to_owned()),
                session_ids: BTreeSet::new(),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                slice_deadline_ms: 300_000,
            },
            &bcode_plugin_sdk::ServiceCancellation::default(),
            None,
        )
        .await
        .expect("complete multi-page backfill");

        let provider = &response.providers[0];
        assert_eq!(provider.catalog_pages, 3);
        assert_eq!(provider.selected_sessions, session_count);
        assert_eq!(provider.completed_sessions, session_count);
        assert_eq!(provider.incomplete_sessions, 0);
        assert_eq!(provider.failed_sessions, 0);
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), session_count);
    }

    #[tokio::test]
    async fn complete_backfill_continues_after_provider_failure_and_reports_partial_failure() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[
            (
                UNAVAILABLE_PROVIDER_ID,
                TestProviderBehavior::UnavailableApply,
            ),
            (FAST_PROVIDER_ID, TestProviderBehavior::Fast),
        ]);
        let workspace = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("partial provider failure".to_owned()),
                workspace.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "index healthy provider".to_owned(), 0)
            .await
            .expect("append event");

        let response = complete_backfill(
            &state,
            bcode_session_search::CompleteSessionSearchBackfillRequest {
                provider_id: None,
                session_ids: BTreeSet::from([session.id]),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                slice_deadline_ms: 5_000,
            },
            &bcode_plugin_sdk::ServiceCancellation::default(),
            None,
        )
        .await
        .expect("complete backfill returns aggregate failure");

        assert_eq!(
            response.provider_ids,
            vec![
                FAST_PROVIDER_ID.to_owned(),
                UNAVAILABLE_PROVIDER_ID.to_owned()
            ]
        );
        let healthy = response
            .providers
            .iter()
            .find(|provider| provider.provider_id == FAST_PROVIDER_ID)
            .expect("healthy provider result");
        assert_eq!(healthy.completed_sessions, 1);
        assert_eq!(healthy.failed_sessions, 0);
        assert!(healthy.error.is_none());
        let unavailable = response
            .providers
            .iter()
            .find(|provider| provider.provider_id == UNAVAILABLE_PROVIDER_ID)
            .expect("unavailable provider result");
        assert_eq!(unavailable.completed_sessions, 0);
        assert_eq!(unavailable.failed_sessions, 1);
        assert!(unavailable.error.is_none());
        assert_eq!(
            unavailable
                .failed_sessions
                .saturating_add(unavailable.incomplete_sessions),
            1,
            "terminal aggregate must retain the provider-local failed session"
        );
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn complete_backfill_rerun_uses_provider_checkpoint_without_duplicate_apply() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        STATEFUL_CHECKPOINTS
            .lock()
            .expect("stateful checkpoints")
            .clear();
        STATEFUL_APPLY_PUBLISHED.store(false, Ordering::SeqCst);
        *STATEFUL_APPLY_RELEASE
            .0
            .lock()
            .expect("stateful apply release") = true;
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Stateful)]);
        let workspace = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("idempotent complete backfill".to_owned()),
                workspace.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "index once".to_owned(), 0)
            .await
            .expect("append event");
        let request = bcode_session_search::CompleteSessionSearchBackfillRequest {
            provider_id: Some(FAST_PROVIDER_ID.to_owned()),
            session_ids: BTreeSet::from([session.id]),
            after_timestamp_ms: None,
            before_timestamp_ms: None,
            slice_deadline_ms: 5_000,
        };
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();

        let first = complete_backfill(&state, request.clone(), &cancellation, None)
            .await
            .expect("first complete backfill");
        assert_eq!(first.providers[0].completed_sessions, 1);
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 1);
        let second = complete_backfill(&state, request, &cancellation, None)
            .await
            .expect("second complete backfill");
        assert_eq!(second.providers[0].completed_sessions, 1);
        assert_eq!(
            APPLY_BATCH_CALLS.load(Ordering::SeqCst),
            1,
            "fresh provider coverage must suppress duplicate apply calls"
        );
    }

    #[tokio::test]
    async fn published_checkpoint_recovers_when_first_coordinator_is_interrupted() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        STATEFUL_CHECKPOINTS
            .lock()
            .expect("stateful checkpoints")
            .clear();
        STATEFUL_APPLY_PUBLISHED.store(false, Ordering::SeqCst);
        *STATEFUL_APPLY_RELEASE
            .0
            .lock()
            .expect("stateful apply release") = false;
        let state = std::sync::Arc::new(state_with_providers(&[(
            FAST_PROVIDER_ID,
            TestProviderBehavior::Stateful,
        )]));
        let workspace = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("publication crash window".to_owned()),
                workspace.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "publish before response".to_owned(), 0)
            .await
            .expect("append event");
        let request = bcode_session_search::CompleteSessionSearchBackfillRequest {
            provider_id: Some(FAST_PROVIDER_ID.to_owned()),
            session_ids: BTreeSet::from([session.id]),
            after_timestamp_ms: None,
            before_timestamp_ms: None,
            slice_deadline_ms: 5_000,
        };
        let first_state = std::sync::Arc::clone(&state);
        let first_request = request.clone();
        let first = tokio::spawn(async move {
            complete_backfill(
                &first_state,
                first_request,
                &bcode_plugin_sdk::ServiceCancellation::default(),
                None,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !STATEFUL_APPLY_PUBLISHED.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider publishes checkpoint");
        first.abort();
        *STATEFUL_APPLY_RELEASE
            .0
            .lock()
            .expect("stateful apply release") = true;
        STATEFUL_APPLY_RELEASE.1.notify_all();
        let _ = first.await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);

        let recovered = complete_backfill(
            &state,
            request,
            &bcode_plugin_sdk::ServiceCancellation::default(),
            None,
        )
        .await
        .expect("checkpoint recovery");
        assert_eq!(recovered.providers[0].completed_sessions, 1);
        assert_eq!(
            APPLY_BATCH_CALLS.load(Ordering::SeqCst),
            0,
            "recovery must trust fresh provider coverage rather than redeliver"
        );
    }

    #[tokio::test]
    async fn concurrent_canonical_append_forces_complete_backfill_convergence() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        STATEFUL_CHECKPOINTS
            .lock()
            .expect("stateful checkpoints")
            .clear();
        STATEFUL_APPLY_PUBLISHED.store(false, Ordering::SeqCst);
        *STATEFUL_APPLY_RELEASE
            .0
            .lock()
            .expect("stateful apply release") = false;
        let state = std::sync::Arc::new(state_with_providers(&[(
            FAST_PROVIDER_ID,
            TestProviderBehavior::Stateful,
        )]));
        let workspace = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("concurrent convergence".to_owned()),
                workspace.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "first tail".to_owned(), 0)
            .await
            .expect("first append");
        state.session_catalog.refresh_native_now(&state).await;
        let revision_started = state.session_catalog.revision();

        let task_state = std::sync::Arc::clone(&state);
        let operation = tokio::spawn(async move {
            complete_backfill(
                &task_state,
                bcode_session_search::CompleteSessionSearchBackfillRequest {
                    provider_id: Some(FAST_PROVIDER_ID.to_owned()),
                    session_ids: BTreeSet::from([session.id]),
                    after_timestamp_ms: None,
                    before_timestamp_ms: None,
                    slice_deadline_ms: 5_000,
                },
                &bcode_plugin_sdk::ServiceCancellation::default(),
                None,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !STATEFUL_APPLY_PUBLISHED.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first provider publication");

        state
            .sessions
            .append_context_compacted(session.id, "concurrent tail".to_owned(), 0)
            .await
            .expect("concurrent append");
        let updated = state
            .sessions
            .session_summary(session.id)
            .await
            .expect("updated summary");
        state.session_catalog.upsert_native_session(updated).await;
        assert!(state.session_catalog.revision() > revision_started);
        *STATEFUL_APPLY_RELEASE
            .0
            .lock()
            .expect("stateful apply release") = true;
        STATEFUL_APPLY_RELEASE.1.notify_all();

        let response = operation
            .await
            .expect("operation task")
            .expect("converged complete backfill");
        assert!(response.convergence_passes >= 2);
        assert_eq!(response.providers[0].completed_sessions, 1);
        assert_eq!(response.providers[0].failed_sessions, 0);
        let canonical_tail = canonical_session_tail(&state, session.id)
            .await
            .expect("canonical tail")
            .expect("tail exists");
        let checkpoint = STATEFUL_CHECKPOINTS
            .lock()
            .expect("stateful checkpoints")
            .get(FAST_PROVIDER_ID)
            .cloned()
            .expect("provider checkpoint");
        assert_eq!(checkpoint.indexed_through_sequence, canonical_tail);
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reinvocation_after_operation_loss_completes_from_provider_checkpoint() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        STATEFUL_CHECKPOINTS
            .lock()
            .expect("stateful checkpoints")
            .clear();
        STATEFUL_APPLY_PUBLISHED.store(false, Ordering::SeqCst);
        *STATEFUL_APPLY_RELEASE
            .0
            .lock()
            .expect("stateful apply release") = false;
        let first_state = std::sync::Arc::new(state_with_providers(&[(
            FAST_PROVIDER_ID,
            TestProviderBehavior::Stateful,
        )]));
        let workspace = tempfile::tempdir().expect("workspace");
        let session = first_state
            .sessions
            .create_session(
                Some("operation loss recovery".to_owned()),
                workspace.path().to_path_buf(),
            )
            .await
            .expect("create session");
        first_state
            .sessions
            .append_context_compacted(session.id, "published before restart".to_owned(), 0)
            .await
            .expect("append event");
        let request = bcode_session_search::CompleteSessionSearchBackfillRequest {
            provider_id: Some(FAST_PROVIDER_ID.to_owned()),
            session_ids: BTreeSet::from([session.id]),
            after_timestamp_ms: None,
            before_timestamp_ms: None,
            slice_deadline_ms: 5_000,
        };
        let task_state = std::sync::Arc::clone(&first_state);
        let task_request = request.clone();
        let interrupted = tokio::spawn(async move {
            complete_backfill(
                &task_state,
                task_request,
                &bcode_plugin_sdk::ServiceCancellation::default(),
                None,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !STATEFUL_APPLY_PUBLISHED.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("provider checkpoint publication");
        interrupted.abort();
        *STATEFUL_APPLY_RELEASE
            .0
            .lock()
            .expect("stateful apply release") = true;
        STATEFUL_APPLY_RELEASE.1.notify_all();
        let _ = interrupted.await;

        let sessions = first_state.sessions.clone();
        drop(first_state);
        let mut restarted =
            state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Stateful)]);
        restarted.sessions = sessions;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let response = complete_backfill(
            &restarted,
            request,
            &bcode_plugin_sdk::ServiceCancellation::default(),
            None,
        )
        .await
        .expect("new invocation after operation loss");

        assert_eq!(response.providers[0].completed_sessions, 1);
        assert_eq!(response.providers[0].failed_sessions, 0);
        assert_eq!(
            APPLY_BATCH_CALLS.load(Ordering::SeqCst),
            0,
            "new operation must continue from the durable provider checkpoint"
        );
    }

    #[tokio::test]
    async fn complete_backfill_scopes_one_provider_and_honors_precancellation() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[
            (FAST_PROVIDER_ID, TestProviderBehavior::Fast),
            (SLOW_PROVIDER_ID, TestProviderBehavior::Fast),
        ]);
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        cancellation.cancel();
        let response = complete_backfill(
            &state,
            bcode_session_search::CompleteSessionSearchBackfillRequest {
                provider_id: Some(FAST_PROVIDER_ID.to_owned()),
                session_ids: BTreeSet::new(),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                slice_deadline_ms: 5_000,
            },
            &cancellation,
            None,
        )
        .await
        .expect("cancelled complete backfill");

        assert_eq!(response.provider_ids, vec![FAST_PROVIDER_ID.to_owned()]);
        assert!(response.cancelled);
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn backfill_deadline_cancels_inflight_provider_ingestion() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        SLOW_APPLY_STARTED.store(false, Ordering::SeqCst);
        SLOW_APPLY_CANCELLED.store(false, Ordering::SeqCst);
        let state = state_with_providers(&[(SLOW_PROVIDER_ID, TestProviderBehavior::Slow)]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("cancel backfill".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "cancel me".to_owned(), 0)
            .await
            .expect("append event");

        let response = backfill_provider(
            &state,
            BackfillSessionSearchRequest {
                provider_id: SLOW_PROVIDER_ID.to_owned(),
                session_ids: BTreeSet::from([session.id]),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                cursor: None,
                deadline_ms: 2_000,
            },
        )
        .await
        .expect("backfill returns terminal cancellation progress");

        assert!(SLOW_APPLY_STARTED.load(Ordering::SeqCst));
        assert!(SLOW_APPLY_CANCELLED.load(Ordering::SeqCst));
        assert_eq!(response.failed_sessions, 0);
        assert_eq!(response.incomplete_sessions, 1);
        assert!(response.deadline_reached);
        assert_eq!(
            response.sessions[0].outcome,
            SessionSearchBackfillOutcome::Incomplete
        );
        assert!(response.sessions[0].error.is_none());
    }

    #[tokio::test]
    async fn backfill_caller_cancellation_cancels_inflight_provider_ingestion() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        SLOW_APPLY_STARTED.store(false, Ordering::SeqCst);
        SLOW_APPLY_CANCELLED.store(false, Ordering::SeqCst);
        let state = state_with_providers(&[(SLOW_PROVIDER_ID, TestProviderBehavior::Slow)]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("caller cancel backfill".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "cancel me".to_owned(), 0)
            .await
            .expect("append event");
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let cancel_signal = cancellation.clone();
        let cancel_task = tokio::spawn(async move {
            while !SLOW_APPLY_STARTED.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            cancel_signal.cancel();
        });

        let response = backfill_provider_with_cancellation(
            &state,
            BackfillSessionSearchRequest {
                provider_id: SLOW_PROVIDER_ID.to_owned(),
                session_ids: BTreeSet::from([session.id]),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                cursor: None,
                deadline_ms: 5_000,
            },
            Some(&cancellation),
        )
        .await
        .expect("backfill returns terminal caller-cancellation progress");
        cancel_task.await.expect("cancellation task");

        assert!(SLOW_APPLY_CANCELLED.load(Ordering::SeqCst));
        assert_eq!(response.failed_sessions, 1);
        assert!(!response.deadline_reached);
        let error = response.sessions[0].error.as_ref().expect("cancel error");
        assert!(error.retryable);
    }

    #[tokio::test]
    async fn provider_failure_modes_never_report_false_complete_coverage() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        for (behavior, expected) in [
            (TestProviderBehavior::QuotaApply, "quota_exceeded"),
            (TestProviderBehavior::CorruptApply, "corrupt_index"),
            (TestProviderBehavior::StaleApply, "stale_generation"),
            (TestProviderBehavior::IncompatiblePolicy, "content_disabled"),
            (
                TestProviderBehavior::UnavailableApply,
                "plugin service invocation failed",
            ),
        ] {
            let state = state_with_providers(&[(FAST_PROVIDER_ID, behavior)]);
            let workspace = tempfile::tempdir().expect("workspace");
            let session = state
                .sessions
                .create_session(
                    Some(format!("failure {expected}")),
                    workspace.path().to_path_buf(),
                )
                .await
                .expect("create session");
            state
                .sessions
                .append_context_compacted(session.id, "must not be complete".to_owned(), 0)
                .await
                .expect("append event");

            let response = complete_backfill(
                &state,
                bcode_session_search::CompleteSessionSearchBackfillRequest {
                    provider_id: Some(FAST_PROVIDER_ID.to_owned()),
                    session_ids: BTreeSet::from([session.id]),
                    after_timestamp_ms: None,
                    before_timestamp_ms: None,
                    slice_deadline_ms: 5_000,
                },
                &bcode_plugin_sdk::ServiceCancellation::default(),
                None,
            )
            .await
            .expect("failure remains a typed aggregate result");
            let provider = &response.providers[0];
            assert_eq!(provider.completed_sessions, 0, "{expected}");
            assert_eq!(provider.incomplete_sessions, 0, "{expected}");
            assert_eq!(provider.failed_sessions, 1, "{expected}");
            assert!(
                provider.error.is_none(),
                "session-local failure is retained"
            );
        }
    }

    #[tokio::test]
    async fn ingestion_rejects_checkpoint_ahead_of_canonical_tail_after_truncation() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("truncated canonical tail".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        let summary = state
            .sessions
            .session_summary(session.id)
            .await
            .expect("read session summary");
        let canonical_tail = canonical_session_tail(&state, session.id)
            .await
            .expect("read canonical tail")
            .expect("created session has canonical events");
        let mut provider = list_providers(&state)
            .await
            .providers
            .into_iter()
            .find(|provider| provider.plugin_id == FAST_PROVIDER_ID)
            .expect("fast provider");
        provider
            .status
            .coverage
            .push(bcode_session_search::SessionSearchCoverage {
                generation: SearchCanonicalGeneration {
                    session_id: session.id,
                    fingerprint: canonical_generation_fingerprint(&summary),
                    last_sequence: Some(canonical_tail),
                },
                content_kinds: BTreeSet::from([SearchContentKind::UserMessage]),
                indexed_through_sequence: Some(canonical_tail.saturating_add(1)),
                complete: true,
                indexed_text_bytes: 1,
                skipped_records: 0,
                truncated_records: 0,
                exclusions: Vec::new(),
            });

        let error = ingest_provider_pages(
            &state, session.id, &summary, &provider, 1, None, false, None,
        )
        .await
        .expect_err("checkpoint ahead of canonical tail must fail closed");

        assert!(!error.retryable_error());
        assert!(
            error
                .message
                .contains("ahead of the canonical session tail")
        );
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn explicit_backfill_reports_released_historical_session_as_migration_required() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let workspace = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("released historical backfill".to_owned()),
                workspace.path().to_path_buf(),
            )
            .await
            .expect("create session");
        let root = state
            .sessions
            .session_store_root()
            .expect("persistent session root");
        let db = bcode_session::db::SessionDb::open_existing_turso_in_root(session.id, &root)
            .await
            .expect("open session DB");
        let source_writer_epoch = *bcode_session_migration::RELEASED_HISTORICAL_WRITER_EPOCHS
            .last()
            .expect("released writer inventory");
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::DatabaseValue::Int64(i64::from(source_writer_epoch)),
            )
            .where_eq("contract_id", switchy::database::DatabaseValue::Int32(1))
            .execute(db.database())
            .await
            .expect("mark released historical writer");
        drop(db);

        let response = backfill_provider(
            &state,
            BackfillSessionSearchRequest {
                provider_id: FAST_PROVIDER_ID.to_owned(),
                session_ids: BTreeSet::from([session.id]),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                cursor: None,
                deadline_ms: 5_000,
            },
        )
        .await
        .expect("backfill returns categorized historical result");

        assert_eq!(response.failed_sessions, 1);
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 0);
        let error = response.sessions[0]
            .error
            .as_ref()
            .expect("migration-required error");
        assert_eq!(error.code, SearchErrorCode::MigrationRequired);
        assert!(!error.retryable);
        assert!(error.message.contains("explicit migration"));
    }

    #[tokio::test]
    async fn explicit_backfill_refuses_to_overlap_active_canonical_migration() {
        use bcode_session_models::{
            SessionMigrationProgress, SessionMigrationStage, SessionOpenOperationId,
            SessionOpenOperationSnapshot, SessionOpenTerminalOutcome,
        };
        use std::sync::Arc;

        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("migration overlap".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        state
            .sessions
            .append_context_compacted(session.id, "must not index yet".to_owned(), 0)
            .await
            .expect("append event");

        let blocker = Arc::new(tokio::sync::Notify::new());
        let task_blocker = Arc::clone(&blocker);
        let operation = state
            .session_migrations
            .operations()
            .start_or_join(
                SessionOpenOperationSnapshot {
                    operation_id: SessionOpenOperationId::new(),
                    revision: 0,
                    session_id: session.id,
                    source_writer_epoch: Some(5),
                    target_writer_epoch: 6,
                    progress: SessionMigrationProgress {
                        stage: SessionMigrationStage::WaitingForOwnership,
                        completed_units: None,
                        total_units: None,
                        unit: None,
                        message: "migration active".to_owned(),
                    },
                    outcome: None,
                    backup_path: None,
                },
                move |_| async move {
                    task_blocker.notified().await;
                    SessionOpenTerminalOutcome::Ready
                },
            )
            .await;
        assert!(state.session_migrations.is_active(session.id).await);

        let response = backfill_provider(
            &state,
            BackfillSessionSearchRequest {
                provider_id: FAST_PROVIDER_ID.to_owned(),
                session_ids: BTreeSet::from([session.id]),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                cursor: None,
                deadline_ms: 5_000,
            },
        )
        .await
        .expect("backfill returns explicit per-session overlap result");

        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(response.failed_sessions, 1);
        assert_eq!(response.sessions.len(), 1);
        assert_eq!(
            response.sessions[0].outcome,
            SessionSearchBackfillOutcome::Failed
        );
        let error = response.sessions[0].error.as_ref().expect("overlap error");
        assert_eq!(error.code, SearchErrorCode::ProviderUnavailable);
        assert!(error.retryable);
        assert!(error.message.contains("migration is active"));

        blocker.notify_one();
        let mut terminal = operation.subscribe();
        terminal
            .wait_for(|snapshot| snapshot.outcome.is_some())
            .await
            .expect("migration reaches terminal outcome");
    }

    #[tokio::test]
    async fn explicit_backfill_refuses_to_overlap_active_canonical_repair_maintenance() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("repair overlap".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        let root = state
            .sessions
            .session_store_root()
            .expect("persistent session root");
        state
            .sessions
            .release_session_ownership(session.id)
            .await
            .expect("release runtime ownership");
        let maintenance =
            bcode_session::lease::acquire_session_maintenance_guard(&root, session.id)
                .expect("active repair maintenance guard");

        let response = backfill_provider(
            &state,
            BackfillSessionSearchRequest {
                provider_id: FAST_PROVIDER_ID.to_owned(),
                session_ids: BTreeSet::from([session.id]),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                cursor: None,
                deadline_ms: 5_000,
            },
        )
        .await
        .expect("backfill returns explicit per-session repair overlap result");

        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(response.failed_sessions, 1);
        let error = response.sessions[0].error.as_ref().expect("overlap error");
        assert!(error.retryable);
        assert!(error.message.contains("maintenance") || error.message.contains("owned"));
        drop(maintenance);
    }

    #[tokio::test]
    async fn explicit_backfill_indexes_only_selected_sessions_with_bounded_progress() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let selected = state
            .sessions
            .create_session(
                Some("selected backfill".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create selected session");
        let omitted = state
            .sessions
            .create_session(
                Some("omitted backfill".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create omitted session");
        state
            .sessions
            .append_context_compacted(selected.id, "selected content".to_owned(), 0)
            .await
            .expect("append selected event");
        state
            .sessions
            .append_context_compacted(omitted.id, "omitted content".to_owned(), 0)
            .await
            .expect("append omitted event");

        let response = backfill_provider(
            &state,
            BackfillSessionSearchRequest {
                provider_id: FAST_PROVIDER_ID.to_owned(),
                session_ids: std::iter::once(selected.id).collect(),
                after_timestamp_ms: None,
                before_timestamp_ms: None,
                cursor: None,
                deadline_ms: 5_000,
            },
        )
        .await
        .expect("selected backfill");

        assert_eq!(response.selected_sessions, 1);
        assert_eq!(response.sessions.len(), 1);
        assert_eq!(response.sessions[0].session_id, selected.id);
        assert_eq!(response.completed_sessions, 1);
        assert_eq!(response.failed_sessions, 0);
        assert!(!response.deadline_reached);
        assert!(!response.selection_truncated);
        assert!(response.next_cursor.is_none());
        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[ignore = "manual daemon ingestion-lag performance baseline"]
    async fn benchmark_daemon_incremental_ingestion_lag() {
        const DEFAULT_SESSIONS: usize = 8;
        const DEFAULT_EVENTS: usize = 64;
        const MAX_SESSIONS: usize = 32;
        const MAX_EVENTS: usize = 512;

        let sessions = std::env::var("BCODE_SESSION_SEARCH_INGESTION_SESSIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_SESSIONS);
        let events = std::env::var("BCODE_SESSION_SEARCH_INGESTION_EVENTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_EVENTS);
        assert!((1..=MAX_SESSIONS).contains(&sessions));
        assert!((1..=MAX_EVENTS).contains(&events));

        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let workspace = tempfile::tempdir().expect("workspace");
        let mut session_ids = Vec::with_capacity(sessions);
        for session_index in 0..sessions {
            let session = state
                .sessions
                .create_session(
                    Some(format!("ingestion benchmark {session_index}")),
                    workspace.path().to_path_buf(),
                )
                .await
                .expect("create session");
            for event_index in 0..events {
                state
                    .sessions
                    .append_context_compacted(
                        session.id,
                        format!("benchmark session {session_index} event {event_index}"),
                        0,
                    )
                    .await
                    .expect("append event");
            }
            session_ids.push(session.id);
        }

        let started = Instant::now();
        for session_id in &session_ids {
            state.session_search_dirty.mark_committed(*session_id).await;
        }
        process_dirty_sessions(&state).await;
        let elapsed = started.elapsed();
        let pending = state.session_search_dirty.snapshot().await.0;
        assert!(pending.is_empty());
        let calls = APPLY_BATCH_CALLS.load(Ordering::SeqCst);
        assert!(calls >= sessions);
        println!(
            "session_search_daemon_ingestion_benchmark sessions={sessions} events_per_session={events} total_events={} apply_calls={calls} ingestion_lag_us={} events_per_second={}",
            sessions * events,
            elapsed.as_micros(),
            (sessions * events) as u128 * 1_000_000 / elapsed.as_micros().max(1)
        );
    }

    #[tokio::test]
    async fn incremental_ingestion_drains_multiple_bounded_pages() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        APPLY_BATCH_CALLS.store(0, Ordering::SeqCst);
        let state = state_with_providers(&[(FAST_PROVIDER_ID, TestProviderBehavior::Fast)]);
        let working_directory = tempfile::tempdir().expect("workspace");
        let session = state
            .sessions
            .create_session(
                Some("multi-page ingestion".to_owned()),
                working_directory.path().to_path_buf(),
            )
            .await
            .expect("create session");
        for sequence in 0..300 {
            state
                .sessions
                .append_context_compacted(session.id, format!("summary {sequence}"), 0)
                .await
                .expect("append event");
        }
        state.session_search_dirty.mark_committed(session.id).await;

        process_dirty_sessions(&state).await;

        assert_eq!(APPLY_BATCH_CALLS.load(Ordering::SeqCst), 2);
        assert!(state.session_search_dirty.snapshot().await.0.is_empty());
    }

    #[tokio::test]
    async fn dirty_session_queue_coalesces_and_bounds_committed_notifications() {
        let queue = SessionSearchDirtyQueue::default();
        let first = SessionId::new();
        queue.mark_committed(first).await;
        queue.mark_committed(first).await;
        let (sessions, rescan_required) = queue.snapshot().await;
        assert_eq!(sessions, vec![first]);
        assert!(!rescan_required);

        for _ in 1..MAX_DIRTY_SESSION_SEARCH_SESSIONS {
            queue.mark_committed(SessionId::new()).await;
        }
        queue.mark_committed(SessionId::new()).await;
        let (sessions, rescan_required) = queue.snapshot().await;
        assert_eq!(sessions.len(), MAX_DIRTY_SESSION_SEARCH_SESSIONS);
        assert!(rescan_required);
    }

    #[tokio::test]
    async fn dirty_session_queue_stays_empty_without_committed_mutations() {
        let queue = SessionSearchDirtyQueue::default();
        let (sessions, rescan_required) = queue.snapshot().await;
        assert!(sessions.is_empty());
        assert!(!rescan_required);
    }

    #[tokio::test]
    async fn all_search_disabled_preserves_complete_session_investigation_boundary() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        let root = tempfile::tempdir().expect("session root");
        let sessions = bcode_session::SessionManager::persistent(root.path()).expect("sessions");
        let state = crate::tests::test_server_state_with_plugins(
            sessions,
            bcode_plugin::PluginHost::default().into(),
        );
        let working_directory = root.path().join("workspace");
        std::fs::create_dir(&working_directory).expect("working directory");

        let inventory = list_providers(&state).await;
        assert!(inventory.providers.is_empty());
        assert!(inventory.failures.is_empty());

        let session = state
            .sessions
            .create_session(
                Some("disabled search".to_owned()),
                working_directory.clone(),
            )
            .await
            .expect("create session");
        let appended = state
            .sessions
            .append_context_compacted(session.id, "bounded summary".to_owned(), 0)
            .await
            .expect("append event");
        assert_eq!(appended.session_id, session.id);

        let listed = state.sessions.list_sessions(&working_directory).await;
        assert!(listed.iter().any(|summary| summary.id == session.id));
        let opened = state
            .sessions
            .prepare_session_open(session.id)
            .await
            .expect("prepare open");
        assert_eq!(
            opened.outcome,
            Some(bcode_session_models::SessionOpenTerminalOutcome::Ready)
        );

        let attached = state
            .sessions
            .attach_session_recent(session.id, bcode_session_models::ClientId::new(), 10)
            .await
            .expect("attach recent");
        assert!(
            attached
                .history
                .iter()
                .any(|event| event.sequence == appended.sequence)
        );
        let page = state
            .sessions
            .session_history_page(
                session.id,
                bcode_session_models::SessionHistoryQuery {
                    cursor: None,
                    limit: 10,
                    direction: bcode_session_models::SessionHistoryDirection::Backward,
                },
            )
            .await
            .expect("history page");
        assert!(
            page.events
                .iter()
                .any(|event| event.sequence == appended.sequence)
        );

        let exported = state
            .sessions
            .session_history(session.id)
            .await
            .expect("explicit export history");
        assert!(
            exported
                .iter()
                .any(|event| event.sequence == appended.sequence)
        );
        let inspection = state
            .sessions
            .session_inspection_page(
                session.id,
                bcode_session_models::SessionInspectionQuery {
                    category: bcode_session_models::SessionInspectionCategory::Compactions,
                    cursor: None,
                    limit: 10,
                    direction: bcode_session_models::SessionHistoryDirection::Backward,
                },
            )
            .await
            .expect("structured inspection");
        assert!(
            inspection
                .events
                .iter()
                .any(|event| event.sequence == appended.sequence)
        );
    }

    #[tokio::test]
    async fn future_provider_status_is_isolated_as_incompatible() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        let state = state_with_providers(&[
            (FAST_PROVIDER_ID, TestProviderBehavior::Fast),
            (FUTURE_PROVIDER_ID, TestProviderBehavior::FutureStatus),
        ]);
        let inventory = list_providers(&state).await;
        assert_eq!(inventory.providers.len(), 1);
        assert_eq!(inventory.providers[0].plugin_id, FAST_PROVIDER_ID);
        assert_eq!(inventory.failures.len(), 1);
        assert_eq!(inventory.failures[0].plugin_id, FUTURE_PROVIDER_ID);
        assert_eq!(
            inventory.failures[0].error.code,
            SearchErrorCode::FutureVersion
        );
        assert!(!inventory.failures[0].error.retryable);
    }

    #[tokio::test]
    async fn future_capability_enum_is_isolated_as_incompatible() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        let state = state_with_providers(&[
            (FAST_PROVIDER_ID, TestProviderBehavior::Fast),
            (
                FUTURE_CAPABILITY_PROVIDER_ID,
                TestProviderBehavior::FutureCapability,
            ),
        ]);
        let inventory = list_providers(&state).await;
        assert_eq!(inventory.providers.len(), 1);
        assert_eq!(inventory.providers[0].plugin_id, FAST_PROVIDER_ID);
        assert_eq!(inventory.failures.len(), 1);
        assert_eq!(
            inventory.failures[0].plugin_id,
            FUTURE_CAPABILITY_PROVIDER_ID
        );
        assert_eq!(
            inventory.failures[0].error.code,
            SearchErrorCode::FutureVersion
        );
        assert!(!inventory.failures[0].error.retryable);
    }

    #[tokio::test]
    async fn unavailable_provider_service_is_isolated_without_losing_healthy_results() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        let state = state_with_providers(&[
            (FAST_PROVIDER_ID, TestProviderBehavior::Fast),
            (CRASH_PROVIDER_ID, TestProviderBehavior::Crash),
        ]);
        let routes = vec![SessionSearchContentRoute {
            content_kinds: BTreeSet::from([SearchContentKind::UserMessage]),
            mode: SessionSearchRouteMode::Parallel,
            provider_ids: vec![FAST_PROVIDER_ID.to_owned(), CRASH_PROVIDER_ID.to_owned()],
        }];
        let response = search_federated_with_policy_and_routes(
            &state,
            &request(),
            &SessionSearchPlanPolicy {
                per_provider_deadline_ms: 100,
                ..SessionSearchPlanPolicy::default()
            },
            &routes,
        )
        .await
        .expect("federated search");
        assert_eq!(response.hits.len(), 1);
        assert_eq!(response.hits[0].provider_id, FAST_PROVIDER_ID);
        assert!(response.failures.iter().any(|failure| {
            failure.plugin_id == CRASH_PROVIDER_ID
                && failure.error.code == SearchErrorCode::ProviderUnavailable
                && failure.error.retryable
        }));
        assert!(!response.query_complete);
        assert!(!response.coverage_complete);
    }

    #[tokio::test]
    async fn timed_out_provider_cannot_reopen_terminal_federated_outcome() {
        let _guard = SEARCH_TEST_LOCK.lock().await;
        reset_slow_provider_state();
        let state = state_with_providers(&[(SLOW_PROVIDER_ID, TestProviderBehavior::Slow)]);
        let mut request = request();
        request.deadline_ms = Some(100);
        let routes = vec![SessionSearchContentRoute {
            content_kinds: BTreeSet::from([SearchContentKind::UserMessage]),
            mode: SessionSearchRouteMode::Primary,
            provider_ids: vec![SLOW_PROVIDER_ID.to_owned()],
        }];
        let response = search_federated_with_policy_and_routes(
            &state,
            &request,
            &SessionSearchPlanPolicy {
                per_provider_deadline_ms: 25,
                ..SessionSearchPlanPolicy::default()
            },
            &routes,
        )
        .await
        .expect("federated search");
        let terminal = serde_json::to_vec(&response).expect("terminal response encodes");

        wait_for_slow_provider_to_finish().await;

        assert!(SLOW_SEARCH_CANCELLED.load(Ordering::SeqCst));
        assert!(SLOW_SEARCH_FINISHED.load(Ordering::SeqCst));
        assert_eq!(
            serde_json::to_vec(&response).expect("terminal response re-encodes"),
            terminal
        );
        assert!(response.hits.is_empty());
        assert_eq!(response.providers.len(), 0);
        assert_eq!(response.failures.len(), 1);
        assert_eq!(
            response.failures[0].error.code,
            SearchErrorCode::DeadlineExceeded
        );
        assert!(!response.query_complete);
        assert!(!response.coverage_complete);
    }

    #[test]
    fn provider_errors_are_secret_safe_and_utf8_safely_bounded() {
        let secret = "sk-session-search-secret";
        let message = format!(
            "Authorization: Bearer {secret} api_key=query-secret {}",
            "é".repeat(5_000)
        );
        let bounded = bounded_message(&message);
        assert!(!bounded.contains(secret));
        assert!(!bounded.contains("query-secret"));
        assert!(bounded.contains("[REDACTED]"));
        assert!(bounded.chars().count() <= 4_110);
        assert!(bounded.ends_with("…[TRUNCATED]"));
    }
}
