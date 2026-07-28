//! Session-search provider discovery and typed service routing.

use bcode_session_search::{
    ListSessionSearchProvidersResponse, OP_CAPABILITIES, OP_SEARCH, OP_STATUS,
    SESSION_SEARCH_INTERFACE_ID, SearchErrorCode, SessionSearchCapabilities,
    SessionSearchProviderFailure, SessionSearchProviderInfo, SessionSearchRequest,
    SessionSearchResponse, SessionSearchServiceError, SessionSearchStatus,
};

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
            Ok(capabilities) if capabilities.provider_id == plugin_id => capabilities,
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
    Ok(response)
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
