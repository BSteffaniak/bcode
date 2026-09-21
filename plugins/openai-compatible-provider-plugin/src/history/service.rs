//! Provider-owned bounded history operation; no persistence or profile fallback.
use bcode_model::history::LoadHistorySnapshotRequest;
use bcode_plugin_sdk::{NativeServiceContext, ServiceResponse};

pub fn capabilities(context: &NativeServiceContext) -> ServiceResponse {
    use bcode_model::history::{
        HistoryCapabilities, HistoryCapabilitiesRequest, HistoryScopeSupport,
    };
    let Ok(request) = context.request.payload_json::<HistoryCapabilitiesRequest>() else {
        return ServiceResponse::error("history_invalid_request", "invalid history request");
    };
    if request.schema_version != 1 {
        return ServiceResponse::error(
            "history_unsupported_version",
            "unsupported history request version",
        );
    }
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("history_cancelled", "history retrieval cancelled");
    }
    crate::json_response(&HistoryCapabilities {
        schema_version: 1,
        auth_schemes: ["chatgpt".to_owned()].into(),
        ordinary: HistoryScopeSupport::Unverified,
        archived: HistoryScopeSupport::Unverified,
        projects: HistoryScopeSupport::Unsupported,
    })
}

pub async fn list(context: NativeServiceContext) -> ServiceResponse {
    use bcode_model::history::{HistoryPageEntry, ListHistoryPageRequest, ListHistoryPageResponse};
    let Ok(request) = context.request.payload_json::<ListHistoryPageRequest>() else {
        return ServiceResponse::error("history_invalid_request", "invalid history request");
    };
    if request.schema_version != 1 {
        return ServiceResponse::error(
            "history_unsupported_version",
            "unsupported history request version",
        );
    }
    let auth = match explicit_auth(&request.provider_context) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let Ok(client) = super::client::HistoryClient::new(16 * 1024 * 1024) else {
        return ServiceResponse::error("history_client_unavailable", "history client unavailable");
    };
    match client
        .list_for_auth(
            auth,
            request.offset,
            request.limit,
            request.archived,
            &context.cancellation,
        )
        .await
    {
        Ok(page) => crate::json_response(&ListHistoryPageResponse {
            schema_version: 1,
            entries: page
                .items
                .into_iter()
                .map(|item| HistoryPageEntry {
                    conversation_id: item.id,
                    title: item.title,
                    updated_at: item.update_time,
                })
                .collect(),
            next_offset: page.next_offset,
        }),
        Err(error) => access_error(error),
    }
}

fn explicit_auth(
    selection: &bcode_model::ProviderRequestContext,
) -> Result<&bcode_model::ProviderAuthContext, ServiceResponse> {
    let Some(auth) = &selection.auth else {
        return Err(ServiceResponse::error(
            "history_auth_required",
            "resolve the selected auth profile first",
        ));
    };
    if selection.auth_pool.is_some()
        || !selection.auth_candidates.is_empty()
        || selection
            .auth_profile
            .as_deref()
            .is_none_or(|profile| profile.trim().is_empty())
        || selection.auth_profile != auth.profile
    {
        return Err(ServiceResponse::error(
            "history_profile_required",
            "history requires one explicitly resolved profile",
        ));
    }
    Ok(auth)
}

pub async fn load(context: NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context.request.payload_json::<LoadHistorySnapshotRequest>() else {
        return ServiceResponse::error("history_invalid_request", "invalid history request");
    };
    if request.schema_version != 1 {
        return ServiceResponse::error(
            "history_unsupported_version",
            "unsupported history request version",
        );
    }
    let auth = match explicit_auth(&request.provider_context) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let Ok(client) = super::client::HistoryClient::new(16 * 1024 * 1024) else {
        return ServiceResponse::error("history_client_unavailable", "history client unavailable");
    };
    match client
        .conversation_branch_for_auth(
            auth,
            &request.conversation_id,
            request.selected_node.as_deref(),
            &context.cancellation,
        )
        .await
    {
        Ok(snapshot) => {
            let Ok(revision_id) = snapshot.revision_id() else {
                return ServiceResponse::error(
                    "history_revision_failed",
                    "history revision could not be encoded",
                );
            };
            crate::json_response(&snapshot.import_snapshot(revision_id))
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
        HistoryAccessError::RefreshRequired => (
            "history_refresh_required",
            "history credentials require provider-owned refresh",
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
        HistoryAccessError::Decode(super::HistoryDecodeError::Incomplete) => (
            "history_incomplete",
            "selected history is still being generated; retry retrieval later",
        ),
        HistoryAccessError::Decode(_) => (
            "history_decode_failed",
            "conversation could not be converted faithfully; import remains incomplete",
        ),
    };
    let mut response = ServiceResponse::error(code, message);
    if let HistoryAccessError::RateLimited {
        retry_after_seconds,
    } = error
    {
        let details = bcode_model::history::HistoryRateLimitDetails {
            schema_version: 1,
            retry_after_seconds,
        };
        // An encoding failure must not convert a failed retrieval into success.
        if let Ok(payload) = serde_json::to_vec(&details) {
            response.payload = payload;
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::client::HistoryAccessError;

    #[test]
    fn explicit_profile_guard_never_substitutes_credentials() {
        use bcode_model::{
            ProviderAuthCandidate, ProviderAuthContext, ProviderAuthCredential,
            ProviderRequestContext,
        };
        let selected = ProviderRequestContext {
            auth_profile: Some("selected".into()),
            auth: Some(ProviderAuthContext {
                profile: Some("selected".into()),
                credentials: [(
                    "access_token".into(),
                    ProviderAuthCredential {
                        value: "selected-secret".into(),
                        ..Default::default()
                    },
                )]
                .into(),
                ..Default::default()
            }),
            env: [("OPENAI_API_KEY".into(), "unrelated-secret".into())].into(),
            ..Default::default()
        };
        let accepted = explicit_auth(&selected).unwrap();
        assert!(std::ptr::eq(accepted, selected.auth.as_ref().unwrap()));
        assert_eq!(
            accepted.credentials["access_token"].value,
            "selected-secret"
        );
        let mut rejected = Vec::new();
        for profile in [None, Some(""), Some(" \t"), Some("other")] {
            let mut context = selected.clone();
            context.auth_profile = profile.map(str::to_owned);
            rejected.push(context);
        }
        let mut pooled = selected.clone();
        pooled.auth_pool = Some("pool".into());
        rejected.push(pooled);
        let mut candidates = selected.clone();
        candidates
            .auth_candidates
            .push(ProviderAuthCandidate::default());
        rejected.push(candidates);
        let mut unresolved = selected;
        unresolved.auth.as_mut().unwrap().profile = None;
        rejected.push(unresolved);
        for context in rejected {
            let response = explicit_auth(&context).unwrap_err();
            let error = response.error.unwrap();
            assert_eq!(error.code, "history_profile_required");
            assert!(!error.message.contains("secret"));
            assert!(response.payload.is_empty());
        }
    }

    #[test]
    fn retrieval_failures_keep_actionable_categories() {
        let failures = [
            (HistoryAccessError::Cancelled, "history_cancelled"),
            (
                HistoryAccessError::AuthenticationRequired,
                "history_auth_required",
            ),
            (
                HistoryAccessError::RefreshRequired,
                "history_refresh_required",
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
            (
                HistoryAccessError::RateLimited {
                    retry_after_seconds: None,
                },
                "history_rate_limited",
            ),
            (HistoryAccessError::NotFound, "history_not_found"),
            (
                HistoryAccessError::Decode(super::super::HistoryDecodeError::Incomplete),
                "history_incomplete",
            ),
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
            if let HistoryAccessError::RateLimited {
                retry_after_seconds,
            } = failure
            {
                let details: bcode_model::history::HistoryRateLimitDetails =
                    serde_json::from_slice(&response.payload).unwrap();
                assert_eq!(details.schema_version, 1);
                assert_eq!(details.retry_after_seconds, retry_after_seconds);
            } else {
                assert!(response.payload.is_empty());
            }
        }
    }
}
