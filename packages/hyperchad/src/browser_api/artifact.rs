//! Observation-scoped artifact ranges and complete bounded downloads.
//! No caller-selected paths or URLs, and no partial successful downloads.
use super::{BACKEND_TIMEOUT, ObservationGuard, error, json_content, parse_body};
use bcode_session_models::{SessionArtifactRange, SessionId};
use hyperchad::{renderer::Content, router::RouteRequest};
use serde::Deserialize;

const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const RANGE_BYTES: u32 = 128 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactRequest {
    session_id: SessionId,
    epoch: SessionId,
    revision: u64,
    item_id: String,
    reference_key: String,
    offset: u64,
    length: u32,
}

impl crate::HyperChadAppState {
    fn observed_artifact(&self, query: &ArtifactRequest) -> Result<String, &'static str> {
        if query.length == 0
            || query.length > RANGE_BYTES
            || query.item_id.len() > 512
            || query.reference_key.len() > 512
        {
            return Err("invalid_artifact_range");
        }
        let observations = self.browser_api.try_lock().map_err(|_| "browser_busy")?;
        let snapshot = observations.guarded_snapshot(&ObservationGuard {
            session_id: query.session_id,
            epoch: query.epoch,
            revision: query.revision,
        })?;
        let item = snapshot
            .transcript
            .items
            .iter()
            .find(|item| item.id.get() == query.item_id)
            .ok_or("stale_artifact")?;
        let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } = &item.kind
        else {
            return Err("unsupported_artifact");
        };
        let Some(bcode_session_view_models::ToolResultView::Artifact { artifact }) = &tool.result
        else {
            return Err("unsupported_artifact");
        };
        let reference = artifact
            .artifact
            .refs
            .iter()
            .find(|reference| reference.key == query.reference_key)
            .ok_or("stale_artifact")?;
        if reference
            .byte_len
            .is_some_and(|length| query.offset > length)
        {
            return Err("invalid_artifact_range");
        }
        let artifact_id = artifact.artifact.artifact_id.clone();
        drop(observations);
        Ok(artifact_id)
    }

    pub(super) async fn browser_artifact(&self, request: RouteRequest) -> Content {
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        let query: ArtifactRequest = match parse_body(&request) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        let artifact_id = match self.observed_artifact(&query) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        match tokio::time::timeout(
            BACKEND_TIMEOUT,
            self.client.session_artifact_range(
                query.session_id,
                artifact_id,
                query.reference_key,
                query.offset,
                query.length,
            ),
        )
        .await
        {
            Ok(Ok(range)) => json_content(&range),
            _ => error("artifact_unavailable"),
        }
    }

    pub(super) async fn browser_artifact_download(&self, request: RouteRequest) -> Content {
        if request.method.as_ref() != "GET" {
            return error("method_not_allowed");
        }
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        let Some(encoded) = request
            .query
            .get("request")
            .filter(|value| value.len() <= 4096)
        else {
            return error("invalid_request");
        };
        let Ok(query) = serde_json::from_str::<ArtifactRequest>(encoded) else {
            return error("invalid_request");
        };
        if query.offset != 0 {
            return error("invalid_artifact_range");
        }
        let artifact_id = match self.observed_artifact(&query) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        let collected = tokio::time::timeout(BACKEND_TIMEOUT, async {
            let mut download =
                ArtifactDownload::new(artifact_id.clone(), query.reference_key.clone());
            loop {
                let range = self
                    .client
                    .session_artifact_range(
                        query.session_id,
                        artifact_id.clone(),
                        query.reference_key.clone(),
                        download.next_offset(),
                        RANGE_BYTES,
                    )
                    .await
                    .map_err(|_| "artifact_unavailable")?;
                if download.accept(range)? {
                    return Ok::<_, &str>(download.bytes);
                }
            }
        })
        .await;
        match collected {
            Ok(Ok(bytes)) => Content::Raw {
                data: bytes.into(),
                content_type: "application/octet-stream".into(),
            },
            Ok(Err(code)) => error(code),
            Err(_) => error("artifact_unavailable"),
        }
    }
}

struct ArtifactDownload {
    artifact_id: String,
    reference_key: String,
    total: Option<u64>,
    revision: Option<u64>,
    bytes: Vec<u8>,
}
impl ArtifactDownload {
    const fn new(artifact_id: String, reference_key: String) -> Self {
        Self {
            artifact_id,
            reference_key,
            total: None,
            revision: None,
            bytes: Vec::new(),
        }
    }
    const fn next_offset(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn accept(&mut self, range: SessionArtifactRange) -> Result<bool, &'static str> {
        if range.total_bytes > MAX_DOWNLOAD_BYTES {
            return Err("artifact_download_exceeds_budget");
        }
        let end = range
            .offset
            .checked_add(range.bytes.len() as u64)
            .ok_or("invalid_artifact_range")?;
        let eof = range.is_eof();
        if range.artifact_id != self.artifact_id
            || range.reference_key != self.reference_key
            || range.offset != self.next_offset()
            || end > range.total_bytes
            || range.bytes.len() > RANGE_BYTES as usize
            || !range.finalized
            || range.complete == Some(false)
            || self
                .revision
                .is_some_and(|revision| revision != range.reference_revision)
            || range
                .reference_bytes
                .is_some_and(|total| total != range.total_bytes)
            || self.total.is_some_and(|total| total != range.total_bytes)
            || (range.bytes.is_empty() && end != range.total_bytes)
        {
            return Err("invalid_artifact_range");
        }
        self.total = Some(range.total_bytes);
        self.revision = Some(range.reference_revision);
        self.bytes.extend(range.bytes);
        Ok(eof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn download_rejects_non_get_before_backend_access() {
        let state =
            crate::HyperChadAppState::new(bcode_client::BcodeClient::default_endpoint(), "secret");
        let mut request = RouteRequest::from_path(
            "/api/session-view/artifact/download?token=secret",
            hyperchad::router::RequestInfo::default(),
        );
        request.method = "POST".parse().unwrap();
        let Content::Raw { data, .. } = state.browser_artifact_download(request).await else {
            panic!("expected bounded JSON error");
        };
        let response: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(response["error"], "method_not_allowed");
    }

    fn range(offset: u64, total: u64, bytes: &[u8]) -> SessionArtifactRange {
        SessionArtifactRange {
            artifact_id: "a".into(),
            reference_key: "r".into(),
            content_type: None,
            offset,
            total_bytes: total,
            bytes: bytes.to_vec(),
            reference_bytes: Some(total),
            reference_revision: 1,
            finalized: true,
            finalized_event_seq: Some(1),
            availability: None,
            complete: Some(true),
            checksum_sha256: None,
        }
    }
    #[test]
    fn complete_download_preserves_exact_bytes_across_ranges_and_empty_files() {
        let mut download = ArtifactDownload::new("a".into(), "r".into());
        assert!(!download.accept(range(0, 5, b"abc")).unwrap());
        assert_eq!(download.next_offset(), 3);
        assert!(download.accept(range(3, 5, b"de")).unwrap());
        assert_eq!(download.bytes, b"abcde");
        assert!(
            ArtifactDownload::new("a".into(), "r".into())
                .accept(range(0, 0, b""))
                .unwrap()
        );
    }
    #[test]
    fn rejects_identity_length_progress_and_budget_changes_without_appending() {
        let mut download = ArtifactDownload::new("a".into(), "r".into());
        download.accept(range(0, 5, b"abc")).unwrap();
        let mut foreign = range(3, 5, b"de");
        foreign.artifact_id = "other".into();
        for invalid in [
            foreign,
            range(0, 5, b"de"),
            range(3, 6, b"de"),
            range(3, 5, b""),
            range(3, MAX_DOWNLOAD_BYTES + 1, b"de"),
        ] {
            assert!(download.accept(invalid).is_err());
            assert_eq!(download.bytes, b"abc");
        }
    }
}
