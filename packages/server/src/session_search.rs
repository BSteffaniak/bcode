//! Session-search provider discovery and typed service routing.

use bcode_session_search::{
    FederatedProviderContribution, FederatedProviderReport, FederatedSessionSearchResponse,
    HydratedSessionSearchHit, ListSessionSearchProvidersResponse, MAX_FEDERATED_PROVIDERS,
    OP_CAPABILITIES, OP_SEARCH, OP_STATUS, SESSION_SEARCH_INTERFACE_ID, SearchErrorCode,
    SearchHitHydrationOutcome, SessionSearchCapabilities, SessionSearchContentRoute,
    SessionSearchProviderFailure, SessionSearchProviderInfo, SessionSearchRequest,
    SessionSearchResponse, SessionSearchServiceError, SessionSearchStatus,
    aggregate_federated_search, plan_session_search_with_policy_and_routes,
};
use futures::future::join_all;
use std::time::{Duration, Instant};

use crate::ServerState;

/// Discover loaded session-search providers and query their typed capabilities/status.
///
/// Provider-local failures are returned as normalized entries so one malformed or unavailable
/// provider does not conceal healthy providers.
#[allow(clippy::too_many_lines)]
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
        let provider_request = request_for_provider(request, &provider);
        async move {
            let provider_started = Instant::now();
            let remaining = deadline.saturating_sub(started.elapsed());
            let call_deadline = remaining.min(per_provider_deadline);
            let result = if call_deadline.is_zero() {
                Err(timeout_error())
            } else {
                tokio::time::timeout(
                    call_deadline,
                    search_provider_with_timeout(
                        state,
                        &provider.plugin_id,
                        &provider_request,
                        call_deadline,
                    ),
                )
                .await
                .unwrap_or_else(|_| Err(timeout_error()))
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

fn provider_request_content_for_failure(
    request: &SessionSearchRequest,
) -> Vec<bcode_session_search::SearchContentKind> {
    request.filters.content_kinds.iter().copied().collect()
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
        stage: bcode_session_search::SessionSearchProviderStage::Discovery,
        elapsed_ms: 0,
        content: Vec::new(),
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
    use bcode_session_models::SessionId;
    use bcode_session_search::{
        ProviderSearchOutcome, SearchContentKind, SearchField, SessionSearchHit,
        SessionSearchLocator,
    };

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

    #[test]
    fn provider_errors_are_utf8_safely_bounded() {
        let message = "é".repeat(3_000);
        let bounded = bounded_message(&message);
        assert!(bounded.len() <= 4 * 1024 + "…".len());
        assert!(bounded.ends_with('…'));
    }
}
