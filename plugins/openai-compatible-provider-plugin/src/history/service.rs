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
        Err(error) => ServiceResponse::error(
            "history_retrieval_failed",
            format!("history retrieval failed: {error:?}"),
        ),
    }
}
