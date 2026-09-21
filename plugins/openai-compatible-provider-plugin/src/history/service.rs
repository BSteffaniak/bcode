//! Provider-owned bounded history operation; no persistence or profile fallback.
use bcode_model::ProviderRequestContext;
use bcode_plugin_sdk::{NativeServiceContext, ServiceResponse};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct LoadRequest {
    schema_version: u32,
    provider_context: ProviderRequestContext,
    conversation_id: String,
}

#[derive(Serialize)]
struct LoadResponse {
    schema_version: u32,
    revision_id: String,
    events: Vec<bcode_session_import::ImportableSessionEvent>,
    warnings: Vec<bcode_session_import::ImportWarning>,
}

pub async fn load(context: NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context.request.payload_json::<LoadRequest>() else {
        return ServiceResponse::error("history_invalid_request", "invalid history request");
    };
    if request.schema_version != 1 {
        return ServiceResponse::error(
            "history_unsupported_version",
            "unsupported history request version",
        );
    }
    let selection = &request.provider_context;
    let Some(auth) = &selection.auth else {
        return ServiceResponse::error(
            "history_auth_required",
            "resolve the selected auth profile first",
        );
    };
    if selection.auth_pool.is_some()
        || !selection.auth_candidates.is_empty()
        || selection.auth_profile.as_deref().is_none_or(str::is_empty)
        || selection.auth_profile != auth.profile
    {
        return ServiceResponse::error(
            "history_profile_required",
            "history requires one explicitly resolved profile",
        );
    }
    let Ok(client) = super::client::HistoryClient::new(16 * 1024 * 1024) else {
        return ServiceResponse::error("history_client_unavailable", "history client unavailable");
    };
    match client
        .conversation_for_auth(auth, &request.conversation_id, &context.cancellation)
        .await
    {
        Ok(snapshot) => {
            let Ok(revision_id) = snapshot.revision_id() else {
                return ServiceResponse::error(
                    "history_revision_failed",
                    "history revision could not be encoded",
                );
            };
            crate::json_response(&LoadResponse {
                schema_version: 1,
                revision_id,
                events: snapshot.import_events(),
                warnings: snapshot.import_warnings(),
            })
        }
        Err(error) => access_error(error),
    }
}

fn access_error(error: super::client::HistoryAccessError) -> ServiceResponse {
    use super::client::HistoryAccessError;
    let (code, message) = match error {
        HistoryAccessError::Cancelled => ("history_cancelled", "history retrieval cancelled"),
        HistoryAccessError::InvalidRequest => (
            "history_invalid_request",
            "invalid history identifier or budget",
        ),
        HistoryAccessError::AuthenticationRequired => (
            "history_auth_required",
            "refresh or reconnect the selected auth profile",
        ),
        HistoryAccessError::AccessDenied => (
            "history_access_denied",
            "the selected profile cannot access this history",
        ),
        HistoryAccessError::AccessChallenge => (
            "history_access_challenge",
            "upstream access challenge prevents history retrieval",
        ),
        HistoryAccessError::RateLimited { .. } => (
            "history_rate_limited",
            "upstream rate limit; retry history retrieval later",
        ),
        HistoryAccessError::NotFound => (
            "history_not_found",
            "remote conversation unavailable; retain previously imported history",
        ),
        HistoryAccessError::Transient => (
            "history_transient",
            "temporary history transport or server failure",
        ),
        HistoryAccessError::IncompatibleResponse => (
            "history_incompatible_response",
            "unsupported history response; import remains incomplete",
        ),
        HistoryAccessError::TooLarge => (
            "history_too_large",
            "conversation exceeds the retrieval budget; import remains incomplete",
        ),
        HistoryAccessError::Decode(_) => (
            "history_decode_failed",
            "conversation could not be converted faithfully; import remains incomplete",
        ),
    };
    ServiceResponse::error(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::client::HistoryAccessError;

    #[test]
    fn retrieval_failures_keep_actionable_categories() {
        let failures = [
            (HistoryAccessError::Cancelled, "history_cancelled"),
            (
                HistoryAccessError::AuthenticationRequired,
                "history_auth_required",
            ),
            (HistoryAccessError::AccessDenied, "history_access_denied"),
            (
                HistoryAccessError::AccessChallenge,
                "history_access_challenge",
            ),
            (
                HistoryAccessError::RateLimited {
                    retry_after_seconds: Some(17),
                },
                "history_rate_limited",
            ),
            (HistoryAccessError::NotFound, "history_not_found"),
            (HistoryAccessError::Transient, "history_transient"),
            (
                HistoryAccessError::IncompatibleResponse,
                "history_incompatible_response",
            ),
            (HistoryAccessError::TooLarge, "history_too_large"),
        ];
        for (failure, code) in failures {
            let response = access_error(failure);
            let error = response.error.unwrap();
            assert_eq!(error.code, code);
            assert!(!error.message.is_empty());
            assert!(response.payload.is_empty());
        }
    }
}
