//! Session-search provider discovery and typed service routing.

use bcode_session_search::{
    FederatedProviderContribution, FederatedProviderReport, FederatedSessionSearchResponse,
    HydratedSessionSearchHit, ListSessionSearchProvidersResponse, MAX_FEDERATED_PROVIDERS,
    OP_CAPABILITIES, OP_SEARCH, OP_STATUS, SESSION_SEARCH_INTERFACE_ID, SearchErrorCode,
    SearchHitHydrationOutcome, SessionSearchCapabilities, SessionSearchContentRoute,
    SessionSearchProviderFailure, SessionSearchProviderInfo, SessionSearchRequest,
    SessionSearchResponse, SessionSearchServiceError, SessionSearchStatus,
    aggregate_federated_search, plan_session_search, plan_session_search_with_routes,
};
use futures::future::join_all;
use std::time::{Duration, Instant};

use crate::ServerState;

/// Discover loaded session-search providers and query their typed capabilities/status.
///
/// Provider-local failures are returned as normalized entries so one malformed or unavailable
/// provider does not conceal healthy providers.
pub async fn list_providers(state: &ServerState) -> ListSessionSearchProvidersResponse {
    let provider_ids = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(SESSION_SEARCH_INTERFACE_ID)
        .cloned()
        .unwrap_or_default();
    let mut providers = Vec::new();
    let mut failures = Vec::new();
    for plugin_id in provider_ids {
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
                failures.push(provider_failure(
                    plugin_id,
                    SearchErrorCode::ProviderUnavailable,
                    &error.to_string(),
                    true,
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
            Err(error) => failures.push(provider_failure(
                plugin_id,
                SearchErrorCode::ProviderUnavailable,
                &error.to_string(),
                true,
            )),
        }
    }
    ListSessionSearchProvidersResponse {
        providers,
        failures,
    }
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
    let response = state
        .plugins
        .invoke_service_json::<_, SessionSearchResponse>(
            plugin_id,
            SESSION_SEARCH_INTERFACE_ID,
            OP_SEARCH,
            request,
        )
        .await
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::ProviderUnavailable,
            message: error.to_string(),
            retryable: true,
        })?;
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
    Ok(response)
}

/// Execute one bounded terminal federated search using deterministic grouped provider ordering.
///
/// Canonical hit hydration is intentionally separate: this function returns provider locators and
/// bounded previews only. Dropping timed-out invocation futures bounds waiting, but does not claim
/// transport-level cancellation or durable resume semantics.
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
    request
        .validate()
        .map_err(|error| SessionSearchServiceError {
            code: SearchErrorCode::InvalidRequest,
            message: bounded_message(&error.to_string()),
            retryable: false,
        })?;
    let discovery = list_providers(state).await;
    let plan = if routes.is_empty() {
        plan_session_search(request, discovery)
    } else {
        plan_session_search_with_routes(request, discovery, routes)
    };
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
        let provider_request = request_for_provider(request, &provider);
        async move {
            let provider_started = Instant::now();
            let remaining = deadline.saturating_sub(started.elapsed());
            let result = if remaining.is_zero() {
                Err(timeout_error())
            } else {
                match tokio::time::timeout(
                    remaining,
                    search_provider(state, &provider.plugin_id, &provider_request),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(timeout_error()),
                }
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
                        searched_content: response.searched_content,
                        excluded_content: response.excluded_content,
                    },
                    hits: response.hits,
                });
            }
            Err(error) => failures.push(SessionSearchProviderFailure {
                plugin_id: provider_id,
                error,
            }),
        }
    }
    Ok(aggregate_federated_search(
        contributions,
        failures,
        request.limit,
    ))
}

/// Hydrate provider locators through exact bounded canonical reads.
///
/// Each hit performs a zero-neighbor around-sequence read. A missing anchor is stale and never
/// substituted with another event. The returned vector preserves grouped hit order.
pub async fn hydrate_hits(
    state: &ServerState,
    hits: Vec<bcode_session_search::SessionSearchHit>,
) -> Vec<HydratedSessionSearchHit> {
    let reads = hits.into_iter().map(|hit| async move {
        let result = state
            .sessions
            .session_history_around(
                hit.locator.session_id,
                bcode_session_models::SessionHistoryAroundQuery {
                    sequence: hit.locator.sequence,
                    before: 0,
                    after: 0,
                },
            )
            .await;
        match result {
            Ok(window) if window.anchor_present => {
                let event = window
                    .events
                    .into_iter()
                    .find(|event| event.sequence == hit.locator.sequence);
                if let Some(event) = event {
                    HydratedSessionSearchHit {
                        hit,
                        outcome: SearchHitHydrationOutcome::Hydrated,
                        event: Some(Box::new(event)),
                        message: None,
                    }
                } else {
                    stale_hydration(hit)
                }
            }
            Ok(_) => stale_hydration(hit),
            Err(error) => hydration_error(hit, &error),
        }
    });
    join_all(reads).await
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
        | bcode_session::SessionError::Db(_)
        | bcode_session::SessionError::DbUnavailable(_) => {
            SearchHitHydrationOutcome::RepairRequired
        }
        bcode_session::SessionError::StorageMigrationRequired { .. }
        | bcode_session::SessionError::Lease(_) => SearchHitHydrationOutcome::Incompatible,
        _ => SearchHitHydrationOutcome::Unavailable,
    };
    HydratedSessionSearchHit {
        hit,
        outcome,
        event: None,
        message: Some(bounded_message(&error.to_string())),
    }
}

fn request_for_provider(
    request: &SessionSearchRequest,
    provider: &SessionSearchProviderInfo,
) -> SessionSearchRequest {
    let mut provider_request = request.clone();
    if !request.filters.content_kinds.is_empty() {
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
    }
}

fn bounded_message(message: &str) -> String {
    const MAX_ERROR_BYTES: usize = 4 * 1024;
    if message.len() <= MAX_ERROR_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_ERROR_BYTES;
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_errors_are_utf8_safely_bounded() {
        let message = "é".repeat(3_000);
        let bounded = bounded_message(&message);
        assert!(bounded.len() <= 4 * 1024 + "…".len());
        assert!(bounded.ends_with('…'));
    }
}
