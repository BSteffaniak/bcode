fn inventory_authorized_search(
    request: &SearchPathsRequest,
    grant: &SearchPathsGrant,
) -> SearchPathsResponse {
    super::inventory_search(request, grant, &|| false, &|path| {
        Some(WorkspaceResourceGrant {
            workspace_root: grant.workspace_root.clone(),
            path: path.into(),
        })
    })
}
fn git(root: &Fixture, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&root.0)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn tracked_request() -> SearchPathsRequest {
    SearchPathsRequest {
        scope: SearchPathsScope::Tracked,
        ..request()
    }
}

#[test]
fn git_index_staged_tracked_ignored_untracked_deleted_and_cache_permissions() {
    let root = Fixture::new();
    git(&root, &["init", "--quiet"]);
    root.file("src/tracked.txt");
    root.file("ignored/staged.txt");
    git(&root, &["add", "src/tracked.txt", "ignored/staged.txt"]);
    root.file("untracked.txt");
    root.file("ignored/untracked.txt");
    fs::write(root.0.join(".gitignore"), "ignored/\n").unwrap();
    let grant = root.grant();
    let response = inventory_authorized_search(&tracked_request(), &grant);
    assert_eq!(response.state, SearchPathsState::Complete);
    assert_eq!(
        paths(&response),
        ["ignored", "ignored/staged.txt", "src", "src/tracked.txt"]
    );
    let response = inventory_authorized_search(&request(), &grant);
    assert!(paths(&response).contains(&"untracked.txt"));
    assert!(paths(&response).contains(&"ignored/staged.txt"));
    assert!(!paths(&response).contains(&"ignored/untracked.txt"));
    // Cache cannot substitute for a new grant or old discovery exclusions.
    let response = super::inventory_search(&tracked_request(), &grant, &|| false, &|_| None);
    assert_eq!(response.reason.as_deref(), Some("git_index_denied"));
    let mut denied = grant.clone();
    denied.excluded_prefixes.push("src".into());
    assert!(
        !paths(&inventory_authorized_search(&tracked_request(), &denied))
            .contains(&"src/tracked.txt")
    );
    fs::remove_file(root.0.join("src/tracked.txt")).unwrap();
    assert!(
        !paths(&inventory_authorized_search(&tracked_request(), &grant))
            .contains(&"src/tracked.txt")
    );
    root.file("newly-staged.txt");
    git(&root, &["add", "newly-staged.txt"]);
    assert!(
        paths(&inventory_authorized_search(&tracked_request(), &grant))
            .contains(&"newly-staged.txt")
    );
    let response = super::inventory_search(&request(), &grant, &|| false, &|path| {
        (path == ".git/index").then(|| WorkspaceResourceGrant {
            workspace_root: grant.workspace_root.clone(),
            path: path.into(),
        })
    });
    assert!(response.entries.is_empty());
    assert_ne!(response.state, SearchPathsState::Complete);
}

#[test]
fn nonrepo_requires_workspace_and_worktree_gitfile_fails_explicitly() {
    let root = Fixture::new();
    root.file("visible");
    assert_eq!(
        inventory_authorized_search(&tracked_request(), &root.grant())
            .reason
            .as_deref(),
        Some("non_repository")
    );
    assert_eq!(
        paths(&inventory_authorized_search(&request(), &root.grant())),
        ["visible"]
    );
    fs::write(root.0.join(".git"), "gitdir: /outside/worktrees/lane\n").unwrap();
    assert_eq!(
        inventory_authorized_search(&tracked_request(), &root.grant())
            .reason
            .as_deref(),
        Some("git_index_denied")
    );
}

#[test]
fn workspace_explicit_refresh_and_symlink_cache_validation() {
    let root = Fixture::new();
    root.file("existing");
    let grant = root.grant();
    assert_eq!(
        paths(&inventory_authorized_search(&request(), &grant)),
        ["existing"]
    );
    root.file("new-file");
    let query = SearchPathsRequest {
        refresh: true,
        ..request()
    };
    assert!(paths(&inventory_authorized_search(&query, &grant)).contains(&"new-file"));
    fs::remove_file(root.0.join("existing")).unwrap();
    std::os::unix::fs::symlink("new-file", root.0.join("existing")).unwrap();
    assert!(!paths(&inventory_authorized_search(&request(), &grant)).contains(&"existing"));
}

/// Coordinator-only, offline end-to-end cold/warm fixture. No provider usage.
#[test]
#[ignore = "100k filesystem fixture; run explicitly in release mode"]
fn benchmark_100k_cold_warm_git_and_workspace() {
    let root = Fixture::new();
    git(&root, &["init", "--quiet"]);
    for directory in 0..100 {
        for file in 0..1000 {
            root.file(&format!("src/domain-{directory:03}/feature-{file:04}.rs"));
        }
    }
    git(&root, &["add", "src"]);
    let grant = root.grant();
    let baseline = SearchPathsRequest {
        scope: SearchPathsScope::Workspace,
        query: "domain099feature0999".into(),
        max_entries: 20_000,
        max_bytes: 262_144,
        timeout_ms: 1000,
        ..request()
    };
    let started = Instant::now();
    let baseline_response = authorized_search(&baseline, &grant);
    eprintln!(
        "uncached walker old-bounds baseline={:?} state={:?}/{:?} visited={}",
        started.elapsed(),
        baseline_response.state,
        baseline_response.reason,
        baseline_response.visited_entries
    );
    for scope in [SearchPathsScope::Tracked, SearchPathsScope::Workspace] {
        let mut query = SearchPathsRequest {
            scope,
            query: "domain099feature0999".into(),
            max_entries: 200_000,
            max_bytes: 1_048_576,
            timeout_ms: 1000,
            refresh: true,
            ..request()
        };
        let started = Instant::now();
        let cold = inventory_authorized_search(&query, &grant);
        let cold_time = started.elapsed();
        query.refresh = false;
        let mut settled = cold.clone();
        let mut continuations = 0;
        while settled.reason.as_deref() == Some("time_limit") && continuations < 8 {
            settled = inventory_authorized_search(&query, &grant);
            continuations += 1;
        }
        assert!(paths(&settled).contains(&"src/domain-099/feature-0999.rs"));
        let started = Instant::now();
        let warm = inventory_authorized_search(&query, &grant);
        let warm_time = started.elapsed();
        eprintln!(
            "scope={scope:?} cold={cold_time:?} warm={warm_time:?} continuations={continuations} cold_state={:?}/{:?} warm_state={:?}/{:?} visited={}",
            cold.state, cold.reason, warm.state, warm.reason, warm.visited_entries
        );
        assert!(paths(&warm).contains(&"src/domain-099/feature-0999.rs"));
        assert_eq!(paths(&settled), paths(&warm));
        assert!(
            warm_time <= Duration::from_millis(50),
            "100k warm request exceeded50ms: {warm_time:?}"
        );
    }
}

#[test]
fn linked_worktree_index_changes_and_fresh_external_grants() {
    let repository = Fixture::new();
    let worktree = Fixture::new();
    git(&repository, &["init", "--quiet"]);
    repository.file("base");
    git(&repository, &["add", "base"]);
    git(
        &repository,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.0.to_str().unwrap(),
            "HEAD",
        ],
    );
    let grant = worktree.grant();
    let resource = |path: &str| {
        Some(WorkspaceResourceGrant {
            workspace_root: grant.workspace_root.clone(),
            path: path.into(),
        })
    };
    let external = |path: &Path| {
        Some(WorkspaceResourceGrant {
            workspace_root: path.parent().unwrap().to_owned(),
            path: path.file_name().unwrap().to_str().unwrap().into(),
        })
    };
    let run =
        || super::search_with_metadata(&tracked_request(), &grant, &|| false, &resource, &external);
    assert!(paths(&run()).contains(&"base"));
    worktree.file("addition");
    git(&worktree, &["add", "addition"]);
    assert!(paths(&run()).contains(&"addition"));
    let denied =
        super::search_with_metadata(&tracked_request(), &grant, &|| false, &resource, &|_| None);
    assert_eq!(denied.reason.as_deref(), Some("git_index_denied"));
    let wrong =
        super::search_with_metadata(&tracked_request(), &grant, &|| false, &resource, &|_| {
            Some(WorkspaceResourceGrant {
                workspace_root: repository.0.clone(),
                path: "base".into(),
            })
        });
    assert_eq!(wrong.reason.as_deref(), Some("git_index_denied"));
    let cancelled =
        super::search_with_metadata(&tracked_request(), &grant, &|| true, &resource, &external);
    assert_eq!(cancelled.state, SearchPathsState::Cancelled);
}

#[test]
fn git_index_versions_and_required_extensions_fail_explicitly() {
    let root = Fixture::new();
    git(&root, &["init", "--quiet"]);
    root.file("src/one");
    root.file("src/two");
    git(&root, &["add", "src"]);
    for version in ["2", "3", "4"] {
        git(
            &root,
            &["update-index", &format!("--index-version={version}")],
        );
        let response = inventory_authorized_search(
            &SearchPathsRequest {
                refresh: true,
                ..tracked_request()
            },
            &root.grant(),
        );
        assert!(paths(&response).contains(&"src/one"));
        assert!(paths(&response).contains(&"src/two"));
    }
    git(&root, &["update-index", "--split-index"]);
    let response = inventory_authorized_search(
        &SearchPathsRequest {
            refresh: true,
            ..tracked_request()
        },
        &root.grant(),
    );
    assert_eq!(
        response.reason.as_deref(),
        Some("git_index_extension_unsupported")
    );
}

#[test]
fn corrupt_index_checksum_is_rejected_before_cache_admission() {
    let root = Fixture::new();
    git(&root, &["init", "--quiet"]);
    root.file("tracked");
    git(&root, &["add", "tracked"]);
    assert!(
        paths(&inventory_authorized_search(
            &tracked_request(),
            &root.grant()
        ))
        .contains(&"tracked")
    );
    let path = root.0.join(".git/index");
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(path, bytes).unwrap();
    let response = inventory_authorized_search(&tracked_request(), &root.grant());
    assert_eq!(
        response.reason.as_deref(),
        Some("git_index_checksum_or_object_format_unsupported")
    );
    assert!(response.entries.is_empty());
}

#[test]
fn nested_workspace_tracks_only_its_repo_relative_subtree_with_fresh_grants() {
    let repository = Fixture::new();
    git(&repository, &["init", "--quiet"]);
    repository.file("nested/src/tracked");
    repository.file("outside");
    git(&repository, &["add", "."]);
    repository.file("nested/untracked");
    let grant = SearchPathsGrant {
        workspace_root: repository.0.join("nested"),
        excluded_prefixes: Vec::new(),
    };
    let external = |path: &Path| {
        Some(WorkspaceResourceGrant {
            workspace_root: path.parent().unwrap().to_owned(),
            path: path.file_name().unwrap().to_str().unwrap().into(),
        })
    };
    let run =
        || super::search_with_metadata(&tracked_request(), &grant, &|| false, &|_| None, &external);
    assert_eq!(paths(&run()), ["src", "src/tracked"]);
    let denied =
        super::search_with_metadata(&tracked_request(), &grant, &|| false, &|_| None, &|_| None);
    assert_eq!(denied.reason.as_deref(), Some("git_index_denied"));
    assert!(denied.entries.is_empty());
    repository.file("nested/added");
    git(&repository, &["add", "nested/added"]);
    assert!(paths(&run()).contains(&"added"));
}

#[test]
fn sha256_index_and_scope_cache_responses() {
    let root = Fixture::new();
    git(&root, &["init", "--quiet", "--object-format=sha256"]);
    root.file("src/tracked");
    git(&root, &["add", "src"]);
    root.file("untracked");
    for scope in [
        SearchPathsScope::Tracked,
        SearchPathsScope::Workspace,
        SearchPathsScope::Tracked,
    ] {
        let query = SearchPathsRequest { scope, ..request() };
        let response = inventory_authorized_search(&query, &root.grant());
        assert_eq!(response.effective_scope, scope);
        assert!(paths(&response).contains(&"src/tracked"));
        assert_eq!(
            paths(&response).contains(&"untracked"),
            scope == SearchPathsScope::Workspace
        );
    }
    let path = root.0.join(".git/index");
    let mut bytes = fs::read(&path).unwrap();
    bytes[20] ^= 1;
    fs::write(path, bytes).unwrap();
    assert_eq!(
        inventory_authorized_search(&tracked_request(), &root.grant())
            .reason
            .as_deref(),
        Some("git_index_checksum_or_object_format_unsupported")
    );
}

#[test]
fn info_exclude_is_authorized_refreshed_and_never_suppresses_tracked() {
    let root = Fixture::new();
    git(&root, &["init", "--quiet"]);
    root.file("tracked.log");
    git(&root, &["add", "tracked.log"]);
    root.file("untracked.log");
    root.file("visible");
    fs::write(root.0.join(".git/info/exclude"), "*.log\n").unwrap();
    let grant = root.grant();
    let response = inventory_authorized_search(&request(), &grant);
    assert!(paths(&response).contains(&"tracked.log"));
    assert!(!paths(&response).contains(&"untracked.log"));
    fs::write(root.0.join(".git/info/exclude"), "visible\n").unwrap();
    let response = inventory_authorized_search(&request(), &grant);
    assert!(paths(&response).contains(&"untracked.log"));
    assert!(!paths(&response).contains(&"visible"));
    let denied = super::inventory_search(&request(), &grant, &|| false, &|path| {
        (path != ".git/info/exclude").then(|| WorkspaceResourceGrant {
            workspace_root: grant.workspace_root.clone(),
            path: path.into(),
        })
    });
    assert!(denied.entries.is_empty());
    assert_eq!(denied.reason.as_deref(), Some("git_ignore_resource_denied"));
}

#[test]
fn nonrepository_workspace_fallback_reports_effective_scope_on_cache_hits() {
    let root = Fixture::new();
    root.file("candidate");
    for query in ["", "cand", "candidate"] {
        let response = inventory_authorized_search(
            &SearchPathsRequest {
                query: query.into(),
                ..request()
            },
            &root.grant(),
        );
        assert_eq!(response.effective_scope, SearchPathsScope::Workspace);
        assert!(paths(&response).contains(&"candidate"));
    }
}
