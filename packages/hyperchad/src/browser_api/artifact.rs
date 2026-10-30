//! Observation-scoped artifact ranges. No caller-selected paths or URLs.
use super::{BACKEND_TIMEOUT, ObservationGuard, error, json_content, parse_body};
use hyperchad::{renderer::Content, router::RouteRequest};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactRequest {
    session_id: bcode_session_models::SessionId,
    epoch: bcode_session_models::SessionId,
    revision: u64,
    item_id: String,
    reference_key: String,
    offset: u64,
    length: u32,
}

impl crate::HyperChadAppState {
    pub(super) async fn browser_artifact(&self, request: RouteRequest) -> Content {
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        let query: ArtifactRequest = match parse_body(&request) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        if query.length == 0
            || query.length > 128 * 1024
            || query.item_id.len() > 512
            || query.reference_key.len() > 512
        {
            return error("invalid_artifact_range");
        }
        let Ok(observations) = self.browser_api.try_lock() else {
            return error("browser_busy");
        };
        let snapshot = match observations.guarded_snapshot(&ObservationGuard {
            session_id: query.session_id,
            epoch: query.epoch,
            revision: query.revision,
        }) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        let Some(item) = snapshot
            .transcript
            .items
            .iter()
            .find(|item| item.id.get() == query.item_id)
        else {
            return error("stale_artifact");
        };
        let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } = &item.kind
        else {
            return error("unsupported_artifact");
        };
        let Some(bcode_session_view_models::ToolResultView::Artifact { artifact }) = &tool.result
        else {
            return error("unsupported_artifact");
        };
        let Some(reference) = artifact
            .artifact
            .refs
            .iter()
            .find(|reference| reference.key == query.reference_key)
        else {
            return error("stale_artifact");
        };
        if reference
            .byte_len
            .is_some_and(|length| query.offset > length)
        {
            return error("invalid_artifact_range");
        }
        let artifact_id = artifact.artifact.artifact_id.clone();
        let result = tokio::time::timeout(
            BACKEND_TIMEOUT,
            self.client.session_artifact_range(
                query.session_id,
                artifact_id,
                query.reference_key,
                query.offset,
                query.length,
            ),
        )
        .await;
        match result {
            Ok(Ok(range)) => json_content(&range),
            _ => error("artifact_unavailable"),
        }
    }
}
