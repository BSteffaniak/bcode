//! JSON adapters from `bcode.code_review/v1` service operations to the typed data layer.
//!
//! Each operation decodes its request payload, calls one `bcode_code_review` function, and
//! encodes the response. Failures map to the operation's stable error code; a payload that does
//! not decode is `invalid_request` and an unreadable plugin configuration is `invalid_config`.

use bcode_code_review::data::DataError;
use bcode_code_review::{CodeReviewConfig, ReviewData, ReviewStore, StoreError, store};
use bcode_code_review_models as models;
use bcode_plugin_sdk::prelude::{ServiceRequest, ServiceResponse};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Encode `value` as a successful JSON response.
pub fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

/// Serve a repository read operation, or return `None` when `request` is not one.
pub fn dispatch_data(data: &ReviewData, request: &ServiceRequest) -> Option<ServiceResponse> {
    fn serve<Req: DeserializeOwned, Resp: Serialize>(
        request: &ServiceRequest,
        operation: impl FnOnce(Req) -> Result<Resp, DataError>,
    ) -> ServiceResponse {
        match request.payload_json::<Req>() {
            Ok(payload) => match operation(payload) {
                Ok(response) => json_response(&response),
                Err(error) => ServiceResponse::error(error.code(), error.to_string()),
            },
            Err(error) => {
                ServiceResponse::error(models::REVIEW_ERROR_INVALID_REQUEST, error.to_string())
            }
        }
    }
    // There is no background timer; idle `cat-file` processes stop on the next call.
    data.evict_idle();
    Some(match request.operation.as_str() {
        models::OP_REVIEW_SOURCES_RESOLVE => serve(request, |req| data.resolve_sources(&req)),
        models::OP_REVIEW_SOURCES_TOKEN => serve(request, |req| data.sources_token(&req)),
        models::OP_REVIEW_INDEX_GET => serve(request, |req| data.index_get(&req)),
        models::OP_REVIEW_INDEX_GET_MANY => serve(request, |req| data.index_get_many(&req)),
        models::OP_REVIEW_FILE_HUNKS_GET => serve(request, |req| data.file_hunks_get(&req)),
        models::OP_REVIEW_BLOB_LINES_GET => serve(request, |req| data.blob_lines_get(&req)),
        models::OP_REVIEW_REFS_SEARCH => serve(request, |req| data.refs_search(&req)),
        models::OP_REVIEW_COMMITS_PAGE => serve(request, |req| data.commits_page(&req)),
        models::OP_REVIEW_FILES_SEARCH => serve(request, |req| data.files_search(&req)),
        models::OP_REVIEW_STACK_ATTRIBUTION => serve(request, |req| data.stack_attribution(&req)),
        models::OP_REVIEW_REPO_STATE => serve(request, |req| data.repo_state(&req)),
        models::OP_REVIEW_REVISIONS_COMPARE => serve(request, |req| data.revisions_compare(&req)),
        models::OP_REVIEW_PATCH_IDS_GET => serve(request, |req| data.patch_ids_get(&req)),
        _ => return None,
    })
}

/// Serve a stored-state or review operation, or return `None` when `request` is not one.
///
/// `config` reads the plugin configuration; it is called only by operations that use stored
/// state, after their payload decoded.
// One arm per operation: a dispatch table.
#[allow(clippy::too_many_lines)]
pub fn dispatch_store(
    request: &ServiceRequest,
    config: impl FnOnce() -> Result<CodeReviewConfig, serde_json::Error>,
) -> Option<ServiceResponse> {
    /// Decode the payload, open the store, and run `operation`.
    fn stored<Req: DeserializeOwned, Resp: Serialize>(
        request: &ServiceRequest,
        config: impl FnOnce() -> Result<CodeReviewConfig, serde_json::Error>,
        code: &str,
        operation: impl FnOnce(&ReviewStore, Req) -> Result<Resp, StoreError>,
    ) -> ServiceResponse {
        let payload = match request.payload_json::<Req>() {
            Ok(payload) => payload,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
        let store = match config() {
            Ok(config) => ReviewStore::new(config),
            Err(error) => return ServiceResponse::error("invalid_config", error.to_string()),
        };
        match operation(&store, payload) {
            Ok(response) => json_response(&response),
            Err(error) => ServiceResponse::error(code, error.to_string()),
        }
    }
    /// Decode the payload and run `operation`, which reads no stored state.
    fn stateless<Req: DeserializeOwned, Resp: Serialize>(
        request: &ServiceRequest,
        code: &str,
        operation: impl FnOnce(Req) -> Result<Resp, StoreError>,
    ) -> ServiceResponse {
        match request.payload_json::<Req>() {
            Ok(payload) => match operation(payload) {
                Ok(response) => json_response(&response),
                Err(error) => ServiceResponse::error(code, error.to_string()),
            },
            Err(error) => ServiceResponse::error("invalid_request", error.to_string()),
        }
    }

    Some(match request.operation.as_str() {
        crate::OP_CREATE_REVIEW => {
            stateless(request, "review_failed", |req| store::create_review(&req))
        }
        models::OP_REVIEW_REPO_FILE_GET => stateless(request, "repo_file_get_failed", |req| {
            store::repository_file(&req)
        }),
        crate::OP_REVIEW_DIFF_GET => stateless(request, "review_diff_failed", |req| {
            ReviewStore::default().review_diff(req)
        }),
        crate::OP_REVIEW_PUBLISHERS_LIST => stateless(
            request,
            "review_publishers_failed",
            |_: serde_json::Value| {
                Ok(models::ListReviewPublishersResponse {
                    publishers: store::publishers(),
                })
            },
        ),
        models::OP_REVIEW_PUBLISH_BUNDLE_PREVIEW => {
            stateless(request, "review_publish_preview_failed", |req| {
                store::preview_bundle(&req)
            })
        }
        models::OP_REVIEW_PUBLISH_BUNDLE_SUBMIT => {
            stateless(request, "review_publish_submit_failed", |req| {
                store::submit_bundle(&req)
            })
        }
        models::OP_REVIEW_WORKSPACE_LIST => {
            stored(request, config, "workspace_list_failed", |store, req| {
                store.list_workspaces(&req)
            })
        }
        models::OP_REVIEW_WORKSPACE_CREATE => stored(
            request,
            config,
            "workspace_create_failed",
            ReviewStore::create_workspace,
        ),
        models::OP_REVIEW_WORKSPACE_GET => stored(
            request,
            config,
            "workspace_get_failed",
            ReviewStore::get_workspace,
        ),
        models::OP_REVIEW_WORKSPACE_UPDATE => stored(
            request,
            config,
            "workspace_update_failed",
            ReviewStore::update_workspace,
        ),
        models::OP_REVIEW_WORKSPACE_ARCHIVE => stored(
            request,
            config,
            "workspace_archive_failed",
            ReviewStore::archive_workspace,
        ),
        crate::OP_DRAFT_LIST => stored(request, config, "draft_list_failed", |store, req| {
            store.list_drafts(&req)
        }),
        crate::OP_DRAFT_SAVE => stored(
            request,
            config,
            "draft_save_failed",
            ReviewStore::save_draft,
        ),
        crate::OP_DRAFT_DELETE => stored(
            request,
            config,
            "draft_delete_failed",
            ReviewStore::delete_draft,
        ),
        crate::OP_DRAFT_UPDATE => stored(
            request,
            config,
            "draft_update_failed",
            ReviewStore::update_draft,
        ),
        crate::OP_THREAD_LINK_SESSION => stored(
            request,
            config,
            "thread_link_session_failed",
            ReviewStore::link_thread_session,
        ),
        crate::OP_THREAD_RESOLVE => stored(
            request,
            config,
            "thread_resolve_failed",
            ReviewStore::resolve_thread,
        ),
        models::OP_THREAD_CLASSIFY => stored(
            request,
            config,
            "thread_classify_failed",
            ReviewStore::classify_thread,
        ),
        models::OP_REVIEW_SUGGESTIONS_LIST => {
            stored(request, config, "suggestion_list_failed", |store, req| {
                store.list_suggestions(&req)
            })
        }
        models::OP_REVIEW_SUGGESTION_SAVE => stored(
            request,
            config,
            "suggestion_save_failed",
            ReviewStore::save_suggestion,
        ),
        models::OP_REVIEW_SUGGESTION_DELETE => stored(
            request,
            config,
            "suggestion_delete_failed",
            ReviewStore::delete_suggestion,
        ),
        models::OP_REVIEW_AI_EXCHANGES_LIST => {
            stored(request, config, "ai_exchange_list_failed", |store, req| {
                store.list_ai_exchanges(&req)
            })
        }
        models::OP_REVIEW_AI_EXCHANGE_SAVE => stored(
            request,
            config,
            "ai_exchange_save_failed",
            ReviewStore::save_ai_exchange,
        ),
        models::OP_REVIEW_AI_EXCHANGE_DELETE => stored(
            request,
            config,
            "ai_exchange_delete_failed",
            ReviewStore::delete_ai_exchange,
        ),
        crate::OP_REVIEW_CONTEXT_GET => stored(
            request,
            config,
            "review_context_failed",
            ReviewStore::review_context,
        ),
        crate::OP_REVIEW_COMMENTS_LIST => stored(
            request,
            config,
            "review_comments_failed",
            ReviewStore::review_comments,
        ),
        crate::OP_REVIEW_THREAD_GET => stored(
            request,
            config,
            "review_thread_failed",
            ReviewStore::review_thread,
        ),
        crate::OP_REVIEW_BUNDLE_GET => stored(
            request,
            config,
            "review_bundle_failed",
            ReviewStore::review_bundle,
        ),
        crate::OP_REVIEW_PUBLISH_PREVIEW => stored(
            request,
            config,
            "review_publish_preview_failed",
            ReviewStore::publish_preview,
        ),
        crate::OP_REVIEW_PUBLISH_SUBMIT => stored(
            request,
            config,
            "review_publish_submit_failed",
            ReviewStore::publish_submit,
        ),
        models::OP_REVIEW_PUBLISH_RECORD_SAVE => stored(
            request,
            config,
            "publish_record_save_failed",
            ReviewStore::save_publish_record,
        ),
        models::OP_REVIEW_PUBLISH_RECORDS_LIST => stored(
            request,
            config,
            "publish_records_list_failed",
            |store, req| store.list_publish_records(&req),
        ),
        models::OP_REVIEW_EXTERNAL_IMPORT_SAVE => stored(
            request,
            config,
            "external_import_save_failed",
            ReviewStore::save_external_import,
        ),
        models::OP_REVIEW_CONVERSATION_SAVE => stored(
            request,
            config,
            "conversation_save_failed",
            ReviewStore::save_conversation,
        ),
        models::OP_REVIEW_CONVERSATION_GET => {
            stored(request, config, "conversation_get_failed", |store, req| {
                store.conversation(&req)
            })
        }
        models::OP_REVIEW_EXTERNAL_IMPORTS_LIST => stored(
            request,
            config,
            "external_imports_list_failed",
            |store, req| store.list_external_imports(&req),
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests;

/// Serve `bcode.review_discovery/v1` with git topology discovery.
pub fn dispatch_discovery(request: &ServiceRequest) -> ServiceResponse {
    use bcode_code_review::discovery::git;
    match request.operation.as_str() {
        models::OP_REVIEW_DISCOVERY_MANIFEST => json_response(&git::manifest()),
        models::OP_REVIEW_DISCOVERY_DISCOVER => {
            match serde_json::from_slice::<models::ReviewDiscoveryRequest>(&request.payload) {
                Ok(discovery) => match git::discover(&discovery) {
                    Ok(response) => json_response(&response),
                    Err(error) => ServiceResponse::error("discovery_failed", error.to_string()),
                },
                Err(error) => {
                    ServiceResponse::error(models::REVIEW_ERROR_INVALID_REQUEST, error.to_string())
                }
            }
        }
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported review discovery operation",
        ),
    }
}
