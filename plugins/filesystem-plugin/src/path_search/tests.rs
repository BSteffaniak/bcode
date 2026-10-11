use super::*;
use bcode_plugin_sdk::{PluginConfigContext, ServiceCancellation};
use std::cell::Cell;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "bcode-path-search-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn file(&self, path: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"never read these contents").unwrap();
    }
    fn grant(&self) -> SearchPathsGrant {
        SearchPathsGrant {
            workspace_root: self.0.clone(),
            excluded_prefixes: Vec::new(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn request() -> SearchPathsRequest {
    SearchPathsRequest {
        timeout_ms: 1000,
        ..SearchPathsRequest::default()
    }
}
fn search(
    request: &SearchPathsRequest,
    grant: &SearchPathsGrant,
    cancelled: &dyn Fn() -> bool,
) -> SearchPathsResponse {
    super::search(request, grant, cancelled, &|_| None)
}

fn authorized_search(
    request: &SearchPathsRequest,
    grant: &SearchPathsGrant,
) -> SearchPathsResponse {
    super::search(request, grant, &|| false, &|path| {
        Some(WorkspaceResourceGrant {
            workspace_root: grant.workspace_root.clone(),
            path: path.into(),
        })
    })
}

fn paths(response: &SearchPathsResponse) -> Vec<&str> {
    response
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect()
}

#[test]
fn unicode_spaces_fuzzy_and_directory_navigation() {
    let root = Fixture::new();
    root.file("src/中文 space.rs");
    root.file("src/cafe\u{301}.rs");
    root.file("src/😀.rs");
    root.file("Other.txt");
    let grant = root.grant();
    let response = search(&request(), &grant, &|| false);
    assert_eq!(response.state, SearchPathsState::Complete);
    assert_eq!(
        paths(&response),
        [
            "Other.txt",
            "src",
            "src/cafe\u{301}.rs",
            "src/中文 space.rs",
            "src/😀.rs"
        ]
    );
    let query = SearchPathsRequest {
        query: "中 s".into(),
        ..request()
    };
    assert_eq!(
        paths(&search(&query, &grant, &|| false)),
        ["src/中文 space.rs"]
    );
    let query = SearchPathsRequest {
        directory: "src".into(),
        recursive: false,
        ..request()
    };
    assert_eq!(search(&query, &grant, &|| false).entries.len(), 3);
}

#[test]
fn hidden_and_policy_exclusions_are_pruned_even_when_navigating() {
    let root = Fixture::new();
    root.file(".hidden/a");
    root.file(".git/config");
    root.file("ignored/a");
    root.file("visible");
    let mut grant = root.grant();
    grant.excluded_prefixes.push("ignored".into());
    assert_eq!(paths(&search(&request(), &grant, &|| false)), ["visible"]);
    let query = SearchPathsRequest {
        include_hidden: true,
        ..request()
    };
    assert_eq!(
        paths(&search(&query, &grant, &|| false)),
        [".hidden", ".hidden/a", "visible"]
    );
    let query = SearchPathsRequest {
        directory: "ignored".into(),
        ..request()
    };
    assert_eq!(
        search(&query, &grant, &|| false).state,
        SearchPathsState::Unavailable
    );
    grant.excluded_prefixes.push(PathBuf::new());
    assert_eq!(
        search(&request(), &grant, &|| false).reason.as_deref(),
        Some("directory_denied")
    );
}

#[test]
fn symlinks_and_parent_components_cannot_escape() {
    let root = Fixture::new();
    let outside = Fixture::new();
    outside.file("secret");
    std::os::unix::fs::symlink(&outside.0, root.0.join("escape")).unwrap();
    std::os::unix::fs::symlink(outside.0.join("secret"), root.0.join("file-link")).unwrap();
    root.file("safe");
    assert_eq!(
        paths(&search(&request(), &root.grant(), &|| false)),
        ["safe"]
    );
    for directory in ["../", "/", "escape"] {
        let query = SearchPathsRequest {
            directory: directory.into(),
            ..request()
        };
        assert_eq!(
            search(&query, &root.grant(), &|| false).state,
            SearchPathsState::Unavailable
        );
    }
    let grant = SearchPathsGrant {
        workspace_root: root.0.join("escape"),
        excluded_prefixes: Vec::new(),
    };
    assert_eq!(
        search(&request(), &grant, &|| false).state,
        SearchPathsState::Unavailable
    );
    let grant = SearchPathsGrant {
        workspace_root: root.0.join("safe"),
        excluded_prefixes: Vec::new(),
    };
    assert_eq!(
        search(&request(), &grant, &|| false).state,
        SearchPathsState::Unavailable
    );
}

#[test]
fn bounds_truthfully_distinguish_empty_truncated_and_cancelled() {
    let root = Fixture::new();
    assert_eq!(
        search(&request(), &root.grant(), &|| false).state,
        SearchPathsState::Complete
    );
    for index in 0..20 {
        root.file(&format!("file-{index:02}"));
    }
    let query = SearchPathsRequest {
        max_results: 3,
        ..request()
    };
    let first = search(&query, &root.grant(), &|| false);
    assert_eq!(first.state, SearchPathsState::Incomplete);
    assert_eq!(paths(&first), ["file-00", "file-01", "file-02"]);
    assert_eq!(
        paths(&first),
        paths(&search(&query, &root.grant(), &|| false))
    );
    let query = SearchPathsRequest {
        max_entries: 3,
        ..request()
    };
    let response = search(&query, &root.grant(), &|| false);
    assert_eq!(response.state, SearchPathsState::Incomplete);
    assert_eq!(response.visited_entries, 3);
    assert!(response.entries.is_empty()); // no arbitrary readdir-order subset
    let query = SearchPathsRequest {
        max_bytes: 1024,
        ..request()
    };
    let response = search(&query, &root.grant(), &|| false);
    assert_eq!(response.reason.as_deref(), Some("byte_limit"));
    assert!(serde_json::to_vec(&response).unwrap().len() <= 1024);
    let calls = Cell::new(0);
    let response = search(&request(), &root.grant(), &|| {
        calls.set(calls.get() + 1);
        calls.get() > 4
    });
    assert_eq!(response.state, SearchPathsState::Cancelled);
    let query = SearchPathsRequest {
        query: "not present".into(),
        ..request()
    };
    let response = search(&query, &root.grant(), &|| false);
    assert_eq!(response.state, SearchPathsState::Complete);
    assert!(response.entries.is_empty());
}

#[test]
fn depth_bound_is_explicitly_incomplete() {
    let root = Fixture::new();
    root.file("one/two/file");
    let query = SearchPathsRequest {
        max_depth: 1,
        ..request()
    };
    let response = search(&query, &root.grant(), &|| false);
    assert_eq!(response.reason.as_deref(), Some("depth_limit"));
    assert_eq!(paths(&response), ["one"]);
}

#[test]
fn non_utf8_names_are_incomplete_when_the_filesystem_supports_them() {
    use std::os::unix::ffi::OsStringExt;
    let root = Fixture::new();
    root.file("valid-peer.rs");
    let path = root.0.join(std::ffi::OsString::from_vec(vec![0xff]));
    if let Err(error) = fs::write(&path, b"unread") {
        // APFS rejects this fixture at creation; that is not traversal coverage.
        // Accept only the platform's specific encoding rejection, never I/O failures.
        #[cfg(target_os = "macos")]
        {
            assert_eq!(error.raw_os_error(), Some(libc::EILSEQ));
            assert!(!path.exists());
            eprintln!("non-UTF-8 traversal not exercised: filesystem rejected fixture with EILSEQ");
            return;
        }
        #[cfg(not(target_os = "macos"))]
        panic!("non-UTF-8 fixture creation failed: {error}");
    }
    let response = search(&request(), &root.grant(), &|| false);
    assert_eq!(response.state, SearchPathsState::Incomplete);
    assert_eq!(response.reason.as_deref(), Some("non_utf8_path"));
    assert_eq!(response.visited_entries, 2);
    assert_eq!(paths(&response), ["valid-peer.rs"]);
}

#[test]
fn ignore_file_presence_is_explicitly_unavailable_without_content_authority() {
    let root = Fixture::new();
    root.file("candidate");
    for contents in ["", "# comment\n", "ignored\n", "**/unsupported\n"] {
        fs::write(root.0.join(".gitignore"), contents).unwrap();
        let response = search(&request(), &root.grant(), &|| false);
        assert_eq!(response.state, SearchPathsState::Incomplete);
        assert_eq!(response.reason.as_deref(), Some("ignore_policy_denied"));
        assert!(response.entries.is_empty());
    }
}

#[test]
fn authorized_ignore_rules_allow_useful_repository_discovery() {
    let root = Fixture::new();
    for path in [
        "target/output",
        "node_modules/pkg",
        "debug/bin",
        "src/main.rs",
        "sub/drop.log",
        "sub/keep.log",
        "sub/space name",
        ".env",
        ".env.example",
    ] {
        root.file(path);
    }
    fs::write(
        root.0.join(".gitignore"),
        "target/\ndebug/\nnode_modules/\n.env\n.env.*\n!.env.example\n*.log\n",
    )
    .unwrap();
    fs::write(root.0.join("sub/.gitignore"), "!keep.log\nspace name\n").unwrap();
    let query = SearchPathsRequest {
        include_hidden: true,
        ..request()
    };
    let response = authorized_search(&query, &root.grant());
    assert_eq!(response.state, SearchPathsState::Complete);
    assert_eq!(
        paths(&response),
        [
            ".env.example",
            ".gitignore",
            "src",
            "src/main.rs",
            "sub",
            "sub/.gitignore",
            "sub/keep.log"
        ]
    );
    let query = SearchPathsRequest {
        directory: "target".into(),
        ..request()
    };
    assert_eq!(
        authorized_search(&query, &root.grant()).reason.as_deref(),
        Some("directory_ignored")
    );
    let query = SearchPathsRequest {
        directory: "sub".into(),
        ..request()
    };
    assert_eq!(
        paths(&authorized_search(&query, &root.grant())),
        ["sub/keep.log"]
    );
}

#[test]
fn authorized_empty_comment_and_complex_ignore_policies_have_truthful_states() {
    let root = Fixture::new();
    root.file("candidate");
    for text in ["", "\n# comment\n", "# 中文\r\n"] {
        fs::write(root.0.join(".gitignore"), text).unwrap();
        let response = authorized_search(&request(), &root.grant());
        assert_eq!(response.state, SearchPathsState::Complete);
        assert_eq!(paths(&response), ["candidate"]);
    }
    for text in ["**/complex", "[ab].rs", "\\#escape", "../escape"] {
        fs::write(root.0.join(".gitignore"), text).unwrap();
        let response = authorized_search(&request(), &root.grant());
        assert_eq!(
            response.reason.as_deref(),
            Some("ignore_policy_unsupported")
        );
        assert!(response.entries.is_empty());
    }
}

#[test]
fn oversized_symlink_and_excluded_ignore_files_fail_closed() {
    let root = Fixture::new();
    let outside = Fixture::new();
    root.file("candidate");
    outside.file("secret");
    fs::write(root.0.join(".gitignore"), vec![b'#'; 16_385]).unwrap();
    assert_eq!(
        authorized_search(&request(), &root.grant())
            .reason
            .as_deref(),
        Some("ignore_policy_unavailable")
    );
    fs::remove_file(root.0.join(".gitignore")).unwrap();
    std::os::unix::fs::symlink(outside.0.join("secret"), root.0.join(".gitignore")).unwrap();
    let response = authorized_search(&request(), &root.grant());
    assert_eq!(
        response.reason.as_deref(),
        Some("ignore_policy_unavailable")
    );
    let mut grant = root.grant();
    grant.excluded_prefixes.push(".gitignore".into());
    assert_eq!(
        authorized_search(&request(), &grant).reason.as_deref(),
        Some("ignore_policy_denied")
    );
}

#[test]
fn mismatched_resource_grant_and_descriptor_replacement_are_rejected() {
    let root = Fixture::new();
    root.file("candidate");
    fs::write(root.0.join(".gitignore"), "candidate\n").unwrap();
    let grant = root.grant();
    let response = super::search(&request(), &grant, &|| false, &|_| {
        Some(WorkspaceResourceGrant {
            workspace_root: root.0.clone(),
            path: "candidate".into(),
        })
    });
    assert_eq!(response.reason.as_deref(), Some("ignore_grant_mismatch"));
    let response = super::search(&request(), &grant, &|| false, &|path| {
        fs::rename(root.0.join(".gitignore"), root.0.join("original-ignore")).unwrap();
        fs::write(root.0.join(".gitignore"), "# replacement\n").unwrap();
        Some(WorkspaceResourceGrant {
            workspace_root: root.0.clone(),
            path: path.into(),
        })
    });
    assert_eq!(
        response.reason.as_deref(),
        Some("ignore_policy_unavailable")
    );
    assert!(response.entries.is_empty());
}

#[test]
fn ignore_parent_symlink_race_cannot_redirect_an_authorized_read() {
    let root = Fixture::new();
    let outside = Fixture::new();
    root.file("sub/candidate");
    fs::write(root.0.join("sub/.gitignore"), "candidate\n").unwrap();
    fs::write(outside.0.join(".gitignore"), "# external secret\n").unwrap();
    let grant = root.grant();
    let query = SearchPathsRequest {
        directory: "sub".into(),
        ..request()
    };
    let response = super::search(&query, &grant, &|| false, &|path| {
        fs::rename(root.0.join("sub"), root.0.join("original-sub")).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.0.join("sub")).unwrap();
        Some(WorkspaceResourceGrant {
            workspace_root: root.0.clone(),
            path: path.into(),
        })
    });
    assert_eq!(response.reason.as_deref(), Some("directory_changed"));
    assert!(response.entries.is_empty());
    assert!(
        !serde_json::to_string(&response)
            .unwrap()
            .contains("external secret")
    );
}

#[test]
fn relocated_directory_cannot_publish_denied_namespace_peers() {
    let root = Fixture::new();
    root.file("allowed/candidate");
    root.file("denied/original");
    fs::write(root.0.join("allowed/.gitignore"), "# policy\n").unwrap();
    let mut grant = root.grant();
    grant.excluded_prefixes.push("denied".into());
    let query = SearchPathsRequest {
        directory: "allowed".into(),
        ..request()
    };
    let response = super::search(&query, &grant, &|| false, &|path| {
        fs::rename(root.0.join("allowed"), root.0.join("denied/relocated")).unwrap();
        fs::write(root.0.join("denied/relocated/secret-peer"), "unread").unwrap();
        fs::create_dir(root.0.join("allowed")).unwrap();
        Some(WorkspaceResourceGrant {
            workspace_root: root.0.clone(),
            path: path.into(),
        })
    });
    assert_eq!(response.state, SearchPathsState::Incomplete);
    assert_eq!(response.reason.as_deref(), Some("directory_changed"));
    assert!(response.entries.is_empty());
}

#[test]
fn exhausted_ignore_budget_stops_before_resource_admission() {
    let root = Fixture::new();
    root.file("candidate");
    fs::write(root.0.join(".gitignore"), format!("#{}", "x".repeat(200))).unwrap();
    let called = Cell::new(false);
    let query = SearchPathsRequest {
        max_bytes: 1024,
        ..request()
    };
    let response = super::search(&query, &root.grant(), &|| false, &|_| {
        called.set(true);
        None
    });
    assert_eq!(response.reason.as_deref(), Some("byte_limit"));
    assert!(!called.get());
    assert!(response.entries.is_empty());
}

#[test]
fn resource_admission_cancellation_prevents_policy_read() {
    let root = Fixture::new();
    root.file("candidate");
    fs::write(root.0.join(".gitignore"), "candidate\n").unwrap();
    let cancelled = Cell::new(false);
    let response = super::search(&request(), &root.grant(), &|| cancelled.get(), &|path| {
        cancelled.set(true);
        Some(WorkspaceResourceGrant {
            workspace_root: root.0.clone(),
            path: path.into(),
        })
    });
    assert_eq!(response.state, SearchPathsState::Cancelled);
    assert!(response.entries.is_empty());
}

#[test]
fn relocation_after_child_discovery_discards_stale_results() {
    let root = Fixture::new();
    root.file("a/visible");
    root.file("z/peer");
    fs::write(root.0.join("z/.gitignore"), "# policy\n").unwrap();
    let grant = root.grant();
    let response = super::search(&request(), &grant, &|| false, &|path| {
        fs::rename(root.0.join("a"), root.0.join("relocated")).unwrap();
        fs::create_dir(root.0.join("a")).unwrap();
        Some(WorkspaceResourceGrant {
            workspace_root: root.0.clone(),
            path: path.into(),
        })
    });
    assert_eq!(response.state, SearchPathsState::Incomplete);
    assert_eq!(response.reason.as_deref(), Some("directory_changed"));
    assert!(response.entries.is_empty());
}

#[test]
fn cumulative_ignore_rules_stop_before_exhausted_parser_allocation() {
    let root = Fixture::new();
    root.file("candidate");
    // Reading fits, but the independently admitted third rule does not.
    fs::write(root.0.join(".gitignore"), "*.tmp\n*.log\n*.bak\n").unwrap();
    let query = SearchPathsRequest {
        max_bytes: 1024,
        ..request()
    };
    let response = authorized_search(&query, &root.grant());
    assert_eq!(response.reason.as_deref(), Some("byte_limit"));
    assert!(response.entries.is_empty());
}

extern "C" fn grant_bridge(
    request: *const u8,
    request_len: usize,
    output: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse};
    // SAFETY: synchronous test bridge receives live request bytes and grant context.
    let request = unsafe { std::slice::from_raw_parts(request, request_len) };
    // SAFETY: test retains the immutable grant for the whole callback.
    let grant = unsafe { &*user_data.cast::<SearchPathsGrant>() };
    let request: ServiceBridgeRequest = serde_json::from_slice(request).unwrap();
    let ServiceBridgeRequest::InvokeService(request) = request else {
        panic!("unexpected bridge family");
    };
    assert_eq!(request.interface_id, PATH_SEARCH_AUTHORIZATION_INTERFACE);
    assert!(request.invocation_id.is_empty());
    let payload = if request.operation == OP_WORKSPACE_ACCESS_AUTHORIZE {
        assert_eq!(
            request.payload,
            serde_json::json!({"directory":"", "recursive":true, "include_hidden":false})
        );
        serde_json::to_value(grant).unwrap()
    } else {
        assert_eq!(request.operation, OP_WORKSPACE_RESOURCE_AUTHORIZE);
        let resource: WorkspaceResourceRequest = serde_json::from_value(request.payload).unwrap();
        assert_eq!(resource.path, ".gitignore");
        serde_json::to_value(WorkspaceResourceGrant {
            workspace_root: grant.workspace_root.clone(),
            path: resource.path,
        })
        .unwrap()
    };
    let response =
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded { payload });
    let encoded = serde_json::to_vec(&response).unwrap();
    assert!(encoded.len() <= output_capacity);
    // SAFETY: ABI output buffer and length slot are writable and nonoverlapping.
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
        *output_len = encoded.len();
    }
    bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
}

#[test]
fn authorized_service_invocation_returns_only_paths() {
    let root = Fixture::new();
    root.file("file name.rs");
    root.file("ignored.log");
    fs::write(root.0.join(".gitignore"), "*.log\n").unwrap();
    let grant = root.grant();
    let cancellation = ServiceCancellation::default();
    let context = NativeServiceContext {
        plugin_id: "bcode.filesystem".into(),
        request: ServiceRequest {
            interface_id: "bcode.filesystem/v1".into(),
            operation: OP_SEARCH_PATHS.into(),
            payload: serde_json::to_vec(&request()).unwrap(),
        },
        config: PluginConfigContext::default(),
        events: ServiceEventEmitter::default(),
        cancellation: cancellation.clone(),
        bridge: ServiceBridge::new(
            Some(grant_bridge),
            std::ptr::from_ref(&grant).cast_mut().cast(),
            cancellation,
        ),
        transient_progress_limits: TransientProgressLimits::default(),
    };
    let response = super::super::invoke_filesystem_service(&context);
    assert!(response.error.is_none());
    let response: SearchPathsResponse = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(response.state, SearchPathsState::Complete);
    assert_eq!(paths(&response), ["file name.rs"]);
    assert!(
        !serde_json::to_string(&response)
            .unwrap()
            .contains("never read")
    );
}

#[test]
fn cooperative_deadline_is_observed_before_more_enumeration() {
    let root = Fixture::new();
    root.file("candidate");
    let query = SearchPathsRequest {
        timeout_ms: 1,
        ..request()
    };
    let response = search(&query, &root.grant(), &|| {
        std::thread::sleep(Duration::from_millis(2));
        false
    });
    assert_eq!(response.reason.as_deref(), Some("time_limit"));
    assert_eq!(response.visited_entries, 0);
}
#[test]
fn missing_bridge_and_caller_supplied_root_fail_closed() {
    let mut context = NativeServiceContext {
        plugin_id: "bcode.filesystem".into(),
        request: ServiceRequest {
            interface_id: "bcode.filesystem/v1".into(),
            operation: OP_SEARCH_PATHS.into(),
            payload: serde_json::to_vec(&request()).unwrap(),
        },
        config: PluginConfigContext::default(),
        events: ServiceEventEmitter::default(),
        cancellation: ServiceCancellation::default(),
        bridge: ServiceBridge::default(),
        transient_progress_limits: TransientProgressLimits::default(),
    };
    assert_eq!(
        invoke_response(&context).reason.as_deref(),
        Some("authorization_unavailable")
    );
    context.request.payload = br#"{"invocation_id":"x","workspace_root":"/"}"#.to_vec();
    assert_eq!(
        invoke_response(&context).reason.as_deref(),
        Some("invalid_request")
    );
}
