//! Typed daemon search preparation. No client-local discovery or content IO.

use bcode_filesystem_plugin::path_search::{
    SearchPathKind, SearchPathsRequest, SearchPathsResponse, SearchPathsState,
};
use bcode_plugin_sdk::composer::{ComposerSearchRequest, ComposerSnapshot};
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

/// Plugin-owned bounded discovery policy. Workspace roots are never configurable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Include dot entries only when the daemon policy also authorizes them.
    pub include_hidden: bool,
    /// Maximum displayed candidates, additionally bounded by the composer request.
    pub max_results: usize,
    /// Maximum examined entries.
    pub max_entries: usize,
    /// Maximum subtree depth.
    pub max_depth: usize,
    /// Cooperative search deadline in milliseconds.
    pub timeout_ms: u64,
    /// Maximum path-only discovery storage.
    pub max_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            include_hidden: false,
            max_results: 32,
            max_entries: 5000,
            max_depth: 16,
            timeout_ms: 100,
            max_bytes: 131_072,
        }
    }
}

/// Prepare a path-only search; invocation identity comes only from the bound bridge.
///
/// # Errors
/// Rejects invalid composer requests, local grammar mismatches,
/// or attempts to navigate outside the workspace. No workspace root is accepted.
pub fn prepare(
    request: &ComposerSearchRequest,
    config: &Config,
) -> Result<SearchPathsRequest, &'static str> {
    request.validate().map_err(|_| "invalid_request")?;
    let active = crate::grammar::activate(&request.document.text, request.document.cursor)
        .ok_or("inactive_reference")?;
    if active.range.start != request.activation.range.start
        || active.range.end != request.activation.range.end
        || active.query != request.activation.query
    {
        return Err("activation_mismatch");
    }
    if request.activation.query.len() > 1024 {
        return Err("query_too_large");
    }
    let (directory, query) = request.activation.query.rsplit_once('/').map_or(
        ("", request.activation.query.as_str()),
        |(directory, query)| (directory, query),
    );
    if request.activation.query.starts_with('/')
        || request.activation.query.split('/').any(|part| part == "..")
        || !Path::new(directory)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        || directory.split('/').any(|part| part == "..")
    {
        return Err("invalid_directory");
    }
    Ok(SearchPathsRequest {
        query: query.to_owned(),
        directory: directory.to_owned(),
        recursive: true,
        include_hidden: config.include_hidden,
        max_results: config.max_results.clamp(1, 64).min(request.max_results),
        max_entries: config.max_entries.clamp(1, 20_000),
        max_depth: config.max_depth.clamp(1, 64),
        timeout_ms: config.timeout_ms.clamp(1, 1000),
        max_bytes: config.max_bytes.clamp(1024, 1_048_576),
    })
}

/// Search via the invocation's nested service bridge. The filesystem plugin
/// authorizes and discovers paths in the daemon workspace; there is no fallback.
///
/// # Errors
/// Returns stable path-free diagnostics for unavailable, denied, cancelled,
/// malformed or invalid searches. No partial invalid snapshot is published.
pub fn search(
    context: &NativeServiceContext,
    request: &ComposerSearchRequest,
) -> Result<ComposerSnapshot, &'static str> {
    if context.cancellation.is_cancelled() {
        return Err("cancelled");
    }
    let config = context
        .config
        .typed_or_default::<Config>()
        .map_err(|_| "invalid_config")?;
    let prepared = prepare(request, &config)?;
    let payload = serde_json::to_value(prepared).map_err(|_| "encode_failed")?;
    let response = context
        .bridge
        .request(&ServiceBridgeRequest::InvokeService(
            ToolInvocationServiceRequest {
                // Empty identity inherits the contextual bridge's bound invocation.
                invocation_id: String::new(),
                request_id: format!("{}:paths", request.request_id),
                route_id: None,
                interface_id: "bcode.filesystem/v1".to_owned(),
                operation: bcode_filesystem_plugin::path_search::OP_SEARCH_PATHS.to_owned(),
                payload,
            },
        ))
        .map_err(|_| "search_unavailable")?;
    if context.cancellation.is_cancelled() {
        return Err("cancelled");
    }
    let resolution = match response {
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded { payload }) => {
            payload
        }
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Cancelled) => {
            return Err("cancelled");
        }
        _ => return Err("search_unavailable"),
    };
    let paths = serde_json::from_value::<SearchPathsResponse>(resolution)
        .map_err(|_| "invalid_response")?;
    snapshot(request, paths)
}

/// Convert bounded daemon results into validated text-only edits.
///
/// # Errors
/// Rejects unavailable/cancelled and oversize responses, unsafe paths or edits.
pub fn snapshot(
    request: &ComposerSearchRequest,
    mut paths: SearchPathsResponse,
) -> Result<ComposerSnapshot, &'static str> {
    request.validate().map_err(|_| "invalid_request")?;
    if !matches!(
        paths.state,
        SearchPathsState::Complete | SearchPathsState::Incomplete
    ) {
        return Err("search_unavailable");
    }
    if paths.entries.len() > request.max_results {
        return Err("oversized_response");
    }
    paths.entries.sort_by(|left, right| {
        left.score
            .cmp(&right.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut incomplete = paths.state == SearchPathsState::Incomplete;
    let mut candidates = Vec::with_capacity(paths.entries.len());
    for (index, entry) in paths.entries.into_iter().enumerate() {
        if entry.path.is_empty()
            || !Path::new(&entry.path)
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err("invalid_path");
        }
        let Ok(candidate) = crate::state::path_candidate(
            request,
            format!("path-{index}"),
            &entry.path,
            entry.kind == SearchPathKind::Directory,
        ) else {
            incomplete = true;
            continue;
        };
        // Use the portable contract's own display/edit validation instead of
        // duplicating its private size limits. Unpresentable names are omitted.
        let probe = ComposerSnapshot {
            request_id: request.request_id.clone(),
            document_id: request.document.document_id.clone(),
            revision: request.document.revision,
            state: bcode_plugin_sdk::composer::ComposerSnapshotState::Ready,
            candidates: vec![candidate.clone()],
            incomplete: false,
        };
        if probe.validate_for(request).is_err() {
            incomplete = true;
            continue;
        }
        candidates.push(candidate);
    }
    let snapshot = ComposerSnapshot {
        request_id: request.request_id.clone(),
        document_id: request.document.document_id.clone(),
        revision: request.document.revision,
        state: if incomplete {
            bcode_plugin_sdk::composer::ComposerSnapshotState::Truncated
        } else {
            bcode_plugin_sdk::composer::ComposerSnapshotState::Ready
        },
        candidates,
        incomplete,
    };
    snapshot
        .validate_for(request)
        .map_err(|_| "invalid_snapshot")?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::composer::{ComposerDocument, PluginComposerContributor};
    use bcode_plugin_sdk::{PluginConfigContext, ServiceCancellation};

    fn request(text: &str) -> ComposerSearchRequest {
        let document = ComposerDocument {
            document_id: "doc".into(),
            revision: 0,
            text: text.into(),
            cursor: text.len(),
            selection: None,
        };
        ComposerSearchRequest {
            contribution_id: crate::CONTRIBUTION_ID.into(),
            request_id: "request".into(),
            activation: crate::FileReferenceContributor.activate(&document).unwrap(),
            document,
            max_results: 8,
        }
    }

    #[test]
    fn bounded_config_and_workspace_relative_navigation() {
        let prepared = prepare(
            &request("@\"two words/src/\""),
            &Config {
                max_results: usize::MAX,
                max_entries: usize::MAX,
                timeout_ms: 0,
                ..Config::default()
            },
        )
        .unwrap();
        assert_eq!(prepared.directory, "two words/src");
        assert_eq!(prepared.query, "");
        assert_eq!(prepared.max_results, 8);
        assert_eq!(prepared.max_entries, 20_000);
        assert_eq!(prepared.timeout_ms, 1);
        for text in ["@/absolute", "@../outside", "@a/../../outside"] {
            assert!(prepare(&request(text), &Config::default()).is_err());
        }
    }

    extern "C" fn bound_host(
        request_ptr: *const u8,
        request_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: ServiceBridge supplies live request/output buffers for this callback.
        let bytes = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
        let Ok(ServiceBridgeRequest::InvokeService(call)) = serde_json::from_slice(bytes) else {
            return 1;
        };
        if !call.invocation_id.is_empty()
            || call.interface_id != "bcode.filesystem/v1"
            || call.operation != "search_paths"
            || call.payload.get("invocation_id").is_some()
            || call.payload.get("workspace_root").is_some()
        {
            return 1;
        }
        let response = ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
            payload: serde_json::json!({
                "entries": [{"path": "src/lib.rs", "kind": "file", "score": 0}],
                "state": "complete", "reason": null, "visited_entries": 1
            }),
        });
        let Ok(encoded) = serde_json::to_vec(&response) else {
            return 1;
        };
        if encoded.len() > output_capacity {
            return 1;
        }
        // SAFETY: capacity was checked; bridge owns valid output_len/output_ptr.
        unsafe {
            *output_len = encoded.len();
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
        }
        bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
    }

    #[test]
    fn nested_bound_host_search_and_unavailable_without_fallback() {
        let request = request("@s");
        let context = NativeServiceContext {
            plugin_id: "bcode.file-references".into(),
            request: ServiceRequest {
                interface_id: "bcode.composer/v1".into(),
                operation: "search".into(),
                payload: serde_json::to_vec(&request).unwrap(),
            },
            config: PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            cancellation: ServiceCancellation::default(),
            bridge: ServiceBridge::new(
                Some(bound_host),
                std::ptr::null_mut(),
                ServiceCancellation::default(),
            ),
            transient_progress_limits: TransientProgressLimits::default(),
        };
        let result = search(&context, &request).unwrap();
        assert_eq!(result.candidates[0].label, "src/lib.rs");
        assert_eq!(
            result.candidates[0].continuation,
            bcode_plugin_sdk::composer::ComposerContinuation::Finish
        );
        let unbound = NativeServiceContext {
            bridge: ServiceBridge::default(),
            ..context
        };
        assert!(search(&unbound, &request).is_err());
    }

    #[test]
    fn directory_acceptance_keeps_scope_while_typing() {
        use bcode_plugin_sdk::composer::ComposerContinuation;
        for directory in ["src", "two words/界", "quote\"dir"] {
            let initial = request("before @d");
            let candidate =
                crate::state::path_candidate(&initial, "directory".into(), directory, true)
                    .unwrap();
            assert_eq!(candidate.continuation, ComposerContinuation::Reactivate);
            let mut document = candidate.edit.apply(&initial.document).unwrap();
            document.text.insert(document.cursor, '文');
            document.cursor += "文".len();
            document.revision += 1;
            let activation = crate::FileReferenceContributor.activate(&document).unwrap();
            let next = ComposerSearchRequest {
                document,
                activation,
                ..initial
            };
            let prepared = prepare(&next, &Config::default()).unwrap();
            assert_eq!(prepared.directory, directory);
            assert_eq!(prepared.query, "文");
            let file = crate::state::path_candidate(
                &next,
                "file".into(),
                &format!("{directory}/文件.rs"),
                false,
            )
            .unwrap();
            assert_eq!(file.continuation, ComposerContinuation::Finish);
            assert!(
                file.edit
                    .apply(&next.document)
                    .unwrap()
                    .text
                    .starts_with("before @")
            );
        }
    }

    #[test]
    fn mixed_unpresentable_names_keep_valid_peers_and_mark_truncation() {
        use bcode_filesystem_plugin::path_search::SearchPathEntry;
        for name in [
            "bad\nname".to_owned(),
            "x".repeat(1025),
            "界".repeat(400),
            "x".repeat(5000),
        ] {
            let request = request("@s");
            let result = snapshot(
                &request,
                SearchPathsResponse {
                    entries: vec![
                        SearchPathEntry {
                            path: name,
                            kind: SearchPathKind::File,
                            score: 0,
                        },
                        SearchPathEntry {
                            path: "src/文件.rs".into(),
                            kind: SearchPathKind::File,
                            score: 1,
                        },
                    ],
                    state: SearchPathsState::Complete,
                    reason: None,
                    visited_entries: 2,
                },
            )
            .unwrap();
            assert_eq!(result.candidates.len(), 1);
            assert_eq!(result.candidates[0].label, "src/文件.rs");
            assert!(result.incomplete);
            assert_eq!(
                result.state,
                bcode_plugin_sdk::composer::ComposerSnapshotState::Truncated
            );
            result.validate_for(&request).unwrap();
        }
    }

    #[test]
    fn deterministic_ranked_snapshot_and_rejection() {
        use bcode_filesystem_plugin::path_search::SearchPathEntry;
        let request = request("@s");
        let response = SearchPathsResponse {
            entries: vec![
                SearchPathEntry {
                    path: "src/z".into(),
                    kind: SearchPathKind::File,
                    score: 2,
                },
                SearchPathEntry {
                    path: "src/a".into(),
                    kind: SearchPathKind::Directory,
                    score: 2,
                },
            ],
            state: SearchPathsState::Incomplete,
            reason: Some("budget".into()),
            visited_entries: 100,
        };
        let result = snapshot(&request, response.clone()).unwrap();
        assert_eq!(result.candidates[0].label, "src/a");
        assert!(result.incomplete);
        let mut bad = response;
        bad.entries[0].path = "../outside".into();
        assert!(snapshot(&request, bad).is_err());
    }
}
