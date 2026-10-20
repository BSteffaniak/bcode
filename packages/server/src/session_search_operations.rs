//! Transport-neutral application operations for federated session search.

use super::ServerState;

/// Portable search result; compatibility name retained for existing callers.
pub use bcode_session_search::application::ApplicationSearchResult as SearchResult;

/// Execute one federated session search without transport framing.
pub async fn search(
    state: &ServerState,
    request: &bcode_session_search::SessionSearchRequest,
    policy: &bcode_session_search::SessionSearchPlanPolicy,
    routes: &[bcode_session_search::SessionSearchContentRoute],
    hydrate: bool,
) -> Result<SearchResult, bcode_session_search::SessionSearchServiceError> {
    let mut response = super::session_search::search_federated_with_policy_and_routes(
        state, request, policy, routes,
    )
    .await?;
    let hydrated_hits = if hydrate {
        let started = std::time::Instant::now();
        let hydrated = super::session_search::hydrate_hits(state, response.hits.clone()).await;
        super::session_search::report_hydration_outcomes(
            &mut response,
            &hydrated,
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        hydrated
    } else {
        Vec::new()
    };
    Ok(SearchResult {
        response,
        hydrated_hits,
    })
}
